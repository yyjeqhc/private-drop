use crate::capabilities::RunnerFeatureInference;
use crate::projects::RunnerLookupError;
use crate::protocol::AcceptedRunnerProtocol;
use crate::registry::{MAX_SHARED_KEY_RUNNERS_PER_GROUP, SHARED_KEY_OFFLINE_TTL_SECS};
use crate::validation::{validate_file_request, validate_run_request, MAX_RUN_STDIN_BYTES};
use crate::*;
use std::sync::Arc;
use tokio::sync::Notify;
use webcodex_core::runner_protocol::*;

fn auth_context(username: Option<&str>, is_bootstrap: bool) -> RunnerAccess {
    RunnerAccess {
        global_visibility: is_bootstrap,
        owner_bypass: is_bootstrap,
        username: username.map(str::to_string),
        group: None,
    }
}

fn shared_key_access(group: &str) -> RunnerAccess {
    RunnerAccess {
        global_visibility: false,
        owner_bypass: false,
        username: None,
        group: Some(RunnerAccessGroup::SharedKey(format!(
            "test-shared-key-hash:{group}"
        ))),
    }
}

fn agent_auth_context(
    username: &str,
    _allowed_client_id: &str,
    _scopes: Vec<&str>,
) -> RunnerAccess {
    RunnerAccess {
        global_visibility: false,
        owner_bypass: false,
        username: Some(username.to_string()),
        group: None,
    }
}

fn project_summary(id: &str, path: &str) -> RunnerProjectSummary {
    RunnerProjectSummary {
        id: id.to_string(),
        name: Some(id.to_string()),
        path: path.to_string(),
        allow_patch: true,
        kind: Some("rust".to_string()),
        registration_source: None,
        description: Some("test project".to_string()),
        hooks: vec!["doctor".to_string(), "precommit".to_string()],
        disabled: false,
        revision: None,
        git_branch: Some("codex".to_string()),
        git_head: Some("9a7d3ce".to_string()),
        git_dirty: Some(false),
        updated_at: 123456,
        shell_profile: None,
    }
}

fn runner_registration(
    client_id: &str,
    runner_instance_id: &str,
    _projects: Vec<RunnerProjectSummary>,
) -> RunnerRegisterRequest {
    RunnerRegisterRequest {
        process_started_at: None,
        build: None,
        job_concurrency_limit: None,
        job_inventory: None,
        coding_agent_providers: None,
        coding_agent_inventory: None,
        client_id: client_id.to_string(),
        runner_instance_id: runner_instance_id.to_string(),
        runner_protocol_generation: RUNNER_PROTOCOL_GENERATION_V2,
        display_name: None,
        owner: None,
        hostname: None,
        host_context: None,
        capabilities: async_job_capabilities(),
        policy: None,
    }
}

fn v2_baseline_capabilities() -> RunnerCapabilities {
    let mut value = serde_json::Map::new();
    value.insert("shell".to_string(), serde_json::Value::Bool(false));
    for capability in RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES {
        value.insert((*capability).to_string(), serde_json::Value::Bool(true));
    }
    serde_json::from_value(serde_json::Value::Object(value)).unwrap()
}

fn current_runner_registration(registration: RunnerRegisterRequest) -> RunnerRegisterRequest {
    crate::test_support::current_runner_registration(registration)
}

fn async_job_capabilities() -> RunnerCapabilities {
    let mut capabilities = v2_baseline_capabilities();
    capabilities.shell = true;
    capabilities
}

fn file_request(op: &str) -> ShellFileOpRequest {
    ShellFileOpRequest {
        op: op.to_string(),
        client_id: "oe".to_string(),
        path: "src/auth/scopes.rs".to_string(),
        cwd: Some("/root/git/webcodex".to_string()),
        content: None,
        max_bytes: None,
        old_text: None,
        pattern: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: None,
        end_line: None,
        line: None,
        create_dirs: false,
        wait_timeout_secs: 0,
    }
}

async fn register_computer_test_runner(
    registry: &RunnerRegistry,
    client_id: &str,
    owner: &str,
    observe_capable: bool,
    accessibility_capable: bool,
    control_capable: bool,
    text_input_capable: bool,
) {
    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: "computer-inst".to_string(),
            runner_protocol_generation: RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some(owner.to_string()),
            hostname: None,
            host_context: None,
            capabilities: RunnerCapabilities {
                shell: true,
                file_read: true,
                computer_observe: observe_capable,
                computer_accessibility_observe: accessibility_capable,
                computer_control: control_capable,
                computer_window_activate: false,
                computer_text_input: text_input_capable,
                ..Default::default()
            },
            policy: None,
        }))
        .await
        .unwrap();
}

async fn register_quic_v1_runner(registry: &RunnerRegistry, client_id: &str) {
    registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: async_job_capabilities(),
            policy: None,
        })
        .await
        .unwrap();
    registry
        .set_transport(client_id, RunnerTransport::Quic)
        .await
        .unwrap();
}

async fn register_instance_with_capabilities(
    registry: &RunnerRegistry,
    client_id: &str,
    instance: &str,
    capabilities: RunnerCapabilities,
) -> Result<RunnerView, String> {
    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: instance.to_string(),
            runner_protocol_generation: RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities,
            policy: None,
        }))
        .await
}

async fn assert_structured_delete_runner_idle(registry: &RunnerRegistry, client_id: &str) {
    let inner = registry.inner.lock().await;
    assert!(inner
        .queues_by_runner
        .get(client_id)
        .is_none_or(|queue| queue.is_empty()));
    assert!(inner
        .pending_by_id
        .values()
        .all(|pending| pending.request.client_id != client_id));
}

async fn register_with_instance(
    registry: &RunnerRegistry,
    client_id: &str,
    instance: &str,
) -> RunnerView {
    registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: instance.to_string(),
            runner_protocol_generation: RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            host_context: None,
            capabilities: async_job_capabilities(),
            policy: None,
        })
        .await
        .unwrap()
}

#[test]
fn runner_owner_access_projection_enforces_owner_boundary() {
    use crate::access_control::assert_runner_owner;

    let bootstrap = auth_context(None, true);
    assert!(assert_runner_owner(Some(&bootstrap), "client-1", None).is_ok());

    let alice = auth_context(Some("alice"), false);
    assert!(assert_runner_owner(Some(&alice), "client-1", Some("alice")).is_ok());

    let non_bootstrap_admin = RunnerAccess {
        global_visibility: true,
        owner_bypass: false,
        username: Some("alice".to_string()),
        group: None,
    };
    let admin_mismatch =
        assert_runner_owner(Some(&non_bootstrap_admin), "client-1", Some("bob")).unwrap_err();
    assert!(admin_mismatch.contains("owned by bob"));
    assert!(admin_mismatch.contains("belongs to alice"));

    let mismatch = assert_runner_owner(Some(&alice), "client-1", Some("bob")).unwrap_err();
    assert!(mismatch.contains("owned by bob"));
    assert!(mismatch.contains("belongs to alice"));

    let missing = assert_runner_owner(Some(&alice), "client-1", None).unwrap_err();
    assert_eq!(missing, "runner client-1 has no owner");

    let anonymous = assert_runner_owner(None, "client-1", Some("anonymous")).unwrap_err();
    assert!(anonymous.contains("belongs to anonymous"));
}

#[path = "tests/abandoned_sync.rs"]
mod abandoned_sync;
#[path = "tests/apply_patch.rs"]
mod apply_patch;
#[path = "tests/apply_text_edit_line_scope.rs"]
mod apply_text_edit_line_scope;
#[path = "tests/apply_text_edit_occurrence.rs"]
mod apply_text_edit_occurrence;
#[path = "tests/artifact_export.rs"]
mod artifact_export;
#[path = "tests/capabilities.rs"]
mod capabilities;
#[path = "tests/computer_accessibility.rs"]
mod computer_accessibility;
#[path = "tests/computer_control.rs"]
mod computer_control;
#[path = "tests/computer_observe.rs"]
mod computer_observe;
#[path = "tests/computer_snapshot_artifact.rs"]
mod computer_snapshot_artifact;
#[path = "tests/computer_text_input.rs"]
mod computer_text_input;
#[path = "tests/configured_skills.rs"]
mod configured_skills;
#[path = "tests/connection_lease.rs"]
mod connection_lease;
#[path = "tests/disconnect_reconciliation.rs"]
mod disconnect_reconciliation;
#[path = "tests/file_validation.rs"]
mod file_validation;
#[path = "tests/instance_lease.rs"]
mod instance_lease;
#[path = "tests/internal_posix.rs"]
mod internal_posix;
#[path = "tests/job_lifecycle.rs"]
mod job_lifecycle;
#[path = "tests/job_log_wait.rs"]
mod job_log_wait;
#[path = "tests/lsp.rs"]
mod lsp;
#[path = "tests/mcp_gateway.rs"]
mod mcp_gateway;
#[path = "tests/plugin_gateway.rs"]
mod plugin_gateway;
#[path = "tests/polling.rs"]
mod polling;
#[path = "tests/project_inventory.rs"]
mod project_inventory;
#[path = "tests/project_projection.rs"]
mod project_projection;
#[path = "tests/project_unregister.rs"]
mod project_unregister;
#[path = "tests/protocol.rs"]
mod protocol;
#[path = "tests/queue_admission.rs"]
mod queue_admission;
#[path = "tests/quic_queueing.rs"]
mod quic_queueing;
#[path = "tests/raw_shell.rs"]
mod raw_shell;
#[path = "tests/registration_projection.rs"]
mod registration_projection;
#[path = "tests/run_enqueue.rs"]
mod run_enqueue;
#[path = "tests/runner_config.rs"]
mod runner_config;
#[path = "tests/runner_liveness.rs"]
mod runner_liveness;
#[path = "tests/shared_key_limits.rs"]
mod shared_key_limits;
#[path = "tests/shared_key_ttl.rs"]
mod shared_key_ttl;
#[path = "tests/skill_store.rs"]
mod skill_store;
#[path = "tests/ssh_resource.rs"]
mod ssh_resource;
#[path = "tests/structured_file_delete.rs"]
mod structured_file_delete;
