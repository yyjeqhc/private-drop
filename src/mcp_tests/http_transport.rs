use super::*;

fn with_mcp_recording_session(mut arguments: Value, session_id: &str) -> Value {
    arguments
        .as_object_mut()
        .expect("tool arguments must be an object")
        .insert(
            crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD.to_string(),
            Value::from(session_id),
        );
    arguments
}

async fn legacy_mcp_jsonrpc(service: &Service, token: &str, body: Value) -> (StatusCode, Value) {
    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth(token)
        .json(&body)
        .send(service)
        .await;
    let status = effective_status(&response);
    let body = response.take_json::<Value>().await.unwrap();
    (status, body)
}

async fn stateless_2026_jsonrpc(
    service: &Service,
    token: &str,
    protocol_header: Option<&str>,
    method_header: Option<&str>,
    name_header: Option<&str>,
    legacy_session_id: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = TestClient::post("http://localhost/mcp").bearer_auth(token);
    if let Some(protocol_header) = protocol_header {
        request = request.add_header(MCP_PROTOCOL_VERSION_HEADER, protocol_header, true);
    }
    if let Some(method_header) = method_header {
        request = request.add_header(MCP_METHOD_HEADER, method_header, true);
    }
    if let Some(name_header) = name_header {
        request = request.add_header(MCP_NAME_HEADER, name_header, true);
    }
    if let Some(legacy_session_id) = legacy_session_id {
        request = request.add_header(
            crate::client_window::MCP_SESSION_HEADER,
            legacy_session_id,
            true,
        );
    }
    let mut response = request.json(&body).send(service).await;
    assert!(
        response
            .headers()
            .get(crate::client_window::MCP_SESSION_HEADER)
            .is_none(),
        "stateless-2026 must never issue a legacy MCP session id"
    );
    let status = effective_status(&response);
    let body = response.take_json::<Value>().await.unwrap();
    (status, body)
}

async fn stateless_2026_tool_call(
    service: &Service,
    token: &str,
    id: i64,
    name: &str,
    arguments: Value,
    legacy_session_id: Option<&str>,
) -> (StatusCode, Value) {
    stateless_2026_jsonrpc(
        service,
        token,
        Some(MCP_STATELESS_PROTOCOL_VERSION),
        Some("tools/call"),
        Some(name),
        legacy_session_id,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": mcp_2026_params(json!({"name": name, "arguments": arguments})),
        }),
    )
    .await
}

async fn stateless_observation_runner_registry() -> Arc<crate::runner_http::RunnerRegistry> {
    let runner_registry = Arc::new(crate::runner_http::RunnerRegistry::default());
    runner_registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: "mcp-observation-agent".to_string(),
                runner_instance_id: "inst-mcp-observation".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: RunnerCapabilities::default(),
                policy: None,
            },
        ))
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runner_registry,
        "mcp-observation-agent",
        "inst-mcp-observation",
        vec![
            crate::runner_protocol::RunnerProjectSummary {
                id: "shared".to_string(),
                name: Some("Shared observation project".to_string()),
                path: "/tmp/mcp-observation-shared".to_string(),
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
            },
            crate::runner_protocol::RunnerProjectSummary {
                id: "foreign".to_string(),
                name: Some("Foreign observation project".to_string()),
                path: "/tmp/mcp-observation-foreign".to_string(),
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
                updated_at: 2,
                shell_profile: None,
            },
        ],
    )
    .await;
    runner_registry
}

fn spawn_stateless_observation_agent_executor(
    registry: Arc<crate::runner_http::RunnerRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Some(request) = registry
                .poll(crate::runner_protocol::RunnerPollRequest {
                    client_id: "mcp-observation-agent".to_string(),
                    runner_instance_id: "inst-mcp-observation".to_string(),
                })
                .await
                .unwrap()
            {
                let (exit_code, stderr) = if request.kind == "file_read" {
                    (1, "No such file or directory")
                } else {
                    (-1, "unexpected observation fixture agent request")
                };
                registry
                    .complete(crate::runner_protocol::RunnerResultRequest {
                        client_id: "mcp-observation-agent".to_string(),
                        runner_instance_id: "inst-mcp-observation".to_string(),
                        request_id: request.request_id,
                        exit_code: Some(exit_code),
                        stdout: Some(String::new()),
                        stderr: Some(stderr.to_string()),
                        duration_ms: Some(1),
                        error: None,
                    })
                    .await
                    .unwrap();
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        }
    })
}

fn stateless_tool_output(body: &Value) -> &Value {
    &body["result"]["structuredContent"]["output"]
}

fn full_trace_dir_with_payload(
    root: &std::path::Path,
    phase: &str,
    expected: &Value,
) -> std::path::PathBuf {
    for entry in std::fs::read_dir(root).unwrap().flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        let Ok(events) = std::fs::read_to_string(path.join("events.jsonl")) else {
            continue;
        };
        for event in events
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            if event["event"] != "tool_trace_payload_captured" || event["phase"] != phase {
                continue;
            }
            let Some(relative) = event["payload_path"].as_str() else {
                continue;
            };
            let Ok(compressed) = std::fs::read(path.join(relative)) else {
                continue;
            };
            let Ok(raw) = zstd::stream::decode_all(compressed.as_slice()) else {
                continue;
            };
            if serde_json::from_slice::<Value>(&raw).ok().as_ref() == Some(expected) {
                return path;
            }
        }
    }
    panic!("missing full-trace payload phase {phase} matching this request");
}

async fn start_stateless_observation_session(
    service: &Service,
    id: i64,
    project: &str,
    title: &str,
) -> String {
    let (status, body) = stateless_2026_tool_call(
        service,
        "secret",
        id,
        "start_session",
        json!({"project": project, "title": title}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], false, "{body}");
    stateless_tool_output(&body)["session_id"]
        .as_str()
        .expect("stateless project Workflow Session")
        .to_string()
}

#[test]
fn stateless_full_trace_preserves_raw_context_ack_and_records_clean_effective_arguments() {
    // Full tracing retains the request/response trees plus decoded trace payloads.
    // Keep this integration fixture off the default libtest stack for the same
    // reason as the larger stateless MCP continuity/observation fixtures below.
    std::thread::Builder::new()
        .name("mcp-stateless-full-trace".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build stateless full-trace test runtime")
                .block_on(
                    stateless_full_trace_preserves_raw_context_ack_and_records_clean_effective_arguments_body(),
                );
        })
        .expect("spawn stateless full-trace test thread")
        .join()
        .expect("stateless full-trace test thread panicked");
}

async fn stateless_full_trace_preserves_raw_context_ack_and_records_clean_effective_arguments_body()
{
    let trace_root = tempfile::tempdir().unwrap();
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
    env.set(
        "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
        trace_root.path().to_string_lossy().as_ref(),
    );
    env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");

    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime));
    let arguments = json!({"ack_session_context_revision": 42});
    let (status, body) = stateless_2026_tool_call(
        &service,
        "secret",
        41,
        "list_tools",
        arguments.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], false, "{body}");
    crate::tool_request_trace::flush_full_trace_writer();

    let trace_dir = full_trace_dir_with_payload(trace_root.path(), "raw_arguments", &arguments);
    let events = std::fs::read_to_string(trace_dir.join("events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();

    let read_phase = |phase: &str| {
        let relative = events
            .iter()
            .find(|event| {
                event["event"] == "tool_trace_payload_captured" && event["phase"] == phase
            })
            .and_then(|event| event["payload_path"].as_str())
            .unwrap_or_else(|| panic!("missing trace payload phase {phase}"));
        let compressed = std::fs::read(trace_dir.join(relative)).unwrap();
        let raw = zstd::stream::decode_all(compressed.as_slice()).unwrap();
        serde_json::from_slice::<Value>(&raw).unwrap()
    };

    let raw = read_phase("raw_arguments");
    assert_eq!(
        raw[crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD],
        42
    );
    let effective = read_phase("effective_arguments");
    assert!(effective
        .get(crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD)
        .is_none());
    assert_eq!(effective, json!({}));
    assert!(!effective.to_string().contains("__webcodex_"));
    let final_response = read_phase("final_response");
    assert_eq!(final_response["result"]["isError"], false);
}

#[test]
fn stateless_full_trace_correlates_only_hashed_openai_window() {
    std::thread::Builder::new()
        .name("mcp-stateless-window-trace".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build stateless window-trace test runtime")
                .block_on(stateless_full_trace_correlates_only_hashed_openai_window_body());
        })
        .expect("spawn stateless window-trace test thread")
        .join()
        .expect("stateless window-trace test thread panicked");
}

async fn stateless_full_trace_correlates_only_hashed_openai_window_body() {
    use std::collections::BTreeSet;

    let trace_root = tempfile::tempdir().unwrap();
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
    env.set(
        "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
        trace_root.path().to_string_lossy().as_ref(),
    );
    env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");

    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));
    let raw_window = "openai-window-trace-opaque-secret";

    for id in [51_i64, 52_i64] {
        let mut params = mcp_2026_params(json!({"name": "list_tools", "arguments": {}}));
        params["_meta"]["openai/session"] = json!(raw_window);
        let (status, body) = stateless_2026_jsonrpc(
            &service,
            "secret",
            Some(MCP_STATELESS_PROTOCOL_VERSION),
            Some("tools/call"),
            Some("list_tools"),
            None,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": params,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["result"]["isError"], false, "{body}");
    }

    // tools/list is liveness evidence for the same hashed Window, but it is not
    // meaningful coding work and cannot establish a Workflow Session relation.
    let mut list_params = mcp_2026_params(json!({}));
    list_params["_meta"]["openai/session"] = json!(raw_window);
    let (status, listed) = stateless_2026_jsonrpc(
        &service,
        "secret",
        Some(MCP_STATELESS_PROTOCOL_VERSION),
        Some("tools/list"),
        None,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 53,
            "method": "tools/list",
            "params": list_params,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed["result"]["tools"].is_array(), "{listed}");
    crate::tool_request_trace::flush_full_trace_writer();

    let mut trace_ids = BTreeSet::new();
    let mut window_keys = BTreeSet::new();
    let mut traced_requests = 0_usize;
    for entry in std::fs::read_dir(trace_root.path()).unwrap().flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let trace_dir = entry.path();
        let Ok(events_text) = std::fs::read_to_string(trace_dir.join("events.jsonl")) else {
            continue;
        };
        assert!(
            !events_text.contains(raw_window),
            "raw OpenAI window identity leaked into trace metadata"
        );
        let events = events_text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        if let Some(parsed) = events.iter().find(|event| {
            event["event"] == "mcp_tool_request_parsed" && event["tool_name"] == "list_tools"
        }) {
            traced_requests += 1;
            let trace_id = parsed["server_trace_id"]
                .as_str()
                .expect("parsed trace event server_trace_id");
            let window_key = parsed["client_window_key"]
                .as_str()
                .expect("parsed trace event hashed client window");
            assert_eq!(parsed["client_window_source"], "openai-session");
            assert_eq!(window_key.len(), 64);
            assert!(window_key.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_ne!(window_key, raw_window);
            trace_ids.insert(trace_id.to_string());
            window_keys.insert(window_key.to_string());
        }

        for event in &events {
            let Some(relative) = event.get("payload_path").and_then(Value::as_str) else {
                continue;
            };
            let compressed = std::fs::read(trace_dir.join(relative)).unwrap();
            let raw = zstd::stream::decode_all(compressed.as_slice()).unwrap();
            assert!(
                !String::from_utf8_lossy(&raw).contains(raw_window),
                "raw OpenAI window identity leaked into compressed trace payload"
            );
        }
    }

    assert_eq!(traced_requests, 2, "expected one parsed event per request");
    assert_eq!(
        trace_ids.len(),
        2,
        "requests must keep distinct trace identities"
    );
    assert_eq!(
        window_keys.len(),
        1,
        "the same ChatGPT window must produce one stable hashed correlation key"
    );

    let durable_rows = {
        let conn = db.conn_for_tests();
        let mut stmt = conn
            .prepare(
                "SELECT action_name, operation, client_window_key, client_window_source,
                        server_trace_id, summary_json, error_summary, warning_summary,
                        window_meaningful, recorder_gap_session_id
                 FROM action_events
                 WHERE client_window_key IS NOT NULL
                 ORDER BY window_started_at_ms, event_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    assert_eq!(durable_rows.len(), 3);
    let durable_debug = format!("{durable_rows:?}");
    assert!(
        !durable_debug.contains(raw_window),
        "raw OpenAI Window identity leaked into ActionAudit SQLite rows: {durable_debug}"
    );
    let durable_keys = durable_rows
        .iter()
        .map(|row| row.2.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(durable_keys.len(), 1);
    assert_eq!(durable_rows.last().unwrap().0, "toolsList");
    assert_eq!(
        durable_rows.last().unwrap().1.as_deref(),
        Some("mcp_tools_list")
    );
    assert_eq!(durable_rows.last().unwrap().8, 0);
    assert!(durable_rows.last().unwrap().9.is_none());
}

#[tokio::test]
async fn mcp_tools_call_writes_a_summary_action_audit_row() {
    // list_tools is a full-operator-only tool; select that surface so the
    // call dispatches and lands an action audit row.
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));
    let mut resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "list_tools", "arguments": {}}
        }))
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Value = resp.take_json().await.unwrap();
    let structured = &body["result"]["structuredContent"];
    assert_eq!(structured["success"], true);
    assert!(structured["error"].is_null());
    let expected_tool_result_bytes = serde_json::to_vec(structured).unwrap().len() as u64;

    // End the connection guard structurally inside the block: the awaited
    // requests below must not overlap it.
    let (
        endpoint,
        action,
        operation,
        status,
        summary,
        duration_ms,
        request_observed_at_ms,
        response_handed_at_ms,
        response_streaming,
        window_continuity_eligible,
    ) = {
        let conn = db.conn_for_tests();
        let (
            endpoint,
            action,
            operation,
            status,
            duration_ms,
            request_observed_at_ms,
            response_handed_at_ms,
            response_streaming,
            window_continuity_eligible,
        ): (
            String,
            String,
            String,
            String,
            i64,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT endpoint, action_name, operation, status, duration_ms,
                        request_observed_at_ms, response_handed_at_ms,
                        response_streaming, window_continuity_eligible
                 FROM action_events",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .unwrap();
        // Summary-level discipline: no tool output is persisted for MCP rows.
        let summary: String = conn
            .query_row("SELECT summary_json FROM action_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        (
            endpoint,
            action,
            operation,
            status,
            summary,
            duration_ms,
            request_observed_at_ms,
            response_handed_at_ms,
            response_streaming,
            window_continuity_eligible,
        )
    };
    assert_eq!(endpoint, "/mcp");
    assert_eq!(action, "toolsCall");
    assert_eq!(operation, "list_tools");
    assert_eq!(status, "success");
    let request_observed_at_ms = request_observed_at_ms.expect("canonical MCP request start");
    let response_handed_at_ms = response_handed_at_ms.expect("canonical MCP response handoff");
    assert!(response_handed_at_ms >= request_observed_at_ms);
    assert!(duration_ms >= 0);
    assert!(
        duration_ms <= response_handed_at_ms - request_observed_at_ms,
        "legacy ActionAudit duration ends at its pre-render audit boundary, not response handoff"
    );
    assert_eq!(response_streaming, Some(0));
    assert_eq!(
        window_continuity_eligible,
        Some(0),
        "observability/discovery tools must not become meaningful continuity endpoints"
    );
    let summary: Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(summary["transport"], "mcp");
    assert!(
        summary.get("output").is_none(),
        "summary must not embed tool output: {summary}"
    );
    let telemetry = &summary["model_ergonomics"];
    assert_eq!(telemetry["schema_version"], 5);
    assert_eq!(telemetry["tool_name"], "list_tools");
    assert_eq!(telemetry["tool_category"], "runtime");
    assert_eq!(telemetry["success"], true);
    assert_eq!(
        telemetry["serialized_result_bytes"].as_u64().unwrap(),
        expected_tool_result_bytes,
        "MCP telemetry must count the final structuredContent ToolResult, not JSON-RPC/content framing"
    );
    assert!(telemetry["recovery_kind"].is_null());

    // Non-tool methods stay out of the audit.
    let resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}))
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    // A no-id tools/call is a JSON-RPC notification. The core dispatcher
    // intentionally ignores every notification, so it must not create a
    // successful action row for work that never ran.
    let resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {"name": "list_tools", "arguments": {}}
        }))
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::ACCEPTED));

    let count: i64 = {
        let conn = db.conn_for_tests();
        conn.query_row("SELECT COUNT(*) FROM action_events", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(count, 1);
}

#[tokio::test]
async fn mcp_pre_result_invalid_arguments_still_records_generic_attempt() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));
    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 101,
            "method": "tools/call",
            "params": {"name": "read_files", "arguments": {}}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&response), StatusCode::BAD_REQUEST);
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["error"]["code"], -32602);

    let summary: String = db
        .conn_for_tests()
        .query_row(
            "SELECT summary_json FROM action_events WHERE operation = 'read_files'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let summary: Value = serde_json::from_str(&summary).unwrap();
    let telemetry = &summary["model_ergonomics"];
    assert_eq!(telemetry["tool_name"], "read_files");
    assert_eq!(telemetry["success"], false);
    assert_eq!(telemetry["error_kind"], "invalid_arguments");
    assert!(telemetry["serialized_result_bytes"].is_null());
}

#[tokio::test]
async fn mcp_pre_kernel_wrapper_validation_still_records_generic_attempt() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));
    let mut arguments = json!({});
    arguments.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD.to_string(),
        json!(42),
    );

    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "tools/call",
            "params": {"name": "list_tools", "arguments": arguments}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&response), StatusCode::BAD_REQUEST);
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["error"]["code"], -32602);

    let summary: String = db
        .conn_for_tests()
        .query_row(
            "SELECT summary_json FROM action_events WHERE operation = 'list_tools'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let summary: Value = serde_json::from_str(&summary).unwrap();
    let telemetry = &summary["model_ergonomics"];
    assert_eq!(telemetry["tool_name"], "list_tools");
    assert_eq!(telemetry["tool_category"], "runtime");
    assert_eq!(telemetry["success"], false);
    assert_eq!(telemetry["error_kind"], "invalid_arguments");
    assert!(telemetry["serialized_result_bytes"].is_null());
}

fn seed_action_audit_pat(db: &crate::Database, user: &crate::models::UserRecord) -> String {
    let plaintext = crate::auth::generate_api_token();
    let record = crate::models::ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        name: "action-audit-test".to_string(),
        key_prefix: crate::auth::token_prefix(&plaintext),
        created_at: chrono::Utc::now().timestamp(),
        last_used_at: None,
        revoked_at: None,
        scopes: "runtime:read project:read project:write job:run".to_string(),
        expires_at: None,
        kind: crate::models::TOKEN_KIND_USER.to_string(),
        allowed_client_id: None,
    };
    db.insert_api_key(&record, &crate::auth::hash_token(&plaintext))
        .unwrap();
    plaintext
}

#[tokio::test]
async fn mcp_pat_tools_call_persists_user_attribution() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let token = seed_action_audit_pat(&db, &user);
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));

    let resp = TestClient::post("http://localhost/mcp")
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "list_tools", "arguments": {}}
        }))
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let attrs: (Option<String>, Option<String>, Option<String>) = db
        .conn_for_tests()
        .query_row(
            "SELECT principal_kind, principal_user_id, oauth_client_id FROM action_events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(attrs.0.as_deref(), Some("user"));
    assert_eq!(attrs.1.as_deref(), Some(user.id.as_str()));
    assert_eq!(attrs.2, None);
}

#[tokio::test]
async fn mcp_oauth_tools_call_persists_user_and_client_attribution() {
    let config = test_config_oauth2(Some("secret"));
    let (_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client = seed_oauth_client(&db, &user);
    let token = seed_oauth_access_token(&db, &client, &user, "runtime:read");
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));

    let resp = TestClient::post("http://localhost/mcp")
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "list_tools", "arguments": {}}
        }))
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let attrs: (Option<String>, Option<String>, Option<String>) = db
        .conn_for_tests()
        .query_row(
            "SELECT principal_kind, principal_user_id, oauth_client_id FROM action_events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(attrs.0.as_deref(), Some("oauth2"));
    assert_eq!(attrs.1.as_deref(), Some(user.id.as_str()));
    assert_eq!(attrs.2.as_deref(), Some(client.client_id.as_str()));
}
#[tokio::test]
async fn http_mcp_initialize_success() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime));
    let mut resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let session_id = resp
        .headers
        .get(crate::client_window::MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .expect("initialize must mint an MCP session id");
    assert!(session_id.starts_with("wc_mcp_"));
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    assert_eq!(body["result"]["serverInfo"]["name"], "webcodex");
    assert_eq!(
        body["result"]["serverInfo"]["runtimeExposure"],
        crate::model_surface::MODEL_SURFACE_FULL_OPERATOR_RUNTIME
    );
    assert!(body["result"]["protocolVersion"].is_string());
    assert_eq!(
        body["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
}

#[tokio::test]
async fn http_mcp_accepts_chatgpt_2025_11_25_protocol_header() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime));
    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_CHATGPT_PROTOCOL_VERSION,
            true,
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 199,
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MCP_CHATGPT_PROTOCOL_VERSION
                }
            }
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&response), StatusCode::OK);
    let body: Value = response.take_json().await.unwrap();
    assert!(body["result"]["tools"].is_array());
    assert!(body["result"].get("resultType").is_none());
}

#[test]
fn http_mcp_2026_request_scoped_ack_redelivers_until_durable_resolution() {
    // This integration fixture intentionally keeps several large MCP response
    // trees alive across many await points. Run the test harness itself on an
    // explicit stack so CI/libtest thread-stack variance cannot abort the
    // process; production request handling is unchanged and each HTTP request
    // still executes through the normal Server runtime path.
    std::thread::Builder::new()
        .name("mcp-request-scoped-ack-test".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build ACK integration test runtime");
            runtime.block_on(
                http_mcp_2026_request_scoped_ack_redelivers_until_durable_resolution_body(),
            );
        })
        .expect("spawn ACK integration test thread")
        .join()
        .expect("ACK integration test thread panicked");
}

async fn http_mcp_2026_request_scoped_ack_redelivers_until_durable_resolution_body() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime.clone()));

    let (status, session_body) = stateless_2026_tool_call(
        &service,
        "secret",
        220,
        "start_session",
        json!({"title": "ACK dogfood"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session_body}");
    let session_id = stateless_tool_output(&session_body)["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, post_body) = stateless_2026_tool_call(
        &service,
        "secret",
        221,
        "post_session_message",
        json!({
            "session_id": session_id,
            "kind": "guidance",
            "priority": "high",
            "requires_ack": true,
            "message": "Keep the exact request-scoped ACK contract."
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{post_body}");
    let message_id = stateless_tool_output(&post_body)["message_id"]
        .as_str()
        .unwrap()
        .to_string();

    let first_args = with_mcp_recording_session(json!({}), &session_id);
    let (status, first_body) =
        stateless_2026_tool_call(&service, "secret", 222, "list_tools", first_args, None).await;
    assert_eq!(status, StatusCode::OK, "{first_body}");
    let first = stateless_tool_output(&first_body);
    assert_eq!(
        first["session_attention"]["messages"][0]["message_id"],
        message_id
    );
    assert_eq!(
        first["session_attention"]["messages"][0]["message"],
        "Keep the exact request-scoped ACK contract."
    );

    let mut ack_args = with_mcp_recording_session(json!({}), &session_id);
    ack_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD.to_string(),
        json!([message_id, message_id]),
    );
    let (status, ack_body) =
        stateless_2026_tool_call(&service, "secret", 223, "list_tools", ack_args, None).await;
    assert_eq!(status, StatusCode::OK, "{ack_body}");
    let acknowledged = stateless_tool_output(&ack_body);
    assert_eq!(
        acknowledged["session_attention"]["ack"]["accepted_count"],
        1
    );
    assert_eq!(acknowledged["session_attention"]["ack"]["ignored_count"], 0);
    assert!(acknowledged["session_attention"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(acknowledged.get("session_continuity").is_none());
    assert!(acknowledged.get("session_recovery").is_none());
    let retained = runtime
        .sessions
        .list_messages(
            &session_id,
            crate::tool_runtime::sessions::ListSessionMessagesFilter {
                message_id: Some(message_id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        retained[0].status,
        crate::tool_runtime::sessions::SessionMessageStatus::Open
    );
    assert!(retained[0].first_ack_observed_at.is_some());

    let forgotten_args = with_mcp_recording_session(json!({}), &session_id);
    let (status, forgotten_body) =
        stateless_2026_tool_call(&service, "secret", 224, "list_tools", forgotten_args, None).await;
    assert_eq!(status, StatusCode::OK, "{forgotten_body}");
    assert_eq!(
        stateless_tool_output(&forgotten_body)["session_attention"]["messages"][0]["message_id"],
        message_id
    );

    let (status, resolve_body) = stateless_2026_tool_call(
        &service,
        "secret",
        225,
        "resolve_session_message",
        json!({"session_id": session_id, "message_id": message_id, "resolution": "handled"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resolve_body}");
    assert_eq!(
        stateless_tool_output(&resolve_body)["message"]["status"],
        "resolved"
    );

    let after_resolve_args = with_mcp_recording_session(json!({}), &session_id);
    let (status, after_resolve_body) = stateless_2026_tool_call(
        &service,
        "secret",
        226,
        "list_tools",
        after_resolve_args,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{after_resolve_body}");
    assert!(stateless_tool_output(&after_resolve_body)
        .get("session_attention")
        .is_none());

    let (status, second_post_body) = stateless_2026_tool_call(
        &service,
        "secret",
        227,
        "post_session_message",
        json!({
            "session_id": session_id,
            "kind": "guidance",
            "priority": "high",
            "requires_ack": true,
            "message": "ACK this fresh guidance on the resolving request."
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second_post_body}");
    let second_message_id = stateless_tool_output(&second_post_body)["message_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut resolve_with_ack_args = json!({
        "session_id": session_id,
        "message_id": second_message_id,
        "resolution": "handled with same-request ACK"
    });
    resolve_with_ack_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD.to_string(),
        json!([second_message_id]),
    );
    // Collaboration session_id is business input, not an inner recorder. Carry
    // the same Session explicitly as wrapper recorder provenance so ACK is
    // observed before the concrete resolve mutation without conflating roles.
    let resolve_with_ack_args = with_mcp_recording_session(resolve_with_ack_args, &session_id);
    let (status, resolve_with_ack_body) = stateless_2026_tool_call(
        &service,
        "secret",
        228,
        "resolve_session_message",
        resolve_with_ack_args,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resolve_with_ack_body}");
    let resolved_with_ack = stateless_tool_output(&resolve_with_ack_body);
    assert_eq!(resolved_with_ack["message"]["status"], "resolved");
    assert_eq!(
        resolved_with_ack["session_attention"]["ack"]["accepted_count"], 1,
        "{resolve_with_ack_body}"
    );
    assert_eq!(
        resolved_with_ack["session_attention"]["ack"]["ignored_count"],
        0
    );
    assert!(resolved_with_ack["session_attention"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    let second_stored = runtime
        .sessions
        .list_messages(
            &session_id,
            crate::tool_runtime::sessions::ListSessionMessagesFilter {
                message_id: Some(second_message_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        second_stored[0].status,
        crate::tool_runtime::sessions::SessionMessageStatus::Resolved
    );
    assert!(second_stored[0].first_ack_observed_at.is_some());

    let (status, third_post_body) = stateless_2026_tool_call(
        &service,
        "secret",
        229,
        "post_session_message",
        json!({
            "session_id": session_id,
            "kind": "guidance",
            "priority": "high",
            "requires_ack": true,
            "message": "Resolve this without a dedicated resolve tool call."
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{third_post_body}");
    let third_message_id = stateless_tool_output(&third_post_body)["message_id"]
        .as_str()
        .unwrap()
        .to_string();
    let resolution_text = "handled through ordinary list_tools wrapper metadata";

    let missing_ack_args = with_mcp_recording_session(
        json!({
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: {
                "message_id": third_message_id,
                "resolution": resolution_text
            }
        }),
        &session_id,
    );
    let (status, missing_ack_body) = stateless_2026_tool_call(
        &service,
        "secret",
        230,
        "list_tools",
        missing_ack_args,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{missing_ack_body}");
    assert_eq!(
        stateless_tool_output(&missing_ack_body)["error_kind"],
        "invalid_session_message"
    );
    let still_open = runtime
        .sessions
        .list_messages(
            &session_id,
            crate::tool_runtime::sessions::ListSessionMessagesFilter {
                message_id: Some(third_message_id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        still_open[0].status,
        crate::tool_runtime::sessions::SessionMessageStatus::Open
    );

    let mut piggyback_args = with_mcp_recording_session(
        json!({
            crate::tool_runtime::sessions::TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: {
                "message_id": third_message_id,
                "resolution": resolution_text
            }
        }),
        &session_id,
    );
    piggyback_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD.to_string(),
        json!([third_message_id]),
    );
    let (status, piggyback_body) = stateless_2026_tool_call(
        &service,
        "secret",
        231,
        "list_tools",
        piggyback_args.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{piggyback_body}");
    assert_eq!(piggyback_body["result"]["isError"], false);
    let piggyback_output = stateless_tool_output(&piggyback_body);
    assert_eq!(
        piggyback_output["session_attention"]["ack"]["accepted_count"],
        1
    );
    assert!(piggyback_output["session_attention"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    let piggyback_stored = runtime
        .sessions
        .list_messages(
            &session_id,
            crate::tool_runtime::sessions::ListSessionMessagesFilter {
                message_id: Some(third_message_id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        piggyback_stored[0].status,
        crate::tool_runtime::sessions::SessionMessageStatus::Resolved
    );
    assert_eq!(
        piggyback_stored[0].resolution.as_deref(),
        Some(resolution_text)
    );

    let (status, replay_body) =
        stateless_2026_tool_call(&service, "secret", 232, "list_tools", piggyback_args, None).await;
    assert_eq!(status, StatusCode::OK, "{replay_body}");
    assert_eq!(replay_body["result"]["isError"], false);

    let audit = serde_json::to_string(
        &runtime
            .sessions
            .summary(&session_id, Some(100))
            .unwrap()
            .events,
    )
    .unwrap();
    assert!(!audit.contains("ack_session_message_ids"));
    assert!(!audit.contains("__webcodex_stateless_ack_session_message_ids"));
    assert!(!audit.contains("__webcodex_stateless_session_message_resolution"));
    assert!(!audit.contains("handled through ordinary list_tools wrapper metadata"));
}

#[tokio::test]
async fn http_mcp_2026_context_request_projects_post_tool_materials_nonfatally() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime));
    let arguments = json!({
        crate::tool_runtime::context_projection::TOOL_CALL_CONTEXT_REQUEST_FIELD: [
            "webcodex.workflow",
            "future.material",
            "webcodex.workflow",
            "project.instructions"
        ]
    });
    let (status, body) =
        stateless_2026_tool_call(&service, "secret", 226, "list_tools", arguments, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], false);
    let output = stateless_tool_output(&body);
    assert_eq!(output["context_projection"]["timing"], "post_tool");
    assert_eq!(
        output["context_projection"]["applies_to_current_effect"],
        false
    );
    let materials = output["context_projection"]["materials"]
        .as_array()
        .unwrap();
    assert_eq!(materials.len(), 3);
    assert_eq!(materials[0]["key"], "webcodex.workflow");
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
}

#[test]
fn http_mcp_2026_session_context_revision_recovers_missing_stale_and_invalid_ack() {
    // Like the neighboring request-scoped ACK and observation fixtures, this
    // end-to-end continuity test keeps several large MCP response trees alive
    // across awaits. The default libtest stack can overflow only in the full
    // workspace suite, so isolate the fixture on the same bounded larger stack
    // without changing production request handling or the release gate.
    std::thread::Builder::new()
        .name("mcp-session-context-continuity".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build session context continuity test runtime")
                .block_on(
                    http_mcp_2026_session_context_revision_recovers_missing_stale_and_invalid_ack_body(),
                );
        })
        .expect("spawn session context continuity test thread")
        .join()
        .expect("session context continuity test thread panicked");
}

async fn http_mcp_2026_session_context_revision_recovers_missing_stale_and_invalid_ack_body() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime.clone()));

    let (status, session_body) = stateless_2026_tool_call(
        &service,
        "secret",
        227,
        "start_session",
        json!({"title": "context continuity dogfood"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session_body}");
    let session_id = stateless_tool_output(&session_body)["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut cached_read_args = with_mcp_recording_session(json!({}), &session_id);
    cached_read_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD.to_string(),
        json!(999),
    );
    let (status, cached_read_body) = stateless_2026_tool_call(
        &service,
        "secret",
        228,
        "list_tools",
        cached_read_args,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cached_read_body}");
    let cached_read = stateless_tool_output(&cached_read_body);
    for field in [
        "session_context_revision",
        "session_continuity",
        "session_recovery",
    ] {
        assert!(cached_read.get(field).is_none(), "{field}");
    }
    assert_eq!(runtime.sessions.context_revision(&session_id), Some(0));

    let mut exact_args =
        with_mcp_recording_session(json!({"title": "context checkpoint exact"}), &session_id);
    exact_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD.to_string(),
        json!(0),
    );
    let (status, exact_body) =
        stateless_2026_tool_call(&service, "secret", 229, "start_session", exact_args, None).await;
    assert_eq!(status, StatusCode::OK, "{exact_body}");
    let exact = stateless_tool_output(&exact_body);
    assert_eq!(exact["session_context_revision"], 1);
    assert!(exact.get("session_continuity").is_none());
    assert!(exact.get("session_recovery").is_none());
    assert_eq!(runtime.sessions.context_revision(&session_id), Some(1));

    let mut second_cached_read_args = with_mcp_recording_session(json!({}), &session_id);
    second_cached_read_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD.to_string(),
        json!(1),
    );
    let (status, second_cached_read_body) = stateless_2026_tool_call(
        &service,
        "secret",
        230,
        "list_tools",
        second_cached_read_args,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second_cached_read_body}");
    let second_cached_read = stateless_tool_output(&second_cached_read_body);
    assert!(second_cached_read.get("session_context_revision").is_none());
    assert!(second_cached_read.get("session_continuity").is_none());
    assert!(second_cached_read.get("session_recovery").is_none());
    assert_eq!(runtime.sessions.context_revision(&session_id), Some(1));

    let mut stale_args =
        with_mcp_recording_session(json!({"title": "context checkpoint stale"}), &session_id);
    stale_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD.to_string(),
        json!(0),
    );
    let (status, stale_body) =
        stateless_2026_tool_call(&service, "secret", 231, "start_session", stale_args, None).await;
    assert_eq!(status, StatusCode::OK, "{stale_body}");
    let stale = stateless_tool_output(&stale_body);
    assert_eq!(stale["session_context_revision"], 2);
    assert_eq!(stale["session_continuity"]["status"], "behind");
    assert_eq!(stale["session_continuity"]["ack_revision"], 0);
    assert_eq!(stale["session_continuity"]["pre_call_revision"], 1);
    assert_eq!(
        stale["session_recovery"]["model_facing_events"][0]["context_revision"],
        1
    );
    assert_eq!(runtime.sessions.context_revision(&session_id), Some(2));

    let mut future_args =
        with_mcp_recording_session(json!({"title": "context checkpoint future"}), &session_id);
    future_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD.to_string(),
        json!(999),
    );
    let (status, future_body) =
        stateless_2026_tool_call(&service, "secret", 232, "start_session", future_args, None).await;
    assert_eq!(status, StatusCode::OK, "{future_body}");
    let future = stateless_tool_output(&future_body);
    assert!(future.get("session_context_revision").is_none());
    assert_eq!(future["session_continuity"]["status"], "invalid");
    assert_eq!(future["session_continuity"]["recovery_required"], true);
    assert!(future.get("session_recovery").is_none());

    let missing_args =
        with_mcp_recording_session(json!({"title": "context checkpoint missing"}), &session_id);
    let (status, missing_body) =
        stateless_2026_tool_call(&service, "secret", 233, "start_session", missing_args, None)
            .await;
    assert_eq!(status, StatusCode::OK, "{missing_body}");
    let missing = stateless_tool_output(&missing_body);
    assert!(missing.get("session_context_revision").is_none());
    assert_eq!(missing["session_continuity"]["status"], "unacknowledged");
    assert!(missing.get("session_recovery").is_none());
    assert_eq!(
        missing["session_continuity"]["recovery_tool"],
        "session_handoff_summary"
    );
    assert_eq!(runtime.sessions.context_revision(&session_id), Some(4));

    // Explicit recovery returns one current-state handoff and a safe baseline.
    let (status, recovered_body) = stateless_2026_tool_call(
        &service,
        "secret",
        2331,
        "session_handoff_summary",
        json!({"session_id": session_id}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{recovered_body}");
    let recovered = stateless_tool_output(&recovered_body);
    assert_eq!(recovered["session_context_revision"], 4);
    assert_eq!(recovered["session_continuity"]["status"], "recovered");
    assert!(recovered["validation"].is_object());
    assert!(recovered["jobs"].is_object());
    assert!(recovered.get("session_recovery").is_none());
    assert_eq!(runtime.sessions.context_revision(&session_id), Some(4));

    for partial in [
        json!({"limit": 1}),
        json!({"summary_only": true}),
        json!({"include_validation": false}),
        json!({"include_workspace": false}),
        json!({"include_checkpoints": false}),
    ] {
        let mut args = partial;
        args["session_id"] = json!(session_id);
        let (status, body) = stateless_2026_tool_call(
            &service,
            "secret",
            2332,
            "session_handoff_summary",
            args,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let output = stateless_tool_output(&body);
        assert!(output.get("session_context_revision").is_none());
        assert_eq!(output["session_continuity"]["recovery_required"], true);
        assert!(output.get("session_recovery").is_none());
    }

    // Reading C with explicit recorder W cannot establish W's baseline.
    let other = runtime.sessions.start_session(None, None);
    let (status, body) = stateless_2026_tool_call(
        &service,
        "secret",
        2333,
        "session_handoff_summary",
        with_mcp_recording_session(json!({"session_id": session_id}), &other.session_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let output = stateless_tool_output(&body);
    assert!(output.get("session_context_revision").is_none());
    assert_eq!(
        output["session_continuity"]["recovery_session_id"],
        other.session_id
    );

    let mut after_missing_args = with_mcp_recording_session(
        json!({"title": "context checkpoint after missing"}),
        &session_id,
    );
    after_missing_args.as_object_mut().unwrap().insert(
        crate::tool_runtime::sessions::TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD.to_string(),
        json!(4),
    );
    let (status, after_missing_body) = stateless_2026_tool_call(
        &service,
        "secret",
        234,
        "start_session",
        after_missing_args,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{after_missing_body}");
    let after_missing = stateless_tool_output(&after_missing_body);
    assert_eq!(after_missing["session_context_revision"], 5);
    assert!(after_missing.get("session_continuity").is_none());
    assert!(after_missing.get("session_recovery").is_none());

    let mut malformed_args =
        with_mcp_recording_session(json!({"title": "malformed ACK effect"}), &session_id);
    malformed_args["ack_session_context_revision"] = json!({"invalid": true});
    let (status, malformed_body) = stateless_2026_tool_call(
        &service,
        "secret",
        2341,
        "start_session",
        malformed_args,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{malformed_body}");
    let malformed = stateless_tool_output(&malformed_body);
    assert!(
        malformed["session_id"].is_string(),
        "business Session creation must execute"
    );
    assert_eq!(malformed["session_continuity"]["status"], "invalid");
    assert!(malformed.get("session_context_revision").is_none());
    assert!(malformed.get("session_recovery").is_none());
    assert_eq!(runtime.sessions.context_revision(&session_id), Some(6));

    let audit = serde_json::to_string(
        &runtime
            .sessions
            .summary(&session_id, Some(100))
            .unwrap()
            .events,
    )
    .unwrap();
    assert!(!audit.contains("ack_session_context_revision"));
    assert!(!audit.contains("__webcodex_stateless_ack_session_context_revision"));
}

#[tokio::test]
async fn http_mcp_2026_collaboration_completion_preserves_explicit_recorder_provenance() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime.clone()));

    let (status, coordinator_body) = stateless_2026_tool_call(
        &service,
        "secret",
        230,
        "start_session",
        json!({"title": "Coordinator C"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{coordinator_body}");
    let coordinator_id = coordinator_body["result"]["structuredContent"]["output"]["session_id"]
        .as_str()
        .expect("coordinator Workflow Session")
        .to_string();

    let (status, worker_body) = stateless_2026_tool_call(
        &service,
        "secret",
        231,
        "start_session",
        json!({"title": "Worker W"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{worker_body}");
    let worker_id = worker_body["result"]["structuredContent"]["output"]["session_id"]
        .as_str()
        .expect("worker Workflow Session")
        .to_string();

    let (status, replay_worker_body) = stateless_2026_tool_call(
        &service,
        "secret",
        232,
        "start_session",
        json!({"title": "Replay worker X"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay_worker_body}");
    let replay_worker_id = replay_worker_body["result"]["structuredContent"]["output"]
        ["session_id"]
        .as_str()
        .expect("replay worker Workflow Session")
        .to_string();

    let (status, posted_body) = stateless_2026_tool_call(
        &service,
        "secret",
        233,
        "post_session_message",
        json!({
            "session_id": coordinator_id,
            "kind": "todo",
            "message": "Review the stateless collaboration provenance path.",
            "priority": "high"
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted_body}");
    let todo_id = posted_body["result"]["structuredContent"]["output"]["message_id"]
        .as_str()
        .expect("todo message id")
        .to_string();
    let assignment_fence = runtime
        .sessions
        .get_assignment(&coordinator_id, &todo_id)
        .unwrap()
        .assignment_fence;

    let completion_arguments = with_mcp_recording_session(
        json!({
            "session_id": coordinator_id,
            "message_id": todo_id,
            "answer": "Reviewed and completed under the explicit worker recorder.",
            "completion_key": "stateless-recorder-v1",
            "expected_assignment_fence": assignment_fence.clone(),
            "author_session_id": "wc_sess_forged_should_not_win"
        }),
        &worker_id,
    );
    let (status, completed_body) = stateless_2026_tool_call(
        &service,
        "secret",
        234,
        "complete_session_message",
        completion_arguments,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed_body}");
    assert_eq!(completed_body["result"]["isError"], false);
    let completed = &completed_body["result"]["structuredContent"]["output"];
    assert_eq!(completed["answer"]["author_session_id"], worker_id);
    assert_eq!(completed["answer"]["reply_to"], todo_id);
    assert_eq!(completed["todo"]["status"], "resolved");
    let answer_message_id = completed["answer_message_id"].clone();

    let worker_summary = runtime.sessions.summary(&worker_id, Some(100)).unwrap();
    assert!(worker_summary.events.iter().any(|event| {
        event.kind == "tool_call_finished" && event.tool_name == "complete_session_message"
    }));
    let worker_audit = serde_json::to_string(&worker_summary.events).unwrap();
    for private in [
        "recording_session_id",
        "Reviewed and completed under the explicit worker recorder.",
        "stateless-recorder-v1",
        "wc_sess_forged_should_not_win",
        assignment_fence.as_str(),
    ] {
        assert!(
            !worker_audit.contains(private),
            "MCP wrapper/private completion data leaked into Session audit: {private}"
        );
    }
    let coordinator_summary = runtime
        .sessions
        .summary(&coordinator_id, Some(100))
        .unwrap();
    assert!(!coordinator_summary.events.iter().any(|event| {
        event.kind == "tool_call_finished" && event.tool_name == "complete_session_message"
    }));

    let conflicting_recorder_arguments = with_mcp_recording_session(
        json!({
            "session_id": coordinator_id,
            "message_id": todo_id,
            "answer": "Reviewed and completed under the explicit worker recorder.",
            "completion_key": "stateless-recorder-v1",
            "expected_assignment_fence": assignment_fence.clone()
        }),
        &replay_worker_id,
    );
    let (status, conflicting_recorder_body) = stateless_2026_tool_call(
        &service,
        "secret",
        235,
        "complete_session_message",
        conflicting_recorder_arguments,
        Some("legacy-session-must-not-affect-2026"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{conflicting_recorder_body}");
    assert_eq!(conflicting_recorder_body["result"]["isError"], true);
    assert_eq!(
        conflicting_recorder_body["result"]["structuredContent"]["output"]["error_kind"],
        "idempotency_conflict"
    );

    let replay_arguments = with_mcp_recording_session(
        json!({
            "session_id": coordinator_id,
            "message_id": todo_id,
            "answer": "Reviewed and completed under the explicit worker recorder.",
            "completion_key": "stateless-recorder-v1",
            "expected_assignment_fence": assignment_fence.clone()
        }),
        &worker_id,
    );
    let (status, replay_body) = stateless_2026_tool_call(
        &service,
        "secret",
        240,
        "complete_session_message",
        replay_arguments,
        Some("legacy-session-must-not-affect-2026"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay_body}");
    let replayed = &replay_body["result"]["structuredContent"]["output"];
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["answer_message_id"], answer_message_id);
    assert_eq!(replayed["answer"]["author_session_id"], worker_id);

    let (status, missing_todo_body) = stateless_2026_tool_call(
        &service,
        "secret",
        236,
        "post_session_message",
        json!({
            "session_id": coordinator_id,
            "kind": "todo",
            "message": "Exercise missing recorder compatibility."
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{missing_todo_body}");
    let missing_todo_id = missing_todo_body["result"]["structuredContent"]["output"]["message_id"]
        .as_str()
        .unwrap()
        .to_string();
    let missing_assignment_fence = runtime
        .sessions
        .get_assignment(&coordinator_id, &missing_todo_id)
        .unwrap()
        .assignment_fence;
    let (status, missing_body) = stateless_2026_tool_call(
        &service,
        "secret",
        237,
        "complete_session_message",
        json!({
            "session_id": coordinator_id,
            "message_id": missing_todo_id,
            "answer": "Compatibility completion without recorder.",
            "completion_key": "stateless-no-recorder-v1",
            "expected_assignment_fence": missing_assignment_fence
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{missing_body}");
    assert_eq!(
        missing_body["result"]["structuredContent"]["output"]["answer"]["author_session_id"],
        Value::Null
    );

    let (status, unknown_todo_body) = stateless_2026_tool_call(
        &service,
        "secret",
        238,
        "post_session_message",
        json!({
            "session_id": coordinator_id,
            "kind": "todo",
            "message": "An unknown recorder must fail before completion mutation."
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unknown_todo_body}");
    let unknown_todo_id = unknown_todo_body["result"]["structuredContent"]["output"]["message_id"]
        .as_str()
        .unwrap()
        .to_string();
    let unknown_assignment_fence = runtime
        .sessions
        .get_assignment(&coordinator_id, &unknown_todo_id)
        .unwrap()
        .assignment_fence;
    let before_unknown = runtime
        .sessions
        .summary(&coordinator_id, Some(100))
        .unwrap();
    let unknown_arguments = with_mcp_recording_session(
        json!({
            "session_id": coordinator_id,
            "message_id": unknown_todo_id,
            "answer": "must not be written",
            "completion_key": "unknown-recorder-must-fail",
            "expected_assignment_fence": unknown_assignment_fence
        }),
        "wc_sess_missing_recorder",
    );
    let (status, unknown_body) = stateless_2026_tool_call(
        &service,
        "secret",
        239,
        "complete_session_message",
        unknown_arguments,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unknown_body}");
    assert_eq!(unknown_body["result"]["isError"], true);
    assert_eq!(
        unknown_body["result"]["structuredContent"]["output"]["error_kind"],
        "unknown_session_id"
    );
    let after_unknown = runtime
        .sessions
        .summary(&coordinator_id, Some(100))
        .unwrap();
    assert_eq!(after_unknown.messages.total, before_unknown.messages.total);
    assert!(runtime
        .sessions
        .summary("wc_sess_missing_recorder", None)
        .is_none());
}

#[test]
fn http_mcp_2026_observe_session_messages_preserves_stateless_delta_contract() {
    // This end-to-end fixture intentionally keeps many independent HTTP response
    // values alive across awaits. On the default ~2 MiB libtest thread stack it
    // can sit close enough to the limit to overflow only under the full workspace
    // suite. Give this one large integration fixture an explicit bounded stack
    // rather than weakening or serializing the workspace release gate.
    std::thread::Builder::new()
        .name("mcp-observe-session-messages".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build stateless observation test runtime")
                .block_on(
                    http_mcp_2026_observe_session_messages_preserves_stateless_delta_contract_body(
                    ),
                );
        })
        .expect("spawn stateless observation test thread")
        .join()
        .expect("stateless observation test thread panicked");
}

async fn http_mcp_2026_observe_session_messages_preserves_stateless_delta_contract_body() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let ledger_dir = tempfile::tempdir().unwrap();
    let ledger = ledger_dir.path().join("sessions.json");
    let runner_registry = stateless_observation_runner_registry().await;
    let agent_executor = spawn_stateless_observation_agent_executor(runner_registry.clone());
    let shared_project =
        crate::tool_runtime::runner_project_runtime_id("mcp-observation-agent", "shared");
    let foreign_project =
        crate::tool_runtime::runner_project_runtime_id("mcp-observation-agent", "foreign");
    let runtime = Arc::new(
        ToolRuntime::new_for_tests_with_runner_registry(runner_registry.clone())
            .with_session_ledger(&ledger)
            .with_model_surface(ModelSurface::FullOperatorRuntime),
    );
    let service = Service::new(build_test_router(
        config.clone(),
        db.clone(),
        runtime.clone(),
    ));

    // Create every project-scoped participant through the real stateless MCP
    // start_session path so target and recorder authorization exercise both
    // project resolution and the immutable caller authority-group fingerprint.
    let coordinator_id = start_stateless_observation_session(
        &service,
        240,
        &shared_project,
        "Observation coordinator C",
    )
    .await;
    let worker_id =
        start_stateless_observation_session(&service, 241, &shared_project, "Observation worker W")
            .await;
    let second_coordinator_id = start_stateless_observation_session(
        &service,
        242,
        &shared_project,
        "Observation coordinator C2",
    )
    .await;
    let foreign_worker_id = start_stateless_observation_session(
        &service,
        243,
        &foreign_project,
        "Foreign observation worker W2",
    )
    .await;

    let pre_baseline_body_marker = "pre-baseline-history-must-not-replay";
    let (status, pre_baseline_body) = stateless_2026_tool_call(
        &service,
        "secret",
        244,
        "post_session_message",
        with_mcp_recording_session(
            json!({
                "session_id": coordinator_id,
                "kind": "note",
                "message": pre_baseline_body_marker
            }),
            &worker_id,
        ),
        Some("legacy-pre-baseline"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pre_baseline_body}");
    assert_eq!(pre_baseline_body["result"]["isError"], false);

    let (status, baseline_body) = stateless_2026_tool_call(
        &service,
        "secret",
        245,
        "observe_session_messages",
        with_mcp_recording_session(json!({"session_id": coordinator_id}), &worker_id),
        Some("legacy-baseline-a"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{baseline_body}");
    assert_eq!(baseline_body["result"]["isError"], false);
    let baseline = stateless_tool_output(&baseline_body);
    assert_eq!(baseline["success"], true);
    assert!(baseline["messages"].as_array().unwrap().is_empty());
    assert_eq!(baseline["changed"], false);
    assert_eq!(baseline["history_lost"], false);
    assert_eq!(baseline["has_more"], false);
    assert_eq!(baseline["wait_outcome"], "immediate");
    let token0 = baseline["observation_token"]
        .as_str()
        .expect("baseline observation token")
        .to_string();
    assert!(token0.starts_with("wsm1_"));
    assert!(token0.len() <= 192);

    let worker_summary = runtime.sessions.summary(&worker_id, Some(100)).unwrap();
    assert!(worker_summary.events.iter().any(|event| {
        event.kind == "tool_call_finished" && event.tool_name == "observe_session_messages"
    }));
    let coordinator_summary = runtime
        .sessions
        .summary(&coordinator_id, Some(100))
        .unwrap();
    assert!(!coordinator_summary.events.iter().any(|event| {
        event.kind == "tool_call_finished" && event.tool_name == "observe_session_messages"
    }));

    let delta_body_marker = "stateless-cross-request-delta-body";
    let (status, posted_body) = stateless_2026_tool_call(
        &service,
        "secret",
        246,
        "post_session_message",
        with_mcp_recording_session(
            json!({
                "session_id": coordinator_id,
                "kind": "progress",
                "message": delta_body_marker
            }),
            &worker_id,
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted_body}");
    assert_eq!(posted_body["result"]["isError"], false);
    let delta_message_id = stateless_tool_output(&posted_body)["message_id"]
        .as_str()
        .expect("delta message id")
        .to_string();

    let (status, delta_body) = stateless_2026_tool_call(
        &service,
        "secret",
        247,
        "observe_session_messages",
        with_mcp_recording_session(
            json!({
                "session_id": coordinator_id,
                "after_observation_token": token0
            }),
            &worker_id,
        ),
        Some("different-legacy-delta-b"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{delta_body}");
    assert_eq!(delta_body["result"]["isError"], false);
    let delta = stateless_tool_output(&delta_body);
    assert_eq!(delta["changed"], true);
    assert_eq!(delta["history_lost"], false);
    assert_eq!(delta["has_more"], false);
    let delta_messages = delta["messages"].as_array().unwrap();
    assert_eq!(delta_messages.len(), 1);
    assert_eq!(delta_messages[0]["message_id"], delta_message_id);
    assert_eq!(delta_messages[0]["message"], delta_body_marker);
    assert!(delta_messages
        .iter()
        .all(|message| message["message"] != pre_baseline_body_marker));
    let token1 = delta["observation_token"]
        .as_str()
        .expect("advanced observation token")
        .to_string();
    assert_ne!(token1, token0);

    let (status, wrong_target_body) = stateless_2026_tool_call(
        &service,
        "secret",
        248,
        "observe_session_messages",
        with_mcp_recording_session(
            json!({
                "session_id": second_coordinator_id,
                "after_observation_token": token1
            }),
            &worker_id,
        ),
        Some("legacy-wrong-target"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{wrong_target_body}");
    assert_eq!(wrong_target_body["result"]["isError"], true);
    assert_eq!(
        stateless_tool_output(&wrong_target_body)["error_kind"],
        "invalid_session_message_observation_token"
    );
    assert!(stateless_tool_output(&wrong_target_body)
        .get("observation_token")
        .is_none());
    let wrong_target_serialized = serde_json::to_string(&wrong_target_body).unwrap();
    assert!(!wrong_target_serialized.contains(delta_body_marker));

    let target_before_foreign = runtime
        .sessions
        .summary(&coordinator_id, Some(100))
        .unwrap();
    let (status, foreign_recorder_body) = stateless_2026_tool_call(
        &service,
        "secret",
        249,
        "observe_session_messages",
        with_mcp_recording_session(json!({"session_id": coordinator_id}), &foreign_worker_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{foreign_recorder_body}");
    assert_eq!(foreign_recorder_body["result"]["isError"], true);
    assert_eq!(
        stateless_tool_output(&foreign_recorder_body)["error_kind"],
        "session_project_mismatch"
    );
    assert!(stateless_tool_output(&foreign_recorder_body)
        .get("observation_token")
        .is_none());
    let foreign_serialized = serde_json::to_string(&foreign_recorder_body).unwrap();
    assert!(!foreign_serialized.contains(delta_body_marker));
    assert!(!foreign_serialized.contains(&token1));
    let target_after_foreign = runtime
        .sessions
        .summary(&coordinator_id, Some(100))
        .unwrap();
    assert_eq!(
        target_after_foreign.messages.total,
        target_before_foreign.messages.total
    );

    let wait_body_marker = "stateless-bounded-wait-delta-body";
    let wait_arguments = with_mcp_recording_session(
        json!({
            "session_id": coordinator_id,
            "after_observation_token": token1,
            "wait_secs": 2
        }),
        &worker_id,
    );
    let post_during_wait_arguments = with_mcp_recording_session(
        json!({
            "session_id": coordinator_id,
            "kind": "progress",
            "message": wait_body_marker
        }),
        &worker_id,
    );
    let wait_request = stateless_2026_tool_call(
        &service,
        "secret",
        250,
        "observe_session_messages",
        wait_arguments,
        Some("legacy-wait-request"),
    );
    let post_during_wait = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        stateless_2026_tool_call(
            &service,
            "secret",
            251,
            "post_session_message",
            post_during_wait_arguments,
            Some("different-legacy-post-during-wait"),
        )
        .await
    };
    let ((wait_status, wait_body), (post_status, post_during_wait_body)) =
        tokio::join!(wait_request, post_during_wait);
    assert_eq!(post_status, StatusCode::OK, "{post_during_wait_body}");
    let wait_message_id = stateless_tool_output(&post_during_wait_body)["message_id"]
        .as_str()
        .expect("wait delta message id")
        .to_string();
    assert_eq!(wait_status, StatusCode::OK, "{wait_body}");
    assert_eq!(wait_body["result"]["isError"], false);
    let waited = stateless_tool_output(&wait_body);
    assert_eq!(waited["changed"], true);
    assert_eq!(waited["wait_outcome"], "updated");
    assert_eq!(waited["history_lost"], false);
    let waited_messages = waited["messages"].as_array().unwrap();
    assert_eq!(waited_messages.len(), 1);
    assert_eq!(waited_messages[0]["message_id"], wait_message_id);
    assert_eq!(waited_messages[0]["message"], wait_body_marker);
    let token2 = waited["observation_token"]
        .as_str()
        .expect("wait observation token")
        .to_string();
    assert_ne!(token2, token1);

    let worker_summary = runtime.sessions.summary(&worker_id, Some(100)).unwrap();
    let observe_events = worker_summary
        .events
        .iter()
        .filter(|event| event.tool_name == "observe_session_messages")
        .collect::<Vec<_>>();
    assert!(observe_events
        .iter()
        .any(|event| event.kind == "tool_call_finished"));
    let session_audit = serde_json::to_string(&observe_events).unwrap();
    for private in [
        token0.as_str(),
        token1.as_str(),
        token2.as_str(),
        pre_baseline_body_marker,
        delta_body_marker,
        wait_body_marker,
        "recording_session_id",
    ] {
        assert!(
            !session_audit.contains(private),
            "stateless observation Session audit leaked private data: {private}"
        );
    }
    let action_audit = {
        let conn = db.conn_for_tests();
        let mut statement = conn
            .prepare("SELECT summary_json FROM action_events WHERE operation = 'observe_session_messages'")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(!action_audit.is_empty());
    for private in [
        token0.as_str(),
        token1.as_str(),
        token2.as_str(),
        pre_baseline_body_marker,
        delta_body_marker,
        wait_body_marker,
        "recording_session_id",
    ] {
        assert!(
            !action_audit.contains(private),
            "stateless observation action audit leaked private data: {private}"
        );
    }

    drop(service);
    drop(runtime);
    let restored_runtime = Arc::new(
        ToolRuntime::new_for_tests_with_runner_registry(runner_registry)
            .with_session_ledger(&ledger)
            .with_model_surface(ModelSurface::FullOperatorRuntime),
    );
    let restored_service = Service::new(build_test_router(config, db, restored_runtime.clone()));
    let (status, restored_unchanged_body) = stateless_2026_tool_call(
        &restored_service,
        "secret",
        252,
        "observe_session_messages",
        with_mcp_recording_session(
            json!({
                "session_id": coordinator_id,
                "after_observation_token": token2
            }),
            &worker_id,
        ),
        Some("legacy-after-restart"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{restored_unchanged_body}");
    assert_eq!(restored_unchanged_body["result"]["isError"], false);
    let restored_unchanged = stateless_tool_output(&restored_unchanged_body);
    assert_eq!(restored_unchanged["changed"], false);
    assert!(restored_unchanged["messages"]
        .as_array()
        .unwrap()
        .is_empty());

    let restart_body_marker = "stateless-post-restart-delta-body";
    let (status, post_restart_body) = stateless_2026_tool_call(
        &restored_service,
        "secret",
        253,
        "post_session_message",
        with_mcp_recording_session(
            json!({
                "session_id": coordinator_id,
                "kind": "progress",
                "message": restart_body_marker
            }),
            &worker_id,
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{post_restart_body}");
    let restart_message_id = stateless_tool_output(&post_restart_body)["message_id"]
        .as_str()
        .expect("restart delta message id")
        .to_string();
    let (status, restart_delta_body) = stateless_2026_tool_call(
        &restored_service,
        "secret",
        254,
        "observe_session_messages",
        with_mcp_recording_session(
            json!({
                "session_id": coordinator_id,
                "after_observation_token": token2
            }),
            &worker_id,
        ),
        Some("different-legacy-restart-delta"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{restart_delta_body}");
    assert_eq!(restart_delta_body["result"]["isError"], false);
    let restart_delta = stateless_tool_output(&restart_delta_body);
    assert_eq!(restart_delta["changed"], true);
    let restart_messages = restart_delta["messages"].as_array().unwrap();
    assert_eq!(restart_messages.len(), 1);
    assert_eq!(restart_messages[0]["message_id"], restart_message_id);
    assert_eq!(restart_messages[0]["message"], restart_body_marker);
    agent_executor.abort();
}

#[tokio::test]
async fn http_mcp_2026_protocol_error_matrix_and_legacy_session_compatibility() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime));
    let params = mcp_2026_params(json!({}));

    let cases = vec![
        (
            "missing headers",
            None,
            None,
            json!({"jsonrpc": "2.0", "id": 200, "method": "tools/list", "params": params.clone()}),
            StatusCode::BAD_REQUEST,
            json!(MCP_HEADER_MISMATCH),
            200,
        ),
        (
            "method header mismatch",
            Some(MCP_STATELESS_PROTOCOL_VERSION),
            Some("ping"),
            json!({"jsonrpc": "2.0", "id": 202, "method": "tools/list", "params": params.clone()}),
            StatusCode::BAD_REQUEST,
            json!(MCP_HEADER_MISMATCH),
            202,
        ),
        (
            "missing client capabilities",
            Some(MCP_STATELESS_PROTOCOL_VERSION),
            Some("tools/list"),
            json!({
                "jsonrpc": "2.0",
                "id": 2021,
                "method": "tools/list",
                "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": MCP_STATELESS_PROTOCOL_VERSION}}
            }),
            StatusCode::BAD_REQUEST,
            json!(-32602),
            2021,
        ),
        (
            "protocol header mismatch",
            Some("2099-01-01"),
            Some("tools/list"),
            json!({"jsonrpc": "2.0", "id": 2022, "method": "tools/list", "params": params.clone()}),
            StatusCode::BAD_REQUEST,
            json!(MCP_HEADER_MISMATCH),
            2022,
        ),
        (
            "malformed client info",
            Some(MCP_STATELESS_PROTOCOL_VERSION),
            Some("tools/list"),
            json!({
                "jsonrpc": "2.0",
                "id": 2023,
                "method": "tools/list",
                "params": {"_meta": {
                    "io.modelcontextprotocol/protocolVersion": MCP_STATELESS_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {},
                    "io.modelcontextprotocol/clientInfo": {"name": "missing-version"}
                }}
            }),
            StatusCode::BAD_REQUEST,
            json!(-32602),
            2023,
        ),
        (
            "invalid jsonrpc",
            Some(MCP_STATELESS_PROTOCOL_VERSION),
            Some("tools/list"),
            json!({"id": 2024, "method": "tools/list", "params": params.clone()}),
            StatusCode::BAD_REQUEST,
            json!(-32600),
            2024,
        ),
        (
            "unsupported protocol",
            Some("2099-01-01"),
            Some("tools/list"),
            json!({
                "jsonrpc": "2.0",
                "id": 203,
                "method": "tools/list",
                "params": {"_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2099-01-01",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }}
            }),
            StatusCode::BAD_REQUEST,
            json!(MCP_UNSUPPORTED_PROTOCOL_VERSION),
            203,
        ),
        (
            "unknown method",
            Some(MCP_STATELESS_PROTOCOL_VERSION),
            Some("prompts/list"),
            json!({"jsonrpc": "2.0", "id": 206, "method": "prompts/list", "params": params.clone()}),
            StatusCode::NOT_FOUND,
            json!(-32601),
            206,
        ),
    ];

    for (label, protocol, method, request, expected_status, expected_code, expected_id) in cases {
        let (status, body) =
            stateless_2026_jsonrpc(&service, "secret", protocol, method, None, None, request).await;
        assert_eq!(status, expected_status, "{label}: {body}");
        assert_eq!(body["id"], expected_id, "{label}: {body}");
        assert_eq!(body["error"]["code"], expected_code, "{label}: {body}");
        if label == "unsupported protocol" {
            assert_eq!(body["error"]["data"]["requested"], "2099-01-01");
            assert_eq!(
                body["error"]["data"]["supported"],
                json!(MCP_SUPPORTED_PROTOCOL_VERSIONS)
            );
        }
    }

    let (status, body) = stateless_2026_jsonrpc(
        &service,
        "secret",
        Some(MCP_STATELESS_PROTOCOL_VERSION),
        Some("tools/list"),
        None,
        Some("legacy-session-must-be-ignored"),
        json!({"jsonrpc": "2.0", "id": 201, "method": "tools/list", "params": params}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], 201);
    assert_eq!(body["result"]["resultType"], "complete");
    assert_eq!(body["result"]["cacheScope"], "private");
}

#[tokio::test]
async fn http_mcp_2026_tools_call_requires_matching_name_and_accepts_base64_sentinel() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime));
    let params = mcp_2026_params(json!({"name": "list_projects", "arguments": {}}));

    for (label, name_header, id) in [
        ("missing name", None, 204),
        ("mismatched name", Some("runtime_status"), 2041),
    ] {
        let (status, body) = stateless_2026_jsonrpc(
            &service,
            "secret",
            Some(MCP_STATELESS_PROTOCOL_VERSION),
            Some("tools/call"),
            name_header,
            None,
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params.clone()}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {body}");
        assert_eq!(body["id"], id);
        assert_eq!(body["error"]["code"], MCP_HEADER_MISMATCH);
    }

    let encoded = general_purpose::STANDARD.encode("list_projects");
    let encoded = format!("=?base64?{encoded}?=");
    let (status, body) = stateless_2026_jsonrpc(
        &service,
        "secret",
        Some(MCP_STATELESS_PROTOCOL_VERSION),
        Some("tools/call"),
        Some(&encoded),
        None,
        json!({"jsonrpc": "2.0", "id": 205, "method": "tools/call", "params": params}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], 205);
    assert_eq!(body["result"]["resultType"], "complete");
}

#[tokio::test]
async fn http_mcp_2026_reads_computer_app_template_with_cache_contract() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));
    let params = mcp_2026_ui_params(json!({"uri": MCP_COMPUTER_UI_RESOURCE_URI}));

    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "resources/read", true)
        .add_header(MCP_NAME_HEADER, MCP_COMPUTER_UI_RESOURCE_URI, true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2060,
            "method": "resources/read",
            "params": params
        }))
        .send(&service)
        .await;

    assert_eq!(effective_status(&response), StatusCode::OK);
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["result"]["resultType"], "complete");
    assert_eq!(
        body["result"]["ttlMs"],
        Value::from(MCP_COMPUTER_UI_RESOURCE_TTL_MS)
    );
    assert_eq!(body["result"]["cacheScope"], "private");
    assert_eq!(
        body["result"]["contents"][0]["uri"],
        MCP_COMPUTER_UI_RESOURCE_URI
    );
    assert_eq!(
        body["result"]["contents"][0]["mimeType"],
        MCP_UI_RESOURCE_MIME_TYPE
    );
    assert!(body["result"]["contents"][0]["_meta"]
        .get("openai/widgetDomain")
        .is_none());
    let html = body["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(html.starts_with("<div id=\"app\""));
    assert!(html.contains("HTML loaded"));
    assert!(html.contains("ui/initialize"));
    assert!(html.contains("ui/notifications/initialized"));
    assert!(html.contains("ui/notifications/tool-result"));
    assert!(!html.contains("tools/call"));
    assert!(!html.contains("ui/request-display-mode"));

    let (endpoint, action, operation, status, http_status, summary): (
        String,
        String,
        String,
        String,
        i64,
        String,
    ) = {
        let conn = db.conn_for_tests();
        conn.query_row(
            "SELECT endpoint, action_name, operation, status, http_status, summary_json FROM action_events ORDER BY started_at DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap()
    };
    assert_eq!(endpoint, "/mcp");
    assert_eq!(action, "resourcesRead");
    assert_eq!(operation, "computer_app_resource_read");
    assert_eq!(status, "success");
    assert_eq!(http_status, 200);
    let summary: Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(
        summary,
        json!({
            "transport": "mcp",
            "resource_uri": MCP_COMPUTER_UI_RESOURCE_URI,
            "resource_version": "v12",
            "protocol_era": "stateless_2026",
            "ui_capability_present": true,
            "mcp_error_code": Value::Null,
        })
    );
    let durable = summary.to_string();
    for forbidden in ["HTML loaded", "content_base64", "surface_", "Waiting for"] {
        assert!(!durable.contains(forbidden));
    }
}

#[tokio::test]
async fn http_mcp_computer_app_resource_protocol_failure_is_audited_without_content() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db.clone(), runtime));
    let mut params = mcp_2026_ui_params(json!({"uri": MCP_COMPUTER_UI_RESOURCE_URI}));
    params["_meta"]["io.modelcontextprotocol/protocolVersion"] = Value::from("2099-01-01");

    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(MCP_PROTOCOL_VERSION_HEADER, "2099-01-01", true)
        .add_header(MCP_METHOD_HEADER, "resources/read", true)
        .add_header(MCP_NAME_HEADER, MCP_COMPUTER_UI_RESOURCE_URI, true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 20601,
            "method": "resources/read",
            "params": params
        }))
        .send(&service)
        .await;

    assert_eq!(effective_status(&response), StatusCode::BAD_REQUEST);
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["error"]["code"], MCP_UNSUPPORTED_PROTOCOL_VERSION);

    let (action, operation, status, http_status, summary): (String, String, String, i64, String) = {
        let conn = db.conn_for_tests();
        conn.query_row(
            "SELECT action_name, operation, status, http_status, summary_json FROM action_events ORDER BY started_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap()
    };
    assert_eq!(action, "resourcesRead");
    assert_eq!(operation, "computer_app_resource_read");
    assert_eq!(status, "failed");
    assert_eq!(http_status, 400);
    let summary: Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(summary["resource_uri"], MCP_COMPUTER_UI_RESOURCE_URI);
    assert_eq!(summary["resource_version"], "v12");
    assert_eq!(summary["protocol_era"], "validation_failed");
    assert_eq!(summary["ui_capability_present"], true);
    assert_eq!(
        summary["mcp_error_code"],
        Value::from(MCP_UNSUPPORTED_PROTOCOL_VERSION)
    );
    let durable = summary.to_string();
    for forbidden in ["HTML loaded", "content_base64", "surface_", "Waiting for"] {
        assert!(!durable.contains(forbidden));
    }
}

#[tokio::test]
async fn http_mcp_2026_validates_resource_name_header_before_resource_contract() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));
    let params = mcp_2026_params(json!({"uri": "file:///demo.txt"}));

    let mut missing_name = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "resources/read", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2061,
            "method": "resources/read",
            "params": params.clone()
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&missing_name), StatusCode::BAD_REQUEST);
    let missing_body: Value = missing_name.take_json().await.unwrap();
    assert_eq!(missing_body["error"]["code"], MCP_HEADER_MISMATCH);

    let mut unsupported = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "resources/read", true)
        .add_header(MCP_NAME_HEADER, "file:///demo.txt", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2062,
            "method": "resources/read",
            "params": params
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&unsupported), StatusCode::BAD_REQUEST);
    let unsupported_body: Value = unsupported.take_json().await.unwrap();
    assert_eq!(unsupported_body["error"]["code"], -32602);
}

#[tokio::test]
async fn http_mcp_2026_rejects_legacy_lifecycle_and_cross_origin_transport() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));

    let mut initialize = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "initialize", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 207,
            "method": "initialize",
            "params": mcp_2026_params(json!({}))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&initialize), StatusCode::NOT_FOUND);
    let initialize_body: Value = initialize.take_json().await.unwrap();
    assert_eq!(initialize_body["error"]["code"], -32601);

    let cross_origin = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/list", true)
        .add_header("origin", "https://attacker.example", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 208,
            "method": "tools/list",
            "params": mcp_2026_params(json!({}))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&cross_origin), StatusCode::FORBIDDEN);

    let legacy_cross_origin = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header("origin", "https://attacker.example", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2081,
            "method": "tools/list",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(
        effective_status(&legacy_cross_origin),
        StatusCode::FORBIDDEN
    );

    let legacy_cross_origin_get = TestClient::get("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header("origin", "https://attacker.example", true)
        .send(&service)
        .await;
    assert_eq!(
        effective_status(&legacy_cross_origin_get),
        StatusCode::FORBIDDEN
    );

    let modern_get = TestClient::get("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .send(&service)
        .await;
    assert_eq!(
        effective_status(&modern_get),
        StatusCode::METHOD_NOT_ALLOWED
    );

    let modern_delete = TestClient::delete("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .send(&service)
        .await;
    assert_eq!(
        effective_status(&modern_delete),
        StatusCode::METHOD_NOT_ALLOWED
    );
}
#[tokio::test]
async fn http_mcp_tools_call_uses_result_envelope_for_success_and_business_failure() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));

    for (id, name, arguments, expected_is_error) in [
        (3, "list_projects", json!({}), false),
        (
            31,
            "git_status",
            json!({"project": "agent:nope:nope"}),
            true,
        ),
    ] {
        let (status, body) = legacy_mcp_jsonrpc(
            &service,
            "secret",
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        assert_eq!(body["id"], id);
        assert_eq!(body["result"]["content"][0]["type"], "text");
        assert!(body["result"]["content"][0]["text"].is_string());
        assert!(body["result"]["structuredContent"].is_object());
        assert!(body["result"]["structuredContent"]["success"].is_boolean());
        assert_eq!(body["result"]["isError"], expected_is_error);
        assert_eq!(
            body["result"]["structuredContent"]["success"],
            !expected_is_error
        );
        assert!(body.get("error").is_none(), "{name}: {body}");
    }
}

#[tokio::test]
async fn http_mcp_protocol_error_matrix_preserves_ids() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));
    let cases = [
        (
            "unknown tool",
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "no_such_tool", "arguments": {}}
            }),
            4,
            -32602,
            Some("no_such_tool"),
        ),
        (
            "unknown method",
            json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list", "params": {}}),
            5,
            -32601,
            Some("resources/list"),
        ),
        (
            "invalid jsonrpc",
            json!({"jsonrpc": "1.0", "id": 6, "method": "initialize", "params": {}}),
            6,
            -32600,
            None,
        ),
    ];

    for (label, request, expected_id, expected_code, message_fragment) in cases {
        let (status, body) = legacy_mcp_jsonrpc(&service, "secret", request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {body}");
        assert_eq!(body["id"], expected_id, "{label}: {body}");
        assert_eq!(body["error"]["code"], expected_code, "{label}: {body}");
        if let Some(fragment) = message_fragment {
            assert!(
                body["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains(fragment)),
                "{label}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn http_mcp_without_bearer_is_unauthorized() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));
    let resp = TestClient::post("http://localhost/mcp")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "initialize",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_mcp_with_wrong_bearer_is_unauthorized() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));
    let resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("wrong-token")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "initialize",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_mcp_with_correct_bearer_succeeds() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));
    let mut resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "ping",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["id"], 9);
    assert!(body["result"].is_object());
}
#[tokio::test]
async fn http_mcp_notification_returns_accepted_with_empty_body() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime());
    let service = Service::new(build_test_router(config, db, runtime));
    let mut resp = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::ACCEPTED);
    let text = resp.take_string().await.unwrap();
    assert!(text.is_empty(), "notification response body must be empty");
}

#[tokio::test]
async fn http_mcp_get_discovery_returns_metadata() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let runtime = Arc::new(test_runtime_with_surface(ModelSurface::FullOperatorRuntime));
    let service = Service::new(build_test_router(config, db, runtime));
    let mut resp = TestClient::get("http://localhost/mcp")
        .bearer_auth("secret")
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["name"], "webcodex");
    assert!(body["version"].is_string());
    assert_eq!(body["protocol"], "mcp");
    assert_eq!(
        body["runtimeExposure"],
        crate::model_surface::MODEL_SURFACE_FULL_OPERATOR_RUNTIME
    );
    assert!(body["protocolVersion"].is_string());
    assert_eq!(body["endpoint"], "/mcp");
    let methods = body["methods"].as_array().unwrap();
    let method_names: Vec<String> = methods
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect();
    assert!(method_names.contains(&"initialize".to_string()));
    assert!(method_names.contains(&"tools/list".to_string()));
    assert!(method_names.contains(&"tools/call".to_string()));
    assert!(method_names.contains(&"notifications/initialized".to_string()));
    assert_eq!(body["auth"]["type"], "bearer");
    assert_eq!(body["auth"]["required"], true);
    assert_eq!(
        body["auth"]["header"],
        "Authorization: Bearer <shared_key_or_wc_pat>"
    );
    let auth_json = body["auth"].to_string();
    assert!(
        auth_json.contains("shared_key_or_wc_pat"),
        "MCP auth metadata must advertise shared key or wc_pat bearer use: {auth_json}"
    );
    assert!(
        !auth_json.contains("wc_pat_user_api_token"),
        "MCP auth metadata must not regress to PAT-only placeholder: {auth_json}"
    );
}
