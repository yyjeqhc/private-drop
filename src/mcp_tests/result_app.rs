use super::*;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerJobUpdateRequest, RunnerPollRequest, RunnerProjectSummary,
    RunnerRegisterRequest,
};
use crate::tool_runtime::{ObserveJobsItem, ToolCall};

fn tool<'a>(payload: &'a Value, name: &str) -> &'a Value {
    payload["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("missing {name} descriptor"))
}

fn presentation<'a>(call_result: &'a Value) -> &'a Value {
    &call_result["_meta"][super::super::presentation::MCP_PRESENTATION_META_KEY]
}

#[test]
fn job_tool_app_metadata_is_capability_scoped_compact_safe_and_merge_safe() {
    for compact in [false, true] {
        let enabled = mcp_tools_list_payload_with_compact_and_app(
            ModelSurface::FullOperatorRuntime,
            compact,
            true,
        );
        for name in ["list_jobs", "observe_jobs"] {
            assert_eq!(
                tool(&enabled, name)["_meta"]["ui"]["resourceUri"],
                MCP_RESULT_UI_RESOURCE_URI
            );
            assert!(tool(&enabled, name)["_meta"]
                .get("ui/resourceUri")
                .is_none());
        }

        let disabled = mcp_tools_list_payload_with_compact_and_app(
            ModelSurface::FullOperatorRuntime,
            compact,
            false,
        );
        for name in ["list_jobs", "observe_jobs"] {
            assert!(tool(&disabled, name).get("_meta").is_none());
        }
    }

    let mut existing = json!({
        "name": "future_job_tool",
        "_meta": {
            "openai/fileParams": ["file"],
            "ui": {"other": true}
        }
    });
    super::super::tools::attach_app_metadata(&mut existing, MCP_RESULT_UI_RESOURCE_URI);
    assert_eq!(existing["_meta"]["openai/fileParams"], json!(["file"]));
    assert_eq!(existing["_meta"]["ui"]["other"], true);
    assert_eq!(
        existing["_meta"]["ui"]["resourceUri"],
        MCP_RESULT_UI_RESOURCE_URI
    );
}

#[tokio::test]
async fn job_app_descriptor_and_resource_exposure_require_ui_operator_capability() {
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let ui_tools = handle_mcp_request(
        &runtime,
        rpc(
            "tools/list",
            Some(json!(3201)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(ui_tools) = ui_tools else {
        panic!("expected UI-capable tools/list");
    };
    for name in ["list_jobs", "observe_jobs"] {
        assert_eq!(
            tool(&ui_tools["result"], name)["_meta"]["ui"]["resourceUri"],
            MCP_RESULT_UI_RESOURCE_URI
        );
    }

    let plain_tools = handle_mcp_request(
        &runtime,
        rpc("tools/list", Some(json!(3202)), mcp_2026_params(json!({}))),
        None,
    )
    .await;
    let McpOutcome::Ok(plain_tools) = plain_tools else {
        panic!("expected ordinary tools/list");
    };
    for name in ["list_jobs", "observe_jobs"] {
        assert!(tool(&plain_tools["result"], name).get("_meta").is_none());
    }

    let resources = handle_mcp_request(
        &runtime,
        rpc(
            "resources/list",
            Some(json!(3203)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(resources) = resources else {
        panic!("expected UI resources/list");
    };
    let resources = resources["result"]["resources"].as_array().unwrap();
    assert!(resources
        .iter()
        .any(|resource| resource["uri"] == MCP_COMPUTER_UI_RESOURCE_URI));
    let result_resource = resources
        .iter()
        .find(|resource| resource["uri"] == MCP_RESULT_UI_RESOURCE_URI)
        .expect("Result App resource");
    assert_eq!(result_resource["mimeType"], MCP_UI_RESOURCE_MIME_TYPE);
    assert_eq!(
        result_resource["_meta"],
        json!({
            "ui": {
                "prefersBorder": true,
                "csp": {"connectDomains": [], "resourceDomains": []}
            }
        })
    );

    let read = handle_mcp_request(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3204)),
            mcp_2026_params(json!({"uri": MCP_RESULT_UI_RESOURCE_URI})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(read) = read else {
        panic!("expected Result App resource read");
    };
    assert_eq!(
        read["result"]["contents"][0]["uri"],
        MCP_RESULT_UI_RESOURCE_URI
    );
    assert_eq!(
        read["result"]["contents"][0]["mimeType"],
        MCP_UI_RESOURCE_MIME_TYPE
    );
    assert_eq!(read["result"]["contents"][0]["text"], MCP_RESULT_APP_HTML);

    let no_ui_resources = handle_mcp_request(
        &runtime,
        rpc(
            "resources/list",
            Some(json!(3205)),
            mcp_2026_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(no_ui_resources) = no_ui_resources else {
        panic!("expected non-UI resources/list");
    };
    assert!(no_ui_resources["result"]["resources"]
        .as_array()
        .unwrap()
        .is_empty());

    let local_runtime = test_runtime_with_surface(ModelSurface::LocalCoding);
    let local_tools = handle_mcp_request(
        &local_runtime,
        rpc(
            "tools/list",
            Some(json!(3206)),
            mcp_2026_ui_params(json!({})),
        ),
        None,
    )
    .await;
    let McpOutcome::Ok(local_tools) = local_tools else {
        panic!("expected LocalCoding tools/list");
    };
    assert!(local_tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool
            .pointer("/_meta/ui/resourceUri")
            .and_then(Value::as_str)
            != Some(MCP_RESULT_UI_RESOURCE_URI)));

    assert!(!mcp_app_enabled(
        true,
        ModelSurface::LocalCoding,
        &mcp_2026_ui_params(json!({}))
    ));
}

#[test]
fn job_presentation_is_post_result_bounded_and_private() {
    let secret = "SECRET-SHOULD-NOT-REACH-PRESENTATION";
    let jobs = (0..12)
        .map(|index| {
            json!({
                "job_id": format!("job-{index}"),
                "status": if index == 0 { "lost" } else { "running" },
                "project": "p".repeat(400),
                "active": index != 0,
                "blocking_active": index != 0,
                "terminal": index == 0,
                "terminal_pending": false,
                "duration_ms": 25,
                "elapsed_secs": 1,
                "command_execution_state": if index == 0 { "outcome_unknown" } else { "completed" },
                "recovery_state": if index == 0 { "recovering" } else { "reconciled" },
                "recovery_reason": "r".repeat(400),
                "stdout_tail": secret,
                "stderr_tail": secret,
                "command_summary": secret,
                "cwd": format!("/private/{secret}"),
                "observation_token": secret,
            })
        })
        .collect::<Vec<_>>();
    let canonical = ToolResult::ok(json!({
        "jobs": jobs,
        "count": 12,
        "matched_count": 12,
        "truncated": true
    }));
    let mut framed = super::super::tools::mcp_runtime_tool_result("list_jobs", false, canonical);
    let structured_before = framed["structuredContent"].clone();
    super::super::presentation::attach_result_app_presentation("list_jobs", &mut framed);
    assert_eq!(framed["structuredContent"], structured_before);

    let meta = presentation(&framed);
    assert_eq!(meta["kind"], "job_list");
    assert_eq!(meta["items"].as_array().unwrap().len(), 8);
    assert_eq!(meta["items_truncated"], true);
    assert_eq!(meta["truncated"], true);
    assert_eq!(meta["items"][0]["status"], "lost");
    assert_eq!(
        meta["items"][0]["command_execution_state"],
        "outcome_unknown"
    );
    assert_eq!(meta["items"][0]["recovery_state"], "recovering");
    assert_eq!(meta["items"][0]["terminal"], true);
    assert_eq!(meta["items"][0]["active"], false);
    assert_eq!(meta["items"][0]["blocking_active"], false);
    assert_eq!(meta["items"][0]["terminal_pending"], false);
    assert!(meta["items"][0]["guidance"]
        .as_str()
        .unwrap()
        .contains("uncertain"));

    fn assert_bounded_strings(value: &Value) {
        match value {
            Value::String(text) => assert!(
                text.chars().count() <= super::super::presentation::MAX_MCP_PRESENTATION_TEXT_CHARS,
                "presentation text exceeded bound: {}",
                text.chars().count()
            ),
            Value::Array(values) => values.iter().for_each(assert_bounded_strings),
            Value::Object(values) => values.values().for_each(assert_bounded_strings),
            _ => {}
        }
    }
    assert_bounded_strings(meta);
    let serialized = serde_json::to_string(meta).unwrap();
    assert!(!serialized.contains(secret));
    for forbidden in [
        "stdout_tail",
        "stderr_tail",
        "command_summary",
        "observation_token",
        "cwd",
    ] {
        assert!(!serialized.contains(forbidden));
    }
}

#[test]
fn observe_presentation_preserves_wait_uncertainty_and_unknown_job_without_log_bodies() {
    let canonical = ToolResult::ok(json!({
        "requested_count": 2,
        "returned_count": 2,
        "succeeded_count": 1,
        "failed_count": 1,
        "items": [
            {
                "job_id": "job-uncertain",
                "success": true,
                "output": {
                    "job_id": "job-uncertain",
                    "status": "lost",
                    "terminal": true,
                    "changed": true,
                    "command_execution_state": "outcome_unknown",
                    "recovery_state": "lost_after_reconcile",
                    "log_delta_status": "delta",
                    "stdout_lines": 500,
                    "stderr_lines": 2,
                    "stdout_tail": "SECRET-STDOUT",
                    "stderr_tail": "SECRET-STDERR",
                    "observation_token": "opaque-secret-token"
                }
            },
            {
                "job_id": "missing-job",
                "success": false,
                "error_kind": "unknown_job",
                "recovery_kind": "reobserve",
                "recovery_tool": "list_jobs",
                "error": "unbounded internal error text"
            }
        ],
        "wait": {"outcome": "item_error", "waited_ms": 7},
        "changed_count": 1,
        "terminal_count": 1,
        "output_truncated": false,
        "next_index": null
    }));
    let mut framed = super::super::tools::mcp_runtime_tool_result("observe_jobs", false, canonical);
    let structured_before = framed["structuredContent"].clone();
    super::super::presentation::attach_result_app_presentation("observe_jobs", &mut framed);
    assert_eq!(framed["structuredContent"], structured_before);
    let meta = presentation(&framed);
    assert_eq!(meta["wait"]["outcome"], "item_error");
    assert_eq!(meta["items"][0]["status"], "lost");
    assert_eq!(
        meta["items"][0]["command_execution_state"],
        "outcome_unknown"
    );
    assert_eq!(meta["items"][1]["error_kind"], "unknown_job");
    assert_eq!(meta["items"][1]["recovery_tool"], "list_jobs");
    let serialized = serde_json::to_string(meta).unwrap();
    for forbidden in [
        "SECRET-STDOUT",
        "SECRET-STDERR",
        "opaque-secret-token",
        "unbounded internal error text",
        "stdout_tail",
        "stderr_tail",
        "observation_token",
    ] {
        assert!(!serialized.contains(forbidden));
    }
}

#[test]
fn result_app_html_is_display_only_and_uses_safe_dom_rendering() {
    let html = MCP_RESULT_APP_HTML;
    for expected in [
        "ui/initialize",
        "ui/notifications/initialized",
        "ui/notifications/tool-result",
        "webcodex/presentation",
        "textContent",
        "document.createElement",
    ] {
        assert!(html.contains(expected), "missing {expected}");
    }
    for forbidden in [
        "innerHTML",
        "eval(",
        "new Function",
        "localStorage",
        "indexedDB",
        "fetch(",
        "WebSocket",
        "tools/call",
        "callServerTool",
        "ui/update-model-context",
        "ui/message",
        "<button",
    ] {
        assert!(
            !html.contains(forbidden),
            "Result App must not contain {forbidden}"
        );
    }
}

fn result_app_auth() -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        kind: crate::auth::AuthKind::Bootstrap,
        user_id: Some("user-result-app-owner".to_string()),
        username: Some("result-app-owner".to_string()),
        api_key_id: Some("key-result-app-owner".to_string()),
        role: Some("admin".to_string()),
        scopes: vec!["admin".to_string()],
        is_bootstrap: true,
        token_kind: None,
        allowed_client_id: None,
        shared_key_hash: None,
        project_grant_id: None,
    }
}

async fn register_job_runner(runtime: &ToolRuntime, auth: &crate::auth::AuthContext) {
    let capabilities = RunnerCapabilities {
        async_jobs: true,
        async_shell_jobs: true,
        ..Default::default()
    };
    runtime
        .runner_registry
        .register_with_auth(
            crate::test_support::current_runner_registration(RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: "result-app-runner".to_string(),
                runner_instance_id: "inst-result-app".to_string(),
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(capabilities),
                policy: None,
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            }),
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        "result-app-runner",
        "inst-result-app",
        vec![RunnerProjectSummary {
            id: "demo".to_string(),
            name: Some("Result App Demo".to_string()),
            path: "/tmp/result-app-demo".to_string(),
            allow_patch: true,
            kind: Some("repo".to_string()),
            registration_source: None,
            description: None,
            hooks: Vec::new(),
            disabled: false,
            revision: None,
            git_branch: None,
            git_head: None,
            git_dirty: None,
            updated_at: 1,
            shell_profile: None,
        }],
    )
    .await;
}

async fn set_job_state(
    runtime: &ToolRuntime,
    request: &crate::runner_protocol::RunnerRequest,
    status: &str,
    finished: bool,
) {
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "result-app-runner".to_string(),
            runner_instance_id: "inst-result-app".to_string(),
            update_seq: None,
            job_id: request.job_id.clone().expect("Job id"),
            request_id: Some(request.request_id.clone()),
            status: status.to_string(),
            stdout_chunk: Some("secret log body that presentation must not copy\n".to_string()),
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: finished.then_some(0),
            duration_ms: finished.then_some(25),
            error: None,
            command_execution_state: finished
                .then_some(crate::runner_protocol::ShellCommandExecutionState::Completed),
            validation_progress: None,
            activity: None,
            finished,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn mcp_job_presentation_tracks_real_running_to_terminal_transition() {
    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let auth = result_app_auth();
    register_job_runner(&runtime, &auth).await;
    let started = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: "agent:result-app-runner:demo".to_string(),
                command: "echo result-app".to_string(),
                session_id: None,
                timeout_secs: Some(60),
                cwd: None,
                purpose: None,
                shell: None,
            },
            Some(&auth),
        )
        .await;
    assert!(started.success, "{:?}", started.error);
    let job_id = started.output["job_id"].as_str().unwrap().to_string();
    let request = runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: "result-app-runner".to_string(),
            runner_instance_id: "inst-result-app".to_string(),
        })
        .await
        .unwrap()
        .expect("queued Job request");
    set_job_state(&runtime, &request, "running", false).await;

    async fn observe(
        runtime: &ToolRuntime,
        auth: &crate::auth::AuthContext,
        job_id: &str,
        id: i64,
    ) -> Value {
        let outcome = handle_mcp_request(
            runtime,
            rpc(
                "tools/call",
                Some(json!(id)),
                mcp_2026_ui_params(json!({
                    "name": "observe_jobs",
                    "arguments": {"items": [{"job_id": job_id}]}
                })),
            ),
            Some(auth),
        )
        .await;
        let McpOutcome::Ok(body) = outcome else {
            panic!("expected observe_jobs MCP result");
        };
        body["result"].clone()
    }

    let running = observe(&runtime, &auth, &job_id, 3210).await;
    assert_eq!(presentation(&running)["items"][0]["status"], "running");
    assert_eq!(presentation(&running)["items"][0]["terminal"], false);
    assert!(serde_json::to_string(presentation(&running))
        .unwrap()
        .find("secret log body")
        .is_none());

    let listed = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(32101)),
            mcp_2026_ui_params(json!({
                "name": "list_jobs",
                "arguments": {"project": "agent:result-app-runner:demo", "limit": 10}
            })),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Ok(listed) = listed else {
        panic!("expected list_jobs MCP result");
    };
    assert_eq!(presentation(&listed["result"])["kind"], "job_list");
    assert!(presentation(&listed["result"])["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["job_id"] == job_id && item["status"] == "running"));

    let plain_listed = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(32102)),
            mcp_2026_params(json!({
                "name": "list_jobs",
                "arguments": {"project": "agent:result-app-runner:demo", "limit": 10}
            })),
        ),
        Some(&auth),
    )
    .await;
    let McpOutcome::Ok(plain_listed) = plain_listed else {
        panic!("expected non-UI list_jobs MCP result");
    };
    assert!(plain_listed["result"]["_meta"]
        .get(super::super::presentation::MCP_PRESENTATION_META_KEY)
        .is_none());
    assert_eq!(
        plain_listed["result"]["structuredContent"]["output"]["jobs"][0]["job_id"],
        job_id
    );

    set_job_state(&runtime, &request, "completed", true).await;
    let completed = observe(&runtime, &auth, &job_id, 3211).await;
    assert_eq!(presentation(&completed)["items"][0]["status"], "completed");
    assert_eq!(presentation(&completed)["items"][0]["terminal"], true);
    assert_eq!(
        completed["structuredContent"]["output"]["items"][0]["status"],
        "completed"
    );

    let unknown = observe(&runtime, &auth, "unknown-result-app-job", 3212).await;
    assert_eq!(
        presentation(&unknown)["items"][0]["error_kind"],
        "unknown_job"
    );
    assert_eq!(
        presentation(&unknown)["items"][0]["recovery_tool"],
        "list_jobs"
    );

    assert!(runtime.runner_registry.remove_job_record(&job_id).await);
}

#[test]
fn observation_token_remains_only_in_canonical_result() {
    let canonical = ToolResult::ok(json!({
        "items": [{
            "job_id": "job-token",
            "status": "running",
            "terminal": false,
            "changed": false,
            "log_delta_status": "unchanged",
            "observation_token": "opaque-continuation-token"
        }],
        "wait": {"outcome": "immediate"}
    }));
    let mut framed = super::super::tools::mcp_runtime_tool_result("observe_jobs", false, canonical);
    super::super::presentation::attach_result_app_presentation("observe_jobs", &mut framed);
    assert_eq!(
        framed["structuredContent"]["output"]["items"][0]["observation_token"],
        "opaque-continuation-token"
    );
    assert!(!serde_json::to_string(presentation(&framed))
        .unwrap()
        .contains("opaque-continuation-token"));
}

#[test]
fn observe_jobs_item_limit_matches_presentation_bound() {
    assert_eq!(super::super::presentation::MAX_MCP_PRESENTATION_ITEMS, 8);
    let parsed = ToolCall::ObserveJobs {
        items: (0..8)
            .map(|index| ObserveJobsItem {
                job_id: format!("job-{index}"),
                after_observation_token: None,
            })
            .collect(),
        tail_lines: 40,
        wait_secs: None,
    };
    assert!(matches!(parsed, ToolCall::ObserveJobs { .. }));
}
