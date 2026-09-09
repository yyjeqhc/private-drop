use super::support::*;
use crate::auth::AuthContext;
use crate::runner_protocol::RunnerPolicySummary;
use crate::tool_runtime::handoff::VALIDATION_IDENTITY_REUSE_ACTION;
use crate::tool_runtime::metadata::lookup_tool_metadata;
use crate::tool_runtime::sessions::SessionTransport;
use crate::tool_runtime::validation_parser::VALIDATION_OUTPUT_METADATA_ABSENT_REASON;
use crate::tool_runtime::{
    is_known_tool_name, registered_tool_specs, SessionMode, StartupDetail, ToolCall, ToolResult,
    ToolRuntime,
};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

async fn service_agent_task_until_finished(
    runtime: &ToolRuntime,
    client_id: &str,
    task: &tokio::task::JoinHandle<ToolResult>,
    label: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !task.is_finished() {
        assert!(
            Instant::now() < deadline,
            "{label} did not finish within the 10-second test deadline"
        );
        if let Some(request) = probe_patch_agent_request(runtime, client_id).await {
            complete_agent_request_by_running_locally(runtime, client_id, request).await;
        } else {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

#[test]
fn coding_task_tools_are_registered_in_metadata_and_openapi() {
    let specs = registered_tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert!(
        !names.contains(&"resolve_or_register_project"),
        "internal Runner path resolution must not become model-visible"
    );
    assert!(!is_known_tool_name("resolve_or_register_project"));
    assert!(!is_known_tool_name("prepare_managed_worktree"));
    assert!(
        !names.contains(&"prepare_managed_worktree"),
        "managed worktree preparation must remain ModelHidden"
    );

    assert!(
        !is_known_tool_name("start_coding_task"),
        "retired start_coding_task must not remain a current tool identity"
    );
    assert!(lookup_tool_metadata("start_coding_task").is_none());
    assert!(
        crate::tool_runtime::tool_definition::lookup_tool_definition("start_coding_task").is_none()
    );

    for name in ["work_on_project", "finish_coding_task"] {
        assert!(is_known_tool_name(name), "{name} missing from known names");
        let metadata = lookup_tool_metadata(name).expect("metadata");
        assert!(!metadata.destructive);
        assert!(!metadata.shell_like);
        assert_eq!(
            metadata.authority,
            crate::tool_runtime::metadata::ToolAuthorityPolicy::Require("runtime:read")
        );
    }
    let start_metadata = lookup_tool_metadata("work_on_project").expect("work metadata");
    assert_eq!(
        start_metadata.effect,
        crate::tool_runtime::metadata::ToolEffect::Mutate
    );
    assert_eq!(
        start_metadata.risk,
        crate::tool_runtime::metadata::ToolRisk::WorkflowManage
    );
    assert_eq!(
        start_metadata.approval,
        crate::tool_runtime::metadata::ToolApprovalPolicy::None
    );
    let finish_metadata = lookup_tool_metadata("finish_coding_task").expect("finish metadata");
    assert_eq!(
        finish_metadata.effect,
        crate::tool_runtime::metadata::ToolEffect::Observe
    );
    assert_eq!(
        finish_metadata.risk,
        crate::tool_runtime::metadata::ToolRisk::Read
    );
    assert_eq!(
        finish_metadata.idempotency,
        crate::tool_runtime::metadata::ToolIdempotency::PureRead
    );
    assert!(!names.contains(&"start_coding_task"));
    assert!(names.contains(&"work_on_project"));
    assert!(names.contains(&"finish_coding_task"));

    let retired = ToolCall::from_tool_name("start_coding_task", json!({"project": "demo"}))
        .expect_err("retired start_coding_task must be an ordinary unknown tool");
    assert!(retired.contains("unknown tool 'start_coding_task'"));
    let finish = specs
        .iter()
        .find(|spec| spec.name == "finish_coding_task")
        .expect("finish_coding_task public spec");
    let finish_session_description = finish.input_schema["properties"]["session_id"]["description"]
        .as_str()
        .unwrap();
    assert!(finish_session_description.contains("current coding task"));
    assert!(finish_session_description.contains("compatible Session bootstrap"));
    assert!(!finish_session_description.contains("work_on_project"));
    let work = spec_named(&specs, "work_on_project");
    let work_props = work.input_schema["properties"].as_object().unwrap();
    for advanced in [
        "temporary_project_name",
        "deny_write_tools",
        "deny_shell_tools",
        "execution_context",
        "detail",
        "resume_session_id",
        "bind_current",
        "new_session",
    ] {
        assert!(
            !work_props.contains_key(advanced),
            "work_on_project must not grow advanced start knob {advanced}"
        );
    }
    assert_eq!(work_props["mode"]["enum"], json!(["checkout", "worktree"]));
    assert_eq!(work_props["mode"]["default"], "checkout");
    assert!(work_props["base_ref"]["description"]
        .as_str()
        .is_some_and(|description| description.contains("Runner resolves")));
    assert!(work.description.contains("managed worktree"));
    let work_output = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    assert!(work_output["properties"]["output"]["properties"]
        .as_object()
        .is_some_and(|properties| properties.contains_key("worktree")));
    let start_output =
        crate::tool_runtime::registry::coding_workflow_diagnostic_output_schema_for_test();
    let startup_variants = start_output["properties"]["output"]["oneOf"]
        .as_array()
        .expect("start output should expose detail-specific variants");
    assert_eq!(startup_variants.len(), 3);
    let standard = startup_variants
        .iter()
        .find(|variant| variant["properties"]["detail"]["const"] == "standard")
        .expect("standard startup brief schema");
    assert!(
        standard["properties"]
            .as_object()
            .unwrap()
            .contains_key("instructions"),
        "standard startup brief should include instructions"
    );
    assert!(
        standard["properties"]
            .as_object()
            .unwrap()
            .contains_key("workflow"),
        "standard startup brief should include built-in workflow"
    );
    assert!(
        standard["properties"]
            .as_object()
            .unwrap()
            .contains_key("startup_verdict"),
        "coding workflow diagnostic schema should include startup_verdict"
    );
    assert!(
        standard["properties"]
            .as_object()
            .unwrap()
            .contains_key("project_resolution"),
        "coding workflow diagnostic schema should include project_resolution"
    );
    assert!(
        !standard["properties"]
            .as_object()
            .unwrap()
            .contains_key("runtime_status"),
        "standard startup brief must omit full runtime diagnostics"
    );
    let finish = spec_named(&specs, "finish_coding_task");
    assert_eq!(required_fields(finish), vec!["project", "session_id"]);
    let finish_props = finish.input_schema["properties"].as_object().unwrap();
    assert!(
        finish_props.contains_key("include_workspace"),
        "finish_coding_task input schema should accept include_workspace"
    );
    assert!(
        !required_fields(finish)
            .iter()
            .any(|field| field == "include_workspace"),
        "include_workspace must remain optional"
    );
    let finish_output = crate::tool_runtime::registry::output_schema_for_tool("finish_coding_task");
    let finish_output_props = finish_output["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    assert!(!finish_output_props.contains_key("verdict"));
    assert!(!finish_output_props.contains_key("finish_verdict"));
    for field in [
        "facts",
        "hard_blockers",
        "advisories",
        "task_outcome",
        "evidence_history",
        "evidence_integrity",
    ] {
        assert!(
            finish_output_props.contains_key(field),
            "finish_coding_task output schema should include {field}"
        );
    }

    let openapi = crate::openapi::build_openapi_spec();
    let tool_call = &openapi["components"]["schemas"]["ToolCallRequest"];
    let tool_desc = tool_call["properties"]["tool"]["description"]
        .as_str()
        .unwrap();
    assert!(!tool_desc.contains("start_coding_task"));
    assert!(tool_desc.contains("work_on_project"));
    assert!(tool_desc.contains("finish_coding_task"));
    let properties = tool_call["properties"].as_object().unwrap();
    for field in [
        "project",
        "client_id",
        "path",
        "mode",
        "base_ref",
        "execution_context",
        "include_hygiene",
        "include_handoff",
        "include_workspace",
        "include_validation_summary",
        "include_validation",
        "summary_only",
    ] {
        assert!(
            properties.contains_key(field),
            "ToolCallRequest missing model-visible flattened field {field}"
        );
    }
    for field in [
        "temporary_project_name",
        "deny_write_tools",
        "deny_shell_tools",
        "detail",
        "resume_session_id",
        "bind_current",
        "new_session",
    ] {
        assert!(
            !properties.contains_key(field),
            "ToolCallRequest must not expose hidden start-only flattened field {field}"
        );
    }
    assert!(!tool_call["description"]
        .as_str()
        .unwrap()
        .contains("start_coding_task"));
    for field in [
        "expected_failure",
        "expected_failure_kind",
        "assertion_name",
    ] {
        assert!(
            !properties.contains_key(field),
            "ToolCallRequest must not publish recorder metadata field {field}"
        );
    }
    let operation_count: usize = openapi["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|methods| methods.as_object().unwrap().len())
        .sum();
    assert_eq!(operation_count, 22, "no dedicated OpenAPI operations added");
}

#[tokio::test]
async fn coding_workflow_test_seam_keeps_internal_diagnostic_modes_without_tool_identity() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let client_id = "advanced-hidden-start";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let params = json!({
        "project": project,
        "title": "advanced hidden compatibility",
        "mode": "read_only",
        "detail": "minimal"
    });

    let wire_error = ToolCall::from_tool_name("start_coding_task", params.clone())
        .expect_err("retired wire entry must remain unknown despite internal test-seam arguments");
    assert!(wire_error.contains("unknown tool 'start_coding_task'"));
    let result = coding_workflow_serviced(&runtime, client_id, params, &auth).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["detail"], "minimal");
    let session_id = result.output["session"]["session_id"].as_str().unwrap();
    let summary = runtime.sessions.summary(session_id, Some(20)).unwrap();
    assert_eq!(summary.mode, SessionMode::ReadOnly);
    assert!(summary.guards.deny_write_tools);
    assert!(summary.guards.deny_shell_tools);
}

#[tokio::test]
async fn coding_workflow_full_diagnostic_has_no_binding_projection() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(
        tmp.path(),
        "AGENTS.md",
        "# Rules\n\nUse focused tests.\n",
        "add instructions",
    );
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-start", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_serviced(
        &runtime,
        "coding-start",
        json!({
            "project": project,
            "title": "implement deterministic aggregate",
            "detail": "full"
        }),
        &auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    let session_id = result.output["session"]["session_id"].as_str().unwrap();
    assert!(session_id.starts_with("wc_sess_"));
    assert_eq!(
        result.output["session"]["explicit_resume_required_for_continuation"],
        true
    );
    assert!(
        result.output["session"].get("current_binding").is_none(),
        "removed binding state must not be projected"
    );
    for field in [
        "session",
        "runtime_status",
        "authority",
        "rules",
        "git",
        "semantic_navigation",
        "recommended_flow",
        "warnings",
        "tool_manifest",
    ] {
        assert!(
            result.output.get(field).is_some(),
            "coding workflow diagnostic should include {field}"
        );
    }
    assert_eq!(result.output["authority"]["mode"], "trusted_agent");
    assert_eq!(result.output["authority"]["human_approval_required"], false);

    let inspect = result.output["recommended_flow"]["inspect"]
        .as_array()
        .unwrap();
    assert!(contains_string(inspect, "read_file"));
    assert!(contains_string(inspect, "search_project_text"));
    assert!(contains_string(inspect, "show_changes"));
    let edit = result.output["recommended_flow"]["edit"]
        .as_array()
        .unwrap();
    assert!(contains_string(edit, "apply_text_edits"));
    assert!(contains_string(edit, "apply_unified_diff"));
    assert!(contains_string(edit, "write_project_file"));
    assert!(!contains_string(edit, "replace_line_range"));
    assert!(!contains_string(edit, "insert_at_line"));
    assert!(!contains_string(edit, "delete_line_range"));

    assert_eq!(result.output["rules"]["present"], true);
    assert_eq!(result.output["rules"]["sources"][0]["path"], "AGENTS.md");
    let manifest = &result.output["tool_manifest"];
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["intent"], Value::Null);
    assert_eq!(manifest["filtered"], false);
    assert_eq!(manifest["categories_requested"], Value::Null);
    assert_eq!(manifest["limit"], Value::Null);
    assert_eq!(manifest["truncated"], false);
    assert!(manifest["count"].as_u64().unwrap() > 0);
    let manifest_tools = manifest["tools"].as_array().unwrap();
    assert!(manifest_tools
        .iter()
        .all(|tool| tool["name"] != "start_coding_task"));
    let work_tool = manifest_tools
        .iter()
        .find(|tool| tool["name"] == "work_on_project")
        .expect("canonical work_on_project manifest entry");
    for field in ["project", "client_id", "path", "instruction", "session_id"] {
        assert!(
            work_tool["accepted_flattened_args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|accepted| accepted == field),
            "work_on_project manifest entry missing {field}"
        );
    }
    for advanced in [
        "detail",
        "temporary_project_name",
        "bind_current",
        "new_session",
    ] {
        assert!(!work_tool["accepted_flattened_args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == advanced));
    }
    assert!(work_tool.get("inputSchema").is_none());
    assert!(work_tool.get("outputSchema").is_none());
    assert_eq!(result.output["git"]["clean"], true);
    assert!(!result.output["git"]["recent_commits"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn coding_workflow_standard_omits_compact_tool_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-no-manifest", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_serviced(
        &runtime,
        "coding-no-manifest",
        json!({
            "project": project,
            "title": "small startup payload"
        }),
        &auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert!(
        result.output.get("tool_manifest").is_none(),
        "include_tool_manifest=false should omit compact manifest"
    );
}

#[tokio::test]
async fn coding_workflow_minimal_brief_is_bounded_and_path_safe() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let policy = RunnerPolicySummary {
        allowed_roots: vec![PathBuf::from("/tmp/startup-full-allowed-root")],
        ..Default::default()
    };
    register_agent_with_shell_profiles(
        &runtime,
        "coding-full-status",
        Some(policy),
        vec![registered_project("demo", &tmp.path().to_string_lossy())],
    )
    .await;
    let auth = auth_context(None, true);
    let project = "agent:coding-full-status:demo".to_string();

    let result = coding_workflow_serviced(
        &runtime,
        "coding-full-status",
        json!({
            "project": project,
            "detail": "minimal"
        }),
        &auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["detail"], "minimal");
    assert!(result.output["session"]["session_id"].is_string());
    assert!(result.output["project"]["repository_identity"]
        .as_str()
        .is_some_and(|value| value.starts_with("repository:v1:")));
    assert_eq!(result.output["instructions"]["content_included"], false);
    assert_eq!(
        result.output["startup_verdict"]["suggested_next_actions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(!serde_json::to_string(&result.output)
        .unwrap()
        .contains("/tmp/startup-full-allowed-root"));
    for omitted in [
        "tool_manifest",
        "rules",
        "recent_commits",
        "authority",
        "recommended_flow",
        "runtime_status",
        "connection_state",
        "git",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "minimal startup must omit {omitted}"
        );
    }
    assert!(result.output["workspace"].is_object());
}

#[tokio::test]
async fn coding_workflow_standard_returns_model_facing_brief_without_diagnostics() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let policy = RunnerPolicySummary {
        allowed_roots: vec![PathBuf::from("/tmp/compact-allowed-root-never-emit")],
        ..Default::default()
    };
    register_agent_with_shell_profiles(
        &runtime,
        "coding-compact-status",
        Some(policy),
        vec![registered_project("demo", &tmp.path().to_string_lossy())],
    )
    .await;
    let auth = auth_context(None, true);
    let project = "agent:coding-compact-status:demo".to_string();

    let result = coding_workflow_serviced(
        &runtime,
        "coding-compact-status",
        json!({ "project": project }),
        &auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["detail"], "standard");
    for field in [
        "session",
        "project",
        "workspace",
        "instructions",
        "continuation",
        "semantic_navigation",
        "blockers",
        "warnings",
        "startup_verdict",
    ] {
        assert!(
            result.output.get(field).is_some(),
            "standard startup brief should include {field}"
        );
    }
    for diagnostic in [
        "runtime_status",
        "connection_state",
        "authority",
        "git",
        "rules",
        "tool_manifest",
        "recommended_flow",
        "resolved_project",
    ] {
        assert!(
            result.output.get(diagnostic).is_none(),
            "standard startup brief must omit {diagnostic}"
        );
    }
    let verdict = &result.output["startup_verdict"];
    assert_ne!(verdict["status"], "fail");
    assert_eq!(verdict["blocking"], false);

    let serialized = serde_json::to_string(&result.output).unwrap();
    for forbidden in [
        "tools.names",
        "allowed_roots",
        "compact-allowed-root-never-emit",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "compact startup leaked sensitive value {forbidden}: {serialized}"
        );
    }
    for forbidden in [
        "policy", "stdout", "stderr", "command", "env", "token", "secret",
    ] {
        assert!(
            !json_contains_key(&result.output, forbidden),
            "compact startup leaked field {forbidden}: {serialized}"
        );
    }
}

#[tokio::test]
async fn coding_workflow_full_startup_verdict_accepts_clean_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-start-verdict", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_serviced(
        &runtime,
        "coding-start-verdict",
        json!({ "project": project, "detail": "full" }),
        &auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    let verdict = &result.output["startup_verdict"];
    assert_startup_verdict_shape(verdict);
    assert_eq!(verdict["status"], "pass");
    assert_eq!(verdict["blocking"], false);
    assert_check_status(verdict, "runtime_status", "pass");
    assert_check_status(verdict, "workspace", "pass");
    assert_check_status(verdict, "jobs", "pass");
    assert_check_status(verdict, "agent", "pass");
    assert_check_status(verdict, "tool_manifest", "pass");
    assert_compact_verdict_safe(verdict, "startup clean verdict");
}

/// Shared helper: canonical coding-workflow diagnostics with Git inspection
/// against a real temp repo.
async fn coding_workflow_with_git_inspection(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    auth: &AuthContext,
) -> ToolResult {
    coding_workflow_serviced(runtime, client_id, json!({"project": project}), auth).await
}

#[derive(serde::Deserialize)]
struct CodingWorkflowTestParams {
    #[serde(default)]
    project: String,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    mode: SessionMode,
    #[serde(default)]
    deny_write_tools: bool,
    #[serde(default)]
    deny_shell_tools: bool,
    #[serde(default)]
    detail: StartupDetail,
    #[serde(default)]
    resume_session_id: Option<String>,
    #[serde(default)]
    execution_context: Option<crate::tool_runtime::sessions::SessionExecutionContext>,
}

/// Shared test-only driver for the canonical coding workflow engine. It
/// preserves the old diagnostic coverage without creating another ToolCall or
/// model/API identity.
async fn coding_workflow_serviced(
    runtime: &ToolRuntime,
    client_id: &str,
    params: Value,
    auth: &AuthContext,
) -> ToolResult {
    let params: CodingWorkflowTestParams = serde_json::from_value(params).unwrap();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            runtime
                .start_coding_workflow_for_test(
                    params.project,
                    params.client_id,
                    params.path,
                    params.title,
                    params.mode,
                    params.deny_write_tools,
                    params.deny_shell_tools,
                    params.detail,
                    params.resume_session_id,
                    params.execution_context,
                    Some(&auth),
                    None,
                    None,
                    SessionTransport::Api,
                )
                .await
        }
    });
    service_agent_task_until_finished(runtime, client_id, &task, "coding workflow").await;
    task.await.unwrap()
}

async fn work_on_project_serviced(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    instruction: &str,
    auth: &AuthContext,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.to_string();
        let instruction = instruction.to_string();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::WorkOnProject {
                        project,
                        client_id: None,
                        path: None,
                        mode: None,
                        base_ref: None,
                        instruction,
                        session_id: None,
                        include_project_instructions: true,
                        include_workflow_guidance: true,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    service_agent_task_until_finished(runtime, client_id, &task, "work_on_project").await;
    task.await.unwrap()
}

fn assert_startup_nonblocking_dirty(result: &ToolResult, workspace_reason: &str) {
    assert!(result.success, "{:?}", result.error);
    let session_id = result.output["session"]["session_id"]
        .as_str()
        .expect("session_id");
    assert!(session_id.starts_with("wc_sess_"), "{session_id}");
    let verdict = &result.output["startup_verdict"];
    assert_eq!(
        verdict["blocking"], false,
        "dirty workspace must not block: {verdict}"
    );
    assert_ne!(
        verdict["status"], "fail",
        "dirty workspace must not fail startup: {verdict}"
    );
    assert_eq!(verdict["status"], "warn");
    assert_eq!(workspace_reason, "workspace_dirty");
    assert_eq!(result.output["workspace"]["status"], "dirty");
    assert_eq!(result.output["workspace"]["clean"], false);
    assert!(
        result.output["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning == "dirty_worktree"),
        "top-level dirty_worktree warning expected: {}",
        result.output["warnings"]
    );
}

#[tokio::test]
async fn coding_workflow_untracked_only_is_nonblocking_warning() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    fs::write(tmp.path().join("report.md"), "audit report\n").unwrap();
    let report_before = fs::read_to_string(tmp.path().join("report.md")).unwrap();

    let runtime = test_runtime();
    let client_id = "coding-start-untracked";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert_startup_nonblocking_dirty(&result, "workspace_dirty");
    assert_eq!(result.output["workspace"]["untracked"], 1);
    assert_eq!(result.output["workspace"]["modified"], 0);
    assert_eq!(
        fs::read_to_string(tmp.path().join("report.md")).unwrap(),
        report_before,
        "coding workflow must not modify untracked report.md"
    );
}

#[tokio::test]
async fn coding_workflow_tracked_modified_is_nonblocking_and_allows_continued_edit() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    commit_file(
        tmp.path(),
        "src/example.rs",
        "fn main() {\n    println!(\"head\");\n}\n",
        "add example",
    );
    // Pre-existing worktree change (M) that must be preserved as the edit baseline.
    let dirty_content = "fn main() {\n    println!(\"user-wip\");\n}\n";
    fs::write(tmp.path().join("src/example.rs"), dirty_content).unwrap();

    let runtime = test_runtime();
    let client_id = "coding-start-modified";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert_startup_nonblocking_dirty(&result, "workspace_dirty");
    assert_eq!(result.output["workspace"]["modified"], 1);

    // Worktree content is the real baseline: edit must match current disk, not HEAD.
    let worktree = fs::read_to_string(tmp.path().join("src/example.rs")).unwrap();
    assert_eq!(worktree, dirty_content);
    assert!(worktree.contains("user-wip"));
    assert!(!worktree.contains("head"));

    let (updated, out) = crate::tool_runtime::files::apply_text_edits_to_string(
        &worktree,
        "src/example.rs",
        &[crate::tool_runtime::tool_inputs::ApplyTextEditInput {
            kind: crate::tool_runtime::tool_inputs::ApplyTextEditKind::ReplaceExact,
            old_text: Some("    println!(\"user-wip\");".to_string()),
            new_text: Some("    println!(\"user-wip-plus-agent\");".to_string()),
            anchor_text: None,
            occurrence: None,
            line_scope: None,
        }],
        None,
        false,
    )
    .expect("continued edit on already-modified worktree content must succeed");
    assert_eq!(out["changed"], true);
    assert!(updated.contains("user-wip-plus-agent"));
    assert!(
        updated.contains("fn main()"),
        "must preserve surrounding worktree content: {updated}"
    );
    assert!(
        !updated.contains("head"),
        "must not revert to HEAD content: {updated}"
    );

    // Applying against HEAD-only content with the same worktree old_text fails,
    // proving the tool is not using HEAD as the silent baseline.
    let head_content = "fn main() {\n    println!(\"head\");\n}\n";
    let head_err = crate::tool_runtime::files::apply_text_edits_to_string(
        head_content,
        "src/example.rs",
        &[crate::tool_runtime::tool_inputs::ApplyTextEditInput {
            kind: crate::tool_runtime::tool_inputs::ApplyTextEditKind::ReplaceExact,
            old_text: Some("    println!(\"user-wip\");".to_string()),
            new_text: Some("    println!(\"user-wip-plus-agent\");".to_string()),
            anchor_text: None,
            occurrence: None,
            line_scope: None,
        }],
        None,
        false,
    )
    .unwrap_err();
    assert!(
        head_err.contains("match text was not found") || head_err.contains("Rejected before write"),
        "HEAD baseline should not satisfy worktree old_text: {head_err}"
    );

    // Persist continued edit and confirm disk still differs from a clean checkout.
    fs::write(tmp.path().join("src/example.rs"), &updated).unwrap();
    assert_eq!(
        fs::read_to_string(tmp.path().join("src/example.rs")).unwrap(),
        updated
    );
}

#[tokio::test]
async fn coding_workflow_staged_changes_are_nonblocking() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    commit_file(tmp.path(), "src/example.rs", "fn a() {}\n", "add example");
    fs::write(
        tmp.path().join("src/example.rs"),
        "fn a() { /* staged */ }\n",
    )
    .unwrap();
    let (exit_code, stdout, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git add -- src/example.rs", tmp.path(), 30);
    assert_eq!(exit_code, 0, "git add failed\n{stdout}\n{stderr}");

    let runtime = test_runtime();
    let client_id = "coding-start-staged";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert_startup_nonblocking_dirty(&result, "workspace_dirty");
    assert_eq!(result.output["workspace"]["staged"], 1);
    // Staging area must remain intact (no auto unstage).
    let (exit_code, status_stdout, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git status --porcelain", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    assert!(
        status_stdout.lines().any(|line| line.starts_with("M ")),
        "staged entry should remain staged: {status_stdout}"
    );
}

#[tokio::test]
async fn coding_workflow_mixed_dirty_workspace_summarizes_counts_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "tracked.rs", "tracked\n", "add tracked");
    commit_file(tmp.path(), "staged.rs", "staged-base\n", "add staged");
    fs::write(tmp.path().join("tracked.rs"), "tracked-mod\n").unwrap();
    fs::write(tmp.path().join("staged.rs"), "staged-mod\n").unwrap();
    let (exit_code, _, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git add -- staged.rs", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    fs::write(tmp.path().join("notes.md"), "notes\n").unwrap();

    let runtime = test_runtime();
    let client_id = "coding-start-mixed";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_with_git_inspection(&runtime, client_id, &project, &auth).await;

    assert_startup_nonblocking_dirty(&result, "workspace_dirty");
    assert_eq!(result.output["workspace"]["modified"], 2);
    assert_eq!(result.output["workspace"]["staged"], 1);
    assert_eq!(result.output["workspace"]["untracked"], 1);
}

#[tokio::test]
async fn coding_workflow_conflict_state_is_a_hard_blocker_but_remains_inspectable() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "conflicted.rs", "base\n", "base");
    let (exit_code, _, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git checkout -b other", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    commit_file(tmp.path(), "conflicted.rs", "other-side\n", "other");
    let (exit_code, _, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git checkout -", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    commit_file(tmp.path(), "conflicted.rs", "main-side\n", "main");
    let (exit_code, _, stderr, _) =
        crate::tool_runtime::helpers::run_command_sync("git merge other || true", tmp.path(), 30);
    assert_eq!(exit_code, 0, "merge helper failed: {stderr}");
    // Ensure conflict markers exist on disk for inspection.
    let conflict_body = fs::read_to_string(tmp.path().join("conflicted.rs")).unwrap();
    assert!(
        conflict_body.contains("<<<<<<<") || conflict_body.contains("other-side"),
        "expected conflict content: {conflict_body}"
    );

    let runtime = test_runtime();
    let client_id = "coding-start-conflict";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_serviced(
        &runtime,
        client_id,
        json!({"project": project, "detail": "minimal"}),
        &auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["detail"], "minimal");
    assert_eq!(result.output["startup_verdict"]["status"], "fail");
    assert_eq!(result.output["startup_verdict"]["blocking"], true);
    assert!(
        result.output["workspace"]["conflicts"].as_u64().unwrap() >= 1,
        "conflicted count: {}",
        result.output["workspace"]["conflicts"]
    );
    assert!(result.output["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker == "workspace_conflicts"));
    // Session still usable for read/inspect of conflicted path content.
    assert!(
        fs::read_to_string(tmp.path().join("conflicted.rs"))
            .unwrap()
            .contains("main-side")
            || fs::read_to_string(tmp.path().join("conflicted.rs"))
                .unwrap()
                .contains("<<<<<<<"),
        "conflict file must remain readable"
    );
}

#[tokio::test]
async fn coding_workflow_unknown_project_still_fails() {
    let runtime = test_runtime();
    let auth = auth_context(None, true);
    let result = runtime
        .start_coding_workflow_for_test(
            "agent:missing:does-not-exist".to_string(),
            None,
            None,
            None,
            SessionMode::Normal,
            false,
            false,
            StartupDetail::Standard,
            None,
            None,
            Some(&auth),
            None,
            None,
            SessionTransport::Api,
        )
        .await;
    assert!(
        !result.success,
        "unresolvable project must fail: {:?}",
        result.output
    );
    assert!(
        !result
            .output
            .as_object()
            .unwrap()
            .contains_key("project_resolution"),
        "unresolved aliases must not be presented as authoritative project resolution metadata"
    );
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("project")
            || result
                .error
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase()
                .contains("not found")
            || result
                .error
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase()
                .contains("unknown"),
        "expected project resolution error: {:?}",
        result.error
    );
}

#[tokio::test]
async fn coding_workflow_runner_offline_is_still_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-start-offline", "demo", tmp.path()).await;
    // Transport disconnect leaves the agent offline while project id may still resolve.
    runtime
        .runner_registry
        .reconcile_disconnect("coding-start-offline", "inst")
        .await;

    let auth = auth_context(None, true);
    let result = runtime
        .start_coding_workflow_for_test(
            project,
            None,
            None,
            None,
            SessionMode::Normal,
            false,
            false,
            StartupDetail::Standard,
            None,
            None,
            Some(&auth),
            None,
            None,
            SessionTransport::Api,
        )
        .await;

    // Project resolution or Runner health may fail closed — either is blocking.
    if result.success {
        let verdict = &result.output["startup_verdict"];
        assert_eq!(
            verdict["blocking"], true,
            "Runner offline / unreachable must remain blocking: {verdict}"
        );
        assert_eq!(verdict["status"], "fail");
        assert!(result.output["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker == "runner_unavailable"));
    } else {
        assert!(
            result.error.is_some(),
            "infrastructure failure must surface an error"
        );
    }
}

#[test]
fn start_coding_task_legacy_arguments_do_not_restore_live_identity() {
    let error = ToolCall::from_tool_name(
        "start_coding_task",
        json!({"project": "agent:demo:demo", "include_runtime_status": false}),
    )
    .expect_err("legacy arguments must not restore a retired tool identity");
    assert!(
        error.contains("unknown tool 'start_coding_task'"),
        "{error}"
    );
}

#[tokio::test]
async fn finish_coding_task_requires_explicit_session_and_returns_structured_fields() {
    let missing_session =
        ToolCall::from_tool_name("finish_coding_task", json!({"project": "demo"})).unwrap_err();
    assert!(missing_session.contains("session_id"));

    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-finish", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let start = work_on_project_serviced(
        &runtime,
        "coding-finish",
        &project,
        "finish contract",
        &auth,
    )
    .await;
    assert!(start.success, "{:?}", start.error);
    let session_id = start.output["session_id"].as_str().unwrap().to_string();

    fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: false,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "coding-finish").await;
    assert_internal_posix_script_contains(&req, "git status --porcelain=v1 -b");
    let show_changes_stdout = format!(
        "{}{}{}",
        crate::tool_runtime::framed_show_changes_test_block(
            'S',
            "## main\n M README.md\n",
            "status_exit=0\nrepository_probe=inside_worktree\nrepository_probe_exit=0\nfiles_total=1\nfiles_returned=1\nfiles_truncated=0\nfiles_limit=200\nmodified=1\nadded=0\ndeleted=0\nrenamed=0\ncopied=0\nuntracked=0\nconflicted=0\nstaged=0\nunstaged=1\nstatus_trunc_count=0\nstatus_trunc_bytes=0\nstatus_trunc_path=0\nstatus_bytes=20\n"
        ),
        crate::tool_runtime::framed_show_changes_test_block(
            'H',
            "commit=abc123\nshort=abc123\nsummary=add readme\n",
            "head_exit=0\nhead_truncated=0\nhead_bytes=45\n"
        ),
        crate::tool_runtime::framed_show_changes_test_block(
            'T',
            " README.md | 1 +\n 1 file changed, 1 insertion(+)\n",
            "diff_stat_exit=0\ndiff_stat_truncated=0\ndiff_stat_bytes=52\n"
        )
    );
    complete_patch_agent_request(
        &runtime,
        "coding-finish",
        &req.request_id,
        0,
        &show_changes_stdout,
        "",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["session_id"], session_id);
    assert_eq!(result.output["deterministic"], true);
    assert_eq!(result.output["llm_summary"], false);
    assert_eq!(result.output["workspace"]["clean"], false);
    assert_eq!(result.output["changes"]["hunks_truncated"], false);
    assert!(result.output["changes"]["show_changes"].is_object());
    let validation = &result.output["validation"];
    assert_eq!(validation["available"], false);
    assert_eq!(validation["status"], "not_run");
    assert_eq!(validation["reason"], "no_validation_tool_invoked");
    assert_eq!(validation["source"], "session_ledger");
    assert_eq!(validation["events_total"], 0);
    assert!(validation["events"].as_array().unwrap().is_empty());
    assert_eq!(result.output["permissions"]["policy"], "trusted_agent");
    assert_eq!(result.output["permissions"]["required_count"], 0);
    assert_eq!(result.output["permissions"]["auto_approved_count"], 0);
    assert_eq!(result.output["permissions"]["manual_approved_count"], 0);
    assert_eq!(result.output["permissions"]["total_approved_count"], 0);
    assert!(result.output["permissions"]["recent"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(validation["parser"]["available"], false);
    assert_eq!(
        validation["parser"]["reason"],
        VALIDATION_OUTPUT_METADATA_ABSENT_REASON
    );
    assert_no_raw_validation_output_fields(validation, "finish validation summary");
    assert!(validation.get("observed_commands").is_none());
    assert_eq!(result.output["review_evidence"]["available"], true);
    assert_eq!(result.output["review_evidence"]["source"], "session_ledger");
    assert_eq!(result.output["review_evidence"]["total"], 1);
    assert_eq!(
        result.output["review_evidence"]["workspace_review_count"],
        1
    );
    assert_eq!(
        result.output["review_evidence"]["tools"],
        json!(["show_changes"])
    );
    assert_review_evidence_tools_safe(&result.output["review_evidence"]);
    assert!(result.output["hygiene"].is_null());
    assert!(result.output["handoff"].is_null());
    assert!(result.output["final_warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["kind"] == "dirty_worktree"));
    assert_eq!(result.output["task_outcome"]["status"], "warn");
    assert_eq!(result.output["evidence_history"]["status"], "clean");
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert!(result.output["informational_notes"].is_array());
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert!(result.output["advisories"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "workspace_dirty"));
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_summary_only_is_compact_for_clean_project() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-finish-compact", "demo", tmp.path())
            .await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("compact finish".to_string()));
    let session_id = session.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(true),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });

    service_agent_task_until_finished(
        &runtime,
        "coding-finish-compact",
        &task,
        "finish_coding_task summary_only",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["summary_only"], true);
    assert!(result.output.get("project").is_none());
    assert!(result.output.get("session_id").is_none());
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["jobs"]["active_count"], 0);
    assert_eq!(result.output["jobs"]["blocking_active_count"], 0);
    assert!(result.output.get("permissions").is_none());
    assert_eq!(result.output["tool_failures"]["expected_count"], 0);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 0);
    assert!(result.output["tool_failures"]
        .get("expectation_mismatch_count")
        .is_some());
    assert!(result.output["tool_failures"]
        .get("unexpected_success_count")
        .is_some());
    assert_eq!(result.output["validation"]["status"], "not_run");
    assert_eq!(
        result.output["validation"]["reason"],
        "no_validation_tool_invoked"
    );
    assert!(result.output.get("review_evidence").is_none());
    assert!(result.output["warnings"].as_array().unwrap().is_empty());
    assert_finish_uses_canonical_outcomes(&result.output);
    assert!(result.output["suggested_next_actions"].is_array());
    let task_outcome = &result.output["task_outcome"];
    assert_task_outcome_shape(task_outcome);
    assert_eq!(task_outcome["status"], "warn");
    assert_eq!(task_outcome["blocking"], false);
    assert_reason_list_contains(
        task_outcome,
        "warning_reasons",
        "validation_not_run_with_review_evidence",
    );
    assert_reason_list_not_contains(task_outcome, "warning_reasons", "validation_not_run");
    assert!(result.output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str()
            == Some("decide whether task-appropriate validation is needed before closeout")));
    assert!(!result.output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str() == Some("run validation before closeout when available")));

    for field in [
        "project",
        "session_id",
        "permissions",
        "review_evidence",
        "work_performed",
        "changed_paths",
        "handoff_brief",
        "facts",
        "evidence_history",
        "informational_notes",
    ] {
        assert!(
            result.output.get(field).is_none(),
            "summary_only finish leaked {field}: {}",
            result.output
        );
    }
    let serialized = serde_json::to_string(&result.output).unwrap();
    for forbidden in [
        "recent_events",
        "recent_failed_tools",
        "command",
        "stdout",
        "stderr",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "summary_only finish leaked {forbidden}: {serialized}"
        );
    }
    assert_no_raw_validation_output_fields(&result.output, "summary_only finish structured output");
    assert!(
        !serialized.contains("\"show_changes\":"),
        "summary_only finish leaked raw show_changes payload: {serialized}"
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_uses_review_evidence_without_projecting_it() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "docs.md", "hello\n", "add docs");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-finish-docs", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("docs-only finish".to_string()));
    let session_id = session.session_id.clone();

    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "write_project_file",
        json!({
            "project": project,
            "path": "docs.md",
            "content": "updated docs"
        }),
        true,
        json!({}),
    );
    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "search_project_text",
        json!({"project": project, "query": "docs"}),
        true,
        json!({}),
    );
    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "show_changes",
        json!({"project": project, "include_diff": false}),
        true,
        json!({}),
    );

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(true),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    service_agent_task_until_finished(
        &runtime,
        "coding-finish-docs",
        &task,
        "finish_coding_task summary_only",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["summary_only"], true);
    assert_eq!(result.output["validation"]["status"], "not_run");
    assert_eq!(
        result.output["validation"]["reason"],
        "no_validation_tool_invoked"
    );
    assert!(result.output.get("review_evidence").is_none());
    let task_outcome = &result.output["task_outcome"];
    assert_task_outcome_shape(task_outcome);
    assert_eq!(task_outcome["status"], "warn");
    assert_reason_list_contains(
        task_outcome,
        "warning_reasons",
        "validation_not_run_with_review_evidence",
    );
    assert_reason_list_not_contains(task_outcome, "warning_reasons", "validation_not_run");
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_summary_only_treats_dirty_workspace_as_advisory() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    fs::write(tmp.path().join("README.md"), "changed\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-finish-dirty", "demo", tmp.path()).await;
    let auth = auth_context(None, true);
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("dirty finish".to_string()));
    let session_id = session.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "coding-finish-dirty").await;
    complete_agent_request_by_running_locally(&runtime, "coding-finish-dirty", req).await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], false);
    assert_eq!(result.output["task_outcome"]["status"], "warn");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_task_outcome_shape(&result.output["task_outcome"]);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "warning_reasons",
        "workspace_dirty",
    );
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_does_not_resolve_a_different_validation_identity() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-finish-resolved", "demo", tmp.path())
            .await;
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("resolved validation finish".to_string()),
    );
    let session_id = session.session_id.clone();

    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "cargo_test",
        json!({
            "project": project,
            "expected_failure": true,
            "expected_failure_kind": "validation_failed",
            "assertion_name": "pre-fix validation should fail"
        }),
        false,
        json!({
            "exit_code": 101,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &runtime,
        &session_id,
        "cargo_check",
        json!({"project": project}),
        true,
        json!({"exit_code": 0}),
    );

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(true),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    service_agent_task_until_finished(
        &runtime,
        "coding-finish-resolved",
        &task,
        "finish_coding_task summary_only",
    )
    .await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 0);
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(result.output["validation"]["resolved_failure_count"], 0);
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 1);
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_task_outcome_shape(&result.output["task_outcome"]);
    assert!(result.output.get("evidence_history").is_none());
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "validation_failed",
    );
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_historical_unresolved_current_pass_does_not_request_ledger_cleanup() {
    let fixture = finish_summary_fixture("coding-finish-current-evidence-pass").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        false,
        json!({"exit_code": 101, "failure_kind": "validation_failed"}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "apply_text_edits",
        json!({
            "project": fixture.project.clone(),
            "changes": [{"kind": "edit", "path": "src/lib.rs"}]
        }),
        true,
        json!({"state_changed": true}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 1);
    assert_eq!(result.output["validation"]["current_status"], "passed");
    assert_eq!(
        result.output["validation"]["current_unresolved_failure_count"],
        0
    );
    assert_eq!(result.output["validation"]["stale_failure_count"], 1);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(
        result.output["tool_failures"]["non_actionable_unexpected_count"],
        1
    );
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        0
    );
    assert_eq!(result.output["task_outcome"]["status"], "pass");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_action_list_not_contains(
        &result.output["suggested_next_actions"],
        VALIDATION_IDENTITY_REUSE_ACTION,
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_passes_with_resolved_unexpected_cargo_fmt_failure() {
    let fixture = finish_summary_fixture("coding-finish-resolved-fmt").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        true,
        json!({"exit_code": 0}),
    );
    // Push the resolved validation pair beyond the default 20-event handoff
    // display window. Canonical closeout evidence must still retain the raw
    // failure and its later same-identity resolution.
    for index in 0..12 {
        record_coding_task_tool_event(
            &fixture.runtime,
            &fixture.session_id,
            "read_file",
            json!({
                "project": fixture.project.clone(),
                "path": format!("src/display-padding-{index}.rs")
            }),
            true,
            json!({}),
        );
    }

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
    )
    .await;
    let full = finish_coding_task_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
        false,
    )
    .await;
    let handoff = session_handoff_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(
        result.output["tool_failures"]["non_actionable_unexpected_count"],
        1
    );
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        0
    );
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(result.output["validation"]["current_status"], "passed");
    assert_eq!(
        result.output["validation"]["current_unresolved_failure_count"],
        0
    );
    assert_eq!(full.output["validation"]["status"], "mixed");
    assert_eq!(
        full.output["validation"]["current_evidence"]["status"],
        "passed"
    );
    assert!(handoff.success, "{:?}", handoff.error);
    assert_eq!(handoff.output["validation"]["status"], "mixed");
    assert_eq!(
        handoff.output["validation"]["current_evidence"]["status"],
        "passed"
    );
    assert_eq!(
        handoff.output["validation"]["resolved_failures"]["count"],
        result.output["validation"]["resolved_failure_count"]
    );
    assert_eq!(
        handoff.output["validation"]["unresolved_failures"]["count"],
        result.output["validation"]["unresolved_failure_count"]
    );
    assert_eq!(result.output["validation"]["resolved_failure_count"], 1);
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 0);
    assert_eq!(result.output["task_outcome"]["status"], "pass");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert!(result.output.get("advisories").is_none());
    assert!(result.output.get("informational_notes").is_none());
    assert!(result.output.get("evidence_history").is_none());
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_not_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
    assert_eq!(full.output["task_outcome"], result.output["task_outcome"]);
    for key in [
        "expected_count",
        "unexpected_count",
        "non_actionable_unexpected_count",
        "actionable_unexpected_count",
        "expectation_mismatch_count",
        "unexpected_success_count",
    ] {
        assert_eq!(
            handoff.output["tool_failures"][key], result.output["tool_failures"][key],
            "tool failure parity for {key}"
        );
    }
    assert_eq!(
        handoff.output["task_outcome"],
        result.output["task_outcome"]
    );
    assert_eq!(
        handoff.output["evidence_integrity"],
        result.output["evidence_integrity"]
    );
    assert!(full.output["advisories"].is_array());
    assert!(full.output["evidence_history"].is_object());
    assert!(full.output["informational_notes"].is_array());
    assert!(full.output["review_evidence"].is_object());
    assert!(full.output["facts"].is_object());
    assert_eq!(
        full.output["evidence_integrity"],
        result.output["evidence_integrity"]
    );
    assert_eq!(
        full.output["suggested_next_actions"],
        result.output["suggested_next_actions"]
    );
    assert_finish_uses_canonical_outcomes(&result.output);
    assert_finish_uses_canonical_outcomes(&full.output);
}

#[tokio::test]
async fn handoff_display_limit_does_not_change_canonical_started_shell_failure_closeout() {
    let fixture = finish_summary_fixture("coding-handoff-display-limit-started-shell").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "run_shell",
        json!({
            "project": fixture.project.clone(),
            "command": "false",
            "purpose": "operation"
        }),
        false,
        json!({
            "failure_kind": "process_exit",
            "exit_code": 1,
            "state_changed": false,
            "command_started": true,
            "command_completed": true,
            "execution_state": "completed"
        }),
    );
    for index in 0..12 {
        record_coding_task_tool_event(
            &fixture.runtime,
            &fixture.session_id,
            "read_file",
            json!({
                "project": fixture.project.clone(),
                "path": format!("src/benign-padding-{index}.rs")
            }),
            true,
            json!({}),
        );
    }
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );

    let handoff_default = session_handoff_summary_only_with_agent_limit(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
        None,
    )
    .await;
    let handoff_narrow = session_handoff_summary_only_with_agent_limit(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
        Some(1),
    )
    .await;
    let handoff_wide = session_handoff_summary_only_with_agent_limit(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
        Some(100),
    )
    .await;
    let finish = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(finish.success, "{:?}", finish.error);
    for handoff in [&handoff_default, &handoff_narrow, &handoff_wide] {
        assert!(handoff.success, "{:?}", handoff.error);
        for key in [
            "expected_count",
            "unexpected_count",
            "non_actionable_unexpected_count",
            "actionable_unexpected_count",
            "expectation_mismatch_count",
            "unexpected_success_count",
        ] {
            assert_eq!(
                handoff.output["tool_failures"][key], finish.output["tool_failures"][key],
                "canonical tool-failure count changed for {key}: {}",
                handoff.output
            );
        }
        assert_eq!(
            handoff.output["validation"]["status"], finish.output["validation"]["status"],
            "canonical validation status changed with handoff display limit"
        );
        assert_eq!(
            handoff.output["task_outcome"], finish.output["task_outcome"],
            "canonical task_outcome changed with handoff display limit"
        );
        assert_eq!(
            handoff.output["evidence_integrity"], finish.output["evidence_integrity"],
            "canonical evidence_integrity changed with handoff display limit"
        );
    }

    assert_eq!(finish.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(
        finish.output["tool_failures"]["non_actionable_unexpected_count"],
        0
    );
    assert_eq!(
        finish.output["tool_failures"]["actionable_unexpected_count"],
        1
    );
    assert_eq!(finish.output["validation"]["status"], "passed");
    assert_eq!(finish.output["task_outcome"]["status"], "fail");
    assert_eq!(finish.output["task_outcome"]["blocking"], true);
    assert_eq!(finish.output["evidence_integrity"]["status"], "clean");
    assert_reason_list_contains(
        &finish.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_eq!(
        handoff_default.output["verdict"],
        handoff_narrow.output["verdict"]
    );
    assert_eq!(
        handoff_default.output["verdict"],
        handoff_wide.output["verdict"]
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_passes_with_resolved_unexpected_cargo_check_failure() {
    let fixture = finish_summary_fixture("coding-finish-resolved-check").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        false,
        json!({
            "exit_code": 101,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(result.output["validation"]["resolved_failure_count"], 1);
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 0);
    assert_eq!(result.output["task_outcome"]["status"], "pass");
    assert!(result.output.get("evidence_history").is_none());
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert!(result.output.get("informational_notes").is_none());
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
}

#[tokio::test]
async fn finish_coding_task_combined_early_fmt_and_test_failures_resolve_without_erasing_history() {
    let fixture = finish_summary_fixture("coding-finish-combined-resolved-validation").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({"exit_code": 1, "failure_kind": "validation_failed"}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "apply_text_edits",
        json!({
            "project": fixture.project.clone(),
            "changes": [{"kind": "edit", "path": "src/lib.rs"}]
        }),
        true,
        json!({"state_changed": true}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        true,
        json!({"exit_code": 0}),
    );

    let test_target = json!({
        "project": fixture.project.clone(),
        "package": "webcodex",
        "filter": "focused",
    });
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        test_target.clone(),
        false,
        json!({
            "exit_code": 101,
            "failure_kind": "validation_failed",
            "stdout_tail": "running 1 test\ntest focused ... FAILED\ntest result: FAILED. 0 passed; 1 failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 1,
            "tests_passed": 0,
            "tests_failed": 1,
            "zero_tests_run": false
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "apply_text_edits",
        json!({
            "project": fixture.project.clone(),
            "changes": [{"kind": "edit", "path": "src/lib.rs"}]
        }),
        true,
        json!({"state_changed": true}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        test_target,
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\ntest focused ... ok\ntest result: ok. 1 passed; 0 failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 1,
            "tests_passed": 1,
            "tests_failed": 0,
            "zero_tests_run": false
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );

    let immutable = fixture
        .runtime
        .sessions
        .summary(&fixture.session_id, Some(100))
        .unwrap();
    assert_eq!(
        immutable
            .events
            .iter()
            .filter(|event| event.kind == "tool_call_finished"
                && event.status.as_deref() == Some("failed"))
            .count(),
        2
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["jobs"]["blocking_active_count"], 0);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 2);
    assert_eq!(
        result.output["tool_failures"]["non_actionable_unexpected_count"],
        2
    );
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        0
    );
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["resolved_failure_count"], 2);
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 0);
    assert_eq!(result.output["validation"]["current_status"], "passed");
    assert_eq!(
        result.output["validation"]["current_unresolved_failure_count"],
        0
    );
    assert_eq!(result.output["task_outcome"]["status"], "pass");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "validation_failed",
    );
}

#[tokio::test]
async fn finish_coding_task_resolved_history_keeps_real_workspace_advisory() {
    let fixture = finish_summary_fixture("coding-finish-resolved-dirty").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        true,
        json!({"exit_code": 0}),
    );
    std::fs::write(fixture._tmp.path().join("README.md"), "dirty\n").unwrap();

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(result.output["validation"]["resolved_failure_count"], 1);
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 0);
    assert_eq!(result.output["validation"]["current_status"], "passed");
    assert_eq!(
        result.output["validation"]["current_unresolved_failure_count"],
        0
    );
    assert!(result.output.get("evidence_history").is_none());
    assert_eq!(result.output["task_outcome"]["status"], "warn");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "warning_reasons",
        "workspace_dirty",
    );
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "warning_reasons",
        "historical_validation_failures_resolved",
    );
    assert!(result.output.get("informational_notes").is_none());
}

#[tokio::test]
async fn finish_coding_task_resolved_history_keeps_real_tool_failure_blocking() {
    let fixture = finish_summary_fixture("coding-finish-resolved-blocker").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        true,
        json!({"exit_code": 0}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "apply_text_edits",
        json!({"project": fixture.project.clone(), "changes": []}),
        false,
        json!({"failure_kind": "stale_precondition"}),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["validation"]["status"], "mixed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(result.output["validation"]["resolved_failure_count"], 1);
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 0);
    assert_eq!(result.output["validation"]["current_status"], "passed");
    assert_eq!(
        result.output["validation"]["current_unresolved_failure_count"],
        0
    );
    assert!(result.output.get("evidence_history").is_none());
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert!(result.output.get("informational_notes").is_none());
}

#[tokio::test]
async fn failure_history_fail_closed_attempts_do_not_block_clean_finish() {
    let fixture = finish_summary_fixture("coding-finish-fail-closed-history").await;
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "read_file",
        json!({"project": fixture.project.clone(), "path": "missing.rs"}),
        false,
        json!({"error_kind": "invalid_arguments"}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "apply_text_edits",
        json!({"project": fixture.project.clone(), "changes": []}),
        false,
        json!({
            "failure_kind": "stale_precondition",
            "state_changed": false
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "run_shell",
        json!({
            "project": fixture.project.clone(),
            "command": "false",
            "purpose": "operation"
        }),
        false,
        json!({
            "failure_kind": "policy_rejected",
            "command_started": false,
            "command_completed": false,
            "execution_state": "not_started"
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
    )
    .await;
    let full = finish_coding_task_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
        false,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 3);
    assert_eq!(
        result.output["tool_failures"]["non_actionable_unexpected_count"],
        3
    );
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        0
    );
    assert_eq!(result.output["validation"]["status"], "passed");
    assert_eq!(result.output["task_outcome"]["status"], "pass");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_eq!(result.output["evidence_integrity"]["status"], "clean");
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_not_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
    assert!(result.output.get("informational_notes").is_none());
    assert!(full.output["informational_notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str()
            == Some(
                "non-actionable failed tool calls are retained as historical/process evidence"
            )));
    for field in [
        "unexpected_count",
        "non_actionable_unexpected_count",
        "actionable_unexpected_count",
    ] {
        assert_eq!(
            full.output["tool_failures"][field], result.output["tool_failures"][field],
            "compact/full tool failure projection must agree for {field}"
        );
    }
    assert_eq!(full.output["task_outcome"], result.output["task_outcome"]);
    assert_eq!(
        full.output["suggested_next_actions"],
        result.output["suggested_next_actions"]
    );
}

#[tokio::test]
async fn failure_history_started_diagnostic_process_failure_remains_actionable() {
    let fixture = finish_summary_fixture("coding-finish-diagnostic-history").await;
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "run_process",
        json!({
            "project": fixture.project.clone(),
            "executable": "diagnostic-probe",
            "purpose": "diagnostic"
        }),
        false,
        json!({
            "failure_kind": "command_exit_nonzero",
            "exit_code": 1,
            "state_changed": false,
            "command_started": true,
            "command_completed": true,
            "execution_state": "completed"
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
    )
    .await;
    let full = finish_coding_task_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
        false,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(
        result.output["tool_failures"]["non_actionable_unexpected_count"],
        0
    );
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        1
    );
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
    assert!(!full.output["informational_notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str()
            == Some("completed diagnostic tool failures are retained as non-actionable evidence")));
}

#[tokio::test]
async fn failure_history_started_shell_failure_remains_actionable() {
    let fixture = finish_summary_fixture("coding-finish-started-shell-failure").await;
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "run_shell",
        json!({
            "project": fixture.project.clone(),
            "command": "false",
            "purpose": "operation"
        }),
        false,
        json!({
            "failure_kind": "process_exit",
            "exit_code": 1,
            "command_started": true,
            "command_completed": true,
            "execution_state": "completed"
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(
        result.output["tool_failures"]["non_actionable_unexpected_count"],
        0
    );
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        1
    );
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
}

#[tokio::test]
async fn failure_history_outcome_unknown_remains_actionable() {
    let fixture = finish_summary_fixture("coding-finish-outcome-unknown").await;
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "apply_text_edits",
        json!({"project": fixture.project.clone(), "changes": []}),
        false,
        json!({
            "failure_kind": "outcome_unknown",
            "state_changed": false,
            "execution_state": "outcome_unknown"
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        1
    );
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
}

#[tokio::test]
async fn failure_history_missing_effect_proof_remains_actionable() {
    let fixture = finish_summary_fixture("coding-finish-legacy-effect-proof").await;
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "apply_text_edits",
        json!({"project": fixture.project.clone(), "changes": []}),
        false,
        json!({"failure_kind": "stale_precondition"}),
    );
    let summary = fixture
        .runtime
        .sessions
        .summary(&fixture.session_id, Some(20))
        .unwrap();
    let failed = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "apply_text_edits")
        .expect("failed guarded mutation event");
    assert!(
        failed.effect_evidence.is_none(),
        "legacy-shaped event has no effect proof"
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(
        result.output["tool_failures"]["non_actionable_unexpected_count"],
        0
    );
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        1
    );
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_keeps_cargo_fmt_failure_blocking_when_only_cargo_test_passes(
) {
    let fixture = finish_summary_fixture("coding-finish-cross-tool-validation").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 1,
            "tests_passed": 1,
            "tests_failed": 0,
            "zero_tests_run": false
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 1);
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_warns_for_cargo_test_zero_tests_success() {
    let fixture = finish_summary_fixture("coding-finish-zero-tests").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({
            "project": fixture.project.clone(),
            "expected_failure": true,
            "expected_failure_kind": "validation_failed",
            "assertion_name": "negative assertion accidentally ran zero tests"
        }),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\n\
                test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(
        result.output["tool_failures"]["unexpected_success_count"],
        1
    );
    assert_eq!(
        result.output["tool_failures"]["expectation_mismatch_count"],
        0
    );
    assert_eq!(result.output["validation"]["status"], "inconclusive");
    assert_eq!(result.output["validation"]["successes"], 0);
    assert_eq!(result.output["validation"]["latest_status"], "inconclusive");
    assert_eq!(
        result.output["validation"]["cargo_test_zero_tests_run"],
        true
    );
    assert_eq!(result.output["task_outcome"]["status"], "warn");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "warning_reasons",
        "validation_inconclusive",
    );
    assert!(result.output.get("evidence_history").is_none());
    assert_eq!(result.output["evidence_integrity"]["status"], "warning");
    assert_reason_list_contains(
        &result.output["evidence_integrity"],
        "warning_reasons",
        "unexpected_successes",
    );
    assert_reason_list_contains(
        &result.output["evidence_integrity"],
        "warning_reasons",
        "cargo_test_zero_tests",
    );
    assert!(result.output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str()
            == Some("cargo_test ran zero tests; verify the test filter or command")));
    assert_finish_uses_canonical_outcomes(&result.output);
}

#[tokio::test]
async fn finish_coding_task_summary_only_keeps_cargo_test_failure_blocking_after_zero_tests_success(
) {
    let fixture = finish_summary_fixture("coding-finish-zero-tests-does-not-resolve").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        false,
        json!({
            "exit_code": 101,
            "failure_kind": "validation_failed"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 0 tests\n\n\
                test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 0,
            "zero_tests_run": true
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["status"], "failed");
    assert_eq!(result.output["validation"]["successes"], 0);
    assert_eq!(result.output["validation"]["latest_status"], "inconclusive");
    assert_eq!(
        result.output["validation"]["cargo_test_zero_tests_run"],
        true
    );
    assert_eq!(result.output["validation"]["resolved_failure_count"], 0);
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 1);
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert!(result.output.get("evidence_history").is_none());
    assert_eq!(result.output["evidence_integrity"]["status"], "warning");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
    assert_action_list_contains(
        &result.output["suggested_next_actions"],
        "cargo_test ran zero tests; verify the test filter or command",
    );
    assert_reason_list_contains(
        &result.output["evidence_integrity"],
        "warning_reasons",
        "cargo_test_zero_tests",
    );
    assert!(result.output.get("informational_notes").is_none());
}

#[tokio::test]
async fn finish_coding_task_summary_only_blocks_unresolved_cargo_fmt_failure() {
    let fixture = finish_summary_fixture("coding-finish-unresolved-fmt").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_fmt",
        json!({"project": fixture.project.clone(), "check": true}),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "validation_failed"
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(result.output["validation"]["status"], "failed");
    assert_eq!(result.output["validation"]["latest_status"], "failed");
    assert_eq!(result.output["validation"]["unresolved_failure_count"], 1);
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert_action_list_contains(
        &result.output["suggested_next_actions"],
        "review unexpected failed tool calls before proceeding",
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_treats_read_failure_as_historical_non_actionable() {
    let fixture = finish_summary_fixture("coding-finish-read-failure").await;

    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "read_file",
        json!({"project": fixture.project.clone(), "path": "README.md"}),
        false,
        json!({
            "error_kind": "permission_denied"
        }),
    );
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({"project": fixture.project.clone()}),
        true,
        json!({
            "exit_code": 0,
            "stdout_tail": "running 1 test\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
            "tests_detected": true,
            "tests_run_count": 1,
            "tests_passed": 1,
            "tests_failed": 0,
            "zero_tests_run": false
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["workspace_clean"], true);
    assert_eq!(result.output["hygiene_clean"], true);
    assert_eq!(result.output["tool_failures"]["unexpected_count"], 1);
    assert_eq!(
        result.output["tool_failures"]["non_actionable_unexpected_count"],
        1
    );
    assert_eq!(
        result.output["tool_failures"]["actionable_unexpected_count"],
        0
    );
    assert_eq!(result.output["validation"]["status"], "passed");
    assert_eq!(result.output["validation"]["latest_status"], "passed");
    assert_eq!(result.output["task_outcome"]["status"], "pass");
    assert_eq!(result.output["task_outcome"]["blocking"], false);
    assert_reason_list_not_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "unexpected_tool_failures",
    );
    assert!(result.output.get("informational_notes").is_none());
}

#[tokio::test]
async fn finish_coding_task_summary_only_keeps_active_blocking_job_decision_complete() {
    let fixture = finish_summary_fixture("coding-finish-compact-jobs").await;
    let _job_id = seed_session_projection_job(
        &fixture.runtime,
        fixture.client_id,
        &fixture.project,
        &fixture.session_id,
        "running",
        "compact-secret-job-output\n",
        &fixture.auth,
    )
    .await;

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["jobs"]["active_count"], 1);
    assert_eq!(result.output["jobs"]["blocking_active_count"], 1);
    assert_eq!(result.output["jobs"]["terminal_pending_count"], 0);
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "blocking_active_jobs",
    );
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(!serialized.contains("compact-secret-job-output"));
    assert!(!serialized.contains("recent"));
}

#[tokio::test]
async fn finish_coding_task_summary_only_keeps_evidence_integrity_error_visible() {
    let fixture = finish_summary_fixture("coding-finish-compact-integrity-error").await;
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_test",
        json!({
            "project": fixture.project.clone(),
            "expected_failure": true,
            "expected_failure_kind": "validation_failed",
            "assertion_name": "must fail as validation_failed"
        }),
        false,
        json!({
            "exit_code": 1,
            "failure_kind": "process_exit"
        }),
    );

    let result = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["evidence_integrity"]["status"], "error");
    assert_reason_list_contains(
        &result.output["evidence_integrity"],
        "error_reasons",
        "expectation_mismatches",
    );
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_reason_list_contains(
        &result.output["task_outcome"],
        "blocking_reasons",
        "expectation_mismatches",
    );
}

#[tokio::test]
async fn finish_coding_task_summary_only_has_bounded_clear_size_reduction() {
    let fixture = finish_summary_fixture("coding-finish-compact-size").await;
    record_coding_task_tool_event(
        &fixture.runtime,
        &fixture.session_id,
        "cargo_check",
        json!({"project": fixture.project.clone()}),
        true,
        json!({"exit_code": 0}),
    );

    let compact = finish_coding_task_summary_only_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project.clone(),
        fixture.session_id.clone(),
        fixture.auth.clone(),
    )
    .await;
    let full = finish_coding_task_with_agent(
        &fixture.runtime,
        fixture.client_id,
        fixture.project,
        fixture.session_id,
        fixture.auth,
        false,
    )
    .await;

    assert!(compact.success, "{:?}", compact.error);
    assert!(full.success, "{:?}", full.error);
    let compact_bytes = serde_json::to_vec(&compact).unwrap().len();
    let full_bytes = serde_json::to_vec(&full).unwrap().len();
    eprintln!("compact_finish_bytes={compact_bytes} full_finish_bytes={full_bytes}");
    assert!(
        compact_bytes <= 4096,
        "compact closeout was {compact_bytes} bytes"
    );
    assert!(
        compact_bytes < full_bytes,
        "compact={compact_bytes} full={full_bytes}"
    );
    assert!(
        compact_bytes * 5 <= full_bytes * 4,
        "compact closeout should save at least 20%: compact={compact_bytes} full={full_bytes}"
    );
    assert_eq!(compact.output["task_outcome"], full.output["task_outcome"]);
    assert_eq!(
        compact.output["evidence_integrity"],
        full.output["evidence_integrity"]
    );
    for field in [
        "review_evidence",
        "work_performed",
        "changed_paths",
        "handoff_brief",
        "facts",
    ] {
        assert!(
            compact.output.get(field).is_none(),
            "compact leaked {field}"
        );
        assert!(
            full.output.get(field).is_some(),
            "full closeout lost {field}"
        );
    }
}

#[tokio::test]
async fn finish_coding_task_includes_active_jobs_warning_without_logs() {
    let fixture = finish_summary_fixture("coding-finish-jobs").await;
    let job_id = seed_session_projection_job(
        &fixture.runtime,
        fixture.client_id,
        &fixture.project,
        &fixture.session_id,
        "running",
        "secret-job-output\n",
        &fixture.auth,
    )
    .await;

    let result = finish_coding_task_jobs_projection(&fixture).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["jobs"]["active_count"], 1);
    assert_eq!(result.output["jobs"]["running_count"], 1);
    assert_eq!(result.output["jobs"]["stop_requested_count"], 0);
    assert_eq!(result.output["jobs"]["terminal_pending_count"], 0);
    assert_eq!(result.output["jobs"]["blocking_active_count"], 1);
    assert_eq!(result.output["jobs"]["nonblocking_active_count"], 0);
    assert_eq!(result.output["task_outcome"]["status"], "fail");
    assert_eq!(result.output["task_outcome"]["blocking"], true);
    assert_eq!(result.output["jobs"]["recent"][0]["job_id"], job_id);
    assert!(result.output["final_warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["kind"] == "active_jobs_present" && warning["blocking"] == true));
    assert_no_raw_validation_output_fields(&result.output["jobs"], "finish jobs summary");
    let serialized = serde_json::to_string(&result.output["jobs"]).unwrap();
    assert!(!serialized.contains("secret-job-output"));
}

#[tokio::test]
async fn finish_coding_task_treats_stop_requested_jobs_as_nonblocking() {
    let fixture = finish_summary_fixture("coding-finish-stop-pending").await;
    let job_id = seed_session_projection_job(
        &fixture.runtime,
        fixture.client_id,
        &fixture.project,
        &fixture.session_id,
        "stop_requested",
        "stop-pending-secret-output\n",
        &fixture.auth,
    )
    .await;

    let result = finish_coding_task_jobs_projection(&fixture).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["jobs"]["active_count"], 1);
    assert_eq!(result.output["jobs"]["running_count"], 0);
    assert_eq!(result.output["jobs"]["stop_requested_count"], 1);
    assert_eq!(result.output["jobs"]["terminal_pending_count"], 1);
    assert_eq!(result.output["jobs"]["blocking_active_count"], 0);
    assert_eq!(result.output["jobs"]["nonblocking_active_count"], 1);
    assert_eq!(result.output["jobs"]["recent"][0]["job_id"], job_id);
    let final_warnings = result.output["final_warnings"].as_array().unwrap();
    assert!(final_warnings
        .iter()
        .all(|warning| warning["kind"] != "active_jobs_present"));
    assert!(final_warnings.iter().any(|warning| {
        warning["kind"] == "jobs_terminal_pending" && warning["blocking"] == false
    }));
    assert_no_raw_validation_output_fields(&result.output["jobs"], "finish jobs summary");
    let serialized = serde_json::to_string(&result.output["jobs"]).unwrap();
    assert!(!serialized.contains("stop-pending-secret-output"));
}

fn contains_string(values: &[Value], needle: &str) -> bool {
    values.iter().any(|value| value.as_str() == Some(needle))
}

fn assert_check_status(verdict: &Value, name: &str, status: &str) {
    let check = verdict["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("missing startup check {name}: {verdict}"));
    assert_eq!(check["status"], status);
}

fn assert_reason_list_contains(verdict: &Value, key: &str, reason: &str) {
    let reasons = verdict[key].as_array().expect("reason list");
    assert!(
        reasons.iter().any(|value| value.as_str() == Some(reason)),
        "{key} should contain {reason}: {verdict}"
    );
}

fn assert_reason_list_not_contains(verdict: &Value, key: &str, reason: &str) {
    let reasons = verdict[key].as_array().expect("reason list");
    assert!(
        !reasons.iter().any(|value| value.as_str() == Some(reason)),
        "{key} should not contain {reason}: {verdict}"
    );
}

fn assert_startup_verdict_shape(verdict: &Value) {
    assert_status_string(verdict);
    assert!(verdict["blocking"].is_boolean(), "blocking bool: {verdict}");
    let checks = verdict["checks"].as_array().expect("startup checks array");
    assert!(!checks.is_empty(), "startup checks should not be empty");
    for check in checks {
        assert!(
            check["name"].is_string(),
            "startup check name should be present: {check}"
        );
        assert_status_string(check);
        if let Some(reason) = check.get("reason") {
            assert!(reason.is_string(), "reason must be a string: {check}");
        }
    }
    assert!(
        verdict["suggested_next_actions"].is_array(),
        "suggested_next_actions array: {verdict}"
    );
}

fn assert_task_outcome_shape(task_outcome: &Value) {
    assert_status_string(task_outcome);
    assert!(
        task_outcome["blocking"].is_boolean(),
        "blocking bool: {task_outcome}"
    );
    for key in ["blocking_reasons", "warning_reasons"] {
        assert!(task_outcome[key].is_array(), "{key} array: {task_outcome}");
    }
}

fn assert_status_string(value: &Value) {
    let status = value["status"].as_str().expect("status string");
    assert!(
        matches!(status, "pass" | "warn" | "fail"),
        "unexpected verdict status {status}: {value}"
    );
}

fn assert_finish_uses_canonical_outcomes(output: &Value) {
    assert!(output["task_outcome"].is_object(), "{output}");
    assert!(output["evidence_integrity"].is_object(), "{output}");
    if output.get("summary_only").and_then(Value::as_bool) == Some(true) {
        assert!(output.get("evidence_history").is_none(), "{output}");
        assert!(output.get("informational_notes").is_none(), "{output}");
        assert!(output.get("facts").is_none(), "{output}");
    } else {
        assert!(output["evidence_history"].is_object(), "{output}");
    }
    assert!(output.get("verdict").is_none(), "{output}");
    assert!(output.get("finish_verdict").is_none(), "{output}");
}

fn assert_action_list_contains(actions: &Value, action: &str) {
    assert!(
        actions
            .as_array()
            .expect("suggested_next_actions array")
            .iter()
            .any(|candidate| candidate.as_str() == Some(action)),
        "suggested_next_actions should contain {action}: {actions}"
    );
}

fn assert_action_list_not_contains(actions: &Value, action: &str) {
    assert!(
        !actions
            .as_array()
            .expect("suggested_next_actions array")
            .iter()
            .any(|candidate| candidate.as_str() == Some(action)),
        "suggested_next_actions should not contain {action}: {actions}"
    );
}

fn assert_compact_verdict_safe(value: &Value, context: &str) {
    let serialized = serde_json::to_string(value).unwrap();
    for forbidden in [
        "stdout", "stderr", "tail", "excerpt", "command", "token", "secret", "env",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "{context} leaked {forbidden}: {serialized}"
        );
    }
}

fn assert_review_evidence_tools_safe(review_evidence: &Value) {
    let tools = review_evidence["tools"]
        .as_array()
        .expect("review_evidence.tools array");
    assert!(
        !tools.is_empty(),
        "review_evidence.tools should not be empty"
    );
    assert!(tools.len() <= 20, "review_evidence.tools should be bounded");
    for tool in tools {
        let tool = tool.as_str().expect("review evidence tool name");
        assert!(
            matches!(
                tool,
                "read_file"
                    | "list_project_files"
                    | "search_project_text"
                    | "search_project_texts"
                    | "git_diff"
                    | "git_diff_summary"
                    | "git_diff_hunks"
                    | "git_review_summary"
                    | "show_changes"
                    | "git_status"
                    | "workspace_hygiene_check"
            ),
            "unexpected review evidence tool name {tool}"
        );
        for forbidden in [
            "stdout", "stderr", "tail", "excerpt", "command", "token", "secret", "env",
        ] {
            assert!(
                !tool.contains(forbidden),
                "review_evidence.tools leaked {forbidden}: {review_evidence}"
            );
        }
    }
}

fn json_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|value| json_contains_key(value, key))
        }
        Value::Array(values) => values.iter().any(|value| json_contains_key(value, key)),
        _ => false,
    }
}

fn assert_no_raw_validation_output_fields(value: &Value, context: &str) {
    for key in [
        "stdout",
        "stderr",
        "stdout_tail",
        "stderr_tail",
        "stdout_tail_excerpt",
        "stderr_tail_excerpt",
        "validation_output_summary",
    ] {
        assert!(
            !json_contains_key(value, key),
            "{context} must not include {key}: {value}"
        );
    }
}

fn record_coding_task_tool_event(
    runtime: &ToolRuntime,
    session_id: &str,
    tool_name: &str,
    arguments: Value,
    success: bool,
    output: Value,
) {
    let start = runtime.sessions.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        tool_name,
        &arguments,
        crate::tool_runtime::sessions::session_tool_contract(tool_name),
    );
    let error = (!success).then_some("tool failed");
    runtime
        .sessions
        .record_tool_call_finished(start, success, &output, error, None);
}

struct FinishSummaryFixture {
    _tmp: tempfile::TempDir,
    runtime: ToolRuntime,
    project: String,
    session_id: String,
    auth: AuthContext,
    client_id: &'static str,
}

async fn finish_summary_fixture(client_id: &'static str) -> FinishSummaryFixture {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "add readme");
    let runtime = test_runtime();
    let auth = auth_context(None, true);
    let project =
        register_runner_project_at_path_with_auth(&runtime, client_id, "demo", tmp.path(), &auth)
            .await;
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some(client_id.to_string()));
    FinishSummaryFixture {
        _tmp: tmp,
        runtime,
        project,
        session_id: session.session_id,
        auth,
        client_id,
    }
}

async fn finish_coding_task_jobs_projection(fixture: &FinishSummaryFixture) -> ToolResult {
    let task = tokio::spawn({
        let runtime = fixture.runtime.clone();
        let project = fixture.project.clone();
        let session_id = fixture.session_id.clone();
        let auth = fixture.auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: false,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(false),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&fixture.runtime, fixture.client_id).await;
    assert_internal_posix_script_contains(&request, "git status --porcelain=v1 -b");
    let show_changes_stdout =
        crate::tool_runtime::framed_clean_show_changes_test_stdout("add readme", false);
    complete_patch_agent_request(
        &fixture.runtime,
        fixture.client_id,
        &request.request_id,
        0,
        &show_changes_stdout,
        "",
    )
    .await;
    task.await.unwrap()
}

async fn finish_coding_task_summary_only_with_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    session_id: String,
    auth: AuthContext,
) -> ToolResult {
    finish_coding_task_with_agent(runtime, client_id, project, session_id, auth, true).await
}

async fn finish_coding_task_with_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    session_id: String,
    auth: AuthContext,
    summary_only: bool,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only,
                        include_diff: Some(false),
                        include_workspace: None,
                        include_hygiene: Some(true),
                        include_handoff: Some(false),
                        include_validation_summary: Some(true),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    service_agent_task_until_finished(runtime, client_id, &task, "finish_coding_task summary_only")
        .await;
    task.await.unwrap()
}

async fn session_handoff_summary_only_with_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    session_id: String,
    auth: AuthContext,
) -> ToolResult {
    session_handoff_summary_only_with_agent_limit(
        runtime, client_id, project, session_id, auth, None,
    )
    .await
}

async fn session_handoff_summary_only_with_agent_limit(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    session_id: String,
    auth: AuthContext,
    limit: Option<usize>,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::SessionHandoffSummary {
                        session_id,
                        project: Some(project),
                        include_workspace: Some(true),
                        include_checkpoints: Some(false),
                        include_validation: Some(true),
                        summary_only: true,
                        limit,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    service_agent_task_until_finished(
        runtime,
        client_id,
        &task,
        "session_handoff_summary summary_only",
    )
    .await;
    task.await.unwrap()
}

#[tokio::test]
async fn coding_workflow_full_diagnostic_recommended_flow_projects_to_visible_manifest_tools() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-flow-proj", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_serviced(
        &runtime,
        "coding-flow-proj",
        json!({ "project": project, "detail": "full" }),
        &auth,
    )
    .await;
    assert!(result.success, "{:?}", result.error);

    let manifest_tools: std::collections::BTreeSet<&str> = result.output["tool_manifest"]["tools"]
        .as_array()
        .expect("tool_manifest.tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        manifest_tools.contains("finish_coding_task"),
        "coding startup should keep finish_coding_task visible"
    );

    let flow = &result.output["recommended_flow"];
    for group in ["inspect", "edit", "validate", "review", "handoff"] {
        assert!(
            flow.get(group).and_then(Value::as_array).is_some(),
            "recommended_flow must keep group key {group}"
        );
    }

    for group in ["inspect", "edit", "validate", "review", "handoff"] {
        for tool in flow[group].as_array().unwrap() {
            let tool = tool.as_str().unwrap();
            assert!(
                manifest_tools.contains(tool),
                "recommended_flow.{group} references invisible tool {tool}; visible={manifest_tools:?}"
            );
        }
    }

    let handoff = flow["handoff"].as_array().unwrap();
    assert!(
        handoff.iter().any(|tool| tool == "finish_coding_task"),
        "handoff should retain finish_coding_task when visible: {handoff:?}"
    );
}

#[tokio::test]
async fn coding_workflow_standard_omits_repeated_manifest_and_recommended_flow() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "coding-flow-full", "demo", tmp.path()).await;
    let auth = auth_context(None, true);

    let result = coding_workflow_serviced(
        &runtime,
        "coding-flow-full",
        json!({
            "project": project,
            "detail": "standard"
        }),
        &auth,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["detail"], "standard");
    assert!(result.output.get("tool_manifest").is_none());
    assert!(result.output.get("recommended_flow").is_none());
    assert!(result.output.get("rules").is_none());
}
