use super::*;

const CONNECTOR_TEST_PROJECT_ID: &str = "wc_proj_1234567890";
const CONNECTOR_TEST_WORKSPACE_ID: &str = "wc_ws_1234567890";
const CONNECTOR_TEST_GRANT_ID: &str = "wc_pgrant_3333333333333333";
const CONNECTOR_TEST_SUBJECT_ID: &str = "project:wc_pgrant_3333333333333333";
const CONNECTOR_TEST_CREDENTIAL: &str =
    "webcodex_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn build_connector_test_router(
    config: Arc<crate::Config>,
    db: Arc<crate::Database>,
    runtime: Arc<ToolRuntime>,
    project_root: &std::path::Path,
) -> Router {
    let state_root = project_root
        .parent()
        .expect("connector test project parent")
        .join("connector-state");
    let connector = crate::connector_runtime::ConnectorRuntime::new(
        runtime.clone(),
        db.clone(),
        crate::connector_runtime::ConnectorContext {
            project_id: CONNECTOR_TEST_PROJECT_ID.to_string(),
            project_name: "demo".to_string(),
            workspace_id: CONNECTOR_TEST_WORKSPACE_ID.to_string(),
            executor_project: "agent:hosted:demo".to_string(),
            executor_root: project_root.to_string_lossy().to_string(),
            runs_root: state_root.join("runs").to_string_lossy().to_string(),
            results_root: state_root.join("results").to_string_lossy().to_string(),
            project_registry_dir: state_root
                .join("agent/project-registry")
                .to_string_lossy()
                .to_string(),
            profile: "personal".to_string(),
            project_grant_id: CONNECTOR_TEST_GRANT_ID.to_string(),
        },
        crate::auth::ProjectCredentialVerifier::new(
            CONNECTOR_TEST_GRANT_ID.to_string(),
            CONNECTOR_TEST_CREDENTIAL,
        )
        .unwrap(),
    )
    .unwrap();
    Router::new()
        .hoop(affix_state::inject(config))
        .hoop(affix_state::inject(db))
        .hoop(affix_state::inject(runtime))
        .hoop(affix_state::inject(
            crate::connector_runtime::ConnectorRuntimeSlot(Some(Arc::new(connector))),
        ))
        .push(
            Router::with_path("mcp")
                .hoop(crate::AuthMiddleware)
                .get(mcp_info)
                .post(mcp_post),
        )
        .push(
            Router::with_path("api")
                .hoop(crate::AuthMiddleware)
                .push(crate::connector_runtime::http::routes())
                .push(Router::with_path("tools/call").post(crate::runtime_http::tools_call)),
        )
        .push(Router::with_path("openapi.json").get(crate::openapi::openapi_json))
}

fn seed_armed_mcp_execution(
    db: &crate::Database,
    project_root: &std::path::Path,
) -> crate::db::ConnectorExecution {
    db.ensure_connector_binding(crate::db::ConnectorBinding {
        project_id: CONNECTOR_TEST_PROJECT_ID,
        project_name: "demo",
        workspace_id: CONNECTOR_TEST_WORKSPACE_ID,
        executor_ref: "agent:hosted:demo",
        subject_id: CONNECTOR_TEST_SUBJECT_ID,
        profile: "personal",
        now: 10,
    })
    .unwrap();
    let task_id = format!("wc_task_{}", uuid::Uuid::new_v4().simple());
    let run_id = format!("wc_run_{}", uuid::Uuid::new_v4().simple());
    let root = project_root.to_string_lossy().into_owned();
    let task = db
        .start_connector_task(crate::db::NewConnectorTask {
            task_id: &task_id,
            run_id: &run_id,
            project_id: CONNECTOR_TEST_PROJECT_ID,
            workspace_id: CONNECTOR_TEST_WORKSPACE_ID,
            subject_id: CONNECTOR_TEST_SUBJECT_ID,
            goal: "exercise MCP 2026 durable task polling",
            mode: "read_only",
            target_executor_ref: "agent:hosted:demo",
            execution_executor_ref: "agent:hosted:demo",
            target_root: &root,
            execution_root: &root,
            baseline_commit: None,
            baseline_tree: None,
            isolated: false,
            now: 11,
        })
        .unwrap();
    let execution = match db
        .reserve_connector_execution(
            &task,
            "command",
            "mcp-task-op",
            "mcp-task-request-hash",
            &[],
            None,
            None,
            120,
            12,
        )
        .unwrap()
    {
        crate::db::ConnectorExecutionReservation::Created(execution) => execution,
        crate::db::ConnectorExecutionReservation::Existing(_) => unreachable!(),
    };
    db.start_connector_execution(&execution.execution_id, 13)
        .unwrap();
    db.arm_connector_terminal_continuation(&execution.execution_id, 14)
        .unwrap()
}

fn seed_mcp_execution(
    db: &crate::Database,
    project_root: &std::path::Path,
) -> crate::db::ConnectorExecution {
    let execution = seed_armed_mcp_execution(db, project_root);
    db.materialize_connector_execution_mcp_task_for_subject(
        &execution.execution_id,
        CONNECTOR_TEST_PROJECT_ID,
        CONNECTOR_TEST_SUBJECT_ID,
        15,
    )
    .unwrap()
}

async fn mcp_2026_task_request(
    service: &Service,
    user_token: &str,
    method: &str,
    task_id: &str,
    params: Value,
) -> salvo::Response {
    TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, method, true)
        .add_header(MCP_NAME_HEADER, task_id, true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 300,
            "method": method,
            "params": params
        }))
        .send(service)
        .await
}

#[tokio::test]
async fn http_project_connector_lists_and_dispatches_only_project_capabilities() {
    // The runtime surface is explicit below and the request path never
    // re-reads WEBCODEX_MCP_MODEL_SURFACE, so no env lock is needed.
    let config = test_config(Some("secret"));
    let (tmp, db) = test_db();
    let project = tmp.path().join("connector-project");
    crate::connector_runtime::tests::init_repo(&project);
    let user_token = "webcodex_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let runtime = Arc::new(test_runtime_with_exposure(
        RuntimeExposure::ProjectConnector,
    ));
    let service = Service::new(build_connector_test_router(config, db, runtime, &project));

    let mut discovery = TestClient::get("http://localhost/mcp")
        .bearer_auth(user_token)
        .send(&service)
        .await;
    assert_eq!(effective_status(&discovery), StatusCode::OK);
    let discovery_body: Value = discovery.take_json().await.unwrap();
    assert_eq!(
        discovery_body["runtimeExposure"],
        crate::model_surface::RUNTIME_EXPOSURE_PROJECT_CONNECTOR
    );
    assert!(discovery_body.get("modelSurface").is_none());
    assert!(!discovery_body.to_string().contains("canonical_connector"));

    let mut schema = TestClient::get("http://localhost/openapi.json")
        .send(&service)
        .await;
    assert_eq!(effective_status(&schema), StatusCode::OK);
    let schema_body: Value = schema.take_json().await.unwrap();
    assert_eq!(schema_body["paths"].as_object().unwrap().len(), 14);
    assert!(schema_body["paths"]
        .get("/api/connector/task/start")
        .is_some());
    assert!(schema_body["paths"]
        .get("/api/connector/code/navigate")
        .is_some());
    assert!(schema_body["paths"]
        .get("/api/connector/code/impact")
        .is_some());
    assert!(schema_body["paths"].get("/api/tools/call").is_none());
    let action_checks_schema = schema_body["paths"]["/api/connector/checks/run"]["post"]
        ["requestBody"]["content"]["application/json"]["schema"]
        .clone();
    let action_navigation_schema = schema_body["paths"]["/api/connector/code/navigate"]["post"]
        ["requestBody"]["content"]["application/json"]["schema"]
        .clone();
    let action_impact_schema = schema_body["paths"]["/api/connector/code/impact"]["post"]
        ["requestBody"]["content"]["application/json"]["schema"]
        .clone();

    let mut listed = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/list",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&listed), StatusCode::OK);
    let listed_body: Value = listed.take_json().await.unwrap();
    let names = listed_body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, crate::connector_runtime::surface::CAPABILITY_NAMES);
    assert_eq!(names.len(), 14);
    for raw_runtime_tool in ["runtime_status", "work_on_project", "call_runtime_tool"] {
        assert!(!names.contains(&raw_runtime_tool));
    }
    let mcp_checks_schema = listed_body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "checks_run")
        .unwrap()["inputSchema"]
        .clone();
    assert_eq!(mcp_checks_schema, action_checks_schema);
    let mcp_navigation_schema = listed_body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "code_navigate")
        .unwrap()["inputSchema"]
        .clone();
    assert_eq!(mcp_navigation_schema, action_navigation_schema);
    let mcp_impact_schema = listed_body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "code_impact")
        .unwrap()["inputSchema"]
        .clone();
    assert_eq!(mcp_impact_schema, action_impact_schema);

    let mut missing_window = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 18,
            "method": "tools/call",
            "params": {
                "name": "task_start",
                "arguments": { "goal": "must not create an anonymous context" }
            }
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&missing_window), StatusCode::BAD_REQUEST);
    let missing_window_body: Value = missing_window.take_json().await.unwrap();
    assert_eq!(missing_window_body["error"]["code"], -32600);
    assert!(missing_window_body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("initialize"));

    let mut initialized = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 19,
            "method": "initialize",
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&initialized), StatusCode::OK);
    let mcp_session_id = initialized
        .headers
        .get(crate::client_window::MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .expect("connector initialize session id")
        .to_string();
    let initialized_body: Value = initialized.take_json().await.unwrap();
    assert_eq!(
        initialized_body["result"]["serverInfo"]["runtimeExposure"],
        crate::model_surface::RUNTIME_EXPOSURE_PROJECT_CONNECTOR
    );
    assert!(initialized_body["result"]["serverInfo"]
        .get("modelSurface")
        .is_none());
    assert!(!initialized_body.to_string().contains("canonical_connector"));

    let mut action_started = TestClient::post("http://localhost/api/connector/task/start")
        .bearer_auth(user_token)
        .json(&json!({
            "goal": "exercise the Actions adapter",
            "mode": "read_only"
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&action_started), StatusCode::OK);
    let window_cookie = action_started
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("webcodex_window="))
        .and_then(|value| value.split(';').next())
        .expect("first connector call must mint a window cookie")
        .to_string();
    let action_body: Value = action_started.take_json().await.unwrap();
    assert_eq!(action_body["ok"], true);
    assert!(action_body["task_id"]
        .as_str()
        .unwrap()
        .starts_with("wc_task_"));
    assert!(action_body.get("success").is_none());

    let mut action_continued = TestClient::post("http://localhost/api/connector/task/start")
        .bearer_auth(user_token)
        .add_header("cookie", &window_cookie, true)
        .json(&json!({
            "goal": "continue the Actions inspection",
            "mode": "read_only"
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&action_continued), StatusCode::OK);
    let continued_body: Value = action_continued.take_json().await.unwrap();
    assert_eq!(continued_body["task_id"], action_body["task_id"]);
    assert_eq!(continued_body["data"]["continuation"], "continued");

    let action_conversation_request = |conversation_id: &'static str, goal: &'static str| {
        TestClient::post("http://localhost/api/connector/task/start")
            .bearer_auth(user_token)
            .add_header("openai-conversation-id", conversation_id, true)
            .json(&json!({"goal": goal, "mode": "read_only"}))
    };
    let mut conversation_a = action_conversation_request("conversation-a", "conversation A work")
        .send(&service)
        .await;
    let conversation_a_body: Value = conversation_a.take_json().await.unwrap();
    let mut conversation_b = action_conversation_request("conversation-b", "conversation B work")
        .send(&service)
        .await;
    let conversation_b_body: Value = conversation_b.take_json().await.unwrap();
    assert_ne!(
        conversation_a_body["task_id"], conversation_b_body["task_id"],
        "one credential must not merge two hosted conversations"
    );
    let mut conversation_a_again =
        action_conversation_request("conversation-a", "conversation A follow-up")
            .send(&service)
            .await;
    let conversation_a_again_body: Value = conversation_a_again.take_json().await.unwrap();
    assert_eq!(
        conversation_a_again_body["task_id"],
        conversation_a_body["task_id"]
    );
    assert_eq!(
        conversation_a_again_body["data"]["continuation"],
        "continued"
    );

    let mut legacy = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth(user_token)
        .json(&json!({ "name": "runtime_status", "arguments": {} }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&legacy), StatusCode::FORBIDDEN);
    let legacy_body: Value = legacy.take_json().await.unwrap();
    assert!(legacy_body["error"]
        .as_str()
        .unwrap()
        .contains("canonical connector capabilities"));

    let mut started = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            crate::client_window::MCP_SESSION_HEADER,
            &mcp_session_id,
            true,
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {
                "name": "task_start",
                "arguments": { "goal": "inspect the project", "mode": "read_only" }
            }
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&started), StatusCode::OK);
    let started_body: Value = started.take_json().await.unwrap();
    assert_eq!(
        started_body["result"]["content"][0]["text"],
        "WebCodex connector tool completed successfully."
    );
    assert!(!started_body["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("task_id"));
    assert_eq!(started_body["result"]["structuredContent"]["ok"], true);
    assert!(started_body["result"]["structuredContent"]["task_id"]
        .as_str()
        .unwrap()
        .starts_with("wc_task_"));
    assert!(started_body["result"]["structuredContent"]
        .get("success")
        .is_none());

    let mut continued = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            crate::client_window::MCP_SESSION_HEADER,
            &mcp_session_id,
            true,
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 211,
            "method": "tools/call",
            "params": {
                "name": "task_start",
                "arguments": {
                    "goal": "continue inspecting the project",
                    "mode": "read_only"
                }
            }
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&continued), StatusCode::OK);
    let continued_body: Value = continued.take_json().await.unwrap();
    assert_eq!(
        continued_body["result"]["structuredContent"]["task_id"],
        started_body["result"]["structuredContent"]["task_id"]
    );
    assert_eq!(
        continued_body["result"]["structuredContent"]["data"]["continuation"],
        "continued"
    );

    let mut hidden = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "tools/call",
            "params": { "name": "runtime_status", "arguments": {} }
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&hidden), StatusCode::BAD_REQUEST);
    let hidden_body: Value = hidden.take_json().await.unwrap();
    assert_eq!(hidden_body["error"]["code"], -32602);
    assert!(hidden_body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not available"));
}

#[tokio::test]
async fn http_project_connector_2026_uses_explicit_task_ids_without_transport_window_state() {
    // The runtime surface is explicit below and the request path never
    // re-reads WEBCODEX_MCP_MODEL_SURFACE, so no env lock is needed.
    let config = test_config(Some("secret"));
    let (tmp, db) = test_db();
    let project = tmp.path().join("connector-2026-project");
    crate::connector_runtime::tests::init_repo(&project);
    let user_token = "webcodex_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let runtime = Arc::new(test_runtime_with_exposure(
        RuntimeExposure::ProjectConnector,
    ));
    let service = Service::new(build_connector_test_router(config, db, runtime, &project));

    let start_params = |goal: &str| {
        mcp_2026_params(json!({
            "name": "task_start",
            "arguments": { "goal": goal, "mode": "read_only" }
        }))
    };

    let mut first = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(MCP_NAME_HEADER, "task_start", true)
        .add_header(
            crate::client_window::MCP_SESSION_HEADER,
            "legacy-session-must-not-bind-2026",
            true,
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 220,
            "method": "tools/call",
            "params": start_params("inspect the stateless connector path")
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&first), StatusCode::OK);
    assert!(first
        .headers
        .get(crate::client_window::MCP_SESSION_HEADER)
        .is_none());
    let first_body: Value = first.take_json().await.unwrap();
    let first_task_id = first_body["result"]["structuredContent"]["task_id"]
        .as_str()
        .expect("2026 task_start must return task_id")
        .to_string();
    assert!(first_task_id.starts_with("wc_task_"));
    assert_eq!(first_body["result"]["resultType"], "complete");

    let mut second = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(MCP_NAME_HEADER, "task_start", true)
        .add_header(
            crate::client_window::MCP_SESSION_HEADER,
            "legacy-session-must-not-bind-2026",
            true,
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 221,
            "method": "tools/call",
            "params": start_params("start independent stateless work")
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&second), StatusCode::OK);
    let second_body: Value = second.take_json().await.unwrap();
    let second_task_id = second_body["result"]["structuredContent"]["task_id"]
        .as_str()
        .expect("second 2026 task_start must return task_id");
    assert_ne!(
        second_task_id, first_task_id,
        "2026 must not derive hidden continuity from Mcp-Session-Id"
    );

    let mut resumed = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(MCP_NAME_HEADER, "task_resume", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 222,
            "method": "tools/call",
            "params": mcp_2026_params(json!({
                "name": "task_resume",
                "arguments": { "task_id": first_task_id }
            }))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resumed), StatusCode::OK);
    let resumed_body: Value = resumed.take_json().await.unwrap();
    assert_eq!(
        resumed_body["result"]["structuredContent"]["task_id"],
        first_task_id
    );
    assert_eq!(
        resumed_body["result"]["structuredContent"]["data"]["continuity"]["window_rebound"],
        false
    );
}

#[tokio::test]
async fn http_project_connector_2026_uses_openai_session_as_host_window_identity() {
    let config = test_config(Some("secret"));
    let (tmp, db) = test_db();
    let project = tmp.path().join("connector-2026-openai-session-project");
    crate::connector_runtime::tests::init_repo(&project);
    let user_token = "webcodex_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let runtime = Arc::new(test_runtime_with_exposure(
        RuntimeExposure::ProjectConnector,
    ));
    let service = Service::new(build_connector_test_router(config, db, runtime, &project));

    let start_params = |session: &str, goal: &str| {
        let mut params = mcp_2026_params(json!({
            "name": "task_start",
            "arguments": { "goal": goal, "mode": "read_only" }
        }));
        params["_meta"]["openai/session"] = json!(session);
        params
    };

    let mut first = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(MCP_NAME_HEADER, "task_start", true)
        .add_header(
            crate::client_window::MCP_SESSION_HEADER,
            "legacy-session-must-not-bind-2026",
            true,
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 223,
            "method": "tools/call",
            "params": start_params("openai-chat-a", "inspect from ChatGPT conversation A")
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&first), StatusCode::OK);
    assert!(first
        .headers
        .get(crate::client_window::MCP_SESSION_HEADER)
        .is_none());
    let first_body: Value = first.take_json().await.unwrap();
    let first_task_id = first_body["result"]["structuredContent"]["task_id"]
        .as_str()
        .expect("OpenAI conversation task_start must return task_id")
        .to_string();

    let mut continued = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(MCP_NAME_HEADER, "task_start", true)
        .add_header(
            crate::client_window::MCP_SESSION_HEADER,
            "different-legacy-session-still-must-not-bind-2026",
            true,
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 224,
            "method": "tools/call",
            "params": start_params("openai-chat-a", "continue ChatGPT conversation A")
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&continued), StatusCode::OK);
    let continued_body: Value = continued.take_json().await.unwrap();
    assert_eq!(
        continued_body["result"]["structuredContent"]["task_id"],
        first_task_id
    );
    assert_eq!(
        continued_body["result"]["structuredContent"]["data"]["continuation"],
        "continued"
    );

    let mut other = TestClient::post("http://localhost/mcp")
        .bearer_auth(user_token)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(MCP_NAME_HEADER, "task_start", true)
        .add_header(
            crate::client_window::MCP_SESSION_HEADER,
            "legacy-session-must-not-bind-2026",
            true,
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 225,
            "method": "tools/call",
            "params": start_params("openai-chat-b", "inspect from ChatGPT conversation B")
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&other), StatusCode::OK);
    let other_body: Value = other.take_json().await.unwrap();
    assert_ne!(
        other_body["result"]["structuredContent"]["task_id"], first_task_id,
        "different OpenAI conversations must not share connector task state"
    );
}

#[tokio::test]
async fn http_project_connector_2026_tasks_poll_durable_execution_across_reopen() {
    let config = test_config(Some("secret"));
    let (tmp, db) = test_db();
    let db_path = tmp.path().join("test.db");
    let project = tmp.path().join("connector-2026-task-project");
    crate::connector_runtime::tests::init_repo(&project);
    let runtime = Arc::new(test_runtime_with_exposure(
        RuntimeExposure::ProjectConnector,
    ));
    let service = Service::new(build_connector_test_router(
        config,
        db.clone(),
        runtime,
        &project,
    ));

    let mut discover = TestClient::post("http://localhost/mcp")
        .bearer_auth(CONNECTOR_TEST_CREDENTIAL)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "server/discover", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 299,
            "method": "server/discover",
            "params": mcp_2026_params(json!({}))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&discover), StatusCode::OK);
    let discover_body: Value = discover.take_json().await.unwrap();
    assert_eq!(
        discover_body["result"]["capabilities"]["extensions"][MCP_TASKS_EXTENSION],
        json!({})
    );
    assert_eq!(
        discover_body["result"]["runtimeExposure"],
        crate::model_surface::RUNTIME_EXPOSURE_PROJECT_CONNECTOR
    );
    assert!(discover_body["result"]["capabilities"]
        .get("resources")
        .is_none());

    let mut resources = TestClient::post("http://localhost/mcp")
        .bearer_auth(CONNECTOR_TEST_CREDENTIAL)
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "resources/list", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 300,
            "method": "resources/list",
            "params": mcp_2026_params(json!({}))
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resources), StatusCode::NOT_FOUND);
    let resources_body: Value = resources.take_json().await.unwrap();
    assert_eq!(resources_body["error"]["code"], -32601);

    let execution = seed_mcp_execution(&db, &project);
    let task_id = execution.execution_id.clone();
    assert!(execution.mcp_task_is_materialized());
    assert_eq!(execution.mcp_task_materialized_at, Some(15));
    let create = mcp_create_task_result(&execution);
    assert_eq!(create["resultType"], "task");
    assert_eq!(create["taskId"], task_id);
    assert_eq!(create["status"], "working");
    assert_eq!(create["lastUpdatedAt"], create["createdAt"]);

    let mut missing_capability = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        &task_id,
        mcp_2026_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(
        effective_status(&missing_capability),
        StatusCode::BAD_REQUEST
    );
    let missing_body: Value = missing_capability.take_json().await.unwrap();
    assert_eq!(
        missing_body["error"]["code"],
        MCP_MISSING_REQUIRED_CLIENT_CAPABILITY
    );
    assert_eq!(
        missing_body["error"]["data"]["requiredCapabilities"]["extensions"][MCP_TASKS_EXTENSION],
        json!({})
    );

    let mut working = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        &task_id,
        mcp_2026_tasks_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(effective_status(&working), StatusCode::OK);
    assert!(working
        .headers
        .get(crate::client_window::MCP_SESSION_HEADER)
        .is_none());
    let working_body: Value = working.take_json().await.unwrap();
    assert_eq!(working_body["result"]["resultType"], "complete");
    assert_eq!(working_body["result"]["taskId"], task_id);
    assert_eq!(working_body["result"]["status"], "working");
    assert!(working_body["result"].get("result").is_none());

    let mut updated = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/update",
        &task_id,
        mcp_2026_tasks_params(json!({
            "taskId": task_id,
            "inputResponses": { "unused": { "resultType": "complete" } }
        })),
    )
    .await;
    assert_eq!(effective_status(&updated), StatusCode::OK);
    let updated_body: Value = updated.take_json().await.unwrap();
    assert_eq!(updated_body["result"]["resultType"], "complete");
    assert!(updated_body["result"].get("taskId").is_none());
    assert!(matches!(
        db.connector_execution_for_subject(
            &task_id,
            CONNECTOR_TEST_PROJECT_ID,
            "project:foreign-grant"
        ),
        Err(crate::db::ConnectorTaskStoreError::NotFound)
    ));
    assert!(matches!(
        db.connector_execution_for_subject(&task_id, "wc_proj_foreign", CONNECTOR_TEST_SUBJECT_ID),
        Err(crate::db::ConnectorTaskStoreError::NotFound)
    ));

    let durable_tail = json!({
        "stdout": "durable terminal stdout\n",
        "stderr": "durable terminal stderr\n",
        "bounded": true
    });
    let finalized = db
        .observe_connector_execution(
            &task_id,
            crate::db::ConnectorExecutionObservation {
                executor_status: "failed",
                stdout_cursor: 2,
                stderr_cursor: 2,
                exit_code: Some(7),
                started_at: Some(13),
                finished_at: Some(20),
                check_completed: None,
                failed_check: None,
                assertion_evidence: None,
                validated_workspace_sha256: None,
                executor_failure_code: None,
                mcp_task_output_tail: Some(&durable_tail),
                now: 20,
            },
        )
        .unwrap();
    assert_eq!(finalized.state, crate::db::ConnectorExecutionState::Failed);
    assert_eq!(finalized.mcp_task_result_finalized_at, Some(20));
    assert_eq!(finalized.mcp_task_output_tail.as_ref(), Some(&durable_tail));
    let mut completed = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        &task_id,
        mcp_2026_tasks_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(effective_status(&completed), StatusCode::OK);
    let completed_body: Value = completed.take_json().await.unwrap();
    assert_eq!(completed_body["result"]["status"], "completed");
    assert_eq!(completed_body["result"]["taskId"], task_id);
    assert_eq!(
        completed_body["result"]["result"]["structuredContent"]["data"]["execution"]
            ["execution_status"],
        "failed"
    );
    assert_eq!(
        completed_body["result"]["result"]["structuredContent"]["data"]["execution"]["output_tail"],
        durable_tail
    );

    let mut replayed = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        &task_id,
        mcp_2026_tasks_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(effective_status(&replayed), StatusCode::OK);
    let replayed_body: Value = replayed.take_json().await.unwrap();
    assert_eq!(replayed_body["result"], completed_body["result"]);

    drop(service);
    drop(db);
    let reopened_db = Arc::new(crate::Database::open(&db_path).unwrap());
    let reopened_runtime = Arc::new(test_runtime_with_exposure(
        RuntimeExposure::ProjectConnector,
    ));
    let reopened_service = Service::new(build_connector_test_router(
        test_config(Some("secret")),
        reopened_db,
        reopened_runtime,
        &project,
    ));
    let mut after_restart = mcp_2026_task_request(
        &reopened_service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        &task_id,
        mcp_2026_tasks_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(effective_status(&after_restart), StatusCode::OK);
    let after_restart_body: Value = after_restart.take_json().await.unwrap();
    assert_eq!(after_restart_body["result"], completed_body["result"]);
}

#[tokio::test]
async fn http_project_connector_2026_tasks_reject_unmaterialized_execution_ids() {
    let config = test_config(Some("secret"));
    let (tmp, db) = test_db();
    let project = tmp.path().join("connector-2026-unmaterialized-task");
    crate::connector_runtime::tests::init_repo(&project);
    let runtime = Arc::new(test_runtime_with_exposure(
        RuntimeExposure::ProjectConnector,
    ));
    let service = Service::new(build_connector_test_router(
        config,
        db.clone(),
        runtime,
        &project,
    ));
    let execution = seed_armed_mcp_execution(&db, &project);
    let execution_id = execution.execution_id.clone();
    assert!(!execution.mcp_task_is_materialized());

    for (method, params) in [
        ("tasks/get", json!({ "taskId": execution_id })),
        (
            "tasks/update",
            json!({ "taskId": execution_id, "inputResponses": {} }),
        ),
        ("tasks/cancel", json!({ "taskId": execution_id })),
    ] {
        let mut response = mcp_2026_task_request(
            &service,
            CONNECTOR_TEST_CREDENTIAL,
            method,
            &execution_id,
            mcp_2026_tasks_params(params),
        )
        .await;
        assert_eq!(effective_status(&response), StatusCode::BAD_REQUEST);
        let body: Value = response.take_json().await.unwrap();
        assert_eq!(body["error"]["code"], -32602, "{method}: {body}");
    }

    let terminal = db
        .finish_connector_execution(
            &execution_id,
            crate::db::ConnectorExecutionFailure::Submission("terminal_before_materialization"),
            20,
        )
        .unwrap();
    assert!(terminal.is_terminal());
    assert!(!terminal.mcp_task_is_materialized());
    assert!(!terminal.mcp_task_result_is_finalized());
    let rematerialize = db
        .materialize_connector_execution_mcp_task_for_subject(
            &execution_id,
            CONNECTOR_TEST_PROJECT_ID,
            CONNECTOR_TEST_SUBJECT_ID,
            21,
        )
        .unwrap();
    assert!(!rematerialize.mcp_task_is_materialized());

    let mut response = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        &execution_id,
        mcp_2026_tasks_params(json!({ "taskId": execution_id })),
    )
    .await;
    assert_eq!(effective_status(&response), StatusCode::BAD_REQUEST);
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["error"]["code"], -32602);
}

#[tokio::test]
async fn http_project_connector_2026_tasks_cancel_reuses_execution_cancellation() {
    let config = test_config(Some("secret"));
    let (tmp, db) = test_db();
    let project = tmp.path().join("connector-2026-task-cancel");
    crate::connector_runtime::tests::init_repo(&project);
    let runtime = Arc::new(test_runtime_with_exposure(
        RuntimeExposure::ProjectConnector,
    ));
    let service = Service::new(build_connector_test_router(
        config,
        db.clone(),
        runtime,
        &project,
    ));
    let execution = seed_mcp_execution(&db, &project);
    let task_id = execution.execution_id.clone();

    let mut cancelled = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/cancel",
        &task_id,
        mcp_2026_tasks_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(effective_status(&cancelled), StatusCode::OK);
    let cancelled_body: Value = cancelled.take_json().await.unwrap();
    assert_eq!(cancelled_body["result"]["resultType"], "complete");

    let mut observed = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        &task_id,
        mcp_2026_tasks_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(effective_status(&observed), StatusCode::OK);
    let observed_body: Value = observed.take_json().await.unwrap();
    assert_eq!(observed_body["result"]["status"], "working");
    assert_eq!(
        observed_body["result"]["result"].is_null(),
        true,
        "cancel ACK must not claim terminal cancellation"
    );
    assert_eq!(
        db.connector_execution(&task_id).unwrap().state,
        crate::db::ConnectorExecutionState::CancelRequested
    );

    db.observe_connector_execution(
        &task_id,
        crate::db::ConnectorExecutionObservation {
            executor_status: "cancelled",
            stdout_cursor: 1,
            stderr_cursor: 1,
            exit_code: None,
            started_at: None,
            finished_at: Some(21),
            check_completed: None,
            failed_check: None,
            assertion_evidence: None,
            validated_workspace_sha256: None,
            executor_failure_code: None,
            mcp_task_output_tail: None,
            now: 21,
        },
    )
    .unwrap();
    let mut terminal = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        &task_id,
        mcp_2026_tasks_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(effective_status(&terminal), StatusCode::OK);
    let terminal_body: Value = terminal.take_json().await.unwrap();
    assert_eq!(terminal_body["result"]["status"], "cancelled");
    assert!(terminal_body["result"].get("result").is_none());

    let mut repeated_cancel = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/cancel",
        &task_id,
        mcp_2026_tasks_params(json!({ "taskId": task_id })),
    )
    .await;
    assert_eq!(effective_status(&repeated_cancel), StatusCode::OK);
    let repeated_body: Value = repeated_cancel.take_json().await.unwrap();
    assert_eq!(repeated_body["result"]["resultType"], "complete");

    let unknown_task_id = "wc_exec_ffffffffffffffffffffffffffffffff";
    let mut missing = mcp_2026_task_request(
        &service,
        CONNECTOR_TEST_CREDENTIAL,
        "tasks/get",
        unknown_task_id,
        mcp_2026_tasks_params(json!({ "taskId": unknown_task_id })),
    )
    .await;
    assert_eq!(effective_status(&missing), StatusCode::BAD_REQUEST);
    let missing_body: Value = missing.take_json().await.unwrap();
    assert_eq!(missing_body["error"]["code"], -32602);
    assert!(!missing_body.to_string().contains(&task_id));
}
