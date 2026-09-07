//! Cross-process continuity tests: runner disconnect/reconnect, explicit Workflow
//! Session recovery, stale registration semantics, meaningful-activity scoping,
//! and mixed-version diagnostics.

use super::support::*;
use crate::auth::AuthContext;
use crate::client_window::ClientWindow;
use crate::runner_http::RunnerTransport;
use crate::runner_protocol::{
    RunnerBuildInfo, RunnerCapabilities, RunnerHostContext, RunnerProtocolGenerationNumber,
    RunnerRegisterRequest, ShellJobOpRequest, RUNNER_PROTOCOL_GENERATION_V2,
};
use crate::tool_runtime::observations::ToolCallObservation;
use crate::tool_runtime::tool_inputs::{SessionMode, StartupDetail};
use crate::tool_runtime::{ToolCall, ToolRuntime};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Notify;

fn persisted_session_authority_fingerprint(
    ledger: &std::path::Path,
    session_id: &str,
) -> Option<String> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(ledger).unwrap()).unwrap();
    value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["session_id"] == session_id)
        .and_then(|session| session["owner_authority_fingerprint"].as_str())
        .map(str::to_string)
}

fn rewrite_persisted_session_with_malformed_authority(ledger: &std::path::Path, session_id: &str) {
    let mut value: Value = serde_json::from_str(&std::fs::read_to_string(ledger).unwrap()).unwrap();
    let session = value["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|session| session["session_id"] == session_id)
        .expect("persisted Session");
    session["owner_authority_fingerprint"] = Value::String("not-a-valid-fingerprint".to_string());
    std::fs::write(ledger, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn register_request(
    client_id: &str,
    instance: &str,
    process_started_at: Option<i64>,
    build: Option<RunnerBuildInfo>,
) -> RunnerRegisterRequest {
    crate::test_support::current_runner_registration(RunnerRegisterRequest {
        client_id: client_id.to_string(),
        runner_instance_id: instance.to_string(),
        runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
        display_name: None,
        owner: None,
        hostname: None,
        host_context: None,
        capabilities: RunnerCapabilities {
            shell: true,
            git: true,
            file_read: true,
            file_write: true,
            jobs: true,
            async_jobs: true,
            async_shell_jobs: true,
            ..Default::default()
        },
        policy: None,
        process_started_at,
        build,
        job_concurrency_limit: None,
        job_inventory: None,
        coding_agent_providers: None,
        coding_agent_inventory: None,
    })
}

async fn register_with_project(
    runtime: &ToolRuntime,
    client_id: &str,
    instance: &str,
    process_started_at: Option<i64>,
    build: Option<RunnerBuildInfo>,
) {
    runtime
        .runner_registry
        .register(register_request(
            client_id,
            instance,
            process_started_at,
            build,
        ))
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        instance,
        vec![registered_project("proj", "/tmp/reconnect-proj")],
    )
    .await;
}

async fn layers(runtime: &ToolRuntime) -> Value {
    let status = runtime.runtime_status(None).await;
    assert!(status.success);
    status.output["connection_layers"].clone()
}

fn assert_layer_contract(layer: &Value, context: &str) {
    for field in [
        "status",
        "observed_at",
        "source",
        "age_secs",
        "stale_after_secs",
        "reason_code",
    ] {
        assert!(
            layer.get(field).is_some(),
            "{context} layer missing contract field {field}: {layer}"
        );
    }
}

#[tokio::test]
async fn runner_disconnect_and_reconnect_change_layers_independently() {
    let runtime = test_runtime();
    register_with_project(&runtime, "rc-agent", "inst-a", Some(1_000), None).await;

    // Connected: every runner-derived layer is a real observation.
    let connected = layers(&runtime).await;
    for name in [
        "runner_process",
        "server_transport",
        "server_registration",
        "project_registry",
        "connector_endpoint",
        "last_successful_tool_call",
    ] {
        assert_layer_contract(&connected[name], name);
    }
    assert_eq!(connected["runner_process"]["status"], "ready");
    assert_eq!(
        connected["runner_process"]["source"],
        "runner_process_report"
    );
    assert_eq!(connected["runner_process"]["process_started_at"], 1_000);
    assert_eq!(connected["server_transport"]["status"], "connected");
    assert_eq!(
        connected["server_transport"]["connection_instance"],
        "inst-a"
    );
    assert_eq!(connected["server_registration"]["status"], "registered");
    assert_eq!(connected["project_registry"]["status"], "registered");
    // Connector runtime is not configured in this process.
    assert_eq!(connected["connector_endpoint"]["status"], "not_configured");
    assert_eq!(
        connected["connector_endpoint"]["reason_code"],
        "connector_runtime_disabled"
    );

    // Disconnect: layers change independently; stale registration is not ready.
    runtime
        .runner_registry
        .reconcile_disconnect("rc-agent", "inst-a")
        .await;
    let disconnected = layers(&runtime).await;
    assert_eq!(disconnected["runner_process"]["status"], "stale");
    assert_eq!(
        disconnected["runner_process"]["reason_code"],
        "heartbeat_expired"
    );
    assert_eq!(disconnected["server_transport"]["status"], "disconnected");
    assert!(disconnected["server_transport"]["disconnected_at"].is_i64());
    assert_eq!(disconnected["server_registration"]["status"], "stale");
    assert_eq!(
        disconnected["server_registration"]["reason_code"],
        "registration_instance_disconnected"
    );
    assert_eq!(disconnected["project_registry"]["status"], "stale");
    assert_eq!(
        disconnected["project_registry"]["reason_code"],
        "providing_runner_disconnected"
    );

    // Reconnect with a NEW process instance: new connection replaces the old
    // state, the project re-registers, and no server restart was needed.
    register_with_project(&runtime, "rc-agent", "inst-b", Some(2_000), None).await;
    let reconnected = layers(&runtime).await;
    assert_eq!(reconnected["runner_process"]["status"], "ready");
    assert_eq!(reconnected["runner_process"]["process_started_at"], 2_000);
    assert_eq!(
        reconnected["server_transport"]["connection_instance"],
        "inst-b"
    );
    assert_eq!(reconnected["server_transport"]["status"], "connected");
    assert_eq!(reconnected["server_registration"]["status"], "registered");
    assert_eq!(
        reconnected["server_registration"]["runner_instance"],
        "inst-b"
    );
    assert_eq!(reconnected["project_registry"]["status"], "registered");

    // Calls recover: a dispatched shell tool reaches the new instance.
    let project = crate::tool_runtime::runner_project_runtime_id("rc-agent", "proj");
    let run = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "echo back".to_string(),
                        session_id: None,
                        timeout_secs: Some(5),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_instance(&runtime, "rc-agent", "inst-b").await;
    complete_patch_agent_request_for_instance(
        &runtime,
        "rc-agent",
        "inst-b",
        &req.request_id,
        0,
        "back\n",
        "",
    )
    .await;
    let response = run.await.unwrap();
    assert!(response.success, "{:?}", response.error);
}

#[tokio::test]
async fn stale_heartbeat_without_disconnect_is_not_ready() {
    let runtime = test_runtime();
    register_with_project(&runtime, "stale-agent", "inst-a", None, None).await;
    runtime
        .runner_registry
        .set_last_seen_for_test("stale-agent", chrono::Utc::now().timestamp() - 3600)
        .await;
    let stale = layers(&runtime).await;
    assert_eq!(stale["runner_process"]["status"], "stale");
    assert_eq!(stale["server_registration"]["status"], "stale");
    assert_eq!(stale["project_registry"]["status"], "stale");
    // Stale must never be projected as ready/connected.
    assert_ne!(stale["server_transport"]["status"], "connected");
}

#[tokio::test]
async fn no_runner_layers_are_not_observed_with_reason_codes() {
    let runtime = test_runtime();
    let empty = layers(&runtime).await;
    assert_eq!(empty["runner_process"]["status"], "not_observed");
    assert_eq!(
        empty["runner_process"]["reason_code"],
        "no_runner_registered"
    );
    assert_eq!(empty["server_transport"]["status"], "not_observed");
    assert_eq!(empty["server_registration"]["status"], "not_observed");
    assert_eq!(empty["project_registry"]["status"], "not_configured");
    assert!(
        empty.get("session_binding").is_none(),
        "removed Workflow binding layer must not be projected"
    );
    assert_eq!(empty["last_successful_tool_call"]["status"], "not_observed");
    assert_eq!(
        empty["last_successful_tool_call"]["reason_code"],
        "no_meaningful_tool_calls_recorded"
    );
}

#[tokio::test]
async fn meaningful_activity_is_scoped_and_not_refreshed_by_status_calls() {
    let runtime = test_runtime();

    // runtime_status / discovery calls are not meaningful activity.
    for _ in 0..3 {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::RuntimeStatus {
                    compact: false,
                    summary_only: false,
                    client_id: None,
                },
                None,
            )
            .await;
        assert!(result.success);
    }
    let before = layers(&runtime).await;
    assert_eq!(
        before["last_successful_tool_call"]["status"],
        "not_observed"
    );

    // A session start is real work and is recorded with principal scope.
    let result = runtime
        .dispatch_with_auth(
            ToolCall::StartSession {
                project: None,
                title: Some("continuity".to_string()),
                mode: SessionMode::Normal,
                deny_write_tools: false,
                deny_shell_tools: false,
                execution_context: None,
            },
            None,
        )
        .await;
    assert!(result.success);

    let after = layers(&runtime).await;
    let last = &after["last_successful_tool_call"];
    assert_eq!(last["status"], "observed");
    assert_eq!(last["tool"], "start_session");
    assert_eq!(last["scope"], "principal");
    assert_eq!(last["surface"], "api");
    assert!(last["principal_kind"].is_string());
    let observed_at = last["observed_at"].as_i64().unwrap();

    // Reading forensic trace data is itself observability and must not become
    // meaningful activity even if the operator call succeeds.
    runtime
        .observations
        .record_successful_tool_call(ToolCallObservation {
            principal_kind: "bootstrap".to_string(),
            principal_id: "trace-reader".to_string(),
            project: None,
            surface: "mcp".to_string(),
            session_id: None,
            tool: "read_tool_trace".to_string(),
            observed_at: observed_at + 100,
        });
    let after_trace_read = layers(&runtime).await;
    assert_eq!(
        after_trace_read["last_successful_tool_call"]["tool"],
        "start_session"
    );
    assert_eq!(
        after_trace_read["last_successful_tool_call"]["observed_at"]
            .as_i64()
            .unwrap(),
        observed_at
    );

    // Additional status polling must not refresh the observation.
    for _ in 0..3 {
        let result = runtime
            .dispatch_with_auth(
                ToolCall::RuntimeStatus {
                    compact: false,
                    summary_only: false,
                    client_id: None,
                },
                None,
            )
            .await;
        assert!(result.success);
    }
    let still = layers(&runtime).await;
    assert_eq!(still["last_successful_tool_call"]["tool"], "start_session");
    assert_eq!(
        still["last_successful_tool_call"]["observed_at"]
            .as_i64()
            .unwrap(),
        observed_at
    );
}
#[tokio::test]
async fn canonical_project_session_explicit_resume_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let project_root = dir.path().join("proj");
    std::fs::create_dir_all(&project_root).unwrap();
    init_git_repo(&project_root);
    let alice = shared_key_auth_context("canonical-restart-alice");
    let expected_fingerprint =
        super::super::session_context::workflow_session_authority_fingerprint(Some(&alice))
            .unwrap();

    let runtime1 = ToolRuntime::new_for_tests().with_session_ledger(&ledger);
    let project = register_runner_project_at_path_with_auth(
        &runtime1,
        "canonical-restart-agent",
        "proj",
        &project_root,
        &alice,
    )
    .await;
    let started = dispatch_coding_call_in_window(
        &runtime1,
        "canonical-restart-agent",
        coding_start_call(&project, "canonical root"),
        Some(&alice),
        "canonical-restart-window",
    )
    .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();
    runtime1.sessions.flush_persistence();
    assert_eq!(
        persisted_session_authority_fingerprint(&ledger, &session_id).as_deref(),
        Some(expected_fingerprint.as_str())
    );
    drop(runtime1);

    let runtime2 = ToolRuntime::new_for_tests().with_session_ledger(&ledger);
    register_runner_project_at_path_with_auth(
        &runtime2,
        "canonical-restart-agent",
        "proj",
        &project_root,
        &alice,
    )
    .await;
    assert_eq!(runtime2.sessions.status().restored_sessions, 1);
    let resumed = dispatch_coding_call_in_window(
        &runtime2,
        "canonical-restart-agent",
        coding_resume_call(&project, "explicit canonical resume", &session_id),
        Some(&alice),
        "canonical-restart-window",
    )
    .await;
    assert!(resumed.success, "{:?}", resumed.error);
    assert_eq!(resumed.output["session_id"], session_id);
    assert_eq!(
        runtime2
            .sessions
            .session_target_authority(&session_id)
            .unwrap()
            .1,
        expected_fingerprint
    );
}
#[tokio::test]
async fn malformed_persisted_authority_is_discarded_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let project_root = dir.path().join("proj");
    std::fs::create_dir_all(&project_root).unwrap();
    init_git_repo(&project_root);
    let alice = shared_key_auth_context("malformed-authority-alice");

    let runtime1 = ToolRuntime::new_for_tests().with_session_ledger(&ledger);
    let project = register_runner_project_at_path_with_auth(
        &runtime1,
        "malformed-authority-agent",
        "proj",
        &project_root,
        &alice,
    )
    .await;
    let started = dispatch_coding_call_in_window(
        &runtime1,
        "malformed-authority-agent",
        coding_start_call(&project, "canonical root"),
        Some(&alice),
        "malformed-authority-window",
    )
    .await;
    assert!(started.success, "{:?}", started.error);
    let session_id = started.output["session_id"].as_str().unwrap().to_string();
    runtime1.sessions.flush_persistence();
    drop(runtime1);
    rewrite_persisted_session_with_malformed_authority(&ledger, &session_id);

    let runtime2 = ToolRuntime::new_for_tests().with_session_ledger(&ledger);
    assert_eq!(runtime2.sessions.status().restored_sessions, 0);
    assert!(runtime2.sessions.summary(&session_id, None).is_none());
    assert!(runtime2
        .sessions
        .session_target_authority(&session_id)
        .is_none());
}
#[tokio::test]
async fn agent_job_lost_on_disconnect_stays_terminal_after_reconnect() {
    let runtime = test_runtime();
    register_with_project(&runtime, "job-agent", "inst-a", None, None).await;

    // Start an async agent job and let the agent pick it up.
    let job = runtime
        .runner_registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("job-agent".to_string()),
                cwd: None,
                command: Some("sleep 60".to_string()),
                timeout_secs: Some(120),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "reconnect-test".to_string(),
        )
        .await
        .unwrap();
    let _req = wait_for_runner_request_for_instance(&runtime, "job-agent", "inst-a").await;

    // Transport drops mid-job: job authority is not silently completed.
    runtime
        .runner_registry
        .reconcile_disconnect("job-agent", "inst-a")
        .await;
    let jobs = runtime.runner_registry.list_jobs(None).await;
    let lost = jobs
        .iter()
        .find(|info| info.job_id == job.job_id)
        .expect("job still queryable after disconnect");
    assert_eq!(lost.status, "lost");
    let first_ended_at = lost.ended_at.expect("lost job has terminal timestamp");

    // Reconnect with a new instance: the terminal state must not be
    // resurrected or duplicated.
    register_with_project(&runtime, "job-agent", "inst-b", None, None).await;
    let jobs = runtime.runner_registry.list_jobs(None).await;
    let still_lost = jobs
        .iter()
        .find(|info| info.job_id == job.job_id)
        .expect("job still queryable after reconnect");
    assert_eq!(still_lost.status, "lost");
    assert_eq!(still_lost.ended_at, Some(first_ended_at));
}

#[tokio::test]
async fn runtime_status_keeps_generation_independent_from_transport() {
    let runtime = test_runtime();
    let cases = [
        ("polling-gen2", RunnerTransport::Polling, "polling"),
        ("websocket-gen2", RunnerTransport::WebSocket, "websocket"),
        ("quic-gen2", RunnerTransport::Quic, "quic"),
    ];

    for (client_id, transport, _) in cases.iter().copied() {
        let registration = register_request(client_id, "inst", None, None);
        match transport {
            RunnerTransport::Polling => {
                runtime
                    .runner_registry
                    .register(registration)
                    .await
                    .unwrap();
            }
            RunnerTransport::WebSocket | RunnerTransport::Quic => {
                runtime
                    .runner_registry
                    .register_streaming_session(
                        registration,
                        None,
                        &format!("connection-{client_id}"),
                        transport,
                        Arc::new(Notify::new()),
                    )
                    .await
                    .unwrap();
            }
        }
    }

    let status = runtime.runtime_status(None).await;
    assert!(status.success);
    let runners = status.output["version_compatibility"]["runners"]
        .as_array()
        .unwrap();
    let clients = status.output["agents"]["clients"].as_array().unwrap();
    for (client_id, _, transport) in cases {
        let runner = runners
            .iter()
            .find(|runner| runner["client_id"] == client_id)
            .unwrap_or_else(|| panic!("runner {client_id} missing"));
        assert_eq!(
            runner["agent_protocol_generation"],
            RUNNER_PROTOCOL_GENERATION_V2.get()
        );
        assert_eq!(runner["status"], "compatible");

        let client = clients
            .iter()
            .find(|client| client["client_id"] == client_id)
            .unwrap_or_else(|| panic!("client {client_id} missing"));
        assert_eq!(client["transport"], transport);
        assert_eq!(
            client["agent_protocol_generation"],
            RUNNER_PROTOCOL_GENERATION_V2.get()
        );
        assert_eq!(client["project_inventory"]["sync_state"], "pending");
    }
}

#[tokio::test]
async fn version_compatibility_reports_stable_mismatch_facts() {
    let runtime = test_runtime();
    let server_version = env!("CARGO_PKG_VERSION");

    // Same package version + supported protocol remains compatible even when
    // exact source differs. Source alignment is a separate diagnostic axis.
    let server_build = crate::build_info::runtime_build_info();
    let different_commit = format!(
        "{}-different",
        server_build.git_commit.unwrap_or("server-source")
    );
    runtime
        .runner_registry
        .register(register_request(
            "same-version-different-source",
            "inst-1",
            None,
            Some(RunnerBuildInfo {
                version: Some(server_version.to_string()),
                git_commit: Some(different_commit),
                git_dirty: Some(false),
            }),
        ))
        .await
        .unwrap();
    // Different build version → version_mismatch (connected ≠ compatible).
    runtime
        .runner_registry
        .register(register_request(
            "old-build",
            "inst-2",
            None,
            Some(RunnerBuildInfo {
                version: Some("0.0.1".to_string()),
                git_commit: None,
                git_dirty: None,
            }),
        ))
        .await
        .unwrap();
    // Unsupported protocol generations fail registration and therefore never
    // become a diagnostic-but-operational runtime client.
    let mut unsupported = register_request("future-generation", "inst-3", None, None);
    unsupported.runner_protocol_generation = RunnerProtocolGenerationNumber::new(3);
    let unsupported = runtime
        .runner_registry
        .register(unsupported)
        .await
        .unwrap_err();
    assert_eq!(unsupported, "agent_protocol_generation is unsupported");

    let status = runtime.runtime_status(None).await;
    assert!(status.success);
    let compat = &status.output["version_compatibility"];
    assert_eq!(compat["status"], "version_mismatch");
    assert_eq!(compat["server"]["version"], server_version);
    let runners = compat["runners"].as_array().unwrap();
    let by_id = |id: &str| {
        runners
            .iter()
            .find(|runner| runner["client_id"] == id)
            .unwrap_or_else(|| panic!("runner {id} missing"))
    };
    let different_source = by_id("same-version-different-source");
    assert_eq!(different_source["status"], "compatible");
    assert_eq!(different_source["version_matches_server"], true);
    assert_eq!(different_source["source_alignment"]["status"], "different");
    assert_eq!(
        different_source["source_alignment"]["reason_code"],
        "runner_git_commit_differs_from_server"
    );
    assert_eq!(compat["source_alignment"]["status"], "different");
    assert!(different_source.get("build_matches_server").is_none());

    assert_eq!(by_id("old-build")["status"], "version_mismatch");
    assert_eq!(
        by_id("old-build")["reason_code"],
        "runner_version_differs_from_server"
    );
    assert!(by_id("old-build")["action"]
        .as_str()
        .unwrap()
        .contains("align"));
    let compact = crate::tool_runtime::runtime_info::compact_runtime_status(&status.output);
    let compact_runner = compact["agents"]["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|runner| runner["client_id"] == "same-version-different-source")
        .unwrap();
    assert_eq!(compact_runner["version_matches_server"], true);
    assert_eq!(compact_runner["source_alignment"]["status"], "different");
    assert!(compact_runner.get("build_matches_server").is_none());
    assert_eq!(
        compact["version_compatibility"]["source_alignment"]["status"],
        "different"
    );

    // No secrets/paths in the diagnostics.
    let text = compat.to_string().to_lowercase();
    assert!(!text.contains("token"));
    assert!(!text.contains("/root/"));
}

#[tokio::test]
async fn runner_host_context_projects_to_full_list_and_compact_runtime() {
    let runtime = test_runtime();
    let mut request = register_request("sf", "inst-host-context", None, None);
    request.host_context = Some(RunnerHostContext {
        role: Some("server_host".to_string()),
        runtime: Some("Prefer this Runner for operations on its own host.".to_string()),
        service: Some("Use the ordinary host-local service mechanism.".to_string()),
        network: None,
        architecture: Some("Hosts the WebCodex Server/control plane.".to_string()),
    });
    runtime.runner_registry.register(request).await.unwrap();

    let status = runtime.runtime_status(None).await;
    assert!(status.success);
    let full = status.output["agents"]["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|client| client["client_id"] == "sf")
        .unwrap();
    assert_eq!(full["host_context"]["source"], "runner_config");
    assert_eq!(full["host_context"]["role"], "server_host");
    assert_eq!(
        full["host_context"]["architecture"],
        "Hosts the WebCodex Server/control plane."
    );

    let compact = crate::tool_runtime::runtime_info::compact_runtime_status(&status.output);
    let compact_sf = compact["agents"]["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|client| client["client_id"] == "sf")
        .unwrap();
    assert_eq!(compact_sf["agent_instance_id"], "inst-host-context");
    assert_eq!(compact_sf["host_context"]["role"], "server_host");
    assert!(compact_sf.get("capabilities").is_none());
    assert!(compact_sf.get("policy").is_none());

    let listed = runtime.list_runners(None).await;
    assert!(listed.success);
    let listed_sf = listed.output["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|client| client["client_id"] == "sf")
        .unwrap();
    assert_eq!(listed_sf["host_context"]["source"], "runner_config");
    assert_eq!(listed_sf["host_context"]["role"], "server_host");

    // Same-instance reconnect republishes the current startup context; it does
    // not depend on the previous transport record for this descriptive fact.
    let mut reconnect = register_request("sf", "inst-host-context", None, None);
    reconnect.host_context = Some(RunnerHostContext {
        role: Some("server_host".to_string()),
        network: Some("Internal destinations normally use the direct path.".to_string()),
        ..Default::default()
    });
    runtime.runner_registry.register(reconnect).await.unwrap();
    let after_reconnect = runtime.runtime_status(None).await;
    let sf = after_reconnect.output["agents"]["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|client| client["client_id"] == "sf")
        .unwrap();
    assert_eq!(
        sf["host_context"]["network"],
        "Internal destinations normally use the direct path."
    );
}

#[tokio::test]
async fn runner_host_context_is_revalidated_at_server_registration() {
    let runtime = test_runtime();
    let mut request = register_request("bad-context", "inst-bad", None, None);
    request.host_context = Some(RunnerHostContext {
        role: Some("Server Host".to_string()),
        ..Default::default()
    });
    let err = runtime.runner_registry.register(request).await.unwrap_err();
    assert!(err.contains("host_context.role"), "{err}");
    assert!(runtime
        .runner_registry
        .get_runner_view("bad-context")
        .await
        .is_none());
}

pub(in crate::tool_runtime::tests) async fn dispatch_coding_call_in_window(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    auth: Option<&AuthContext>,
    window_id: &str,
) -> crate::tool_runtime::ToolResult {
    dispatch_coding_call_in_window_with_transport(
        runtime,
        client_id,
        call,
        auth,
        window_id,
        crate::tool_runtime::sessions::SessionTransport::Mcp,
    )
    .await
}

async fn dispatch_coding_call_in_window_with_transport(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    auth: Option<&AuthContext>,
    window_id: &str,
    transport: crate::tool_runtime::sessions::SessionTransport,
) -> crate::tool_runtime::ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.cloned();
        let window_id = window_id.to_string();
        async move {
            let window = ClientWindow::for_test(&window_id);
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_window(
                    call,
                    auth.as_ref(),
                    transport,
                    Default::default(),
                    Some(&window),
                )
                .await
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !task.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "coding workflow did not finish within the 10-second test deadline"
        );
        if let Some(req) = runtime
            .runner_registry
            .poll(crate::runner_protocol::RunnerPollRequest {
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
            })
            .await
            .unwrap()
        {
            let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&req);
            complete_patch_agent_request(
                runtime,
                client_id,
                &req.request_id,
                exit_code,
                &stdout,
                &stderr,
            )
            .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    task.await.unwrap()
}

fn coding_start_call(project: &str, instruction: &str) -> ToolCall {
    ToolCall::WorkOnProject {
        project: project.to_string(),
        client_id: None,
        path: None,
        mode: None,
        base_ref: None,
        instruction: instruction.to_string(),
        session_id: None,
        include_project_instructions: true,
        include_workflow_guidance: true,
    }
}

fn coding_resume_call(project: &str, instruction: &str, session_id: &str) -> ToolCall {
    ToolCall::WorkOnProject {
        project: project.to_string(),
        client_id: None,
        path: None,
        mode: None,
        base_ref: None,
        instruction: instruction.to_string(),
        session_id: Some(session_id.to_string()),
        include_project_instructions: true,
        include_workflow_guidance: true,
    }
}

#[tokio::test]
async fn work_on_project_without_resume_always_creates_fresh_session() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "continuity-agent", "demo", dir.path()).await;
    let auth = auth_context(None, true);

    let first = dispatch_coding_call_in_window(
        &runtime,
        "continuity-agent",
        coding_start_call(&project, "root objective"),
        Some(&auth),
        "continuity-window",
    )
    .await;
    let second = dispatch_coding_call_in_window(
        &runtime,
        "continuity-agent",
        coding_start_call(&project, "follow-up objective"),
        Some(&auth),
        "continuity-window",
    )
    .await;

    assert!(first.success, "{:?}", first.error);
    assert!(second.success, "{:?}", second.error);
    let first_id = first.output["session_id"].as_str().unwrap();
    let second_id = second.output["session_id"].as_str().unwrap();
    assert_ne!(second_id, first_id);
    assert_eq!(first.output["continuation"], "created");
    assert_eq!(second.output["continuation"], "created");
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        2
    );
    let first_summary = runtime.sessions.summary(first_id, Some(20)).unwrap();
    let second_summary = runtime.sessions.summary(second_id, Some(20)).unwrap();
    assert_eq!(first_summary.title.as_deref(), Some("root objective"));
    assert_eq!(second_summary.title.as_deref(), Some("follow-up objective"));
    assert_eq!(
        first_summary
            .events
            .iter()
            .filter(|event| event.kind == "task_instruction")
            .count(),
        1
    );
    assert_eq!(
        second_summary
            .events
            .iter()
            .filter(|event| event.kind == "task_instruction")
            .count(),
        1
    );
}

#[tokio::test]
async fn work_on_project_second_fresh_start_preserves_previous_session() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "isolation-agent", "demo", dir.path()).await;
    let auth = auth_context(None, true);
    let first = dispatch_coding_call_in_window(
        &runtime,
        "isolation-agent",
        coding_start_call(&project, "first root"),
        Some(&auth),
        "isolation-window",
    )
    .await;
    let isolated = dispatch_coding_call_in_window(
        &runtime,
        "isolation-agent",
        coding_start_call(&project, "second root"),
        Some(&auth),
        "isolation-window",
    )
    .await;
    assert!(first.success && isolated.success);
    let first_id = first.output["session_id"].as_str().unwrap();
    let isolated_id = isolated.output["session_id"].as_str().unwrap();
    assert_ne!(first_id, isolated_id);
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        2
    );
    let old = runtime.sessions.summary(first_id, Some(10)).unwrap();
    assert_eq!(old.title.as_deref(), Some("first root"));
    assert_eq!(
        old.lifecycle,
        crate::tool_runtime::sessions::SessionLifecycle::Active
    );
}
#[tokio::test]
async fn failed_project_start_does_not_create_or_mutate_workflow_session() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project = register_runner_project_at_path(&runtime, "stable-agent", "a", dir.path()).await;
    let auth = auth_context(None, true);
    let first = dispatch_coding_call_in_window(
        &runtime,
        "stable-agent",
        coding_start_call(&project, "stable A"),
        Some(&auth),
        "failed-switch-window",
    )
    .await;
    assert!(first.success);

    let failed = dispatch_coding_call_in_window(
        &runtime,
        "stable-agent",
        coding_start_call("agent:missing-agent:b", "failed B"),
        Some(&auth),
        "failed-switch-window",
    )
    .await;
    assert!(!failed.success);
    assert_eq!(failed.output["error_kind"], "unknown_project");
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        1
    );

    let again = dispatch_coding_call_in_window(
        &runtime,
        "stable-agent",
        coding_start_call(&project, "fresh A"),
        Some(&auth),
        "failed-switch-window",
    )
    .await;
    assert!(again.success);
    assert_ne!(first.output["session_id"], again.output["session_id"]);
    assert_eq!(
        runtime
            .sessions
            .active_session_count_for_test(Some(&project)),
        2
    );
}

#[tokio::test]
async fn coding_workflow_read_only_upgrade_is_atomic_and_permission_checked() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    commit_file(
        dir.path(),
        "AGENTS.md",
        "# Test rules\n\nPreserve focused exploration.\n",
        "add rules",
    );
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    commit_file(
        dir.path(),
        "src/inspect.rs",
        "pub fn inspected() -> bool { true }\n",
        "add inspected source",
    );
    let runtime = ToolRuntime::new_for_tests();
    let read_auth = oauth_bridge_auth_context(
        "continuity-subject",
        &[
            crate::auth::SCOPE_RUNTIME_READ,
            crate::auth::SCOPE_PROJECT_READ,
        ],
    );
    let write_auth = oauth_bridge_auth_context(
        "continuity-subject",
        &[
            crate::auth::SCOPE_RUNTIME_READ,
            crate::auth::SCOPE_PROJECT_READ,
            crate::auth::SCOPE_PROJECT_WRITE,
        ],
    );
    let project = register_runner_project_at_path_with_auth(
        &runtime,
        "oauth-client",
        "demo",
        dir.path(),
        &read_auth,
    )
    .await;
    let read_only_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let read_auth = read_auth.clone();
        async move {
            runtime
                .start_coding_workflow_for_test(
                    project,
                    None,
                    None,
                    Some("read only first".to_string()),
                    SessionMode::ReadOnly,
                    false,
                    false,
                    StartupDetail::Standard,
                    None,
                    None,
                    Some(&read_auth),
                    None,
                    None,
                    crate::tool_runtime::sessions::SessionTransport::Mcp,
                )
                .await
        }
    });
    while !read_only_task.is_finished() {
        if let Some(req) = runtime
            .runner_registry
            .poll(crate::runner_protocol::RunnerPollRequest {
                client_id: "oauth-client".to_string(),
                runner_instance_id: "inst".to_string(),
            })
            .await
            .unwrap()
        {
            let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&req);
            complete_patch_agent_request(
                &runtime,
                "oauth-client",
                &req.request_id,
                exit_code,
                &stdout,
                &stderr,
            )
            .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    let read_only_started = read_only_task.await.unwrap();
    assert!(read_only_started.success, "{:?}", read_only_started.error);
    let session_id = read_only_started.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let read = dispatch_coding_call_in_window(
        &runtime,
        "oauth-client",
        ToolCall::ReadFile {
            project: project.clone(),
            path: "src/inspect.rs".to_string(),
            session_id: Some(session_id.clone()),
            start_line: None,
            limit: None,
            with_line_numbers: None,
        },
        Some(&read_auth),
        "upgrade-window",
    )
    .await;
    assert!(read.success, "{:?}", read.error);

    let denied = dispatch_coding_call_in_window(
        &runtime,
        "oauth-client",
        coding_resume_call(&project, "request writes", &session_id),
        Some(&read_auth),
        "upgrade-window",
    )
    .await;
    assert!(!denied.success);
    assert_eq!(
        denied.output["error_kind"],
        "session_capability_upgrade_denied"
    );
    let unchanged = runtime.sessions.summary(&session_id, Some(20)).unwrap();
    assert_eq!(unchanged.mode, SessionMode::ReadOnly);
    assert!(unchanged.guards.deny_write_tools);
    assert!(unchanged.guards.deny_shell_tools);
    assert_eq!(
        unchanged
            .events
            .iter()
            .filter(|event| event.kind == "task_instruction")
            .count(),
        1
    );

    let upgraded = dispatch_coding_call_in_window(
        &runtime,
        "oauth-client",
        coding_resume_call(&project, "enable writes", &session_id),
        Some(&write_auth),
        "upgrade-window",
    )
    .await;
    assert!(upgraded.success, "{:?}", upgraded.error);
    assert_eq!(upgraded.output["session_id"], session_id);
    assert_eq!(upgraded.output["continuation"], "resumed_explicitly");
    assert_eq!(upgraded.output["instructions"]["status"], "reused");
    assert_eq!(upgraded.output["instructions"]["content_included"], true);
    assert!(upgraded.output.get("continuation_feedback").is_none());
    let summary = runtime.sessions.summary(&session_id, Some(20)).unwrap();
    assert!(!summary.guards.deny_write_tools);
    assert!(!summary.guards.deny_shell_tools);
    let transition = summary
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "task_instruction")
        .unwrap();
    assert_eq!(transition.previous_mode.as_deref(), Some("read_only"));
    assert_eq!(transition.requested_mode.as_deref(), Some("normal"));
    assert_eq!(transition.capability_changed, Some(true));
    assert_eq!(
        summary
            .events
            .iter()
            .filter(|event| {
                event.kind == "tool_call_finished"
                    && event.tool_name == "read_file"
                    && event.status.as_deref() == Some("succeeded")
            })
            .count(),
        1,
        "coding workflow must not reread ordinary explored source files"
    );
}
