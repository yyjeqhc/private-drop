use salvo::http::StatusCode;
use salvo::test::{ResponseExt, TestClient};
use salvo::Service;
use serde_json::{json, Value};
use std::sync::Arc;

fn single_model_ergonomics(
    db: &crate::Database,
    action_session_id: &str,
    expected_tool: &str,
) -> Value {
    let events = db.list_action_events(action_session_id, 20).unwrap();
    assert_eq!(
        events.len(),
        1,
        "one outer tool call must create one ActionAudit row"
    );
    assert_eq!(events[0].operation.as_deref(), Some(expected_tool));
    let summary: Value = serde_json::from_str(&events[0].summary_json).unwrap();
    summary
        .get("model_ergonomics")
        .cloned()
        .unwrap_or_else(|| panic!("missing generic telemetry in summary: {summary}"))
}

#[tokio::test]
async fn api_model_ergonomics_success_is_exact_and_queryable() {
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let project_tmp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(
        project_tmp.path(),
        "demo",
    ));
    let service = Service::new(super::build_projects_router(config, db.clone(), runtime));

    let mut response = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .add_header("x-action-session-id", "ergonomics-success", true)
        .json(&json!({"tool": "tool_manifest", "intent": "audit"}))
        .send(&service)
        .await;
    assert_eq!(super::effective_status(&response), StatusCode::OK);
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["success"], true);

    let telemetry = single_model_ergonomics(&db, "ergonomics-success", "tool_manifest");
    assert_eq!(telemetry["schema_version"], 4);
    assert_eq!(telemetry["tool_name"], "tool_manifest");
    assert_eq!(telemetry["tool_category"], "runtime");
    assert_eq!(telemetry["success"], true);
    assert!(telemetry["duration_ms"].as_u64().is_some());
    assert_eq!(
        telemetry["serialized_result_bytes"].as_u64().unwrap(),
        serde_json::to_vec(&body).unwrap().len() as u64,
        "API telemetry must count the exact final ToolResult UTF-8 serialization"
    );
    assert!(telemetry["error_kind"].is_null());
    assert!(telemetry["failure_kind"].is_null());
    assert!(telemetry["recovery_kind"].is_null());
    assert!(telemetry.get("result_truncated").is_none());
}

#[tokio::test]
async fn api_model_ergonomics_failure_uses_structured_kinds_without_private_text() {
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let project_tmp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(
        project_tmp.path(),
        "demo",
    ));
    let service = Service::new(super::build_projects_router(config, db.clone(), runtime));
    let private_project = "PRIVATE-command-path-query-token";

    let mut response = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .add_header("x-action-session-id", "ergonomics-failure", true)
        .json(&json!({"tool": "project_overview", "project": private_project}))
        .send(&service)
        .await;
    assert_eq!(super::effective_status(&response), StatusCode::BAD_REQUEST);
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["success"], false);
    assert_eq!(body["output"]["error_kind"], "unknown_project");
    assert_eq!(body["output"]["recovery_kind"], "fix_input");

    let telemetry = single_model_ergonomics(&db, "ergonomics-failure", "project_overview");
    assert_eq!(telemetry["success"], false);
    assert_eq!(telemetry["error_kind"], "unknown_project");
    assert!(telemetry["failure_kind"].is_null());
    assert_eq!(telemetry["recovery_kind"], "fix_input");
    assert_eq!(
        telemetry["serialized_result_bytes"].as_u64().unwrap(),
        serde_json::to_vec(&body).unwrap().len() as u64
    );
    let serialized = serde_json::to_string(&telemetry).unwrap();
    for forbidden in [private_project, "command", "path", "query", "token"] {
        assert!(
            !serialized.contains(forbidden),
            "generic telemetry leaked arbitrary/private text {forbidden}: {serialized}"
        );
    }
}

#[tokio::test]
async fn api_pre_result_invalid_arguments_still_counts_without_fabricated_bytes() {
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let project_tmp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(
        project_tmp.path(),
        "demo",
    ));
    let service = Service::new(super::build_projects_router(config, db.clone(), runtime));

    let mut response = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .add_header("x-action-session-id", "ergonomics-invalid", true)
        .json(&json!({"tool": "read_files"}))
        .send(&service)
        .await;
    assert_eq!(super::effective_status(&response), StatusCode::BAD_REQUEST);
    let body: Value = response.take_json().await.unwrap();
    assert!(body["error"].is_string());

    let telemetry = single_model_ergonomics(&db, "ergonomics-invalid", "read_files");
    assert_eq!(telemetry["success"], false);
    assert_eq!(telemetry["error_kind"], "invalid_arguments");
    assert!(telemetry["serialized_result_bytes"].is_null());
    assert!(telemetry["failure_kind"].is_null());
    assert!(telemetry["recovery_kind"].is_null());
    assert!(telemetry["execution_state"].is_null());
}

#[tokio::test]
async fn api_batch_call_records_one_generic_outer_invocation() {
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let project_tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = super::register_import_agent_with_capabilities(
        project_tmp.path(),
        Some(crate::runner_protocol::RunnerCapabilities {
            file_read: true,
            ..Default::default()
        }),
    )
    .await;
    let executor = super::spawn_startup_agent_executor(registry);
    let service = Service::new(super::build_projects_router(config, db.clone(), runtime));

    let mut response = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .add_header("x-action-session-id", "ergonomics-batch", true)
        .json(&json!({
            "tool": "read_files",
            "project": "agent:importer:demo",
            "items": [
                {"path": "missing-a.rs"},
                {"path": "missing-b.rs"}
            ]
        }))
        .send(&service)
        .await;
    let status = super::effective_status(&response);
    let body: Value = response.take_json().await.unwrap();
    executor.abort();

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true);
    assert_eq!(body["output"]["items"].as_array().unwrap().len(), 2);
    assert!(body["output"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["success"] == false));
    let telemetry = single_model_ergonomics(&db, "ergonomics-batch", "read_files");
    assert_eq!(telemetry["tool_name"], "read_files");
    assert_eq!(telemetry["success"], true);
}

#[tokio::test]
async fn api_work_on_project_preferences_persist_as_privacy_bounded_action_audit_facts() {
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let project_tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = super::register_import_agent(project_tmp.path()).await;
    let executor = super::spawn_startup_agent_executor(registry);
    let service = Service::new(super::build_projects_router(config, db.clone(), runtime));
    let private_instruction = "PRIVATE_API_WORK_ON_PROJECT_INSTRUCTION";
    let project = "agent:importer:demo";

    let mut response = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .add_header("x-action-session-id", "ergonomics-work-on-project", true)
        .json(&json!({
            "tool": "work_on_project",
            "project": project,
            "instruction": private_instruction,
            "include_project_instructions": false,
            "include_workflow_guidance": false,
            "include_extension_catalog": false
        }))
        .send(&service)
        .await;
    let status = super::effective_status(&response);
    let body: Value = response.take_json().await.unwrap();
    executor.abort();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true, "{body}");

    let telemetry = single_model_ergonomics(&db, "ergonomics-work-on-project", "work_on_project");
    assert_eq!(telemetry["schema_version"], 4);
    let facts = &telemetry["work_on_project"];
    assert_eq!(facts["resume_requested"], false);
    assert_eq!(facts["source"], "project");
    assert_eq!(facts["mode"], "checkout");
    assert_eq!(facts["mode_explicit"], false);
    assert_eq!(facts["base_ref_present"], false);
    assert_eq!(facts["include_project_instructions"], false);
    assert_eq!(facts["include_project_instructions_explicit"], true);
    assert_eq!(facts["include_workflow_guidance"], false);
    assert_eq!(facts["include_workflow_guidance_explicit"], true);
    assert_eq!(facts["include_extension_catalog"], false);
    assert_eq!(facts["include_extension_catalog_explicit"], true);
    let serialized = serde_json::to_string(&telemetry).unwrap();
    for forbidden in [private_instruction, project] {
        assert!(
            !serialized.contains(forbidden),
            "persisted API model ergonomics leaked {forbidden}: {serialized}"
        );
    }
}

#[tokio::test]
async fn action_audit_sink_failure_never_changes_success_or_failure_tool_result() {
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let project_tmp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(
        project_tmp.path(),
        "demo",
    ));
    let service = Service::new(super::build_projects_router(config, db.clone(), runtime));
    db.conn_for_tests()
        .execute("DROP TABLE action_events", [])
        .unwrap();

    let mut success = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "tool_manifest", "intent": "audit"}))
        .send(&service)
        .await;
    assert_eq!(super::effective_status(&success), StatusCode::OK);
    let success_body: Value = success.take_json().await.unwrap();
    assert_eq!(success_body["success"], true);
    assert!(success_body["output"]["tools"].is_array());

    let mut failure = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "project_overview", "project": "missing-project"}))
        .send(&service)
        .await;
    assert_eq!(super::effective_status(&failure), StatusCode::BAD_REQUEST);
    let failure_body: Value = failure.take_json().await.unwrap();
    assert_eq!(failure_body["success"], false);
    assert_eq!(failure_body["output"]["error_kind"], "unknown_project");
    assert_eq!(failure_body["output"]["recovery_kind"], "fix_input");
}
