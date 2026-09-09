use super::*;
use crate::action_audit_sessions::{record_action_event, ActionAuditEventInput};
use crate::test_support::{seed_oauth_client_named, seed_user, test_config, test_db};
use crate::Database;
use salvo::prelude::{affix_state, Response, Router, StatusCode};
use salvo::test::{ResponseExt, TestClient};
use salvo::Service;
use serde_json::{json, Value};
use std::sync::Arc;

fn build_audit_router(config: Arc<crate::Config>, db: Arc<Database>) -> Router {
    Router::new()
        .hoop(affix_state::inject(config))
        .hoop(affix_state::inject(db))
        .push(
            Router::with_path("api")
                .hoop(crate::AuthMiddleware)
                .push(Router::with_path("audit/sessions").post(audit_sessions))
                .push(Router::with_path("audit/session").post(audit_session))
                .push(Router::with_path("audit/stats").post(audit_stats)),
        )
}

fn effective_status(resp: &Response) -> StatusCode {
    resp.status_code.unwrap_or(StatusCode::OK)
}

fn seed_event(
    db: &Arc<Database>,
    session_id: &str,
    endpoint: &str,
    action_name: &str,
    status: &str,
    summary: Value,
) {
    record_action_event(
        db,
        ActionAuditEventInput {
            explicit_session_id: Some(session_id.to_string()),
            session_title: None,
            endpoint: endpoint.to_string(),
            action_name: action_name.to_string(),
            operation: Some("op".to_string()),
            project: Some("demo".to_string()),
            principal_kind: None,
            principal_user_id: None,
            oauth_client_id: None,
            status: status.to_string(),
            http_status: Some(200),
            started_at: 1,
            ended_at: 2,
            duration_ms: 10,
            error_summary: None,
            warning_summary: None,
            changed_files: Vec::new(),
            ids: json!({}),
            summary,
            request_bytes: None,
            response_bytes: None,
            client_window_key: None,
            client_window_source: None,
            server_trace_id: None,
            principal_correlation_kind: None,
            principal_correlation_id: None,
            window_started_at_ms: None,
            window_ended_at_ms: None,
            request_observed_at_ms: None,
            response_handed_at_ms: None,
            window_transition_kind: None,
            response_streaming: None,
            window_continuity_eligible: None,
            window_meaningful: false,
            recorder_gap_session_id: None,
            workflow_links: Vec::new(),
        },
    );
}

fn seed_attributed_event(
    db: &Arc<Database>,
    session_id: &str,
    principal_kind: Option<&str>,
    principal_user_id: Option<&str>,
    oauth_client_id: Option<&str>,
) {
    record_action_event(
        db,
        ActionAuditEventInput {
            explicit_session_id: Some(session_id.to_string()),
            session_title: None,
            endpoint: "/api/runtime/status".to_string(),
            action_name: "getRuntimeStatus".to_string(),
            operation: Some("runtime_status".to_string()),
            project: Some("demo".to_string()),
            principal_kind: principal_kind.map(str::to_string),
            principal_user_id: principal_user_id.map(str::to_string),
            oauth_client_id: oauth_client_id.map(str::to_string),
            status: "success".to_string(),
            http_status: Some(200),
            started_at: 1,
            ended_at: 2,
            duration_ms: 10,
            error_summary: None,
            warning_summary: None,
            changed_files: Vec::new(),
            ids: json!({}),
            summary: json!({}),
            request_bytes: None,
            response_bytes: None,
            client_window_key: None,
            client_window_source: None,
            server_trace_id: None,
            principal_correlation_kind: None,
            principal_correlation_id: None,
            window_started_at_ms: None,
            window_ended_at_ms: None,
            request_observed_at_ms: None,
            response_handed_at_ms: None,
            window_transition_kind: None,
            response_streaming: None,
            window_continuity_eligible: None,
            window_meaningful: false,
            recorder_gap_session_id: None,
            workflow_links: Vec::new(),
        },
    );
}

// =========================================================================
// /api/audit/sessions
// =========================================================================

#[tokio::test]
async fn http_audit_sessions_requires_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let service = Service::new(build_audit_router(config, db));

    let resp = TestClient::post("http://localhost/api/audit/sessions")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_audit_sessions_rejects_wrong_bearer() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let service = Service::new(build_audit_router(config, db));

    let resp = TestClient::post("http://localhost/api/audit/sessions")
        .bearer_auth("wrong")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_audit_sessions_happy_path_returns_seeded_session() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    seed_event(
        &db,
        "sess-a",
        "/api/projects/list",
        "listProjects",
        "success",
        json!({"project_count": 2}),
    );
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/sessions")
        .bearer_auth("secret")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    let sessions = body["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], "sess-a");
    assert_eq!(sessions[0]["status"], "open");
    assert_eq!(sessions[0]["total_actions"], 1);
}

#[tokio::test]
async fn http_audit_sessions_limit_upper_cap_is_two_hundred() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    // Seed more sessions than the hard cap to prove the API never returns
    // more than 200.
    for i in 0..250 {
        seed_event(
            &db,
            &format!("cap-{}", i),
            "/api/projects/list",
            "listProjects",
            "success",
            json!({}),
        );
    }
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/sessions")
        .bearer_auth("secret")
        .json(&json!({ "limit": 10000 }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 200);
}

#[tokio::test]
async fn http_audit_sessions_limit_lower_bound_is_one() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    for i in 0..3 {
        seed_event(
            &db,
            &format!("low-{}", i),
            "/api/projects/list",
            "listProjects",
            "success",
            json!({}),
        );
    }
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/sessions")
        .bearer_auth("secret")
        .json(&json!({ "limit": 0 }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn http_audit_sessions_status_filter() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    seed_event(
        &db,
        "open-1",
        "/api/projects/list",
        "listProjects",
        "success",
        json!({}),
    );
    seed_event(
        &db,
        "closed-1",
        "/api/projects/list",
        "listProjects",
        "success",
        json!({}),
    );
    db.close_action_session("closed-1", 100).unwrap();
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/sessions")
        .bearer_auth("secret")
        .json(&json!({ "status": "closed" }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], "closed-1");
    assert_eq!(sessions[0]["status"], "closed");
}

// =========================================================================
// /api/audit/session
// =========================================================================

#[tokio::test]
async fn http_audit_session_requires_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let service = Service::new(build_audit_router(config, db));

    let resp = TestClient::post("http://localhost/api/audit/session")
        .json(&json!({ "session_id": "x" }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_audit_session_not_found() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let service = Service::new(build_audit_router(config, db));

    let resp = TestClient::post("http://localhost/api/audit/session")
        .bearer_auth("secret")
        .json(&json!({ "session_id": "missing" }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_audit_session_happy_path_returns_session_and_events() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    seed_event(
        &db,
        "sess-detail",
        "/api/projects/apply_unified_diff",
        "applyUnifiedDiff",
        "success",
        json!({"files_changed": 1}),
    );
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/session")
        .bearer_auth("secret")
        .json(&json!({ "session_id": "sess-detail" }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["session"]["session_id"], "sess-detail");
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["endpoint"], "/api/projects/apply_unified_diff");
    assert_eq!(events[0]["summary"]["files_changed"], 1);
}

// =========================================================================
// /api/audit/stats
// =========================================================================

#[tokio::test]
async fn http_audit_stats_requires_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let service = Service::new(build_audit_router(config, db));

    let resp = TestClient::post("http://localhost/api/audit/stats")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_audit_stats_happy_path_scoped_to_session() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    seed_event(
        &db,
        "stats-1",
        "/api/projects/run_job",
        "startProjectShellJob",
        "success",
        json!({}),
    );
    seed_event(
        &db,
        "stats-1",
        "/api/projects/apply_unified_diff",
        "applyUnifiedDiff",
        "failed",
        json!({}),
    );
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/stats")
        .bearer_auth("secret")
        .json(&json!({ "session_id": "stats-1" }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["by_endpoint"]["/api/projects/run_job"], 1);
    assert_eq!(body["by_endpoint"]["/api/projects/apply_unified_diff"], 1);
    assert_eq!(body["by_status"]["success"], 1);
    assert_eq!(body["by_status"]["failed"], 1);
    assert_eq!(body["job_count"], 1);
    assert_eq!(body["edit_count"], 1);
    assert_eq!(body["events_scanned"], 2);
    assert_eq!(body["events_available"], 2);
    assert_eq!(body["sessions_scanned"], 1);
    assert_eq!(body["truncated_sessions"], 0);
    assert_eq!(body["partial"], false);
    assert_eq!(body["truncated"], false);
}

#[tokio::test]
async fn http_audit_stats_global_over_recent_sessions() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    seed_event(
        &db,
        "g-1",
        "/api/projects/git_status",
        "getProjectGitStatus",
        "success",
        json!({}),
    );
    seed_event(
        &db,
        "g-2",
        "/api/runtime/status",
        "getRuntimeStatus",
        "success",
        json!({}),
    );
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/stats")
        .bearer_auth("secret")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["by_endpoint"]["/api/projects/git_status"], 1);
    assert_eq!(body["by_endpoint"]["/api/runtime/status"], 1);
    assert_eq!(body["git_count"], 1);
    assert_eq!(body["report_count"], 1);
    assert_eq!(body["events_scanned"], 2);
    assert_eq!(body["events_available"], 2);
    assert_eq!(body["sessions_scanned"], 2);
    assert_eq!(body["truncated_sessions"], 0);
    assert_eq!(body["partial"], false);
    assert_eq!(body["truncated"], false);
}

#[tokio::test]
async fn http_audit_stats_unknown_session_is_not_found() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let service = Service::new(build_audit_router(config, db));

    let resp = TestClient::post("http://localhost/api/audit/stats")
        .bearer_auth("secret")
        .json(&json!({ "session_id": "missing-stats-session" }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_audit_stats_reports_bounded_event_truncation() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    for _ in 0..201 {
        seed_event(
            &db,
            "stats-truncated",
            "/api/runtime/status",
            "getRuntimeStatus",
            "success",
            json!({}),
        );
    }
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/stats")
        .bearer_auth("secret")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["events_scanned"], 200);
    assert_eq!(body["events_available"], 201);
    assert_eq!(body["sessions_scanned"], 1);
    assert_eq!(body["truncated_sessions"], 1);
    assert_eq!(body["partial"], true);
    assert_eq!(body["truncated"], true);
}

#[tokio::test]
async fn http_audit_stats_aggregates_principal_and_oauth_client_usage() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let user = seed_user(&db, "audit-stats-alice");
    let (client, _secret) = seed_oauth_client_named(&db, &user, "ChatGPT");

    seed_attributed_event(&db, "stats-attribution", Some("user"), Some(&user.id), None);
    for _ in 0..2 {
        seed_attributed_event(
            &db,
            "stats-attribution",
            Some("oauth2"),
            Some(&user.id),
            Some(&client.client_id),
        );
    }
    seed_attributed_event(&db, "stats-attribution", None, None, None);
    db.revoke_oauth_client(&client.id, 10).unwrap();

    let service = Service::new(build_audit_router(config, db));
    let mut resp = TestClient::post("http://localhost/api/audit/stats")
        .bearer_auth("secret")
        .json(&json!({ "session_id": "stats-attribution" }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    let attribution = &body["attribution"];
    assert_eq!(attribution["attributed_count"], 3);
    assert_eq!(attribution["unattributed_count"], 1);
    assert_eq!(attribution["by_principal_kind"]["user"], 1);
    assert_eq!(attribution["by_principal_kind"]["oauth2"], 2);
    let clients = attribution["by_oauth_client"].as_array().unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0]["client_id"], client.client_id);
    assert_eq!(clients[0]["name"], "ChatGPT");
    assert_eq!(clients[0]["count"], 2);
    assert!(!body.to_string().contains(&user.id));
}

#[tokio::test]
async fn http_audit_session_keeps_principal_attribution_out_of_event_details() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    seed_attributed_event(
        &db,
        "private-attribution",
        Some("oauth2"),
        Some("user-private"),
        Some("wc_client_public"),
    );
    let service = Service::new(build_audit_router(config, db));

    let mut resp = TestClient::post("http://localhost/api/audit/session")
        .bearer_auth("secret")
        .json(&json!({ "session_id": "private-attribution" }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    let event = &body["events"][0];
    assert!(event.get("principal_kind").is_none());
    assert!(event.get("principal_user_id").is_none());
    assert!(event.get("oauth_client_id").is_none());
}

// =========================================================================
// Secret leakage
// =========================================================================

#[tokio::test]
async fn http_audit_responses_do_not_leak_secret_fields_or_values() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    // Seed a payload that contains secret-like keys and secret-like values
    // at write time. The write path already redacts, and the audit read
    // path must drop the keys entirely.
    seed_event(
        &db,
        "leak-1",
        "/api/projects/apply_unified_diff",
        "applyUnifiedDiff",
        "success",
        json!({
            "api_key": "sk-leak-12345",
            "password": "hunter2",
            "token": "bearer abc",
            "project_count": 7,
            "stdout": "should-be-stripped",
            "command_text": "echo hello"
        }),
    );
    let service = Service::new(build_audit_router(config, db.clone()));

    for (path, body) in [
        ("/api/audit/sessions", json!({ "limit": 10 })),
        ("/api/audit/session", json!({ "session_id": "leak-1" })),
        ("/api/audit/stats", json!({ "session_id": "leak-1" })),
    ] {
        let mut resp = TestClient::post(format!("http://localhost{}", path))
            .bearer_auth("secret")
            .json(&body)
            .send(&service)
            .await;
        assert_eq!(effective_status(&resp), StatusCode::OK, "path {}", path);
        let text = resp.take_string().await.unwrap();
        let lower = text.to_lowercase();
        for forbidden in ["sk-leak-12345", "hunter2", "should-be-stripped"] {
            assert!(
                !lower.contains(&forbidden.to_lowercase()),
                "path {} leaked value '{}': {}",
                path,
                forbidden,
                text
            );
        }
        for forbidden in ["api_key", "password", "token", "secret"] {
            assert!(
                !lower.contains(&forbidden.to_lowercase()),
                "path {} leaked secret field name '{}': {}",
                path,
                forbidden,
                text
            );
        }
    }

    // Legitimate non-secret fields still pass through.
    let mut resp = TestClient::post("http://localhost/api/audit/session")
        .bearer_auth("secret")
        .json(&json!({ "session_id": "leak-1" }))
        .send(&service)
        .await;
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["events"][0]["summary"]["project_count"], 7);
    // command_text is summarized into an object (hash + bounded preview),
    // never stored as the raw string.
    let cmd = &body["events"][0]["summary"]["command_text"];
    assert!(
        cmd.is_object(),
        "command_text must be summarized, got: {}",
        cmd
    );
    assert!(cmd["sha256"].is_string());
    // A non-secret one-liner keeps a short first-line preview by design.
    assert_eq!(cmd["first_line_preview"], "echo hello");

    // A secret-like command value is redacted in the preview and never
    // echoed verbatim.
    seed_event(
        &db,
        "leak-2",
        "/api/projects/apply_unified_diff",
        "applyUnifiedDiff",
        "success",
        json!({ "command_text": "token=cmd-secret-xyz" }),
    );
    let mut resp = TestClient::post("http://localhost/api/audit/session")
        .bearer_auth("secret")
        .json(&json!({ "session_id": "leak-2" }))
        .send(&service)
        .await;
    let body: Value = resp.take_json().await.unwrap();
    let cmd = &body["events"][0]["summary"]["command_text"];
    assert!(cmd.is_object());
    assert_eq!(cmd["first_line_preview"], "[redacted]");
    assert!(!body.to_string().contains("cmd-secret-xyz"));
}
