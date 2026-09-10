use salvo::http::StatusCode;
use salvo::test::{ResponseExt, TestClient};
use salvo::Service;
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::test]
async fn http_job_stop_preserves_owner_and_scope_boundaries() {
    use crate::runner_protocol::{
        RunnerCapabilities, RunnerJobUpdateRequest, RunnerPollRequest, RunnerRegisterRequest,
        ShellJobOpRequest,
    };

    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = super::test_config_oauth2(Some("secret"));
    let (_temp, db) = super::test_db();
    let alice = super::seed_user(&db, "alice");
    let bob = super::seed_user(&db, "bob");
    let alice_client = super::seed_oauth_client(&db, &alice);
    let bob_client = super::seed_oauth_client(&db, &bob);
    let alice_token = super::seed_oauth_access_token_with_shared_key_hash(
        &db,
        &alice_client,
        &alice,
        "job:run",
        None,
    );
    let bob_token = super::seed_oauth_access_token_with_shared_key_hash(
        &db,
        &bob_client,
        &bob,
        "job:run",
        None,
    );
    let read_token = super::seed_oauth_access_token_with_shared_key_hash(
        &db,
        &alice_client,
        &alice,
        "runtime:read",
        None,
    );
    let registry = Arc::new(super::RunnerRegistry::default());
    registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: "owner-runner".to_string(),
                runner_instance_id: "owner-instance".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                host_context: None,
                capabilities: RunnerCapabilities::default(),
                policy: None,
            },
        ))
        .await
        .unwrap();
    let runtime = Arc::new(
        crate::tool_runtime::ToolRuntime::new_for_tests_with_runner_registry(registry.clone()),
    );
    let service = Service::new(super::build_projects_router(config, db, runtime));
    let poll = RunnerPollRequest {
        client_id: "owner-runner".to_string(),
        runner_instance_id: "owner-instance".to_string(),
    };

    for running in [false, true] {
        let job = registry
            .start_job(
                serde_json::from_value::<ShellJobOpRequest>(json!({
                    "op": "start",
                    "client_id": "owner-runner",
                    "command": "fixture-only",
                    "timeout_secs": 30
                }))
                .unwrap(),
                "alice".to_string(),
            )
            .await
            .unwrap();
        if running {
            let request = registry.poll(poll.clone()).await.unwrap().unwrap();
            registry
                .update_job(
                    serde_json::from_value::<RunnerJobUpdateRequest>(json!({
                        "client_id": "owner-runner",
                        "agent_instance_id": "owner-instance",
                        "job_id": job.job_id,
                        "request_id": request.request_id,
                        "status": "running",
                        "update_seq": 1,
                        "finished": false
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        let status_before = registry.get_job(&job.job_id).await.unwrap().status;
        let body = json!({"job_id": job.job_id});

        let response = TestClient::post("http://localhost/api/jobs/stop")
            .json(&body)
            .send(&service)
            .await;
        assert_eq!(super::effective_status(&response), StatusCode::UNAUTHORIZED);

        let response = TestClient::post("http://localhost/api/jobs/stop")
            .bearer_auth(&read_token)
            .json(&body)
            .send(&service)
            .await;
        assert_eq!(super::effective_status(&response), StatusCode::FORBIDDEN);

        let mut response = TestClient::post("http://localhost/api/jobs/stop")
            .bearer_auth(&bob_token)
            .json(&body)
            .send(&service)
            .await;
        let output: Value = response.take_json().await.unwrap();
        assert_eq!(output["success"], false, "{output}");
        assert!(output["error"].as_str().unwrap().contains("unknown job"));
        assert_eq!(
            registry.get_job(&job.job_id).await.unwrap().status,
            status_before
        );
        if running {
            assert!(registry.poll(poll.clone()).await.unwrap().is_none());
        }

        let mut response = TestClient::post("http://localhost/api/jobs/stop")
            .bearer_auth(&alice_token)
            .json(&body)
            .send(&service)
            .await;
        assert_eq!(super::effective_status(&response), StatusCode::OK);
        let output: Value = response.take_json().await.unwrap();
        assert_eq!(output["success"], true, "{output}");
        if running {
            let stop = registry.poll(poll.clone()).await.unwrap().unwrap();
            assert_eq!(stop.kind, "stop_job");
            assert_eq!(stop.job_id.as_deref(), Some(job.job_id.as_str()));
        } else {
            assert_eq!(
                registry.get_job(&job.job_id).await.unwrap().status,
                "stopped"
            );
            assert!(registry.poll(poll.clone()).await.unwrap().is_none());
        }
    }
}

// =========================================================================
// runProjectShellCommand
// =========================================================================

#[tokio::test]
async fn http_projects_run_shell_requires_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let (_tmp, service) = super::phase2_service();
    let resp = TestClient::post("http://localhost/api/projects/run_shell")
        .json(&json!({"project": "demo", "command": "echo hi"}))
        .send(&service)
        .await;

    assert_eq!(super::effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_projects_run_shell_rejects_server_configured_project() {
    let config = super::test_config(Some("secret"));
    let (_tmp, db) = super::test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(tmp_proj.path(), "demo"));
    let service = Service::new(super::build_projects_router(config, db, runtime));

    let mut resp = TestClient::post("http://localhost/api/projects/run_shell")
        .bearer_auth("secret")
        .json(&json!({"project": "demo", "command": "echo hi"}))
        .send(&service)
        .await;
    assert_eq!(super::effective_status(&resp), StatusCode::BAD_REQUEST);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(body["error"].as_str().unwrap().contains("unknown_project"));
}

#[tokio::test]
async fn dedicated_run_shell_with_session_id_records_event() {
    let config = super::test_config(Some("secret"));
    let (_tmp, db) = super::test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let caps = crate::runner_protocol::RunnerCapabilities::default();
    let (runtime, registry) =
        super::register_import_agent_with_capabilities(tmp_proj.path(), Some(caps)).await;
    let service = Service::new(super::build_projects_router(config, db, runtime));

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session", "params": {"project": "agent:importer:demo"}}))
        .send(&service)
        .await;
    let start_body: Value = resp.take_json().await.unwrap();
    let session_id = start_body["output"]["session_id"].as_str().unwrap();

    let request = async {
        TestClient::post("http://localhost/api/projects/run_shell")
            .bearer_auth("secret")
            .json(&json!({
                "project": "agent:importer:demo",
                "command": "echo hi",
                "session_id": session_id
            }))
            .send(&service)
            .await
    };
    let complete = super::complete_one_agent_request(registry.clone(), "hi\n", "", 0);
    let (mut resp, _) = tokio::join!(request, complete);
    assert_eq!(super::effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(body["output"].get("session_recorded").is_none());
    assert!(body["output"].get("session_event_id").is_none());
    assert!(body["output"].get("session_id").is_none());

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "session_summary", "params": {"session_id": session_id}}))
        .send(&service)
        .await;
    let summary: Value = resp.take_json().await.unwrap();
    assert_eq!(summary["output"]["counts"]["tool_calls"], 1);
    assert_eq!(summary["output"]["counts"]["shell_like"], 1);
    assert!(summary["output"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["tool_name"] == "run_shell"
            && event["status"] == "succeeded"
            && event["exit_code"] == 0));
}

// =========================================================================
// startProjectShellJob
// =========================================================================

#[tokio::test]
async fn http_projects_run_job_requires_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let (_tmp, service) = super::phase2_service();
    let resp = TestClient::post("http://localhost/api/projects/run_job")
        .json(&json!({"project": "demo", "command": "echo hi"}))
        .send(&service)
        .await;

    assert_eq!(super::effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_projects_run_job_dispatches_to_runtime() {
    let (_tmp, service) = super::phase2_service();
    let mut resp = TestClient::post("http://localhost/api/projects/run_job")
        .bearer_auth("secret")
        .json(&json!({"project": "agent:nope:nope", "command": "echo hi"}))
        .send(&service)
        .await;

    assert_eq!(
        super::effective_status(&resp),
        StatusCode::BAD_REQUEST,
        "run_job should reach runtime and return structured error"
    );
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "run_job should return a structured runtime error"
    );
}

// =========================================================================
// Runtime job list/tail routes
// =========================================================================

#[tokio::test]
async fn http_jobs_routes_require_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let (_tmp, service) = super::phase2_service();

    for (path, body) in [
        ("/api/jobs/list", json!({})),
        ("/api/jobs/tail", json!({"job_id": "abc"})),
    ] {
        let resp = TestClient::post(format!("http://localhost{}", path))
            .json(&body)
            .send(&service)
            .await;
        assert_eq!(
            super::effective_status(&resp),
            StatusCode::UNAUTHORIZED,
            "{} should require auth",
            path
        );
    }
}

#[tokio::test]
async fn http_jobs_list_accepts_correct_bearer_and_routes_to_runtime() {
    let (_tmp, service) = super::phase2_service();
    let mut resp = TestClient::post("http://localhost/api/jobs/list")
        .bearer_auth("secret")
        .json(&json!({}))
        .send(&service)
        .await;

    assert_eq!(super::effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(body["output"]["jobs"].is_array());
}
