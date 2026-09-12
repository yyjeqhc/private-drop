use serde_json::Value;

mod memory;
mod skills;

use super::super::tool_definition::{
    lookup_tool_definition, model_visible_tool_definitions, ToolDefinition,
};
use super::super::tool_spec::ToolSpec;
use super::{output_schema_for_tool, tool_annotations};
use std::collections::BTreeSet;

pub fn registered_tool_specs() -> Vec<ToolSpec> {
    resolve_tool_specs(model_visible_tool_definitions())
}

/// Fixed admin-only forensic trace reader. It remains globally ModelHidden and
/// is projected only by capable Stateless MCP 2026 operator adapters.
pub fn operator_diagnostic_tool_specs() -> Vec<ToolSpec> {
    vec![tool_spec(
        "read_tool_trace",
        "Admin-only bounded reader for Server-hosted full tool-request traces. Omit payload_index to list safe payload metadata first; then read one bounded JSON payload by index. Available only when full trace mode is enabled. Trace payloads may contain sensitive tool data and never grant execution authority.",
        super::input_schemas::read_tool_trace_input_schema(),
    )]
}

/// Fixed read-only project Memory runtime contract. Definitions remain hidden
/// from generic/GPT Action registries and are projected only by capable
/// Stateless MCP Full Operator adapters.
pub fn memory_runtime_tool_specs() -> Vec<ToolSpec> {
    memory::tool_specs()
        .into_iter()
        .filter(|spec| matches!(spec.name.as_str(), "memory_search" | "memory_read"))
        .collect()
}

/// Fixed project Memory mutation plus global Memory lifecycle contract. Durable
/// Memory/scope cardinality never changes this schema set. Each tool's canonical
/// ToolDefinition authority distinguishes project-scoped memory:manage from
/// admin-only lifecycle inspection/purge; permission evaluation remains independent.
pub fn memory_management_tool_specs() -> Vec<ToolSpec> {
    memory::tool_specs()
        .into_iter()
        .filter(|spec| {
            matches!(
                spec.name.as_str(),
                "memory_set" | "memory_delete" | "memory_scope_list" | "memory_scope_purge"
            )
        })
        .collect()
}

/// Fixed read-only Skill runtime contract. These definitions are deliberately
/// ModelHidden globally and are projected only by the capable Stateless MCP
/// Full Operator adapter.
pub fn skill_runtime_tool_specs() -> Vec<ToolSpec> {
    skills::tool_specs()
        .into_iter()
        .filter(|spec| matches!(spec.name.as_str(), "skill_list" | "skill_read_file"))
        .collect()
}

/// Fixed Runner-global Skill-management contract. Package/version cardinality
/// never changes this schema set; the MCP adapter additionally requires explicit
/// operator authority before projecting these tools.
pub fn skill_management_tool_specs() -> Vec<ToolSpec> {
    skills::tool_specs()
        .into_iter()
        .filter(|spec| {
            matches!(
                spec.name.as_str(),
                "skill_versions" | "skill_install" | "skill_activate" | "skill_remove_revision"
            )
        })
        .collect()
}

/// Canonical fixed Stateless MCP 2026 operator-extension universe. These specs
/// remain globally ModelHidden and are projected only by protocol/surface-aware
/// adapters and discovery. Keeping the composition here prevents tools/list,
/// Adaptive gateway admission, and tool_manifest from maintaining separate name sets.
pub fn stateless_operator_extension_tool_specs() -> Vec<ToolSpec> {
    skill_runtime_tool_specs()
        .into_iter()
        .chain(skill_management_tool_specs())
        .chain(memory_runtime_tool_specs())
        .chain(memory_management_tool_specs())
        .chain(operator_diagnostic_tool_specs())
        .collect()
}

fn resolve_tool_specs<'a>(
    definitions: impl IntoIterator<Item = &'a ToolDefinition>,
) -> Vec<ToolSpec> {
    let mut specs = Vec::new();
    let mut seen_definition_names = BTreeSet::new();
    for definition in definitions {
        if !seen_definition_names.insert(definition.name) {
            panic!(
                "{} model-visible ToolDefinition is duplicated",
                definition.name
            );
        }
        let model_spec = definition.model_spec.unwrap_or_else(|| {
            panic!(
                "{} model-visible ToolDefinition is missing model spec",
                definition.name
            )
        });
        specs.push(tool_spec(
            definition.name,
            model_spec.description,
            (model_spec.input_schema)(),
        ));
    }
    specs
}

pub(super) fn tool_spec(
    name: &'static str,
    description: impl Into<String>,
    input_schema: Value,
) -> ToolSpec {
    debug_assert!(
        lookup_tool_definition(name).is_some(),
        "{name} ToolSpec is missing a ToolDefinition"
    );
    ToolSpec {
        name: name.to_string(),
        description: description.into(),
        input_schema,
        output_schema: output_schema_for_tool(name),
        annotations: tool_annotations(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_schema_descriptions<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::Object(map) => {
                if let Some(description) = map.get("description").and_then(Value::as_str) {
                    out.push(description);
                }
                for nested in map.values() {
                    collect_schema_descriptions(nested, out);
                }
            }
            Value::Array(values) => {
                for nested in values {
                    collect_schema_descriptions(nested, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn model_visible_definitions_own_all_registered_model_specs() {
        let definitions = model_visible_tool_definitions().collect::<Vec<_>>();
        let definition_names = definitions
            .iter()
            .map(|definition| definition.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            definition_names.len(),
            definitions.len(),
            "model-visible ToolDefinition names must be unique"
        );
        for definition in &definitions {
            assert!(
                definition.model_spec.is_some(),
                "{} model-visible ToolDefinition must own description + input schema",
                definition.name
            );
        }

        let specs = registered_tool_specs();
        let spec_names = specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            spec_names.len(),
            specs.len(),
            "registered ToolSpecs must be unique"
        );
        assert_eq!(spec_names, definition_names);
        assert_eq!(specs.len(), definitions.len());
    }

    #[test]
    fn stateless_operator_extension_specs_are_one_unique_hidden_family_union() {
        let expected = skill_runtime_tool_specs()
            .into_iter()
            .chain(skill_management_tool_specs())
            .chain(memory_runtime_tool_specs())
            .chain(memory_management_tool_specs())
            .chain(operator_diagnostic_tool_specs())
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        let actual = stateless_operator_extension_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(
            actual, expected,
            "canonical extension ordering/families drifted"
        );

        let unique = actual.iter().map(String::as_str).collect::<BTreeSet<_>>();
        assert_eq!(
            unique.len(),
            actual.len(),
            "extension ToolSpecs must be unique"
        );
        let generic = registered_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<BTreeSet<_>>();
        assert!(
            unique.iter().all(|name| !generic.contains(*name)),
            "Stateless operator extensions must remain outside registered_tool_specs: {unique:?}"
        );
    }

    #[test]
    fn register_project_definition_owned_model_spec_matches_exact_contract() {
        let actual = registered_tool_specs()
            .into_iter()
            .find(|spec| spec.name == "register_project")
            .expect("register_project spec");
        let expected = tool_spec(
            "register_project",
            "Register an existing directory, including a non-Git or ad-hoc workspace, as a Project on one Runner. Use this when the directory already exists; policy still bounds allowed paths.",
            super::super::input_schemas::register_project_input_schema(),
        );
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap(),
            "definition-owned register_project ToolSpec must preserve name, description, input/output schemas, and annotations exactly"
        );
    }

    #[test]
    #[should_panic(
        expected = "register_project model-visible ToolDefinition is missing model spec"
    )]
    fn model_spec_omission_fails_closed() {
        let mut definition =
            *lookup_tool_definition("register_project").expect("register_project ToolDefinition");
        assert!(definition.model_spec.is_some());
        definition.model_spec = None;
        let _ = resolve_tool_specs(std::iter::once(&definition));
    }

    #[test]
    #[should_panic(expected = "register_project model-visible ToolDefinition is duplicated")]
    fn duplicate_definition_owned_model_specs_fail_closed() {
        let definition =
            *lookup_tool_definition("register_project").expect("register_project ToolDefinition");
        assert!(definition.model_spec.is_some());
        let duplicate = definition;
        let _ = resolve_tool_specs([&definition, &duplicate]);
    }

    #[test]
    fn reliability_tool_descriptions_are_selection_dense_and_keep_lifecycle_contracts() {
        let specs = registered_tool_specs();
        let find = |name: &str| {
            specs
                .iter()
                .find(|spec| spec.name == name)
                .unwrap_or_else(|| panic!("missing tool spec: {name}"))
        };

        for spec in &specs {
            assert!(
                spec.description.chars().count() <= crate::MODEL_TOOL_DESCRIPTION_MAX_CHARS,
                "{} description exceeds the hard model budget ({} > {} chars): {}",
                spec.name,
                spec.description.chars().count(),
                crate::MODEL_TOOL_DESCRIPTION_MAX_CHARS,
                spec.description
            );
        }

        for name in [
            "run_process",
            "run_script",
            "cargo_fmt",
            "cargo_check",
            "cargo_test",
            "go_test",
        ] {
            let description = &find(name).description;
            assert!(
                description.contains("same execution"),
                "{name}: {description}"
            );
            assert!(!description.contains("run_job"), "{name}: {description}");
        }
        assert!(find("run_process").description.contains("run_shell"));
        assert!(find("run_script").description.contains("run_shell"));
        for name in ["cargo_fmt", "cargo_check", "cargo_test", "go_test"] {
            let description = &find(name).description;
            assert!(!description.contains("run_shell"), "{name}: {description}");
        }

        let run_job = &find("run_job").description;
        assert!(run_job.contains("stable job_id"), "{run_job}");
        assert!(run_job.contains("observe"), "{run_job}");
        assert!(run_job.contains("retry"), "{run_job}");

        let observe_jobs = find("observe_jobs");
        let description = &observe_jobs.description;
        let lower = description.to_ascii_lowercase();
        assert!(lower.contains("never"), "observe_jobs: {description}");
        assert!(lower.contains("retr"), "observe_jobs: {description}");

        let token = observe_jobs.input_schema["properties"]["items"]["items"]["properties"]
            ["after_observation_token"]["description"]
            .as_str()
            .expect("observe_jobs observation token description");
        assert!(token.contains("not execution identity"), "{token}");
        assert!(token.contains("Server epoch"), "{token}");
        assert!(token.contains("without interpreting"), "{token}");
        assert!(token.contains("Return it unchanged"), "{token}");
        let wait = observe_jobs.input_schema["properties"]["wait_secs"]["description"]
            .as_str()
            .expect("observe_jobs wait description");
        assert!(wait.contains("one shared bounded wait"), "{wait}");

        let register = find("register_project");
        assert!(
            register.description.contains("non-Git"),
            "{}",
            register.description
        );
        assert_eq!(
            register.input_schema["properties"]["path"]["description"].as_str(),
            Some("Existing absolute directory path on the Runner. Git is not required.")
        );
        let work_on_project = &find("work_on_project").description;
        assert!(work_on_project.contains("checkout uses path"));
        assert!(work_on_project.contains("mode=worktree"));
        assert!(work_on_project.contains("exact Git base"));
        assert!(work_on_project.contains("Project authority"));
        for name in ["list_runners", "runtime_status"] {
            let description = &find(name).description;
            assert!(
                description.contains("shared Job concurrency"),
                "{name}: {description}"
            );
            assert!(
                description.contains("host_context"),
                "{name}: {description}"
            );
            assert!(description.contains("advisory"), "{name}: {description}");
            assert!(description.contains("authority"), "{name}: {description}");
        }

        for spec in &specs {
            if !matches!(
                spec.name.as_str(),
                "run_shell" | "run_process" | "run_script" | "session_shell_exec"
            ) {
                assert!(
                    !spec.description.contains("run_shell"),
                    "{} pollutes exact run_shell discovery: {}",
                    spec.name,
                    spec.description
                );
            }
            if spec.name != "run_job" {
                assert!(
                    !spec.description.contains("run_job"),
                    "{} pollutes exact run_job discovery: {}",
                    spec.name,
                    spec.description
                );
            }
            if spec.name != "work_on_project" {
                assert!(
                    !spec.description.contains("work_on_project"),
                    "{} pollutes exact work_on_project discovery: {}",
                    spec.name,
                    spec.description
                );
            }
        }

        for spec in &specs {
            let mut descriptions = Vec::new();
            collect_schema_descriptions(&spec.input_schema, &mut descriptions);
            collect_schema_descriptions(&spec.output_schema, &mut descriptions);
            for description in descriptions {
                if spec.name != "run_shell" {
                    assert!(
                        !description.contains("run_shell"),
                        "{} schema pollutes exact run_shell discovery: {description}",
                        spec.name
                    );
                }
                if spec.name != "run_job" {
                    assert!(
                        !description.contains("run_job"),
                        "{} schema pollutes exact run_job discovery: {description}",
                        spec.name
                    );
                }
                if spec.name != "work_on_project" {
                    assert!(
                        !description.contains("work_on_project"),
                        "{} schema pollutes exact work_on_project discovery: {description}",
                        spec.name
                    );
                }
            }
        }
    }

    #[test]
    fn tool_specs_unified_diff_field_rejects_codex_wrapper() {
        let specs = registered_tool_specs();
        let spec = specs
            .iter()
            .find(|spec| spec.name == "apply_unified_diff")
            .expect("missing apply_unified_diff spec");
        let description = spec.input_schema["properties"]["diff"]["description"]
            .as_str()
            .expect("missing unified diff description");
        let lower = description.to_lowercase();
        assert!(lower.contains("raw standard unified diff"), "{description}");
        assert!(
            description.contains("Codex apply_patch wrapper"),
            "{description}"
        );
        assert!(description.contains("*** Begin Patch"), "{description}");
    }
}
