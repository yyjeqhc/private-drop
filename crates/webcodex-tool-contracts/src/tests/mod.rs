use crate::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;

macro_rules! assert_schema_fields {
    (
        $properties:expr,
        $context:expr,
        present: [$($present:expr),* $(,)?]
        $(, absent: [$($absent:expr),* $(,)?])?
        $(,)?
    ) => {{
        let properties = $properties;
        let context = $context;
        $(
            assert!(
                properties.contains_key($present),
                "{context}: missing schema field {}",
                $present
            );
        )*
        $(
            $(
                assert!(
                    !properties.contains_key($absent),
                    "{context}: unexpected schema field {}",
                    $absent
                );
            )*
        )?
    }};
}

fn registered_tool_names() -> Vec<String> {
    registered_tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect()
}

fn spec_named<'a>(specs: &'a [ToolSpec], name: &str) -> &'a ToolSpec {
    specs
        .iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("tool '{name}' missing from specs"))
}

fn required_fields(spec: &ToolSpec) -> Vec<String> {
    spec.input_schema["required"]
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .map(|field| field.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

mod artifact_schemas;
mod catalog_discovery;
mod computer_schemas;
mod definitions;
mod edit_schemas;
mod flattened_args;
mod input_schemas;
mod metadata_policy;
mod migration_contracts;
mod output_schemas;
mod policy_contracts;
mod registry_specs;

mod audit_policy;
