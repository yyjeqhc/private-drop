use super::*;
use std::sync::Arc;
use webcodex_core::ssh_resource::{
    SshResourceInventoryEntry, SshResourceResponse, SshResourceSource,
};

fn ssh_auth() -> crate::auth::AuthContext {
    let mut auth = mcp_export_api_auth("ssh-resource-test-pat", "alice");
    auth.user_id = Some("ssh-resource-test-user-alice".to_string());
    auth.scopes.push(crate::auth::SCOPE_SSH_LOCAL.to_string());
    auth
}

async fn register_managed_runner(runtime: &ToolRuntime, instance: &str) {
    let mut capabilities = RunnerCapabilities::default();
    capabilities.managed_ssh_resources = true;
    runtime
        .runner_registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                client_id: "runner-a".to_string(),
                runner_instance_id: instance.to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some("SSH Resource Runner".to_string()),
                owner: Some("alice".to_string()),
                hostname: None,
                host_context: None,
                capabilities,
                policy: None,
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
            },
        ))
        .await
        .unwrap();
}

async fn wait_for_request(
    runtime: &ToolRuntime,
    instance: &str,
) -> crate::runner_protocol::RunnerRequest {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(request) = runtime
            .runner_registry
            .poll(RunnerPollRequest {
                client_id: "runner-a".to_string(),
                runner_instance_id: instance.to_string(),
            })
            .await
            .unwrap()
        {
            return request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "SSH resource request did not dispatch"
        );
        tokio::task::yield_now().await;
    }
}

async fn complete_response(
    runtime: &ToolRuntime,
    request: crate::runner_protocol::RunnerRequest,
    instance: &str,
    response: SshResourceResponse,
) {
    runtime
        .runner_registry
        .complete(crate::runner_protocol::RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: request.client_id,
                runner_instance_id: instance.to_string(),
                request_id: request.request_id,
                exit_code: Some(0),
                stdout: Some(serde_json::to_string(&response).unwrap()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            },
            command_execution_state: Some(
                crate::runner_protocol::ShellCommandExecutionState::Completed,
            ),
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
        })
        .await
        .unwrap();
}

async fn call_in_task(
    runtime: Arc<ToolRuntime>,
    auth: crate::auth::AuthContext,
    arguments: Value,
    id: u64,
) -> tokio::task::JoinHandle<McpOutcome> {
    tokio::spawn(async move {
        handle_mcp_request(
            &runtime,
            rpc(
                "tools/call",
                Some(json!(id)),
                json!({
                    "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
                    "arguments": arguments
                }),
            ),
            Some(&auth),
        )
        .await
    })
}

fn tool_result(outcome: McpOutcome) -> Value {
    let McpOutcome::Ok(value) = outcome else {
        panic!("expected normal MCP tool result: {outcome:?}");
    };
    value["result"].clone()
}

async fn list_binding(
    runtime: &Arc<ToolRuntime>,
    auth: &crate::auth::AuthContext,
    revision: u64,
) -> String {
    let task = call_in_task(
        Arc::clone(runtime),
        auth.clone(),
        json!({"action":"list","runner":"runner-a"}),
        801,
    )
    .await;
    let request = wait_for_request(runtime, "instance-a").await;
    assert_eq!(request.kind, "ssh_resource");
    assert!(!request
        .content
        .as_deref()
        .unwrap_or_default()
        .contains("target"));
    complete_response(
        runtime,
        request,
        "instance-a",
        SshResourceResponse::List {
            revision,
            resources: vec![SshResourceInventoryEntry {
                name: "spe".to_string(),
                source: SshResourceSource::Static,
                active: true,
                pending_restart: false,
            }],
        },
    )
    .await;
    let result = tool_result(task.await.unwrap());
    assert_eq!(result["isError"], false);
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(!serialized.contains("17724@w10"));
    result["structuredContent"]["binding"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn managed_ssh_list_register_keeps_target_out_of_model_result() {
    let runtime = Arc::new(test_runtime());
    let auth = ssh_auth();
    register_managed_runner(&runtime, "instance-a").await;
    let binding = list_binding(&runtime, &auth, 0).await;

    let target = "17724@w10";
    let task = call_in_task(
        Arc::clone(&runtime),
        auth,
        json!({
            "action":"register",
            "binding":binding,
            "name":"w10",
            "target":target
        }),
        802,
    )
    .await;
    let request = wait_for_request(&runtime, "instance-a").await;
    assert_eq!(request.kind, "ssh_resource");
    assert!(request.content.as_deref().unwrap().contains(target));
    assert!(request.command.is_empty());
    assert!(request.path.is_none());
    complete_response(
        &runtime,
        request,
        "instance-a",
        SshResourceResponse::Register {
            revision: 1,
            resource: "w10".to_string(),
            persisted: true,
            active: false,
            restart_required: true,
        },
    )
    .await;
    let result = tool_result(task.await.unwrap());
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(!serialized.contains(target));
    assert_eq!(result["structuredContent"]["resource"], "w10");
    assert_eq!(result["structuredContent"]["persisted"], true);
    assert_eq!(result["structuredContent"]["active"], false);
    assert_eq!(result["structuredContent"]["restart_required"], true);
}

#[tokio::test]
async fn managed_ssh_stale_revision_invalidates_binding_without_lost_update() {
    let runtime = Arc::new(test_runtime());
    let auth = ssh_auth();
    register_managed_runner(&runtime, "instance-a").await;
    let binding = list_binding(&runtime, &auth, 7).await;

    let task = call_in_task(
        Arc::clone(&runtime),
        auth.clone(),
        json!({
            "action":"remove",
            "binding":binding,
            "name":"old"
        }),
        803,
    )
    .await;
    let request = wait_for_request(&runtime, "instance-a").await;
    complete_response(
        &runtime,
        request,
        "instance-a",
        SshResourceResponse::Error {
            code: "ssh_resource_registry_stale".to_string(),
            message: "safe".to_string(),
        },
    )
    .await;
    let result = tool_result(task.await.unwrap());
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "ssh_resource_registry_stale"
    );

    // The stale binding was discarded. A second use fails before enqueue, so
    // no lost update can be replayed against a newer registry revision.
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(804)),
            json!({
                "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
                "arguments": {"action":"remove","binding":binding,"name":"old"}
            }),
        ),
        Some(&auth),
    )
    .await;
    let result = tool_result(outcome);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "ssh_resource_binding_required"
    );
    assert!(runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: "runner-a".to_string(),
            runner_instance_id: "instance-a".to_string(),
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn managed_ssh_binding_rejects_runner_instance_replacement() {
    let runtime = Arc::new(test_runtime());
    let auth = ssh_auth();
    register_managed_runner(&runtime, "instance-a").await;
    let binding = list_binding(&runtime, &auth, 1).await;

    register_managed_runner(&runtime, "instance-b").await;
    let outcome = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(805)),
            json!({
                "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
                "arguments": {
                    "action":"register",
                    "binding":binding,
                    "name":"w10",
                    "target":"17724@w10"
                }
            }),
        ),
        Some(&auth),
    )
    .await;
    let result = tool_result(outcome);
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "runner_replaced"
    );
    assert!(runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: "runner-a".to_string(),
            runner_instance_id: "instance-b".to_string(),
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn managed_ssh_invalid_post_dispatch_response_is_outcome_unknown_and_binding_is_retired() {
    let runtime = Arc::new(test_runtime());
    let auth = ssh_auth();
    register_managed_runner(&runtime, "instance-a").await;
    let binding = list_binding(&runtime, &auth, 3).await;

    let task = call_in_task(
        Arc::clone(&runtime),
        auth.clone(),
        json!({
            "action":"register",
            "binding":binding,
            "name":"w10",
            "target":"17724@w10"
        }),
        806,
    )
    .await;
    let request = wait_for_request(&runtime, "instance-a").await;
    complete_response(
        &runtime,
        request,
        "instance-a",
        SshResourceResponse::Register {
            revision: 4,
            resource: "different-name".to_string(),
            persisted: true,
            active: false,
            restart_required: true,
        },
    )
    .await;
    let result = tool_result(task.await.unwrap());
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "ssh_resource_outcome_unknown"
    );

    let retry = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(807)),
            json!({
                "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
                "arguments": {
                    "action":"register",
                    "binding":binding,
                    "name":"w10",
                    "target":"17724@w10"
                }
            }),
        ),
        Some(&auth),
    )
    .await;
    let result = tool_result(retry);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "ssh_resource_binding_required"
    );
}

#[tokio::test]
async fn read_only_session_allows_ssh_inspect_but_denies_management_before_runner_dispatch() {
    let runtime = Arc::new(test_runtime());
    let auth = ssh_auth();
    register_managed_runner(&runtime, "instance-a").await;
    let session =
        start_authorized_test_session(&runtime, &auth, crate::tool_runtime::SessionMode::ReadOnly);

    let list_task = call_in_task(
        Arc::clone(&runtime),
        auth.clone(),
        json!({
            "action":"list",
            "runner":"runner-a",
            "recording_session_id":session.session_id
        }),
        808,
    )
    .await;
    let list_request = wait_for_request(&runtime, "instance-a").await;
    complete_response(
        &runtime,
        list_request,
        "instance-a",
        SshResourceResponse::List {
            revision: 5,
            resources: vec![],
        },
    )
    .await;
    let list_result = tool_result(list_task.await.unwrap());
    assert_eq!(list_result["isError"], false);
    let binding = list_result["structuredContent"]["binding"]
        .as_str()
        .unwrap()
        .to_string();

    let denied = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(809)),
            json!({
                "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
                "arguments": {
                    "action":"register",
                    "binding":binding,
                    "name":"w10",
                    "target":"private-user@private-host",
                    "recording_session_id":session.session_id
                }
            }),
        ),
        Some(&auth),
    )
    .await;
    let result = tool_result(denied);
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["output"]["error_kind"],
        "session_guard_denied"
    );
    assert_eq!(
        result["structuredContent"]["output"]["dispatch_certainty"],
        "not_started"
    );
    assert!(!serde_json::to_string(&result)
        .unwrap()
        .contains("private-user@private-host"));
    let ledger = format!(
        "{:?}",
        runtime.sessions.summary(&session.session_id, Some(100))
    );
    assert!(!ledger.contains("private-user@private-host"));
    assert!(runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: "runner-a".to_string(),
            runner_instance_id: "instance-a".to_string(),
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn ssh_resource_does_not_accept_stateless_continuity_wrappers() {
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let auth = ssh_auth();
    register_managed_runner(&runtime, "instance-a").await;

    for (id, arguments) in [
        (
            811,
            json!({
                "action":"list",
                "runner":"runner-a",
                crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD: ["wc_msg_cached"]
            }),
        ),
        (
            812,
            json!({
                "action":"list",
                "runner":"runner-a",
                crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD: 7
            }),
        ),
        (
            813,
            json!({
                "action":"list",
                "runner":"runner-a",
                crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD: ["webcodex.workflow"]
            }),
        ),
        (
            814,
            json!({
                "action":"list",
                "runner":"runner-a",
                crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: {
                    "message_id": "wc_msg_cached",
                    "resolution": "handled"
                }
            }),
        ),
    ] {
        let outcome = handle_mcp_request(
            &runtime,
            rpc(
                "tools/call",
                Some(json!(id)),
                mcp_2026_params(json!({
                    "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
                    "arguments": arguments
                })),
            ),
            Some(&auth),
        )
        .await;
        let result = tool_result(outcome);
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(
            result["structuredContent"]["error"]["code"], "ssh_resource_invalid",
            "generic wrappers must remain invalid specialized SSH arguments"
        );
        assert!(runtime
            .runner_registry
            .poll(RunnerPollRequest {
                client_id: "runner-a".to_string(),
                runner_instance_id: "instance-a".to_string(),
            })
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn restricted_permission_denies_ssh_management_before_runner_dispatch() {
    let runtime = Arc::new(test_runtime().with_permission_evaluator(
        crate::tool_runtime::PermissionEvaluator::with_mode(
            crate::tool_runtime::AuthorityMode::Restricted,
        ),
    ));
    let auth = ssh_auth();
    register_managed_runner(&runtime, "instance-a").await;

    let denied = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(810)),
            json!({
                "name": crate::ssh_resource_gateway::SSH_RESOURCE_TOOL_NAME,
                "arguments": {
                    "action":"register",
                    "binding":"wc_sshbind_0123456789abcdef0123456789abcdef",
                    "name":"w10",
                    "target":"private-user@private-host"
                }
            }),
        ),
        Some(&auth),
    )
    .await;
    let result = tool_result(denied);
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["output"]["failure_kind"],
        "permission_denied"
    );
    assert_eq!(
        result["structuredContent"]["output"]["dispatch_certainty"],
        "not_started"
    );
    assert!(!serde_json::to_string(&result)
        .unwrap()
        .contains("private-user@private-host"));
    assert!(runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: "runner-a".to_string(),
            runner_instance_id: "instance-a".to_string(),
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn generic_runtime_ssh_resource_dispatch_preserves_native_requests_and_results() {
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolTransport,
    };
    use webcodex_core::ssh_resource::SshResourceRequest;

    let runtime = Arc::new(test_runtime());
    let auth = ssh_auth();
    register_managed_runner(&runtime, "instance-a").await;
    let mut binding = String::new();
    for (arguments, expected, response) in [
        (
            json!({"action":"list", "runner":"runner-a"}),
            SshResourceRequest::List,
            SshResourceResponse::List {
                revision: 0,
                resources: vec![],
            },
        ),
        (
            json!({"action":"register", "name":"test", "target":"user@host"}),
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "test".into(),
                target: "user@host".into(),
                default_cwd: None,
            },
            SshResourceResponse::Register {
                revision: 1,
                resource: "test".into(),
                persisted: true,
                active: false,
                restart_required: true,
            },
        ),
        (
            json!({"action":"list", "runner":"runner-a"}),
            SshResourceRequest::List,
            SshResourceResponse::List {
                revision: 1,
                resources: vec![],
            },
        ),
        (
            json!({"action":"remove", "name":"test"}),
            SshResourceRequest::Remove {
                expected_revision: 1,
                name: "test".into(),
            },
            SshResourceResponse::Remove {
                revision: 2,
                resource: "test".into(),
                persisted: true,
                active: true,
                restart_required: true,
            },
        ),
    ] {
        let mut arguments = arguments;
        let read = arguments["action"] == "list";
        if !read {
            arguments["binding"] = json!(binding);
        }
        let task = {
            let runtime = Arc::clone(&runtime);
            let auth = auth.clone();
            tokio::spawn(async move {
                runtime
                    .call_tool_with_context(
                        ToolCallRequest {
                            tool_name: "ssh_resource".into(),
                            arguments,
                        },
                        ToolCallContext {
                            transport: ToolTransport::Api,
                            session_id: None,
                            auth: Some(&auth),
                            window: None,
                            record_oauth_scope_denials: true,
                            host_file_import_trust: HostFileImportTrust::Untrusted,
                        },
                    )
                    .await
            })
        };
        let request = wait_for_request(&runtime, "instance-a").await;
        assert_eq!(request.kind, "ssh_resource");
        assert!(request.plugin_gateway.is_none());
        assert_eq!(
            serde_json::from_str::<SshResourceRequest>(request.content.as_deref().unwrap())
                .unwrap(),
            expected
        );
        complete_response(&runtime, request, "instance-a", response).await;
        let outcome = task.await.unwrap();
        assert!(outcome.success, "{outcome:?}");
        assert!(outcome.error_status.is_none());
        let output = outcome.result.unwrap().output;
        if read {
            binding = output["binding"].as_str().unwrap().to_string();
        } else {
            assert_eq!(output["persisted"], true);
            assert_eq!(output["restart_required"], true);
            assert!(!output.to_string().contains("user@host"));
        }
        assert!(
            runtime
                .runner_registry
                .poll(RunnerPollRequest {
                    client_id: "runner-a".into(),
                    runner_instance_id: "instance-a".into(),
                })
                .await
                .unwrap()
                .is_none(),
            "one kernel invocation must dispatch exactly once"
        );
    }
}
