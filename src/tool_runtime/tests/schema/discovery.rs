use super::*;

#[test]
fn discovery_output_schemas_cover_runtime_payload_keys() {
    use crate::tool_runtime::tool_definition::TOOL_CATEGORY_GIT;

    let runtime = test_runtime();
    let specs = registered_tool_specs();

    let list_tools_spec = spec_named(&specs, "list_tools");
    let list_tools_payload = runtime.list_tools_payload(ListToolsOptions {
        category: Some(TOOL_CATEGORY_GIT.to_string()),
        features: Some("read".to_string()),
        summary_only: true,
        limit: Some(3),
    });
    assert_payload_keys_declared(
        "list_tools",
        &list_tools_payload,
        output_schema_properties(list_tools_spec),
    );

    let tool_manifest_spec = spec_named(&specs, "tool_manifest");
    let tool_manifest_payload = runtime
        .compact_tool_manifest_payload_bounded(
            Some(vec![TOOL_CATEGORY_GIT.to_string()]),
            None,
            Some(2),
        )
        .unwrap();
    assert_payload_keys_declared(
        "tool_manifest",
        &tool_manifest_payload,
        output_schema_properties(tool_manifest_spec),
    );
}

#[test]
fn tool_manifest_and_list_tools_limit_truncation_reports_limit_reason() {
    use crate::tool_runtime::tool_definition::TOOL_CATEGORY_SESSION;

    let runtime = test_runtime();
    let list_tools = runtime.list_tools_payload(ListToolsOptions {
        category: None,
        features: None,
        summary_only: true,
        limit: Some(2),
    });
    assert_eq!(list_tools["truncated"], true);
    assert_eq!(list_tools["truncation_reason"], "limit");
    assert_eq!(list_tools["limit_applied"], true);
    assert_eq!(list_tools["requested_limit"], 2);
    assert_eq!(list_tools["count"], 2);
    assert_eq!(list_tools["returned_count"], 2);
    assert_eq!(list_tools["filtered_count"], list_tools["total_count"]);
    assert!(list_tools["total_count"].as_u64().unwrap() > 2);
    assert!(!serde_json::to_string(&list_tools)
        .unwrap()
        .contains("ResponseTooLarge"));

    let manifest = runtime
        .compact_tool_manifest_payload_bounded(
            Some(vec![TOOL_CATEGORY_SESSION.to_string()]),
            None,
            Some(2),
        )
        .unwrap();
    assert_eq!(manifest["truncated"], true);
    assert_eq!(manifest["truncation_reason"], "limit");
    assert_eq!(manifest["limit_applied"], true);
    assert_eq!(manifest["requested_limit"], 2);
    assert_eq!(manifest["count"], 2);
    assert_eq!(manifest["returned_count"], 2);
    assert!(manifest["filtered_count"].as_u64().unwrap() > 2);
    assert!(
        manifest["total_count"].as_u64().unwrap() >= manifest["filtered_count"].as_u64().unwrap()
    );
    assert!(!serde_json::to_string(&manifest)
        .unwrap()
        .contains("ResponseTooLarge"));
}

#[test]
fn tool_manifest_sparse_filtered_projection_only_keeps_truncation_metadata_when_needed() {
    let runtime = test_runtime();
    let canonical = runtime
        .compact_tool_manifest_payload_bounded(None, Some("coding".to_string()), Some(3))
        .expect("limited coding manifest");
    let mut result = crate::tool_runtime::ToolResult::ok(canonical);
    crate::tool_runtime::surface::sparsify_tool_manifest_model_result(&mut result);

    assert_eq!(result.output["truncated"], true);
    assert_eq!(result.output["truncation_reason"], "limit");
    assert_eq!(result.output["returned_count"], 3);
    assert!(result.output["filtered_count"].as_u64().unwrap() > 3);
    assert_eq!(result.output["limit"], 3);
    assert!(result.output.get("categories").is_none());
    assert!(result.output.get("tool_count").is_none());
    assert!(result.output.get("count").is_none());
    assert!(result.output.get("total_count").is_none());
}

fn output_schema_properties(spec: &ToolSpec) -> &serde_json::Map<String, Value> {
    spec.output_schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("{} output schema properties", spec.name))
}

fn assert_payload_keys_declared(
    tool_name: &str,
    payload: &Value,
    output_schema_properties: &serde_json::Map<String, Value>,
) {
    let payload = payload
        .as_object()
        .unwrap_or_else(|| panic!("{tool_name} payload object"));
    for key in payload.keys() {
        assert!(
            output_schema_properties.contains_key(key),
            "{tool_name} runtime output key {key} is missing from output_schema properties"
        );
    }
}

fn string_array(value: &Value, context: &str) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("{context} must be an array"))
        .iter()
        .map(|member| {
            member
                .as_str()
                .unwrap_or_else(|| panic!("{context} member must be a string: {member:?}"))
                .to_string()
        })
        .collect()
}

fn string_set(value: &Value, context: &str) -> BTreeSet<String> {
    string_array(value, context).into_iter().collect()
}

fn category_member_sets(
    categories: &Value,
    context: &str,
) -> std::collections::BTreeMap<String, BTreeSet<String>> {
    categories
        .as_object()
        .unwrap_or_else(|| panic!("{context} must be an object"))
        .iter()
        .map(|(category, members)| {
            (
                category.clone(),
                string_set(members, &format!("{context}.{category}")),
            )
        })
        .collect()
}

fn definition_category_member_sets() -> std::collections::BTreeMap<String, BTreeSet<String>> {
    use crate::tool_runtime::tool_definition::model_visible_tool_definitions;

    let mut categories = std::collections::BTreeMap::new();
    for definition in model_visible_tool_definitions() {
        categories
            .entry(definition.category.to_string())
            .or_insert_with(BTreeSet::new)
            .insert(definition.name.to_string());
    }
    categories
}

fn tool_entry_names(tools: &Value, context: &str) -> BTreeSet<String> {
    tools
        .as_array()
        .unwrap_or_else(|| panic!("{context} must be an array"))
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .unwrap_or_else(|| panic!("{context} entry missing name: {tool:?}"))
                .to_string()
        })
        .collect()
}

fn assert_categories_hide_runtime_only_tools(
    categories: &std::collections::BTreeMap<String, BTreeSet<String>>,
    context: &str,
) {
    for forbidden in ["delete_files", "run_codex"] {
        assert!(
            categories
                .values()
                .all(|members| !members.contains(forbidden)),
            "{context} categories must not expose {forbidden}: {categories:?}"
        );
    }
}

fn assert_no_response_too_large(surface: &str, payload: &Value) {
    assert!(
        !serde_json::to_string(payload)
            .unwrap()
            .contains("ResponseTooLarge"),
        "{surface} bounded discovery must not surface ResponseTooLarge: {payload:?}"
    );
}

fn allowed_tool_definition_categories_for_discovery_group(group: &str) -> &'static [&'static str] {
    match group {
        "checkpoint" => &["checkpoint"],
        "cleanup" => &["checkpoint", "cleanup"],
        "coding_agent" => &["coding_agent"],
        "agent_task" => &["agent_task"],
        "communication" => &["communication"],
        "edit" => &["artifact", "edit", "patch"],
        "file_transfer" => &["artifact"],
        "git" => &["checkpoint", "cleanup", "file", "git"],
        "inspect" => &[
            "checkpoint",
            "computer",
            "file",
            "git",
            "job",
            "lsp",
            "project",
            "runtime",
            "session",
            "workflow",
        ],
        "jobs" => &["job"],
        "patch" => &["patch"],
        "projects" => &["project"],
        "review" => &["checkpoint", "cleanup", "file", "git", "workflow"],
        "runtime" => &["checkpoint", "project", "runtime", "session", "workflow"],
        "shell" => &["job", "validation"],
        "validation" => &["validation"],
        other => panic!("missing discovery group category allowlist for {other}"),
    }
}

fn expected_cross_listed_discovery_groups(tool: &str) -> Option<&'static [&'static str]> {
    match tool {
        "apply_patch" => Some(&["edit", "patch"]),
        "apply_unified_diff" => Some(&["edit", "patch"]),
        "cargo_check" => Some(&["shell", "validation"]),
        "cargo_fmt" => Some(&["shell", "validation"]),
        "cargo_test" => Some(&["shell", "validation"]),
        "discard_untracked" => Some(&["cleanup", "git"]),
        "finish_coding_task" => Some(&["review", "runtime"]),
        "artifact_upload_abort"
        | "artifact_upload_begin"
        | "artifact_upload_chunk"
        | "artifact_upload_finish"
        | "export_project_artifact"
        | "import_conversation_files_to_project"
        | "read_project_artifact"
        | "read_project_artifact_metadata"
        | "save_project_artifact" => Some(&["edit", "file_transfer"]),
        "git_diff_hunks" => Some(&["git", "inspect", "review"]),
        "git_review_summary" => Some(&["git", "inspect", "review"]),
        "git_log" => Some(&["git", "inspect", "review"]),
        "git_restore_paths" => Some(&["cleanup", "git"]),
        "git_status" => Some(&["git", "inspect", "review"]),
        "list_runners" => Some(&["inspect", "runtime"]),
        "list_projects" => Some(&["inspect", "projects", "runtime"]),
        "list_tools" => Some(&["inspect", "runtime"]),
        "run_job" => Some(&["jobs", "shell"]),
        "run_detached_process" => Some(&["jobs", "shell"]),
        "run_process" | "run_script" => Some(&["inspect", "shell"]),
        "run_shell" => Some(&["inspect", "shell"]),
        "open_session_shell"
        | "session_shell_exec"
        | "session_shell_status"
        | "close_session_shell" => Some(&["jobs", "shell"]),
        "runtime_status" => Some(&["inspect", "runtime"]),
        "show_changes" => Some(&["git", "inspect", "review"]),
        "work_on_project" => Some(&["inspect", "runtime"]),
        "workspace_checkpoint_create" => Some(&["checkpoint", "git", "runtime"]),
        "workspace_checkpoint_delete" => Some(&["checkpoint", "cleanup", "runtime"]),
        "workspace_checkpoint_list" => Some(&["checkpoint", "inspect", "review", "runtime"]),
        "workspace_checkpoint_restore" => Some(&["checkpoint", "git", "runtime"]),
        "workspace_checkpoint_show" => Some(&["checkpoint", "inspect", "review", "runtime"]),
        _ => None,
    }
}

#[test]
fn tool_discovery_groups_drive_tool_categories() {
    use crate::tool_runtime::tool_definition::{
        is_model_visible_tool_name, lookup_tool_definition, model_visible_tool_definitions,
        TOOL_DISCOVERY_GROUPS,
    };
    use std::collections::{BTreeMap, BTreeSet};

    let categories = registered_tool_categories();
    let category_map = categories.as_object().expect("categories object");
    let actual_category_names = category_map
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected_group_names = TOOL_DISCOVERY_GROUPS
        .iter()
        .map(|group| group.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual_category_names, expected_group_names,
        "registered_tool_categories keys must come only from TOOL_DISCOVERY_GROUPS"
    );

    let mut memberships: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    for group in TOOL_DISCOVERY_GROUPS {
        let actual_tools = string_array(
            category_map
                .get(group.name)
                .unwrap_or_else(|| panic!("{} discovery category missing", group.name)),
            group.name,
        );
        let tools = group
            .tools
            .iter()
            .map(|name| {
                let definition = lookup_tool_definition(name)
                    .unwrap_or_else(|| panic!("{name} discovery group entry missing definition"));
                assert!(
                    definition.visibility.is_model_visible(),
                    "{name} discovery group entry must be model-visible"
                );
                assert!(
                    is_model_visible_tool_name(name),
                    "{name} discovery group entry must pass visibility facade"
                );
                assert!(
                    allowed_tool_definition_categories_for_discovery_group(group.name)
                        .contains(&definition.category),
                    "{} discovery group entry {} has ToolDefinition category {}, which is not in the explicit allowlist",
                    group.name,
                    name,
                    definition.category
                );
                memberships.entry(name).or_default().push(group.name);
                Value::String((*name).to_string())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            category_map.get(group.name),
            Some(&Value::Array(tools)),
            "{} category must derive from ToolDefinition discovery groups",
            group.name
        );
        assert_eq!(
            actual_tools,
            group
                .tools
                .iter()
                .map(|tool| (*tool).to_string())
                .collect::<Vec<_>>(),
            "{} registered category order must match TOOL_DISCOVERY_GROUPS",
            group.name
        );
    }

    let exact_discovery_only = ["git_diff", "git_diff_summary"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut omitted_from_groups = BTreeSet::new();
    for definition in model_visible_tool_definitions() {
        let Some(groups) = memberships.get(definition.name) else {
            omitted_from_groups.insert(definition.name);
            continue;
        };
        if groups.len() == 1 {
            continue;
        }
        let expected =
            expected_cross_listed_discovery_groups(definition.name).unwrap_or_else(|| {
                panic!(
                    "{} appears in multiple discovery groups without an explicit allowlist: {:?}",
                    definition.name, groups
                )
            });
        let actual_groups = groups.iter().copied().collect::<BTreeSet<_>>();
        let expected_groups = expected.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            actual_groups, expected_groups,
            "{} discovery cross-listing changed",
            definition.name
        );
    }
    assert_eq!(
        omitted_from_groups, exact_discovery_only,
        "only exact-discovery compatibility primitives may stay out of ordinary discovery groups"
    );

    for allowed in [
        "apply_unified_diff",
        "cargo_check",
        "cargo_fmt",
        "cargo_test",
        "discard_untracked",
        "finish_coding_task",
        "git_diff_hunks",
        "git_log",
        "git_restore_paths",
        "git_status",
        "list_runners",
        "list_projects",
        "list_tools",
        "run_job",
        "run_process",
        "run_script",
        "runtime_status",
        "show_changes",
        "work_on_project",
        "workspace_checkpoint_create",
        "workspace_checkpoint_delete",
        "workspace_checkpoint_list",
        "workspace_checkpoint_restore",
        "workspace_checkpoint_show",
    ] {
        assert!(
            memberships
                .get(allowed)
                .is_some_and(|groups| groups.len() > 1),
            "{allowed} discovery cross-list allowlist must stay tied to an actual duplicate"
        );
    }
}

#[test]
fn tool_manifest_categories_cover_every_model_visible_definition() {
    use crate::tool_runtime::tool_definition::model_visible_tool_definitions;

    let runtime = test_runtime();
    let manifest = runtime.compact_tool_manifest_payload();
    assert_eq!(
        manifest["tool_count"],
        registered_tool_specs().len() as i64,
        "tool_manifest tool_count must mirror model-facing ToolSpec count"
    );
    let categories = manifest["categories"]
        .as_object()
        .expect("tool_manifest categories");

    for definition in model_visible_tool_definitions() {
        let members = categories
            .get(definition.category)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("missing tool_manifest category {}", definition.category));
        assert!(
            members.iter().any(|member| member == definition.name),
            "{} ToolDefinition category {} must include the tool in tool_manifest",
            definition.name,
            definition.category
        );
    }
}

#[test]
fn tool_manifest_compact_categories_match_single_tool_definition_category() {
    use crate::tool_runtime::tool_definition::{
        lookup_tool_definition, model_visible_tool_definitions,
    };
    use std::collections::BTreeMap;

    let runtime = test_runtime();
    let manifest = runtime.compact_tool_manifest_payload();
    let categories = manifest["categories"]
        .as_object()
        .expect("tool_manifest categories");
    let visible_names = model_visible_tool_definitions()
        .map(|definition| definition.name)
        .collect::<BTreeSet<_>>();
    let mut memberships: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (category, members) in categories {
        for member in members
            .as_array()
            .unwrap_or_else(|| panic!("{category} members must be an array"))
        {
            let name = member
                .as_str()
                .unwrap_or_else(|| panic!("{category} member must be a string"));
            let definition = lookup_tool_definition(name)
                .unwrap_or_else(|| panic!("{category} member {name} missing ToolDefinition"));
            assert!(
                definition.visibility.is_model_visible(),
                "{category} member {name} must be model-visible"
            );
            assert_eq!(
                definition.category, category,
                "{name} compact manifest category must match ToolDefinition category"
            );
            memberships
                .entry(name.to_string())
                .or_default()
                .push(category.clone());
        }
    }

    assert_eq!(
        memberships.len(),
        visible_names.len(),
        "compact tool_manifest categories must cover every model-visible tool exactly once"
    );
    for definition in model_visible_tool_definitions() {
        let member_categories = memberships
            .get(definition.name)
            .unwrap_or_else(|| panic!("{} missing compact manifest category", definition.name));
        assert_eq!(
            member_categories,
            &vec![definition.category.to_string()],
            "{} must have exactly one compact manifest category",
            definition.name
        );
    }

    let tools = manifest["tools"].as_array().expect("tool_manifest tools");
    assert_eq!(
        tools.len(),
        visible_names.len(),
        "unfiltered compact tool_manifest must list every model-visible tool"
    );
    for tool in tools {
        let name = tool["name"]
            .as_str()
            .expect("tool_manifest tool name must be a string");
        let definition = lookup_tool_definition(name)
            .unwrap_or_else(|| panic!("{name} compact manifest entry missing ToolDefinition"));
        assert!(
            visible_names.contains(name),
            "{name} compact manifest entry must be model-visible"
        );
        assert_eq!(
            tool["category"].as_str(),
            Some(definition.category),
            "{name} compact manifest entry category must match ToolDefinition"
        );
    }
}

#[test]
fn compact_tool_manifest_categories_match_bounded_list_tools_categories() {
    let runtime = test_runtime();
    let expected_categories = definition_category_member_sets();
    let expected_count: usize = expected_categories
        .values()
        .map(|members| members.len())
        .sum();

    let manifest = runtime.compact_tool_manifest_payload();
    let manifest_categories = category_member_sets(&manifest["categories"], "tool_manifest");
    assert_eq!(
        manifest_categories, expected_categories,
        "compact tool_manifest categories must be grouped by ToolDefinition category"
    );
    assert_categories_hide_runtime_only_tools(&manifest_categories, "tool_manifest");

    let list_tools = runtime.list_tools_payload(ListToolsOptions {
        category: None,
        features: None,
        summary_only: true,
        limit: None,
    });
    let list_categories = category_member_sets(&list_tools["categories"], "list_tools");
    assert_eq!(
        list_categories, manifest_categories,
        "bounded list_tools categories must match compact tool_manifest categories"
    );
    assert_categories_hide_runtime_only_tools(&list_categories, "list_tools");
    assert_eq!(manifest["tool_count"].as_u64(), Some(expected_count as u64));
    assert_eq!(
        manifest["returned_count"].as_u64(),
        Some(expected_count as u64)
    );
    assert_eq!(
        list_tools["total_count"].as_u64(),
        Some(expected_count as u64)
    );
    assert_eq!(
        list_tools["returned_count"].as_u64(),
        Some(expected_count as u64)
    );
    assert_eq!(list_tools["truncated"], false);
}

#[test]
fn tool_manifest_category_filter_matches_tool_definition_categories() {
    let runtime = test_runtime();
    let expected_categories = definition_category_member_sets();
    let all_manifest_categories = category_member_sets(
        &runtime.compact_tool_manifest_payload()["categories"],
        "unfiltered tool_manifest",
    );

    for (category, expected_tools) in expected_categories {
        let manifest = runtime
            .compact_tool_manifest_payload_bounded(Some(vec![category.clone()]), None, None)
            .unwrap();
        assert_eq!(manifest["filtered"], true);
        assert_eq!(manifest["category"].as_str(), Some(category.as_str()));
        assert_eq!(
            string_array(&manifest["categories_requested"], "categories_requested"),
            vec![category.clone()]
        );
        assert_eq!(
            manifest["filtered_count"].as_u64(),
            Some(expected_tools.len() as u64)
        );
        assert_eq!(
            manifest["returned_count"].as_u64(),
            Some(expected_tools.len() as u64)
        );
        assert_eq!(
            manifest["count"].as_u64(),
            Some(expected_tools.len() as u64)
        );
        assert_eq!(manifest["truncated"], false);
        assert_eq!(manifest["limit_applied"], false);
        assert!(manifest["total_count"].as_u64().unwrap() >= expected_tools.len() as u64);
        assert_no_response_too_large("tool_manifest", &manifest);

        let filtered_categories = category_member_sets(
            &manifest["categories"],
            &format!("tool_manifest filtered {category} categories"),
        );
        assert_eq!(
            filtered_categories, all_manifest_categories,
            "filtered compact tool_manifest currently preserves the full categories map"
        );
        assert_categories_hide_runtime_only_tools(&filtered_categories, "filtered tool_manifest");

        let returned_tools = tool_entry_names(
            &manifest["tools"],
            &format!("tool_manifest filtered {category} tools"),
        );
        assert_eq!(
            returned_tools, expected_tools,
            "tool_manifest category filter must return exactly the ToolDefinition category members"
        );
        for tool in manifest["tools"].as_array().expect("tool_manifest tools") {
            assert_eq!(
                tool["category"].as_str(),
                Some(category.as_str()),
                "filtered tool_manifest must not mix categories: {tool:?}"
            );
        }
    }
}

#[test]
fn list_tools_category_filter_matches_tool_definition_categories() {
    let runtime = test_runtime();
    let expected_categories = definition_category_member_sets();
    let all_list_categories = category_member_sets(
        &runtime.list_tools_payload(ListToolsOptions {
            category: None,
            features: None,
            summary_only: true,
            limit: None,
        })["categories"],
        "unfiltered list_tools",
    );

    for (category, expected_tools) in expected_categories {
        let list_tools = runtime.list_tools_payload(ListToolsOptions {
            category: Some(category.clone()),
            features: None,
            summary_only: true,
            limit: None,
        });
        assert_eq!(list_tools["category"].as_str(), Some(category.as_str()));
        assert_eq!(list_tools["features"], Value::Null);
        assert_eq!(
            list_tools["filtered_count"].as_u64(),
            Some(expected_tools.len() as u64)
        );
        assert_eq!(
            list_tools["returned_count"].as_u64(),
            Some(expected_tools.len() as u64)
        );
        assert_eq!(
            list_tools["count"].as_u64(),
            Some(expected_tools.len() as u64)
        );
        assert_eq!(list_tools["truncated"], false);
        assert_eq!(list_tools["limit_applied"], false);
        assert!(list_tools["total_count"].as_u64().unwrap() >= expected_tools.len() as u64);
        assert_no_response_too_large("list_tools", &list_tools);

        let filtered_categories = category_member_sets(
            &list_tools["categories"],
            &format!("list_tools filtered {category} categories"),
        );
        assert_eq!(
            filtered_categories, all_list_categories,
            "filtered list_tools currently preserves the full ToolDefinition category map"
        );
        assert_categories_hide_runtime_only_tools(&filtered_categories, "filtered list_tools");

        let names = string_set(
            &list_tools["names"],
            &format!("list_tools {category} names"),
        );
        assert_eq!(
            names, expected_tools,
            "list_tools category filter names must match ToolDefinition category members"
        );
        let returned_tools = tool_entry_names(
            &list_tools["tools"],
            &format!("list_tools filtered {category} tools"),
        );
        assert_eq!(
            returned_tools, expected_tools,
            "list_tools category filter tools must match ToolDefinition category members"
        );
        for tool in list_tools["tools"].as_array().expect("list_tools tools") {
            assert_eq!(
                tool["category"].as_str(),
                Some(category.as_str()),
                "filtered list_tools must not mix categories: {tool:?}"
            );
        }
    }
}

#[test]
fn tool_manifest_recommended_flows_reference_visible_defined_tools() {
    use crate::tool_runtime::tool_definition::{
        is_model_visible_tool_name, lookup_tool_definition, TOOL_RECOMMENDED_FLOWS,
    };

    let runtime = test_runtime();
    let manifest = runtime.compact_tool_manifest_payload();
    let manifest_categories = category_member_sets(&manifest["categories"], "tool_manifest");
    let flows = manifest["recommended_flows"]
        .as_array()
        .expect("tool_manifest recommended_flows");
    assert_eq!(flows.len(), TOOL_RECOMMENDED_FLOWS.len());

    for (actual, expected) in flows.iter().zip(TOOL_RECOMMENDED_FLOWS) {
        assert_eq!(actual["name"], expected.name);
        assert_eq!(actual["purpose"], expected.manifest_purpose);
        let tools = actual["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{} recommended flow tools", expected.name));
        assert_eq!(tools.len(), expected.tools.len());
        for (actual_tool, expected_tool) in tools.iter().zip(expected.tools) {
            assert_eq!(actual_tool, expected_tool);
            let definition = lookup_tool_definition(expected_tool).unwrap_or_else(|| {
                panic!(
                    "{} recommended flow references unknown tool {expected_tool}",
                    expected.name
                )
            });
            assert!(
                definition.visibility.is_model_visible(),
                "{} recommended flow references hidden tool {expected_tool}",
                expected.name
            );
            assert!(
                is_model_visible_tool_name(expected_tool),
                "{} recommended flow references non-visible tool {expected_tool}",
                expected.name
            );
            assert!(
                manifest_categories
                    .values()
                    .any(|members| members.contains(*expected_tool)),
                "{} recommended flow references {expected_tool}, which is missing from compact manifest categories",
                expected.name
            );
        }
    }
}

#[tokio::test]
async fn tool_manifest_omits_recommended_flows_when_disabled() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: None,
            include_recommended_flows: false,
            include_risk_summary: true,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    assert!(
        result.output.get("recommended_flows").is_none(),
        "include_recommended_flows=false currently omits recommended_flows: {:?}",
        result.output
    );
}

#[tokio::test]
async fn tool_manifest_without_intent_keeps_compat_shape_and_lists_available_intents() {
    let runtime = test_runtime();
    let call = ToolCall::from_tool_name("tool_manifest", json!({})).unwrap();
    let result = runtime.dispatch(call).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["schema_version"], 1);
    assert_eq!(result.output["intent"], Value::Null);
    assert_eq!(result.output["filtered"], false);
    assert!(
        result.output["count"].as_u64().unwrap() > 20,
        "unfiltered tool_manifest should still return the broad compact tool set"
    );
    let available = string_array(&result.output["available_intents"], "available_intents");
    assert_eq!(
        available,
        vec![
            "coding".to_string(),
            "audit".to_string(),
            "exploration".to_string(),
            "file_transfer".to_string(),
            "release".to_string(),
            "discovery".to_string(),
        ]
    );
    assert_payload_keys_declared(
        "tool_manifest",
        &result.output,
        output_schema_properties(spec_named(&registered_tool_specs(), "tool_manifest")),
    );
}

#[tokio::test]
async fn tool_manifest_all_available_intents_parse_and_filter_through_tool_call() {
    let runtime = test_runtime();
    for intent in [
        "coding",
        "audit",
        "exploration",
        "file_transfer",
        "release",
        "discovery",
    ] {
        let call = ToolCall::from_tool_name(
            "tool_manifest",
            json!({
                "intent": intent,
                "include_recommended_flows": false,
                "include_risk_summary": false,
            }),
        )
        .unwrap_or_else(|error| panic!("{intent} must parse: {error}"));
        let result = runtime.dispatch(call).await;

        assert!(result.success, "{intent}: {:?}", result.error);
        assert_eq!(result.output["intent"], intent);
        assert_eq!(result.output["filtered"], true);
        assert!(
            result.output["returned_count"].as_u64().unwrap()
                < result.output["total_count"].as_u64().unwrap(),
            "{intent} must return a bounded manifest: {:?}",
            result.output
        );
    }
}

#[tokio::test]
async fn tool_manifest_intent_coding_returns_ranked_compact_tools() {
    use crate::model_surface::ModelSurface;
    use crate::tool_runtime::tool_definition::TOOL_MANIFEST_INTENTS;

    let runtime = test_runtime().with_model_surface(ModelSurface::AdaptiveRuntime);
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: Some("coding".to_string()),
            include_recommended_flows: false,
            include_risk_summary: true,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["intent"], "coding");
    assert_eq!(result.output["filtered"], true);
    assert_eq!(result.output["schema_version"], 1);

    let expected: Vec<&str> = TOOL_MANIFEST_INTENTS
        .iter()
        .find(|intent| intent.name == "coding")
        .unwrap()
        .tools
        .to_vec();
    let names: Vec<&str> = result.output["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names, expected,
        "coding intent tools must be ranked in table order"
    );
    assert_eq!(result.output["count"], expected.len() as u64);
    assert_eq!(result.output["filtered_count"], expected.len() as u64);
    assert!(
        result.output["total_count"].as_u64().unwrap() > expected.len() as u64,
        "total_count should remain the full runtime tool count"
    );
    assert!(result.output.get("recommended_flows").is_none());
    assert!(result.output["risk_summary"].is_object());
    assert_eq!(
        names,
        crate::tool_runtime::tool_definition::CODING_INTENT_TOOL_NAMES,
        "coding manifest must use its independent ordered selection surface"
    );
    for compatibility_or_overlap in [
        "read_file",
        "search_project_text",
        "git_diff",
        "git_diff_summary",
        "job_status",
        "job_log",
        "run_job",
        "apply_unified_diff",
    ] {
        assert!(
            !names.contains(&compatibility_or_overlap),
            "coding intent should not recommend {compatibility_or_overlap}: {names:?}"
        );
    }
    for gateway_specialist in ["apply_patch", "run_script", "cargo_fmt", "go_test"] {
        let tool = result.output["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == gateway_specialist)
            .unwrap_or_else(|| panic!("missing coding specialist {gateway_specialist}"));
        assert_eq!(tool["availability"], "gateway", "{gateway_specialist}");
        assert_eq!(
            tool["gateway_tool"],
            crate::model_surface::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
            "{gateway_specialist}"
        );
    }
    for direct in [
        "work_on_project",
        "search_project_texts",
        "read_files",
        "apply_text_edits",
        "run_process",
        "run_shell",
        "observe_jobs",
        "cargo_check",
        "cargo_test",
        "show_changes",
        "git_diff_hunks",
        "workspace_hygiene_check",
        "finish_coding_task",
    ] {
        let tool = result.output["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == direct)
            .unwrap_or_else(|| panic!("missing canonical coding tool {direct}"));
        assert_eq!(tool["availability"], "direct", "{direct}");
        assert!(tool["gateway_tool"].is_null(), "{direct}");
    }
}

#[tokio::test]
async fn tool_manifest_accepts_hyphenated_intent_alias() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: Some("Discovery".to_string()),
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["intent"], "discovery");
    let names: Vec<&str> = result.output["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "tool_manifest",
            "list_tools",
            "runtime_status",
            "list_runners",
            "list_projects",
            "project_overview",
        ]
    );
}

#[tokio::test]
async fn tool_manifest_unknown_intent_returns_structured_error() {
    let runtime = test_runtime();
    let call =
        ToolCall::from_tool_name("tool_manifest", json!({"intent": "not_a_real_intent"})).unwrap();
    let result = runtime.dispatch(call).await;
    assert!(!result.success, "unknown intent must fail");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("unknown tool_manifest intent"),
        "{:?}",
        result.error
    );
    assert_eq!(result.output["code"], "unknown_tool_manifest_intent");
    assert_eq!(result.output["intent"], "not_a_real_intent");
    let available = string_array(&result.output["available_intents"], "available_intents");
    assert!(available.contains(&"coding".to_string()));
    assert!(
        result.output.get("tools").is_none(),
        "unknown intent must not return a silent empty tool list: {:?}",
        result.output
    );
}

#[tokio::test]
async fn tool_manifest_intent_can_combine_with_category_filter() {
    // category is a strict ToolDefinition.category filter, not a flow/group filter.
    // apply_unified_diff is a patch-category mutation and is intentionally not a
    // validation-category tool merely because it performs an internal preflight.
    // Intent only ranks/filters discovery output and does not change tool behavior.
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: Some("validation".to_string()),
            intent: Some("coding".to_string()),
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["intent"], "coding");
    assert_eq!(result.output["category"], "validation");
    let names: Vec<&str> = result.output["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    // Coding intent keeps executable validation choices, while the read-only
    // validation_summary remains available through exact/category discovery.
    assert_eq!(
        names,
        vec!["cargo_fmt", "cargo_check", "cargo_test", "go_test"]
    );
}

#[tokio::test]
async fn audit_and_exploration_intents_exclude_shell_and_jobs() {
    for intent in ["audit", "exploration"] {
        let runtime = test_runtime();
        let result = runtime
            .dispatch(ToolCall::ToolManifest {
                tool_name: None,
                category: None,
                intent: Some(intent.to_string()),
                include_recommended_flows: false,
                include_risk_summary: false,
            })
            .await;
        assert!(result.success, "{intent}: {:?}", result.error);
        assert_eq!(result.output["intent"], intent);
        assert_eq!(result.output["filtered"], true);
        assert!(
            result.output["returned_count"].as_u64().unwrap()
                < result.output["total_count"].as_u64().unwrap(),
            "{intent} must not return the full manifest"
        );
        let names: Vec<&str> = result.output["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for forbidden in ["run_shell", "run_job"] {
            assert!(
                !names.contains(&forbidden),
                "{intent} must not include {forbidden}: {names:?}"
            );
        }
        assert!(
            !names.contains(&"start_coding_task"),
            "{intent} must not expose retired start_coding_task: {names:?}"
        );
        if intent == "audit" {
            for required in [
                "work_on_project",
                "project_overview",
                "read_files",
                "search_project_texts",
                "git_status",
                "git_review_summary",
                "git_diff_hunks",
                "git_log",
                "show_changes",
                "workspace_hygiene_check",
                "session_handoff_summary",
                "finish_coding_task",
                "tool_manifest",
            ] {
                assert!(
                    names.contains(&required),
                    "audit intent must include {required}: {names:?}"
                );
            }
            for compatibility_primitive in [
                "read_file",
                "search_project_text",
                "git_diff",
                "git_diff_summary",
            ] {
                assert!(
                    !names.contains(&compatibility_primitive),
                    "audit intent should keep {compatibility_primitive} exact-discovery-only: {names:?}"
                );
            }
            for tool in result.output["tools"].as_array().unwrap() {
                let name = tool["name"].as_str().unwrap();
                assert_ne!(
                    tool["risk"], "project_write",
                    "audit intent must exclude Project mutation: {tool:?}"
                );
                assert_ne!(
                    tool["risk"], "job_run",
                    "audit intent must exclude command/Job execution: {tool:?}"
                );
                assert_eq!(
                    tool["approval"], "none",
                    "audit intent must not introduce standard interactive approval: {tool:?}"
                );
                assert_eq!(
                    tool["shell_like"], false,
                    "audit intent must exclude shell-like tools: {tool:?}"
                );
                if name == "work_on_project" {
                    assert_eq!(tool["effect"], "mutate");
                    assert_eq!(tool["risk"], "workflow_manage");
                } else {
                    assert_eq!(
                        tool["effect"], "observe",
                        "only work_on_project may mutate bounded Workflow state in audit intent: {tool:?}"
                    );
                    assert_eq!(tool["read_only"], true, "{name}");
                }
            }
        }
    }
}

#[tokio::test]
async fn release_intent_includes_list_jobs_but_not_run_shell_or_run_job() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: Some("release".to_string()),
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["intent"], "release");
    let names: Vec<&str> = result.output["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"project_overview"),
        "release intent must not include project_overview: {names:?}"
    );
    assert!(
        names.contains(&"list_jobs"),
        "release intent should keep read-only list_jobs: {names:?}"
    );
    for forbidden in ["run_shell", "run_job"] {
        assert!(
            !names.contains(&forbidden),
            "release intent must not include {forbidden}: {names:?}"
        );
    }
}

fn assert_recommended_flows_subset_of_manifest_tools(manifest: &Value, context: &str) {
    let tool_names: std::collections::BTreeSet<&str> = manifest["tools"]
        .as_array()
        .expect("manifest tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    let flows = manifest["recommended_flows"]
        .as_array()
        .expect("recommended_flows");
    for flow in flows {
        let flow_name = flow["name"].as_str().unwrap_or("<unnamed>");
        let tools = flow["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{context}: flow {flow_name} tools"));
        assert!(
            !tools.is_empty(),
            "{context}: projected flow {flow_name} must not be empty"
        );
        for tool in tools {
            let tool = tool.as_str().expect("flow tool name");
            assert!(
                tool_names.contains(tool),
                "{context}: recommended_flows[{flow_name}] references invisible tool {tool}; visible={tool_names:?}"
            );
        }
    }
}

#[tokio::test]
async fn filtered_tool_manifest_recommended_flows_only_reference_returned_tools() {
    let runtime = test_runtime();

    // intent=coding
    let coding = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: Some("coding".to_string()),
            include_recommended_flows: true,
            include_risk_summary: false,
        })
        .await;
    assert!(coding.success, "{:?}", coding.error);
    assert_eq!(coding.output["filtered"], true);
    assert_recommended_flows_subset_of_manifest_tools(&coding.output, "intent=coding");
    let coding_names: Vec<&str> = coding.output["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        coding_names.contains(&"apply_text_edits"),
        "coding intent tools should expose canonical precise edits: {coding_names:?}"
    );
    assert!(
        coding_names.contains(&"apply_patch"),
        "coding intent tools should expose model-generated Codex patch mutation: {coding_names:?}"
    );
    assert!(
        !coding_names.contains(&"apply_unified_diff"),
        "external raw unified-diff compatibility path should stay outside ordinary coding intent: {coding_names:?}"
    );
    assert!(
        !coding_names.contains(&"replace_line_range"),
        "coding intent should not rank line compatibility tools: {coding_names:?}"
    );

    // single category filter
    let file_only = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: Some("file".to_string()),
            intent: None,
            include_recommended_flows: true,
            include_risk_summary: false,
        })
        .await;
    assert!(file_only.success, "{:?}", file_only.error);
    assert_eq!(file_only.output["filtered"], true);
    assert_recommended_flows_subset_of_manifest_tools(&file_only.output, "category=file");

    // multi-category startup-style filter without patch
    let no_patch = runtime
        .compact_tool_manifest_payload_bounded(
            Some(vec![
                "workflow".to_string(),
                "file".to_string(),
                "edit".to_string(),
                "validation".to_string(),
                "git".to_string(),
                "cleanup".to_string(),
            ]),
            Some("coding".to_string()),
            None,
        )
        .expect("startup-style no-patch manifest");
    assert_eq!(no_patch["filtered"], true);
    assert_recommended_flows_subset_of_manifest_tools(&no_patch, "startup no-patch");
    let no_patch_tools = serde_json::to_string(&no_patch["tools"]).unwrap();
    assert!(
        !no_patch_tools.contains("apply_patch"),
        "without patch category, tools must not include apply_patch"
    );
    assert!(
        !no_patch_tools.contains("apply_unified_diff"),
        "without patch category, tools must not include apply_unified_diff"
    );
    for forbidden in ["apply_patch", "apply_unified_diff"] {
        assert!(
            no_patch["recommended_flows"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|flow| flow["tools"].as_array().into_iter().flatten())
                .all(|tool| tool.as_str() != Some(forbidden)),
            "without patch category, recommended flow tool references must not include {forbidden}"
        );
    }

    // same filter with patch
    let with_patch = runtime
        .compact_tool_manifest_payload_bounded(
            Some(vec![
                "workflow".to_string(),
                "file".to_string(),
                "edit".to_string(),
                "patch".to_string(),
                "validation".to_string(),
                "git".to_string(),
                "cleanup".to_string(),
            ]),
            Some("coding".to_string()),
            None,
        )
        .expect("startup-style with-patch manifest");
    assert_recommended_flows_subset_of_manifest_tools(&with_patch, "startup with-patch");
    let with_patch_tools: Vec<&str> = with_patch["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        with_patch_tools.contains(&"apply_patch"),
        "with patch category, tools should include apply_patch: {with_patch_tools:?}"
    );
    assert!(
        !with_patch_tools.contains(&"apply_unified_diff"),
        "coding intent should keep external raw unified-diff mutation exact/category-only: {with_patch_tools:?}"
    );
    let edit_flow = with_patch["recommended_flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|flow| flow["name"] == "edit")
        .expect("edit flow");
    assert!(
        edit_flow["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool == "apply_patch"),
        "with patch category, edit flow may include apply_patch: {edit_flow}"
    );
    assert!(
        edit_flow["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool != "apply_unified_diff"),
        "filtered coding edit flow should not reintroduce apply_unified_diff: {edit_flow}"
    );

    // limit truncation after intent ordering
    let limited = runtime
        .compact_tool_manifest_payload_bounded(None, Some("coding".to_string()), Some(3))
        .expect("limit-truncated coding manifest");
    assert_eq!(limited["returned_count"], 3);
    assert_eq!(limited["truncated"], true);
    assert_recommended_flows_subset_of_manifest_tools(&limited, "intent=coding limit=3");
}

#[tokio::test]
async fn tool_manifest_exact_tool_returns_input_contract_without_output_schema() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: Some("cargo_test".to_string()),
            category: None,
            intent: None,
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_name"], "cargo_test");
    assert_eq!(result.output["count"], 1);
    assert_eq!(result.output["returned_count"], 1);
    let categories = result.output["categories"].as_object().unwrap();
    assert_eq!(categories.len(), 1);
    assert_eq!(categories["validation"], json!(["cargo_test"]));
    let contract = &result.output["contract"];
    assert_eq!(contract["name"], "cargo_test");
    assert!(contract["description"].as_str().is_some());
    assert_eq!(contract["input_schema"]["type"], "object");
    assert!(contract["input_schema"]["properties"]["package"].is_object());
    let sync_wait = &contract["input_schema"]["properties"]["sync_wait_secs"];
    assert_eq!(sync_wait["type"], "integer");
    assert_eq!(sync_wait["minimum"], 1);
    assert_eq!(sync_wait["maximum"], 60);
    assert!(contract["annotations"].is_object());
    let specs = registered_tool_specs();
    let manifest_spec = spec_named(&specs, "tool_manifest");
    let contract_schema =
        &manifest_spec.output_schema["properties"]["output"]["properties"]["contract"]["anyOf"][0];
    let contract_schema_properties = contract_schema["properties"].as_object().unwrap();
    for key in contract.as_object().unwrap().keys() {
        assert!(
            contract_schema_properties.contains_key(key),
            "tool_manifest exact contract runtime key {key} is missing from output_schema"
        );
    }
    for key in ["effect", "risk", "approval", "idempotency"] {
        assert!(
            contract_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|required| required == key),
            "tool_manifest exact contract output_schema must require {key}"
        );
    }
    assert_eq!(contract["availability"], "direct");
    assert!(contract["gateway_tool"].is_null());
    assert!(contract.get("output_schema").is_none());
    assert_eq!(result.output["tools"][0]["name"], "cargo_test");
    assert_eq!(result.output["tools"][0]["availability"], "direct");
    assert!(result.output["tools"][0]["gateway_tool"].is_null());
    assert!(result.output["tools"][0].get("input_schema").is_none());
}

#[tokio::test]
async fn tool_manifest_exact_persistent_shell_tool_surfaces_its_reuse_flow() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: Some("open_session_shell".to_string()),
            category: None,
            intent: None,
            include_recommended_flows: true,
            include_risk_summary: false,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    let flows = result.output["recommended_flows"]
        .as_array()
        .expect("exact persistent-shell recommended flows");
    let flow = flows
        .iter()
        .find(|flow| flow["name"] == "persistent_shell")
        .expect("persistent_shell flow");
    let purpose = flow["purpose"].as_str().expect("persistent_shell purpose");
    assert!(purpose.contains("Runner-local named SSH resource"));
    assert!(purpose.contains("not an arbitrary host"));
    assert!(purpose.contains("does not run WebCodex Runner"));
    for tool in [
        "update_session_context",
        "open_session_shell",
        "session_shell_exec",
        "session_shell_status",
        "close_session_shell",
        "run_process",
    ] {
        assert!(purpose.contains(tool), "persistent_shell purpose: {tool}");
    }
    // Exact manifests keep their returned tool set bounded to the requested tool,
    // while the flow purpose names the sibling tools needed to complete the route.
    assert_eq!(flow["tools"], json!(["open_session_shell"]));
}

#[tokio::test]
async fn tool_manifest_exact_fleet_tool_surfaces_exact_runner_targeting_route() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: Some("list_runners".to_string()),
            category: None,
            intent: None,
            include_recommended_flows: true,
            include_risk_summary: false,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    let flow = result.output["recommended_flows"]
        .as_array()
        .expect("exact fleet recommended flows")
        .iter()
        .find(|flow| flow["name"] == "discovery")
        .expect("discovery flow");
    let purpose = flow["purpose"].as_str().expect("discovery purpose");
    assert!(purpose.contains("runtime_status(client_id=...)"));
    assert!(purpose.contains("list_projects(client_id=...)"));
    assert!(purpose.contains("list_runners"));
    assert!(!purpose.contains("list_agents"));
    assert_eq!(flow["tools"], json!(["list_runners"]));
}

#[tokio::test]
async fn tool_manifest_projects_canonical_semantic_contracts() {
    let runtime = test_runtime();
    for (tool_name, effect, risk, approval, idempotency, read_only) in [
        (
            "read_files",
            "observe",
            "read_only",
            "none",
            "pure_read",
            true,
        ),
        (
            "close_session",
            "mutate",
            "session_collaborate",
            "none",
            "desired_state",
            false,
        ),
        (
            "coding_agent_start",
            "execute",
            "job_run",
            "standard",
            "keyed",
            false,
        ),
        (
            "coding_agent_cancel",
            "mutate",
            "run_control",
            "inherit_from_start",
            "desired_state",
            false,
        ),
    ] {
        let result = runtime
            .dispatch(ToolCall::ToolManifest {
                tool_name: Some(tool_name.to_string()),
                category: None,
                intent: None,
                include_recommended_flows: false,
                include_risk_summary: false,
            })
            .await;
        assert!(result.success, "{tool_name}: {:?}", result.error);
        let contract = &result.output["contract"];
        assert_eq!(contract["effect"], effect, "{tool_name}");
        assert_eq!(contract["risk"], risk, "{tool_name}");
        assert_eq!(contract["approval"], approval, "{tool_name}");
        assert_eq!(contract["idempotency"], idempotency, "{tool_name}");
        let compact = &result.output["tools"][0];
        assert_eq!(compact["effect"], effect, "{tool_name}");
        assert_eq!(compact["risk"], risk, "{tool_name}");
        assert_eq!(compact["approval"], approval, "{tool_name}");
        assert_eq!(compact["idempotency"], idempotency, "{tool_name}");
        assert_eq!(compact["read_only"], read_only, "{tool_name}");
    }
}

#[tokio::test]
async fn tool_manifest_surface_routing_metadata_tracks_current_model_surface() {
    use crate::model_surface::ModelSurface;

    for (surface, tool_name, availability, gateway_tool) in [
        (
            ModelSurface::LocalCoding,
            "computer_snapshot",
            "unavailable",
            None,
        ),
        (ModelSurface::AdaptiveRuntime, "run_process", "direct", None),
        (ModelSurface::AdaptiveRuntime, "run_shell", "direct", None),
        (
            ModelSurface::AdaptiveRuntime,
            "import_conversation_files_to_project",
            "direct",
            None,
        ),
        (
            ModelSurface::AdaptiveRuntime,
            "export_project_artifact",
            "direct",
            None,
        ),
        (
            ModelSurface::AdaptiveRuntime,
            "session_discussion_summary",
            "direct",
            None,
        ),
        (ModelSurface::AdaptiveRuntime, "list_jobs", "direct", None),
        (
            ModelSurface::AdaptiveRuntime,
            "git_diff_hunks",
            "direct",
            None,
        ),
        (
            ModelSurface::AdaptiveRuntime,
            "run_script",
            "gateway",
            Some("call_runtime_tool"),
        ),
        (
            ModelSurface::AdaptiveRuntime,
            "save_project_artifact",
            "gateway",
            Some("call_runtime_tool"),
        ),
        (
            ModelSurface::AdaptiveRuntime,
            "read_project_artifact",
            "gateway",
            Some("call_runtime_tool"),
        ),
        (
            ModelSurface::AdaptiveRuntime,
            "artifact_upload_begin",
            "gateway",
            Some("call_runtime_tool"),
        ),
        (
            ModelSurface::FullOperatorRuntime,
            "run_script",
            "direct",
            None,
        ),
    ] {
        let runtime = test_runtime().with_model_surface(surface);
        let result = runtime
            .dispatch(ToolCall::ToolManifest {
                tool_name: Some(tool_name.to_string()),
                category: None,
                intent: None,
                include_recommended_flows: false,
                include_risk_summary: false,
            })
            .await;
        assert!(
            result.success,
            "{surface:?} {tool_name}: {:?}",
            result.error
        );
        assert_eq!(result.output["contract"]["availability"], availability);
        assert_eq!(
            result.output["contract"]["gateway_tool"],
            gateway_tool.map_or(Value::Null, |name| json!(name))
        );
        assert_eq!(result.output["tools"][0]["availability"], availability);
        assert_eq!(
            result.output["tools"][0]["gateway_tool"],
            gateway_tool.map_or(Value::Null, |name| json!(name))
        );
    }
}

#[tokio::test]
async fn tool_manifest_operator_extensions_require_explicit_family_capabilities() {
    use crate::model_surface::ModelSurface;
    use crate::tool_runtime::kernel::ToolProtocolCapabilities;

    let runtime = test_runtime().with_model_surface(ModelSurface::AdaptiveRuntime);
    let manifest = |tool_name: &'static str, capabilities: ToolProtocolCapabilities| {
        runtime.tool_manifest(
            Some(tool_name.to_string()),
            None,
            None,
            false,
            false,
            capabilities,
        )
    };

    let no_capability = manifest("skill_list", ToolProtocolCapabilities::default()).await;
    assert!(!no_capability.success);
    assert_eq!(no_capability.output["code"], "unknown_tool_manifest_tool");

    let skill_only = ToolProtocolCapabilities {
        skill_runtime: true,
        ..Default::default()
    };
    let skill = manifest("skill_list", skill_only).await;
    assert!(skill.success, "{:?}", skill.error);
    assert_eq!(skill.output["contract"]["availability"], "gateway");
    assert_eq!(
        skill.output["contract"]["gateway_tool"],
        "call_runtime_tool"
    );
    for hidden_without_skill_cap in ["skill_install", "memory_search", "read_tool_trace"] {
        let hidden = manifest(hidden_without_skill_cap, skill_only).await;
        assert!(
            !hidden.success,
            "{hidden_without_skill_cap} leaked via skill runtime capability"
        );
        assert_eq!(hidden.output["code"], "unknown_tool_manifest_tool");
    }

    let memory_only = ToolProtocolCapabilities {
        memory_surface: true,
        ..Default::default()
    };
    assert!(manifest("memory_search", memory_only).await.success);
    assert!(!manifest("skill_list", memory_only).await.success);
    assert!(!manifest("read_tool_trace", memory_only).await.success);

    let diagnostic_only = ToolProtocolCapabilities {
        trace_diagnostics: true,
        ..Default::default()
    };
    assert!(manifest("read_tool_trace", diagnostic_only).await.success);
    assert!(!manifest("memory_search", diagnostic_only).await.success);

    let local = test_runtime().with_model_surface(ModelSurface::LocalCoding);
    let local_with_capability = local
        .tool_manifest(
            Some("skill_list".to_string()),
            None,
            None,
            false,
            false,
            skill_only,
        )
        .await;
    assert!(
        !local_with_capability.success,
        "unsupported Local Coding surface must not expose operator extensions even with inconsistent internal capability input"
    );
    assert_eq!(
        local_with_capability.output["code"],
        "unknown_tool_manifest_tool"
    );

    let management_only = ToolProtocolCapabilities {
        skill_management: true,
        ..Default::default()
    };
    assert!(manifest("skill_install", management_only).await.success);
    assert!(!manifest("skill_list", management_only).await.success);
}

#[tokio::test]
async fn tool_manifest_is_not_a_project_connector_runtime_route() {
    let runtime = test_runtime()
        .with_runtime_exposure(crate::model_surface::RuntimeExposure::ProjectConnector);
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: Some("run_process".to_string()),
            category: None,
            intent: None,
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("project_connector")));
}

#[tokio::test]
async fn tool_manifest_exact_tool_fails_closed_for_unknown_or_mixed_filters() {
    let runtime = test_runtime();
    let unknown = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: Some("not_a_real_webcodex_tool".to_string()),
            category: None,
            intent: None,
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(!unknown.success);
    assert_eq!(unknown.output["code"], "unknown_tool_manifest_tool");

    let mixed = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: Some("cargo_test".to_string()),
            category: Some("validation".to_string()),
            intent: None,
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(!mixed.success);
    assert_eq!(mixed.output["code"], "tool_manifest_exact_filter_conflict");
}

#[tokio::test]
async fn unfiltered_tool_manifest_keeps_full_recommended_flows() {
    use crate::tool_runtime::tool_definition::TOOL_RECOMMENDED_FLOWS;

    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: None,
            include_recommended_flows: true,
            include_risk_summary: true,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["filtered"], false);
    let flows = result.output["recommended_flows"]
        .as_array()
        .expect("unfiltered recommended_flows");
    assert_eq!(
        flows.len(),
        TOOL_RECOMMENDED_FLOWS.len(),
        "unfiltered recommended_flows must keep full global set"
    );
    let serialized = result.output["recommended_flows"]
        .to_string()
        .to_lowercase();
    assert!(
        serialized.contains("run_shell")
            && serialized.contains("shell semantics or one tightly related observation goal")
            && serialized.contains("do not combine validation, commit, push, deploy, restart"),
        "unfiltered flows must keep run_shell selection and effect-boundary guidance: {serialized}"
    );
}
