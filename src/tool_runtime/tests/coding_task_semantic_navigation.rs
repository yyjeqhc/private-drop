use super::support::*;
use crate::lsp_bridge::{
    LspAvailabilityStatus, LspCommandSource, LspServerStatusEntry, LspStatusResult,
    RunnerLspRequest, RunnerLspResultEnvelope, AGENT_LSP_REQUEST_KIND,
};
use crate::runner_protocol::{
    RunnerCapabilities, RunnerResultRequest, RUNNER_CAPABILITY_LSP_READ_ONLY_NAVIGATION,
};
use crate::tool_runtime::{
    known_tool_names, model_hidden_tool_names, registered_tool_specs, SessionMode, ToolResult,
    ToolRuntime,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn semantic_capabilities(enabled: bool) -> RunnerCapabilities {
    RunnerCapabilities {
        shell: true,
        git: true,
        file_read: true,
        file_write: true,
        lsp_read_only_navigation: enabled,
        ..Default::default()
    }
}

async fn register_semantic_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    root: &std::path::Path,
    lsp_capable: bool,
) -> String {
    register_agent_with_projects(
        runtime,
        client_id,
        None,
        semantic_capabilities(lsp_capable),
        vec![registered_project(project_id, &root.to_string_lossy())],
    )
    .await;
    crate::tool_runtime::runner_project_runtime_id(client_id, project_id)
}

fn spawn_start(
    runtime: &ToolRuntime,
    project: String,
    mode: SessionMode,
) -> tokio::task::JoinHandle<ToolResult> {
    let runtime = runtime.clone();
    tokio::spawn(async move {
        runtime
            .start_coding_workflow_for_test(
                project,
                None,
                None,
                Some("semantic navigation startup".to_string()),
                mode,
                false,
                false,
                Default::default(),
                None,
                None,
                Some(&auth_context(None, true)),
                None,
                None,
                crate::tool_runtime::sessions::SessionTransport::Api,
            )
            .await
    })
}

async fn next_semantic_status_request(
    runtime: &ToolRuntime,
    client_id: &str,
) -> crate::runner_protocol::RunnerRequest {
    let request = wait_for_patch_agent_request(runtime, client_id).await;
    assert_eq!(request.kind, AGENT_LSP_REQUEST_KIND);
    assert!(request.command.is_empty());
    assert!(request.stdin.is_none());
    let payload = request.lsp.as_ref().expect("typed LSP payload");
    assert_eq!(payload.request, RunnerLspRequest::Status);
    request
}

async fn complete_status_envelope(
    runtime: &ToolRuntime,
    client_id: &str,
    request_id: &str,
    envelope: RunnerLspResultEnvelope,
) {
    complete_patch_agent_request(
        runtime,
        client_id,
        request_id,
        0,
        &envelope.to_stdout_json(),
        "",
    )
    .await;
}

fn status_result(
    project_id: &str,
    detected_rust: bool,
    status: LspAvailabilityStatus,
    position_encoding: Option<&str>,
) -> LspStatusResult {
    LspStatusResult {
        project: project_id.to_string(),
        detected_languages: if detected_rust {
            vec!["rust".to_string()]
        } else {
            Vec::new()
        },
        servers: vec![LspServerStatusEntry {
            language: "rust".to_string(),
            server: "rust-analyzer".to_string(),
            available: status != LspAvailabilityStatus::Unavailable,
            running: status == LspAvailabilityStatus::Running,
            status,
            source: Some(LspCommandSource::Path),
            position_encoding: position_encoding.map(str::to_string),
        }],
        warnings: Vec::new(),
    }
}

async fn start_with_status(
    status: LspAvailabilityStatus,
    position_encoding: Option<&str>,
) -> ToolResult {
    let runtime = test_runtime();
    let temp = tempfile::tempdir().unwrap();
    seed_clean_repo(temp.path());
    let project =
        register_semantic_agent(&runtime, "semantic-agent", "demo", temp.path(), true).await;
    let task = spawn_start(&runtime, project, SessionMode::Normal);
    let request = next_semantic_status_request(&runtime, "semantic-agent").await;
    complete_status_envelope(
        &runtime,
        "semantic-agent",
        &request.request_id,
        RunnerLspResultEnvelope::ok(status_result("demo", true, status, position_encoding)),
    )
    .await;
    finish_start_servicing_locally(&runtime, "semantic-agent", task).await
}

fn seed_clean_repo(root: &std::path::Path) {
    init_git_repo(root);
    commit_file(root, "README.md", "# demo\n", "chore: seed fixture");
}

/// Standard-detail startup inspects Git through the Runner; service those
/// remaining requests locally until the coding workflow finishes.
async fn finish_start_servicing_locally(
    runtime: &ToolRuntime,
    client_id: &str,
    task: tokio::task::JoinHandle<ToolResult>,
) -> ToolResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !task.is_finished() {
        assert!(
            Instant::now() < deadline,
            "coding workflow did not finish within the 10-second test deadline"
        );
        if let Some(request) = probe_patch_agent_request(runtime, client_id).await {
            complete_agent_request_by_running_locally(runtime, client_id, request).await;
        } else {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    task.await.unwrap()
}

fn assert_optional_sections_disabled(output: &Value) {
    assert!(output["workspace"].is_object());
    assert!(output["instructions"].is_object());
    assert!(output.get("runtime_status").is_none());
    assert!(output.get("connection_state").is_none());
    assert!(output.get("git").is_none());
    assert!(output.get("rules").is_none());
    assert!(output.get("tool_manifest").is_none());
    assert!(output.get("recommended_flow").is_none());
    assert!(output.get("semantic_navigation").is_some());
}

#[tokio::test]
async fn coding_task_semantic_navigation_available_is_recommended_and_bounded() {
    let result = start_with_status(LspAvailabilityStatus::Available, None).await;
    assert!(result.success, "{result:?}");
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["available"], true);
    assert_eq!(semantic["status"], "available");
    assert_eq!(semantic["provider"], "rust-analyzer");
    assert_eq!(semantic["capability"], "lsp_read_only_navigation");
    assert_eq!(semantic["reason_code"], Value::Null);
    assert!(result.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .starts_with("wc_sess_"));
    assert_eq!(result.output["startup_verdict"]["status"], "pass");
    assert!(!result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "current_binding_unavailable"));
    assert_optional_sections_disabled(&result.output);
}

#[tokio::test]
async fn coding_task_semantic_navigation_running_propagates_position_encoding() {
    for encoding in ["utf-8", "utf-16", "utf-32"] {
        let result = start_with_status(LspAvailabilityStatus::Running, Some(encoding)).await;
        let semantic = &result.output["semantic_navigation"];
        assert_eq!(semantic["status"], "running");
        assert_eq!(semantic["available"], true);
        assert_eq!(semantic["provider"], "rust-analyzer");
        assert_eq!(semantic["capability"], "lsp_read_only_navigation");
        assert!(matches!(encoding, "utf-8" | "utf-16" | "utf-32"));
    }
}

#[tokio::test]
async fn coding_task_semantic_navigation_initializing_is_available_not_recommended() {
    let result = start_with_status(LspAvailabilityStatus::Initializing, None).await;
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["status"], "initializing");
    assert_eq!(semantic["available"], true);
    assert_eq!(semantic["provider"], "rust-analyzer");
}

#[tokio::test]
async fn coding_task_semantic_navigation_crashed_does_not_lower_startup_verdict() {
    let result = start_with_status(LspAvailabilityStatus::Crashed, None).await;
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["status"], "crashed");
    assert_eq!(semantic["available"], true);
    assert_eq!(semantic["reason_code"], "server_crashed");
    assert_eq!(result.output["startup_verdict"]["status"], "pass");
}

#[tokio::test]
async fn coding_task_semantic_navigation_unavailable_is_nonblocking() {
    let result = start_with_status(LspAvailabilityStatus::Unavailable, None).await;
    assert!(result.success, "{result:?}");
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["available"], false);
    assert_eq!(semantic["status"], "unavailable");
    assert_eq!(semantic["reason_code"], "server_unavailable");
    assert_eq!(result.output["startup_verdict"]["status"], "warn");
    assert!(result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "semantic_navigation_unavailable"));
}

#[tokio::test]
async fn coding_task_semantic_navigation_non_rust_agent_is_not_applicable() {
    let runtime = test_runtime();
    let temp = tempfile::tempdir().unwrap();
    seed_clean_repo(temp.path());
    let project =
        register_semantic_agent(&runtime, "non-rust-agent", "demo", temp.path(), true).await;
    let task = spawn_start(&runtime, project, SessionMode::Normal);
    let request = next_semantic_status_request(&runtime, "non-rust-agent").await;
    complete_status_envelope(
        &runtime,
        "non-rust-agent",
        &request.request_id,
        RunnerLspResultEnvelope::ok(status_result(
            "demo",
            false,
            LspAvailabilityStatus::Available,
            None,
        )),
    )
    .await;
    let result = finish_start_servicing_locally(&runtime, "non-rust-agent", task).await;
    assert!(result.success, "{result:?}");
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["status"], "not_applicable");
    assert_eq!(semantic["reason_code"], "rust_not_detected");
    assert_eq!(semantic["available"], false);
}

#[tokio::test]
async fn coding_task_semantic_navigation_disconnected_agent_is_nonblocking() {
    let runtime = test_runtime();
    let temp = tempfile::tempdir().unwrap();
    let project =
        register_semantic_agent(&runtime, "offline-agent", "demo", temp.path(), true).await;
    runtime
        .runner_registry
        .reconcile_disconnect("offline-agent", "inst")
        .await;
    let result = runtime
        .start_coding_workflow_for_test(
            project,
            None,
            None,
            Some("semantic navigation startup".to_string()),
            SessionMode::Normal,
            false,
            false,
            Default::default(),
            None,
            None,
            Some(&auth_context(None, true)),
            None,
            None,
            crate::tool_runtime::sessions::SessionTransport::Api,
        )
        .await;
    assert!(result.success, "{result:?}");
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["status"], "agent_unavailable");
    assert_eq!(semantic["reason_code"], "agent_not_connected");
    // The offline runner is the deterministic blocker; the semantic probe
    // itself adds no failure of its own.
    assert_eq!(result.output["startup_verdict"]["status"], "fail");
    assert_eq!(result.output["startup_verdict"]["blocking"], true);
    assert!(result.output["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker == "runner_unavailable"));
    // Git inspection cannot reach the offline agent: advisory, not blocking.
    let warning_kinds: Vec<&str> = result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        warning_kinds.contains(&"git_unavailable"),
        "{warning_kinds:?}"
    );
    assert!(probe_patch_agent_request(&runtime, "offline-agent")
        .await
        .is_none());
}

#[tokio::test]
async fn coding_task_semantic_navigation_timeout_uses_one_budget_and_cancels_waiter() {
    let runtime = test_runtime().with_semantic_navigation_probe_timeout(Duration::from_millis(25));
    let temp = tempfile::tempdir().unwrap();
    seed_clean_repo(temp.path());
    let project =
        register_semantic_agent(&runtime, "timeout-agent", "demo", temp.path(), true).await;
    let started = Instant::now();
    let task = spawn_start(&runtime, project, SessionMode::Normal);
    let request = next_semantic_status_request(&runtime, "timeout-agent").await;
    let result = finish_start_servicing_locally(&runtime, "timeout-agent", task).await;
    // This wall-clock bound includes the normal Git startup inspection that
    // follows the 25ms semantic-navigation probe. Keep it well below the
    // helper's 10-second liveness deadline so a lost probe budget still fails,
    // but leave enough scheduler headroom for the full Windows test suite.
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(result.success, "{result:?}");
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["status"], "probe_timeout");
    assert_eq!(semantic["reason_code"], "status_probe_timed_out");
    assert_eq!(semantic["supported"], true);
    assert_eq!(semantic["available"], Value::Null);
    assert_eq!(semantic["provider"], Value::Null);
    assert_eq!(semantic["capability"], "lsp_read_only_navigation");
    assert!(!result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "semantic_navigation_unavailable"));
    assert_eq!(result.output["startup_verdict"]["status"], "pass");

    let expired = runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "timeout-agent".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: request.request_id,
            exit_code: Some(0),
            stdout: Some("{}".to_string()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .expect_err("timed-out startup probe must remove pending waiter");
    assert!(expired.contains("unknown or expired shell request"));

    let session_id = result.output["session"]["session_id"].as_str().unwrap();
    let summary = runtime.sessions.summary(session_id, Some(20)).unwrap();
    assert!(summary
        .events
        .iter()
        .all(|event| event.tool_name != "lsp_status"));
    assert_eq!(summary.counts.failed, 0);
}

#[tokio::test]
async fn coding_task_semantic_navigation_malformed_result_is_sanitized() {
    let runtime = test_runtime();
    let temp = tempfile::tempdir().unwrap();
    seed_clean_repo(temp.path());
    let project =
        register_semantic_agent(&runtime, "malformed-agent", "demo", temp.path(), true).await;
    let task = spawn_start(&runtime, project, SessionMode::Normal);
    let request = next_semantic_status_request(&runtime, "malformed-agent").await;
    complete_patch_agent_request(
        &runtime,
        "malformed-agent",
        &request.request_id,
        0,
        r#"{"raw":"/private/secret","stdout":"do not leak"}"#,
        "stderr must not leak",
    )
    .await;
    let result = finish_start_servicing_locally(&runtime, "malformed-agent", task).await;
    assert!(result.success, "{result:?}");
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["status"], "probe_failed");
    assert_eq!(semantic["reason_code"], "malformed_agent_result");
    let serialized = semantic.to_string();
    for forbidden in ["/private/secret", "do not leak", "stderr"] {
        assert!(!serialized.contains(forbidden), "{serialized}");
    }
    assert!(result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "semantic_navigation_unavailable"));
}

#[tokio::test]
async fn coding_task_semantic_navigation_agent_failure_uses_fixed_reason_code() {
    let runtime = test_runtime();
    let temp = tempfile::tempdir().unwrap();
    seed_clean_repo(temp.path());
    let project =
        register_semantic_agent(&runtime, "failed-agent", "demo", temp.path(), true).await;
    let task = spawn_start(&runtime, project, SessionMode::Normal);
    let request = next_semantic_status_request(&runtime, "failed-agent").await;
    complete_status_envelope(
        &runtime,
        "failed-agent",
        &request.request_id,
        RunnerLspResultEnvelope::err("lsp_protocol_error", "private raw failure detail"),
    )
    .await;
    let result = finish_start_servicing_locally(&runtime, "failed-agent", task).await;
    assert!(result.success, "{result:?}");
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic["status"], "probe_failed");
    assert_eq!(semantic["reason_code"], "status_probe_failed");
    assert!(!semantic.to_string().contains("private raw failure detail"));
    assert!(result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "semantic_navigation_unavailable"));
}

#[tokio::test]
async fn coding_task_semantic_navigation_read_only_keeps_compact_shape() {
    let runtime = test_runtime();
    let temp = tempfile::tempdir().unwrap();
    seed_clean_repo(temp.path());
    let project =
        register_semantic_agent(&runtime, "compact-agent", "demo", temp.path(), true).await;
    let task = spawn_start(&runtime, project, SessionMode::ReadOnly);
    let request = next_semantic_status_request(&runtime, "compact-agent").await;
    complete_status_envelope(
        &runtime,
        "compact-agent",
        &request.request_id,
        RunnerLspResultEnvelope::ok(status_result(
            "demo",
            true,
            LspAvailabilityStatus::Available,
            None,
        )),
    )
    .await;
    let result = finish_start_servicing_locally(&runtime, "compact-agent", task).await;
    let semantic = &result.output["semantic_navigation"];
    assert_eq!(semantic.as_object().unwrap().len(), 6);
    assert_eq!(semantic["supported"], true);
    assert_eq!(semantic["status"], "available");
    assert_eq!(semantic["provider"], "rust-analyzer");
    assert_eq!(semantic["capability"], "lsp_read_only_navigation");
    assert_eq!(result.output["session"]["mode"], "read_only");
    assert_optional_sections_disabled(&result.output);
}

#[test]
fn coding_workflow_semantic_navigation_output_schema_is_explicit_and_surface_counts_are_stable() {
    let specs = registered_tool_specs();
    let runtime_tool_count = specs.len();
    assert_eq!(
        runtime_tool_count + model_hidden_tool_names().count(),
        known_tool_names().count(),
        "visible runtime tool count + hidden tools must cover every known tool"
    );
    let schema = crate::tool_runtime::registry::coding_workflow_diagnostic_output_schema_for_test();
    let standard = schema["properties"]["output"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|variant| variant["properties"]["detail"]["const"] == "standard")
        .unwrap();
    let semantic = &standard["properties"]["semantic_navigation"];
    assert_eq!(semantic["additionalProperties"], false);
    assert_eq!(
        semantic["properties"]["status"]["enum"],
        json!([
            "running",
            "available",
            "initializing",
            "crashed",
            "unavailable",
            "not_applicable",
            "agent_unavailable",
            "agent_capability_unavailable",
            "probe_timeout",
            "probe_failed"
        ])
    );
    assert!(semantic["properties"]["reason_code"]["anyOf"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["type"] == "null"));

    let openapi = crate::openapi::build_openapi_spec();
    let operation_count: usize = openapi["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|methods| methods.as_object().unwrap().len())
        .sum();
    assert_eq!(operation_count, 22);
    assert!(!known_tool_names().any(|name| name == "semantic_navigation"));
    assert!(crate::runner_protocol::RUNNER_CAPABILITY_NAMES
        .contains(&RUNNER_CAPABILITY_LSP_READ_ONLY_NAVIGATION));
}
