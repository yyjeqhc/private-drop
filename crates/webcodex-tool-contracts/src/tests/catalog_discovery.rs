use super::*;
use std::collections::BTreeSet;

fn registered_tool_categories() -> Value {
    Value::Object(
        TOOL_DISCOVERY_GROUPS
            .iter()
            .map(|group| {
                (
                    group.name.to_string(),
                    Value::Array(
                        group
                            .tools
                            .iter()
                            .map(|tool| Value::String((*tool).to_string()))
                            .collect(),
                    ),
                )
            })
            .collect(),
    )
}

fn recommended_flows() -> Vec<&'static str> {
    TOOL_RECOMMENDED_FLOWS
        .iter()
        .map(|flow| flow.summary)
        .collect()
}

#[test]
fn model_tool_contracts_do_not_teach_retired_runner_as_agent_prose() {
    for spec in registered_tool_specs() {
        let rendered = format!(
            "{}\n{}\n{}",
            spec.description, spec.input_schema, spec.output_schema
        )
        .to_ascii_lowercase();
        for retired in [
            "agent-registered",
            "agent registry",
            "owning registered agent",
            "agent project config",
            "an runner",
        ] {
            assert!(
                !rendered.contains(retired),
                "{} still exposes retired Runner-as-Agent prose: {retired}",
                spec.name
            );
        }
    }
}

#[test]
fn list_tools_schema_exposes_bounded_discovery_fields() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "list_tools");
    let props = spec.input_schema["properties"].as_object().unwrap();
    assert_schema_fields!(
        props,
        "list_tools input schema",
        present: ["category", "features", "summary_only", "limit"]
    );
    assert!(spec.input_schema["required"].as_array().unwrap().is_empty());
    let output = spec.output_schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    assert_schema_fields!(
        output,
        "list_tools output schema",
        present: [
            "category", "features", "limit", "returned_count", "total_count",
            "filtered_count", "limit_applied", "requested_limit", "truncation_reason",
            "truncated", "categories", "recommended_flows",
        ]
    );
}

#[test]
fn tool_manifest_schema_exposes_compact_discovery_fields() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "tool_manifest");
    let props = spec.input_schema["properties"].as_object().unwrap();
    assert_schema_fields!(
        props,
        "tool_manifest input schema",
        present: ["category", "intent", "include_recommended_flows", "include_risk_summary"]
    );
    let risk_summary_description = props["include_risk_summary"]["description"]
        .as_str()
        .expect("include_risk_summary description");
    assert!(risk_summary_description.contains("where the selected projection exposes it"));
    assert!(risk_summary_description.contains("Unfiltered/full discovery can return the aggregate"));
    assert!(risk_summary_description.contains("sparse filtered discovery omits it"));
    assert!(risk_summary_description
        .contains("does not change authority, permission, or tool behavior"));
    assert!(!risk_summary_description.contains("Include risk_summary in the output"));
    let output = spec.output_schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    assert_schema_fields!(
        output,
        "tool_manifest output schema",
        present: [
            "schema_version", "count", "tool_count", "filtered_count", "category", "intent",
            "available_intents", "filtered", "categories_requested", "limit", "returned_count",
            "total_count", "limit_applied", "requested_limit", "truncation_reason", "truncated",
            "categories", "tools", "risk_summary", "recommended_flows",
        ]
    );
}

#[test]
fn tool_recommended_flows_reference_visible_defined_tools() {
    let expected_summaries = TOOL_RECOMMENDED_FLOWS
        .iter()
        .map(|flow| {
            assert!(!flow.name.trim().is_empty());
            assert!(!flow.manifest_purpose.trim().is_empty(), "{}", flow.name);
            assert!(flow.summary.chars().count() <= 300, "{}", flow.name);
            assert!(!flow.tools.is_empty(), "{}", flow.name);
            for tool in flow.tools {
                let definition = lookup_tool_definition(tool)
                    .unwrap_or_else(|| panic!("{} references unknown tool {tool}", flow.name));
                assert!(
                    definition.visibility.is_model_visible(),
                    "{}: {tool}",
                    flow.name
                );
                assert!(is_model_visible_tool_name(tool), "{}: {tool}", flow.name);
            }
            flow.summary
        })
        .collect::<Vec<_>>();
    assert_eq!(recommended_flows(), expected_summaries);
}

#[test]
fn edit_recommended_flow_pairs_reads_with_guarded_exact_edits() {
    let flow = TOOL_RECOMMENDED_FLOWS
        .iter()
        .find(|flow| flow.name == "edit")
        .expect("edit recommended flow");
    assert_eq!(flow.tools.first().copied(), Some("read_files"));
    assert_eq!(flow.tools.get(1).copied(), Some("apply_text_edits"));
    assert_eq!(flow.tools.get(2).copied(), Some("apply_patch"));
    assert!(flow.summary.starts_with(
        "Edit: after read_file/read_files, apply_text_edits with current SHA is the default"
    ));
    assert!(flow.summary.contains("even when many lines change"));
    assert!(flow.summary.contains("Use apply_patch only when"));
    let guidance = format!("{}\n{}", flow.summary, flow.manifest_purpose).to_lowercase();
    for phrase in [
        "canonical default even when many lines change",
        "stable unique containing function/impl/type/test/module context",
        "matching_mode_rejected",
        "do not weaken the guard or switch to first_match",
        "prefer apply_text_edits if exact edits are easy",
        "bounded read_files recovery",
        "preserve the requested guard",
        "unique retries use matching_mode=unique with unique context",
        "exact_unique retries remain matching_mode=exact_unique",
        "never downgrade the stale-context/concurrency fence",
        "context_mismatch requires bounded reread",
        "never blind retry",
    ] {
        assert!(
            guidance.contains(phrase),
            "edit flow should mention {phrase}: {guidance}"
        );
    }
}

#[test]
fn execution_lifetime_flow_routes_runner_owned_and_supervisor_owned_work() {
    let flow = TOOL_RECOMMENDED_FLOWS
        .iter()
        .find(|flow| flow.name == "execution_lifetime")
        .expect("execution_lifetime recommended flow");
    assert_eq!(
        flow.tools,
        &[
            "run_process",
            "run_job",
            "run_detached_process",
            "observe_jobs",
            "stop_job",
        ]
    );
    let text = format!("{}\n{}", flow.summary, flow.manifest_purpose).to_ascii_lowercase();
    for phrase in [
        "runner-owned",
        "outlive the current runner process",
        "run_detached_process",
        "supervisor-owned",
        "replacement runner",
    ] {
        assert!(
            text.contains(phrase),
            "execution_lifetime flow should mention {phrase}: {text}"
        );
    }
}

#[test]
fn tool_categories_and_recommended_flows_are_well_formed() {
    let categories = registered_tool_categories();
    let names = registered_tool_names();
    for (cat, members) in categories.as_object().unwrap() {
        let arr = members.as_array().unwrap();
        assert!(!arr.is_empty(), "category '{cat}' must not be empty");
        for member in arr {
            let name = member.as_str().unwrap();
            assert!(
                names.iter().any(|candidate| candidate == name),
                "{cat}: {name}"
            );
        }
    }
    for cat in [
        TOOL_DISCOVERY_GROUP_INSPECT,
        TOOL_DISCOVERY_GROUP_GIT,
        TOOL_DISCOVERY_GROUP_REVIEW,
        TOOL_DISCOVERY_GROUP_VALIDATION,
        TOOL_DISCOVERY_GROUP_PATCH,
        TOOL_DISCOVERY_GROUP_SHELL,
        TOOL_DISCOVERY_GROUP_JOBS,
        TOOL_DISCOVERY_GROUP_RUNTIME,
        TOOL_DISCOVERY_GROUP_CLEANUP,
        TOOL_DISCOVERY_GROUP_CHECKPOINT,
    ] {
        assert!(
            categories.as_object().unwrap().contains_key(cat),
            "missing category {cat}"
        );
    }
    let validation = categories[TOOL_DISCOVERY_GROUP_VALIDATION]
        .as_array()
        .unwrap();
    for name in ["cargo_fmt", "cargo_check", "cargo_test"] {
        assert!(validation.iter().any(|value| value == name));
    }
    let review = categories[TOOL_DISCOVERY_GROUP_REVIEW].as_array().unwrap();
    assert!(review.iter().any(|value| value == "git_diff_hunks"));
    assert!(review
        .iter()
        .any(|value| value == "workspace_hygiene_check"));
    assert!(review.iter().any(|value| value == "git_log"));
    let inspect = categories[TOOL_DISCOVERY_GROUP_INSPECT].as_array().unwrap();
    for name in [
        "read_file",
        "run_shell",
        "search_project_text",
        "show_changes",
    ] {
        assert!(
            inspect.iter().any(|value| value == name),
            "inspect category: {name}"
        );
    }
    let edit = categories[TOOL_DISCOVERY_GROUP_EDIT].as_array().unwrap();
    let edit_prefix = edit
        .iter()
        .take(5)
        .map(|value| value.as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        edit_prefix,
        vec![
            "apply_text_edits",
            "apply_patch",
            "apply_unified_diff",
            "write_project_file",
            "save_project_artifact"
        ]
    );
    let flows = recommended_flows();
    assert!(!flows.is_empty());
    for flow in &flows {
        assert!(flow.chars().count() <= 300, "flow too long: {flow}");
    }
    let joined_flows = flows.join("\n").to_lowercase();
    for phrase in [
        "if the user gives an exact runner client_id",
        "runtime_status/list_projects for that runner",
        "persistent shell: primarily reuse one shell for repeated commands on an active named ssh resource",
        "keep remote shell state",
        "ssh_resource list/register -> restart -> list -> bind -> open/reuse",
        "local persistent shell is only for true same-process state",
        "one-shot ssh uses run_process",
        "execution lifetime: run_process/run_job stay runner-owned",
        "outlive the current runner process",
        "discover run_detached_process",
        "supervisor-owned job",
        "inspect: use search_project_text and read_file before editing",
        "run_shell with rg or git grep is the diagnostic escape hatch",
        "edit: after read_file/read_files, apply_text_edits with current sha is the default",
        "even when many lines change",
        "use apply_patch only when contextual/large multi-hunk patch form is materially clearer",
        "external diffs use apply_unified_diff",
        "validate: use cargo_check / cargo_test / go_test",
        "raw run_shell is a bounded escape hatch",
        "not the primary validation path",
        "copy show_changes.head.commit",
        "review: start with show_changes for the bounded worktree overview",
        "if hunks truncate, continue/focus with git_diff_hunks",
        "handoff: use session_summary / session_handoff_summary",
    ] {
        assert!(
            joined_flows.contains(phrase),
            "recommended flows should mention {phrase}"
        );
    }
}

#[test]
fn discovery_and_persistent_shell_flows_route_high_value_adaptive_tools() {
    let discovery = TOOL_RECOMMENDED_FLOWS
        .iter()
        .find(|flow| flow.name == "discovery")
        .expect("discovery recommended flow");
    for tool in ["runtime_status", "list_runners", "list_projects"] {
        assert!(discovery.tools.contains(&tool), "discovery: {tool}");
    }
    assert!(discovery.summary.contains("exact Runner client_id"));
    assert!(discovery.summary.contains("before treating it as absent"));
    for phrase in [
        "runtime_status(client_id=...)",
        "list_projects(client_id=...)",
        "list_runners",
    ] {
        assert!(
            discovery.manifest_purpose.contains(phrase),
            "discovery: {phrase}"
        );
    }
    assert!(!discovery.tools.contains(&"list_agents"));
    assert!(!discovery.manifest_purpose.contains("list_agents"));

    let persistent = TOOL_RECOMMENDED_FLOWS
        .iter()
        .find(|flow| flow.name == "persistent_shell")
        .expect("persistent shell recommended flow");
    assert_eq!(persistent.tools.first().copied(), Some("ssh_resource"));
    assert_eq!(
        persistent.tools.get(1).copied(),
        Some("update_session_context")
    );
    assert_eq!(persistent.tools.get(2).copied(), Some("open_session_shell"));
    assert_eq!(persistent.tools.get(3).copied(), Some("session_shell_exec"));
    assert!(persistent.tools.contains(&"session_shell_status"));
    assert!(persistent.tools.contains(&"close_session_shell"));
    assert!(persistent.tools.contains(&"run_process"));
    assert!(persistent.summary.contains("primarily reuse one shell"));
    assert!(persistent.summary.contains("active named SSH resource"));
    assert!(persistent.summary.contains("remote shell state"));
    assert!(persistent.summary.contains("ssh_resource list/register"));
    assert!(persistent
        .summary
        .contains("restart -> list -> bind -> open/reuse"));
    assert!(persistent
        .summary
        .contains("Local persistent shell is only for true same-process state"));
    assert!(persistent.summary.contains("one-shot SSH uses run_process"));
    assert!(persistent
        .manifest_purpose
        .contains("SSH target does not run WebCodex Runner"));
    assert!(persistent.manifest_purpose.contains("ssh_resource list"));
    assert!(persistent
        .manifest_purpose
        .contains("ssh_resource register"));
    for phrase in [
        "ssh-resource-primary",
        "session_shell_exec repeatedly preserves remote cwd/env/exports/functions/umask",
        "local persistent shell remains supported only when same local-process state is required",
        "several ordinary local commands are not enough",
        "explicit one-shot/no-persistence ssh",
    ] {
        assert!(
            persistent
                .manifest_purpose
                .to_ascii_lowercase()
                .contains(phrase),
            "persistent_shell should mention {phrase}: {}",
            persistent.manifest_purpose
        );
    }
    for tool in [
        "ssh_resource",
        "update_session_context",
        "open_session_shell",
        "session_shell_exec",
        "session_shell_status",
        "close_session_shell",
        "run_process",
    ] {
        assert!(
            persistent.manifest_purpose.contains(tool),
            "persistent_shell: {tool}"
        );
    }
}

#[test]
fn tool_categories_include_edit_group() {
    let categories = registered_tool_categories();
    let edit = categories[TOOL_DISCOVERY_GROUP_EDIT]
        .as_array()
        .expect("edit category present");
    for present in [
        "apply_text_edits",
        "apply_patch",
        "write_project_file",
        "apply_unified_diff",
    ] {
        assert!(edit.iter().any(|value| value == present));
    }
    for removed in ["replace_in_file", "replace_line_range", "insert_at_line"] {
        assert!(!edit.iter().any(|value| value == removed));
    }
}

#[test]
fn tool_categories_include_projects_with_management_tools() {
    let categories = registered_tool_categories();
    let projects = categories[TOOL_DISCOVERY_GROUP_PROJECTS]
        .as_array()
        .expect("projects category present");
    assert!(projects.iter().any(|value| value == "register_project"));
    assert!(projects.iter().any(|value| value == "create_project"));
}

#[test]
fn tool_manifest_intents_reference_only_known_model_visible_tools() {
    let expected = ["coding", "audit", "exploration", "release", "discovery"];
    let names = TOOL_MANIFEST_INTENTS
        .iter()
        .map(|intent| intent.name)
        .collect::<Vec<_>>();
    assert_eq!(names, expected);
    assert_eq!(available_tool_manifest_intent_names(), names);
    for name in &names {
        let resolved = resolve_tool_manifest_intent(name)
            .unwrap_or_else(|unknown| panic!("available intent {unknown} must resolve"))
            .unwrap_or_else(|| panic!("available intent {name} must not resolve as empty"));
        assert_eq!(resolved.name, *name);
    }

    let mut seen = BTreeSet::new();
    for intent in TOOL_MANIFEST_INTENTS {
        assert!(!intent.tools.is_empty(), "{}", intent.name);
        assert!(seen.insert(intent.name), "duplicate intent {}", intent.name);
        for tool in intent.tools {
            assert!(is_known_tool_name(tool), "{}: {tool}", intent.name);
            assert!(is_model_visible_tool_name(tool), "{}: {tool}", intent.name);
            if matches!(intent.name, "audit" | "exploration" | "release") {
                assert_ne!(*tool, "run_shell", "{}", intent.name);
                assert_ne!(*tool, "run_job", "{}", intent.name);
            }
        }
    }
}

#[test]
fn project_overview_manifest_profiles_match_intended_workflows() {
    for intent in ["coding", "audit", "exploration", "discovery"] {
        let profile = TOOL_MANIFEST_INTENTS
            .iter()
            .find(|profile| profile.name == intent)
            .unwrap_or_else(|| panic!("missing {intent} intent"));
        assert!(profile.tools.contains(&"project_overview"), "{intent}");
    }
    let release = TOOL_MANIFEST_INTENTS
        .iter()
        .find(|profile| profile.name == "release")
        .expect("release intent");
    assert!(!release.tools.contains(&"project_overview"));
}

#[test]
fn coding_intent_matches_local_coding_canonical_tools() {
    let coding = TOOL_MANIFEST_INTENTS
        .iter()
        .find(|intent| intent.name == "coding")
        .expect("coding intent");
    assert_eq!(coding.tools, LOCAL_CODING_TOOL_NAMES);
    assert_eq!(coding.tools.first().copied(), Some("work_on_project"));
    assert_eq!(coding.tools.last().copied(), Some("finish_coding_task"));
    assert!(!coding.tools.contains(&"start_coding_task"));
    let apply_patch_position = coding
        .tools
        .iter()
        .position(|tool| *tool == "apply_patch")
        .unwrap();
    let apply_text_edits_position = coding
        .tools
        .iter()
        .position(|tool| *tool == "apply_text_edits")
        .unwrap();
    assert!(apply_text_edits_position < apply_patch_position);
    for middle in [
        "project_overview",
        "apply_patch",
        "apply_text_edits",
        "apply_unified_diff",
        "cargo_test",
        "show_changes",
    ] {
        let position = coding
            .tools
            .iter()
            .position(|tool| *tool == middle)
            .unwrap();
        assert!(position > 0 && position + 1 < coding.tools.len());
    }
    assert!(coding.tools.contains(&"run_shell"));
    assert!(coding.tools.contains(&"run_job"));
    assert!(!coding.tools.contains(&"git_restore_paths"));
    assert!(!coding.tools.contains(&"discard_untracked"));
}
