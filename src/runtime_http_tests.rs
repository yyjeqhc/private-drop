use super::*;
use crate::runner_http::RunnerRegistry;
use crate::test_support::{seed_oauth_client, seed_user, test_config, test_config_oauth2, test_db};
use salvo::test::{ResponseExt, TestClient};
use salvo::Service;
use std::time::Duration;

#[path = "runtime_http/tests/import_http_tests.rs"]
mod import_http_tests;
#[path = "runtime_http/tests/jobs_tests.rs"]
mod jobs_tests;
#[path = "runtime_http/tests/model_ergonomics_tests.rs"]
mod model_ergonomics_tests;
#[path = "runtime_http/tests/project_files_tests.rs"]
mod project_files_tests;
#[path = "runtime_http/tests/projects_tests.rs"]
mod projects_tests;
#[path = "runtime_http/tests/runner_config_tests.rs"]
mod runner_config_tests;

#[test]
fn computer_action_audit_projection_omits_sensitive_observation_payloads() {
    let targets_output = serde_json::json!({
        "targets": [{
            "client_id": "private-runner",
            "display_name": "Private Desktop",
            "connected": true,
            "capabilities": {
                "computer_observe": true,
                "computer_accessibility_observe": true
            }
        }],
        "count": 1,
        "total_count": 1,
        "truncated": false
    });
    let targets_audit = action_audit_output_for_tool("computer_list_targets", &targets_output);
    assert_eq!(
        targets_audit,
        serde_json::json!({"count": 1, "total_count": 1, "truncated": false})
    );
    let targets_serialized = serde_json::to_string(&targets_audit).unwrap();
    assert!(!targets_serialized.contains("private-runner"));
    assert!(!targets_serialized.contains("Private Desktop"));

    let list_output = serde_json::json!({
        "windows": [{
            "surface_id": "surface_secret",
            "application": "Private App",
            "title": "Confidential Window Title",
            "width": 1200,
            "height": 800,
            "focused": true,
            "active": true
        }],
        "count": 1,
        "truncated": false
    });
    let list_audit = action_audit_output_for_tool("computer_list_windows", &list_output);
    assert_eq!(
        list_audit,
        serde_json::json!({"count": 1, "truncated": false})
    );
    let list_serialized = serde_json::to_string(&list_audit).unwrap();
    assert!(!list_serialized.contains("Confidential"));
    assert!(!list_serialized.contains("Private App"));
    assert!(!list_serialized.contains("surface_secret"));

    let snapshot_output = serde_json::json!({
        "surface": {
            "surface_id": "surface_safe",
            "application": "Private App",
            "title": "Confidential Window Title",
            "width": 1200,
            "height": 800,
            "focused": null,
            "active": null
        },
        "width": 900,
        "height": 600,
        "mime_type": "image/jpeg",
        "file_bytes": 12345,
        "content_base64": "SUPER_SECRET_SCREENSHOT_BYTES"
    });
    let snapshot_audit = action_audit_output_for_tool("computer_snapshot", &snapshot_output);
    let snapshot_serialized = serde_json::to_string(&snapshot_audit).unwrap();
    assert_eq!(snapshot_audit["surface_id"], "surface_safe");
    assert_eq!(snapshot_audit["width"], 900);
    assert_eq!(snapshot_audit["height"], 600);
    assert_eq!(snapshot_audit["file_bytes"], 12345);
    assert!(!snapshot_serialized.contains("SUPER_SECRET"));
    assert!(!snapshot_serialized.contains("Confidential"));
    assert!(!snapshot_serialized.contains("Private App"));

    let text_output = serde_json::json!({
        "platform": "macos",
        "surface_id": "surface_safe",
        "element_id": "element_safe",
        "text_bytes": 12,
        "success": true,
        "text": "REST_AUDIT_SECRET",
        "value": "REST_AUDIT_SECRET"
    });
    let text_audit = action_audit_output_for_tool("computer_input_text", &text_output);
    let text_serialized = serde_json::to_string(&text_audit).unwrap();
    assert_eq!(text_audit["surface_id"], "surface_safe");
    assert_eq!(text_audit["element_id"], "element_safe");
    assert_eq!(text_audit["text_bytes"], 12);
    assert_eq!(text_audit["success"], true);
    assert!(!text_serialized.contains("REST_AUDIT_SECRET"));
}

fn seed_oauth_access_token_with_shared_key_hash(
    db: &crate::Database,
    client: &crate::models::OAuthClientRecord,
    user: &crate::models::UserRecord,
    scopes: &str,
    shared_key_hash: Option<&str>,
) -> String {
    let now = chrono::Utc::now().timestamp();
    let plaintext = crate::auth::generate_oauth_access_token();
    let (subject_kind, subject_id, user_id, shared_key_hash) = match shared_key_hash {
        Some(hash) => (
            "shared_key".to_string(),
            hash.to_string(),
            None,
            Some(hash.to_string()),
        ),
        None => (
            "managed_user".to_string(),
            user.id.clone(),
            Some(user.id.clone()),
            None,
        ),
    };
    let record = crate::models::OAuthAccessTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash: crate::auth::hash_token(&plaintext),
        client_id: client.client_id.clone(),
        subject_kind,
        subject_id,
        user_id,
        scopes: scopes.to_string(),
        resource: None,
        shared_key_hash,
        created_at: now,
        expires_at: now + 3600,
        revoked_at: None,
        last_used_at: None,
    };
    db.insert_oauth_access_token(&record).unwrap();
    plaintext
}

fn phase2_oauth_service_with_scopes(
    scopes: &[&str],
) -> (tempfile::TempDir, salvo::Service, Vec<String>) {
    let config = test_config_oauth2(Some("secret"));
    let (tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client = seed_oauth_client(&db, &user);
    let tokens = scopes
        .iter()
        .map(|scope| seed_oauth_access_token_with_shared_key_hash(&db, &client, &user, scope, None))
        .collect();
    let project_dir = tmp.path().join("project");
    std::fs::create_dir(&project_dir).unwrap();
    std::fs::write(project_dir.join("README.md"), "hello\n").unwrap();
    let runtime = Arc::new(runtime_with_local_project(&project_dir, "demo"));
    let service = Service::new(build_projects_router(config, db, runtime));
    (tmp, service, tokens)
}

fn phase2_oauth_service(scopes: &str) -> (tempfile::TempDir, salvo::Service, String) {
    let (tmp, service, mut tokens) = phase2_oauth_service_with_scopes(&[scopes]);
    (tmp, service, tokens.pop().unwrap())
}

fn phase2_oauth_service_with_shared_key_hash(
    scopes: &str,
    shared_key_hash: Option<&str>,
) -> (tempfile::TempDir, salvo::Service, String) {
    let config = test_config_oauth2(Some("secret"));
    let (tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client = seed_oauth_client(&db, &user);
    let token =
        seed_oauth_access_token_with_shared_key_hash(&db, &client, &user, scopes, shared_key_hash);
    let project_dir = tmp.path().join("project");
    std::fs::create_dir(&project_dir).unwrap();
    std::fs::write(project_dir.join("README.md"), "hello\n").unwrap();
    let runtime = Arc::new(runtime_with_local_project(&project_dir, "demo"));
    let service = Service::new(build_projects_router(config, db, runtime));
    (tmp, service, token)
}

/// Build a ToolRuntime without any server-side project configuration.
fn runtime_with_local_project(root: &std::path::Path, project_id: &str) -> ToolRuntime {
    let _ = (root, project_id);
    ToolRuntime::new(
        Arc::new(RunnerRegistry::default()),
        Arc::new(crate::tool_runtime::RuntimeInfo::default()),
    )
}

/// Build a router that mirrors the production /api wiring for the new
/// dedicated project actions: Config, Database, and ToolRuntime are
/// injected so AuthMiddleware and the handlers resolve state exactly as
/// in `main.rs`.
fn build_projects_router(
    config: Arc<crate::Config>,
    db: Arc<crate::Database>,
    runtime: Arc<ToolRuntime>,
) -> Router {
    Router::new()
        .hoop(affix_state::inject(config))
        .hoop(affix_state::inject(db))
        .hoop(affix_state::inject(runtime))
        .push(
            Router::with_path("api")
                .hoop(crate::AuthMiddleware)
                .push(Router::with_path("tools/list").post(tools_list))
                .push(Router::with_path("tools/call").post(tools_call))
                .push(
                    Router::with_path("artifacts/import")
                        .post(import_conversation_files_to_project),
                )
                .push(Router::with_path("projects/list").post(projects_list))
                .push(Router::with_path("projects/register").post(projects_register))
                .push(Router::with_path("projects/create").post(projects_create))
                .push(Router::with_path("projects/git_status").post(projects_git_status))
                .push(Router::with_path("projects/git_diff").post(projects_git_diff))
                .push(
                    Router::with_path("projects/apply_unified_diff")
                        .post(projects_apply_unified_diff),
                )
                .push(Router::with_path("projects/run_shell").post(projects_run_shell))
                .push(
                    Router::with_path("projects/git_restore_paths")
                        .post(projects_git_restore_paths),
                )
                .push(
                    Router::with_path("projects/discard_untracked")
                        .post(projects_discard_untracked),
                )
                .push(Router::with_path("projects/run_job").post(projects_run_job))
                .push(Router::with_path("projects/list_files").post(projects_list_files))
                .push(
                    Router::with_path("projects/git_diff_summary").post(projects_git_diff_summary),
                )
                .push(Router::with_path("jobs/list").post(jobs_list))
                .push(Router::with_path("jobs/stop").post(job_stop))
                .push(Router::with_path("jobs/tail").post(job_tail))
                .push(Router::with_path("runtime/status").post(runtime_status)),
        )
}

fn effective_status(resp: &Response) -> StatusCode {
    resp.status_code.unwrap_or(StatusCode::OK)
}

async fn register_import_agent_with_capabilities(
    root: &std::path::Path,
    capabilities: Option<crate::runner_protocol::RunnerCapabilities>,
) -> (Arc<ToolRuntime>, Arc<RunnerRegistry>) {
    use crate::runner_protocol::{RunnerProjectSummary, RunnerRegisterRequest};
    let registry = Arc::new(RunnerRegistry::default());
    registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: "importer".to_string(),
                runner_instance_id: "inst-import".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: capabilities.unwrap_or_default(),
                policy: None,
            },
        ))
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &registry,
        "importer",
        "inst-import",
        vec![RunnerProjectSummary {
            id: "demo".to_string(),
            name: Some("demo".to_string()),
            path: root.to_string_lossy().to_string(),
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
    let runtime = Arc::new(ToolRuntime::new_for_tests_with_runner_registry(
        registry.clone(),
    ));
    (runtime, registry)
}

async fn register_import_agent(root: &std::path::Path) -> (Arc<ToolRuntime>, Arc<RunnerRegistry>) {
    register_import_agent_with_capabilities(root, None).await
}

async fn complete_one_agent_request(
    registry: Arc<RunnerRegistry>,
    stdout: impl Into<String>,
    stderr: impl Into<String>,
    exit_code: i32,
) {
    use crate::runner_protocol::{RunnerPollRequest, RunnerResultRequest};
    let request = loop {
        if let Some(request) = registry
            .poll(RunnerPollRequest {
                client_id: "importer".to_string(),
                runner_instance_id: "inst-import".to_string(),
            })
            .await
            .unwrap()
        {
            break request;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    registry
        .complete(RunnerResultRequest {
            client_id: "importer".to_string(),
            runner_instance_id: "inst-import".to_string(),
            request_id: request.request_id,
            exit_code: Some(exit_code),
            stdout: Some(stdout.into()),
            stderr: Some(stderr.into()),
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
}

fn spawn_startup_agent_executor(registry: Arc<RunnerRegistry>) -> tokio::task::JoinHandle<()> {
    use crate::runner_protocol::{RunnerPollRequest, RunnerRequest, RunnerResultRequest};
    use std::path::Path;

    fn execute(request: &RunnerRequest) -> (i32, String, String) {
        if request.kind == "file_read" {
            return (1, String::new(), "No such file or directory".to_string());
        }
        #[cfg(windows)]
        let mut command = {
            let mut command = std::process::Command::new("powershell.exe");
            command
                .args(["-NoProfile", "-NonInteractive", "-Command"])
                .arg(&request.command);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(&request.command);
            command
        };
        let output = match command
            .current_dir(Path::new(request.cwd.as_deref().unwrap_or(".")))
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                return (
                    -1,
                    String::new(),
                    format!("failed to execute test agent shell: {error}"),
                );
            }
        };
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    tokio::spawn(async move {
        loop {
            if let Some(request) = registry
                .poll(RunnerPollRequest {
                    client_id: "importer".to_string(),
                    runner_instance_id: "inst-import".to_string(),
                })
                .await
                .unwrap()
            {
                let (exit_code, stdout, stderr) = execute(&request);
                registry
                    .complete(RunnerResultRequest {
                        client_id: "importer".to_string(),
                        runner_instance_id: "inst-import".to_string(),
                        request_id: request.request_id,
                        exit_code: Some(exit_code),
                        stdout: Some(stdout),
                        stderr: Some(stderr),
                        duration_ms: Some(1),
                        error: None,
                    })
                    .await
                    .unwrap();
            } else {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    })
}

// =========================================================================
// listProjects
// =========================================================================

#[tokio::test]
async fn all_project_endpoints_require_bearer_auth() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = Arc::new(runtime_with_local_project(tmp_proj.path(), "demo"));
    let service = Service::new(build_projects_router(config, db, runtime));

    let endpoints: Vec<(&str, Value)> = vec![
        ("/api/projects/list", json!({})),
        ("/api/projects/git_status", json!({"project": "demo"})),
        ("/api/projects/git_diff", json!({"project": "demo"})),
        (
            "/api/projects/apply_unified_diff",
            json!({"project": "demo", "diff": "diff"}),
        ),
        ("/api/tools/list", json!({})),
        ("/api/tools/call", json!({"tool": "list_tools"})),
        ("/api/runtime/status", json!({})),
        (
            "/api/projects/register",
            json!({"client_id": "oe", "id": "my-project", "name": "My Project", "path": "/root/git/my-project"}),
        ),
        (
            "/api/projects/create",
            json!({"client_id": "oe", "id": "hello", "name": "Hello", "path": "/root/git/hello"}),
        ),
    ];
    for (path, body) in &endpoints {
        let resp = TestClient::post(format!("http://localhost{path}"))
            .json(body)
            .send(&service)
            .await;
        assert_eq!(
            effective_status(&resp),
            StatusCode::UNAUTHORIZED,
            "{path} should require bearer auth"
        );
    }
}

// =========================================================================
// getRuntimeStatus / /api/runtime/status
// =========================================================================

#[tokio::test]
async fn http_runtime_status_rejects_wrong_bearer() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = Arc::new(runtime_with_local_project(tmp_proj.path(), "demo"));
    let service = Service::new(build_projects_router(config, db, runtime));

    let resp = TestClient::post("http://localhost/api/runtime/status")
        .bearer_auth("wrong")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_runtime_status_correct_bearer_returns_summary() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let (runtime, _registry) = register_import_agent(tmp_proj.path()).await;
    let service = Service::new(build_projects_router(config, db, runtime));

    let mut resp = TestClient::post("http://localhost/api/runtime/status")
        .bearer_auth("secret")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], true);
    let out = &body["output"];
    assert_eq!(out["service"], "webcodex");
    assert_eq!(out["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(out["projects"]["mode"], "agent_registered");
    assert!(out["projects"].get("configured").is_none());
    assert!(out["projects"].get("server_static").is_none());
    assert_eq!(out["projects"]["count"], 1);
    assert_eq!(out["projects"]["agent_registered"]["count"], 1);
    assert_eq!(out["projects"]["agent_registered"]["online_count"], 1);
    assert_eq!(out["projects"]["effective"]["count"], 1);
    assert_eq!(out["projects"]["effective"]["status"], "ok");
    assert!(out["agents"]["count"].is_i64());
    assert!(out["jobs"]["active_count"].is_i64());
    assert!(out["jobs"]["running_count"].is_i64());
    assert!(out["jobs"]["queued_count"].is_i64());
    assert_eq!(
        out["agents"]["clients"][0]["job_concurrency"],
        json!({"limit": null, "running": 0, "queued": 0})
    );
    assert!(out["tools"]["count"].is_i64());
    // No secrets in the HTTP response either.
    let serialized = serde_json::to_string(&body).unwrap();
    for forbidden in ["token", "api_key", "secret", "password"] {
        assert!(
            !serialized
                .to_lowercase()
                .contains(&forbidden.to_lowercase()),
            "runtime_status HTTP response must not contain '{}'",
            forbidden
        );
    }
}

#[tokio::test]
async fn http_runtime_status_optional_body_accepts_empty_and_rejects_malformed_json() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let (runtime, _registry) = register_import_agent(tmp_proj.path()).await;
    let service = Service::new(build_projects_router(config, db, runtime));

    let resp = TestClient::post("http://localhost/api/runtime/status")
        .bearer_auth("secret")
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);

    let mut resp = TestClient::post("http://localhost/api/runtime/status")
        .bearer_auth("secret")
        .add_header("content-type", "application/json", true)
        .body("{\"client_id\":")
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::BAD_REQUEST);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["status"], 400);
    assert!(body["error"]
        .as_str()
        .is_some_and(|error| error.contains("Invalid JSON")));
}

// =========================================================================
// Phase 2: callRuntimeTool / /api/tools/call generic entry point
// =========================================================================

fn phase2_service() -> (tempfile::TempDir, salvo::Service) {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = Arc::new(runtime_with_local_project(tmp_proj.path(), "demo"));
    let service = Service::new(build_projects_router(config, db, runtime));
    (_tmp, service)
}

async fn http_tool_call(service: &Service, body: Value) -> (StatusCode, Value) {
    let mut response = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&body)
        .send(service)
        .await;
    let status = effective_status(&response);
    let body = response.take_json::<Value>().await.unwrap();
    (status, body)
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

#[tokio::test]
async fn http_tools_call_full_trace_captures_raw_effective_and_final_payloads() {
    let trace_root = tempfile::tempdir().unwrap();
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
    env.set(
        "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
        trace_root.path().to_string_lossy().as_ref(),
    );
    env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");

    let (_tmp, service) = phase2_service();
    let request = json!({
        "tool": "list_tools",
        "params": {}
    });
    let (status, response) = http_tool_call(&service, request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["success"], true);
    crate::tool_request_trace::flush_full_trace_writer();

    let trace_dir = full_trace_dir_with_payload(trace_root.path(), "raw_request_body", &request);
    let events = std::fs::read_to_string(trace_dir.join("events.jsonl")).unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events
        .iter()
        .any(|event| event["event"] == "api_tool_request_received"));
    assert!(events
        .iter()
        .any(|event| event["event"] == "api_tool_handler_returned"));

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

    let raw = read_phase("raw_request_body");
    assert_eq!(raw, request);
    let effective = read_phase("effective_arguments");
    assert_eq!(effective, json!({}));
    let final_response = read_phase("final_response");
    assert_eq!(final_response["success"], true);
    assert_eq!(final_response["output"]["tools"].is_array(), true);
}

#[tokio::test]
async fn http_tools_call_full_trace_captures_pre_dispatch_error_response() {
    let trace_root = tempfile::tempdir().unwrap();
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
    env.set(
        "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
        trace_root.path().to_string_lossy().as_ref(),
    );
    env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");

    let (_tmp, service) = phase2_service();
    let request = json!({"params": {"project": "demo"}});
    let (status, response) = http_tool_call(&service, request.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["status"], StatusCode::BAD_REQUEST.as_u16());
    assert!(response["error"]
        .as_str()
        .is_some_and(|error| error.contains("tool")));
    crate::tool_request_trace::flush_full_trace_writer();

    let trace_dir = full_trace_dir_with_payload(trace_root.path(), "final_response", &response);
    let events = std::fs::read_to_string(trace_dir.join("events.jsonl")).unwrap();
    let final_payload_path = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| {
            event["event"] == "tool_trace_payload_captured" && event["phase"] == "final_response"
        })
        .and_then(|event| event["payload_path"].as_str().map(str::to_string))
        .expect("pre-dispatch final response trace payload");
    let compressed = std::fs::read(trace_dir.join(final_payload_path)).unwrap();
    let raw = zstd::stream::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), response);
}

#[tokio::test]
async fn flattened_tool_manifest_audit_intent_survives_null_params_wrapper() {
    let (tool, params) = extract_tool_call(&json!({
        "tool": "tool_manifest",
        "params": null,
        "intent": "audit",
    }))
    .unwrap();
    let call = ToolCall::from_tool_name(&tool, params).unwrap();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = runtime_with_local_project(tmp_proj.path(), "demo");

    let result = runtime.dispatch(call).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["intent"], "audit");
    assert_eq!(result.output["filtered"], true);
    assert!(
        result.output["returned_count"].as_u64().unwrap()
            < result.output["total_count"].as_u64().unwrap(),
        "audit intent must not silently degrade to the full manifest: {:?}",
        result.output
    );
}

#[tokio::test]
async fn flattened_tool_manifest_exact_name_survives_null_params_wrapper() {
    let (tool, params) = extract_tool_call(&json!({
        "tool": "tool_manifest",
        "params": null,
        "tool_name": "cargo_test",
        "include_recommended_flows": false,
        "include_risk_summary": false,
    }))
    .unwrap();
    let call = ToolCall::from_tool_name(&tool, params).unwrap();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = runtime_with_local_project(tmp_proj.path(), "demo");

    let result = runtime.dispatch(call).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["tool_name"], "cargo_test");
    assert_eq!(result.output["contract"]["name"], "cargo_test");
    assert_eq!(result.output["contract"]["input_schema"]["type"], "object");
    assert!(result.output["contract"].get("output_schema").is_none());
}

#[tokio::test]
async fn http_start_coding_task_flattened_legacy_params_do_not_revive_unknown_tool() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let (runtime, _registry) = register_import_agent(tmp_proj.path()).await;
    let service = Service::new(build_projects_router(config, db, runtime));

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "start_coding_task",
            "params": null,
            "project": "agent:importer:demo",
            "include_runtime_status": false,
            "include_git": false,
            "include_recent_commits": false,
            "include_rules": false,
            "include_tool_manifest": true,
            "tool_manifest_intent": "audit",
        }))
        .send(&service)
        .await;

    assert_eq!(effective_status(&resp), StatusCode::BAD_REQUEST);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["status"], 400);
    let error = body["error"].as_str().unwrap_or_default();
    assert!(error.contains("unknown tool 'start_coding_task'"), "{body}");
}

// =========================================================================
// Removed tool identity
// =========================================================================

#[tokio::test]
async fn http_start_coding_task_uses_ordinary_unknown_tool_path() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "start_coding_task",
            "params": {"project": "agent:importer:demo"}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::BAD_REQUEST);
    let body: Value = resp.take_json().await.unwrap();
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("unknown tool 'start_coding_task'"),
        "{error}"
    );
}

#[test]
fn extract_tool_call_params_precede_flattened_fields() {
    let (tool, params) = extract_tool_call(&json!({
        "tool": "git_status",
        "project": "wrong",
        "params": {"project": "right"},
    }))
    .unwrap();

    assert_eq!(tool, "git_status");
    assert_eq!(params, json!({"project": "right"}));
}

#[test]
fn extract_tool_call_plugin_tool_preserves_provider_local_tool_inside_params() {
    let body = json!({
        "tool": "plugin_tool",
        "params": {
            "action": "describe",
            "runner": "runner-a",
            "plugin": "repo-tools",
            "tool": "safe_delete"
        },
        TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_plugin_record"
    });
    let (tool, params) = extract_tool_call(&body).unwrap();
    assert_eq!(tool, "plugin_tool");
    assert_eq!(params["action"], "describe");
    assert_eq!(params["tool"], "safe_delete");
    let parsed = ToolCall::from_tool_name(&tool, params).unwrap();
    assert_eq!(parsed.tool_name(), "plugin_tool");
    assert!(matches!(parsed, ToolCall::PluginTool(_)));
    assert_eq!(
        extract_recording_session_id(&body),
        Some("wc_sess_plugin_record".to_string())
    );

    let (tool, params) = extract_tool_call(&json!({
        "tool": "plugin_tool",
        "params": {
            "action": "call",
            "binding": "wc_pbind_00000000000000000000000000000000",
            "arguments": {"path": "build/old.bin"}
        }
    }))
    .unwrap();
    assert!(matches!(
        ToolCall::from_tool_name(&tool, params).unwrap(),
        ToolCall::PluginTool(_)
    ));
}

#[test]
fn plugin_tool_api_trace_projection_hides_binding_and_raw_arguments() {
    let body = json!({
        "tool": "plugin_tool",
        "params": {
            "action": "call",
            "binding": "wc_pbind_0123456789abcdef0123456789abcdef",
            "arguments": {"path": "private/target.txt", "secret": "must-not-leak"}
        },
        TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_plugin_record"
    });
    let raw = tool_call_trace_raw_body(&body);
    let encoded = serde_json::to_string(&raw).unwrap();
    assert!(!encoded.contains("wc_pbind_"));
    assert!(!encoded.contains("private/target.txt"));
    assert!(!encoded.contains("must-not-leak"));
    assert_eq!(raw["tool"], "plugin_tool");
    assert_eq!(raw["arguments"]["binding_present"], true);
    assert_eq!(raw["arguments"]["arguments_present"], true);
    assert_eq!(raw["recording_session_id_present"], true);

    let effective = tool_call_trace_effective_arguments("plugin_tool", &body["params"]);
    let encoded = serde_json::to_string(&effective).unwrap();
    assert!(!encoded.contains("wc_pbind_"));
    assert!(!encoded.contains("private/target.txt"));
    assert!(!encoded.contains("must-not-leak"));
    assert_eq!(effective["binding_present"], true);
    assert_eq!(effective["arguments_present"], true);
}

#[test]
fn extract_tool_call_rejects_retired_arguments_envelope() {
    for arguments in [json!(null), json!({"project": "right"})] {
        let error = extract_tool_call(&json!({
            "tool": "git_status",
            "project": "flattened",
            "arguments": arguments,
        }))
        .unwrap_err();
        assert!(error.contains("arguments"));
        assert!(error.contains("no longer supported"));
        assert!(error.contains("params"));
    }
}

#[test]
fn extract_tool_call_collects_flattened_top_level_fields() {
    let (tool, params) = extract_tool_call(&json!({
        "tool": "git_status",
        "project": "agent:oe:webcodex",
        "session_id": "wc_sess_tool_arg",
        TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_recorder",
    }))
    .unwrap();

    assert_eq!(tool, "git_status");
    assert_eq!(
        params,
        json!({"project": "agent:oe:webcodex", "session_id": "wc_sess_tool_arg"})
    );
    assert_eq!(
        extract_recording_session_id(
            &json!({TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_recorder"})
        ),
        Some("wc_sess_recorder".to_string())
    );
}

#[test]
fn extract_tool_call_collects_flattened_session_handoff_flags() {
    let body = json!({
        "tool": "session_handoff_summary",
        "project": "agent:special:test-mcp",
        "session_id": "wc_sess_test",
        "include_validation": true,
        "include_workspace": true,
        "include_checkpoints": true,
        "limit": 20,
        TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_recorder"
    });
    let (tool, params) = extract_tool_call(&body).unwrap();

    assert_eq!(tool, "session_handoff_summary");
    assert_eq!(
        params,
        json!({
            "project": "agent:special:test-mcp",
            "session_id": "wc_sess_test",
            "include_validation": true,
            "include_workspace": true,
            "include_checkpoints": true,
            "limit": 20
        })
    );
    assert!(
        params
            .as_object()
            .is_some_and(|m| !m.contains_key(TOOL_CALL_RECORDING_SESSION_ID_FIELD)),
        "recording_session_id must not leak into concrete params"
    );
    assert_eq!(
        extract_recording_session_id(&body),
        Some("wc_sess_recorder".to_string())
    );
}

#[test]
fn extract_tool_call_collects_flattened_write_project_file_fields() {
    let (tool, params) = extract_tool_call(&json!({
        "tool": "write_project_file",
        "project": "agent:oe:webcodex",
        "path": "x.tmp",
        "content": "BETA\n",
        "overwrite": true,
    }))
    .unwrap();

    assert_eq!(tool, "write_project_file");
    assert_eq!(params["project"], "agent:oe:webcodex");
    assert_eq!(params["path"], "x.tmp");
    assert_eq!(params["content"], "BETA\n");
    assert_eq!(params["overwrite"], true);
}

#[test]
fn extract_tool_call_collects_flattened_checkpoint_restore_fields() {
    // GPT Action flattened call for workspace_checkpoint_restore: the
    // recorder metadata (recording_session_id) must be stripped from
    // params while the business fields (project/checkpoint_id/confirm)
    // are collected into params for concrete dispatch.
    let body = json!({
        "tool": "workspace_checkpoint_restore",
        "project": "agent:special:test",
        "checkpoint_id": "wc_ckpt_abc",
        "confirm": true,
        TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_record"
    });
    let (tool, params) = extract_tool_call(&body).unwrap();

    assert_eq!(tool, "workspace_checkpoint_restore");
    assert_eq!(params["project"], "agent:special:test");
    assert_eq!(params["checkpoint_id"], "wc_ckpt_abc");
    assert_eq!(params["confirm"], true);
    assert!(
        params
            .as_object()
            .is_some_and(|m| !m.contains_key(TOOL_CALL_RECORDING_SESSION_ID_FIELD)),
        "recording_session_id must not leak into concrete params"
    );
    assert_eq!(
        extract_recording_session_id(&body),
        Some("wc_sess_record".to_string()),
        "recording_session_id must remain available as wrapper recorder metadata"
    );
}

#[test]
fn extract_tool_call_collects_flattened_apply_text_edits_fields() {
    // GPT Action flattened call for apply_text_edits: nested `changes`
    // array and scalar flattened fields must be collected into params.
    let (tool, params) = extract_tool_call(&json!({
        "tool": "apply_text_edits",
        "project": "agent:special:test",
        "dry_run": true,
        "changes": [{
            "kind": "edit",
            "path": "a.txt",
            "expected_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "edits": [{"kind": "replace_exact", "old_text": "a", "new_text": "b"}]
        }]
    }))
    .unwrap();

    assert_eq!(tool, "apply_text_edits");
    assert_eq!(params["project"], "agent:special:test");
    assert_eq!(params["dry_run"], true);
    let changes = params["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["kind"], "edit");
    assert_eq!(changes[0]["path"], "a.txt");
    assert_eq!(changes[0]["edits"][0]["kind"], "replace_exact");
}

#[test]
fn extract_tool_call_no_argument_tool_keeps_null_params() {
    let (tool, params) = extract_tool_call(&json!({"tool": "list_tools"})).unwrap();

    assert_eq!(tool, "list_tools");
    assert!(params.is_null() || params.as_object().is_some_and(|m| m.is_empty()));
}

#[tokio::test]
async fn http_tools_list_returns_names_and_count() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/list")
        .bearer_auth("secret")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(
        body["tools"].is_array(),
        "tools array must remain for back-compat"
    );
    assert!(body["names"].is_array(), "names array must be present");
    let names = body["names"].as_array().unwrap();
    assert!(!names.is_empty(), "names must not be empty");
    assert!(names.iter().any(|n| n == "list_tools"));
    assert!(names.iter().any(|n| n == "git_diff_summary"));
    assert!(names.iter().any(|n| n == "git_log"));
    assert!(names.iter().any(|n| n == "show_changes"));
    assert_eq!(body["count"], names.len());
    for tool in body["tools"].as_array().unwrap() {
        assert!(tool["inputSchema"].is_object());
        assert!(tool["outputSchema"].is_object());
    }
    // Optional enrichment fields.
    assert!(body["categories"].is_object(), "categories must be present");
    assert!(
        body["recommended_flows"].is_array(),
        "recommended_flows must be present"
    );
    // names and tools must stay in sync.
    let tools_count = body["tools"].as_array().unwrap().len();
    assert_eq!(tools_count, names.len());
}

#[tokio::test]
async fn http_tools_list_supports_bounded_summary_request() {
    let (_tmp, service) = phase2_service();
    let mut full_resp = TestClient::post("http://localhost/api/tools/list")
        .bearer_auth("secret")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&full_resp), StatusCode::OK);
    let full_body: Value = full_resp.take_json().await.unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/list")
        .bearer_auth("secret")
        .json(&json!({
            "category": "artifact",
            "features": "artifact_upload",
            "summary_only": true,
            "limit": 10
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["category"], "artifact");
    assert_eq!(body["features"], "artifact_upload");
    assert_eq!(body["truncated"], false);
    assert_eq!(body["total_count"], full_body["total_count"]);
    let names = body["names"].as_array().unwrap();
    for tool in [
        "artifact_upload_begin",
        "artifact_upload_chunk",
        "artifact_upload_finish",
        "artifact_upload_abort",
    ] {
        assert!(names.iter().any(|name| name == tool), "missing {tool}");
    }
    for tool in body["tools"].as_array().unwrap() {
        assert!(tool.get("inputSchema").is_none(), "{tool:?}");
        assert!(tool.get("outputSchema").is_none(), "{tool:?}");
    }
    assert!(
        body.to_string().len() < full_body.to_string().len() / 2,
        "bounded response should be substantially smaller than full list"
    );
}

#[tokio::test]
async fn http_tools_call_rejects_malformed_and_unknown_request_matrix() {
    let (_tmp, service) = phase2_service();
    let cases = vec![
        (
            "unknown tool",
            json!({"tool": "definitely_not_a_tool"}),
            vec!["definitely_not_a_tool"],
            false,
        ),
        (
            "removed run_codex",
            json!({
                "tool": "run_codex",
                "params": {"project": "demo", "prompt": "summarize"}
            }),
            vec!["run_codex"],
            true,
        ),
        (
            "missing required project",
            json!({"tool": "run_shell", "params": {"command": "echo"}}),
            vec!["run_shell", "project"],
            false,
        ),
        (
            "wrong project type",
            json!({"tool": "run_shell", "params": {"project": 123, "command": "echo"}}),
            vec!["run_shell"],
            false,
        ),
        (
            "missing outer tool",
            json!({"params": {}}),
            vec!["tool"],
            false,
        ),
    ];

    for (label, request, expected_fragments, verify_no_job) in cases {
        let (status, body) = http_tool_call(&service, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {body}");
        let error = body["error"].as_str().unwrap_or("");
        for fragment in expected_fragments {
            assert!(
                error.contains(fragment),
                "{label}: error must contain {fragment:?}: {error}"
            );
        }
        if verify_no_job {
            let (status, jobs) =
                http_tool_call(&service, json!({"tool": "list_jobs", "params": {}})).await;
            assert_eq!(status, StatusCode::OK, "{jobs}");
            assert_eq!(jobs["success"], true);
            assert!(jobs["output"]["jobs"].as_array().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn http_tools_call_accepts_omitted_and_null_params_but_rejects_arguments_alias() {
    let (_tmp, service) = phase2_service();
    for request in [
        json!({"tool": "list_tools"}),
        json!({"tool": "list_tools", "params": null}),
    ] {
        let (status, body) = http_tool_call(&service, request.clone()).await;
        assert_eq!(status, StatusCode::OK, "request: {request}");
        assert_eq!(body["success"], true, "request: {request}");
        assert!(body["output"]["tools"].is_array(), "request: {request}");
    }

    let request = json!({"tool": "list_tools", "arguments": null});
    let (status, body) = http_tool_call(&service, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("arguments"));
    assert!(error.contains("no longer supported"));
}

#[tokio::test]
async fn start_session_returns_session_id() {
    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let (runtime, _registry) = register_import_agent(tmp_proj.path()).await;
    let service = Service::new(build_projects_router(config, db, runtime));
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "start_session",
            "project": "demo",
            "title": "implement show_changes follow-up"
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["output"]["success"], true);
    assert!(body["output"]["session_id"]
        .as_str()
        .is_some_and(|id| id.starts_with("wc_sess_")));
    assert_eq!(body["output"]["project"], "agent:importer:demo");
    assert_eq!(body["output"]["project_input"], "demo");
    assert_eq!(body["output"]["resolved_project"], "agent:importer:demo");
    assert_eq!(body["output"]["title"], "implement show_changes follow-up");
    assert!(body["output"]["created_at"].is_i64());
}

#[tokio::test]
async fn session_summary_empty_session() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session", "params": {"title": "empty"}}))
        .send(&service)
        .await;
    let start_body: Value = resp.take_json().await.unwrap();
    let session_id = start_body["output"]["session_id"].as_str().unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "session_summary",
            "session_id": session_id,
            "limit": 50
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["session_id"], session_id);
    assert_eq!(body["output"]["counts"]["tool_calls"], 0);
    assert_eq!(body["output"]["counts"]["succeeded"], 0);
    assert_eq!(body["output"]["counts"]["failed"], 0);
    assert!(body["output"]["events"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn start_session_read_only_summary_returns_guard_config() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "start_session",
            "params": {"title": "readonly", "mode": "read_only"}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let start_body: Value = resp.take_json().await.unwrap();
    let session_id = start_body["output"]["session_id"].as_str().unwrap();
    assert_eq!(start_body["output"]["mode"], "read_only");
    assert_eq!(start_body["output"]["guards"]["deny_write_tools"], true);
    assert_eq!(start_body["output"]["guards"]["deny_shell_tools"], true);

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "session_summary", "params": {"session_id": session_id}}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["session_id"], session_id);
    assert_eq!(body["output"]["mode"], "read_only");
    assert_eq!(body["output"]["guards"]["deny_write_tools"], true);
    assert_eq!(body["output"]["guards"]["deny_shell_tools"], true);
}

#[tokio::test]
async fn api_tools_call_records_success_event_with_session_id() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session", "params": {"title": "tracking"}}))
        .send(&service)
        .await;
    let start_body: Value = resp.take_json().await.unwrap();
    let session_id = start_body["output"]["session_id"].as_str().unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "list_projects",
            TOOL_CALL_RECORDING_SESSION_ID_FIELD: session_id,
            "params": {}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let _: Value = resp.take_json().await.unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "session_summary", "params": {"session_id": session_id}}))
        .send(&service)
        .await;
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["counts"]["tool_calls"], 1);
    assert_eq!(body["output"]["counts"]["succeeded"], 1);
    assert_eq!(body["output"]["counts"]["failed"], 0);
    let events = body["output"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], "tool_call_started");
    assert_eq!(events[1]["kind"], "tool_call_finished");
    assert_eq!(events[1]["transport"], "api");
    assert_eq!(events[1]["tool_name"], "list_projects");
    assert_eq!(events[1]["risk_class"], "read_only");
    assert_eq!(events[1]["status"], "succeeded");
    assert!(events[1]["duration_ms"].is_u64());
}

#[tokio::test]
async fn api_tools_call_accepts_hidden_testing_metadata_and_records_expectation() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session", "params": {"title": "tracking"}}))
        .send(&service)
        .await;
    let start_body: Value = resp.take_json().await.unwrap();
    let session_id = start_body["output"]["session_id"].as_str().unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "job_status",
            TOOL_CALL_RECORDING_SESSION_ID_FIELD: session_id,
            "job_id": "missing-job",
            "expected_failure": true,
            "expected_failure_kind": "job_not_found",
            "assertion_name": "api hidden metadata compatibility"
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::BAD_REQUEST);
    let _: Value = resp.take_json().await.unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "session_summary", "params": {"session_id": session_id}}))
        .send(&service)
        .await;
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["counts"]["tool_calls"], 1);
    assert_eq!(body["output"]["counts"]["failed"], 1);
    let event = &body["output"]["events"].as_array().unwrap()[1];
    assert_eq!(event["tool_name"], "job_status");
    assert_eq!(event["status"], "failed");
    assert_eq!(event["expected_failure"], true);
    assert_eq!(event["expected_failure_kind"], "job_not_found");
    assert_eq!(event["assertion_name"], "api hidden metadata compatibility");
    assert_eq!(event["actual_failure_kind"], "job_not_found");
    assert_eq!(
        event["failure_expectation_result"],
        "matched_expected_failure"
    );
}

#[tokio::test]
async fn api_tools_call_uses_recording_session_id_for_recorder_metadata() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session", "title": "tracking"}))
        .send(&service)
        .await;
    let tracking_body: Value = resp.take_json().await.unwrap();
    let tracking_session_id = tracking_body["output"]["session_id"].as_str().unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session", "title": "business"}))
        .send(&service)
        .await;
    let business_body: Value = resp.take_json().await.unwrap();
    let business_session_id = business_body["output"]["session_id"].as_str().unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "session_summary",
            "session_id": business_session_id,
            TOOL_CALL_RECORDING_SESSION_ID_FIELD: tracking_session_id
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["session_id"], business_session_id);
    assert_eq!(body["output"]["title"], "business");
    assert!(body["output"].get("session_recorded").is_none());
    assert!(body["output"].get("session_event_id").is_none());
    assert!(body["output"].get("session_context_revision").is_none());
    assert!(body["output"].get("session_continuity").is_none());
    assert!(body["output"].get("session_recovery").is_none());

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "session_summary",
            "session_id": tracking_session_id
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let tracking_summary: Value = resp.take_json().await.unwrap();
    assert_eq!(
        tracking_summary["output"]["events"][0]["tool_name"],
        "session_summary"
    );
    assert_eq!(
        tracking_summary["output"]["events"][0]["input_summary"]["session_id"],
        business_session_id
    );
    let finished = tracking_summary["output"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "tool_call_finished")
        .expect("recorded REST model-facing result");
    assert_eq!(finished["context_revision"], 1);
}

#[tokio::test]
async fn api_tools_call_message_tool_keeps_business_session_id_with_recording_session_id() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session", "title": "tracking"}))
        .send(&service)
        .await;
    let tracking_body: Value = resp.take_json().await.unwrap();
    let tracking_session_id = tracking_body["output"]["session_id"].as_str().unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session", "title": "business"}))
        .send(&service)
        .await;
    let business_body: Value = resp.take_json().await.unwrap();
    let business_session_id = business_body["output"]["session_id"].as_str().unwrap();

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "post_session_message",
            "session_id": business_session_id,
            TOOL_CALL_RECORDING_SESSION_ID_FIELD: tracking_session_id,
            "kind": "guidance",
            "message": "Keep this behind callRuntimeTool.",
            "tags": ["openapi", "constraint"],
            "priority": "normal"
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["session_id"], business_session_id);
    assert!(body["output"]["message_id"]
        .as_str()
        .is_some_and(|id| id.starts_with("wc_msg_")));
    assert!(body["output"].get("session_recorded").is_none());
    assert!(body["output"].get("session_event_id").is_none());

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "list_session_messages",
            "session_id": business_session_id,
            "kind": "guidance"
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let business_messages: Value = resp.take_json().await.unwrap();
    assert_eq!(
        business_messages["output"]["session_id"],
        business_session_id
    );
    assert_eq!(
        business_messages["output"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "session_summary",
            "session_id": tracking_session_id
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let tracking_summary: Value = resp.take_json().await.unwrap();
    assert_eq!(
        tracking_summary["output"]["events"][0]["tool_name"],
        "post_session_message"
    );
    assert_eq!(
        tracking_summary["output"]["events"][0]["input_summary"]["session_id"],
        business_session_id
    );
}

#[tokio::test]
async fn read_only_session_allows_post_session_message_metadata() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "start_session",
            "title": "readonly message board",
            "mode": "read_only"
        }))
        .send(&service)
        .await;
    let start_body: Value = resp.take_json().await.unwrap();
    let session_id = start_body["output"]["session_id"].as_str().unwrap();
    assert_eq!(start_body["output"]["mode"], "read_only");

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "post_session_message",
            "session_id": session_id,
            "kind": "progress",
            "message": "Read-only sessions may still record collaboration metadata."
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["message"]["kind"], "progress");
    assert!(body["output"].get("changed_paths").is_none());

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "session_summary", "session_id": session_id}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let summary: Value = resp.take_json().await.unwrap();
    assert_eq!(summary["output"]["messages"]["total"], 1);
    assert_eq!(summary["output"]["counts"]["tool_calls"], 0);
}

#[tokio::test]
async fn session_summary_bounds_event_limit() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({"tool": "start_session"}))
        .send(&service)
        .await;
    let start_body: Value = resp.take_json().await.unwrap();
    let session_id = start_body["output"]["session_id"].as_str().unwrap();

    for _ in 0..3 {
        let mut resp = TestClient::post("http://localhost/api/tools/call")
            .bearer_auth("secret")
            .json(&json!({
                "tool": "list_projects",
                TOOL_CALL_RECORDING_SESSION_ID_FIELD: session_id,
                "params": {}
            }))
            .send(&service)
            .await;
        let _: Value = resp.take_json().await.unwrap();
    }

    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "session_summary",
            "params": {"session_id": session_id, "limit": 1}
        }))
        .send(&service)
        .await;
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["counts"]["tool_calls"], 3);
    let events = body["output"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "tool_call_finished");
}

#[tokio::test]
async fn http_tools_call_rejects_arguments_even_when_params_are_present() {
    let (_tmp, service) = phase2_service();
    let (status, body) = http_tool_call(
        &service,
        json!({
            "tool": "git_diff_summary",
            "params": {"project": "agent:canonical:p"},
            "arguments": {"project": "agent:retired:p"},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("arguments"), "{error}");
    assert!(error.contains("no longer supported"), "{error}");
    assert!(!error.contains("agent:canonical:p"), "{error}");
    assert!(!error.contains("agent:retired:p"), "{error}");
}

#[tokio::test]
async fn http_tools_call_generic_path_dispatches_representative_project_tools() {
    // One read-side and one write-side tool are sufficient to prove the generic
    // extraction -> ToolCall -> ToolRuntime -> HTTP ToolResult path.
    let (_tmp, service) = phase2_service();
    for (tool, params) in [
        ("git_diff_summary", json!({"project": "agent:nope:nope"})),
        (
            "write_project_file",
            json!({"project": "agent:nope:nope", "path": "x.txt", "content": "a"}),
        ),
    ] {
        let (status, body) =
            http_tool_call(&service, json!({"tool": tool, "params": params})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{tool}: {body}");
        assert_eq!(body["success"], false, "{tool}: {body}");
        assert!(body["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()));
    }
}

#[tokio::test]
async fn api_show_changes_with_session_id() {
    use crate::runner_protocol::{RunnerCapabilities, RunnerPollRequest, RunnerResultRequest};

    let config = test_config(Some("secret"));
    let (_tmp, db) = test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let (runtime, registry) = register_import_agent_with_capabilities(
        tmp_proj.path(),
        Some(RunnerCapabilities {
            shell: true,
            git: true,
            internal_posix_script: true,
            ..Default::default()
        }),
    )
    .await;
    let service = Service::new(build_projects_router(config, db, runtime));
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth("secret")
        .json(&json!({
            "tool": "start_session",
            "params": {"project": "agent:importer:demo", "title": "api show changes"}
        }))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let start_body: Value = resp.take_json().await.unwrap();
    let session_id = start_body["output"]["session_id"].as_str().unwrap();

    let request = async {
        TestClient::post("http://localhost/api/tools/call")
            .bearer_auth("secret")
            .json(&json!({
                "tool": "show_changes",
                "params": {
                    "project": "agent:importer:demo",
                    "session_id": session_id,
                    "include_diff": false,
                    "session_event_limit": 10
                }
            }))
            .send(&service)
            .await
    };
    let complete = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let req = loop {
            let req = registry
                .poll(RunnerPollRequest {
                    client_id: "importer".to_string(),
                    runner_instance_id: "inst-import".to_string(),
                })
                .await
                .unwrap();
            if let Some(req) = req {
                break req;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "show_changes did not enqueue an agent request within 10 seconds"
            );
            tokio::task::yield_now().await;
        };
        let stdout = format!(
            "{}{}{}",
            crate::tool_runtime::framed_show_changes_test_block(
                'S',
                "## main\n?? README.md\n",
                "status_exit=0\nrepository_probe=inside_worktree\nrepository_probe_exit=0\nfiles_total=1\nfiles_returned=1\nfiles_truncated=0\nfiles_limit=200\nmodified=0\nadded=0\ndeleted=0\nrenamed=0\ncopied=0\nuntracked=1\nconflicted=0\nstaged=0\nunstaged=0\nstatus_trunc_count=0\nstatus_trunc_bytes=0\nstatus_trunc_path=0\nstatus_bytes=20\n"
            ),
            crate::tool_runtime::framed_show_changes_test_block(
                'H',
                "commit=abc123\nshort=abc123\nsummary=test head\n",
                "head_exit=0\nhead_truncated=0\nhead_bytes=44\n"
            ),
            crate::tool_runtime::framed_show_changes_test_block(
                'T',
                "",
                "diff_stat_exit=0\ndiff_stat_truncated=0\ndiff_stat_bytes=0\n"
            )
        );
        registry
            .complete(RunnerResultRequest {
                client_id: "importer".to_string(),
                runner_instance_id: "inst-import".to_string(),
                request_id: req.request_id,
                exit_code: Some(0),
                stdout: Some(stdout),
                stderr: Some(String::new()),
                duration_ms: Some(1),
                error: None,
            })
            .await
            .unwrap();
    };
    let (mut resp, _) = tokio::join!(request, complete);
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["output"]["project"], "agent:importer:demo");
    assert_eq!(body["output"]["session"]["found"], true);
    assert_eq!(body["output"]["session"]["session_id"], session_id);
    assert_eq!(body["output"]["session"]["title"], "api show changes");
}

async fn oauth_tools_call(
    service: &Service,
    token: &str,
    tool: &str,
    params: Value,
) -> (StatusCode, Value, Option<String>) {
    let mut resp = TestClient::post("http://localhost/api/tools/call")
        .bearer_auth(token)
        .json(&json!({"tool": tool, "params": params}))
        .send(service)
        .await;
    let status = effective_status(&resp);
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.take_json::<Value>().await.unwrap();
    (status, body, challenge)
}

fn assert_oauth_scope_rejected(
    status: StatusCode,
    body: &Value,
    challenge: Option<&str>,
    scope: Option<&str>,
) {
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {:?}", body);
    assert_eq!(body["error"], "insufficient_scope");
    let challenge = challenge.unwrap_or("");
    assert!(
        challenge.contains("error=\"insufficient_scope\""),
        "challenge: {}",
        challenge
    );
    if let Some(scope) = scope {
        assert!(
            body["error_description"]
                .as_str()
                .unwrap_or("")
                .contains(scope),
            "body: {:?}",
            body
        );
        assert!(challenge.contains(scope), "challenge: {}", challenge);
    }
}

#[tokio::test]
async fn oauth2_tools_call_scope_matrix() {
    let (_tmp, service, tokens) = phase2_oauth_service_with_scopes(&[
        crate::auth::SCOPE_RUNTIME_READ,
        crate::auth::SCOPE_PROJECT_READ,
        crate::auth::SCOPE_PROJECT_WRITE,
        crate::auth::SCOPE_JOB_RUN,
    ]);
    let runtime_read = &tokens[0];
    let project_read = &tokens[1];
    let project_write = &tokens[2];
    let job_run = &tokens[3];

    let cases = [
        (
            "list_tools",
            Value::Null,
            runtime_read,
            project_read,
            crate::auth::SCOPE_RUNTIME_READ,
        ),
        (
            "runtime_status",
            Value::Null,
            runtime_read,
            project_read,
            crate::auth::SCOPE_RUNTIME_READ,
        ),
        (
            "read_files",
            json!({"project": "demo", "items": [{"path": "README.md"}]}),
            project_read,
            runtime_read,
            crate::auth::SCOPE_PROJECT_READ,
        ),
        (
            "show_changes",
            json!({"project": "agent:nope:nope", "session_id": "wc_sess_missing"}),
            project_read,
            runtime_read,
            crate::auth::SCOPE_PROJECT_READ,
        ),
        (
            "write_project_file",
            json!({"project": "demo", "path": "README.md", "content": "new"}),
            project_write,
            project_read,
            crate::auth::SCOPE_PROJECT_WRITE,
        ),
        (
            "run_shell",
            json!({"project": "demo", "command": "echo hi"}),
            job_run,
            project_read,
            crate::auth::SCOPE_JOB_RUN,
        ),
        (
            "run_job",
            json!({"project": "demo", "command": "echo hi"}),
            job_run,
            project_read,
            crate::auth::SCOPE_JOB_RUN,
        ),
    ];

    for (tool, params, allowed_token, denied_token, required_scope) in cases {
        let (status, body, _) =
            oauth_tools_call(&service, allowed_token, tool, params.clone()).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{tool}: {body}");
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{tool}: {body}");

        let (status, body, challenge) =
            oauth_tools_call(&service, denied_token, tool, params).await;
        assert_oauth_scope_rejected(status, &body, challenge.as_deref(), Some(required_scope));
    }
}

#[tokio::test]
async fn session_tools_oauth_scope_policy() {
    let (_tmp, service, tokens) = phase2_oauth_service_with_scopes(&[
        crate::auth::SCOPE_RUNTIME_READ,
        crate::auth::SCOPE_SESSION_COLLABORATE,
        crate::auth::SCOPE_PROJECT_READ,
    ]);
    let runtime_read = &tokens[0];
    let session_collaborate = &tokens[1];
    let project_read = &tokens[2];

    let (status, body, _) = oauth_tools_call(
        &service,
        runtime_read,
        "start_session",
        json!({"title": "oauth"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let session_id = body["output"]["session_id"].as_str().unwrap();

    let (status, body, _) = oauth_tools_call(
        &service,
        runtime_read,
        "session_summary",
        json!({"session_id": session_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "session_summary: {body}");
    let (status, body, _) = oauth_tools_call(
        &service,
        session_collaborate,
        "post_session_message",
        json!({"session_id": session_id, "kind": "todo", "message": "oauth assignment"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "post_session_message: {body}");
    let todo_id = body["output"]["message_id"].as_str().unwrap();

    let (status, body, _) = oauth_tools_call(
        &service,
        runtime_read,
        "get_session_assignment",
        json!({"session_id": session_id, "message_id": todo_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get_session_assignment: {body}");
    assert!(body["output"]["assignment_fence"]
        .as_str()
        .is_some_and(|fence| fence.starts_with("wsa1_")));

    let (status, body, challenge) = oauth_tools_call(
        &service,
        project_read,
        "get_session_assignment",
        json!({"session_id": session_id, "message_id": todo_id}),
    )
    .await;
    assert_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_RUNTIME_READ),
    );

    let (status, body, challenge) = oauth_tools_call(
        &service,
        runtime_read,
        "complete_session_message",
        json!({
            "session_id": session_id,
            "message_id": todo_id,
            "answer": "must be scope denied before mutation",
            "completion_key": "oauth-e3"
        }),
    )
    .await;
    assert_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_SESSION_COLLABORATE),
    );

    let (status, body, challenge) =
        oauth_tools_call(&service, project_read, "start_session", json!({})).await;
    assert_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_RUNTIME_READ),
    );
    let (status, body, challenge) = oauth_tools_call(
        &service,
        runtime_read,
        "post_session_message",
        json!({"session_id": "wc_sess_missing", "kind": "note", "message": "denied"}),
    )
    .await;
    assert_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_SESSION_COLLABORATE),
    );
}

#[tokio::test]
async fn bridge_oauth2_tools_call_still_requires_project_read_and_job_run_scopes() {
    let (_tmp, service, token) =
        phase2_oauth_service_with_shared_key_hash("runtime:read", Some("hash-a"));
    let (status, body, challenge) = oauth_tools_call(
        &service,
        &token,
        "read_files",
        json!({"project": "demo", "items": [{"path": "README.md"}]}),
    )
    .await;
    assert_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_PROJECT_READ),
    );

    let (_tmp, service, token) =
        phase2_oauth_service_with_shared_key_hash("project:read", Some("hash-a"));
    let (status, body, challenge) = oauth_tools_call(
        &service,
        &token,
        "run_job",
        json!({"project": "demo", "command": "echo hi"}),
    )
    .await;
    assert_oauth_scope_rejected(
        status,
        &body,
        challenge.as_deref(),
        Some(crate::auth::SCOPE_JOB_RUN),
    );
}

#[tokio::test]
async fn oauth2_tools_call_unknown_tool_fails_closed() {
    let (_tmp, service, token) = phase2_oauth_service("runtime:read project:read");
    let (status, body, challenge) =
        oauth_tools_call(&service, &token, "definitely_not_a_tool", Value::Null).await;
    assert_oauth_scope_rejected(status, &body, challenge.as_deref(), None);
}

#[tokio::test]
async fn http_tools_list_includes_phase4_edit_tools() {
    let (_tmp, service) = phase2_service();
    let mut resp = TestClient::post("http://localhost/api/tools/list")
        .bearer_auth("secret")
        .json(&json!({}))
        .send(&service)
        .await;
    assert_eq!(effective_status(&resp), StatusCode::OK);
    let body: Value = resp.take_json().await.unwrap();
    let names = body["names"].as_array().unwrap();
    // `replace_in_file` was removed and `start_session` is ModelHidden; removed
    // Workflow current-session controls must likewise stay absent. Only the
    // visible canonical tools appear here.
    assert!(!names.iter().any(|n| n == "replace_in_file"));
    assert!(!names.iter().any(|n| n == "start_session"));
    for removed in [
        "bind_current_session",
        "current_session",
        "unbind_current_session",
    ] {
        assert!(!names.iter().any(|n| n == removed));
    }
    assert!(names.iter().any(|n| n == "write_project_file"));
    assert_eq!(body["count"], names.len());
    let tools = body["tools"].as_array().unwrap();
    for name in ["read_files", "run_shell", "write_project_file"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        assert!(
            tool["inputSchema"]["properties"]
                .get("session_id")
                .is_some(),
            "tools/list schema missing session_id for {name}"
        );
        assert!(
            !tool["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "session_id"),
            "session_id must be optional for {name}"
        );
    }
}
