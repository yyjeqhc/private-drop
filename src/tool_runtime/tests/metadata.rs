//! Metadata tests for tool_runtime.

use super::super::*;
use super::support::*;
use crate::runner_http::RunnerRegistry;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerRegisterRequest, RunnerResultRequest, ShellProjectInventoryPage,
    RUNNER_PROTOCOL_GENERATION_V2,
};
use crate::tool_runtime::kernel::{
    HostFileImportTrust, ToolCallContext, ToolCallErrorStatus, ToolCallRequest,
    ToolProtocolCapabilities, ToolTransport,
};
use crate::tool_runtime::sessions::{
    TOOL_CALL_EXPECTATION_METADATA_FIELDS, TOOL_CALL_RECORDING_SESSION_ID_FIELD,
};
use crate::tool_runtime::TOOL_CALL_WRAPPER_FIELDS;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

fn shared_key_auth(hash: &str) -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        kind: crate::auth::AuthKind::SharedKey,
        user_id: None,
        username: None,
        api_key_id: None,
        role: Some("shared-key".to_string()),
        scopes: vec![
            crate::auth::SCOPE_RUNTIME_READ.to_string(),
            crate::auth::SCOPE_PROJECT_READ.to_string(),
            crate::auth::SCOPE_PROJECT_WRITE.to_string(),
            crate::auth::SCOPE_JOB_RUN.to_string(),
            crate::auth::SCOPE_AGENT_REGISTER.to_string(),
        ],
        is_bootstrap: false,
        token_kind: Some("shared-key".to_string()),
        allowed_client_id: None,
        shared_key_hash: Some(hash.to_string()),
        project_grant_id: None,
    }
}

fn open_auth() -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        kind: crate::auth::AuthKind::OpenAnonymous,
        user_id: None,
        username: None,
        api_key_id: None,
        role: Some("open".to_string()),
        scopes: vec![
            crate::auth::SCOPE_RUNTIME_READ.to_string(),
            crate::auth::SCOPE_PROJECT_READ.to_string(),
            crate::auth::SCOPE_PROJECT_WRITE.to_string(),
            crate::auth::SCOPE_JOB_RUN.to_string(),
            crate::auth::SCOPE_AGENT_REGISTER.to_string(),
        ],
        is_bootstrap: false,
        token_kind: Some("open".to_string()),
        allowed_client_id: None,
        shared_key_hash: None,
        project_grant_id: None,
    }
}

fn bootstrap_auth() -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        kind: crate::auth::AuthKind::Bootstrap,
        user_id: None,
        username: None,
        api_key_id: None,
        role: Some("admin".to_string()),
        scopes: vec![crate::auth::SCOPE_ADMIN.to_string()],
        is_bootstrap: true,
        token_kind: None,
        allowed_client_id: None,
        shared_key_hash: None,
        project_grant_id: None,
    }
}

fn runtime_status_call() -> ToolCall {
    ToolCall::RuntimeStatus {
        compact: false,
        summary_only: false,
        client_id: None,
    }
}

fn list_projects_call() -> ToolCall {
    ToolCall::ListProjects {
        client_id: None,
        project: None,
        query: None,
        limit: None,
        summary_only: false,
    }
}

fn list_runners_call() -> ToolCall {
    ToolCall::ListRunners {
        client_id: None,
        client_ids: None,
        include_projects: None,
        summary_only: false,
    }
}

fn metadata_agent_registration(client_id: &str) -> RunnerRegisterRequest {
    crate::test_support::current_runner_registration(RunnerRegisterRequest {
        process_started_at: None,
        build: None,
        job_concurrency_limit: None,
        job_inventory: None,
        coding_agent_providers: None,
        coding_agent_inventory: None,
        client_id: client_id.to_string(),
        runner_instance_id: format!("inst-{client_id}"),
        runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
        display_name: None,
        owner: None,
        hostname: None,
        host_context: None,
        capabilities: RunnerCapabilities::default(),
        policy: None,
    })
}

async fn register_computer_target_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    display_name: &str,
    auth: &crate::auth::AuthContext,
    computer_observe: bool,
    computer_snapshot_region: bool,
    computer_accessibility_observe: bool,
) {
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: format!("inst-{client_id}"),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some(display_name.to_string()),
                owner: None,
                hostname: Some(format!("host-{client_id}")),
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        computer_observe,
                        computer_snapshot_region,
                        computer_accessibility_observe,
                        ..Default::default()
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
}

async fn register_application_target_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    display_name: &str,
    auth: &crate::auth::AuthContext,
    computer_application_discovery: bool,
    computer_application_launch: bool,
) {
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: format!("inst-{client_id}"),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some(display_name.to_string()),
                owner: None,
                hostname: Some(format!("host-{client_id}")),
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        computer_application_discovery,
                        computer_application_launch,
                        ..Default::default()
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
}

async fn register_display_target_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    display_name: &str,
    auth: &crate::auth::AuthContext,
) {
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: format!("inst-{client_id}"),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some(display_name.to_string()),
                owner: None,
                hostname: Some(format!("host-{client_id}")),
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        computer_display_observe: true,
                        ..Default::default()
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
}

async fn register_pointer_target_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    display_name: &str,
    auth: &crate::auth::AuthContext,
) {
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: format!("inst-{client_id}"),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some(display_name.to_string()),
                owner: None,
                hostname: Some(format!("host-{client_id}")),
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        computer_pointer_control: true,
                        ..Default::default()
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
}
async fn register_clipboard_target_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    display_name: &str,
    auth: &crate::auth::AuthContext,
    read: bool,
    write: bool,
) {
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: format!("inst-{client_id}"),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some(display_name.to_string()),
                owner: None,
                hostname: Some(format!("host-{client_id}")),
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        computer_clipboard_read: read,
                        computer_clipboard_write: write,
                        ..Default::default()
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
}

async fn register_agent_projects_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    auth: &crate::auth::AuthContext,
    project_id: &str,
) {
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: format!("inst-{}", client_id),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        shell: true,
                        file_read: true,
                        file_write: true,
                        artifact_export_chunk_read: false,
                        artifact_export_streaming_metadata: false,
                        structured_file_delete: false,
                        apply_text_edit_occurrence: false,
                        apply_text_edit_line_scope: false,
                        apply_patch: false,
                        apply_patch_match_metadata: false,
                        apply_patch_matching_mode: false,
                        apply_patch_strict_matching: false,
                        git: true,
                        jobs: true,
                        async_jobs: true,
                        async_shell_jobs: true,
                        ssh_shell: false,
                        persistent_shell: false,
                        ssh_persistent_shell: false,
                        structured_validation_argv: true,
                        structured_cargo_test_count_assertion: true,
                        structured_cargo_test_execution_policy: true,
                        structured_go_test_json: true,
                        structured_go_test_tool: true,
                        structured_go_test_packages: true,
                        structured_process_argv: true,
                        structured_script_payload: false,
                        structured_script_javascript: false,
                        internal_posix_script: false,
                        structured_execution_jobs: false,
                        detached_process_jobs: false,
                        lsp_read_only_navigation: false,
                        lsp_call_hierarchy: false,
                        project_lifecycle: false,
                        project_path_registration: false,
                        managed_worktree: false,
                        skill_store_read: false,
                        skill_store_manage: false,
                        computer_observe: false,
                        computer_application_discovery: false,
                        computer_application_launch: false,
                        computer_display_observe: false,
                        computer_pointer_control: false,
                        computer_clipboard_read: false,
                        computer_clipboard_write: false,
                        computer_snapshot_region: false,
                        computer_accessibility_observe: false,
                        computer_element_state: false,
                        computer_control: false,
                        computer_scroll_to_element: false,
                        computer_key_input: false,
                        computer_window_activate: false,
                        computer_text_input: false,
                        job_state_reconciliation: false,
                        coding_agent_runs: false,
                        native_tool_plugins: false,
                        managed_ssh_resources: false,
                        runner_config_control: false,
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        &format!("inst-{client_id}"),
        vec![registered_project(
            project_id,
            &format!("/tmp/{project_id}"),
        )],
    )
    .await;
}

#[tokio::test]
async fn coding_agent_start_uses_canonical_runner_capability_gate() {
    let runtime = test_runtime();
    register_agent_with_projects(
        &runtime,
        "coding-capability-gate",
        None,
        RunnerCapabilities::default(),
        vec![registered_project("demo", "/tmp/coding-capability-gate")],
    )
    .await;
    let project = runner_project_runtime_id("coding-capability-gate", "demo");

    let result = runtime
        .coding_agent_start(
            project,
            "codex".to_string(),
            "c3b-canonical-capability-gate".to_string(),
            "prove capability admission".to_string(),
            None,
            None,
            None,
            None,
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "coding_agent_unsupported");
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("does not advertise CodingAgentRun")));
}

#[tokio::test]
async fn list_projects_returns_agent_registered_projects_without_server_config() {
    let runtime = test_runtime();
    register_agent_with_projects(
        &runtime,
        "workstation-1",
        None,
        RunnerCapabilities::default(),
        vec![registered_project("webcodex", "/root/git/webcodex")],
    )
    .await;

    let result = runtime.dispatch(list_projects_call()).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["count"], 1);
    let projects = result.output["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["id"], "agent:workstation-1:webcodex");
    assert_eq!(projects[0]["agent_project_id"], "webcodex");
    assert_eq!(projects[0]["executor"], "agent");
    assert_eq!(projects[0]["source"], "agent_registered");
    assert!(projects[0]["capabilities"].is_object());
    assert_eq!(projects[0]["capabilities"]["git_available"], false);
    assert_eq!(projects[0]["capabilities"]["recommended_for_smoke"], false);
}

#[tokio::test]
async fn list_projects_uses_registration_provenance_without_reclassifying_project_kind() {
    let runtime = test_runtime();
    let explicit = registered_project("explicit-repo", "/root/git/explicit-repo");
    assert_eq!(explicit.kind.as_deref(), Some("repo"));
    let mut auto = registered_project("auto-repo", "/root/git/auto-repo");
    auto.kind = Some("auto_registered".to_string());
    auto.registration_source = Some("auto_registered".to_string());
    register_agent_with_projects(
        &runtime,
        "registration-source-runner",
        None,
        RunnerCapabilities::default(),
        vec![explicit, auto],
    )
    .await;

    let result = runtime.dispatch(list_projects_call()).await;
    assert!(result.success, "{:?}", result.error);
    let projects = result.output["projects"].as_array().unwrap();
    let explicit = projects
        .iter()
        .find(|project| project["agent_project_id"] == "explicit-repo")
        .unwrap();
    let auto = projects
        .iter()
        .find(|project| project["agent_project_id"] == "auto-repo")
        .unwrap();
    assert_eq!(explicit["source"], "agent_registered");
    assert_eq!(auto["source"], "auto_registered");
}

#[tokio::test]
async fn list_projects_reports_smoke_selection_capabilities() {
    let runtime = test_runtime();
    let mut test_mcp = registered_project("test-mcp", "/tmp/test-mcp");
    test_mcp.name = Some("Test MCP".to_string());
    let mut smoke = registered_project("webcodex-smoke", "/tmp/webcodex-smoke");
    smoke.name = Some("WebCodex Smoke Workspace".to_string());
    smoke.git_branch = Some("main".to_string());
    smoke.git_head = Some("abc1234".to_string());
    smoke.git_dirty = Some(false);
    register_agent_with_projects(
        &runtime,
        "special",
        None,
        RunnerCapabilities {
            file_read: true,
            file_write: true,
            git: true,
            ..Default::default()
        },
        vec![test_mcp, smoke],
    )
    .await;

    let result = runtime.dispatch(list_projects_call()).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["count"], 2);
    let projects = result.output["projects"].as_array().unwrap();
    let test_mcp = projects
        .iter()
        .find(|project| project["id"] == "agent:special:test-mcp")
        .expect("test-mcp project");
    let smoke = projects
        .iter()
        .find(|project| project["id"] == "agent:special:webcodex-smoke")
        .expect("webcodex-smoke project");

    assert_eq!(test_mcp["capabilities"]["safe_smoke_project"], true);
    assert_eq!(test_mcp["capabilities"]["git_available"], false);
    assert_eq!(
        test_mcp["capabilities"]["supports_cleanup_verification"],
        true
    );
    assert_eq!(test_mcp["capabilities"]["recommended_for_smoke"], false);
    assert_eq!(smoke["capabilities"]["safe_smoke_project"], true);
    assert_eq!(smoke["capabilities"]["git_available"], true);
    assert_eq!(smoke["capabilities"]["supports_artifact_smoke"], true);
    assert_eq!(smoke["capabilities"]["recommended_for_smoke"], true);
    assert_eq!(
        result.output["recommended_for_smoke"],
        json!(["agent:special:webcodex-smoke"])
    );
}

#[tokio::test]
async fn runner_config_tools_use_normal_kernel_scope_gate_before_runner_dispatch() {
    let runtime = test_runtime();
    let read_only = oauth_bridge_auth_context(
        "runner-config-read-only",
        &[crate::auth::SCOPE_RUNTIME_READ],
    );
    let context = |auth| ToolCallContext {
        transport: ToolTransport::Mcp,
        session_id: None,
        auth,
        window: None,
        record_oauth_scope_denials: false,
        host_file_import_trust: HostFileImportTrust::Untrusted,
    };

    let check = runtime
        .call_tool_with_protocol_capabilities(
            ToolCallRequest {
                tool_name: "runner_config_check".to_string(),
                arguments: json!({"client_id": "missing-runner"}),
            },
            context(Some(&read_only)),
            ToolProtocolCapabilities::default(),
        )
        .await;
    assert!(check.error_status.is_none());
    let check = check
        .result
        .expect("read-like config check should reach dispatch");
    assert!(!check.success);
    assert_eq!(check.output["execution_state"], "not_started");
    assert_eq!(check.output["error_code"], "runner_unavailable");

    let denied_reload = runtime
        .call_tool_with_protocol_capabilities(
            ToolCallRequest {
                tool_name: "runner_config_reload".to_string(),
                arguments: json!({
                    "client_id": "missing-runner",
                    "expected_generation": 1
                }),
            },
            context(Some(&read_only)),
            ToolProtocolCapabilities::default(),
        )
        .await;
    assert!(denied_reload.result.is_none());
    assert!(matches!(
        denied_reload.error_status,
        Some(ToolCallErrorStatus::InsufficientScope {
            required_scope: Some(webcodex_core::authority::SCOPE_RUNNER_MANAGE),
            ..
        })
    ));

    let manager = oauth_bridge_auth_context(
        "runner-config-manager",
        &[webcodex_core::authority::SCOPE_RUNNER_MANAGE],
    );
    let allowed_reload = runtime
        .call_tool_with_protocol_capabilities(
            ToolCallRequest {
                tool_name: "runner_config_reload".to_string(),
                arguments: json!({
                    "client_id": "missing-runner",
                    "expected_generation": 1
                }),
            },
            context(Some(&manager)),
            ToolProtocolCapabilities::default(),
        )
        .await;
    assert!(allowed_reload.error_status.is_none());
    let allowed_reload = allowed_reload
        .result
        .expect("runner:manage should reach exact Runner dispatch");
    assert!(!allowed_reload.success);
    assert_eq!(allowed_reload.output["execution_state"], "not_started");
    assert_eq!(allowed_reload.output["error_code"], "runner_unavailable");
}

#[tokio::test]
async fn shared_key_list_projects_and_dispatch_are_filtered_by_auth_group() {
    let runtime = test_runtime();
    let shared_a = shared_key_auth("hash-a");
    let shared_b = shared_key_auth("hash-b");
    let bridge_a = oauth_bridge_auth_context("hash-a", &[crate::auth::SCOPE_PROJECT_READ]);
    let bridge_b = oauth_bridge_auth_context("hash-b", &[crate::auth::SCOPE_PROJECT_READ]);
    let open = open_auth();
    let bootstrap = bootstrap_auth();

    register_agent_projects_for_auth(&runtime, "client-a", &shared_a, "proj-a").await;
    register_agent_projects_for_auth(&runtime, "client-b", &shared_b, "proj-b").await;
    register_agent_projects_for_auth(&runtime, "client-open", &open, "proj-open").await;

    let result = runtime
        .dispatch_with_auth(list_projects_call(), Some(&shared_a))
        .await;
    assert!(result.success, "{:?}", result.error);
    let ids: Vec<&str> = result
        .output
        .get("projects")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|project| project["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["agent:client-a:proj-a"]);

    let result = runtime
        .dispatch_with_auth(list_projects_call(), Some(&bridge_a))
        .await;
    assert!(result.success, "{:?}", result.error);
    let ids: Vec<&str> = result
        .output
        .get("projects")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|project| project["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["agent:client-a:proj-a"]);

    let bridge_read = tokio::spawn({
        let runtime = runtime.clone();
        let bridge_a = bridge_a.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFile {
                        project: "agent:client-a:proj-a".to_string(),
                        path: "README.md".to_string(),
                        session_id: None,
                        start_line: None,
                        limit: None,
                        with_line_numbers: None,
                    },
                    Some(&bridge_a),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "client-a").await;
    complete_patch_agent_request_for_instance(
        &runtime,
        "client-a",
        "inst-client-a",
        &req.request_id,
        0,
        &canonical_agent_file_read_output("bridge\n", 1),
        "",
    )
    .await;
    let result = bridge_read.await.unwrap();
    assert!(result.success, "{:?}", result.error);

    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFile {
                project: "agent:client-b:proj-b".to_string(),
                path: "README.md".to_string(),
                session_id: None,
                start_line: None,
                limit: None,
                with_line_numbers: None,
            },
            Some(&bridge_a),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_project");

    let result = runtime
        .dispatch_with_auth(list_projects_call(), Some(&bridge_b))
        .await;
    assert!(result.success, "{:?}", result.error);
    let ids: Vec<&str> = result
        .output
        .get("projects")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|project| project["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["agent:client-b:proj-b"]);

    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFile {
                project: "agent:client-a:proj-a".to_string(),
                path: "README.md".to_string(),
                session_id: None,
                start_line: None,
                limit: None,
                with_line_numbers: None,
            },
            Some(&bridge_b),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_project");

    let result = runtime
        .dispatch_with_auth(list_projects_call(), Some(&open))
        .await;
    let ids: Vec<&str> = result
        .output
        .get("projects")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|project| project["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["agent:client-open:proj-open"]);

    let open_read = tokio::spawn({
        let runtime = runtime.clone();
        let open = open.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFile {
                        project: "agent:client-open:proj-open".to_string(),
                        path: "README.md".to_string(),
                        session_id: None,
                        start_line: None,
                        limit: None,
                        with_line_numbers: None,
                    },
                    Some(&open),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "client-open").await;
    complete_patch_agent_request_for_instance(
        &runtime,
        "client-open",
        "inst-client-open",
        &req.request_id,
        0,
        &canonical_agent_file_read_output("open\n", 1),
        "",
    )
    .await;
    let result = open_read.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_ne!(result.output["error_kind"], "current_session_unavailable");

    let open_git = tokio::spawn({
        let runtime = runtime.clone();
        let open = open.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::GitStatus {
                        project: "agent:client-open:proj-open".to_string(),
                        session_id: None,
                    },
                    Some(&open),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "client-open").await;
    complete_patch_agent_request_for_instance(
        &runtime,
        "client-open",
        "inst-client-open",
        &req.request_id,
        0,
        "",
        "",
    )
    .await;
    let result = open_git.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_ne!(result.output["error_kind"], "current_session_unavailable");

    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFile {
                project: "agent:client-a:proj-a".to_string(),
                path: "README.md".to_string(),
                session_id: None,
                start_line: None,
                limit: None,
                with_line_numbers: None,
            },
            Some(&open),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_project");

    let result = runtime
        .dispatch_with_auth(list_projects_call(), Some(&bootstrap))
        .await;
    let ids: Vec<&str> = result
        .output
        .get("projects")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|project| project["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            "agent:client-a:proj-a",
            "agent:client-b:proj-b",
            "agent:client-open:proj-open",
        ]
    );

    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFile {
                project: "agent:client-b:proj-b".to_string(),
                path: "README.md".to_string(),
                session_id: None,
                start_line: None,
                limit: None,
                with_line_numbers: None,
            },
            Some(&shared_a),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_project");

    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFile {
                project: "agent:client-open:proj-open".to_string(),
                path: "README.md".to_string(),
                session_id: None,
                start_line: None,
                limit: None,
                with_line_numbers: None,
            },
            Some(&shared_a),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "unknown_project");
}

#[tokio::test]
async fn replacement_runner_pending_inventory_has_zero_project_routing_authority() {
    let runtime = test_runtime();
    let auth = bootstrap_auth();
    let client_id = "restart-authority";
    let old_instance = format!("inst-{client_id}");
    let new_instance = "restart-authority-new";
    let path_a = tempfile::tempdir().unwrap();
    let path_b = tempfile::tempdir().unwrap();
    let path_a = path_a.path().to_string_lossy().to_string();
    let path_b = path_b.path().to_string_lossy().to_string();
    let project_id = crate::tool_runtime::runner_project_runtime_id(client_id, "demo");
    let capabilities = RunnerCapabilities {
        shell: true,
        file_read: true,
        file_write: true,
        jobs: true,
        async_jobs: true,
        async_shell_jobs: true,
        ..Default::default()
    };

    register_agent_projects(
        &runtime,
        client_id,
        None,
        capabilities.clone(),
        vec![registered_project("demo", &path_a)],
    )
    .await;
    let initial = runtime
        .resolve_project_input_for_auth(&project_id, Some(&auth))
        .await
        .unwrap();
    assert_eq!(initial.config.path, path_a);

    runtime
        .runner_registry
        .reconcile_disconnect(client_id, &old_instance)
        .await;
    let replacement = runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: new_instance.to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(capabilities),
            policy: None,
        })
        .await
        .unwrap();
    assert!(replacement.connected);
    assert!(replacement.projects.is_empty());
    assert_eq!(
        replacement
            .project_inventory
            .as_ref()
            .map(|status| status.sync_state.as_str()),
        Some("pending")
    );

    let pending_calls = vec![
        ToolCall::RunShell {
            project: project_id.clone(),
            command: "pwd".to_string(),
            session_id: None,
            timeout_secs: Some(30),
            cwd: None,
            purpose: None,
            shell: None,
        },
        ToolCall::ReadFile {
            project: project_id.clone(),
            path: "README.md".to_string(),
            session_id: None,
            start_line: None,
            limit: None,
            with_line_numbers: None,
        },
        ToolCall::WriteProjectFile {
            project: project_id.clone(),
            path: "blocked.txt".to_string(),
            content: "must not dispatch".to_string(),
            session_id: None,
            overwrite: None,
            expected_sha256: None,
        },
        ToolCall::RunJob {
            project: project_id.clone(),
            command: "pwd".to_string(),
            session_id: None,
            timeout_secs: Some(30),
            cwd: None,
            purpose: None,
            shell: None,
        },
    ];
    for call in pending_calls {
        let result = runtime.dispatch_with_auth(call, Some(&auth)).await;
        assert!(
            !result.success,
            "pending replacement unexpectedly routed: {result:?}"
        );
        assert_eq!(result.output["error_kind"], "unknown_project");
    }
    assert!(
        probe_agent_request_for_instance(&runtime, client_id, new_instance)
            .await
            .is_none(),
        "pending replacement must receive zero project execution dispatches"
    );

    let completed = runtime
        .runner_registry
        .apply_project_inventory_page(
            client_id,
            new_instance,
            ShellProjectInventoryPage {
                generation: "replacement-authoritative".to_string(),
                snapshot_sequence: 1,
                page_index: 0,
                total_reported: 1,
                complete: true,
                projects: vec![registered_project("demo", &path_b)],
            },
        )
        .await
        .unwrap();
    assert_eq!(completed.sync_state, "complete");
    let resolved = runtime
        .resolve_project_input_for_auth(&project_id, Some(&auth))
        .await
        .unwrap();
    assert_eq!(resolved.config.path, path_b);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let project_id = project_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project: project_id,
                        command: "pwd".to_string(),
                        session_id: None,
                        timeout_secs: Some(30),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_runner_request_for_instance(&runtime, client_id, new_instance).await;
    assert_eq!(request.cwd.as_deref(), Some(path_b.as_str()));
    complete_patch_agent_request_for_instance(
        &runtime,
        client_id,
        new_instance,
        &request.request_id,
        0,
        "ok\n",
        "",
    )
    .await;
    assert!(task.await.unwrap().success);
}

#[tokio::test]
async fn replacement_runner_removed_project_never_inherits_old_authority() {
    let runtime = test_runtime();
    let auth = bootstrap_auth();
    let client_id = "restart-removal-authority";
    let old_instance = format!("inst-{client_id}");
    let new_instance = "restart-removal-new";
    let old_root = tempfile::tempdir().unwrap();
    let old_root = old_root.path().to_string_lossy().to_string();
    let project_id = crate::tool_runtime::runner_project_runtime_id(client_id, "demo");
    let capabilities = RunnerCapabilities {
        shell: true,
        file_read: true,
        file_write: true,
        ..Default::default()
    };

    register_agent_projects(
        &runtime,
        client_id,
        None,
        capabilities.clone(),
        vec![registered_project("demo", &old_root)],
    )
    .await;
    runtime
        .runner_registry
        .reconcile_disconnect(client_id, &old_instance)
        .await;
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: new_instance.to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(capabilities),
            policy: None,
        })
        .await
        .unwrap();

    for phase in ["pending", "complete"] {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::RunShell {
                    project: project_id.clone(),
                    command: "pwd".to_string(),
                    session_id: None,
                    timeout_secs: Some(30),
                    cwd: None,
                    purpose: None,
                    shell: None,
                },
                Some(&auth),
            )
            .await;
        assert!(
            !result.success,
            "removed project routed during {phase}: {result:?}"
        );
        assert_eq!(result.output["error_kind"], "unknown_project");
        assert!(
            probe_agent_request_for_instance(&runtime, client_id, new_instance)
                .await
                .is_none(),
            "removed project must receive zero dispatch during {phase}"
        );
        if phase == "pending" {
            let status = runtime
                .runner_registry
                .apply_project_inventory_page(
                    client_id,
                    new_instance,
                    ShellProjectInventoryPage {
                        generation: "replacement-empty".to_string(),
                        snapshot_sequence: 1,
                        page_index: 0,
                        total_reported: 0,
                        complete: true,
                        projects: Vec::new(),
                    },
                )
                .await
                .unwrap();
            assert_eq!(status.sync_state, "complete");
        }
    }
}

#[tokio::test]
async fn list_projects_shows_shell_profile_resolution() {
    use crate::runner_protocol::{RunnerPolicySummary, ShellProfilesSummary};
    let runtime = test_runtime();
    let summary = ShellProfilesSummary {
        default_dialect: None,
        available_dialects: None,
        default_profile: Some("rust".to_string()),
        configured_count: 1,
        prepared_cache_count: 0,
        profiles: vec![profile_summary_entry("rust", false, 2)],
    };
    let policy = RunnerPolicySummary {
        allow_raw_shell: true,
        allow_cwd_anywhere: true,
        allowed_roots: Vec::new(),
        max_timeout_secs: 3600,
        max_output_bytes: 262144,
        shell_profiles: Some(summary),
        tool_providers: None,
        mcp_gateway_providers: None,
    };
    let mut configured = registered_project("rust-proj", "/root/git/rust");
    configured.shell_profile = Some("rust".to_string());
    let mut missing = registered_project("bad-proj", "/root/git/bad");
    missing.shell_profile = Some("nope".to_string());
    let mut fallback = registered_project("default-proj", "/root/git/default");
    // No explicit shell_profile: should resolve to default_profile "rust".
    let _ = fallback.shell_profile.take();
    register_agent_with_shell_profiles(
        &runtime,
        "ws-1",
        Some(policy),
        vec![configured, missing, fallback],
    )
    .await;

    let result = runtime.dispatch(list_projects_call()).await;
    assert!(result.success, "{:?}", result.error);
    let projects = result.output["projects"].as_array().unwrap();
    let by_id: std::collections::HashMap<&str, &Value> = projects
        .iter()
        .map(|p| (p["agent_project_id"].as_str().unwrap(), p))
        .collect();
    // Explicit profile that is configured.
    let cfg = by_id["rust-proj"];
    assert_eq!(cfg["shell_profile"], "rust");
    assert_eq!(cfg["resolved_shell_profile"], "rust");
    assert_eq!(cfg["shell_profile_status"], "configured");
    // Explicit profile that is missing.
    let miss = by_id["bad-proj"];
    assert_eq!(miss["shell_profile"], "nope");
    assert_eq!(miss["resolved_shell_profile"], "nope");
    assert_eq!(miss["shell_profile_status"], "missing");
    // No explicit profile: resolves to default_profile "rust".
    let def = by_id["default-proj"];
    assert_eq!(def["shell_profile"], Value::Null);
    assert_eq!(def["resolved_shell_profile"], "rust");
    assert_eq!(def["shell_profile_status"], "configured");
    // Agent liveness fields are surfaced for each project.
    assert_eq!(def["agent_status"], "online");
    assert_eq!(def["connected"], true);
}

#[tokio::test]
async fn list_projects_shell_profile_status_unknown_without_summary() {
    // An older agent that did not report a shell-profiles summary (policy
    // is None): a project with a shell_profile resolves but its configured
    // state is "unknown" because the configured set cannot be checked.
    let runtime = test_runtime();
    let mut project = registered_project("proj", "/root/git/proj");
    project.shell_profile = Some("rust".to_string());
    register_agent_with_shell_profiles(&runtime, "legacy", None, vec![project]).await;

    let result = runtime.dispatch(list_projects_call()).await;
    assert!(result.success);
    let projects = result.output["projects"].as_array().unwrap();
    assert_eq!(projects[0]["resolved_shell_profile"], "rust");
    assert_eq!(projects[0]["shell_profile_status"], "unknown");
}

#[tokio::test]
async fn project_path_registration_capability_is_projected_safely() {
    let runtime = test_runtime();
    let private_path = "/srv/private/project-path-registration";
    register_agent_with_projects(
        &runtime,
        "path-capable-agent",
        None,
        RunnerCapabilities {
            project_path_registration: true,
            ..Default::default()
        },
        vec![registered_project("private-project", private_path)],
    )
    .await;

    let listed = runtime.dispatch(list_runners_call()).await;
    assert!(listed.success, "{:?}", listed.error);
    assert_eq!(
        listed.output["agents"][0]["capabilities"]["project_path_registration"],
        true
    );

    let status = runtime.dispatch(runtime_status_call()).await;
    assert!(status.success, "{:?}", status.error);
    assert_eq!(
        status.output["agents"]["clients"][0]["capabilities"]["project_path_registration"],
        true
    );
    assert!(
        !status.output.to_string().contains(private_path),
        "runtime_status leaked a registered project path"
    );
}

#[tokio::test]
async fn runtime_status_shell_profiles_summary_is_sanitized() {
    use crate::runner_protocol::{
        RunnerPolicySummary, ShellProfileSummaryEntry, ShellProfilesSummary,
    };
    let registry = Arc::new(RunnerRegistry::default());
    let secret_env_value = "DO_NOT_LEAK_THIS_ENV_VALUE";
    let secret_script = "DO_NOT_LEAK_THIS_INIT_SCRIPT_BODY";
    let summary = ShellProfilesSummary {
        default_dialect: None,
        available_dialects: None,
        default_profile: Some("rust".to_string()),
        configured_count: 1,
        prepared_cache_count: 0,
        profiles: vec![ShellProfileSummaryEntry {
            dialect: None,
            name: "rust".to_string(),
            has_init_script: true,
            env_keys_count: 3,
            program: "sh".to_string(),
            args_count: 1,
        }],
    };
    // The summary itself never carries env values or init_script bodies;
    // the secrets below are only carried in local test variables to prove
    // they never reach the status JSON.
    let _ = (secret_env_value, secret_script);
    registry
        .register(crate::runner_protocol::RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "profile-agent".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(
                RunnerCapabilities::default(),
            ),
            policy: Some(RunnerPolicySummary {
                allow_raw_shell: true,
                allow_cwd_anywhere: false,
                allowed_roots: Vec::new(),
                max_timeout_secs: 3600,
                max_output_bytes: 262144,
                shell_profiles: Some(summary),
                tool_providers: None,
                mcp_gateway_providers: None,
            }),
        })
        .await
        .unwrap();
    let runtime = ToolRuntime::new(registry, Arc::new(RuntimeInfo::default()));
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    let client = &result.output["agents"]["clients"][0];
    let sp = &client["shell_profiles"];
    assert_eq!(sp["default_profile"], "rust");
    assert_eq!(sp["configured_count"], 1);
    assert_eq!(sp["profiles"][0]["name"], "rust");
    assert_eq!(sp["profiles"][0]["has_init_script"], true);
    assert_eq!(sp["profiles"][0]["env_keys_count"], 3);
    assert_eq!(sp["profiles"][0]["program"], "sh");
    assert_eq!(sp["profiles"][0]["args_count"], 1);
    // Sanitization: never expose init_script bodies or env values.
    let rendered = sp.to_string();
    assert!(!rendered.contains("DO_NOT_LEAK_THIS_ENV_VALUE"));
    assert!(!rendered.contains("DO_NOT_LEAK_THIS_INIT_SCRIPT_BODY"));
    assert!(sp["profiles"][0].get("init_script").is_none());
    assert!(sp["profiles"][0].get("env").is_none());
}

#[tokio::test]
async fn unique_short_agent_project_id_is_resolved_by_runtime_surface() {
    let runtime = runtime_with_agent_project("oe");
    register_agent(
        &runtime,
        "oe",
        None,
        RunnerCapabilities {
            shell: true,
            ..Default::default()
        },
    )
    .await;
    let bootstrap = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project: "agent-proj".to_string(),
                        command: "echo hi".to_string(),
                        session_id: None,
                        timeout_secs: Some(1),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_instance(&runtime, "oe", "inst").await;
    assert_eq!(req.cwd.as_deref(), Some("/tmp/agent-proj"));
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "oe".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some("hi\n".to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test]
async fn runner_capability_rejection_matrix_names_required_capability() {
    enum CapabilityCase {
        RunShell,
        GitStatus,
    }

    let cases = [
        (
            "cap-shell",
            RunnerCapabilities {
                shell: false,
                ..Default::default()
            },
            CapabilityCase::RunShell,
            vec!["does not support shell", "Runner cap-shell"],
        ),
        (
            "cap-git",
            RunnerCapabilities {
                shell: false,
                ..Default::default()
            },
            CapabilityCase::GitStatus,
            vec!["does not support shell or git"],
        ),
    ];
    let bootstrap = auth_context(None, true);

    for (client_id, capabilities, case, expected_fragments) in cases {
        let runtime = runtime_with_agent_project(client_id);
        register_agent(&runtime, client_id, None, capabilities).await;
        let project = agent_test_project_id(client_id);
        let call = match case {
            CapabilityCase::RunShell => ToolCall::RunShell {
                project,
                command: "echo hi".to_string(),
                session_id: None,
                timeout_secs: None,
                cwd: None,
                purpose: None,
                shell: None,
            },
            CapabilityCase::GitStatus => ToolCall::GitStatus {
                project,
                session_id: None,
            },
        };
        let result = runtime.dispatch_with_auth(call, Some(&bootstrap)).await;
        assert!(
            !result.success,
            "{client_id}: capability gate unexpectedly allowed call"
        );
        let error = result.error.unwrap_or_default();
        for fragment in expected_fragments {
            assert!(
                error.contains(fragment),
                "{client_id}: missing {fragment:?} in {error:?}"
            );
        }
    }
}

#[tokio::test]
async fn runner_tool_unknown_client_returns_unknown_project_error() {
    // Project points at client "ghost" which never registered.
    let runtime = runtime_with_agent_project("ghost");
    let bootstrap = auth_context(None, true);
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunShell {
                project: agent_test_project_id("ghost"),
                command: "echo hi".to_string(),
                session_id: None,
                timeout_secs: None,
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&bootstrap),
        )
        .await;
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("unknown_project"), "{}", err);
    assert!(err.contains("ghost"), "{}", err);
    assert_eq!(result.output["error_kind"], "unknown_project");
    assert_eq!(result.output["project"], agent_test_project_id("ghost"));
}

#[tokio::test]
async fn agent_tool_authority_admission_matrix() {
    let runtime = runtime_with_agent_project("authority-agent");
    register_agent(
        &runtime,
        "authority-agent",
        Some("alice"),
        RunnerCapabilities {
            async_shell_jobs: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("authority-agent");
    let alice = auth_context(Some("alice"), false);
    let bob = auth_context(Some("bob"), false);
    let bootstrap = auth_context(None, true);
    let cases = [
        ("wrong owner", Some(&bob), false),
        ("missing auth", None, false),
        ("owner PAT", Some(&alice), true),
        ("bootstrap", Some(&bootstrap), true),
    ];

    for (label, auth, should_succeed) in cases {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::RunJob {
                    project: project.clone(),
                    command: "echo hi".to_string(),
                    session_id: None,
                    timeout_secs: None,
                    cwd: None,
                    purpose: None,
                    shell: None,
                },
                auth,
            )
            .await;
        assert_eq!(
            result.success, should_succeed,
            "{label}: {:?}",
            result.error
        );
        if should_succeed {
            assert!(
                result.output["job_id"].is_string(),
                "{label}: {}",
                result.output
            );
            continue;
        }
        let error = result.error.unwrap_or_default();
        if label == "wrong owner" {
            assert_eq!(
                result.output["error_kind"], "unknown_project",
                "{label}: {error}"
            );
            assert!(error.contains("unknown_project"), "{label}: {error}");
            assert!(!error.contains("owned by"), "{label}: {error}");
            assert!(!error.contains("belongs to"), "{label}: {error}");
        } else {
            assert!(error.contains("owned by alice"), "{label}: {error}");
            assert!(error.contains("belongs to anonymous"), "{label}: {error}");
        }
    }
}

#[test]
fn runtime_status_input_schema_exposes_compact_flags() {
    let specs = registered_tool_specs();
    let spec = specs
        .iter()
        .find(|spec| spec.name == "runtime_status")
        .expect("runtime_status spec");
    let properties = spec.input_schema["properties"]
        .as_object()
        .expect("runtime_status input properties");
    assert_eq!(properties["client_id"]["type"], "string");
    assert_eq!(properties["client_id"]["maxLength"], 128);
    for field in ["compact", "summary_only"] {
        assert!(
            properties.contains_key(field),
            "runtime_status input schema should expose {field}"
        );
        assert_eq!(properties[field]["type"], "boolean");
    }
    let required = spec.input_schema["required"]
        .as_array()
        .expect("runtime_status required fields");
    assert!(
        required.is_empty(),
        "runtime_status compact flags must stay optional: {required:?}"
    );

    let output_schema = crate::tool_runtime::registry::output_schema_for_tool("runtime_status");
    let agents_description = output_schema["properties"]["output"]["properties"]["agents"]
        ["description"]
        .as_str()
        .expect("runtime_status agents output description");
    assert!(agents_description.contains("stale_count"));
    assert!(!agents_description.contains("offline_count"));
    let runtime_exposure_description = output_schema["properties"]["output"]["properties"]
        ["runtime_exposure"]["description"]
        .as_str()
        .expect("runtime_status runtime_exposure output description");
    for exposure in [
        crate::model_surface::MODEL_SURFACE_LOCAL_CODING,
        crate::model_surface::MODEL_SURFACE_ADAPTIVE_RUNTIME,
        crate::model_surface::MODEL_SURFACE_FULL_OPERATOR_RUNTIME,
        crate::model_surface::RUNTIME_EXPOSURE_PROJECT_CONNECTOR,
    ] {
        assert!(
            runtime_exposure_description.contains(exposure),
            "runtime_status runtime_exposure output description missing {exposure}"
        );
    }
    assert!(!runtime_exposure_description.contains("canonical_connector"));
    let compact_schemas =
        &output_schema["properties"]["output"]["properties"]["mcp_compact_schemas"];
    assert_eq!(compact_schemas["type"], "boolean");
    assert!(compact_schemas["description"]
        .as_str()
        .is_some_and(|description| description.contains("omits outputSchema")));
    let effective_config = &output_schema["properties"]["output"]["properties"]["effective_config"];
    assert_eq!(effective_config["type"], "object");
    assert_eq!(effective_config["additionalProperties"], false);
    assert_eq!(
        effective_config["properties"]["auth"]["additionalProperties"],
        false
    );
    assert_eq!(
        effective_config["properties"]["tool_request_trace_mode"]["enum"],
        json!(["off", "metadata", "full"])
    );

    let openapi = crate::openapi::build_openapi_spec();
    let tool_call_properties = openapi["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .expect("ToolCallRequest properties");
    for field in ["compact", "summary_only"] {
        assert!(
            tool_call_properties.contains_key(field),
            "ToolCallRequest.properties should expose flattened runtime_status {field}"
        );
    }
}

#[test]
fn session_handoff_validation_exposure_keeps_read_only_metadata() {
    let metadata = crate::tool_runtime::metadata::lookup_tool_metadata("session_handoff_summary")
        .expect("session_handoff_summary metadata");
    assert_eq!(
        metadata.effect,
        crate::tool_runtime::metadata::ToolEffect::Observe
    );
    assert!(!metadata.destructive);
    assert!(!metadata.shell_like);
    assert_eq!(
        metadata.authority,
        crate::tool_runtime::metadata::ToolAuthorityPolicy::Require("runtime:read")
    );
}

#[test]
fn project_overview_metadata_schema_and_flattened_args_are_read_only() {
    let metadata = crate::tool_runtime::metadata::lookup_tool_metadata("project_overview")
        .expect("project_overview metadata");
    assert_eq!(metadata.provider_id, "agent");
    assert!(metadata.requires_project);
    assert_eq!(
        metadata.effect,
        crate::tool_runtime::metadata::ToolEffect::Observe
    );
    assert!(!metadata.destructive);
    assert!(!metadata.shell_like);
    assert_eq!(
        metadata.authority,
        crate::tool_runtime::metadata::ToolAuthorityPolicy::Require("project:read")
    );
    assert_eq!(tool_manifest_category("project_overview"), "project");

    let spec = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "project_overview")
        .expect("project_overview ToolSpec");
    let properties = spec.input_schema["properties"].as_object().unwrap();
    for field in ["project", "path", "max_depth", "limit", "session_id"] {
        assert!(
            properties.contains_key(field),
            "missing input field {field}"
        );
    }
    assert_eq!(spec.input_schema["additionalProperties"], false);
    let accepted = accepted_flattened_args_for_spec(&spec);
    for field in ["project", "path", "max_depth", "limit", "session_id"] {
        assert!(
            accepted.contains(&field.to_string()),
            "missing {field}: {accepted:?}"
        );
    }
    assert_eq!(spec.annotations["readOnlyHint"], true);
    assert_eq!(spec.annotations["destructiveHint"], false);
    assert_eq!(spec.annotations["idempotentHint"], true);
    assert_eq!(spec.annotations["openWorldHint"], false);

    let openapi = crate::openapi::build_openapi_spec();
    let action_properties = openapi["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .unwrap();
    for field in ["max_depth", "limit", "path", "project"] {
        assert!(
            action_properties.contains_key(field),
            "missing flattened {field}"
        );
    }
}

#[tokio::test]
async fn tool_manifest_keeps_list_compact_and_exact_contract_bounded() {
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
    assert_eq!(result.output["schema_version"], 1);
    assert!(result.output["count"].as_u64().unwrap() > 0);

    let tools = result.output["tools"].as_array().unwrap();
    assert!(
        tools.iter().all(|tool| tool["name"] != "start_coding_task"),
        "retired start_coding_task must stay out of the model manifest"
    );
    for tool in tools {
        assert!(
            tool.get("inputSchema").is_none() && tool.get("outputSchema").is_none(),
            "tool_manifest must stay compact: {tool:?}"
        );
        assert!(
            tool["accepted_flattened_args"].is_array(),
            "tool_manifest entry must expose accepted_flattened_args: {tool:?}"
        );
        assert_eq!(tool["deprecated_or_unsupported_args"], json!([]));
        let accepted = tool["accepted_flattened_args"].as_array().unwrap();
        let tool_name = tool["name"].as_str().unwrap_or("unknown");
        for &field in TOOL_CALL_EXPECTATION_METADATA_FIELDS {
            let advertised = accepted.iter().any(|value| value.as_str() == Some(field));
            let expected = match field {
                "assertion_name" => {
                    matches!(
                        tool_name,
                        "run_process" | "run_script" | "run_shell" | "run_job"
                    )
                }
                "result_expectation" => matches!(
                    tool_name,
                    "run_process"
                        | "run_script"
                        | "run_shell"
                        | "session_shell_exec"
                        | "cargo_fmt"
                        | "cargo_check"
                        | "cargo_test"
                        | "go_test"
                ),
                "accepted_exit_codes" => tool_name == "run_process",
                _ => false,
            };
            assert_eq!(
                advertised, expected,
                "{tool_name} manifest expectation-field exposure mismatch for {field}"
            );
        }
    }

    let exact = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: Some("run_script".to_string()),
            category: None,
            intent: None,
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(exact.success, "{:?}", exact.error);
    assert_eq!(exact.output["contract"]["name"], "run_script");
    assert_eq!(exact.output["contract"]["input_schema"]["type"], "object");
    assert!(exact.output["contract"].get("output_schema").is_none());

    let accepted = |name: &str| -> Vec<String> {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing manifest tool {name}"))["accepted_flattened_args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect()
    };

    for field in [
        "category",
        "intent",
        "include_recommended_flows",
        "include_risk_summary",
        TOOL_CALL_RECORDING_SESSION_ID_FIELD,
    ] {
        assert!(accepted("tool_manifest").contains(&field.to_string()));
    }
    for field in ["summary_only", "category", "features", "limit"] {
        assert!(accepted("list_tools").contains(&field.to_string()));
    }
    for field in ["compact", "summary_only"] {
        assert!(accepted("runtime_status").contains(&field.to_string()));
    }
    for field in [
        "project",
        "client_id",
        "path",
        "instruction",
        "include_project_instructions",
        "include_workflow_guidance",
        "session_id",
        TOOL_CALL_RECORDING_SESSION_ID_FIELD,
    ] {
        assert!(accepted("work_on_project").contains(&field.to_string()));
    }
    for field in [
        "session_id",
        "include_validation",
        "include_workspace",
        "include_checkpoints",
        "summary_only",
        "limit",
    ] {
        assert!(accepted("session_handoff_summary").contains(&field.to_string()));
    }
    for field in [
        "project",
        "session_id",
        "include_diff",
        "include_hygiene",
        "include_handoff",
        "include_workspace",
        "include_validation_summary",
        "summary_only",
    ] {
        assert!(accepted("finish_coding_task").contains(&field.to_string()));
    }
    for field in ["project", "path", "allow_missing", "session_id"] {
        assert!(accepted("read_project_artifact_metadata").contains(&field.to_string()));
    }
    assert!(!accepted("read_project_artifact_metadata")
        .contains(&"allow_cross_project_session".to_string()));
    for (tool, fields) in [
        (
            "artifact_upload_begin",
            vec![
                "project",
                "path",
                "expected_bytes",
                "expected_sha256",
                "mime_type",
                "overwrite",
                "session_id",
            ],
        ),
        (
            "artifact_upload_chunk",
            vec![
                "project",
                "path",
                "upload_id",
                "offset",
                "content_base64",
                "session_id",
            ],
        ),
        (
            "artifact_upload_finish",
            vec!["project", "path", "upload_id", "session_id"],
        ),
        (
            "artifact_upload_abort",
            vec!["project", "path", "upload_id", "session_id"],
        ),
        ("job_status", vec!["job_id", "include_command_preview"]),
    ] {
        let accepted = accepted(tool);
        for field in fields {
            assert!(
                accepted.contains(&field.to_string()),
                "{tool} missing accepted flattened arg {field}: {accepted:?}"
            );
        }
    }
}

#[tokio::test]
async fn tool_manifest_model_fields_and_hidden_start_compatibility_stay_separate() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ToolManifest {
            tool_name: None,
            category: None,
            intent: None,
            include_recommended_flows: false,
            include_risk_summary: false,
        })
        .await;
    assert!(result.success, "{:?}", result.error);

    let openapi = crate::openapi::build_openapi_spec();
    let properties = openapi["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .expect("ToolCallRequest properties");
    let tools = result.output["tools"]
        .as_array()
        .expect("tool_manifest tools");
    let mut accepted_fields = BTreeSet::new();

    for tool in tools {
        let tool_name = tool["name"].as_str().expect("tool name");
        let accepted = tool["accepted_flattened_args"]
            .as_array()
            .unwrap_or_else(|| panic!("{tool_name} accepted_flattened_args"));
        for field in accepted {
            let field = field
                .as_str()
                .unwrap_or_else(|| panic!("{tool_name} accepted field"));
            accepted_fields.insert(field.to_string());
            if TOOL_CALL_EXPECTATION_METADATA_FIELDS.contains(&field) {
                assert!(
                    !properties.contains_key(field),
                    "{tool_name} recorder metadata arg {field} must stay out of generic ToolCallRequest.properties"
                );
                continue;
            }
            assert!(
                properties.contains_key(field),
                "{tool_name} advertises flattened arg {field}, but ToolCallRequest.properties does not declare it"
            );
        }
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
            !accepted_fields.contains(field),
            "retired start-only field {field} must not be owned by the model-visible manifest"
        );
        assert!(
            !properties.contains_key(field),
            "retired start-only flattened arg {field} must not remain in ToolCallRequest"
        );
    }
    for field in ["mode", "base_ref", "execution_context"] {
        assert!(
            accepted_fields.contains(field),
            "current model-visible flattened field {field} must be owned by the manifest"
        );
        assert!(
            properties.contains_key(field),
            "current model-visible flattened field {field} must be declared by ToolCallRequest"
        );
    }

    for field in properties.keys() {
        if TOOL_CALL_WRAPPER_FIELDS.contains(&field.as_str()) {
            continue;
        }
        assert!(
            accepted_fields.contains(field),
            "ToolCallRequest.properties declares flattened field {field}, but no model-visible manifest entry accepts it"
        );
    }
}

#[tokio::test]
async fn bounded_list_tools_hides_schemas_and_finds_artifact_upload_tools() {
    let runtime = test_runtime();
    let full = runtime
        .dispatch(ToolCall::ListTools {
            category: None,
            features: None,
            summary_only: false,
            limit: None,
        })
        .await;
    assert!(full.success, "{:?}", full.error);

    let bounded = runtime
        .dispatch(ToolCall::ListTools {
            category: Some("artifact".to_string()),
            features: Some("artifact_upload".to_string()),
            summary_only: true,
            limit: Some(10),
        })
        .await;
    assert!(bounded.success, "{:?}", bounded.error);
    assert_eq!(bounded.output["total_count"], full.output["total_count"]);
    assert!(bounded.output["count"].as_u64().unwrap() > 0);
    assert_eq!(bounded.output["truncated"], false);
    let tools = bounded.output["tools"].as_array().unwrap();
    let names = bounded.output["names"].as_array().unwrap();
    for tool in [
        "artifact_upload_begin",
        "artifact_upload_chunk",
        "artifact_upload_finish",
        "artifact_upload_abort",
    ] {
        assert!(names.iter().any(|name| name == tool), "missing {tool}");
    }
    assert!(
        !names.iter().any(|name| name == "run_codex"),
        "bounded list_tools must not expose run_codex: {:?}",
        names
    );
    for tool in tools {
        assert!(tool["category"].as_str() == Some("artifact"), "{tool:?}");
        assert!(tool.get("inputSchema").is_none(), "{tool:?}");
        assert!(tool.get("outputSchema").is_none(), "{tool:?}");
    }

    let full_json = serde_json::to_string(&full.output).unwrap();
    let bounded_json = serde_json::to_string(&bounded.output).unwrap();
    assert!(
        bounded_json.len() < full_json.len() / 2,
        "bounded discovery should be substantially smaller than full list"
    );
}

#[tokio::test]
async fn bounded_list_tools_limit_reports_truncation() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::ListTools {
            category: None,
            features: Some("artifact_upload".to_string()),
            summary_only: true,
            limit: Some(2),
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["count"], 2);
    assert_eq!(result.output["filtered_count"], 4);
    assert_eq!(result.output["truncated"], true);
}

#[tokio::test]
async fn tool_manifest_recommends_default_remote_coding_loop() {
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

    let flows = result.output["recommended_flows"]
        .as_array()
        .expect("tool_manifest should include recommended_flows");
    for name in [
        "discovery",
        "inspect",
        "edit",
        "file_transfer",
        "validate",
        "review",
        "handoff",
    ] {
        assert!(
            flows.iter().any(|flow| flow["name"] == name),
            "recommended_flows should include {name}: {:?}",
            flows
        );
    }

    let serialized = result.output["recommended_flows"]
        .to_string()
        .to_lowercase();
    for tool in [
        "read_file",
        "search_project_text",
        "search_project_texts",
        "read_files",
        "import_conversation_files_to_project",
        "export_project_artifact",
        "show_changes",
        "apply_text_edits",
        "apply_unified_diff",
        "write_project_file",
        "cargo_check",
        "cargo_test",
        "git_diff_hunks",
        "workspace_hygiene_check",
        "session_summary",
        "session_handoff_summary",
    ] {
        assert!(
            serialized.contains(tool),
            "recommended_flows should mention {tool}: {serialized}"
        );
    }
    for tool in ["replace_line_range", "insert_at_line", "delete_line_range"] {
        assert!(
            !serialized.contains(tool),
            "recommended_flows should not rank retired edit tool {tool}: {serialized}"
        );
    }
    // The edit flow must expose only the canonical unified-diff mutation, never retired patch names.
    let edit_tools = result.output["recommended_flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|flow| flow["name"] == "edit")
        .expect("edit flow")["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(edit_tools.iter().any(|tool| tool == "apply_unified_diff"));
    assert!(edit_tools.iter().any(|tool| tool == "apply_patch"));
    for removed in ["apply_patch_checked", "validate_patch"] {
        assert!(
            !edit_tools.iter().any(|tool| tool == removed),
            "recommended edit flow must not rank retired patch tool {removed}: {edit_tools:?}"
        );
    }
    assert!(
        serialized.contains("run_shell")
            && serialized.contains("shell semantics or one tightly related observation goal")
            && serialized.contains("do not combine validation, commit, push, deploy, restart"),
        "recommended_flows should keep run_shell selection and effect-boundary guidance: {serialized}"
    );
}

#[tokio::test]
async fn runtime_status_with_no_projects_returns_configured_false() {
    let runtime = test_runtime();
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success, "{:?}", result.error);
    let out = &result.output;
    assert_eq!(out["service"], "webcodex");
    assert_eq!(out["version"], env!("CARGO_PKG_VERSION"));
    assert!(out["server_time"].is_i64());
    assert!(out["pid"].is_i64());
    assert_eq!(out["authority"]["mode"], "trusted_agent");
    assert_eq!(out["authority"]["human_approval_required"], false);
    assert_eq!(out["projects"]["mode"], "agent_registered");
    assert_eq!(out["projects"]["count"], 0);
    assert!(out["projects"].get("configured").is_none());
    assert!(out["projects"].get("config_path").is_none());
    assert!(out["projects"].get("load_error").is_none());
    assert!(out["projects"].get("server_static").is_none());
    assert_eq!(out["projects"]["agent_registered"]["count"], 0);
    assert_eq!(out["projects"]["agent_registered"]["online_count"], 0);
    assert_eq!(out["projects"]["effective"]["count"], 0);
    assert_eq!(out["projects"]["effective"]["status"], "no_projects");
}

#[tokio::test]
async fn runtime_status_uses_agent_projects_as_effective() {
    let runtime = test_runtime();
    let mut smoke = registered_project("webcodex-smoke", "/tmp/webcodex-smoke");
    smoke.git_branch = Some("main".to_string());
    smoke.git_head = Some("abc1234".to_string());
    smoke.git_dirty = Some(false);
    register_agent_with_projects(
        &runtime,
        "special",
        None,
        RunnerCapabilities {
            file_read: true,
            file_write: true,
            git: true,
            ..Default::default()
        },
        vec![smoke],
    )
    .await;

    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success, "{:?}", result.error);
    let projects = &result.output["projects"];
    assert_eq!(projects["mode"], "agent_registered");
    assert!(projects.get("server_static").is_none());
    assert!(projects.get("configured").is_none());
    assert!(projects.get("config_path").is_none());
    assert!(projects.get("load_error").is_none());
    assert_eq!(projects["agent_registered"]["count"], 1);
    assert_eq!(projects["agent_registered"]["online_count"], 1);
    assert_eq!(projects["effective"]["count"], 1);
    assert_eq!(projects["effective"]["status"], "ok");
    assert_eq!(projects["count"], 1);
}

#[tokio::test]
async fn runtime_status_includes_build_metadata() {
    let runtime =
        test_runtime().with_model_surface(crate::model_surface::ModelSurface::FullOperatorRuntime);
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["runtime_exposure"],
        crate::model_surface::MODEL_SURFACE_FULL_OPERATOR_RUNTIME
    );
    assert_eq!(
        result.output["mcp_compact_schemas"],
        crate::config::mcp_compact_schemas_enabled()
    );
    let build = &result.output["build"];
    assert!(build.is_object());
    assert!(build.get("git_commit").is_some());
    assert!(build.get("git_dirty").is_some());
    assert!(build.get("built_at").is_some());
}

#[tokio::test]
async fn runtime_status_preserves_allowlisted_effective_config_across_projections() {
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_SHARED_KEY_ENABLED", "true");
    env.set("WEBCODEX_ALLOW_ANONYMOUS", "true");
    env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");

    let runtime = runtime_with_info(RuntimeInfo {
        auth_enabled: true,
        configured_public_url: Some("https://runtime.example.com".to_string()),
        oauth2_enabled: true,
        oauth2_shared_key_bridge_enabled: true,
        ..RuntimeInfo::default()
    });
    register_agent(
        &runtime,
        "effective-config-agent",
        None,
        RunnerCapabilities::default(),
    )
    .await;

    let full = runtime.dispatch(runtime_status_call()).await;
    assert!(full.success, "{:?}", full.error);
    let config = &full.output["effective_config"];
    let config_object = config.as_object().expect("effective_config object");
    assert_eq!(
        config_object.len(),
        2,
        "effective_config must stay allowlisted"
    );
    assert_eq!(config["tool_request_trace_mode"], "full");
    let auth = config["auth"].as_object().expect("effective auth object");
    assert_eq!(auth.len(), 4, "effective auth facts must stay allowlisted");
    assert_eq!(auth["shared_key_enabled"], true);
    assert_eq!(auth["anonymous_enabled"], true);
    assert_eq!(auth["oauth2_enabled"], true);
    assert_eq!(auth["oauth2_shared_key_bridge_enabled"], true);

    let focused = runtime
        .dispatch(
            ToolCall::from_tool_name(
                "runtime_status",
                json!({"client_id": "effective-config-agent"}),
            )
            .unwrap(),
        )
        .await;
    assert!(focused.success, "{:?}", focused.error);
    assert_eq!(focused.output["effective_config"], *config);
    assert_eq!(focused.output["auth_enabled"], full.output["auth_enabled"]);
    assert_eq!(
        focused.output["configured_public_url"],
        full.output["configured_public_url"]
    );

    for arguments in [
        json!({"compact": true}),
        json!({"summary_only": true}),
        json!({"client_id": "effective-config-agent", "compact": true}),
    ] {
        let compact = runtime
            .dispatch(ToolCall::from_tool_name("runtime_status", arguments).unwrap())
            .await;
        assert!(compact.success, "{:?}", compact.error);
        assert_eq!(compact.output["effective_config"], *config);
        assert_eq!(compact.output["auth_enabled"], full.output["auth_enabled"]);
        assert_eq!(
            compact.output["configured_public_url"],
            full.output["configured_public_url"]
        );
    }
}

#[tokio::test]
async fn runtime_status_defaults_to_local_coding_surface() {
    // ToolRuntime::new_for_tests defaults to local_coding; keep this as a real
    // default-constructor check rather than overriding the value under test.
    let runtime = test_runtime();
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["runtime_exposure"],
        crate::model_surface::MODEL_SURFACE_LOCAL_CODING
    );
}

#[tokio::test]
async fn runtime_status_reports_project_connector_exposure_when_configured() {
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_SHARED_KEY_ENABLED", "true");
    env.set("WEBCODEX_ALLOW_ANONYMOUS", "true");
    let runtime = runtime_with_info(RuntimeInfo {
        auth_enabled: true,
        oauth2_enabled: true,
        oauth2_shared_key_bridge_enabled: true,
        ..RuntimeInfo::default()
    })
    .with_runtime_exposure(crate::model_surface::RuntimeExposure::ProjectConnector);
    let full = runtime.dispatch(runtime_status_call()).await;
    assert_eq!(
        full.output["runtime_exposure"],
        crate::model_surface::RUNTIME_EXPOSURE_PROJECT_CONNECTOR
    );
    assert_eq!(
        full.output["effective_config"]["auth"]["shared_key_enabled"],
        false
    );
    assert_eq!(
        full.output["effective_config"]["auth"]["anonymous_enabled"],
        false
    );
    assert_eq!(
        full.output["effective_config"]["auth"]["oauth2_enabled"],
        true
    );
    assert_eq!(
        full.output["effective_config"]["auth"]["oauth2_shared_key_bridge_enabled"],
        true
    );
    let compact = runtime
        .dispatch(ToolCall::from_tool_name("runtime_status", json!({"compact": true})).unwrap())
        .await;
    assert_eq!(
        compact.output["runtime_exposure"],
        crate::model_surface::RUNTIME_EXPOSURE_PROJECT_CONNECTOR
    );
    assert_eq!(
        compact.output["mcp_compact_schemas"],
        crate::config::mcp_compact_schemas_enabled()
    );
    assert_eq!(
        compact.output["effective_config"]["auth"]["oauth2_shared_key_bridge_enabled"],
        true
    );
}

// runtime_status reads the process-global compact-schema switch on each call.
// Serialize this async assertion with tests that mutate that switch so the
// value cannot change between dispatch and the matching expectation.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn runtime_status_compact_and_summary_only_return_sanitized_summary() {
    use crate::runner_protocol::{RunnerPolicySummary, ShellProfilesSummary};

    let _env = crate::test_support::TestEnvGuard::new();
    let runtime = test_runtime();
    let policy = RunnerPolicySummary {
        allowed_roots: vec![PathBuf::from(
            "/tmp/runtime-compact-allowed-root-never-emit",
        )],
        shell_profiles: Some(ShellProfilesSummary {
            default_dialect: None,
            available_dialects: None,
            default_profile: Some("rust".to_string()),
            configured_count: 1,
            prepared_cache_count: 0,
            profiles: vec![profile_summary_entry("rust", true, 3)],
        }),
        ..Default::default()
    };
    register_agent_with_shell_profiles(
        &runtime,
        "runtime-compact-status",
        Some(policy),
        vec![registered_project("demo", "/tmp/runtime-compact-demo")],
    )
    .await;

    for arguments in [json!({"compact": true}), json!({"summary_only": true})] {
        let result = runtime
            .dispatch(ToolCall::from_tool_name("runtime_status", arguments.clone()).unwrap())
            .await;
        assert!(result.success, "{:?}", result.error);
        let summary = &result.output;
        assert_eq!(summary["compact"], true, "arguments: {arguments}");
        assert_eq!(
            summary["mcp_compact_schemas"],
            crate::config::mcp_compact_schemas_enabled(),
            "arguments: {arguments}"
        );
        assert!(summary["effective_config"].is_object());
        assert_eq!(summary["auth_enabled"], false);
        assert!(summary["configured_public_url"].is_null());
        for pointer in [
            "/service",
            "/version",
            "/build/git_commit",
            "/build/git_dirty",
            "/tools/count",
            "/jobs/active_count",
            "/agents/count",
            "/agents/online_count",
            "/agents/stale_count",
            "/agents/summary/online",
            "/projects/effective/status",
            "/projects/effective/count",
            "/projects/agent_registered/count",
            "/projects/agent_registered/online_count",
            "/connection_layers/runner_process/status",
            "/connection_layers/server_transport/status",
            "/connection_layers/server_registration/status",
            "/connection_layers/project_registry/status",
            "/connection_layers/connector_endpoint/status",
            "/connection_layers/last_successful_tool_call/status",
        ] {
            assert!(
                summary.pointer(pointer).is_some(),
                "compact runtime_status should include {pointer}: {summary:?}"
            );
        }
        assert_eq!(summary["service"], "webcodex");
        assert_eq!(summary["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(summary["agents"]["summary"]["count"], 1);
        assert_eq!(summary["agents"]["summary"]["online"], 1);
        assert_eq!(summary["agents"]["count"], 1);
        assert_eq!(summary["agents"]["online_count"], 1);
        assert_eq!(summary["agents"]["stale_count"], 0);
        assert!(summary["agents"].get("offline_count").is_none());
        assert_eq!(summary["projects"]["effective"]["count"], 1);
        assert_eq!(summary["projects"]["effective"]["status"], "ok");
        assert!(summary["tools"].get("names").is_none());
        assert!(
            summary
                .pointer("/agents/clients/0/policy/allowed_roots")
                .is_none(),
            "compact runtime_status must not include full client policy"
        );
        assert!(
            summary
                .pointer("/agents/clients/0/shell_profiles")
                .is_none(),
            "compact runtime_status must not include shell profile details"
        );

        let serialized = serde_json::to_string(summary).unwrap();
        for forbidden in [
            "tools.names",
            "allowed_roots",
            "runtime-compact-allowed-root-never-emit",
            "shell_profiles",
            "stdout",
            "stderr",
            "command",
            "tail",
            "excerpt",
            "env",
            "token",
            "secret",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "compact runtime_status leaked {forbidden}: {serialized}"
            );
        }
        assert!(summary.pointer("/projects/server_static").is_none());
    }
}

#[tokio::test]
async fn runtime_status_does_not_expose_tokens_or_secrets() {
    let info = RuntimeInfo {
        auth_enabled: true,
        configured_public_url: Some("https://example.com".to_string()),
        oauth2_enabled: true,
        oauth2_shared_key_bridge_enabled: true,
        quic: Some(Arc::new(std::sync::Mutex::new(
            crate::config::QuicServerConfig::default().runtime_status(),
        ))),
    };
    let runtime = runtime_with_info(info);
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    let serialized = serde_json::to_string(&result.output).unwrap();
    // The summary must never contain secret-like field names.
    for forbidden in [
        "token",
        "WEBCODEX_TOKEN",
        "api_key",
        "apikey",
        "secret",
        "password",
        "authorization",
        "bearer",
    ] {
        assert!(
            !serialized
                .to_lowercase()
                .contains(&forbidden.to_lowercase()),
            "runtime_status output must not contain '{}': {}",
            forbidden,
            serialized
        );
    }
    // auth_enabled is a bool, not the token value.
    assert_eq!(result.output["auth_enabled"], true);
}

#[tokio::test]
async fn runtime_status_quic_disabled_is_non_sensitive() {
    let runtime = runtime_with_info(RuntimeInfo::default());
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    assert_eq!(result.output["quic"]["enabled"], false);
    assert_eq!(result.output["quic"]["listen"], "0.0.0.0:8443");
    assert_eq!(result.output["quic"]["alpn"], "webcodex-runner/1");
    assert_eq!(result.output["quic"]["listener_started"], false);
    assert!(result.output["quic"]["last_error"].is_null());
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(!serialized.contains("WEBCODEX_QUIC_CERT"));
    assert!(!serialized.contains("WEBCODEX_QUIC_KEY"));
    assert!(!serialized.to_ascii_lowercase().contains("token"));
}

#[tokio::test]
async fn runtime_status_quic_enabled_error_is_sanitized() {
    let quic_cfg = crate::config::QuicServerConfig {
        enabled: true,
        listen: "0.0.0.0:8443".to_string(),
        cert: PathBuf::from("/secret/certs/fullchain.pem"),
        key: PathBuf::from("/secret/certs/privkey.pem"),
        alpn: "webcodex-runner/1".to_string(),
    };
    let status = Arc::new(std::sync::Mutex::new(quic_cfg.runtime_status()));
    status
        .lock()
        .unwrap()
        .mark_error("WEBCODEX_QUIC_KEY path does not exist: /secret/certs/privkey.pem");
    let runtime = runtime_with_info(RuntimeInfo {
        auth_enabled: false,
        configured_public_url: None,
        quic: Some(status),
        oauth2_enabled: false,
        oauth2_shared_key_bridge_enabled: false,
    });
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    assert_eq!(result.output["quic"]["enabled"], true);
    assert_eq!(result.output["quic"]["listener_started"], false);
    assert_eq!(
        result.output["quic"]["last_error"],
        "WEBCODEX_QUIC_KEY path does not exist"
    );
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(!serialized.contains("/secret/certs"));
    assert!(!serialized.contains("privkey.pem"));
}

#[tokio::test]
async fn runtime_status_quic_started_reports_listen_and_alpn() {
    let quic_cfg = crate::config::QuicServerConfig {
        enabled: true,
        listen: "127.0.0.1:9443".to_string(),
        cert: PathBuf::from("/hidden/cert.pem"),
        key: PathBuf::from("/hidden/key.pem"),
        alpn: "webcodex-runner/1".to_string(),
    };
    let status = Arc::new(std::sync::Mutex::new(quic_cfg.runtime_status()));
    status.lock().unwrap().mark_started();
    let runtime = runtime_with_info(RuntimeInfo {
        auth_enabled: false,
        configured_public_url: None,
        quic: Some(status),
        oauth2_enabled: false,
        oauth2_shared_key_bridge_enabled: false,
    });
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    assert_eq!(result.output["quic"]["enabled"], true);
    assert_eq!(result.output["quic"]["listen"], "127.0.0.1:9443");
    assert_eq!(result.output["quic"]["alpn"], "webcodex-runner/1");
    assert_eq!(result.output["quic"]["listener_started"], true);
    assert!(result.output["quic"]["last_error"].is_null());
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(!serialized.contains("/hidden"));
}

#[tokio::test]
async fn runtime_status_auth_enabled_reflects_runtime_info() {
    let runtime = runtime_with_info(RuntimeInfo {
        auth_enabled: false,
        configured_public_url: None,
        oauth2_enabled: false,
        oauth2_shared_key_bridge_enabled: false,
        quic: Some(Arc::new(std::sync::Mutex::new(
            crate::config::QuicServerConfig::default().runtime_status(),
        ))),
    });
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    assert_eq!(result.output["auth_enabled"], false);
    assert!(result.output["configured_public_url"].is_null());

    let runtime = runtime_with_info(RuntimeInfo {
        auth_enabled: true,
        configured_public_url: Some("https://webcodex.example.com".to_string()),
        oauth2_enabled: true,
        oauth2_shared_key_bridge_enabled: true,
        quic: Some(Arc::new(std::sync::Mutex::new(
            crate::config::QuicServerConfig::default().runtime_status(),
        ))),
    });
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    assert_eq!(result.output["auth_enabled"], true);
    assert_eq!(
        result.output["configured_public_url"],
        "https://webcodex.example.com"
    );
}

#[test]
fn runtime_info_from_env_reads_effective_server_config() {
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_TOKEN", "token");
    env.set("WEBCODEX_PUBLIC_URL", "https://new.example.com");
    env.set("WEBCODEX_OAUTH2_ENABLED", "true");
    env.set("WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE", "true");

    let info = RuntimeInfo::from_env();
    assert!(info.auth_enabled);
    assert_eq!(
        info.configured_public_url.as_deref(),
        Some("https://new.example.com")
    );
    assert!(info.oauth2_enabled);
    assert!(info.oauth2_shared_key_bridge_enabled);

    env.set("WEBCODEX_OAUTH2_ENABLED", "false");
    let info = RuntimeInfo::from_env();
    assert!(!info.oauth2_enabled);
    assert!(!info.oauth2_shared_key_bridge_enabled);
}

#[tokio::test]
async fn runtime_status_agent_summary_includes_protocol_version() {
    let registry = Arc::new(RunnerRegistry::default());
    let mut registration = metadata_agent_registration("agent-1");
    registration.runner_instance_id = "inst".to_string();
    registration.job_concurrency_limit = Some(4);
    registration.display_name = Some("Workstation".to_string());
    registration.owner = Some("alice".to_string());
    registry.register(registration).await.unwrap();
    let runtime = ToolRuntime::new(registry, Arc::new(RuntimeInfo::default()));
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    let agents = &result.output["agents"];
    assert_eq!(agents["count"], 1);
    assert_eq!(agents["online_count"], 1);
    assert_eq!(agents["stale_count"], 0);
    assert!(agents.get("offline_count").is_none());
    assert_eq!(agents["summary"]["online"], 1);
    assert_eq!(agents["summary"]["offline"], 0);
    assert_eq!(agents["summary"]["stale"], 0);
    let clients = agents["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0]["client_id"], "agent-1");
    assert_eq!(
        clients[0]["agent_protocol_generation"],
        RUNNER_PROTOCOL_GENERATION_V2.get()
    );
    assert_eq!(clients[0]["transport"], "polling");
    assert_eq!(clients[0]["connected"], true);
    assert!(clients[0]["capabilities"].is_object());
    assert_eq!(clients[0]["projects_count"], 0);
    assert!(clients[0]["last_seen_age_secs"].is_i64());
    assert_eq!(clients[0]["pending_requests"], 0);
    assert_eq!(clients[0]["active_jobs"], 0);
    assert_eq!(
        clients[0]["job_concurrency"],
        json!({"limit": 4, "running": 0, "queued": 0})
    );
    let health_clients = agents["summary"]["clients"].as_array().unwrap();
    assert_eq!(health_clients.len(), 1);
    assert_eq!(health_clients[0]["client_id"], "agent-1");
    assert_eq!(health_clients[0]["status"], "online");
    assert_eq!(health_clients[0]["transport"], "polling");
    assert_eq!(health_clients[0]["projects_count"], 0);
    assert_eq!(health_clients[0]["pending_requests"], 0);
    assert_eq!(health_clients[0]["active_jobs"], 0);
    assert_eq!(
        health_clients[0]["job_concurrency"],
        json!({"limit": 4, "running": 0, "queued": 0})
    );
    // last_seen must be present as an integer unix timestamp (seconds).
    assert!(
        clients[0]["last_seen"].is_i64(),
        "last_seen must be an integer timestamp: {:?}",
        clients[0]["last_seen"]
    );
}

#[tokio::test]
async fn runtime_status_includes_sanitized_policy_summary() {
    use crate::runner_protocol::{
        ClaudeCodeProviderStatus, ProviderCallSummary, RunnerConfigReloadStatus,
        RunnerPolicySummary, ToolProvidersStatus,
    };
    let registry = Arc::new(RunnerRegistry::default());
    let mut registration = metadata_agent_registration("policy-agent");
    registration.runner_instance_id = "inst-p".to_string();
    registration.owner = Some("alice".to_string());
    registration.policy = Some(RunnerPolicySummary {
        allow_raw_shell: true,
        allow_cwd_anywhere: false,
        allowed_roots: vec![std::path::PathBuf::from("/root")],
        max_timeout_secs: 3600,
        max_output_bytes: 262144,
        shell_profiles: None,
        tool_providers: Some(ToolProvidersStatus {
            strategy: "claude_code".to_string(),
            claude_code: ClaudeCodeProviderStatus {
                enabled: true,
                version: Some("test-version".to_string()),
                available: true,
                process_state: "running".to_string(),
                discovered_tool_names: vec!["Edit".to_string()],
                capabilities: std::collections::BTreeMap::from([
                    ("search_project_text".to_string(), "unmapped".to_string()),
                    ("edit_file".to_string(), "available".to_string()),
                ]),
                last_error_code: None,
                last_call: None,
            },
            config_reload: RunnerConfigReloadStatus::default(),
        }),
        mcp_gateway_providers: None,
    });
    registry.register(registration).await.unwrap();
    let current_provider = ToolProvidersStatus {
        strategy: "claude_code".to_string(),
        claude_code: ClaudeCodeProviderStatus {
            enabled: true,
            version: Some("2.1.217".to_string()),
            available: true,
            process_state: "running".to_string(),
            discovered_tool_names: ["Edit", "Read", "Bash", "FutureTool"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            capabilities: std::collections::BTreeMap::from([
                ("search_project_text".to_string(), "unmapped".to_string()),
                ("edit_file".to_string(), "available".to_string()),
            ]),
            last_error_code: None,
            last_call: Some(ProviderCallSummary {
                capability: "edit_file".to_string(),
                selected_provider: "claude_code".to_string(),
                fallback_used: false,
                result: "success".to_string(),
                write_state: Some("confirmed".to_string()),
                duration_ms: 14,
                error_code: None,
            }),
        },
        config_reload: RunnerConfigReloadStatus {
            generation: 2,
            last_reload_result: "success".to_string(),
            ..RunnerConfigReloadStatus::default()
        },
    };
    registry
        .update_tool_providers("policy-agent", "inst-p", Some(current_provider))
        .await
        .unwrap();
    let runtime = ToolRuntime::new(registry, Arc::new(RuntimeInfo::default()));
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    let clients = result.output["agents"]["clients"].as_array().unwrap();
    let policy = &clients[0]["policy"];
    assert_eq!(policy["allow_raw_shell"], true);
    assert_eq!(policy["allow_cwd_anywhere"], false);
    assert_eq!(policy["allowed_roots"], json!(["/root"]));
    assert_eq!(policy["max_timeout_secs"], 3600);
    assert_eq!(policy["max_output_bytes"], 262144);
    let providers = &clients[0]["tool_providers"];
    assert_eq!(providers["strategy"], "claude_code");
    assert_eq!(providers["claude_code"]["process_state"], "running");
    assert_eq!(providers["claude_code"]["version"], "2.1.217");
    assert_eq!(providers["config_reload"]["generation"], 2);
    assert_eq!(
        providers["claude_code"]["last_call"]["selected_provider"],
        "claude_code"
    );
    assert_eq!(
        providers["claude_code"]["last_call"]["write_state"],
        "confirmed"
    );
    assert_eq!(
        providers["claude_code"]["capabilities"]["edit_file"],
        "available"
    );
    // Sanitization: never expose token/env/init_script.
    assert!(policy.get("token").is_none());
    assert!(policy.get("env").is_none());
    assert!(policy.get("init_script").is_none());

    let listed = runtime.dispatch(list_runners_call()).await;
    assert_eq!(
        listed.output["agents"][0]["tool_providers"]["claude_code"]["last_call"]["fallback_used"],
        false
    );
}

#[tokio::test]
async fn external_provider_discovery_cannot_change_public_tool_or_openapi_surface() {
    use crate::runner_protocol::{
        ClaudeCodeProviderStatus, RunnerPolicySummary, ToolProvidersStatus,
    };
    let before = crate::tool_runtime::registry::registered_tool_specs();
    let names_before = before
        .iter()
        .map(|spec| spec.name.clone())
        .collect::<BTreeSet<_>>();
    // Snapshot a model-visible tool's schema as the baseline that external
    // provider discovery must not perturb.
    let write_schema_before = before
        .iter()
        .find(|spec| spec.name == "write_project_file")
        .unwrap()
        .input_schema
        .clone();
    let registry = Arc::new(RunnerRegistry::default());
    let mut registration = metadata_agent_registration("provider-surface");
    registration.runner_instance_id = "inst-surface".to_string();
    registration.policy = Some(RunnerPolicySummary {
        tool_providers: Some(ToolProvidersStatus {
            strategy: "claude_code_then_native".to_string(),
            claude_code: ClaudeCodeProviderStatus {
                enabled: true,
                version: Some("2.1.217".to_string()),
                available: true,
                process_state: "running".to_string(),
                discovered_tool_names: ["Edit", "Read", "Bash", "Write", "FutureTool"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                capabilities: std::collections::BTreeMap::from([
                    ("edit_file".to_string(), "available".to_string()),
                    ("search_project_text".to_string(), "unmapped".to_string()),
                ]),
                last_error_code: None,
                last_call: None,
            },
            config_reload: Default::default(),
        }),
        ..RunnerPolicySummary::default()
    });
    registry.register(registration).await.unwrap();
    let runtime = ToolRuntime::new(registry, Arc::new(RuntimeInfo::default()));
    let status = runtime.dispatch(runtime_status_call()).await;
    let public_names = status.output["tools"]["names"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    for internal in ["Edit", "Read", "Bash", "Write", "FutureTool"] {
        assert!(!public_names.contains(internal));
    }

    let after = crate::tool_runtime::registry::registered_tool_specs();
    let names_after = after
        .iter()
        .map(|spec| spec.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(names_after, names_before);
    assert_eq!(
        after
            .iter()
            .find(|spec| spec.name == "write_project_file")
            .unwrap()
            .input_schema,
        write_schema_before
    );
    let openapi = crate::openapi::build_openapi_spec();
    let operation_count: usize = openapi["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|path| path.as_object().unwrap().len())
        .sum();
    assert_eq!(operation_count, 22);
}

#[tokio::test]
async fn runtime_status_policy_summary_is_null_for_older_agents() {
    let registry = Arc::new(RunnerRegistry::default());
    // Older agent: no policy field (None).
    let mut registration = metadata_agent_registration("legacy-agent");
    registration.runner_instance_id = "inst-l".to_string();
    registry.register(registration).await.unwrap();
    let runtime = ToolRuntime::new(registry, Arc::new(RuntimeInfo::default()));
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    let clients = result.output["agents"]["clients"].as_array().unwrap();
    // Older/minimal payload -> policy is null, not a fatal error.
    assert!(clients[0]["policy"].is_null());
    assert_eq!(
        clients[0]["job_concurrency"],
        json!({"limit": null, "running": 0, "queued": 0})
    );
}

#[tokio::test]
async fn computer_list_targets_is_minimal_capability_filtered_and_auth_scoped() {
    let runtime = test_runtime();
    let shared_a = shared_key_auth_context("computer-targets-a");
    let shared_b = shared_key_auth_context("computer-targets-b");

    register_computer_target_for_auth(
        &runtime,
        "a-accessibility",
        "Alice Accessibility",
        &shared_a,
        false,
        false,
        true,
    )
    .await;
    register_computer_target_for_auth(
        &runtime,
        "a-none",
        "Alice No Computer",
        &shared_a,
        false,
        false,
        false,
    )
    .await;
    register_computer_target_for_auth(
        &runtime,
        "a-region-only",
        "Alice Region Only",
        &shared_a,
        false,
        true,
        false,
    )
    .await;
    register_computer_target_for_auth(
        &runtime,
        "a-observe",
        "Alice Desktop",
        &shared_a,
        true,
        true,
        false,
    )
    .await;
    register_computer_target_for_auth(
        &runtime,
        "b-private",
        "Bob Private Desktop",
        &shared_b,
        true,
        true,
        true,
    )
    .await;
    register_application_target_for_auth(
        &runtime,
        "a-app-discovery",
        "Alice Applications",
        &shared_a,
        true,
        false,
    )
    .await;
    register_display_target_for_auth(&runtime, "a-display-only", "Alice Full Display", &shared_a)
        .await;
    register_pointer_target_for_auth(&runtime, "a-pointer-only", "Alice Pointer", &shared_a).await;
    register_clipboard_target_for_auth(
        &runtime,
        "a-clipboard-read",
        "Alice Clipboard Read",
        &shared_a,
        true,
        false,
    )
    .await;
    register_clipboard_target_for_auth(
        &runtime,
        "a-clipboard-write",
        "Alice Clipboard Write",
        &shared_a,
        false,
        true,
    )
    .await;

    register_application_target_for_auth(
        &runtime,
        "a-launch-only",
        "Alice Launch",
        &shared_a,
        false,
        true,
    )
    .await;

    let result = runtime
        .dispatch_with_auth(ToolCall::ComputerListTargets, Some(&shared_a))
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["count"], 8);
    assert_eq!(result.output["total_count"], 8);
    assert_eq!(result.output["truncated"], false);
    let targets = result.output["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 8);
    let target = |client_id: &str| {
        targets
            .iter()
            .find(|target| target["client_id"] == client_id)
            .unwrap()
    };
    let accessibility = target("a-accessibility");
    assert_eq!(accessibility["display_name"], "Alice Accessibility");
    assert_eq!(accessibility["connected"], true);
    assert_eq!(accessibility["capabilities"]["computer_observe"], false);
    assert_eq!(
        accessibility["capabilities"]["computer_application_discovery"],
        false
    );
    assert_eq!(
        accessibility["capabilities"]["computer_application_launch"],
        false
    );
    assert_eq!(
        accessibility["capabilities"]["computer_snapshot_region"],
        false
    );
    assert_eq!(
        accessibility["capabilities"]["computer_accessibility_observe"],
        true
    );
    let observe = target("a-observe");
    assert_eq!(observe["capabilities"]["computer_observe"], true);
    assert_eq!(
        observe["capabilities"]["computer_application_discovery"],
        false
    );
    assert_eq!(
        observe["capabilities"]["computer_application_launch"],
        false
    );
    assert_eq!(observe["capabilities"]["computer_snapshot_region"], true);
    let discovery = target("a-app-discovery");
    assert_eq!(
        discovery["capabilities"]["computer_application_discovery"],
        true
    );
    assert_eq!(
        discovery["capabilities"]["computer_application_launch"],
        false
    );
    assert_eq!(discovery["capabilities"]["computer_observe"], false);
    let launch = target("a-launch-only");
    assert_eq!(
        launch["capabilities"]["computer_application_discovery"],
        false
    );
    assert_eq!(launch["capabilities"]["computer_application_launch"], true);
    assert_eq!(launch["capabilities"]["computer_observe"], false);
    assert_eq!(launch["capabilities"]["computer_display_observe"], false);
    let display = target("a-display-only");
    assert_eq!(display["capabilities"]["computer_display_observe"], true);
    assert_eq!(display["capabilities"]["computer_observe"], false);
    assert_eq!(display["capabilities"]["computer_snapshot_region"], false);
    assert_eq!(display["capabilities"]["computer_pointer_control"], false);
    assert_eq!(
        display["capabilities"]["computer_application_discovery"],
        false
    );
    assert_eq!(
        display["capabilities"]["computer_application_launch"],
        false
    );
    let pointer = target("a-pointer-only");
    assert_eq!(pointer["capabilities"]["computer_pointer_control"], true);
    assert_eq!(pointer["capabilities"]["computer_display_observe"], false);
    assert_eq!(pointer["capabilities"]["computer_clipboard_read"], false);
    assert_eq!(pointer["capabilities"]["computer_clipboard_write"], false);
    let clipboard_read = target("a-clipboard-read");
    assert_eq!(
        clipboard_read["capabilities"]["computer_clipboard_read"],
        true
    );
    assert_eq!(
        clipboard_read["capabilities"]["computer_clipboard_write"],
        false
    );
    assert_eq!(
        clipboard_read["capabilities"]["computer_pointer_control"],
        false
    );
    assert_eq!(clipboard_read["capabilities"]["computer_observe"], false);
    let clipboard_write = target("a-clipboard-write");
    assert_eq!(
        clipboard_write["capabilities"]["computer_clipboard_write"],
        true
    );
    assert_eq!(
        clipboard_write["capabilities"]["computer_clipboard_read"],
        false
    );
    assert_eq!(
        clipboard_write["capabilities"]["computer_display_observe"],
        false
    );

    for target in targets {
        let object = target.as_object().unwrap();
        assert_eq!(
            object.len(),
            4,
            "target projection must stay minimal: {object:?}"
        );
        for forbidden in [
            "owner",
            "hostname",
            "projects",
            "policy",
            "pending_requests",
            "transport",
            "active_jobs",
            "job_concurrency",
            "tool_providers",
        ] {
            assert!(
                !object.contains_key(forbidden),
                "Computer target projection leaked {forbidden}: {target}"
            );
        }
    }
    let serialized = result.output.to_string();
    assert!(!serialized.contains("a-none"));
    assert!(!serialized.contains("a-region-only"));
    assert!(!serialized.contains("Bob Private Desktop"));
    assert!(!serialized.contains("b-private"));
    assert!(!serialized.contains("/tmp/private"));
}

#[tokio::test]
async fn list_runners_includes_sanitized_policy_summary() {
    use crate::runner_protocol::RunnerPolicySummary;
    let registry = Arc::new(RunnerRegistry::default());
    let mut registration = metadata_agent_registration("list-policy-agent");
    registration.runner_instance_id = "inst-lp".to_string();
    registration.job_concurrency_limit = Some(8);
    registration.owner = Some("alice".to_string());
    registration.policy = Some(RunnerPolicySummary {
        allow_raw_shell: false,
        allow_cwd_anywhere: true,
        allowed_roots: vec![],
        max_timeout_secs: 120,
        max_output_bytes: 4096,
        shell_profiles: None,
        tool_providers: None,
        mcp_gateway_providers: None,
    });
    registry.register(registration).await.unwrap();
    let runtime = ToolRuntime::new(registry, Arc::new(RuntimeInfo::default()));
    let result = runtime.dispatch(list_runners_call()).await;
    assert!(result.success);
    assert_eq!(result.output["count"], 1);
    assert_eq!(result.output["summary"]["online"], 1);
    assert_eq!(result.output["summary"]["offline"], 0);
    assert_eq!(result.output["summary"]["stale"], 0);
    let health_clients = result.output["summary"]["clients"].as_array().unwrap();
    assert_eq!(health_clients.len(), 1);
    assert_eq!(health_clients[0]["client_id"], "list-policy-agent");
    assert_eq!(health_clients[0]["transport"], "polling");
    assert_eq!(health_clients[0]["pending_requests"], 0);
    assert_eq!(health_clients[0]["active_jobs"], 0);
    assert_eq!(
        health_clients[0]["job_concurrency"],
        json!({"limit": 8, "running": 0, "queued": 0})
    );
    let agents = result.output["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["projects_count"], 0);
    assert!(agents[0]["last_seen_age_secs"].is_i64());
    assert_eq!(agents[0]["active_jobs"], 0);
    assert_eq!(
        agents[0]["job_concurrency"],
        json!({"limit": 8, "running": 0, "queued": 0})
    );
    let policy = &agents[0]["policy"];
    assert_eq!(policy["allow_raw_shell"], false);
    assert_eq!(policy["allow_cwd_anywhere"], true);
    assert_eq!(policy["max_timeout_secs"], 120);
    assert_eq!(policy["max_output_bytes"], 4096);
    // No secret fields leak through list_runners either.
    assert!(policy.get("token").is_none());
    assert!(policy.get("env").is_none());
    assert!(policy.get("init_script").is_none());
}

#[tokio::test]
async fn runtime_status_distinguishes_stale_registration_from_transport_connection() {
    use crate::runner_http::RunnerTransport;
    let registry = Arc::new(RunnerRegistry::default());
    let mut registration = metadata_agent_registration("ws-stale");
    registration.runner_instance_id = "inst".to_string();
    registration.display_name = Some("Stale WS".to_string());
    registration.owner = Some("alice".to_string());
    registry.register(registration).await.unwrap();
    registry
        .set_transport("ws-stale", RunnerTransport::WebSocket)
        .await
        .unwrap();
    // Force the agent past the 60s online window so it reads as stale.
    let stale_ts = chrono::Utc::now().timestamp() - 120;
    registry.set_last_seen_for_test("ws-stale", stale_ts).await;

    let runtime = ToolRuntime::new(registry, Arc::new(RuntimeInfo::default()));
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    let agents = &result.output["agents"];
    assert_eq!(agents["count"], 1);
    assert_eq!(agents["online_count"], 0);
    assert_eq!(agents["stale_count"], 1);
    assert!(agents.get("offline_count").is_none());
    let entry = &agents["clients"][0];
    assert_eq!(entry["client_id"], "ws-stale");
    assert_eq!(entry["transport"], "websocket");
    assert_eq!(entry["status"], "stale");
    assert_eq!(entry["connected"], false);
    assert_eq!(entry["last_seen"], stale_ts);
    let layers = &result.output["connection_layers"];
    assert_eq!(layers["runner_process"]["status"], "stale");
    assert_eq!(layers["runner_process"]["reason_code"], "heartbeat_expired");
    assert_eq!(layers["server_transport"]["status"], "disconnected");
    assert_eq!(layers["server_registration"]["status"], "stale");
    assert_eq!(
        layers["server_registration"]["reason_code"],
        "registration_instance_disconnected"
    );
    assert_eq!(layers["project_registry"]["status"], "not_configured");
    assert_eq!(layers["connector_endpoint"]["status"], "not_configured");
    assert_eq!(
        layers["connector_endpoint"]["reason_code"],
        "connector_runtime_disabled"
    );
    assert!(layers.get("session_binding").is_none());
}

#[tokio::test]
async fn runtime_status_reflects_websocket_transport_label() {
    let registry = Arc::new(RunnerRegistry::default());
    let runtime = ToolRuntime::new(registry.clone(), Arc::new(RuntimeInfo::default()));
    let mut registration = metadata_agent_registration("ws-agent");
    registration.runner_instance_id = "inst".to_string();
    registration.owner = Some("alice".to_string());
    registry.register(registration).await.unwrap();
    // Simulate the authoritative WebSocket ingress without changing the raw
    // announced compatibility label.
    registry
        .set_transport("ws-agent", crate::runner_http::RunnerTransport::WebSocket)
        .await
        .unwrap();

    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    let clients = &result.output["agents"]["clients"];
    let entry = clients
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["client_id"] == "ws-agent")
        .expect("ws-agent present");
    assert_eq!(entry["transport"], "websocket");
    assert_eq!(
        entry["agent_protocol_generation"],
        RUNNER_PROTOCOL_GENERATION_V2.get()
    );
}

#[tokio::test]
async fn runtime_status_tools_summary_lists_names() {
    let runtime = test_runtime();
    let result = runtime.dispatch(runtime_status_call()).await;
    assert!(result.success);
    let tools = &result.output["tools"];
    let names = tools["names"].as_array().unwrap();
    assert!(!names.is_empty());
    assert!(
        names.iter().any(|n| n == "runtime_status"),
        "tools.names must include runtime_status: {:?}",
        names
    );
    assert!(
        !names.iter().any(|n| n == "run_codex"),
        "runtime_status tools.names must not include removed run_codex: {:?}",
        names
    );
    assert_eq!(tools["count"], names.len() as i64);
}
