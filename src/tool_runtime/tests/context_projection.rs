use super::super::kernel::{
    HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolInvocationMetadata,
    ToolProtocolCapabilities, ToolTransport,
};
use super::super::sessions::{
    SessionContextRevisionAck, SessionTransport, ToolCallRecorderMetadata,
};
use super::super::{ToolCall, ToolResult, ToolRuntime};
use super::support::*;
use crate::runner_protocol::{RunnerCapabilities, RunnerResultPayload, RunnerResultRequest};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use webcodex_core::plugin::{
    PluginGatewayRequest, PluginGatewayResponse, PluginGatewayResponsePayload,
    PluginSelectionAnnotations, ProjectPluginCatalog, ProjectPluginCatalogEntry,
};

fn context_material<'a>(result: &'a ToolResult, key: &str) -> &'a Value {
    result.output["context_projection"]["materials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|material| material["key"] == key)
        .unwrap_or_else(|| panic!("missing context material {key}: {}", result.output))
}

async fn complete_plugin_catalog_request(
    runtime: &ToolRuntime,
    request: crate::runner_protocol::RunnerRequest,
    catalog: ProjectPluginCatalog,
) {
    runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: request.client_id,
                runner_instance_id: "inst".to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: None,
            },
            command_execution_state: None,
            mcp_gateway: None,
            plugin_gateway: Some(PluginGatewayResponse::success(
                PluginGatewayResponsePayload::ProjectCatalog { catalog },
            )),
            coding_agent: None,
        })
        .await
        .unwrap();
}

fn plugin_catalog(entries: usize) -> ProjectPluginCatalog {
    ProjectPluginCatalog {
        catalog_revision: format!("wc_plugcat_{}", "a".repeat(64)),
        total_count: entries,
        entries: (0..entries)
            .map(|index| ProjectPluginCatalogEntry {
                plugin: format!("repo-tools-{index:03}"),
                name: format!("Repo Tools {index:03}"),
                tool: format!("repo_context_{index:03}"),
                title: Some(format!("Repository context {index:03}")),
                description: Some(format!(
                    "Bounded selection description {index:03} {}",
                    "x".repeat(256)
                )),
                annotations: PluginSelectionAnnotations {
                    read_only_hint: Some(true),
                    destructive_hint: Some(false),
                    idempotent_hint: Some(true),
                    open_world_hint: Some(false),
                },
            })
            .collect(),
    }
}

async fn dispatch_with_context_and_local_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
    context_request: Vec<String>,
) -> ToolResult {
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_recording_mode_and_context(
                    call,
                    Some(&auth),
                    SessionTransport::Mcp,
                    ToolCallRecorderMetadata::default(),
                    None,
                    true,
                    context_request,
                    super::super::context_projection::ContextMaterialCapabilities::default(),
                )
                .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if task.is_finished() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "context projection fixture timed out"
        );
        if let Some(request) = probe_patch_agent_request(runtime, client_id).await {
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
        } else {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    task.await.unwrap()
}

#[tokio::test]
async fn context_projection_is_explicit_deduped_open_ended_and_nonfatal() {
    let runtime = ToolRuntime::new_for_tests();
    let call = ToolCall::from_tool_name("list_tools", json!({})).unwrap();
    let without = runtime
        .dispatch_with_auth_transport_options_and_metadata_with_recording_mode_and_context(
            call,
            None,
            SessionTransport::Mcp,
            Default::default(),
            None,
            true,
            Vec::new(),
            super::super::context_projection::ContextMaterialCapabilities::default(),
        )
        .await;
    assert!(without.success);
    assert!(without.output.get("context_projection").is_none());

    let call = ToolCall::from_tool_name("list_tools", json!({})).unwrap();
    let result = runtime
        .dispatch_with_auth_transport_options_and_metadata_with_recording_mode_and_context(
            call,
            None,
            SessionTransport::Mcp,
            Default::default(),
            None,
            true,
            vec![
                "webcodex.workflow".to_string(),
                "future.material".to_string(),
                "webcodex.workflow".to_string(),
                "project.instructions".to_string(),
            ],
            super::super::context_projection::ContextMaterialCapabilities::default(),
        )
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["context_projection"]["timing"], "post_tool");
    assert_eq!(
        result.output["context_projection"]["applies_to_current_effect"],
        false
    );
    let materials = result.output["context_projection"]["materials"]
        .as_array()
        .unwrap();
    assert_eq!(materials.len(), 3, "duplicates must be projected once");
    assert_eq!(materials[0]["key"], "webcodex.workflow");
    assert_eq!(
        materials[0]["projection"],
        crate::tool_runtime::startup_brief::builtin_coding_workflow_projection(),
        "context recovery must return the same guidance as coding startup"
    );
    assert_eq!(materials[0]["status"], "available");
    assert_eq!(
        materials[0]["projection"]["contract"],
        "webcodex.coding_workflow"
    );
    assert_eq!(materials[1]["key"], "future.material");
    assert_eq!(materials[1]["status"], "unsupported");
    assert_eq!(materials[2]["key"], "project.instructions");
    assert_eq!(materials[2]["status"], "unavailable");
    assert_eq!(materials[2]["reason_code"], "project_target_unavailable");
    assert!(
        serde_json::to_vec(&result.output["context_projection"])
            .unwrap()
            .len()
            <= crate::tool_runtime::context_projection::MAX_CONTEXT_PROJECTION_BYTES
    );
}

#[tokio::test]
async fn private_context_marker_requires_explicit_sidecar_capability() {
    let runtime = ToolRuntime::new_for_tests();

    let outcome = runtime
        .call_tool_with_invocation_metadata(
            ToolCallRequest {
                tool_name: "list_tools".to_string(),
                arguments: json!({}),
            },
            ToolCallContext {
                transport: ToolTransport::Mcp,
                session_id: None,
                auth: None,
                window: None,
                record_oauth_scope_denials: false,
                host_file_import_trust: HostFileImportTrust::Untrusted,
            },
            ToolInvocationMetadata {
                context_request: vec!["webcodex.workflow".to_string()],
                ..Default::default()
            },
            ToolProtocolCapabilities {
                context_continuity: true,
                context_sidecar: false,
                ..Default::default()
            },
        )
        .await;
    let result = outcome.result.expect("model-facing result");
    assert!(result.success, "{:?}", result.error);
    assert!(
        result.output.get("context_projection").is_none(),
        "private wrapper marker must not enable context sidecars on a non-capable surface"
    );
}

#[tokio::test]
async fn project_instructions_context_projection_is_authorized_scoped_and_bounded() {
    let alpha_root = tempfile::tempdir().unwrap();
    let bravo_root = tempfile::tempdir().unwrap();
    init_git_repo(alpha_root.path());
    init_git_repo(bravo_root.path());
    std::fs::write(
        alpha_root.path().join("AGENTS.md"),
        "# Alpha rules\n\nALPHA_PRIVATE_RULE\n",
    )
    .unwrap();
    let bravo_rules = format!(
        "# Bravo rules\n\nBRAVO_SCOPED_RULE\n{}\n",
        "x".repeat(50_000)
    );
    std::fs::write(bravo_root.path().join("AGENTS.md"), bravo_rules).unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let alpha =
        register_runner_project_at_path(&runtime, "context-alpha", "alpha", alpha_root.path())
            .await;
    let bravo =
        register_runner_project_at_path(&runtime, "context-bravo", "bravo", bravo_root.path())
            .await;

    let result = dispatch_with_context_and_local_agent(
        &runtime,
        "context-bravo",
        ToolCall::GitStatus {
            project: bravo.clone(),
            session_id: None,
        },
        vec![
            "project.instructions".to_string(),
            "webcodex.workflow".to_string(),
        ],
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let instructions = context_material(&result, "project.instructions");
    assert_eq!(instructions["status"], "available");
    assert_eq!(instructions["projection"]["content_included"], true);
    assert_eq!(instructions["projection"]["truncated"], true);
    assert!(instructions["projection"]["sources"][0]["read_more"].is_object());
    let serialized = instructions.to_string();
    assert!(serialized.contains("BRAVO_SCOPED_RULE"));
    assert!(!serialized.contains("ALPHA_PRIVATE_RULE"));
    assert!(
        context_material(&result, "webcodex.workflow")["projection"]["contract"]
            == "webcodex.coding_workflow"
    );
    assert!(
        serde_json::to_vec(&result.output["context_projection"])
            .unwrap()
            .len()
            <= crate::tool_runtime::context_projection::MAX_CONTEXT_PROJECTION_BYTES
    );
    let session = runtime
        .sessions
        .start_session(Some(alpha), Some("cross-project sidecar fence".to_string()));
    let auth = auth_context(None, true);
    let cross_project = runtime
        .dispatch_with_auth_transport_options_and_metadata_with_recording_mode_and_context(
            ToolCall::RunShell {
                project: bravo,
                command: "pwd".to_string(),
                session_id: Some(session.session_id),
                timeout_secs: Some(30),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
            SessionTransport::Mcp,
            Default::default(),
            None,
            true,
            vec!["project.instructions".to_string()],
            super::super::context_projection::ContextMaterialCapabilities::default(),
        )
        .await;
    assert!(!cross_project.success);
    assert_eq!(
        cross_project.output["failure_kind"],
        "session_project_mismatch"
    );
    assert_eq!(cross_project.output["command_started"], false);
    assert_eq!(
        context_material(&cross_project, "project.instructions")["status"],
        "unavailable"
    );
    assert_eq!(
        context_material(&cross_project, "project.instructions")["reason_code"],
        "project_target_unavailable"
    );
    let cross_serialized = serde_json::to_string(&cross_project).unwrap();
    assert!(!cross_serialized.contains("ALPHA_PRIVATE_RULE"));
    assert!(!cross_serialized.contains("BRAVO_SCOPED_RULE"));

    let auth = auth_context(None, true);
    let wrong_project = runtime
        .dispatch_with_auth_transport_options_and_metadata_with_recording_mode_and_context(
            ToolCall::GitStatus {
                project: "agent:context-bravo:missing".to_string(),
                session_id: None,
            },
            Some(&auth),
            SessionTransport::Mcp,
            Default::default(),
            None,
            true,
            vec!["project.instructions".to_string()],
            super::super::context_projection::ContextMaterialCapabilities::default(),
        )
        .await;
    assert!(!wrong_project.success);
    assert_eq!(
        context_material(&wrong_project, "project.instructions")["status"],
        "unavailable"
    );
    assert_eq!(
        context_material(&wrong_project, "project.instructions")["reason_code"],
        "project_target_unavailable"
    );
    let wrong_serialized = serde_json::to_string(&wrong_project).unwrap();
    assert!(!wrong_serialized.contains("ALPHA_PRIVATE_RULE"));
    assert!(!wrong_serialized.contains("BRAVO_SCOPED_RULE"));
}

#[tokio::test]
async fn unavailable_project_instructions_provider_does_not_change_main_success() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    std::fs::write(
        root.path().join("AGENTS.md"),
        "# Provider failure fixture\n",
    )
    .unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "context-provider-fail", "demo", root.path())
            .await;
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_recording_mode_and_context(
                    ToolCall::GitStatus {
                        project,
                        session_id: None,
                    },
                    Some(&auth),
                    SessionTransport::Mcp,
                    Default::default(),
                    None,
                    true,
                    vec!["project.instructions".to_string()],
                    super::super::context_projection::ContextMaterialCapabilities::default(),
                )
                .await
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut main_observed = false;
    let mut failed_instruction_reads = 0usize;
    while !task.is_finished() {
        assert!(
            Instant::now() < deadline,
            "provider failure fixture timed out"
        );
        if let Some(request) = probe_patch_agent_request(&runtime, "context-provider-fail").await {
            if request.kind == "file_read" {
                assert!(
                    main_observed,
                    "instruction provider must run only after the main tool"
                );
                complete_patch_agent_request(
                    &runtime,
                    "context-provider-fail",
                    &request.request_id,
                    1,
                    "",
                    "provider unavailable",
                )
                .await;
                failed_instruction_reads += 1;
            } else {
                assert!(!main_observed, "main GitStatus should execute once");
                let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
                assert_eq!(
                    exit_code, 0,
                    "main GitStatus fixture must succeed: {stderr}"
                );
                complete_patch_agent_request(
                    &runtime,
                    "context-provider-fail",
                    &request.request_id,
                    exit_code,
                    &stdout,
                    &stderr,
                )
                .await;
                main_observed = true;
            }
        } else {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    let result = task.await.unwrap();
    assert!(main_observed);
    assert!(failed_instruction_reads > 0);
    assert!(
        result.success,
        "main GitStatus must remain successful: {:?}",
        result.error
    );
    let instructions = context_material(&result, "project.instructions");
    assert_eq!(instructions["status"], "unavailable");
    assert_eq!(
        instructions["reason_code"],
        "project_instructions_observation_incomplete"
    );
    assert_eq!(instructions["projection"]["content_included"], false);
}

#[tokio::test]
async fn mutation_context_projection_is_post_tool_and_does_not_change_authority_or_effect() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    std::fs::write(
        root.path().join("AGENTS.md"),
        "# Rules\n\nRECOVER_BEFORE_MUTATION\n",
    )
    .unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "context-write", "demo", root.path()).await;
    let auth = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .dispatch_with_auth_transport_options_and_metadata_with_recording_mode_and_context(
                    ToolCall::WriteProjectFile {
                        project,
                        path: "written.txt".to_string(),
                        content: "written before sidecar\n".to_string(),
                        session_id: None,
                        overwrite: None,
                        expected_sha256: None,
                    },
                    Some(&auth),
                    SessionTransport::Mcp,
                    Default::default(),
                    None,
                    true,
                    vec!["project.instructions".to_string()],
                    super::super::context_projection::ContextMaterialCapabilities::default(),
                )
                .await
        }
    });

    let write = wait_for_patch_agent_request(&runtime, "context-write").await;
    assert_eq!(write.kind, "file_write_project_file");
    std::fs::write(root.path().join("written.txt"), "written before sidecar\n").unwrap();
    complete_patch_agent_request(
        &runtime,
        "context-write",
        &write.request_id,
        0,
        r#"{"path":"written.txt","bytes_written":23,"sha256":"abc","changed":true,"state_changed":true,"execution_state":"completed"}"#,
        "",
    )
    .await;
    assert!(
        root.path().join("written.txt").exists(),
        "main mutation must complete before sidecar observation"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while !task.is_finished() {
        assert!(
            Instant::now() < deadline,
            "mutation sidecar fixture timed out"
        );
        if let Some(request) = probe_patch_agent_request(&runtime, "context-write").await {
            assert!(
                matches!(request.kind.as_str(), "file_read" | "file_list"),
                "only post-tool instruction observation may follow the write: {}",
                request.kind
            );
            let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
            complete_patch_agent_request(
                &runtime,
                "context-write",
                &request.request_id,
                exit_code,
                &stdout,
                &stderr,
            )
            .await;
        } else {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["permission"]["status"], "auto_approved");
    assert_eq!(result.output["permission"]["risk"], "write");
    assert_eq!(result.output["context_projection"]["timing"], "post_tool");
    assert_eq!(
        result.output["context_projection"]["applies_to_current_effect"],
        false
    );
    assert!(context_material(&result, "project.instructions")
        .to_string()
        .contains("RECOVER_BEFORE_MUTATION"));
}

#[tokio::test]
async fn plugins_catalog_sidecar_requires_inspect_scope_without_affecting_main_result() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let mut capabilities = RunnerCapabilities::default();
    capabilities.native_tool_plugins = true;
    let project_id = register_runner_project_at_path_with_capabilities(
        &runtime,
        "plugin-sidecar-scope",
        "repo",
        root.path(),
        capabilities,
    )
    .await;
    let resolver_auth = auth_context(None, true);
    let project = runtime
        .resolve_project_input_for_auth(&project_id, Some(&resolver_auth))
        .await
        .unwrap();
    let mut auth = auth_context(None, false);
    auth.scopes
        .push(crate::auth::SCOPE_PROJECT_READ.to_string());
    let mut result = ToolResult::ok(json!({"main_observation": "success"}));
    runtime
        .add_requested_context_projection(
            &mut result,
            &["plugins.catalog".to_string()],
            Some(&project),
            Some(&auth),
            super::super::context_projection::ContextMaterialCapabilities::default(),
        )
        .await;
    assert!(result.success);
    assert_eq!(result.output["main_observation"], "success");
    let material = context_material(&result, "plugins.catalog");
    assert_eq!(material["status"], "unavailable");
    assert_eq!(material["reason_code"], "plugin_inspect_scope_unavailable");
    assert!(material.get("projection").is_none());
    assert!(
        probe_agent_request_for_instance(&runtime, "plugin-sidecar-scope", "inst")
            .await
            .is_none(),
        "scope denial must fail closed before Plugin inventory dispatch"
    );
}

#[tokio::test]
async fn plugins_catalog_sidecar_is_project_scoped_bounded_and_creates_no_binding() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let mut capabilities = RunnerCapabilities::default();
    capabilities.native_tool_plugins = true;
    let project_id = register_runner_project_at_path_with_capabilities(
        &runtime,
        "plugin-sidecar",
        "repo",
        root.path(),
        capabilities,
    )
    .await;
    let auth = auth_context(None, true);
    let project = runtime
        .resolve_project_input_for_auth(&project_id, Some(&auth))
        .await
        .unwrap();
    let bindings_before = runtime.plugin_gateway.binding_count();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            let mut result = ToolResult::ok(json!({"main_observation": "success"}));
            runtime
                .add_requested_context_projection(
                    &mut result,
                    &["plugins.catalog".to_string()],
                    Some(&project),
                    Some(&auth),
                    super::super::context_projection::ContextMaterialCapabilities::default(),
                )
                .await;
            result
        }
    });
    let request = wait_for_runner_request_for_instance(&runtime, "plugin-sidecar", "inst").await;
    assert!(matches!(
        request.plugin_gateway,
        Some(PluginGatewayRequest::ProjectCatalog { ref project_id }) if project_id == "repo"
    ));
    complete_plugin_catalog_request(&runtime, request, plugin_catalog(64)).await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["main_observation"], "success");
    let material = context_material(&result, "plugins.catalog");
    assert_eq!(material["status"], "available");
    assert_eq!(material["projection"]["total_count"], 64);
    assert_eq!(material["projection"]["truncated"], true);
    assert!(material["projection"]["returned_count"].as_u64().unwrap() < 64);
    assert!(material["projection"].get("next_cursor").is_none());
    assert!(material["projection"]
        .to_string()
        .contains("plugin_tool list and describe"));
    assert!(
        serde_json::to_vec(&material["projection"]).unwrap().len()
            <= crate::plugin_gateway::MAX_PLUGIN_CATALOG_CONTEXT_BYTES
    );
    assert_eq!(runtime.plugin_gateway.binding_count(), bindings_before);
    let serialized = material.to_string();
    for forbidden in [
        root.path().to_string_lossy().as_ref(),
        "inputSchema",
        "outputSchema",
        "provider_instance_id",
        "binding",
        "command",
        "argv",
        "cwd",
        "env",
        "stderr",
        "pid",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "leaked {forbidden}: {serialized}"
        );
    }
}

#[tokio::test]
async fn plugins_catalog_sidecar_reports_plugin_runtime_unavailable_nonfatally() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let project_id =
        register_runner_project_at_path(&runtime, "plugin-sidecar-no-runtime", "repo", root.path())
            .await;
    let auth = auth_context(None, true);
    let project = runtime
        .resolve_project_input_for_auth(&project_id, Some(&auth))
        .await
        .unwrap();
    let mut result = ToolResult::ok(json!({"main_observation": "success"}));
    runtime
        .add_requested_context_projection(
            &mut result,
            &["plugins.catalog".to_string()],
            Some(&project),
            Some(&auth),
            super::super::context_projection::ContextMaterialCapabilities::default(),
        )
        .await;
    assert!(result.success);
    assert_eq!(result.output["main_observation"], "success");
    let material = context_material(&result, "plugins.catalog");
    assert_eq!(material["status"], "unavailable");
    assert_eq!(material["reason_code"], "plugin_runtime_unavailable");
    assert!(material.get("projection").is_none());
    assert!(
        probe_agent_request_for_instance(&runtime, "plugin-sidecar-no-runtime", "inst")
            .await
            .is_none()
    );
}

#[test]
fn plugins_catalog_selection_projection_has_independent_hard_bound() {
    let projection = crate::plugin_gateway::project_plugin_catalog_projection(
        &plugin_catalog(128),
        crate::plugin_gateway::MAX_PLUGIN_CATALOG_CONTEXT_BYTES,
    );
    let bytes = serde_json::to_vec(&projection).unwrap().len();
    assert!(bytes <= crate::plugin_gateway::MAX_PLUGIN_CATALOG_CONTEXT_BYTES);
    assert_eq!(projection["total_count"], 128);
    assert_eq!(projection["truncated"], true);
    assert!(projection["returned_count"].as_u64().unwrap() < 128);
    assert!(projection.get("next_cursor").is_none());
}

#[tokio::test]
async fn context_projection_coexists_with_session_continuity_and_attention() {
    use crate::tool_runtime::sessions::{
        PostSessionMessageInput, SessionMessageKind, SessionMessagePriority,
    };
    let runtime = ToolRuntime::new_for_tests();
    let session = runtime
        .sessions
        .start_session(None, Some("sidecar overlay".to_string()));
    runtime
        .sessions
        .post_message_with_ack(
            PostSessionMessageInput {
                session_id: session.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: "retain guidance ACK semantics".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();
    let outcome = runtime
        .call_tool_with_invocation_metadata(
            ToolCallRequest {
                tool_name: "list_tools".to_string(),
                arguments: json!({}),
            },
            ToolCallContext {
                transport: ToolTransport::Mcp,
                session_id: Some(&session.session_id),
                auth: None,
                window: None,
                record_oauth_scope_denials: false,
                host_file_import_trust: HostFileImportTrust::Untrusted,
            },
            ToolInvocationMetadata {
                context_request: vec!["webcodex.workflow".to_string()],
                ack_session_context_revision: SessionContextRevisionAck::Revision(0),
                ..Default::default()
            },
            ToolProtocolCapabilities {
                context_continuity: true,
                context_sidecar: true,
                ..Default::default()
            },
        )
        .await;
    let result = outcome.result.expect("model-facing result");
    assert!(result.success);
    assert!(result.output.get("session_context_revision").is_none());
    assert!(result.output.get("session_continuity").is_none());
    assert!(result.output.get("session_recovery").is_none());
    assert!(result.output["session_attention"]["requires_ack"].as_bool() == Some(true));
    assert_eq!(result.output["context_projection"]["timing"], "post_tool");
    assert_eq!(
        context_material(&result, "webcodex.workflow")["status"],
        "available"
    );
    let audit = serde_json::to_string(
        &runtime
            .sessions
            .summary(&session.session_id, Some(20))
            .unwrap()
            .events,
    )
    .unwrap();
    assert!(!audit.contains("context_request"));
    assert!(!audit.contains("__webcodex_stateless_context_request"));
    assert!(!audit.contains("context_projection"));
    assert!(!audit.contains("webcodex.coding_workflow"));
}
