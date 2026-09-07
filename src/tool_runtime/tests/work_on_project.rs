//! Focused tests for the canonical `work_on_project` coding entry point.
//!
//! `work_on_project` validates one of two project sources plus the task inputs,
//! invokes the shared coding workflow engine, and projects a compact startup
//! result. It never binds a current window, never guesses a recent Session, and
//! never falls back to a credential-wide Session.

use super::reconnect::dispatch_coding_call_in_window;
use super::support::*;
use crate::lsp_bridge::{RunnerLspRequest, RunnerLspResultEnvelope, AGENT_LSP_REQUEST_KIND};
use crate::runner_protocol::RunnerCapabilities;
use crate::tool_runtime::kernel::{
    HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolTransport,
};
use crate::tool_runtime::permissions::{AuthorityMode, PermissionEvaluator};
use crate::tool_runtime::sessions::{SessionEvent, SessionGuards};
use crate::tool_runtime::{
    registered_tool_specs, SessionMode, StartupDetail, ToolCall, ToolResult, ToolRuntime,
};
use serde_json::{json, Value};
use std::path::Path;

fn work_on_project_call(project: &str, instruction: &str, session_id: Option<&str>) -> ToolCall {
    work_on_project_call_with_projections(project, instruction, session_id, true, true)
}

fn work_on_project_call_with_instruction_projection(
    project: &str,
    instruction: &str,
    session_id: Option<&str>,
    include_project_instructions: bool,
) -> ToolCall {
    work_on_project_call_with_projections(
        project,
        instruction,
        session_id,
        include_project_instructions,
        true,
    )
}

fn work_on_project_call_with_projections(
    project: &str,
    instruction: &str,
    session_id: Option<&str>,
    include_project_instructions: bool,
    include_workflow_guidance: bool,
) -> ToolCall {
    ToolCall::WorkOnProject {
        project: project.to_string(),
        client_id: None,
        path: None,
        mode: None,
        base_ref: None,
        instruction: instruction.to_string(),
        include_project_instructions,
        include_workflow_guidance,
        session_id: session_id.map(str::to_string),
    }
}

fn path_work_on_project_call(
    client_id: &str,
    path: &str,
    instruction: &str,
    session_id: Option<&str>,
) -> ToolCall {
    ToolCall::WorkOnProject {
        project: String::new(),
        client_id: Some(client_id.to_string()),
        path: Some(path.to_string()),
        mode: None,
        base_ref: None,
        instruction: instruction.to_string(),
        include_project_instructions: true,
        include_workflow_guidance: true,
        session_id: session_id.map(str::to_string),
    }
}

fn worktree_work_on_project_call(
    client_id: &str,
    path: &str,
    instruction: &str,
    base_ref: Option<&str>,
    session_id: Option<&str>,
) -> ToolCall {
    ToolCall::WorkOnProject {
        project: String::new(),
        client_id: Some(client_id.to_string()),
        path: Some(path.to_string()),
        mode: Some("worktree".to_string()),
        base_ref: base_ref.map(str::to_string),
        instruction: instruction.to_string(),
        include_project_instructions: true,
        include_workflow_guidance: true,
        session_id: session_id.map(str::to_string),
    }
}

fn managed_fixture_git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run managed-worktree fixture git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn seed_managed_tool_runtime_fixture(source: &Path, worktree: &Path) -> String {
    std::fs::create_dir_all(source).unwrap();
    managed_fixture_git(source, &["init"]);
    managed_fixture_git(
        source,
        &["config", "user.email", "webcodex@example.invalid"],
    );
    managed_fixture_git(source, &["config", "user.name", "WebCodex Test"]);
    std::fs::write(source.join("hello.txt"), "committed\n").unwrap();
    managed_fixture_git(source, &["add", "hello.txt"]);
    managed_fixture_git(source, &["commit", "-m", "seed"]);
    let sha = managed_fixture_git(source, &["rev-parse", "HEAD"]);
    let worktree_arg = worktree.to_string_lossy().to_string();
    managed_fixture_git(
        source,
        &[
            "worktree",
            "add",
            "--detach",
            worktree_arg.as_str(),
            sha.as_str(),
        ],
    );
    std::fs::write(source.join("hello.txt"), "dirty source only\n").unwrap();
    sha
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_with_managed_worktree_runner(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    source_path: &str,
    managed_path: &str,
    agent_project_id: &str,
    base_ref: &str,
    base_sha: &str,
    source_dirty: bool,
    outcome: &str,
    registered: bool,
    first_indeterminate: bool,
) -> (ToolResult, Vec<Value>) {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth_context(None, true);
        async move { runtime.dispatch_with_auth(call, Some(&auth)).await }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut prepare_payloads = Vec::new();
    let mut sent_indeterminate = false;
    loop {
        if task.is_finished() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "managed-worktree coding call did not finish within 10 seconds for client {client_id}"
        );
        if let Some(request) = probe_patch_agent_request(runtime, client_id).await {
            if request.kind == "prepare_managed_worktree" {
                let payload: Value =
                    serde_json::from_str(request.stdin.as_deref().unwrap()).unwrap();
                assert_eq!(payload["path"], source_path);
                prepare_payloads.push(payload);
                if first_indeterminate && !sent_indeterminate {
                    sent_indeterminate = true;
                    let response = json!({
                        "error_code": "operation_indeterminate",
                        "error_kind": "operation_indeterminate",
                        "failure_kind": "operation_indeterminate",
                        "state_changed": true,
                    });
                    complete_patch_agent_request(
                        runtime,
                        client_id,
                        &request.request_id,
                        1,
                        &response.to_string(),
                        "",
                    )
                    .await;
                    continue;
                }
                let response = json!({
                    "id": format!("agent:{client_id}:{agent_project_id}"),
                    "agent_project_id": agent_project_id,
                    "client_id": client_id,
                    "name": agent_project_id,
                    "path": managed_path,
                    "kind": "auto_registered",
                    "registration_source": "auto_registered",
                    "description": null,
                    "allow_patch": true,
                    "disabled": false,
                    "revision": format!("sha256:{}", "b".repeat(64)),
                    "source": "managed_worktree",
                    "outcome": outcome,
                    "registered": registered,
                    "created_config": registered,
                    "changed": registered,
                    "recovered": !registered,
                    "managed": true,
                    "base_ref": base_ref,
                    "base_sha": base_sha,
                    "source_dirty": source_dirty,
                });
                complete_patch_agent_request(
                    runtime,
                    client_id,
                    &request.request_id,
                    0,
                    &response.to_string(),
                    "",
                )
                .await;
            } else if request.kind == AGENT_LSP_REQUEST_KIND {
                complete_patch_agent_request(
                    runtime,
                    client_id,
                    &request.request_id,
                    0,
                    &RunnerLspResultEnvelope::err(
                        "lsp_status_unavailable",
                        "fixture intentionally has no language server",
                    )
                    .to_stdout_json(),
                    "",
                )
                .await;
            } else {
                complete_agent_request_by_running_locally(runtime, client_id, request).await;
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    (task.await.unwrap(), prepare_payloads)
}

/// Drive any coding startup to completion while recording every Runner request.
/// The typed LSP status probe gets a valid bounded error envelope; file/Git and
/// overview requests use the existing local fixture implementation.
async fn dispatch_recording_startup_requests(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    auth: Option<&crate::auth::AuthContext>,
    window_id: &str,
) -> (ToolResult, Vec<String>) {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.cloned();
        let window_id = window_id.to_string();
        async move {
            let window = crate::client_window::ClientWindow::for_test(&window_id);
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_window(
                    call,
                    auth.as_ref(),
                    crate::tool_runtime::sessions::SessionTransport::Mcp,
                    Default::default(),
                    Some(&window),
                )
                .await
        }
    });
    record_startup_requests(runtime, client_id, task).await
}

async fn dispatch_recording_coding_workflow_diagnostic(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    instruction: &str,
    detail: StartupDetail,
    auth: Option<&crate::auth::AuthContext>,
) -> (ToolResult, Vec<String>) {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.to_string();
        let instruction = instruction.to_string();
        let auth = auth.cloned();
        async move {
            runtime
                .start_coding_workflow_for_test(
                    project,
                    None,
                    None,
                    Some(instruction),
                    SessionMode::Normal,
                    false,
                    false,
                    detail,
                    None,
                    None,
                    auth.as_ref(),
                    None,
                    None,
                    crate::tool_runtime::sessions::SessionTransport::Mcp,
                )
                .await
        }
    });
    record_startup_requests(runtime, client_id, task).await
}

async fn record_startup_requests(
    runtime: &ToolRuntime,
    client_id: &str,
    task: tokio::task::JoinHandle<ToolResult>,
) -> (ToolResult, Vec<String>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut request_kinds = Vec::new();
    loop {
        if task.is_finished() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "coding startup did not finish within 10 seconds; serviced requests: {request_kinds:?}"
        );
        let Some(request) = probe_patch_agent_request(runtime, client_id).await else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            continue;
        };
        request_kinds.push(request.kind.clone());
        if request.kind == AGENT_LSP_REQUEST_KIND {
            assert_eq!(
                request.lsp.as_ref().map(|payload| &payload.request),
                Some(&RunnerLspRequest::Status)
            );
            complete_patch_agent_request(
                runtime,
                client_id,
                &request.request_id,
                0,
                &RunnerLspResultEnvelope::err(
                    "lsp_status_unavailable",
                    "fixture intentionally has no language server",
                )
                .to_stdout_json(),
                "",
            )
            .await;
        } else {
            complete_agent_request_by_running_locally(runtime, client_id, request).await;
        }
    }
    (task.await.unwrap(), request_kinds)
}

async fn dispatch_startup_without_window(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    auth: Option<&crate::auth::AuthContext>,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.cloned();
        async move {
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_window(
                    call,
                    auth.as_ref(),
                    crate::tool_runtime::sessions::SessionTransport::Mcp,
                    Default::default(),
                    None,
                )
                .await
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !task.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "coding startup without window did not finish within 10 seconds for client {client_id}"
        );
        if let Some(request) = probe_patch_agent_request(runtime, client_id).await {
            complete_agent_request_by_running_locally(runtime, client_id, request).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    task.await.unwrap()
}

async fn dispatch_with_path_runner(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    agent_project_id: &str,
    project_path: &str,
    outcome: &str,
    registered: bool,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth_context(None, true);
        async move { runtime.dispatch_with_auth(call, Some(&auth)).await }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if task.is_finished() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "path-based coding call did not finish within 10 seconds for client {client_id}"
        );
        if let Some(request) = probe_patch_agent_request(runtime, client_id).await {
            if request.kind == "resolve_or_register_project" {
                let payload: Value =
                    serde_json::from_str(request.stdin.as_deref().unwrap()).unwrap();
                assert_eq!(payload["path"], project_path);
                let response = json!({
                    "id": format!("agent:{client_id}:{agent_project_id}"),
                    "agent_project_id": agent_project_id,
                    "client_id": client_id,
                    "name": agent_project_id,
                    "path": project_path,
                    "kind": "auto_registered",
                    "description": null,
                    "allow_patch": true,
                    "disabled": false,
                    "revision": format!("sha256:{}", "a".repeat(64)),
                    "source": "path",
                    "outcome": outcome,
                    "registered": registered,
                    "created_config": registered,
                    "changed": registered,
                    "recovered": !registered,
                });
                complete_patch_agent_request(
                    runtime,
                    client_id,
                    &request.request_id,
                    0,
                    &response.to_string(),
                    "",
                )
                .await;
            } else {
                complete_agent_request_by_running_locally(runtime, client_id, request).await;
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    task.await.unwrap()
}

fn instruction_events(runtime: &ToolRuntime, session_id: &str) -> Vec<SessionEvent> {
    runtime
        .sessions
        .summary(session_id, Some(200))
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == "task_instruction")
        .collect()
}

fn valid_work_on_project_projection_input() -> serde_json::Value {
    json!({
        "detail": "standard",
        "session": {
            "session_id": "wc_sess_projection",
            "continuation": "created",
            "execution_context": {},
        },
        "project": {
            "resolved_id": "agent:wop:demo",
        },
        "project_resolution": {
            "source": "project",
            "outcome": "resolved_existing_project",
            "resolved_project": "agent:wop:demo",
            "registered": false,
        },
        "workspace": {
            "status": "clean",
            "git_available": true,
            "branch": "main",
            "head": "0123456789abcdef0123456789abcdef01234567",
            "clean": true,
            "conflicts": 0,
        },
        "workflow": crate::tool_runtime::startup_brief::builtin_coding_workflow_projection(),
        "instructions": {
            "status": "loaded",
            "sources": [],
            "content_included": true,
            "truncated": false,
            "total_chars": 0,
        },
        "semantic_navigation": {
            "supported": false,
            "available": false,
            "status": "not_applicable",
            "capability": null,
            "reason_code": "project_not_agent_backed",
        },
        "repository": {
            "status": "unavailable",
            "reason_code": "not_requested_by_work_on_project",
        },
        "continuation": {
            "suggested_next_actions": {
                "items": [],
            },
            "jobs": {
                "active_count": 0,
                "blocking_active_count": 0,
                "nonblocking_active_count": 0,
                "recovering_count": 0,
                "terminal_pending_count": 0,
                "latest_status": "not_observed",
            },
        },
        "blockers": [],
        "warnings": [],
        "startup_verdict": {
            "status": "pass",
            "blocking": false,
            "suggested_next_actions": [],
        },
    })
}

#[test]
fn work_on_project_schema_and_registration() {
    let specs = registered_tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert!(names.contains(&"work_on_project"), "missing from specs");

    // Workflow bootstrap mutates durable Session state without adding a new
    // interactive approval; its existing runtime:read authority is unchanged.
    let metadata = crate::tool_runtime::metadata::lookup_tool_metadata("work_on_project").unwrap();
    assert_eq!(
        metadata.effect,
        crate::tool_runtime::metadata::ToolEffect::Mutate
    );
    assert_eq!(
        metadata.risk,
        crate::tool_runtime::metadata::ToolRisk::WorkflowManage
    );
    assert_eq!(
        metadata.approval,
        crate::tool_runtime::metadata::ToolApprovalPolicy::None
    );
    assert_eq!(
        metadata.idempotency,
        crate::tool_runtime::metadata::ToolIdempotency::NonIdempotent
    );
    assert!(!metadata.destructive);
    assert!(!metadata.shell_like);
    assert!(metadata.requires_project);
    assert_eq!(
        metadata.authority,
        crate::tool_runtime::metadata::ToolAuthorityPolicy::Require("runtime:read")
    );
    assert_eq!(
        crate::tool_runtime::tool_definition::runtime_tool_category("work_on_project"),
        "workflow"
    );
    let definition =
        crate::tool_runtime::tool_definition::lookup_tool_definition("work_on_project").unwrap();
    assert_eq!(
        definition.runner_capability,
        Some(crate::tool_runtime::RunnerCapabilityRequirement::GitOrShell)
    );
    assert!(!definition.requires_explicit_business_session());

    // Keep the model-facing schema simple enough for reliable host projection.
    // Project-source exclusivity remains authoritative in ToolCall/runtime parsing.
    let spec = spec_named(&specs, "work_on_project");
    assert_eq!(spec.input_schema["type"], "object");
    assert_eq!(required_fields(spec), vec!["instruction"]);
    assert_eq!(spec.input_schema["additionalProperties"], false);
    let props = spec.input_schema["properties"].as_object().unwrap();
    for field in [
        "project",
        "client_id",
        "path",
        "mode",
        "base_ref",
        "instruction",
        "include_project_instructions",
        "include_workflow_guidance",
        "session_id",
    ] {
        assert!(
            props.contains_key(field),
            "missing explicit {field} property"
        );
    }
    assert_eq!(props["project"]["minLength"], 1);
    assert!(
        props["path"].get("pattern").is_none(),
        "path portability is enforced by ToolCall/runtime parsing, not a POSIX-only schema pattern"
    );
    assert_eq!(props["instruction"]["minLength"], 1);
    assert_eq!(
        props["instruction"]["maxLength"],
        crate::tool_runtime::sessions::MAX_CODING_INSTRUCTION_CHARS
    );
    assert_eq!(props["session_id"]["type"], "string");
    assert_eq!(props["session_id"]["pattern"], "^wc_sess_[A-Za-z0-9_]+$");
    assert_eq!(props["include_project_instructions"]["type"], "boolean");
    assert_eq!(props["include_project_instructions"]["default"], true);
    assert_eq!(props["include_workflow_guidance"]["type"], "boolean");
    assert_eq!(props["include_workflow_guidance"]["default"], true);
    for keyword in [
        "oneOf",
        "anyOf",
        "allOf",
        "not",
        "dependentRequired",
        "if",
        "then",
        "else",
    ] {
        assert!(
            spec.input_schema.get(keyword).is_none(),
            "work_on_project model schema must not use top-level {keyword}"
        );
    }
    let schema_accepts = |value: Value| {
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(
            &value,
            &spec.input_schema,
        )
        .is_ok()
    };
    assert!(schema_accepts(
        json!({"project": SAMPLE_PROJECT, "instruction": "do it"})
    ));
    assert!(schema_accepts(json!({
        "project": SAMPLE_PROJECT,
        "instruction": "do it",
        "include_project_instructions": false,
        "include_workflow_guidance": false
    })));
    assert!(schema_accepts(json!({
        "client_id": "special",
        "path": "/root/git/example",
        "instruction": "do it"
    })));
    assert!(
        schema_accepts(json!({"instruction": "runtime must select the source"})),
        "model schema intentionally advertises a safe source-selection superset"
    );
    assert!(schema_accepts(json!({
        "project": SAMPLE_PROJECT,
        "client_id": "special",
        "path": "/root/git/example",
        "instruction": "runtime must reject ambiguity"
    })));
    let accepted = crate::tool_runtime::registry::accepted_flattened_args_for_spec(spec);
    for field in [
        "project",
        "client_id",
        "path",
        "instruction",
        "include_project_instructions",
        "include_workflow_guidance",
        "session_id",
    ] {
        assert!(
            accepted.contains(&field.to_string()),
            "flattened Action projection missing {field}"
        );
    }

    // The canonical entry must not expose internal diagnostic controls.
    for hidden in [
        "resume_session_id",
        "deny_write_tools",
        "deny_shell_tools",
        "execution_context",
        "detail",
        "temporary_project_name",
    ] {
        assert!(
            !props.contains_key(hidden),
            "work_on_project schema must not expose {hidden}"
        );
    }

    // Output schema describes the compact projection fields.
    let output = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let output_props = output["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "session_id",
        "project",
        "resolved_project",
        "project_resolution",
        "continuation",
        "execution_context",
        "readiness",
        "workspace",
        "worktree",
        "repository",
        "workflow",
        "instructions",
        "semantic_navigation",
        "jobs",
        "blockers",
        "warnings",
        "suggested_next_actions",
    ] {
        assert!(
            output_props.contains_key(field),
            "work_on_project output schema should include {field}"
        );
    }
    for hidden in [
        "runtime_status",
        "connection_state",
        "authority",
        "tool_manifest",
        "recommended_flow",
        "startup_verdict",
        "git",
        "continuation_feedback",
        "deterministic",
        "llm_summary",
    ] {
        assert!(
            !output_props.contains_key(hidden),
            "work_on_project output schema must not include {hidden}"
        );
    }

    // ToolCall parsing maps the wrapper's session_id to the business accessor.
    let call = ToolCall::from_tool_name(
        "work_on_project",
        json!({
            "project": SAMPLE_PROJECT,
            "instruction": "do the thing",
            "session_id": "wc_sess_target"
        }),
    )
    .unwrap();
    match &call {
        ToolCall::WorkOnProject {
            include_project_instructions,
            include_workflow_guidance,
            session_id,
            ..
        } => {
            assert!(*include_project_instructions);
            assert!(*include_workflow_guidance);
            assert_eq!(session_id.as_deref(), Some("wc_sess_target"));
        }
        _ => panic!("expected WorkOnProject"),
    }
    assert_eq!(call.project(), Some(SAMPLE_PROJECT));
    assert_eq!(call.session_id(), Some("wc_sess_target"));

    let suppressed = ToolCall::from_tool_name(
        "work_on_project",
        json!({
            "project": SAMPLE_PROJECT,
            "instruction": "do the thing without repeating static context",
            "include_project_instructions": false,
            "include_workflow_guidance": false
        }),
    )
    .unwrap();
    match suppressed {
        ToolCall::WorkOnProject {
            include_project_instructions,
            include_workflow_guidance,
            ..
        } => {
            assert!(!include_project_instructions);
            assert!(!include_workflow_guidance);
        }
        _ => panic!("expected WorkOnProject"),
    }

    let audit = super::super::tool_audit::session_log_arguments_for_tool_request(
        "work_on_project",
        &json!({
            "project": SAMPLE_PROJECT,
            "instruction": "do not persist this full instruction body",
            "include_project_instructions": false,
            "include_workflow_guidance": false
        }),
    );
    assert_eq!(audit["include_project_instructions"], false);
    assert_eq!(audit["include_workflow_guidance"], false);
    assert_eq!(audit["instruction_present"], true);
    assert!(audit["instruction_summary"].is_string());
    assert!(audit.get("instruction").is_none());
}

#[test]
fn work_on_project_tool_call_enforces_authoritative_source_contract() {
    assert!(
        ToolCall::from_tool_name("work_on_project", json!({})).is_err(),
        "project source and instruction are required"
    );
    assert!(
        ToolCall::from_tool_name("work_on_project", json!({"project": SAMPLE_PROJECT})).is_err(),
        "instruction is required"
    );
    let project_call = ToolCall::from_tool_name(
        "work_on_project",
        json!({"project": SAMPLE_PROJECT, "instruction": "do it"}),
    )
    .unwrap();
    assert_eq!(project_call.project(), Some(SAMPLE_PROJECT));
    let path_call = ToolCall::from_tool_name(
        "work_on_project",
        json!({
            "client_id": "special",
            "path": "/root/git/example",
            "instruction": "do it"
        }),
    )
    .unwrap();
    assert!(path_call.project().is_none());
    let worktree_call = ToolCall::from_tool_name(
        "work_on_project",
        json!({
            "client_id": "special",
            "path": "/root/git/example",
            "mode": "worktree",
            "base_ref": "origin/main",
            "instruction": "do it in isolation"
        }),
    )
    .unwrap();
    match worktree_call {
        ToolCall::WorkOnProject { mode, base_ref, .. } => {
            assert_eq!(mode.as_deref(), Some("worktree"));
            assert_eq!(base_ref.as_deref(), Some("origin/main"));
        }
        _ => panic!("expected WorkOnProject"),
    }
    for invalid in [
        json!({"instruction": "no source"}),
        json!({"project": SAMPLE_PROJECT, "client_id": "special", "instruction": "mixed"}),
        json!({"project": SAMPLE_PROJECT, "path": "/root/git/example", "instruction": "mixed"}),
        json!({"project": SAMPLE_PROJECT, "client_id": "special", "path": "/root/git/example", "instruction": "mixed"}),
        json!({"client_id": "special", "instruction": "missing path"}),
        json!({"path": "/root/git/example", "instruction": "missing client"}),
    ] {
        assert!(
            ToolCall::from_tool_name("work_on_project", invalid.clone()).is_err(),
            "authoritative parser accepted invalid source form: {invalid}"
        );
    }
    // The schema declares additionalProperties: false so advanced
    // internal diagnostic controls are not part of the canonical entry surface.
    let spec = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "work_on_project")
        .unwrap();
    assert_eq!(spec.input_schema["additionalProperties"], false);
    let props = spec.input_schema["properties"].as_object().unwrap();
    for hidden in [
        "resume_session_id",
        "deny_write_tools",
        "deny_shell_tools",
        "execution_context",
        "detail",
        "temporary_project_name",
    ] {
        assert!(
            !props.contains_key(hidden),
            "work_on_project schema must not expose {hidden}"
        );
    }
}

#[test]
fn work_on_project_projection_fails_closed_when_required_field_is_missing() {
    let mut output = valid_work_on_project_projection_input();
    output["session"]
        .as_object_mut()
        .unwrap()
        .remove("session_id");

    let result = crate::tool_runtime::coding_task::project_work_on_project_output(
        SAMPLE_PROJECT.to_string(),
        output,
    );
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "work_on_project_projection_failed"
    );
    assert_eq!(result.output["state_changed"], true);
    assert!(result.output["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("session_id")));
}

#[test]
fn work_on_project_projection_fails_closed_for_wrong_field_type() {
    let mut output = valid_work_on_project_projection_input();
    output["workspace"]["conflicts"] = json!("0");

    let result = crate::tool_runtime::coding_task::project_work_on_project_output(
        SAMPLE_PROJECT.to_string(),
        output,
    );
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "work_on_project_projection_failed"
    );
    assert_eq!(result.output["state_changed"], true);
}

#[test]
fn work_on_project_projection_fails_closed_for_noncanonical_workflow() {
    for include_workflow_guidance in [true, false] {
        let mut output = valid_work_on_project_projection_input();
        output["workflow"]["version"] =
            json!(crate::tool_runtime::startup_brief::BUILTIN_CODING_WORKFLOW_VERSION + 1);

        let result = crate::tool_runtime::coding_task::project_work_on_project_output_with_workflow(
            SAMPLE_PROJECT.to_string(),
            output,
            include_workflow_guidance,
        );
        assert!(!result.success);
        assert_eq!(
            result.output["error_kind"],
            "work_on_project_projection_failed"
        );
        assert_eq!(result.output["field"], "workflow");
        assert_eq!(result.output["state_changed"], true);
    }
}

#[test]
fn work_on_project_projection_does_not_default_missing_instruction_sources() {
    let mut output = valid_work_on_project_projection_input();
    output["instructions"]
        .as_object_mut()
        .unwrap()
        .remove("sources");

    let result = crate::tool_runtime::coding_task::project_work_on_project_output(
        SAMPLE_PROJECT.to_string(),
        output,
    );
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "work_on_project_projection_failed"
    );
    assert_eq!(result.output["state_changed"], true);
    assert!(result.output["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("sources")));
}

#[test]
fn work_on_project_projection_is_sparse_for_defaults_and_keeps_noteworthy_state() {
    let default_result = crate::tool_runtime::coding_task::project_work_on_project_output(
        SAMPLE_PROJECT.to_string(),
        valid_work_on_project_projection_input(),
    );
    assert!(default_result.success, "{:?}", default_result.error);
    for omitted in [
        "project_resolution",
        "execution_context",
        "readiness",
        "repository",
        "jobs",
        "blockers",
        "warnings",
        "suggested_next_actions",
        "deterministic",
        "llm_summary",
    ] {
        assert!(
            default_result.output.get(omitted).is_none(),
            "boring default field {omitted} should be omitted: {}",
            default_result.output
        );
    }
    assert_eq!(default_result.output["workspace"]["status"], "clean");
    assert!(default_result.output["workspace"]["branch"].is_string());
    assert!(default_result.output["workspace"]["head"].is_string());
    for omitted in ["git_available", "clean", "conflicts"] {
        assert!(default_result.output["workspace"].get(omitted).is_none());
    }
    assert_eq!(
        default_result.output["instructions"]["content_included"],
        true
    );
    for omitted in ["changed_sources", "truncated", "total_chars"] {
        assert!(default_result.output["instructions"].get(omitted).is_none());
    }

    let mut noteworthy = valid_work_on_project_projection_input();
    noteworthy["session"]["execution_context"] = json!({"default_cwd": "src"});
    noteworthy["project_resolution"] = json!({
        "source": "path",
        "outcome": "auto_registered",
        "resolved_project": "agent:wop:demo",
        "registered": true,
    });
    noteworthy["workspace"]["status"] = json!("blocked");
    noteworthy["workspace"]["conflicts"] = json!(2);
    noteworthy["repository"] = json!({
        "status": "unavailable",
        "reason_code": "probe_failed",
    });
    noteworthy["continuation"]["jobs"]["active_count"] = json!(1);
    noteworthy["continuation"]["jobs"]["latest_status"] = json!("running");
    noteworthy["blockers"] = json!(["workspace_conflicts"]);
    noteworthy["warnings"] = json!(["active_jobs_present"]);
    noteworthy["startup_verdict"] = json!({
        "status": "fail",
        "blocking": true,
        "suggested_next_actions": ["inspect or await blocking active jobs"],
    });
    let noteworthy_result = crate::tool_runtime::coding_task::project_work_on_project_output(
        SAMPLE_PROJECT.to_string(),
        noteworthy,
    );
    assert!(noteworthy_result.success, "{:?}", noteworthy_result.error);
    assert_eq!(
        noteworthy_result.output["project_resolution"]["outcome"],
        "auto_registered"
    );
    assert_eq!(
        noteworthy_result.output["execution_context"]["default_cwd"],
        "src"
    );
    assert_eq!(noteworthy_result.output["readiness"]["status"], "fail");
    assert_eq!(
        noteworthy_result.output["repository"]["reason_code"],
        "probe_failed"
    );
    assert_eq!(noteworthy_result.output["workspace"]["conflicts"], 2);
    assert_eq!(noteworthy_result.output["jobs"]["active_count"], 1);
    assert_eq!(noteworthy_result.output["jobs"]["latest_status"], "running");
    assert_eq!(
        noteworthy_result.output["blockers"],
        json!(["workspace_conflicts"])
    );
    assert_eq!(
        noteworthy_result.output["warnings"],
        json!(["active_jobs_present"])
    );
    assert_eq!(
        noteworthy_result.output["suggested_next_actions"],
        json!(["inspect or await blocking active jobs"])
    );
    let schema = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let instance = json!({ "success": true, "output": noteworthy_result.output });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| panic!("noteworthy sparse output must match schema: {error}"));
}

#[tokio::test]
async fn work_on_project_without_session_id_always_creates_fresh_session() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "wop-create", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let result = dispatch_coding_call_in_window(
        &runtime,
        "wop-create",
        work_on_project_call("demo", "first root instruction", None),
        Some(&auth),
        "wop-create-window",
    )
    .await;
    assert!(result.success, "{:?}", result.error);

    // Compact projection keeps identity and omits boring default metadata.
    let session_id = result.output["session_id"].as_str().unwrap().to_string();
    assert!(session_id.starts_with("wc_sess_"));
    assert_eq!(result.output["project"], "demo");
    assert_eq!(result.output["resolved_project"], project);
    assert_eq!(result.output["continuation"], "created");
    for omitted in [
        "project_resolution",
        "execution_context",
        "repository",
        "jobs",
        "blockers",
        "suggested_next_actions",
        "deterministic",
        "llm_summary",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "boring default field {omitted} should be omitted: {}",
            result.output
        );
    }
    assert_eq!(result.output["readiness"]["status"], "warn");
    assert!(result.output["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings
            .iter()
            .any(|warning| warning == "semantic_navigation_unavailable")));
    assert_eq!(
        result.output["workflow"],
        crate::tool_runtime::startup_brief::builtin_coding_workflow_projection()
    );
    let model_protocol = &result.output["workflow"]["model_protocol"];
    assert!(model_protocol["session_recording"]
        .as_str()
        .is_some_and(|value| value.contains("recording_session_id")));
    assert!(model_protocol["session_message_ack"]
        .as_str()
        .is_some_and(|value| value.contains("ack_session_message_ids")));
    assert!(
        result.output["workflow"]["roles"]["implementation_owner"]["guidance"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item
                .as_str()
                .is_some_and(|value| value.contains("reuse the same assertion_name")))
    );
    assert!(result.output["instructions"].is_object());
    for hidden in [
        "runtime_status",
        "connection_state",
        "authority",
        "tool_manifest",
        "recommended_flow",
        "startup_verdict",
        "git",
        "continuation_feedback",
    ] {
        assert!(
            !result.output.as_object().unwrap().contains_key(hidden),
            "compact output must not include {hidden}"
        );
    }

    // A new active normal session was created with the instruction as root.
    let summary = runtime.sessions.summary(&session_id, Some(50)).unwrap();
    assert_eq!(summary.project.as_deref(), Some(project.as_str()));
    assert_eq!(summary.mode, SessionMode::Normal);
    assert!(!summary.guards.deny_write_tools);
    assert!(!summary.guards.deny_shell_tools);
    let instructions = instruction_events(&runtime, &session_id);
    assert_eq!(instructions.len(), 1);
    assert_eq!(
        instructions[0].instruction.as_deref(),
        Some("first root instruction")
    );

    // A second call in the same window/project without session_id must create a
    // distinct Workflow Session instead of continuing the first implicitly.
    let second = dispatch_coding_call_in_window(
        &runtime,
        "wop-create",
        work_on_project_call("demo", "second root instruction", None),
        Some(&auth),
        "wop-create-window",
    )
    .await;
    assert!(second.success, "{:?}", second.error);
    let second_session_id = second.output["session_id"].as_str().unwrap();
    assert_ne!(second_session_id, session_id);
    assert_eq!(second.output["continuation"], "created");
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        2
    );

    // Compact workspace/instruction projection reflects the underlying brief.
    assert!(result.output["workspace"]["branch"].is_string());
    assert!(result.output["instructions"]["status"].is_string());

    // The actual compact projection validates against its output schema.
    let schema = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let instance = json!({ "success": true, "output": result.output });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| panic!("compact output must match its schema: {error}"));

    // B: an ordinary project tool stays unrecorded when neither a business
    // Session nor an explicit recording_session_id is supplied, even in the
    // same stable window that created the Workflow Sessions above.
    let first_before = runtime
        .sessions
        .summary(&session_id, Some(200))
        .unwrap()
        .events
        .len();
    let second_before = runtime
        .sessions
        .summary(second_session_id, Some(200))
        .unwrap()
        .events
        .len();
    let ordinary_window = crate::client_window::ClientWindow::for_test(
        "ordinary_project_tool_without_explicit_session_is_unrecorded",
    );
    let ordinary = runtime
        .call_tool_with_context(
            ToolCallRequest {
                tool_name: "workspace_hygiene_check".to_string(),
                arguments: json!({"project": project}),
            },
            ToolCallContext {
                transport: ToolTransport::Api,
                session_id: None,
                auth: Some(&auth),
                window: Some(&ordinary_window),
                record_oauth_scope_denials: true,
                host_file_import_trust: HostFileImportTrust::Untrusted,
            },
        )
        .await;
    assert!(ordinary.success, "{:?}", ordinary.error_status);
    assert_eq!(
        runtime
            .sessions
            .summary(&session_id, Some(200))
            .unwrap()
            .events
            .len(),
        first_before
    );
    assert_eq!(
        runtime
            .sessions
            .summary(second_session_id, Some(200))
            .unwrap()
            .events
            .len(),
        second_before
    );
}

#[tokio::test]
async fn managed_worktree_bootstrap_recovers_same_operation_and_binds_session_to_final_project() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let managed = root.path().join("managed");
    let base_sha = seed_managed_tool_runtime_fixture(&source, &managed);
    let source_path = source.canonicalize().unwrap().to_string_lossy().to_string();
    let managed_path = managed
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let source_status_before = managed_fixture_git(&source, &["status", "--porcelain"]);
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "wop-managed";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            project_path_registration: true,
            managed_worktree: true,
            internal_posix_script: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let (first, payloads) = dispatch_with_managed_worktree_runner(
        &runtime,
        client_id,
        worktree_work_on_project_call(
            client_id,
            &source_path,
            "work in an isolated checkout",
            Some("HEAD"),
            None,
        ),
        &source_path,
        &managed_path,
        "managed-a1b2c3d4",
        "HEAD",
        &base_sha,
        true,
        "managed_worktree_recovered",
        false,
        true,
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(
        payloads.len(),
        2,
        "indeterminate response should re-observe once"
    );
    assert_eq!(payloads[0]["operation_id"], payloads[1]["operation_id"]);
    assert!(payloads[0]["resume_project_id"].is_null());
    assert_eq!(payloads[0]["base_ref"], "HEAD");
    let project = "agent:wop-managed:managed-a1b2c3d4";
    assert_eq!(first.output["resolved_project"], project);
    assert_eq!(first.output["project"], project);
    assert_eq!(first.output["worktree"]["managed"], true);
    assert_eq!(first.output["worktree"]["base_ref"], "HEAD");
    assert_eq!(first.output["worktree"]["base_sha"], base_sha);
    assert_eq!(first.output["worktree"]["source_dirty"], true);
    assert_eq!(
        first.output["project_resolution"]["source"],
        "managed_worktree"
    );
    assert_eq!(
        first.output["project_resolution"]["outcome"],
        "managed_worktree_recovered"
    );
    assert_eq!(
        first.output["project_resolution"]["registered"], true,
        "lost-response recovery must preserve call-level registration mutation"
    );
    assert!(first.output["project_resolution"].get("worktree").is_none());
    let compact = first.output.to_string();
    assert!(!compact.contains(&source_path));
    assert!(!compact.contains(&managed_path));
    let session_id = first.output["session_id"].as_str().unwrap().to_string();
    let session = runtime.sessions.summary(&session_id, Some(50)).unwrap();
    assert_eq!(session.project.as_deref(), Some(project));

    let listed = runtime.list_projects(Some(&auth_context(None, true))).await;
    assert!(listed.output["projects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == project));

    let read = ToolCall::from_tool_name(
        "read_file",
        json!({"project": project, "session_id": session_id, "path": "hello.txt"}),
    )
    .unwrap();
    let read =
        dispatch_startup_without_window(&runtime, client_id, read, Some(&auth_context(None, true)))
            .await;
    assert!(read.success, "{:?}", read.error);
    assert!(read.output["text"]
        .as_str()
        .is_some_and(|text| text.contains("committed")));
    assert_eq!(
        std::fs::read_to_string(source.join("hello.txt")).unwrap(),
        "dirty source only\n"
    );
    assert_eq!(
        managed_fixture_git(&source, &["status", "--porcelain"]),
        source_status_before
    );

    let (resumed, resume_payloads) = dispatch_with_managed_worktree_runner(
        &runtime,
        client_id,
        worktree_work_on_project_call(
            client_id,
            &source_path,
            "continue the exact isolated checkout",
            None,
            Some(&session_id),
        ),
        &source_path,
        &managed_path,
        "managed-a1b2c3d4",
        "HEAD",
        &base_sha,
        true,
        "managed_worktree_recovered",
        false,
        false,
    )
    .await;
    assert!(resumed.success, "{:?}", resumed.error);
    assert_eq!(resumed.output["session_id"], session_id);
    assert_eq!(resumed.output["continuation"], "resumed_explicitly");
    assert_eq!(resume_payloads.len(), 1);
    assert_eq!(resume_payloads[0]["resume_project_id"], "managed-a1b2c3d4");
    assert!(resume_payloads[0]["base_ref"].is_null());
    assert_eq!(
        runtime
            .sessions
            .summary(&session_id, Some(50))
            .unwrap()
            .project
            .as_deref(),
        Some(project)
    );

    let schema = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let instance = json!({"success": true, "output": first.output});
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| panic!("managed worktree output must match schema: {error}"));
}

#[tokio::test]
async fn managed_worktree_invalid_arguments_and_authority_fail_before_runner_mutation() {
    let runtime = ToolRuntime::new_for_tests();
    let root = tempfile::tempdir().unwrap();
    let source_path = root
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let invalid = runtime
        .dispatch(ToolCall::WorkOnProject {
            project: String::new(),
            client_id: Some("unresolved-runner".to_string()),
            path: Some(source_path.clone()),
            mode: Some("checkout".to_string()),
            base_ref: Some("main".to_string()),
            instruction: "must fail before resolution".to_string(),
            include_project_instructions: true,
            include_workflow_guidance: true,
            session_id: None,
        })
        .await;
    assert!(!invalid.success);
    assert_eq!(invalid.output["error_kind"], "invalid_arguments");
    assert_eq!(invalid.output["field"], "base_ref");
    assert_eq!(invalid.output["state_changed"], false);

    let client_id = "wop-managed-no-cap";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            project_path_registration: true,
            managed_worktree: false,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;
    let unavailable = runtime
        .dispatch_with_auth(
            worktree_work_on_project_call(client_id, &source_path, "no mutation", None, None),
            Some(&auth_context(None, true)),
        )
        .await;
    assert!(!unavailable.success);
    assert_eq!(
        unavailable.output["error_kind"],
        "agent_capability_unavailable"
    );
    assert_eq!(unavailable.output["state_changed"], false);
    assert!(probe_patch_agent_request(&runtime, client_id)
        .await
        .is_none());

    let restricted = ToolRuntime::new_for_tests()
        .with_permission_evaluator(PermissionEvaluator::with_mode(AuthorityMode::Restricted));
    let restricted_client = "wop-managed-restricted";
    register_agent_with_projects(
        &restricted,
        restricted_client,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            managed_worktree: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;
    let denied = restricted
        .dispatch_with_auth(
            worktree_work_on_project_call(
                restricted_client,
                &source_path,
                "permission must precede prepare",
                None,
                None,
            ),
            Some(&auth_context(None, true)),
        )
        .await;
    assert!(!denied.success);
    assert_eq!(denied.output["error_kind"], "permission_denied");
    assert!(probe_patch_agent_request(&restricted, restricted_client)
        .await
        .is_none());
}

#[tokio::test]
async fn source_project_session_cannot_resume_a_managed_worktree() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    init_git_repo(&source);
    let source_path = source.canonicalize().unwrap().to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "wop-managed-source-session";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            managed_worktree: true,
            ..Default::default()
        },
        vec![registered_project("source", &source_path)],
    )
    .await;
    let source_project = format!("agent:{client_id}:source");
    let auth = auth_context(None, true);
    let started = dispatch_coding_call_in_window(
        &runtime,
        client_id,
        work_on_project_call(&source_project, "source task", None),
        Some(&auth),
        "source-session-window",
    )
    .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let source_path = source_path.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    worktree_work_on_project_call(
                        client_id,
                        &source_path,
                        "must not switch workspace",
                        None,
                        Some(&session_id),
                    ),
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, client_id).await;
    assert_eq!(request.kind, "prepare_managed_worktree");
    let payload: Value = serde_json::from_str(request.stdin.as_deref().unwrap()).unwrap();
    assert_eq!(payload["resume_project_id"], "source");
    let error = json!({
        "error_code": "managed_worktree_resume_mismatch",
        "error_kind": "managed_worktree_resume_mismatch",
        "failure_kind": "managed_worktree_resume_mismatch",
        "state_changed": false,
    });
    complete_patch_agent_request(
        &runtime,
        client_id,
        &request.request_id,
        1,
        &error.to_string(),
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(
        result.output["error_kind"],
        "managed_worktree_resume_mismatch"
    );
    assert_eq!(result.output["state_changed"], false);
    assert_eq!(instruction_events(&runtime, &session_id).len(), 1);
    assert_eq!(runtime.list_projects(Some(&auth)).await.output["count"], 1);
}

#[tokio::test]
async fn path_source_auto_registers_reuses_and_supports_canonical_coding_entry() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    std::fs::write(root.path().join("hello.txt"), "hello\n").unwrap();
    let project_path = root.path().canonicalize().unwrap();
    let project_path = project_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "wop-path";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            project_path_registration: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let first = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(client_id, &project_path, "first path instruction", None),
        "repo-a1b2c3d4",
        &project_path,
        "auto_registered",
        true,
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.output["project_resolution"]["source"], "path");
    assert_eq!(
        first.output["project_resolution"]["outcome"],
        "auto_registered"
    );
    assert_eq!(first.output["project_resolution"]["registered"], true);
    assert_eq!(first.output["permission"]["status"], "auto_approved");
    assert_eq!(first.output["permission"]["tool_name"], "register_project");
    assert_eq!(
        first.output["resolved_project"],
        "agent:wop-path:repo-a1b2c3d4"
    );
    assert!(
        !first.output.to_string().contains(&project_path),
        "compact work_on_project output leaked the absolute input path"
    );
    let session_id = first.output["session_id"].as_str().unwrap().to_string();

    let second = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(
            client_id,
            &project_path,
            "second path instruction",
            Some(&session_id),
        ),
        "repo-a1b2c3d4",
        &project_path,
        "reused_existing_registration",
        false,
    )
    .await;
    assert!(second.success, "{:?}", second.error);
    assert_eq!(second.output["session_id"], session_id);
    assert_eq!(second.output["continuation"], "resumed_explicitly");
    assert_eq!(second.output["permission"]["status"], "auto_approved");
    assert_eq!(second.output["permission"]["tool_name"], "register_project");
    assert_eq!(
        second.output["project_resolution"]["outcome"],
        "reused_existing_registration"
    );
    assert_eq!(instruction_events(&runtime, &session_id).len(), 2);

    let listed = runtime.list_projects(Some(&auth_context(None, true))).await;
    assert!(listed.success);
    assert!(listed.output["projects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|project| project["id"] == "agent:wop-path:repo-a1b2c3d4"
            && project["source"] == "auto_registered"));

    let read = ToolCall::from_tool_name(
        "read_file",
        json!({
            "project": "agent:wop-path:repo-a1b2c3d4",
            "session_id": session_id,
            "path": "hello.txt"
        }),
    )
    .unwrap();
    let read = dispatch_with_path_runner(
        &runtime,
        client_id,
        read,
        "repo-a1b2c3d4",
        &project_path,
        "reused_existing_registration",
        false,
    )
    .await;
    assert!(read.success, "{:?}", read.error);
    assert!(read.output["text"]
        .as_str()
        .is_some_and(|content| content.contains("hello")));
}

#[tokio::test]
async fn path_source_explicit_session_mismatch_fails_before_registration() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    init_git_repo(first_root.path());
    init_git_repo(second_root.path());
    let first_path = first_root.path().canonicalize().unwrap();
    let second_path = second_root.path().canonicalize().unwrap();
    let first_path = first_path.to_string_lossy().to_string();
    let second_path = second_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "wop-path-mismatch";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            project_path_registration: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let first = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(client_id, &first_path, "first project", None),
        "first-a1b2c3d4",
        &first_path,
        "auto_registered",
        true,
    )
    .await;
    assert!(first.success);
    let session_id = first.output["session_id"].as_str().unwrap();
    let mismatch = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(
            client_id,
            &second_path,
            "must not fall back",
            Some(session_id),
        ),
        "second-a1b2c3d4",
        &second_path,
        "auto_registered",
        true,
    )
    .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["error_kind"], "session_project_mismatch");
    assert_eq!(mismatch.output["state_changed"], false);
    assert!(mismatch.output.get("permission").is_none());
    assert!(mismatch.output.get("project_resolution").is_none());
    assert_eq!(
        mismatch.output["request_project"],
        format!("path:{client_id}:{second_path}")
    );
    assert_eq!(instruction_events(&runtime, session_id).len(), 1);

    let listed = runtime.list_projects(Some(&auth_context(None, true))).await;
    assert_eq!(listed.output["count"], 1);
}

#[tokio::test]
async fn path_source_unknown_session_fails_before_registration() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    let project_path = root.path().canonicalize().unwrap();
    let project_path = project_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "wop-path-unknown";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            project_path_registration: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let result = dispatch_with_path_runner(
        &runtime,
        client_id,
        path_work_on_project_call(
            client_id,
            &project_path,
            "unknown must not fall back",
            Some("wc_sess_unknown"),
        ),
        "unknown-a1b2c3d4",
        &project_path,
        "auto_registered",
        true,
    )
    .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_session_id");
    assert!(result.output.get("permission").is_none());
    assert!(result.output.get("project_resolution").is_none());
    let listed = runtime.list_projects(Some(&auth_context(None, true))).await;
    assert_eq!(listed.output["count"], 0);
}

#[tokio::test]
async fn path_source_cross_project_recording_session_fails_before_registration() {
    let recorder_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    init_git_repo(recorder_root.path());
    init_git_repo(target_root.path());
    let target_path = target_root.path().canonicalize().unwrap();
    let target_path = target_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let recorder_project = register_runner_project_at_path(
        &runtime,
        "wop-recorder-owner",
        "recorder",
        recorder_root.path(),
    )
    .await;
    let target_client = "wop-recorder-target";
    register_agent_with_projects(
        &runtime,
        target_client,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            project_path_registration: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;
    let auth = auth_context(None, true);
    let recorder = runtime.sessions.start_session(
        Some(recorder_project.clone()),
        Some("path recorder boundary".to_string()),
    );
    let recorder_session_id = recorder.session_id.clone();

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let target_path = target_path.clone();
        let recorder_session_id = recorder_session_id.clone();
        async move {
            runtime
                .call_tool_with_context(
                    crate::tool_runtime::kernel::ToolCallRequest {
                        tool_name: "work_on_project".to_string(),
                        arguments: json!({
                            "client_id": target_client,
                            "path": target_path,
                            "instruction": "bootstrap a different project through its path"
                        }),
                    },
                    crate::tool_runtime::kernel::ToolCallContext {
                        transport: crate::tool_runtime::kernel::ToolTransport::Api,
                        session_id: Some(&recorder_session_id),
                        auth: Some(&auth),
                        window: None,
                        record_oauth_scope_denials: true,
                        host_file_import_trust:
                            crate::tool_runtime::kernel::HostFileImportTrust::Untrusted,
                    },
                )
                .await
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if task.is_finished() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "kernel path bootstrap did not finish within 10 seconds for client {target_client}"
        );
        if let Some(request) = probe_patch_agent_request(&runtime, target_client).await {
            if request.kind == "resolve_or_register_project" {
                let payload: Value =
                    serde_json::from_str(request.stdin.as_deref().unwrap()).unwrap();
                assert_eq!(payload["path"], target_path);
                let response = json!({
                    "id": "agent:wop-recorder-target:target-a1b2c3d4",
                    "agent_project_id": "target-a1b2c3d4",
                    "client_id": target_client,
                    "name": "target-a1b2c3d4",
                    "path": target_path,
                    "kind": "auto_registered",
                    "description": null,
                    "allow_patch": true,
                    "disabled": false,
                    "revision": format!("sha256:{}", "a".repeat(64)),
                    "source": "path",
                    "outcome": "auto_registered",
                    "registered": true,
                    "created_config": true,
                    "changed": true,
                    "recovered": false,
                });
                complete_patch_agent_request(
                    &runtime,
                    target_client,
                    &request.request_id,
                    0,
                    &response.to_string(),
                    "",
                )
                .await;
            } else {
                complete_agent_request_by_running_locally(&runtime, target_client, request).await;
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    let outcome = task.await.unwrap();
    assert!(!outcome.success);
    let result = outcome.result.expect("work_on_project mismatch result");
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "session_project_mismatch");
    assert_eq!(result.output["failure_kind"], "session_project_mismatch");
    assert_eq!(result.output["state_changed"], false);
    assert!(result.output.get("permission").is_none());
    assert_eq!(result.output["session_project"], recorder_project);
    assert_eq!(
        result.output["request_project"],
        format!("path:{target_client}:{target_path}")
    );
    let listed = runtime.list_projects(Some(&auth)).await;
    assert_eq!(listed.output["count"], 1);
    let summary = runtime
        .sessions
        .summary(&recorder_session_id, Some(50))
        .expect("recording session summary");
    let event = summary
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "work_on_project")
        .expect("recorded work_on_project event");
    assert_eq!(
        event.failure_kind.as_deref(),
        Some("session_project_mismatch")
    );
    assert_eq!(
        event.error_kind.as_deref(),
        Some("session_project_mismatch")
    );
    assert!(event.warning_kind.is_none());
    assert_eq!(
        event.session_project.as_deref(),
        Some(recorder_project.as_str())
    );
    assert_eq!(
        event.request_project.as_deref(),
        Some(format!("path:{target_client}:{target_path}").as_str())
    );
    assert!(event.permission.is_none());
}

#[tokio::test]
async fn path_source_requires_project_write_scope_before_runner_enqueue() {
    let root = tempfile::tempdir().unwrap();
    let project_path = root.path().canonicalize().unwrap();
    let project_path = project_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests();
    let auth = managed_oauth_auth_context("path-read-only", Some("path-read-only-hash"));
    register_agent_projects_for_auth(
        &runtime,
        "oauth-client",
        &auth,
        RunnerCapabilities {
            shell: true,
            git: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let result = runtime
        .dispatch_with_auth(
            path_work_on_project_call("oauth-client", &project_path, "must not register", None),
            Some(&auth),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "insufficient_scope");
    assert_eq!(
        result.output["required_scope"],
        crate::auth::SCOPE_PROJECT_WRITE
    );
    assert_eq!(result.output["state_changed"], false);
}

#[tokio::test]
async fn path_source_respects_restricted_authority_before_runner_enqueue() {
    let root = tempfile::tempdir().unwrap();
    let project_path = root.path().canonicalize().unwrap();
    let project_path = project_path.to_string_lossy().to_string();
    let runtime = ToolRuntime::new_for_tests()
        .with_permission_evaluator(PermissionEvaluator::with_mode(AuthorityMode::Restricted));
    let client_id = "wop-path-restricted";
    register_agent_with_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            ..Default::default()
        },
        Vec::new(),
    )
    .await;

    let result = runtime
        .dispatch_with_auth(
            path_work_on_project_call(client_id, &project_path, "must not register", None),
            Some(&auth_context(None, true)),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "permission_denied");
    assert_eq!(result.output["permission"]["status"], "denied");
    assert_eq!(result.output["permission"]["tool_name"], "register_project");
    assert!(probe_patch_agent_request(&runtime, client_id)
        .await
        .is_none());
}

#[tokio::test]
async fn work_on_project_continues_exact_session_and_appends_instruction() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "wop-continue", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let first = dispatch_coding_call_in_window(
        &runtime,
        "wop-continue",
        work_on_project_call(&project, "root objective", None),
        Some(&auth),
        "wop-continue-window",
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let session_id = first.output["session_id"].as_str().unwrap().to_string();
    let before = instruction_events(&runtime, &session_id);
    assert_eq!(before.len(), 1);

    let continued = dispatch_coding_call_in_window(
        &runtime,
        "wop-continue",
        work_on_project_call(&project, "follow-up instruction", Some(&session_id)),
        Some(&auth),
        "wop-continue-window",
    )
    .await;
    assert!(continued.success, "{:?}", continued.error);
    assert_eq!(continued.output["session_id"], session_id);
    assert_eq!(continued.output["continuation"], "resumed_explicitly");
    assert_eq!(first.output["workflow"], continued.output["workflow"]);
    assert_eq!(
        continued.output["workflow"],
        crate::tool_runtime::startup_brief::builtin_coding_workflow_projection()
    );

    // Explicit resume reuses exactly one Session and appends one instruction.
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        1
    );

    // Follow-up instruction appended; root title preserved.
    let events = instruction_events(&runtime, &session_id);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].instruction.as_deref(), Some("root objective"));
    assert_eq!(
        events[1].instruction.as_deref(),
        Some("follow-up instruction")
    );
    let summary = runtime.sessions.summary(&session_id, Some(50)).unwrap();
    assert_eq!(summary.title.as_deref(), Some("root objective"));
    assert_eq!(summary.mode, SessionMode::Normal);
    assert!(!summary.guards.deny_write_tools);
    assert!(!summary.guards.deny_shell_tools);
    assert!(
        !serde_json::to_string(&summary)
            .unwrap()
            .contains("webcodex.coding_workflow"),
        "workflow projection must not become Session state"
    );
}

#[tokio::test]
async fn work_on_project_failures_never_create_or_fall_back() {
    let dir = tempfile::tempdir().unwrap();
    let root_a = dir.path().join("a");
    let root_b = dir.path().join("b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    init_git_repo(&root_a);
    init_git_repo(&root_b);
    let runtime = ToolRuntime::new_for_tests();
    register_agent_with_projects(
        &runtime,
        "wop-fail",
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            ..Default::default()
        },
        vec![
            registered_project("a", &root_a.to_string_lossy()),
            registered_project("b", &root_b.to_string_lossy()),
        ],
    )
    .await;
    let project_a = crate::tool_runtime::runner_project_runtime_id("wop-fail", "a");
    let project_b = crate::tool_runtime::runner_project_runtime_id("wop-fail", "b");
    let auth = auth_context(None, true);

    // Create a stable active session on project A, plus a closed one.
    let first = dispatch_coding_call_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_a, "stable session", None),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(first.success);
    let active_id = first.output["session_id"].as_str().unwrap().to_string();
    let closed_id = runtime
        .sessions
        .start_session_with_guards(
            Some(project_a.clone()),
            Some("closed project A".to_string()),
            SessionMode::Normal,
            SessionGuards::default(),
        )
        .session_id;
    runtime.sessions.close_session(&closed_id).unwrap();

    // Unknown Session: no creation, structured unknown_session_id failure.
    let unknown = dispatch_coding_call_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_a, "must not create", Some("wc_sess_missing")),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(!unknown.success);
    assert_eq!(unknown.output["error_kind"], "unknown_session_id");

    // Closed Session: no creation, structured session_closed failure.
    let closed = dispatch_coding_call_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_a, "must not reopen", Some(&closed_id)),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(!closed.success);
    assert_eq!(closed.output["error_kind"], "session_closed");
    assert_eq!(closed.output["lifecycle"], "closed");

    // Project mismatch: no fallback to any other session.
    let mismatch = dispatch_coding_call_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_b, "must not cross", Some(&active_id)),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["error_kind"], "session_project_mismatch");
    assert_eq!(mismatch.output["session_project"], project_a);
    assert_eq!(mismatch.output["request_project"], project_b);

    // Invalid Session id fails before execution (no session created).
    let invalid = dispatch_coding_call_in_window(
        &runtime,
        "wop-fail",
        work_on_project_call(&project_a, "must not run", Some("not-a-session")),
        Some(&auth),
        "wop-fail-window",
    )
    .await;
    assert!(!invalid.success);
    assert_eq!(invalid.output["error_kind"], "invalid_session_id");

    // Nothing new was created and the active session is unchanged.
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project_a)),
        1
    );
    let events = instruction_events(&runtime, &active_id);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].instruction.as_deref(), Some("stable session"));
}

#[test]
fn finish_coding_task_remains_optional_and_advisory() {
    let specs = registered_tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert!(names.contains(&"finish_coding_task"), "still public");

    let finish = spec_named(&specs, "finish_coding_task");
    let description = finish.description.to_lowercase();
    for phrase in [
        "optional",
        "advisory",
        "does not decide task completion",
        "generate the user-facing final report",
    ] {
        assert!(
            description.contains(phrase),
            "finish_coding_task description must include {phrase}: {description}"
        );
    }
    assert!(
        finish.description.contains("does not"),
        "finish_coding_task description must be explicit about non-authority"
    );

    // The default coding manifest intent does not mark finish as the required
    // final step: it is the last optional evidence snapshot in the list.
    let coding = crate::tool_runtime::tool_definition::TOOL_MANIFEST_INTENTS
        .iter()
        .find(|intent| intent.name == "coding")
        .expect("coding intent");
    assert!(coding.tools.contains(&"work_on_project"));
    assert!(coding.tools.contains(&"finish_coding_task"));
    assert!(
        coding
            .tools
            .iter()
            .position(|t| *t == "finish_coding_task")
            .unwrap()
            > coding
                .tools
                .iter()
                .position(|t| *t == "work_on_project")
                .unwrap()
    );
}

/// Seed a representative Rust-style repository for the startup overview. The
/// files are committed so the tracked git index (the overview's project
/// boundary) includes every fixture entry; sensitive/build paths stay
/// excluded by the overview's own path policy.
fn seed_coding_repository(root: &std::path::Path, agents_body: &str) {
    init_git_repo(root);
    std::fs::write(
        root.join("AGENTS.md"),
        format!("# Repository rules\n\n{agents_body}\n"),
    )
    .unwrap();
    for path in [
        "README.md",
        "Cargo.toml",
        "src/lib.rs",
        "tests/basic.rs",
        "docs/index.md",
        "scripts/check.sh",
        ".github/workflows/ci.yml",
        "src/generated/deep/path.rs",
    ] {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"fixture contents must never be read").unwrap();
    }
    // Untracked/build/sensitive paths must never appear in the overview.
    std::fs::write(root.join(".env"), b"SECRET=do-not-leak").unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join("target/debug/output"), b"binary").unwrap();
    for cmd in [
        "git add -A",
        "git commit -m 'seed fixture'",
        "git config status.showUntrackedFiles all",
    ] {
        let (exit_code, stdout, stderr, _) =
            crate::tool_runtime::helpers::run_command_sync(cmd, root, 30);
        assert_eq!(exit_code, 0, "{cmd}\n{stdout}{stderr}");
    }
}

/// Overwrite `AGENTS.md` in place (still tracked) so a follow-up resume sees a
/// changed fingerprint without a commit.
fn overwrite_agents_rule(root: &std::path::Path, body: &str) {
    std::fs::write(
        root.join("AGENTS.md"),
        format!("# Repository rules\n\n{body}\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn work_on_project_new_task_is_lightweight_and_preserves_startup_context() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "Preserve unrelated changes.");
    let runtime = ToolRuntime::new_for_tests();
    register_agent_with_projects(
        &runtime,
        "wop-repo",
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            lsp_read_only_navigation: true,
            internal_posix_script: true,
            ..Default::default()
        },
        vec![registered_project("demo", &root.path().to_string_lossy())],
    )
    .await;
    let project = crate::tool_runtime::runner_project_runtime_id("wop-repo", "demo");
    let auth = auth_context(None, true);

    let (result, request_kinds) = dispatch_recording_startup_requests(
        &runtime,
        "wop-repo",
        work_on_project_call(&project, "start on the repository", None),
        Some(&auth),
        "wop-repo-window",
    )
    .await;
    assert!(result.success, "{:?}", result.error);

    // resolved_project is the full runtime project id.
    assert_eq!(result.output["resolved_project"], project);
    // This fixture still has a real semantic-navigation warning, so readiness
    // remains warn while the intentionally skipped repository overview is omitted.
    assert!(result.output.get("repository").is_none());
    assert_eq!(result.output["readiness"]["status"], "warn");
    let warnings = result.output["warnings"].as_array().unwrap();
    assert!(warnings
        .iter()
        .any(|warning| warning == "semantic_navigation_unavailable"));

    // Runner request evidence: rules, Git, and LSP probes remain; repository
    // overview is not merely hidden from JSON, it is never enqueued.
    assert!(
        request_kinds.iter().any(|kind| kind == "file_read"),
        "repository rules were not observed: {request_kinds:?}"
    );
    assert!(
        request_kinds
            .iter()
            .any(|kind| kind == "run_internal_posix_script"),
        "Git/workspace inspection was not executed through the internal POSIX runtime: {request_kinds:?}"
    );
    assert!(
        request_kinds
            .iter()
            .any(|kind| kind == AGENT_LSP_REQUEST_KIND),
        "semantic navigation was not probed: {request_kinds:?}"
    );
    assert!(
        request_kinds
            .iter()
            .all(|kind| kind != "file_project_overview"),
        "work_on_project unexpectedly enqueued an overview: {request_kinds:?}"
    );

    // Instructions loaded with bounded body and headings.
    let instructions = &result.output["instructions"];
    assert_eq!(instructions["status"], "loaded");
    assert_eq!(instructions["content_included"], true);
    assert!(instructions["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["path"] == "AGENTS.md"
            && source["content"]
                .as_str()
                .is_some_and(|content| content.contains("Preserve unrelated changes"))
            && source["headings"]
                .as_array()
                .is_some_and(|headings| !headings.is_empty())));

    // Semantic navigation block exists and is deterministic.
    assert!(result.output["semantic_navigation"].is_object());
    assert!(result.output["semantic_navigation"]["status"].is_string());

    // No noteworthy Job state means no jobs block at all.
    assert!(result.output.get("jobs").is_none());

    // No full diagnostics leak.
    for hidden in [
        "runtime_status",
        "connection_state",
        "authority",
        "tool_manifest",
        "recommended_flow",
        "startup_verdict",
        "git",
        "continuation_feedback",
    ] {
        assert!(
            !result.output.as_object().unwrap().contains_key(hidden),
            "compact output must not include {hidden}"
        );
    }

    // Exactly one fresh Session exists.
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        1
    );

    // Schema validates.
    let schema = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let instance = json!({ "success": true, "output": result.output });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| panic!("compact output must match its schema: {error}"));
    let bytes = serde_json::to_vec(&result.output).unwrap().len();
    assert!(bytes <= crate::tool_runtime::startup_brief::STANDARD_STARTUP_HARD_MAX_BYTES);
    assert!(
        !result
            .output
            .to_string()
            .contains(&root.path().to_string_lossy().to_string()),
        "compact output leaked the absolute repository path"
    );
}

#[tokio::test]
async fn work_on_project_can_omit_instruction_bodies_for_a_fresh_session() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "caller already knows this rule");
    let runtime = ToolRuntime::new_for_tests();
    let project = register_runner_project_at_path(
        &runtime,
        "wop-instruction-projection",
        "demo",
        root.path(),
    )
    .await;
    let auth = auth_context(None, true);

    let first = dispatch_coding_call_in_window(
        &runtime,
        "wop-instruction-projection",
        work_on_project_call(&project, "first task", None),
        Some(&auth),
        "wop-instruction-projection-window",
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.output["instructions"]["content_included"], true);
    let first_session_id = first.output["session_id"].as_str().unwrap().to_string();

    let (second, request_kinds) = dispatch_recording_startup_requests(
        &runtime,
        "wop-instruction-projection",
        work_on_project_call_with_instruction_projection(
            &project,
            "second independent task",
            None,
            false,
        ),
        Some(&auth),
        "wop-instruction-projection-window",
    )
    .await;
    assert!(second.success, "{:?}", second.error);
    let second_session_id = second.output["session_id"].as_str().unwrap().to_string();
    assert_ne!(second_session_id, first_session_id);
    assert_eq!(second.output["continuation"], "created");

    let instructions = &second.output["instructions"];
    assert_eq!(instructions["status"], "loaded");
    assert!(instructions.get("content_included").is_none());
    let agents_source = instructions["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "AGENTS.md")
        .expect("AGENTS.md metadata");
    assert!(agents_source.get("content").is_none());
    assert!(agents_source.get("headings").is_none());
    assert!(agents_source.get("truncated").is_none());
    assert!(agents_source["fingerprint"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
    assert!(
        request_kinds.iter().any(|kind| kind == "file_read"),
        "instruction files must still be observed when their bodies are omitted: {request_kinds:?}"
    );
    let schema = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let instance = json!({ "success": true, "output": second.output.clone() });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| {
            panic!("sparse instruction metadata must match output schema: {error}")
        });

    let summary = runtime
        .sessions
        .summary(&second_session_id, Some(20))
        .unwrap();
    let snapshot = summary
        .project_instructions
        .expect("fresh Workflow Session instruction summary");
    assert!(snapshot.loaded);
    let stored_agents = snapshot
        .files
        .iter()
        .find(|file| file.path == "AGENTS.md")
        .expect("stored AGENTS.md summary");
    assert_eq!(
        Some(stored_agents.fingerprint.as_str()),
        agents_source["fingerprint"].as_str()
    );
}

#[tokio::test]
async fn work_on_project_can_omit_static_workflow_guidance() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "keep repository guidance visible");
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "wop-workflow-projection", "demo", root.path())
            .await;
    let auth = auth_context(None, true);

    let result = dispatch_coding_call_in_window(
        &runtime,
        "wop-workflow-projection",
        work_on_project_call_with_projections(
            &project,
            "caller already knows the static workflow",
            None,
            true,
            false,
        ),
        Some(&auth),
        "wop-workflow-projection-window",
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("workflow").is_none());
    assert_eq!(result.output["instructions"]["content_included"], true);
    assert!(result.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["content"]
            .as_str()
            .is_some_and(|content| content.contains("keep repository guidance visible"))));

    let session_id = result.output["session_id"].as_str().unwrap();
    let summary = runtime.sessions.summary(session_id, Some(20)).unwrap();
    assert_eq!(summary.project.as_deref(), Some(project.as_str()));

    let schema = crate::tool_runtime::registry::output_schema_for_tool("work_on_project");
    let instance = json!({ "success": true, "output": result.output });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
        .unwrap_or_else(|error| panic!("workflow-omitted output must match schema: {error}"));
}

#[tokio::test]
async fn work_on_project_static_projection_is_caller_explicit_not_window_state() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "static caller-explicit rule");
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "wop-explicit", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let first = dispatch_coding_call_in_window(
        &runtime,
        "wop-explicit",
        work_on_project_call(&project, "first", None),
        Some(&auth),
        "same-window",
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let session_id = first.output["session_id"].as_str().unwrap().to_string();
    assert!(first.output["workflow"].is_object());

    let repeated = dispatch_coding_call_in_window(
        &runtime,
        "wop-explicit",
        work_on_project_call(&project, "repeat true", Some(&session_id)),
        Some(&auth),
        "same-window",
    )
    .await;
    assert!(repeated.success, "{:?}", repeated.error);
    assert!(repeated.output["workflow"].is_object());
    assert_eq!(repeated.output["instructions"]["status"], "reused");
    assert_eq!(repeated.output["instructions"]["content_included"], true);

    let suppressed = dispatch_coding_call_in_window(
        &runtime,
        "wop-explicit",
        work_on_project_call_with_projections(
            &project,
            "caller suppresses static content",
            Some(&session_id),
            false,
            false,
        ),
        Some(&auth),
        "same-window",
    )
    .await;
    assert!(suppressed.success, "{:?}", suppressed.error);
    assert!(suppressed.output.get("workflow").is_none());
    assert_eq!(suppressed.output["instructions"]["status"], "reused");
    assert!(suppressed.output["instructions"]
        .get("content_included")
        .is_none());
    let suppressed_agents = suppressed.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "AGENTS.md")
        .unwrap();
    assert!(suppressed_agents["fingerprint"].is_string());
    assert!(suppressed_agents.get("content").is_none());
    assert!(suppressed_agents.get("headings").is_none());
    assert!(suppressed_agents.get("read_more").is_none());

    let restored_other_window = dispatch_coding_call_in_window(
        &runtime,
        "wop-explicit",
        work_on_project_call(&project, "true in another window", Some(&session_id)),
        Some(&auth),
        "different-window",
    )
    .await;
    assert!(
        restored_other_window.success,
        "{:?}",
        restored_other_window.error
    );
    assert!(restored_other_window.output["workflow"].is_object());
    assert_eq!(
        restored_other_window.output["instructions"]["content_included"],
        true
    );

    let no_window = dispatch_startup_without_window(
        &runtime,
        "wop-explicit",
        work_on_project_call(&project, "true without window", Some(&session_id)),
        Some(&auth),
    )
    .await;
    assert!(no_window.success, "{:?}", no_window.error);
    assert!(no_window.output["workflow"].is_object());
    assert_eq!(no_window.output["instructions"]["content_included"], true);
}

#[tokio::test]
async fn work_on_project_suppressed_instruction_bodies_still_track_changed_rules() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "old body");
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "wop-suppressed-change", "demo", root.path())
            .await;
    let auth = auth_context(None, true);

    let first = dispatch_coding_call_in_window(
        &runtime,
        "wop-suppressed-change",
        work_on_project_call(&project, "first", None),
        Some(&auth),
        "window-a",
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let session_id = first.output["session_id"].as_str().unwrap().to_string();
    let old_fingerprint = first.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "AGENTS.md")
        .unwrap()["fingerprint"]
        .as_str()
        .unwrap()
        .to_string();

    let long_changed_body = std::iter::once("new body while projection is suppressed".to_string())
        .chain((0..500).map(|index| format!("suppressed-line-{index}")))
        .collect::<Vec<_>>()
        .join("\n");
    overwrite_agents_rule(root.path(), &long_changed_body);
    let changed = dispatch_coding_call_in_window(
        &runtime,
        "wop-suppressed-change",
        work_on_project_call_with_instruction_projection(
            &project,
            "observe change without body",
            Some(&session_id),
            false,
        ),
        Some(&auth),
        "window-a",
    )
    .await;
    assert!(changed.success, "{:?}", changed.error);
    assert_eq!(changed.output["instructions"]["status"], "changed");
    assert!(changed.output["instructions"]["changed_sources"]
        .as_array()
        .unwrap()
        .contains(&json!("AGENTS.md")));
    let changed_agents = changed.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "AGENTS.md")
        .unwrap();
    assert_ne!(changed_agents["fingerprint"], old_fingerprint);
    assert!(changed_agents.get("content").is_none());
    assert!(changed_agents.get("headings").is_none());
    assert!(changed_agents.get("read_more").is_none());

    let projected = dispatch_coding_call_in_window(
        &runtime,
        "wop-suppressed-change",
        work_on_project_call(&project, "project current body", Some(&session_id)),
        Some(&auth),
        "window-b",
    )
    .await;
    assert!(projected.success, "{:?}", projected.error);
    assert_eq!(projected.output["instructions"]["status"], "reused");
    assert_eq!(projected.output["instructions"]["content_included"], true);
    assert!(projected.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["path"] == "AGENTS.md"
            && source["content"].as_str().is_some_and(
                |content| content.contains("new body while projection is suppressed")
            )));
}

#[tokio::test]
async fn work_on_project_exact_resume_reuses_rules_and_detects_changes() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "first rule body");
    let runtime = ToolRuntime::new_for_tests();
    let project = register_runner_project_at_path(&runtime, "wop-reuse", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let first = dispatch_coding_call_in_window(
        &runtime,
        "wop-reuse",
        work_on_project_call(&project, "root objective", None),
        Some(&auth),
        "wop-reuse-window",
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let session_id = first.output["session_id"].as_str().unwrap().to_string();

    // Exact resume with unchanged rules: repository delta status remains reused,
    // while the caller-explicit default still projects the current bounded body.
    let reused = dispatch_coding_call_in_window(
        &runtime,
        "wop-reuse",
        work_on_project_call(&project, "follow-up", Some(&session_id)),
        Some(&auth),
        "wop-reuse-window",
    )
    .await;
    assert!(reused.success, "{:?}", reused.error);
    assert_eq!(reused.output["session_id"], session_id);
    assert_eq!(reused.output["continuation"], "resumed_explicitly");
    let reused_instructions = &reused.output["instructions"];
    assert_eq!(reused_instructions["status"], "reused");
    assert_eq!(reused_instructions["content_included"], true);
    assert!(reused_instructions.get("changed_sources").is_none());
    let reused_agents = reused_instructions["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "AGENTS.md")
        .expect("reused AGENTS.md source");
    assert!(reused_agents["content"]
        .as_str()
        .is_some_and(|content| content.contains("first rule body")));
    assert!(reused_agents["headings"].is_array());
    assert!(reused_agents["fingerprint"].is_string());

    // Change the rule then resume: status=changed, changed_sources includes it.
    overwrite_agents_rule(root.path(), "changed rule body");
    let changed = dispatch_coding_call_in_window(
        &runtime,
        "wop-reuse",
        work_on_project_call(&project, "after rule change", Some(&session_id)),
        Some(&auth),
        "wop-reuse-window",
    )
    .await;
    assert!(changed.success, "{:?}", changed.error);
    assert_eq!(changed.output["session_id"], session_id);
    assert_eq!(changed.output["instructions"]["status"], "changed");
    assert!(
        changed.output["instructions"]["changed_sources"]
            .as_array()
            .unwrap()
            .contains(&json!("AGENTS.md")),
        "{:?}",
        changed.output["instructions"]["changed_sources"]
    );
    assert!(changed.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["path"] == "AGENTS.md"
            && source["content"]
                .as_str()
                .is_some_and(|content| content.contains("changed rule body"))));
}

#[tokio::test]
async fn work_on_project_sizes_and_runner_request_reduction_are_stable() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "Keep the focused startup safe.");
    let runtime = ToolRuntime::new_for_tests();
    register_agent_with_projects(
        &runtime,
        "wop-size",
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            lsp_read_only_navigation: true,
            ..Default::default()
        },
        vec![registered_project("demo", &root.path().to_string_lossy())],
    )
    .await;
    let project = crate::tool_runtime::runner_project_runtime_id("wop-size", "demo");
    let auth = auth_context(None, true);

    let (fresh, fresh_requests) = dispatch_recording_startup_requests(
        &runtime,
        "wop-size",
        work_on_project_call(&project, "fresh lightweight startup", None),
        Some(&auth),
        "wop-size-fresh",
    )
    .await;
    assert!(fresh.success, "{:?}", fresh.error);
    let session_id = fresh.output["session_id"].as_str().unwrap().to_string();

    let (reused, reused_requests) = dispatch_recording_startup_requests(
        &runtime,
        "wop-size",
        work_on_project_call(&project, "unchanged continuation", Some(&session_id)),
        Some(&auth),
        "wop-size-reused",
    )
    .await;
    assert!(reused.success, "{:?}", reused.error);
    assert_eq!(reused.output["instructions"]["status"], "reused");
    assert_eq!(reused.output["instructions"]["content_included"], true);

    let (workflow_omitted, workflow_omitted_requests) = dispatch_recording_startup_requests(
        &runtime,
        "wop-size",
        work_on_project_call_with_projections(
            &project,
            "fresh startup without repeated workflow",
            None,
            true,
            false,
        ),
        Some(&auth),
        "wop-size-workflow-omitted",
    )
    .await;
    assert!(workflow_omitted.success, "{:?}", workflow_omitted.error);
    assert!(workflow_omitted.output.get("workflow").is_none());

    let (standard, standard_requests) = dispatch_recording_coding_workflow_diagnostic(
        &runtime,
        "wop-size",
        &project,
        "same fixture standard startup",
        StartupDetail::Standard,
        Some(&auth),
    )
    .await;
    assert!(standard.success, "{:?}", standard.error);
    assert_eq!(standard.output["repository"]["status"], "available");

    for output in [&fresh.output, &reused.output, &workflow_omitted.output] {
        for omitted in [
            "repository",
            "execution_context",
            "jobs",
            "blockers",
            "deterministic",
            "llm_summary",
        ] {
            assert!(
                output.get(omitted).is_none(),
                "work_on_project boring default field {omitted} should be omitted: {output}"
            );
        }
    }

    let fresh_overviews = fresh_requests
        .iter()
        .filter(|kind| kind.as_str() == "file_project_overview")
        .count();
    let reused_overviews = reused_requests
        .iter()
        .filter(|kind| kind.as_str() == "file_project_overview")
        .count();
    let workflow_omitted_overviews = workflow_omitted_requests
        .iter()
        .filter(|kind| kind.as_str() == "file_project_overview")
        .count();
    let standard_overviews = standard_requests
        .iter()
        .filter(|kind| kind.as_str() == "file_project_overview")
        .count();
    assert_eq!(fresh_overviews, 0);
    assert_eq!(reused_overviews, 0);
    assert_eq!(workflow_omitted_overviews, 0);
    assert_eq!(standard_overviews, 1);
    assert_eq!(
        standard_requests.len(),
        fresh_requests.len() + 1,
        "the identical advanced fixture should add only the overview request"
    );
    assert_eq!(
        reused_requests.len(),
        fresh_requests.len(),
        "unchanged continuation should retain the same lightweight probes"
    );
    assert_eq!(
        workflow_omitted_requests.len(),
        fresh_requests.len(),
        "omitting static workflow guidance must not reduce repository/runtime probes"
    );

    let fresh_bytes = serde_json::to_vec(&fresh.output).unwrap().len();
    let reused_bytes = serde_json::to_vec(&reused.output).unwrap().len();
    let workflow_omitted_bytes = serde_json::to_vec(&workflow_omitted.output).unwrap().len();
    let standard_bytes = serde_json::to_vec(&standard.output).unwrap().len();
    eprintln!(
        "work_on_project_fixture_bytes fresh={fresh_bytes} unchanged_continuation={reused_bytes} workflow_omitted={workflow_omitted_bytes} standard={standard_bytes}; runner_requests fresh={} unchanged_continuation={} workflow_omitted={} standard={}",
        fresh_requests.len(),
        reused_requests.len(),
        workflow_omitted_requests.len(),
        standard_requests.len()
    );
    let hard_max = crate::tool_runtime::startup_brief::STANDARD_STARTUP_HARD_MAX_BYTES;
    assert!(fresh_bytes < hard_max);
    assert!(reused_bytes < hard_max);
    assert!(standard_bytes <= hard_max);
    assert!(fresh_bytes < standard_bytes);
    assert!(reused_bytes < standard_bytes);
    assert!(workflow_omitted_bytes < fresh_bytes);
    // With the same repository observations and instruction body retained, the
    // static workflow-only omission is 757 bytes in this fixture. Keep enough
    // headroom for small projection growth while preserving the context win.
    assert!(
        workflow_omitted_bytes <= 1000,
        "workflow-omitted projection regressed above the context budget: {workflow_omitted_bytes} bytes"
    );
    // The sparse projection itself remains below 1 KiB when static workflow
    // guidance is omitted. With Session ACK/recording/sidecar guidance included,
    // the canonical default is about 4.0 KiB fresh and 4.1 KiB on unchanged
    // continuation. Keep that default tightly bounded and still far below the
    // standard startup hard cap while leaving modest protocol headroom.
    assert!(
        fresh_bytes <= 4300,
        "fresh work_on_project projection regressed above the sparse context budget: {fresh_bytes} bytes"
    );
    assert!(
        reused_bytes <= 4400,
        "unchanged work_on_project projection regressed above the sparse continuation budget: {reused_bytes} bytes"
    );
}

#[tokio::test]
async fn coding_workflow_standard_repository_overview_timeout_is_nonblocking() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules load despite overview timeout");
    // Tight overview timeout so the probe expires quickly.
    let runtime = ToolRuntime::new_for_tests()
        .with_repository_overview_probe_timeout(std::time::Duration::from_millis(50));
    let project =
        register_runner_project_at_path(&runtime, "wop-timeout", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let project = project.clone();
        async move {
            runtime
                .start_coding_workflow_for_test(
                    project,
                    None,
                    None,
                    Some("start despite overview timeout".to_string()),
                    SessionMode::Normal,
                    false,
                    false,
                    StartupDetail::Standard,
                    None,
                    None,
                    Some(&auth),
                    None,
                    None,
                    crate::tool_runtime::sessions::SessionTransport::Mcp,
                )
                .await
        }
    });

    // Service the git/instruction probes but never the overview request.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut overview_request = None;
    while !task.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "overview-timeout startup did not finish within 10 seconds"
        );
        let Some(request) = probe_patch_agent_request(&runtime, "wop-timeout").await else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            continue;
        };
        if request.kind == "file_project_overview" {
            overview_request = Some(request.request_id.clone());
            // Intentionally never complete it; the probe must time out.
            continue;
        }
        let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
        complete_patch_agent_request(
            &runtime,
            "wop-timeout",
            &request.request_id,
            exit_code,
            &stdout,
            &stderr,
        )
        .await;
    }
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);

    // Overview unavailable with the deterministic reason; session still works.
    assert_eq!(result.output["repository"]["status"], "unavailable");
    assert_eq!(
        result.output["repository"]["reason_code"],
        "unsupported_or_unavailable"
    );
    assert!(result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "repository_overview_unavailable"));
    let session_id = result.output["session"]["session_id"].as_str().unwrap();
    assert!(session_id.starts_with("wc_sess_"));
    let summary = runtime.sessions.summary(session_id, Some(20)).unwrap();
    assert_eq!(summary.project.as_deref(), Some(project.as_str()));

    // The timed-out overview request was cancelled server-side.
    if let Some(request_id) = overview_request {
        let expired = runtime
            .runner_registry
            .complete(crate::runner_protocol::RunnerResultRequest {
                client_id: "wop-timeout".to_string(),
                runner_instance_id: "inst".to_string(),
                request_id,
                exit_code: Some(0),
                stdout: Some("{}".to_string()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            })
            .await
            .expect_err("timed-out overview probe must remove pending waiter");
        assert!(
            expired.contains("unknown or expired shell request"),
            "{expired}"
        );
    }
}

/// Drive a coding-workflow diagnostic to completion, completing the
/// `file_project_overview` probe with `overview_stdout` (exit code 0, no error)
/// while servicing every other agent request locally. Returns the startup
/// result and the overview request id that was answered.
async fn dispatch_coding_workflow_diagnostic_with_overview_stdout(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    instruction: &str,
    detail: StartupDetail,
    overview_stdout: String,
    auth: Option<&crate::auth::AuthContext>,
) -> (crate::tool_runtime::ToolResult, Option<String>) {
    use crate::tool_runtime::sessions::SessionTransport;

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.cloned();
        let project = project.to_string();
        let instruction = instruction.to_string();
        async move {
            runtime
                .start_coding_workflow_for_test(
                    project,
                    None,
                    None,
                    Some(instruction),
                    SessionMode::Normal,
                    false,
                    false,
                    detail,
                    None,
                    None,
                    auth.as_ref(),
                    None,
                    None,
                    SessionTransport::Mcp,
                )
                .await
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut overview_request_id = None;
    while !task.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "overview startup did not finish within 10 seconds for client {client_id}"
        );
        let Some(request) = probe_patch_agent_request(runtime, client_id).await else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            continue;
        };
        if request.kind == "file_project_overview" {
            overview_request_id = Some(request.request_id.clone());
            complete_patch_agent_request(
                runtime,
                client_id,
                &request.request_id,
                0,
                &overview_stdout,
                "",
            )
            .await;
            continue;
        }
        let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
        complete_patch_agent_request(
            runtime,
            client_id,
            &request.request_id,
            exit_code,
            &stdout,
            &stderr,
        )
        .await;
    }
    let result = task.await.unwrap();
    (result, overview_request_id)
}

/// Build a structurally-valid root overview (depth 2 / limit 120) so a test can
/// mutate one field and observe fail-closed behavior. The fixture repo must
/// already be seeded and committed.
fn valid_agent_overview_stdout(
    runtime: &ToolRuntime,
    client_id: &str,
    root: &std::path::Path,
) -> String {
    let _ = (runtime, client_id);
    let overview = crate::project_overview::build_project_overview(root, ".", Some(2), Some(120))
        .expect("valid agent overview fixture");
    overview.to_string()
}

#[tokio::test]
async fn coding_workflow_standard_repository_overview_rejects_malformed_runner_responses() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules load despite malformed overview");
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "wop-malformed", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let valid = valid_agent_overview_stdout(&runtime, "wop-malformed", root.path());
    let valid_value: serde_json::Value = serde_json::from_str(&valid).unwrap();

    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("absolute path", {
            let mut v = valid_value.clone();
            v["top_level"]
                .as_array_mut()
                .unwrap()
                .push(json!({"path": "/etc/passwd", "kind": "file"}));
            v
        }),
        ("parent traversal", {
            let mut v = valid_value.clone();
            v["manifests"]
                .as_array_mut()
                .unwrap()
                .push(json!({"path": "../outside/Cargo.toml", "kind": "rust_manifest"}));
            v
        }),
        ("request boundary mismatch", {
            let mut v = valid_value.clone();
            v["scan"]["max_depth"] = json!(4);
            v["scan"]["limit"] = json!(500);
            v["path"] = json!("src");
            v
        }),
        ("unknown project type", {
            let mut v = valid_value.clone();
            v["project_types"]
                .as_array_mut()
                .unwrap()
                .push(json!({"kind": "cobol", "evidence": []}));
            v
        }),
        ("unknown key-file kind", {
            let mut v = valid_value.clone();
            v["key_files"]
                .as_array_mut()
                .unwrap()
                .push(json!({"path": "README.md", "kind": "mystery", "reason": "x"}));
            v
        }),
        ("unknown warning", {
            let mut v = valid_value.clone();
            v["warnings"]
                .as_array_mut()
                .unwrap()
                .push(json!("nuclear_launch_detected"));
            v
        }),
        ("returned_entry_count as string", {
            let mut v = valid_value.clone();
            v["scan"]["returned_entry_count"] = json!("plenty");
            v
        }),
        ("warnings as object", {
            let mut v = valid_value.clone();
            v["warnings"] = json!({"note": "not an array"});
            v
        }),
        ("duplicate top-level paths", {
            let mut v = valid_value.clone();
            let top = v["top_level"].as_array_mut().unwrap();
            top.push(top[0].clone());
            v
        }),
    ];

    for (label, payload) in cases {
        let stdout = payload.to_string();
        let (result, overview_id) = dispatch_coding_workflow_diagnostic_with_overview_stdout(
            &runtime,
            "wop-malformed",
            &project,
            label,
            StartupDetail::Standard,
            stdout,
            Some(&auth),
        )
        .await;
        assert!(
            result.success,
            "{label}: task must still succeed: {:?}",
            result.error
        );
        assert!(
            overview_id.is_some(),
            "{label}: overview probe must be issued"
        );
        let repository = &result.output["repository"];
        assert_eq!(
            repository["status"], "unavailable",
            "{label}: malformed Runner response must fail closed"
        );
        assert_eq!(
            repository["reason_code"], "unsupported_or_unavailable",
            "{label}: deterministic reason code"
        );
        // No raw stdout, stderr, error text, or absolute paths leak into the
        // model-facing compact output.
        let serialized = result.output.to_string();
        assert!(
            !serialized.contains("runner_secret"),
            "{label}: extra Runner field leaked"
        );
        assert!(
            !serialized.contains("/etc/passwd") && !serialized.contains("/absolute/leak"),
            "{label}: absolute path leaked"
        );
        assert!(
            !serialized.contains("nuclear_launch_detected") && !serialized.contains("cobol"),
            "{label}: malformed enum leaked"
        );
        assert!(
            !serialized.contains("../outside"),
            "{label}: traversal path leaked"
        );
        assert!(
            serialized.len() <= crate::tool_runtime::startup_brief::STANDARD_STARTUP_HARD_MAX_BYTES,
            "{label}: compact output exceeded 30 KiB"
        );
        // The deterministic unavailable warning is surfaced.
        assert!(
            result.output["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning == "repository_overview_unavailable"),
            "{label}: repository_overview_unavailable warning missing"
        );
        // A session is still created despite the malformed overview.
        let session_id = result.output["session"]["session_id"].as_str().unwrap();
        assert!(
            session_id.starts_with("wc_sess_"),
            "{label}: session not created"
        );
    }
}

#[tokio::test]
async fn coding_workflow_standard_overview_strips_unknown_runner_fields_and_stays_bounded() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules load despite extra runner fields");
    let runtime = ToolRuntime::new_for_tests();
    let project = register_runner_project_at_path(&runtime, "wop-strip", "demo", root.path()).await;
    let auth = auth_context(None, true);

    let valid = valid_agent_overview_stdout(&runtime, "wop-strip", root.path());
    let mut payload: serde_json::Value = serde_json::from_str(&valid).unwrap();
    // A malicious/defensive Runner adds an oversized `scan` extra field and
    // top-level unknowns (including an absolute path). The contract must not
    // fail on mere extras — it must strip them and keep the formal fields only,
    // so the model output stays small and free of leaked content.
    payload["scan"]["padding"] = json!("X".repeat(40_000));
    payload["scan"]["nested"] = json!({"deep": json!(["Y".repeat(10_000), 1, 2])});
    payload["runner_secret"] = json!("/absolute/leak");

    let (result, overview_id) = dispatch_coding_workflow_diagnostic_with_overview_stdout(
        &runtime,
        "wop-strip",
        &project,
        "strip extras",
        StartupDetail::Standard,
        payload.to_string(),
        Some(&auth),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert!(overview_id.is_some(), "overview probe must be issued");
    let repository = &result.output["repository"];
    assert_eq!(
        repository["status"], "available",
        "extras must be stripped, not rejected"
    );

    // scan keeps exactly the 5 fixed formal fields; padding/nested dropped.
    let scan = &repository["scan"];
    assert!(scan.is_object());
    let mut scan_keys: Vec<&str> = scan
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    scan_keys.sort_unstable();
    assert_eq!(
        scan_keys,
        [
            "limit",
            "max_depth",
            "returned_entry_count",
            "truncated",
            "truncation_reason"
        ],
        "scan must keep only the fixed fields: {scan:?}"
    );
    assert_eq!(scan["max_depth"], 2);
    assert_eq!(scan["limit"], 120);

    let serialized = result.output.to_string();
    assert!(!serialized.contains("padding"), "scan padding leaked");
    assert!(
        !serialized.contains("runner_secret"),
        "extra runner field leaked"
    );
    assert!(
        !serialized.contains("/absolute/leak"),
        "absolute path leaked"
    );
    assert!(
        serialized.len() <= crate::tool_runtime::startup_brief::STANDARD_STARTUP_HARD_MAX_BYTES,
        "compact output exceeded 30 KiB after stripping"
    );
}

#[tokio::test]
async fn coding_workflow_standard_and_full_accept_valid_repository_overview() {
    let root = tempfile::tempdir().unwrap();
    seed_coding_repository(root.path(), "rules load with valid overview");
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "wop-valid-overview", "demo", root.path()).await;

    let auth = auth_context(None, true);
    for detail in [StartupDetail::Standard, StartupDetail::Full] {
        let stdout = valid_agent_overview_stdout(&runtime, "wop-valid-overview", root.path());
        let (result, overview_id) = dispatch_coding_workflow_diagnostic_with_overview_stdout(
            &runtime,
            "wop-valid-overview",
            &project,
            "valid overview",
            detail,
            stdout,
            Some(&auth),
        )
        .await;
        assert!(result.success, "{detail:?}: {:?}", result.error);
        assert!(
            overview_id.is_some(),
            "{detail:?}: overview probe must be issued"
        );
        let brief = crate::tool_runtime::startup_brief::startup_brief_from_output(&result.output)
            .expect("advanced startup brief");
        let repository = &brief["repository"];
        assert_eq!(
            repository["status"], "available",
            "{detail:?}: valid response must be accepted"
        );
        // scan projection keeps only the fixed fields, no extras.
        let scan = &repository["scan"];
        assert!(scan.is_object());
        assert_eq!(scan.as_object().unwrap().len(), 5);
        assert_eq!(scan["max_depth"], 2);
        assert_eq!(scan["limit"], 120);
        // Rust is detected via the committed Cargo.toml fixture.
        let types = repository["project_types"]["items"].as_array().unwrap();
        assert!(types.iter().any(|kind| kind["kind"] == "rust"));
        assert!(repository["manifests"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|manifest| manifest["path"] == "Cargo.toml"));
        assert!(!repository["key_files"]["items"]
            .as_array()
            .unwrap()
            .is_empty());
        let serialized = repository.to_string();
        assert!(!serialized.contains(&root.path().to_string_lossy().to_string()));

        let schema =
            crate::tool_runtime::registry::coding_workflow_diagnostic_output_schema_for_test();
        let instance = json!({"success": true, "output": result.output});
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&instance, &schema)
            .unwrap_or_else(|error| {
                panic!("{detail:?} coding workflow diagnostic must match strict schema: {error}")
            });
    }
}
