use salvo::http::StatusCode;
use salvo::test::{ResponseExt, TestClient};
use salvo::Service;
use serde_json::{json, Value};
use std::sync::Arc;

// =========================================================================
// getProjectGitStatus
// =========================================================================

#[tokio::test]
async fn http_projects_git_status_rejects_server_configured_project() {
    let config = super::test_config(Some("secret"));
    let (_tmp, db) = super::test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    // Initialize a real git repo so `git status --porcelain` succeeds.
    let root = tmp_proj.path();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(root)
        .output()
        .expect("git init");
    std::fs::write(root.join("tracked.txt"), "a").unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(root, "demo"));
    let service = Service::new(super::build_projects_router(config, db, runtime));

    let mut resp = TestClient::post("http://localhost/api/projects/git_status")
        .bearer_auth("secret")
        .json(&json!({"project": "demo"}))
        .send(&service)
        .await;
    assert_eq!(super::effective_status(&resp), StatusCode::BAD_REQUEST);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(body["error"].as_str().unwrap().contains("unknown_project"));
}

// =========================================================================
// Phase A read-only console REST wrappers (wiring + auth gate)
// =========================================================================

#[tokio::test]
async fn http_console_routes_require_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = super::test_config(Some("secret"));
    let (_tmp, db) = super::test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(tmp_proj.path(), "demo"));
    let service = Service::new(super::build_projects_router(config, db, runtime));

    for (path, body) in [("/api/projects/list_files", json!({"project": "demo"}))] {
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
async fn retired_project_compatibility_routes_are_unreachable() {
    let (_tmp, service) = super::phase2_service();
    for path in [
        "/api/projects/replace_in_file",
        "/api/projects/write_file",
        "/api/projects/apply_patch",
        "/api/projects/apply_patch_checked",
        "/api/projects/validate_patch",
        "/api/projects/read_file",
        "/api/projects/search_text",
        "/api/projects/git_diff",
        "/api/projects/git_diff_summary",
    ] {
        let resp = TestClient::post(format!("http://localhost{path}"))
            .bearer_auth("secret")
            .json(&json!({}))
            .send(&service)
            .await;
        assert_eq!(
            super::effective_status(&resp),
            StatusCode::NOT_FOUND,
            "{path} must not bypass canonical /api/tools/call dispatch"
        );
    }
}

#[tokio::test]
async fn http_console_routes_accept_correct_bearer_and_route_to_runtime() {
    // With a correct bearer token the routes reach the runtime. The
    // project id below is not agent-registered, so the runtime returns a
    // structured error (not a 401/404) — proving the request was
    // authenticated, deserialized, and dispatched to ToolRuntime.
    let config = super::test_config(Some("secret"));
    let (_tmp, db) = super::test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(tmp_proj.path(), "demo"));
    let service = Service::new(super::build_projects_router(config, db, runtime));

    let mut resp = TestClient::post("http://localhost/api/projects/list_files")
        .bearer_auth("secret")
        .json(&json!({"project": "agent:nope:nope"}))
        .send(&service)
        .await;
    // Authenticated and dispatched to ToolRuntime: a structured error
    // (BAD_REQUEST + success=false), not a 401/404.
    assert_eq!(super::effective_status(&resp), StatusCode::BAD_REQUEST);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "list_files should return a structured runtime error"
    );
}

// =========================================================================
// applyUnifiedDiff (POST /api/projects/apply_unified_diff)
// =========================================================================

#[tokio::test]
async fn http_projects_apply_unified_diff_dispatches_to_runtime() {
    // With a correct bearer token the route reaches the runtime. The project
    // id below is not agent-registered, so the runtime returns a structured
    // error rather than a 401/404, proving auth/deserialization/dispatch wiring.
    let config = super::test_config(Some("secret"));
    let (_tmp, db) = super::test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(tmp_proj.path(), "demo"));
    let service = Service::new(super::build_projects_router(config, db, runtime));

    let mut resp = TestClient::post("http://localhost/api/projects/apply_unified_diff")
        .bearer_auth("secret")
        .json(&json!({
            "project": "agent:nope:nope",
            "diff": "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1 +1,2 @@\nx\n+y\n"
        }))
        .send(&service)
        .await;
    assert_eq!(super::effective_status(&resp), StatusCode::BAD_REQUEST);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(body["error"].as_str().is_some_and(|e| !e.is_empty()));
}

// =========================================================================
// Dedicated mutation actions (apply_unified_diff, git_restore_paths,
// discard_untracked) — auth gate + dispatch wiring
// =========================================================================

#[tokio::test]
async fn http_phase3_mutation_actions_require_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let (_tmp, service) = super::phase2_service();
    for (path, body) in [
        (
            "/api/projects/apply_unified_diff",
            json!({"project": "demo", "diff": "diff"}),
        ),
        (
            "/api/projects/git_restore_paths",
            json!({"project": "demo", "paths": ["x.txt"]}),
        ),
        (
            "/api/projects/discard_untracked",
            json!({"project": "demo", "paths": ["x.txt"]}),
        ),
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
async fn http_phase3_mutation_actions_dispatch_to_runtime() {
    // With a correct bearer token the mutation routes reach the runtime.
    // The project id is not agent-registered, so the runtime returns a
    // structured error (not a 401/404) — proving the request was
    // authenticated, deserialized, and dispatched to ToolRuntime.
    let (_tmp, service) = super::phase2_service();
    for (path, body) in [
        (
            "/api/projects/apply_unified_diff",
            json!({"project": "agent:nope:nope", "diff": "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1 +1,2 @@\nx\n+y\n"}),
        ),
        (
            "/api/projects/git_restore_paths",
            json!({"project": "agent:nope:nope", "paths": ["x.txt"]}),
        ),
        (
            "/api/projects/discard_untracked",
            json!({"project": "agent:nope:nope", "paths": ["x.txt"]}),
        ),
    ] {
        let mut resp = TestClient::post(format!("http://localhost{}", path))
            .bearer_auth("secret")
            .json(&body)
            .send(&service)
            .await;
        assert_eq!(
            super::effective_status(&resp),
            StatusCode::BAD_REQUEST,
            "{} should reach runtime and return structured error",
            path
        );
        let body: Value = resp.take_json().await.unwrap();
        assert_eq!(body["success"], false);
        assert!(
            body["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{} should return a structured runtime error",
            path
        );
    }
}
