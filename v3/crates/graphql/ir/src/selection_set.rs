use hasura_authn_core::SessionVariables;
use indexmap::IndexMap;
use lang_graphql::ast::common::Alias;
use lang_graphql::normalized_ast;
use open_dds::data_connector::DataConnectorColumnName;
use open_dds::types::{CustomTypeName, DataConnectorArgumentName, FieldName};
use plan_types::NdcFieldAlias;
use serde::Serialize;
use std::collections::BTreeMap;

use super::arguments;
use super::commands::FunctionBasedCommand;
use super::model_selection::ModelSelection;
use super::relationship::{
    self, LocalCommandRelationshipInfo, RemoteCommandRelationshipInfo, RemoteModelRelationshipInfo,
};
use crate::error;
use crate::global_id;
use graphql_schema::TypeKind;
use graphql_schema::{Annotation, OutputAnnotation, RootFieldAnnotation, GDS};
use plan::UnresolvedArgument;
use plan_types::{LocalModelRelationshipInfo, NdcRelationshipName, UsagesCounts};

#[derive(Debug, Serialize)]
pub enum NestedSelection<'s> {
    Object(ResultSelectionSet<'s>),
    Array(Box<NestedSelection<'s>>),
}

#[derive(Debug, Serialize)]
pub enum FieldSelection<'s> {
    Column {
        column: DataConnectorColumnName,
        nested_selection: Option<NestedSelection<'s>>,
        arguments: BTreeMap<DataConnectorArgumentName, UnresolvedArgument<'s>>,
    },
    ModelRelationshipLocal {
        query: ModelSelection<'s>,
        // Relationship names needs to be unique across the IR. This field contains
        // the uniquely generated relationship name. `ModelRelationshipAnnotation`
        // contains a relationship name but that is the name from the metadata.
        name: NdcRelationshipName,
        relationship_info: LocalModelRelationshipInfo<'s>,
    },
    CommandRelationshipLocal {
        ir: FunctionBasedCommand<'s>,
        name: NdcRelationshipName,
        relationship_info: LocalCommandRelationshipInfo<'s>,
    },
    ModelRelationshipRemote {
        ir: ModelSelection<'s>,
        relationship_info: RemoteModelRelationshipInfo,
    },
    CommandRelationshipRemote {
        ir: FunctionBasedCommand<'s>,
        relationship_info: RemoteCommandRelationshipInfo<'s>,
    },
}

/// IR that represents the selected fields of an output type.
#[derive(Debug, Serialize, Default)]
pub struct ResultSelectionSet<'s> {
    // The fields in the selection set. They are stored in the form that would
    // be converted and sent over the wire. Serialized the map as ordered to
    // produce deterministic golden files.
    pub fields: IndexMap<NdcFieldAlias, FieldSelection<'s>>,
}

impl ResultSelectionSet<'_> {
    /// Check if the field is found in existing fields. Returns the alias of the field.
    pub fn contains(&self, other_field: &metadata_resolve::FieldMapping) -> Option<NdcFieldAlias> {
        self.fields.iter().find_map(|(alias, field)| match field {
            FieldSelection::Column { column, .. } => {
                if column.as_str() == other_field.column.as_str() {
                    Some(alias.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
    }
}

fn build_global_id_fields(
    global_id_fields: &Vec<FieldName>,
    field_mappings: &BTreeMap<FieldName, metadata_resolve::FieldMapping>,
    field_alias: &Alias,
    fields: &mut IndexMap<NdcFieldAlias, FieldSelection>,
) -> Result<(), error::Error> {
    for field_name in global_id_fields {
        let field_mapping = field_mappings.get(field_name).ok_or_else(|| {
            error::InternalEngineError::InternalGeneric {
                description: format!("invalid global id field in annotation: {field_name:}"),
            }
        })?;
        // Prefix the global column id with something that will be unlikely to be chosen
        // by the user,
        //  to not have any conflicts with any of the fields
        // in the selection set.
        let global_col_id_alias = global_id::global_id_col_format(field_alias, field_name);

        fields.insert(
            NdcFieldAlias::from(global_col_id_alias.as_str()),
            FieldSelection::Column {
                column: field_mapping.column.clone(),
                nested_selection: None,
                arguments: BTreeMap::new(),
            },
        );
    }
    Ok(())
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum NestedSelectionType {
    /// The nested selection is selecting the root of a command.
    CommandRootSelection,

    /// Any other nested selection
    NestedSelection,
}

pub fn generate_nested_selection<'s>(
    qualified_type_reference: &metadata_resolve::QualifiedTypeReference,
    field_base_type_kind: TypeKind,
    selection_set_field_nestedness: metadata_resolve::FieldNestedness,
    nested_selection_type: NestedSelectionType,
    field: &normalized_ast::Field<'s, GDS>,
    data_connector: &'s metadata_resolve::DataConnectorLink,
    type_mappings: &'s BTreeMap<
        metadata_resolve::Qualified<CustomTypeName>,
        metadata_resolve::TypeMapping,
    >,
    session_variables: &SessionVariables,
    request_headers: &reqwest::header::HeaderMap,
    usage_counts: &mut UsagesCounts,
) -> Result<Option<NestedSelection<'s>>, error::Error> {
    match &qualified_type_reference.underlying_type {
        metadata_resolve::QualifiedBaseType::List(element_type) => {
            // If we're selecting the root of a command, then we don't regard this as a "nested field" as such
            // until we nest past the return type of the command.
            // Commands use nested selections for their root because they are either embedded in a '__value'
            // field in a single row for queries or use nested selection types at their root for mutations.
            // However, we don't consider these to be truly nested until they nest past their return type.
            let new_nestedness = match nested_selection_type {
                NestedSelectionType::CommandRootSelection => selection_set_field_nestedness,
                NestedSelectionType::NestedSelection => selection_set_field_nestedness
                    .max(metadata_resolve::FieldNestedness::ArrayNested),
            };

            let array_selection = generate_nested_selection(
                element_type,
                field_base_type_kind,
                new_nestedness,
                NestedSelectionType::NestedSelection,
                field,
                data_connector,
                type_mappings,
                session_variables,
                request_headers,
                usage_counts,
            )?;
            Ok(array_selection.map(|a| NestedSelection::Array(Box::new(a))))
        }
        metadata_resolve::QualifiedBaseType::Named(qualified_type_name) => {
            match qualified_type_name {
                metadata_resolve::QualifiedTypeName::Inbuilt(_) => Ok(None), // Inbuilt types are all scalars so there should be no subselections.
                metadata_resolve::QualifiedTypeName::Custom(data_type) => {
                    match field_base_type_kind {
                        TypeKind::Scalar => Ok(None),
                        TypeKind::Object => {
                            let metadata_resolve::TypeMapping::Object { field_mappings, .. } =
                                type_mappings.get(data_type).ok_or(
                                    error::InternalEngineError::InternalGeneric {
                                        description: format!(
                                            "no type mapping found for type {data_type}"
                                        ),
                                    },
                                )?;
                            let nested_selection = generate_selection_set_ir(
                                &field.selection_set,
                                selection_set_field_nestedness,
                                data_connector,
                                type_mappings,
                                field_mappings,
                                session_variables,
                                request_headers,
                                usage_counts,
                            )?;
                            Ok(Some(NestedSelection::Object(nested_selection)))
                        }
                    }
                }
            }
        }
    }
}

/// Builds the IR from a normalized selection set
/// `field_mappings` is needed separately during IR generation and cannot be embedded
/// into the annotation itself because the same GraphQL type may have different field
/// sources depending on the model being queried.
pub fn generate_selection_set_ir<'s>(
    selection_set: &normalized_ast::SelectionSet<'s, GDS>,
    selection_set_field_nestedness: metadata_resolve::FieldNestedness,
    data_connector: &'s metadata_resolve::DataConnectorLink,
    type_mappings: &'s BTreeMap<
        metadata_resolve::Qualified<CustomTypeName>,
        metadata_resolve::TypeMapping,
    >,
    field_mappings: &BTreeMap<FieldName, metadata_resolve::FieldMapping>,
    session_variables: &SessionVariables,
    request_headers: &reqwest::header::HeaderMap,
    usage_counts: &mut UsagesCounts,
) -> Result<ResultSelectionSet<'s>, error::Error> {
    let mut fields = IndexMap::new();
    for field in selection_set.fields.values() {
        let field_call = field.field_call()?;
        match field_call.info.generic {
            annotation @ Annotation::Output(annotated_field) => match annotated_field {
                OutputAnnotation::Field {
                    name,
                    field_type,
                    field_base_type_kind,
                    argument_types,
                    ..
                } => {
                    let field_mapping = &field_mappings.get(name).ok_or_else(|| {
                        error::InternalEngineError::InternalGeneric {
                            description: format!("invalid field in annotation: {name:}"),
                        }
                    })?;
                    let nested_selection = generate_nested_selection(
                        field_type,
                        *field_base_type_kind,
                        selection_set_field_nestedness
                            .max(metadata_resolve::FieldNestedness::ObjectNested),
                        NestedSelectionType::NestedSelection,
                        field,
                        data_connector,
                        type_mappings,
                        session_variables,
                        request_headers,
                        usage_counts,
                    )?;
                    let mut field_arguments = BTreeMap::new();
                    for (argument_name, argument_type) in argument_types {
                        let argument_value = match field_call.arguments.get(argument_name) {
                            None => {
                                if argument_type.nullable {
                                    Ok(None)
                                } else {
                                    Err(error::Error::MissingNonNullableArgument {
                                        argument_name: argument_name.to_string(),
                                        field_name: name.to_string(),
                                    })
                                }
                            }
                            Some(val) => arguments::map_argument_value_to_ndc_type(
                                argument_type,
                                &val.value,
                                type_mappings,
                            )
                            .map(Some),
                        }?;
                        if let Some(argument_value) = argument_value {
                            let argument = UnresolvedArgument::Literal {
                                value: argument_value,
                            };
                            // If argument name is not found in the mapping, use the open_dd argument name as the ndc argument name
                            let ndc_argument_name = field_mapping
                                .argument_mappings
                                .get(argument_name.as_str())
                                .map_or_else(
                                    || DataConnectorArgumentName::from(argument_name.as_str()),
                                    Clone::clone,
                                );
                            field_arguments.insert(ndc_argument_name, argument);
                        }
                    }

                    fields.insert(
                        NdcFieldAlias::from(field.alias.0.as_str()),
                        FieldSelection::Column {
                            column: field_mapping.column.clone(),
                            nested_selection,
                            arguments: field_arguments,
                        },
                    );
                }
                OutputAnnotation::RootField(RootFieldAnnotation::Introspection) => {}
                OutputAnnotation::GlobalIDField { global_id_fields } => {
                    build_global_id_fields(
                        global_id_fields,
                        field_mappings,
                        &field.alias,
                        &mut fields,
                    )?;
                }
                OutputAnnotation::RelayNodeInterfaceID { typename_mappings } => {
                    // Even though we already have the value of the global ID field
                    // here, we try to re-compute the value of the same ID by decoding the ID.
                    // We do this because it simplifies the code structure.
                    // If the NDC were to accept key-value pairs from the v3-engine that will
                    // then be outputted as it is, then we could avoid this computation.
                    let type_name = field.selection_set.type_name.clone().ok_or(
                        error::InternalEngineError::InternalGeneric {
                            description: "typename not found while resolving NodeInterfaceId"
                                .to_string(),
                        },
                    )?;
                    let global_id_fields = typename_mappings.get(&type_name).ok_or(
                        error::InternalEngineError::InternalGeneric {
                            description: format!(
                                "Global ID fields not found of the type {type_name}"
                            ),
                        },
                    )?;

                    build_global_id_fields(
                        global_id_fields,
                        field_mappings,
                        &field.alias,
                        &mut fields,
                    )?;
                }
                OutputAnnotation::RelationshipToModel(relationship_annotation) => {
                    fields.insert(
                        NdcFieldAlias::from(field.alias.0.as_str()),
                        relationship::generate_model_relationship_ir(
                            field,
                            relationship_annotation,
                            selection_set_field_nestedness,
                            data_connector,
                            type_mappings,
                            session_variables,
                            request_headers,
                            usage_counts,
                        )?,
                    );
                }
                OutputAnnotation::RelationshipToModelAggregate(relationship_annotation) => {
                    fields.insert(
                        NdcFieldAlias::from(field.alias.0.as_str()),
                        relationship::generate_model_aggregate_relationship_ir(
                            field,
                            relationship_annotation,
                            selection_set_field_nestedness,
                            data_connector,
                            type_mappings,
                            session_variables,
                            request_headers,
                            usage_counts,
                        )?,
                    );
                }
                OutputAnnotation::RelationshipToCommand(relationship_annotation) => {
                    fields.insert(
                        NdcFieldAlias::from(field.alias.0.as_str()),
                        relationship::generate_command_relationship_ir(
                            field,
                            relationship_annotation,
                            selection_set_field_nestedness,
                            data_connector,
                            type_mappings,
                            session_variables,
                            request_headers,
                            usage_counts,
                        )?,
                    );
                }
                _ => Err(error::InternalEngineError::UnexpectedAnnotation {
                    annotation: annotation.clone(),
                })?,
            },

            annotation => Err(error::InternalEngineError::UnexpectedAnnotation {
                annotation: annotation.clone(),
            })?,
        }
    }
    Ok(ResultSelectionSet { fields })
}
