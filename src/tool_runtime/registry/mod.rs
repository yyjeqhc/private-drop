//! Compatibility facade for declarative tool schemas and registered specs.

pub(super) mod input_schemas {
    #[allow(unused_imports)]
    pub(super) use webcodex_tool_contracts::registry::input_schemas::*;
}

#[allow(unused_imports)]
pub(crate) use webcodex_tool_contracts::registry::{
    accepted_flattened_args_for_spec, generic_tool_call_flattened_args_for_spec,
    memory_management_tool_specs, memory_runtime_tool_specs, operator_diagnostic_tool_specs,
    output_schema_for_tool, registered_tool_specs, skill_management_tool_specs,
    skill_runtime_tool_specs, stateless_operator_extension_tool_specs, tool_annotations,
};

#[cfg(test)]
pub(crate) use webcodex_tool_contracts::registry::coding_workflow_diagnostic_output_schema_for_test;
