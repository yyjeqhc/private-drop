use super::*;
use crate::connector_runtime::{ConnectorContext, ConnectorRuntime, ConnectorRuntimeSlot};
use crate::runner_http::RunnerRegistry;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerJobUpdateRequest, RunnerPollRequest, RunnerProjectSummary,
    RunnerRegisterRequest, RunnerRequest, RunnerResultRequest, ShellJobValidationProgress,
};
use crate::tool_runtime::ToolRuntime;
use salvo::prelude::{affix_state, handler, Depot, Request, Router, Service, StatusCode};
use salvo::test::{ResponseExt, TestClient};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn git(args: &[&str], cwd: &Path) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repo(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(name);
    let state = temp.path().join("state");
    fs::create_dir(&root).unwrap();
    git(&["init", "-q"], &root);
    git(&["config", "core.autocrlf", "false"], &root);
    git(&["config", "core.longpaths", "true"], &root);
    fs::write(root.join("README.md"), "fixture\n").unwrap();
    git(&["add", "README.md"], &root);
    git(
        &[
            "-c",
            "user.name=WebCodex Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ],
        &root,
    );
    (temp, root, state)
}

fn options(root: PathBuf, state: PathBuf) -> ProjectCommandOptions {
    ProjectCommandOptions {
        root,
        profile: "personal".to_string(),
        state_dir: Some(state),
        json: false,
        console_assets_dir: None,
    }
}

#[test]
fn connector_project_registry_environment_clears_legacy_alias() {
    let mut command = tokio::process::Command::new("unused-test-command");
    command.env(LEGACY_CONNECTOR_PROJECTS_DIR_ENV, "/legacy/projects.d");

    configure_connector_project_registry_environment(
        &mut command,
        Path::new("/current/project-registry"),
    );

    let env = command
        .as_std()
        .get_envs()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        env.get(CONNECTOR_PROJECT_REGISTRY_DIR_ENV),
        Some(&Some("/current/project-registry".to_string()))
    );
    assert_eq!(env.get(LEGACY_CONNECTOR_PROJECTS_DIR_ENV), Some(&None));
}

fn write_console_assets(directory: &Path) {
    fs::create_dir_all(directory).unwrap();
    fs::write(directory.join("console.html"), "<html></html>\n").unwrap();
    fs::write(directory.join("app.js"), "globalThis.consoleDev = true;\n").unwrap();
    fs::write(directory.join("styles.css"), "body { color: black; }\n").unwrap();
}

#[test]
fn console_assets_are_validated_and_passed_only_to_the_serve_child() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("assets");
    write_console_assets(&directory);
    let mut start_options = options(
        temp.path().join("project"),
        temp.path().join("project-state"),
    );
    start_options.console_assets_dir = Some(directory.clone());

    let canonical = resolve_console_assets_directory(&start_options)
        .unwrap()
        .unwrap();
    assert_eq!(canonical, fs::canonicalize(&directory).unwrap());

    let mut command = tokio::process::Command::new("webcodex");
    configure_console_assets_environment(&mut command, Some(&canonical));
    let configured = command
        .as_std()
        .get_envs()
        .find(|(key, _)| *key == crate::console_web::CONSOLE_ASSETS_DIR_ENV)
        .and_then(|(_, value)| value)
        .map(PathBuf::from);
    assert_eq!(configured, Some(canonical));

    let mut embedded_command = tokio::process::Command::new("webcodex");
    configure_console_assets_environment(&mut embedded_command, None);
    assert!(embedded_command
        .as_std()
        .get_envs()
        .any(|(key, value)| key == crate::console_web::CONSOLE_ASSETS_DIR_ENV && value.is_none()));

    fs::remove_file(directory.join("styles.css")).unwrap();
    let error = resolve_console_assets_directory(&start_options).unwrap_err();
    assert_eq!(error.code, "console_assets_invalid");
    assert!(error.message.contains("styles.css"));
}

#[test]
fn npm_wrapper_network_credentials_are_removed_from_runtime_children() {
    let mut command = tokio::process::Command::new("webcodex-runner");
    for key in NPM_WRAPPER_NETWORK_ENV_KEYS {
        command.env(key, "credential-like-value");
    }
    command.env("WEBCODEX_TEST_UNRELATED_ENV", "preserved");

    remove_npm_wrapper_network_environment(&mut command);
    let envs: Vec<_> = command.as_std().get_envs().collect();
    for key in NPM_WRAPPER_NETWORK_ENV_KEYS {
        assert!(
            envs.iter()
                .any(|(candidate, value)| { candidate.to_str() == Some(key) && value.is_none() }),
            "runtime child did not remove wrapper-only environment key {key}"
        );
    }
    assert!(envs.iter().any(|(key, value)| {
        key.to_str() == Some("WEBCODEX_TEST_UNRELATED_ENV")
            && value.and_then(|value| value.to_str()) == Some("preserved")
    }));
}

#[test]
fn runner_parent_credentials_are_removed_before_spawn() {
    let mut command = tokio::process::Command::new("webcodex-runner");
    for key in ["WEBCODEX_TOKEN", "WEBCODEX_PAT", "WEBCODEX_AGENT_TOKEN"] {
        command.env(key, "credential-like-value");
    }
    command.env("WEBCODEX_TEST_UNRELATED_ENV", "preserved");

    remove_runner_parent_credentials(&mut command);
    let envs: Vec<_> = command.as_std().get_envs().collect();
    for key in ["WEBCODEX_TOKEN", "WEBCODEX_PAT", "WEBCODEX_AGENT_TOKEN"] {
        assert!(
            envs.iter()
                .any(|(candidate, value)| { candidate.to_str() == Some(key) && value.is_none() }),
            "Runner parent credential was not removed before spawn: {key}"
        );
    }
    assert!(envs.iter().any(|(key, value)| {
        key.to_str() == Some("WEBCODEX_TEST_UNRELATED_ENV")
            && value.and_then(|value| value.to_str()) == Some("preserved")
    }));
}

fn fact<'a>(readiness: &'a ProjectReadiness, code: &str) -> &'a ReadinessFact {
    readiness
        .findings
        .iter()
        .find(|finding| finding.code == code)
        .unwrap_or_else(|| panic!("missing readiness fact {code}: {readiness:?}"))
}

struct AuthenticatedProjectFixture {
    _temp: tempfile::TempDir,
    service: Service,
    db: Arc<crate::Database>,
    registry: Arc<RunnerRegistry>,
    connector: Arc<ConnectorRuntime>,
    agent_auth: crate::auth::AuthContext,
    credential: String,
    agent_token: String,
    bootstrap: String,
    client_id: String,
    root: PathBuf,
    state: PathBuf,
    recorded_requests: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct AuthenticatedRequestRecorder(Arc<Mutex<Vec<String>>>);

#[handler]
async fn record_authenticated_connector_request(req: &mut Request, depot: &mut Depot) {
    if let Ok(recorder) = depot.obtain::<AuthenticatedRequestRecorder>() {
        recorder
            .0
            .lock()
            .unwrap()
            .push(req.uri().path().to_string());
    }
}

async fn authenticated_project_fixture() -> AuthenticatedProjectFixture {
    authenticated_project_fixture_for("rust").await
}

async fn authenticated_project_fixture_for(recipe: &str) -> AuthenticatedProjectFixture {
    let (temp, root, state) = repo("authenticated");
    match recipe {
        "rust" => fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='golden-fixture'\nversion='0.1.0'\n",
        )
        .unwrap(),
        "node" => {
            fs::write(
                root.join("package.json"),
                r#"{"packageManager":"npm@10","scripts":{"check":"eslint ."}}"#,
            )
            .unwrap();
            fs::write(root.join("package-lock.json"), "{}").unwrap();
        }
        "python" => {
            fs::write(root.join("pyproject.toml"), "[tool.ruff]\nline-length=88\n").unwrap()
        }
        "go" => fs::write(
            root.join("go.mod"),
            "module example.test/golden\n\ngo 1.22\n",
        )
        .unwrap(),
        _ => unreachable!(),
    }
    git(&["add", "."], &root);
    git(
        &[
            "-c",
            "user.name=WebCodex Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "add validation manifest",
        ],
        &root,
    );
    let options = options(root.clone(), state.clone());
    setup(&options).unwrap();
    let (config, paths) = ProjectConfig::resolve(&options).unwrap();
    let connector_key = read_private_value(&paths.connector_key).unwrap();
    let bootstrap_key = read_private_value(&paths.bootstrap_key).unwrap();
    let agent_token = read_private_value(&paths.agent_token).unwrap();
    let grant_id = config.project_grant_id(&paths);
    let credential_verifier =
        crate::auth::ProjectCredentialVerifier::new(grant_id.clone(), &connector_key).unwrap();
    let project_agent_verifier = crate::auth::ProjectAgentTokenVerifier::new(
        grant_id.clone(),
        config.executor_client_id.clone(),
        "local-owner".to_string(),
        &agent_token,
    )
    .unwrap();
    let agent_auth = project_agent_verifier.authenticate(&agent_token).unwrap();
    let registry = Arc::new(RunnerRegistry::default());
    registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: config.executor_client_id.clone(),
                runner_instance_id: "project-agent-instance".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: Some("configured project Agent".to_string()),
                owner: Some("local-owner".to_string()),
                hostname: Some("private-host".to_string()),
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        shell: true,
                        ..Default::default()
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(&agent_auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &registry,
        &config.executor_client_id,
        "project-agent-instance",
        vec![RunnerProjectSummary {
            id: config.executor_project_id.clone(),
            name: Some(config.project_name.clone()),
            path: config.root.to_string_lossy().into_owned(),
            allow_patch: true,
            kind: Some("auto".to_string()),
            registration_source: None,
            description: None,
            hooks: Vec::new(),
            disabled: false,
            revision: None,
            git_branch: Some("main".to_string()),
            git_head: None,
            git_dirty: Some(false),
            updated_at: 1,
            shell_profile: None,
        }],
    )
    .await;
    let db = Arc::new(crate::Database::open(&state.join("data/webcodex.db")).unwrap());
    let tools = Arc::new(ToolRuntime::new_for_tests_with_runner_registry(
        registry.clone(),
    ));
    let runtime_project_id = config.runtime_project_id();
    let connector = Arc::new(
        ConnectorRuntime::new(
            tools.clone(),
            db.clone(),
            ConnectorContext {
                project_id: config.logical_project_id.clone(),
                project_name: config.project_name.clone(),
                workspace_id: config.workspace_id.clone(),
                executor_project: runtime_project_id,
                executor_root: config.root.to_string_lossy().into_owned(),
                runs_root: paths.runs.to_string_lossy().into_owned(),
                results_root: paths.results.to_string_lossy().into_owned(),
                project_registry_dir: paths.project_registry.to_string_lossy().into_owned(),
                profile: config.profile.clone(),
                project_grant_id: grant_id,
            },
            credential_verifier,
        )
        .unwrap()
        .with_project_agent_token(project_agent_verifier),
    );
    let recorded_requests = Arc::new(Mutex::new(Vec::new()));
    let http_config = Arc::new(crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: state.join("data"),
        token: Some(bootstrap_key.clone()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    });
    let router = Router::new()
        .hoop(affix_state::inject(http_config))
        .hoop(affix_state::inject(db.clone()))
        .hoop(affix_state::inject(tools))
        .hoop(affix_state::inject(registry.clone()))
        .hoop(affix_state::inject(ConnectorRuntimeSlot(Some(
            connector.clone(),
        ))))
        .hoop(affix_state::inject(AuthenticatedRequestRecorder(
            recorded_requests.clone(),
        )))
        .push(
            Router::with_path("api")
                .hoop(crate::AuthMiddleware)
                .hoop(record_authenticated_connector_request)
                .push(crate::connector_runtime::http::routes())
                .push(crate::host_console_http::routes())
                .push(
                    Router::with_path("shell/agent/register")
                        .post(crate::runner_http::runner_register),
                )
                .push(
                    Router::with_path("shell/agent/offline")
                        .post(crate::runner_http::runner_offline),
                )
                .push(Router::with_path("shell/agent/poll").post(crate::runner_http::runner_poll))
                .push(
                    Router::with_path("shell/agent/result").post(crate::runner_http::runner_result),
                )
                .push(
                    Router::with_path("shell/agent/job_update")
                        .post(crate::runner_http::runner_job_update),
                ),
        );
    AuthenticatedProjectFixture {
        _temp: temp,
        service: Service::new(router),
        db,
        registry,
        connector,
        agent_auth,
        credential: connector_key,
        agent_token,
        bootstrap: bootstrap_key,
        client_id: config.executor_client_id,
        root,
        state,
        recorded_requests,
    }
}

fn response_status(response: &salvo::Response) -> StatusCode {
    response.status_code.unwrap_or(StatusCode::OK)
}

async fn post_connector(
    fixture: &AuthenticatedProjectFixture,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut response = TestClient::post(format!("http://localhost{path}"))
        .bearer_auth(token)
        .json(&body)
        .send(&fixture.service)
        .await;
    let status = response_status(&response);
    let body = response
        .take_json::<serde_json::Value>()
        .await
        .unwrap_or_else(|_| serde_json::json!({}));
    (status, body)
}

fn runner_transport_cases(client_id: &str) -> [(&'static str, serde_json::Value); 5] {
    [
        (
            "/api/shell/agent/register",
            serde_json::json!({
                "client_id": client_id,
                "agent_instance_id": PROJECT_AGENT_INSTANCE,
                "agent_protocol_generation": 2,
                "capabilities": crate::test_support::current_runner_capabilities(RunnerCapabilities::default()),
                "owner": "local-owner"
            }),
        ),
        (
            "/api/shell/agent/offline",
            serde_json::json!({
                "client_id": client_id,
                "agent_instance_id": PROJECT_AGENT_INSTANCE
            }),
        ),
        (
            "/api/shell/agent/poll",
            serde_json::json!({
                "client_id": client_id,
                "agent_instance_id": PROJECT_AGENT_INSTANCE
            }),
        ),
        (
            "/api/shell/agent/result",
            serde_json::json!({
                "client_id": client_id,
                "agent_instance_id": PROJECT_AGENT_INSTANCE,
                "request_id": "req-auth-boundary"
            }),
        ),
        (
            "/api/shell/agent/job_update",
            serde_json::json!({
                "client_id": client_id,
                "agent_instance_id": PROJECT_AGENT_INSTANCE,
                "job_id": "job-auth-boundary",
                "status": "running"
            }),
        ),
    ]
}

const PROJECT_AGENT_INSTANCE: &str = "project-agent-instance";

#[tokio::test]
async fn project_credential_is_rejected_from_every_runner_transport_route() {
    let fixture = authenticated_project_fixture().await;
    for (path, body) in runner_transport_cases(&fixture.client_id) {
        let (status, response) = post_connector(&fixture, path, &fixture.credential, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {response}");
        assert!(
            response["error"]
                .as_str()
                .unwrap_or_default()
                .contains("bound Agent Token"),
            "{path}: {response}"
        );
    }
}

#[tokio::test]
async fn project_agent_token_enforces_client_id_and_bootstrap_still_registers() {
    let fixture = authenticated_project_fixture().await;

    let (status, response) = post_connector(
        &fixture,
        "/api/shell/agent/register",
        &fixture.agent_token,
        runner_transport_cases(&fixture.client_id)[0].1.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["success"], true);

    for (path, body) in runner_transport_cases("wrong-project-client") {
        let (status, response) = post_connector(&fixture, path, &fixture.agent_token, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {response}");
    }

    let (status, response) = post_connector(
        &fixture,
        "/api/shell/agent/register",
        &fixture.bootstrap,
        serde_json::json!({
            "client_id": "bootstrap-client",
            "agent_instance_id": "bootstrap-instance",
            "agent_protocol_generation": 2,
            "capabilities": crate::test_support::current_runner_capabilities(RunnerCapabilities::default()),
            "owner": "local-owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["success"], true);
}

async fn next_project_agent_request(registry: &RunnerRegistry, client_id: &str) -> RunnerRequest {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(request) = registry
            .poll(RunnerPollRequest {
                client_id: client_id.to_string(),
                runner_instance_id: PROJECT_AGENT_INSTANCE.to_string(),
            })
            .await
            .unwrap()
        {
            return request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "configured project Agent did not receive a request within 10 seconds"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn complete_project_agent_request(
    registry: &RunnerRegistry,
    client_id: &str,
    request: RunnerRequest,
    exit_code: i32,
    stdout: String,
    stderr: String,
) {
    registry
        .complete(RunnerResultRequest {
            client_id: client_id.to_string(),
            runner_instance_id: PROJECT_AGENT_INSTANCE.to_string(),
            request_id: request.request_id,
            exit_code: Some(exit_code),
            stdout: Some(stdout),
            stderr: Some(stderr),
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
}

fn record_agent_request(recorder: &Arc<Mutex<Vec<String>>>, request: &RunnerRequest) {
    recorder.lock().unwrap().push(request.kind.clone());
}

fn run_runner_shell_request(request: &RunnerRequest) -> (i32, String, String) {
    let mut command = Command::new(crate::tool_runtime::test_shell());
    command.args(["-lc", &request.command]);
    if let Some(cwd) = request.cwd.as_deref() {
        command.current_dir(cwd);
    }
    let output = command.output().unwrap();
    (
        output.status.code().unwrap_or(1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

async fn complete_project_job(
    registry: &RunnerRegistry,
    client_id: &str,
    request: RunnerRequest,
    validation: bool,
) {
    let job_id = request.job_id.expect("job dispatch must include job_id");
    registry
        .update_job(RunnerJobUpdateRequest {
            client_id: client_id.to_string(),
            runner_instance_id: PROJECT_AGENT_INSTANCE.to_string(),
            update_seq: None,
            job_id,
            request_id: None,
            status: "completed".to_string(),
            stdout_chunk: Some(if validation {
                "check passed\n".to_string()
            } else {
                "command completed\n".to_string()
            }),
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: Some(0),
            duration_ms: Some(1),
            error: None,
            command_execution_state: None,
            validation_progress: validation.then_some(ShellJobValidationProgress {
                completed: 1,
                current_step: None,
                failed_step: None,
            }),
            activity: None,
            finished: true,
        })
        .await
        .unwrap();
}

struct GoldenPathEvidence {
    request_paths: Vec<String>,
    event_kinds: Vec<String>,
    agent_request_kinds: Vec<String>,
    accepted_output: String,
    accepted_content: String,
    execution_count: i64,
}

async fn run_authenticated_golden_path(recipe: &str) -> GoldenPathEvidence {
    let fixture = authenticated_project_fixture_for(recipe).await;
    let agent_requests = Arc::new(Mutex::new(Vec::new()));

    let (status, readiness) = post_connector(
        &fixture,
        "/api/connector/readiness",
        &fixture.credential,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(readiness["ready"], true);

    let registry = fixture.registry.clone();
    let client_id = fixture.client_id.clone();
    let recorder = agent_requests.clone();
    let registration = tokio::spawn(async move {
        let request = next_project_agent_request(&registry, &client_id).await;
        record_agent_request(&recorder, &request);
        assert_eq!(request.kind, "register_project");
        let payload: serde_json::Value =
            serde_json::from_str(request.stdin.as_deref().unwrap()).unwrap();
        let stdout = serde_json::json!({
            "agent_project_id": payload["id"],
            "client_id": client_id,
            "name": payload["name"],
            "path": payload["path"],
            "allow_patch": true
        })
        .to_string();
        complete_project_agent_request(&registry, &client_id, request, 0, stdout, String::new())
            .await;
    });
    let (status, started) = post_connector(
        &fixture,
        "/api/connector/task/start",
        &fixture.credential,
        serde_json::json!({
            "goal": "exercise the authenticated project golden path",
            "mode": "normal"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["ok"], true, "{started}");
    registration.await.unwrap();
    let task_id = started["task_id"].as_str().unwrap().to_string();

    let registry = fixture.registry.clone();
    let client_id = fixture.client_id.clone();
    let recorder = agent_requests.clone();
    let reader = tokio::spawn(async move {
        let request = next_project_agent_request(&registry, &client_id).await;
        record_agent_request(&recorder, &request);
        assert_eq!(request.kind, "file_read");
        let stdout = serde_json::json!({
            "format": "webcodex.file_read_range.v1",
            "path": "README.md",
            "content": "fixture",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "start_line": 1,
            "limit": 200,
            "total_lines": 1,
            "truncated": false
        })
        .to_string();
        complete_project_agent_request(&registry, &client_id, request, 0, stdout, String::new())
            .await;
    });
    let (status, read) = post_connector(
        &fixture,
        "/api/connector/files/read",
        &fixture.credential,
        serde_json::json!({
            "task_id": task_id,
            "files": [{"path": "README.md"}]
        }),
    )
    .await;
    reader.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["ok"], true, "{read}");

    let registry = fixture.registry.clone();
    let client_id = fixture.client_id.clone();
    let recorder = agent_requests.clone();
    let searcher = tokio::spawn(async move {
        let request = next_project_agent_request(&registry, &client_id).await;
        record_agent_request(&recorder, &request);
        assert_eq!(request.kind, "run_shell");
        let (exit_code, stdout, stderr) = run_runner_shell_request(&request);
        complete_project_agent_request(&registry, &client_id, request, exit_code, stdout, stderr)
            .await;
    });
    let (status, searched) = post_connector(
        &fixture,
        "/api/connector/files/search",
        &fixture.credential,
        serde_json::json!({
            "task_id": task_id,
            "pattern": "fixture",
            "limit": 10
        }),
    )
    .await;
    searcher.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(searched["ok"], true, "{searched}");

    let task = fixture
        .db
        .local_connector_task(&task_id, &fixture.connector.context().project_id)
        .unwrap();
    let execution_root = PathBuf::from(&task.execution_root);
    let registry = fixture.registry.clone();
    let client_id = fixture.client_id.clone();
    let recorder = agent_requests.clone();
    let editor = tokio::spawn(async move {
        let request = next_project_agent_request(&registry, &client_id).await;
        record_agent_request(&recorder, &request);
        assert_eq!(request.kind, "file_apply_text_edits");
        fs::write(execution_root.join("golden.txt"), "accepted\n").unwrap();
        let stdout = serde_json::json!({
            "dry_run": false,
            "applied_count": 1,
            "changed": true,
            "would_change": true,
            "files": [{"index": 0, "kind": "create", "path": "golden.txt"}],
            "changed_paths": ["golden.txt"]
        })
        .to_string();
        complete_project_agent_request(&registry, &client_id, request, 0, stdout, String::new())
            .await;
    });
    let (status, edited) = post_connector(
        &fixture,
        "/api/connector/edits/apply",
        &fixture.credential,
        serde_json::json!({
            "task_id": task_id,
            "operation_id": "golden-edit-1",
            "changes": [{
                "kind": "create",
                "path": "golden.txt",
                "content": "accepted\n"
            }]
        }),
    )
    .await;
    editor.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["ok"], true, "{edited}");

    let command = serde_json::json!({
        "task_id": task_id,
        "operation_id": "golden-command-1",
        "command": "printf golden-command",
        "timeout_secs": 30
    });
    let registry = fixture.registry.clone();
    let client_id = fixture.client_id.clone();
    let recorder = agent_requests.clone();
    let commander = tokio::spawn(async move {
        let request = next_project_agent_request(&registry, &client_id).await;
        record_agent_request(&recorder, &request);
        assert_eq!(request.kind, "start_job");
        complete_project_job(&registry, &client_id, request, false).await;
    });
    let (status, commanded) = post_connector(
        &fixture,
        "/api/connector/commands/run",
        &fixture.credential,
        command,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{commanded}");
    assert_eq!(commanded["ok"], true, "{commanded}");
    commander.await.unwrap();

    let registry = fixture.registry.clone();
    let client_id = fixture.client_id.clone();
    let recorder = agent_requests.clone();
    let expected_program = match recipe {
        "rust" => "cargo",
        "node" => "npm",
        "python" => "python",
        "go" => "go",
        _ => unreachable!(),
    }
    .to_string();
    let checker = tokio::spawn(async move {
        let request = next_project_agent_request(&registry, &client_id).await;
        record_agent_request(&recorder, &request);
        assert_eq!(request.kind, "start_validation_job");
        let steps: Vec<crate::runner_protocol::ShellJobValidationStep> =
            serde_json::from_str(&request.command).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].program, expected_program);
        complete_project_job(&registry, &client_id, request, true).await;
    });
    let (status, checked) = post_connector(
        &fixture,
        "/api/connector/checks/run",
        &fixture.credential,
        serde_json::json!({
            "task_id": task_id,
            "operation_id": "golden-check-1",
            "checks": ["check"],
            "recipe": recipe,
            "timeout_secs": 30
        }),
    )
    .await;
    checker.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(checked["ok"], true, "{checked}");
    assert_eq!(checked["data"]["execution"]["assertion_status"], "passed");

    let (status, finished) = post_connector(
        &fixture,
        "/api/connector/task/finish",
        &fixture.credential,
        serde_json::json!({
            "task_id": task_id,
            "summary": "authenticated golden path result"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(finished["ok"], true, "{finished}");
    assert_eq!(finished["data"]["status"], "ready_for_review");

    let (status, reviewed) = post_connector(
        &fixture,
        "/api/connector/task/review",
        &fixture.credential,
        serde_json::json!({
            "task_id": task_id,
            "include_diff": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reviewed["ok"], true, "{reviewed}");
    assert!(reviewed["data"]["changes"]["diff_preview"]["text"]
        .as_str()
        .unwrap()
        .contains("accepted"));

    let accept = crate::task_cli::parse(&[
        "accept".to_string(),
        task_id.clone(),
        "--root".to_string(),
        fixture.root.to_string_lossy().into_owned(),
        "--state-dir".to_string(),
        fixture.state.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let accepted_output = crate::task_cli::run(accept).unwrap();
    let accepted_content = fs::read_to_string(fixture.root.join("golden.txt")).unwrap();
    let event_kinds = fixture
        .db
        .local_connector_task_events(&task_id, &fixture.connector.context().project_id, 100)
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    let execution_count = fixture
        .db
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM wc_executions", [], |row| row.get(0))
        .unwrap();

    let request_paths = fixture.recorded_requests.lock().unwrap().clone();
    let agent_request_kinds = agent_requests.lock().unwrap().clone();
    GoldenPathEvidence {
        request_paths,
        event_kinds,
        agent_request_kinds,
        accepted_output,
        accepted_content,
        execution_count,
    }
}

#[tokio::test]
async fn configured_project_credential_can_complete_connector_golden_path() {
    let evidence = run_authenticated_golden_path("rust").await;
    assert!(evidence.accepted_output.contains("Accepted"));
    assert_eq!(evidence.accepted_content, "accepted\n");
    assert!(evidence.execution_count >= 2);
    for capability in [
        "task_started",
        "files_read",
        "files_search",
        "edits_apply",
        "authority_auto_authorized",
        "execution_succeeded",
        "task_finished",
        "workspace_release",
        "task_accepted",
    ] {
        assert!(
            evidence.event_kinds.iter().any(|kind| kind == capability),
            "durable task ledger did not record {capability}: {:?}",
            evidence.event_kinds
        );
    }
}

#[tokio::test]
async fn authenticated_golden_path_emits_no_discovery_or_session_calls() {
    let evidence = run_authenticated_golden_path("rust").await;
    let canonical_paths = [
        "/api/connector/readiness",
        "/api/connector/task/start",
        "/api/connector/files/read",
        "/api/connector/files/search",
        "/api/connector/edits/apply",
        "/api/connector/commands/run",
        "/api/connector/checks/run",
        "/api/connector/task/finish",
        "/api/connector/task/review",
    ];
    assert!(evidence
        .request_paths
        .iter()
        .all(|path| canonical_paths.contains(&path.as_str())));
    assert_eq!(
        evidence.request_paths,
        [
            "/api/connector/readiness",
            "/api/connector/task/start",
            "/api/connector/files/read",
            "/api/connector/files/search",
            "/api/connector/edits/apply",
            "/api/connector/commands/run",
            "/api/connector/checks/run",
            "/api/connector/task/finish",
            "/api/connector/task/review",
        ]
    );
    assert_eq!(
        evidence
            .request_paths
            .iter()
            .filter(|path| path.as_str() == "/api/connector/commands/run")
            .count(),
        1,
        "trusted_agent authority must run the command in one call without an approval retry"
    );
    for forbidden in [
        "list_projects",
        "runtime_status",
        "tool_manifest",
        "start_session",
        "current_session",
        "list_runners",
    ] {
        assert!(!evidence
            .request_paths
            .iter()
            .chain(evidence.event_kinds.iter())
            .chain(evidence.agent_request_kinds.iter())
            .any(|record| record.contains(forbidden)));
    }
}

fn assert_no_project_state_artifacts(root: &Path) {
    for relative in [
        "credentials",
        "agent",
        "project.toml",
        "runs",
        "results",
        ".webcodex",
    ] {
        assert!(
            !root.join(relative).exists(),
            "unsafe state resolution created {relative} inside the checkout"
        );
    }
}

#[tokio::test]
async fn state_directory_boundary_is_shared_and_has_no_failure_side_effects() {
    let (temp, root, _) = repo("state-boundary");

    let relative_inside = ProjectCommandOptions {
        state_dir: Some(PathBuf::from(".webcodex")),
        ..options(root.clone(), temp.path().join("unused"))
    };
    let relative_error =
        setup_service::resolve_state_path_from(&relative_inside, &root, "unused-project-id", &root)
            .unwrap_err();
    assert_eq!(relative_error.code, "state_directory_unsafe");
    assert_no_project_state_artifacts(&root);

    for state in [root.clone(), root.join(".webcodex")] {
        let unsafe_options = options(root.clone(), state);
        let setup_error = setup(&unsafe_options).unwrap_err();
        assert_eq!(setup_error.code, "state_directory_unsafe");
        assert!(setup_error.message.contains("outside"));

        let readiness = readiness_with_probe(&unsafe_options, RemoteProbe::Unreachable);
        assert_eq!(
            fact(&readiness, "state_directory_unsafe").status,
            ReadinessStatus::Fail
        );
        let task_error =
            resolve_local_task_state(&root, "personal", unsafe_options.state_dir.as_deref())
                .unwrap_err();
        assert!(task_error.contains("outside"));
        let start_error = start_runner(&unsafe_options).await.unwrap_err();
        assert_eq!(start_error.code, "state_directory_unsafe");
        assert_no_project_state_artifacts(&root);
    }
}

#[test]
fn state_directory_allows_absolute_and_relative_paths_outside_checkout() {
    let (temp, root, _) = repo("outside-state");

    let absolute = temp.path().join("absolute-state");
    setup(&options(root.clone(), absolute.clone())).unwrap();
    assert!(absolute.join("credentials/connector-key").is_file());
    assert!(absolute.join("credentials/agent-token").is_file());

    let relative_options = ProjectCommandOptions {
        state_dir: Some(PathBuf::from("../relative-state")),
        ..options(root.clone(), temp.path().join("unused"))
    };
    let resolved = setup_service::resolve_state_path_from(
        &relative_options,
        &root,
        "unused-project-id",
        &root,
    )
    .unwrap();
    assert_eq!(
        resolved,
        temp.path().canonicalize().unwrap().join("relative-state")
    );
    setup(&options(root, resolved.clone())).unwrap();
    assert!(resolved.join("project.toml").is_file());
}

#[test]
fn default_state_base_preserves_home_and_has_windows_localappdata_fallback() {
    use std::ffi::OsStr;

    assert_eq!(
        setup_service::default_state_base_from(
            Some(OsStr::new("/state")),
            Some(OsStr::new("/home/user")),
            Some(OsStr::new("/local")),
        )
        .unwrap(),
        PathBuf::from("/state/webcodex/projects")
    );
    assert_eq!(
        setup_service::default_state_base_from(
            None,
            Some(OsStr::new("/home/user")),
            Some(OsStr::new("/local")),
        )
        .unwrap(),
        PathBuf::from("/home/user/.local/state/webcodex/projects")
    );
    assert_eq!(
        setup_service::default_state_base_from(None, None, Some(OsStr::new("/local"))).unwrap(),
        PathBuf::from("/local/WebCodex/state/projects")
    );
    assert!(setup_service::default_state_base_from(None, None, None).is_err());
}

#[cfg(unix)]
#[test]
fn state_directory_rejects_symlink_that_resolves_into_checkout() {
    use std::os::unix::fs::symlink;

    let (temp, root, _) = repo("symlink-state");
    let link = temp.path().join("state-link");
    symlink(&root, &link).unwrap();
    let unsafe_state = link.join(".webcodex");
    let error = setup(&options(root.clone(), unsafe_state)).unwrap_err();

    assert_eq!(error.code, "state_directory_unsafe");
    assert_no_project_state_artifacts(&root);
}

#[test]
fn fresh_setup_is_minimal_idempotent_and_does_not_expose_internal_ids() {
    let (_temp, root, state) = repo("demo");
    let options = options(root, state.clone());

    let first = setup(&options).unwrap();
    assert_eq!(first.status, "configured");
    assert_eq!(
        first.changed,
        ["Connection", "Runner", "Project registration"]
    );
    let agent = fs::read_to_string(state.join("agent/runner.toml")).unwrap();
    let registration = fs::read_to_string(
        fs::read_dir(state.join("agent/project-registry"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    let agent_toml: toml::Value = toml::from_str(&agent).unwrap();
    let registration_toml: toml::Value = toml::from_str(&registration).unwrap();
    let connector_credential =
        read_private_value(&state.join("credentials/connector-key")).unwrap();
    let agent_token = read_private_value(&state.join("credentials/agent-token")).unwrap();
    assert_ne!(connector_credential, agent_token);
    assert!(agent_token.starts_with("wc_agent_"));
    assert_eq!(agent_toml["token"].as_str(), Some(agent_token.as_str()));
    let canonical_root = options.root.canonicalize().unwrap();
    let canonical_state = state.canonicalize().unwrap();
    assert_eq!(
        registration_toml["path"].as_str(),
        Some(canonical_root.to_string_lossy().as_ref())
    );
    assert_eq!(
        agent_toml["project_registry_dir"].as_str(),
        Some(
            canonical_state
                .join("agent/project-registry")
                .to_string_lossy()
                .as_ref()
        )
    );
    let before = (agent, registration);

    let second = setup(&options).unwrap();
    assert_eq!(second.status, "already_configured");
    assert!(second.changed.is_empty());
    assert_eq!(
        fs::read_to_string(state.join("agent/runner.toml")).unwrap(),
        before.0
    );
    let project_file = fs::read_dir(state.join("agent/project-registry"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(fs::read_to_string(project_file).unwrap(), before.1);

    let output = render_setup_text(&second);
    for forbidden in [
        "client_id",
        "runtime project",
        "executor_ref",
        "workflow session",
        "agent:local",
        "wc_proj_",
        "token",
        "credentials/",
    ] {
        assert!(
            !output.to_ascii_lowercase().contains(forbidden),
            "default setup output leaked {forbidden}: {output}"
        );
    }
    assert!(output.contains("Next:\n  webcodex doctor"));
}

#[test]
fn setup_preserves_a_single_legacy_project_registry_layout() {
    let (_temp, root, state) = repo("legacy-registry");
    let legacy = state.join("agent/projects.d");
    fs::create_dir_all(&legacy).unwrap();
    let options = options(root, state.clone());

    setup(&options).unwrap();

    assert!(legacy.is_dir());
    assert!(!state.join("agent/project-registry").exists());
    let runner = fs::read_to_string(state.join("agent/runner.toml")).unwrap();
    let runner_toml: toml::Value = toml::from_str(&runner).unwrap();
    assert_eq!(
        runner_toml["project_registry_dir"].as_str(),
        Some(legacy.canonicalize().unwrap().to_string_lossy().as_ref())
    );
    assert_eq!(fs::read_dir(legacy).unwrap().count(), 1);
}

#[test]
fn setup_fails_closed_when_both_project_registry_layouts_exist() {
    let (_temp, root, state) = repo("ambiguous-registry");
    fs::create_dir_all(state.join("agent/project-registry")).unwrap();
    fs::create_dir_all(state.join("agent/projects.d")).unwrap();

    let error = setup(&options(root, state)).unwrap_err();
    assert_eq!(error.code, "project_registration_invalid");
    assert!(
        error
            .message
            .contains("both Runner project registry directories exist"),
        "{}",
        error.message
    );
}

#[test]
fn setup_repairs_only_missing_components_and_preserves_existing_config() {
    let (_temp, root, state) = repo("repair");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let agent_path = state.join("agent/runner.toml");
    let original_agent = fs::read_to_string(&agent_path).unwrap();
    let project_path = fs::read_dir(state.join("agent/project-registry"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&project_path).unwrap();

    let report = setup(&options).unwrap();
    assert_eq!(report.changed, ["Project registration"]);
    assert_eq!(fs::read_to_string(agent_path).unwrap(), original_agent);
    assert!(project_path.is_file());
}

#[test]
fn setup_accepts_legacy_agent_toml_only_without_creating_runner_toml() {
    let (_temp, root, state) = repo("legacy-runner-config");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let runner_config = state.join("agent/runner.toml");
    let legacy_config = state.join("agent/agent.toml");
    let original = fs::read(&runner_config).unwrap();
    fs::rename(&runner_config, &legacy_config).unwrap();

    let report = setup(&options).unwrap();
    assert_eq!(report.status, "already_configured");
    assert_eq!(fs::read(&legacy_config).unwrap(), original);
    assert!(!runner_config.exists());
}

#[test]
fn setup_rejects_dual_runner_config_names_without_guessing() {
    let (_temp, root, state) = repo("dual-runner-config");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let runner_config = state.join("agent/runner.toml");
    let legacy_config = state.join("agent/agent.toml");
    fs::copy(&runner_config, &legacy_config).unwrap();

    let error = setup(&options).unwrap_err();
    assert_eq!(error.code, "project_registration_invalid");
    assert!(error.message.contains("runner.toml"));
    assert!(error.message.contains("agent.toml"));
    assert!(error.message.contains("refusing to guess"));
}

#[test]
fn setup_conflict_and_project_root_collision_fail_closed() {
    let (temp, root, state) = repo("first");
    let first = options(root, state.clone());
    setup(&first).unwrap();
    let agent_path = state.join("agent/runner.toml");
    let before = fs::read_to_string(&agent_path).unwrap();
    let mut conflicting = before.replace("server_url = ", "server_url = \"http://invalid\" # ");
    if conflicting == before {
        conflicting.push_str("\nserver_url = \"http://invalid\"\n");
    }
    fs::write(&agent_path, conflicting).unwrap();

    let error = setup(&first).unwrap_err();
    assert_eq!(error.code, "project_registration_invalid");
    assert!(error.message.contains("server_url"));

    let second_root = temp.path().join("second");
    fs::create_dir(&second_root).unwrap();
    git(&["init", "-q"], &second_root);
    let collision = options(second_root, state);
    let error = setup(&collision).unwrap_err();
    assert_eq!(error.code, "project_registration_invalid");
    assert!(error.message.contains("project root"));
}

#[test]
fn setup_client_ids_include_project_grant_identity() {
    let (temp, root, _) = repo("grant-scoped-client");
    let first = options(root.clone(), temp.path().join("state-a"));
    let second = options(root, temp.path().join("state-b"));
    let (first_config, first_paths) = ProjectConfig::resolve(&first).unwrap();
    let (second_config, second_paths) = ProjectConfig::resolve(&second).unwrap();

    assert_eq!(
        first_config.logical_project_id,
        second_config.logical_project_id
    );
    assert_ne!(
        first_config.project_grant_id(&first_paths),
        second_config.project_grant_id(&second_paths)
    );
    assert_ne!(
        first_config.executor_client_id,
        second_config.executor_client_id
    );
}

#[test]
fn setup_selects_stable_fallback_port_and_persists_it() {
    let (_temp, root, state) = repo("port-collision");
    let options = options(root, state.clone());
    let (expected, _) = ProjectConfig::resolve(&options).unwrap();
    let occupied = std::net::TcpListener::bind(("127.0.0.1", expected.port)).unwrap();

    setup(&options).unwrap();
    let persisted: ProjectConfig =
        toml::from_str(&fs::read_to_string(state.join("project.toml")).unwrap()).unwrap();
    assert_ne!(persisted.port, expected.port);
    assert_eq!(
        persisted.port,
        20_000 + ((u32::from(expected.port - 20_000) + 7_919) % 20_000) as u16
    );
    drop(occupied);

    let second = setup(&options).unwrap();
    assert_eq!(second.status, "already_configured");
    let again: ProjectConfig =
        toml::from_str(&fs::read_to_string(state.join("project.toml")).unwrap()).unwrap();
    assert_eq!(again.port, persisted.port);
}

#[test]
fn setup_does_not_replace_persisted_port_when_temporarily_occupied() {
    let (_temp, root, state) = repo("stable-port");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let config_path = state.join("project.toml");
    let before = fs::read(&config_path).unwrap();
    let config: ProjectConfig = toml::from_str(std::str::from_utf8(&before).unwrap()).unwrap();
    let occupied = std::net::TcpListener::bind(("127.0.0.1", config.port)).unwrap();

    let report = setup(&options).unwrap();

    assert_eq!(report.status, "already_configured");
    assert_eq!(fs::read(config_path).unwrap(), before);
    drop(occupied);
}

#[test]
fn doctor_and_status_share_table_driven_readiness_facts_and_stay_read_only() {
    let (_temp, root, state) = repo("readiness");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let agent_path = state.join("agent/runner.toml");
    let before = fs::read(&agent_path).unwrap();
    let managed_slot = state.join("runs/wc-slot-write-01");
    let managed_lease = state.join("runs/.wc-slot-write-01.lease.json");
    assert!(!managed_slot.exists());
    assert!(!managed_lease.exists());

    let cases = [
        (RemoteProbe::Ready, true, "ready"),
        (RemoteProbe::Unreachable, false, "server_unreachable"),
        (
            RemoteProbe::CredentialRejected,
            false,
            "project_credential_rejected",
        ),
        (RemoteProbe::RunnerOffline, false, "agent_offline"),
        (
            RemoteProbe::ProjectMissing,
            false,
            "project_registration_invalid",
        ),
        (
            RemoteProbe::RequiredCapabilityMissing,
            false,
            "required_capability_unavailable",
        ),
        (
            RemoteProbe::StructuredValidationMissing,
            false,
            "structured_validation_unavailable",
        ),
    ];
    for (probe, expected_ready, expected_code) in cases {
        let readiness = readiness_with_probe(&options, probe);
        assert_eq!(readiness.ready, expected_ready);
        let writable = fact(&readiness, "writable_workspace_uninitialized");
        assert_eq!(writable.status, ReadinessStatus::Pass);
        assert!(writable.summary.contains("will be created"));
        assert!(readiness
            .findings
            .iter()
            .any(|finding| finding.code == expected_code));
        let status = render_status_text(&readiness);
        let doctor = render_doctor_text(&readiness);
        assert_eq!(
            status.contains("Coding access: ready"),
            expected_ready,
            "{status}"
        );
        assert!(status.contains(&format!("Capabilities: {}", readiness.capabilities)));
        assert!(doctor.contains(&fact(&readiness, expected_code).summary));
        assert!(doctor.contains("Writable workspace"));
        assert!(!doctor.contains("Command sandbox"));
        assert!(
            !managed_slot.exists(),
            "doctor/status must not create a worktree"
        );
        assert!(
            !managed_lease.exists(),
            "doctor/status must not claim a write-slot lease"
        );
        for output in [status, doctor] {
            assert!(!output.contains("agent:"));
            assert!(!output.contains("client_id"));
            assert!(!output.contains("wc_proj_"));
        }
    }
    assert_eq!(fs::read(agent_path).unwrap(), before);
}

#[test]
fn doctor_reports_not_setup_and_invalid_workspace_with_stable_actions() {
    let (_temp, root, state) = repo("invalid");
    let options = options(root.clone(), state);
    let missing = readiness_with_probe(&options, RemoteProbe::Unreachable);
    assert_eq!(missing.connection, "not configured");
    let finding = fact(&missing, "project_not_configured");
    assert_eq!(finding.status, ReadinessStatus::Fail);
    assert_eq!(finding.next_action.as_deref(), Some("webcodex setup"));

    setup(&options).unwrap();
    fs::remove_dir_all(root).unwrap();
    let invalid = readiness_with_probe(&options, RemoteProbe::Ready);
    assert_eq!(
        fact(&invalid, "workspace_unavailable").status,
        ReadinessStatus::Fail
    );
}

#[tokio::test]
async fn arbitrary_shared_key_cannot_access_project_connector() {
    // Expected pre-fix failure: project mode currently accepts every unknown
    // non-wc_* bearer as a new shared-key principal.
    let auth_env = crate::auth::AuthEnvGuard::new();
    auth_env.enable_direct_shared_key();
    let fixture = authenticated_project_fixture().await;

    let readiness = TestClient::post("http://localhost/api/connector/readiness")
        .bearer_auth("unconfigured-random-project-token")
        .json(&serde_json::json!({}))
        .send(&fixture.service)
        .await;
    let task_start = TestClient::post("http://localhost/api/connector/task/start")
        .bearer_auth("unconfigured-random-project-token")
        .json(&serde_json::json!({
            "goal": "must never be persisted",
            "mode": "read_only"
        }))
        .send(&fixture.service)
        .await;

    // End the connection guard structurally inside the block so it is not
    // held across the registry await below.
    let (task_count, execution_count, binding_count) = {
        let conn = fixture.db.conn_for_tests();
        (
            conn.query_row("SELECT COUNT(*) FROM wc_tasks", [], |row| row.get(0))
                .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM wc_executions", [], |row| row.get(0))
                .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM wc_connector_grants", [], |row| {
                row.get(0)
            })
            .unwrap(),
        )
    };
    let pending = fixture
        .registry
        .get_runner_view_for_auth(
            &fixture.client_id,
            Some(&crate::test_support::runner_access(&fixture.agent_auth)),
        )
        .await
        .unwrap()
        .pending_requests;

    assert_eq!(
        (
            response_status(&readiness),
            response_status(&task_start),
            task_count,
            execution_count,
            binding_count,
            pending,
        ),
        (
            StatusCode::UNAUTHORIZED,
            StatusCode::UNAUTHORIZED,
            0,
            0,
            0,
            0,
        ),
        "an unconfigured token must be rejected before connector state or Agent requests are created"
    );
    assert!(
        fixture.recorded_requests.lock().unwrap().is_empty(),
        "an arbitrary token must not reach the authenticated Connector adapter"
    );
}

#[tokio::test]
async fn connector_credential_cannot_cross_agent_auth_group() {
    // Expected pre-fix failure: a credential generated by another project is
    // accepted as a fresh shared-key group before ConnectorRuntime runs.
    let auth_env = crate::auth::AuthEnvGuard::new();
    auth_env.enable_direct_shared_key();
    let fixture = authenticated_project_fixture().await;
    let (_other_temp, other_root, other_state) = repo("other-authenticated-project");
    let other_options = options(other_root, other_state.clone());
    setup(&other_options).unwrap();
    let wrong_project_credential =
        read_private_value(&other_state.join("credentials/connector-key")).unwrap();
    let opaque_task_id = format!("wc_task_{}", "0".repeat(32));

    let calls = [
        ("/api/connector/readiness", serde_json::json!({})),
        (
            "/api/connector/task/start",
            serde_json::json!({"goal": "cross-project request", "mode": "read_only"}),
        ),
        (
            "/api/connector/files/read",
            serde_json::json!({"task_id": opaque_task_id, "files": [{"path": "README.md"}]}),
        ),
        (
            "/api/connector/files/search",
            serde_json::json!({"task_id": opaque_task_id, "query": "forbidden"}),
        ),
        (
            "/api/connector/edits/apply",
            serde_json::json!({
                "task_id": opaque_task_id,
                "operation_id": "cross-edit",
                "changes": [{"kind": "create", "path": "forbidden.txt", "content": "no"}]
            }),
        ),
        (
            "/api/connector/commands/run",
            serde_json::json!({
                "task_id": opaque_task_id,
                "operation_id": "cross-command",
                "command": "true"
            }),
        ),
        (
            "/api/connector/checks/run",
            serde_json::json!({
                "task_id": opaque_task_id,
                "operation_id": "cross-check",
                "checks": ["check"]
            }),
        ),
        (
            "/api/connector/task/cancel",
            serde_json::json!({"task_id": opaque_task_id, "reason": "cross-cancel"}),
        ),
        (
            "/api/connector/task/review",
            serde_json::json!({"task_id": opaque_task_id}),
        ),
    ];
    let mut responses = Vec::new();
    for (path, body) in calls {
        responses.push(post_connector(&fixture, path, &wrong_project_credential, body).await);
    }

    // End the connection guard structurally inside the block so it is not
    // held across the registry await below.
    let (task_count, execution_count, binding_count, edit_count) = {
        let conn = fixture.db.conn_for_tests();
        (
            conn.query_row("SELECT COUNT(*) FROM wc_tasks", [], |row| row.get(0))
                .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM wc_executions", [], |row| row.get(0))
                .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM wc_connector_grants", [], |row| {
                row.get(0)
            })
            .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM wc_edit_operations", [], |row| {
                row.get(0)
            })
            .unwrap(),
        )
    };
    let agent = fixture
        .registry
        .get_runner_view_for_auth(
            &fixture.client_id,
            Some(&crate::test_support::runner_access(&fixture.agent_auth)),
        )
        .await
        .unwrap();
    let serialized =
        serde_json::to_string(&responses.iter().map(|(_, body)| body).collect::<Vec<_>>()).unwrap();
    for private in [
        fixture.client_id.as_str(),
        "private-host",
        "project-agent-instance",
    ] {
        assert!(
            !serialized.contains(private),
            "cross-group response leaked private Agent registration data"
        );
    }
    assert!(
        responses
            .iter()
            .all(|(status, _)| *status == StatusCode::UNAUTHORIZED),
        "wrong-project credential statuses: {:?}",
        responses
            .iter()
            .map(|(status, _)| status.as_u16())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        (task_count, execution_count, binding_count, edit_count),
        (0, 0, 0, 0)
    );
    assert_eq!(agent.pending_requests, 0);
    assert!(!fixture.root.join("forbidden.txt").exists());
    assert!(
        fixture.recorded_requests.lock().unwrap().is_empty(),
        "a different project's credential must not reach Connector dispatch"
    );
}

#[tokio::test]
async fn connector_readiness_is_scoped_to_request_principal() {
    // Expected pre-fix failure: readiness ignores the request AuthContext, so
    // another project's credential can distinguish online from offline.
    let auth_env = crate::auth::AuthEnvGuard::new();
    auth_env.enable_direct_shared_key();
    let fixture = authenticated_project_fixture().await;
    let (_other_temp, other_root, other_state) = repo("readiness-other-project");
    let other_options = options(other_root, other_state.clone());
    setup(&other_options).unwrap();
    let wrong_project_credential =
        read_private_value(&other_state.join("credentials/connector-key")).unwrap();

    let online = post_connector(
        &fixture,
        "/api/connector/readiness",
        &wrong_project_credential,
        serde_json::json!({}),
    )
    .await;
    fixture
        .registry
        .set_last_seen_for_test(&fixture.client_id, 0)
        .await;
    let offline = post_connector(
        &fixture,
        "/api/connector/readiness",
        &wrong_project_credential,
        serde_json::json!({}),
    )
    .await;

    assert_eq!(online.0, StatusCode::UNAUTHORIZED);
    assert_eq!(offline.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        online.1, offline.1,
        "an unauthorized principal must not receive an Agent liveness oracle"
    );
}

#[test]
fn doctor_reports_malformed_registration() {
    let (_temp, root, state) = repo("malformed-registration");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let project_path = fs::read_dir(state.join("agent/project-registry"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&project_path, "this is not = valid TOML [").unwrap();
    let before = fs::read(&project_path).unwrap();

    let readiness = readiness_with_probe(&options, RemoteProbe::Unreachable);
    assert_eq!(
        fact(&readiness, "project_registration_invalid").status,
        ReadinessStatus::Fail
    );
    assert!(!readiness
        .findings
        .iter()
        .any(|finding| finding.code == "project_not_configured"));
    assert_eq!(fs::read(project_path).unwrap(), before);
}

#[test]
fn doctor_reports_conflicting_registration() {
    let (temp, root, state) = repo("conflicting-registration");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let project_path = fs::read_dir(state.join("agent/project-registry"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let before = fs::read_to_string(&project_path).unwrap();
    let different_root = temp.path().join("different-project");
    fs::create_dir(&different_root).unwrap();
    let different_root = different_root.canonicalize().unwrap();
    let mut registration: toml::Value = toml::from_str(&before).unwrap();
    registration["path"] = toml::Value::String(different_root.to_string_lossy().into_owned());
    let conflicting = toml::to_string(&registration).unwrap();
    assert_ne!(conflicting, before);
    fs::write(&project_path, &conflicting).unwrap();

    let readiness = readiness_with_probe(&options, RemoteProbe::Unreachable);
    assert_eq!(
        fact(&readiness, "project_registration_invalid").status,
        ReadinessStatus::Fail
    );
    assert_eq!(fs::read_to_string(project_path).unwrap(), conflicting);
}

#[test]
fn doctor_reports_missing_private_credential() {
    let (_temp, root, state) = repo("missing-credential");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    fs::remove_file(state.join("credentials/connector-key")).unwrap();

    let readiness = readiness_with_probe(&options, RemoteProbe::Unreachable);
    assert_eq!(
        fact(&readiness, "project_credential_invalid").status,
        ReadinessStatus::Fail
    );
}

#[test]
fn doctor_reports_unreadable_private_credential() {
    let (_temp, root, state) = repo("unreadable-credential");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let credential = state.join("credentials/connector-key");
    fs::remove_file(&credential).unwrap();
    fs::create_dir(&credential).unwrap();

    let readiness = readiness_with_probe(&options, RemoteProbe::Unreachable);
    assert_eq!(
        fact(&readiness, "project_credential_invalid").status,
        ReadinessStatus::Fail
    );
}

#[test]
fn status_does_not_turn_invalid_config_into_not_configured() {
    let (_temp, root, state) = repo("malformed-config");
    let options = options(root, state.clone());
    setup(&options).unwrap();
    let config_path = state.join("project.toml");
    fs::write(&config_path, "version = [broken").unwrap();
    let before = fs::read(&config_path).unwrap();

    let readiness = readiness_with_probe(&options, RemoteProbe::Unreachable);
    assert_eq!(readiness.connection, "invalid");
    assert!(readiness
        .findings
        .iter()
        .any(|finding| finding.code == "project_registration_invalid"));
    assert!(!readiness
        .findings
        .iter()
        .any(|finding| finding.code == "project_not_configured"));
    assert_eq!(fs::read(config_path).unwrap(), before);
}

#[tokio::test]
async fn doctor_reports_rejected_project_credential_without_agent_offline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (_temp, root, state) = repo("rejected-credential");
    let options = options(root, state);
    setup(&options).unwrap();
    let (config, _) = ProjectConfig::resolve(&options).unwrap();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.port))
        .await
        .unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: 24\r\nconnection: close\r\n\r\n{\"error\":\"Unauthorized\"}",
            )
            .await
            .unwrap();
    });

    let readiness = collect_readiness(&options).await;
    server.await.unwrap();
    assert_eq!(readiness.connection, "connected");
    assert_eq!(
        fact(&readiness, "project_credential_rejected").status,
        ReadinessStatus::Fail
    );
    assert!(!readiness
        .findings
        .iter()
        .any(|finding| finding.code == "server_unreachable"));
    assert!(!readiness
        .findings
        .iter()
        .any(|finding| finding.code == "agent_offline"));
}

fn seed_ready_console(
    fixture: &AuthenticatedProjectFixture,
    task_id: &str,
    run_id: &str,
    now: i64,
) -> String {
    let context = fixture.connector.context().clone();
    let manager = crate::connector_runtime::workspace::WorkspaceManager::new(&context).unwrap();
    let subject = format!("project:{}", context.project_grant_id);
    fixture
        .db
        .ensure_connector_binding(crate::db::ConnectorBinding {
            project_id: &context.project_id,
            project_name: &context.project_name,
            workspace_id: &context.workspace_id,
            executor_ref: &context.executor_project,
            subject_id: &subject,
            profile: "personal",
            now,
        })
        .unwrap();
    let prepared = webcodex_connector_runtime::workspace::root_test_support::prepare(
        &manager, &context, task_id, run_id, false,
    )
    .unwrap();
    let task = fixture
        .db
        .start_connector_task(crate::db::NewConnectorTask {
            task_id,
            run_id,
            project_id: &context.project_id,
            workspace_id: &context.workspace_id,
            subject_id: &subject,
            goal: "update readme from the browser console",
            mode: "normal",
            target_executor_ref: &context.executor_project,
            execution_executor_ref: &prepared.execution_executor_ref,
            target_root: &context.executor_root,
            execution_root: &prepared.execution_root,
            baseline_commit: prepared.baseline_commit.as_deref(),
            baseline_tree: prepared.baseline_tree.as_deref(),
            isolated: true,
            now,
        })
        .unwrap();
    fs::write(Path::new(&task.execution_root).join("README.md"), "after\n").unwrap();
    let captured =
        webcodex_connector_runtime::workspace::root_test_support::capture_result(&manager, &task)
            .unwrap();
    let result_id = format!(
        "wc_result_{}",
        &task_id["wc_task_".len().."wc_task_".len() + 16]
    );
    fixture
        .db
        .finish_connector_task(
            task_id,
            &context.project_id,
            &subject,
            crate::db::NewConnectorResult {
                result_id: &result_id,
                summary: "updated readme",
                patch_artifact: captured.patch_artifact.as_deref(),
                patch_sha256: captured.patch_sha256.as_deref(),
                patch_bytes: captured.patch_bytes,
                changed_paths: &captured.changed_paths,
                validation: &serde_json::json!({"status": "not_run"}),
                warnings: &captured.warnings,
            },
            now + 1,
        )
        .unwrap();
    task_id.to_string()
}

#[tokio::test]
async fn console_review_and_accept_golden_path() {
    let fixture = authenticated_project_fixture().await;
    let task_id = seed_ready_console(
        &fixture,
        "wc_task_aa11223344556677889900aabbccddee",
        "wc_run_ca",
        2,
    );

    let (list_status, list) = post_connector(
        &fixture,
        "/api/console/tasks",
        &fixture.credential,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(list_status, StatusCode::OK);
    let rows = list["tasks"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["task_id"], task_id);
    assert_eq!(rows[0]["task_status"], "ready_for_review");
    assert_eq!(rows[0]["next_action"], "review_and_accept");
    let list_text = serde_json::to_string(&list).unwrap();
    assert!(
        !list_text.contains(fixture.root.to_string_lossy().as_ref())
            && !list_text.contains(fixture.connector.context().executor_project.as_str()),
        "work queue leaked a host path or executor reference"
    );

    let (review_status, review) = post_connector(
        &fixture,
        "/api/console/task/review",
        &fixture.credential,
        serde_json::json!({ "task_id": task_id, "include_diff": true }),
    )
    .await;
    assert_eq!(review_status, StatusCode::OK);
    assert_eq!(review["can_accept"], true);
    assert_eq!(review["can_reject"], true);
    assert_eq!(review["can_cancel"], false);
    assert!(review["changes"]["diff_preview"]["text"]
        .as_str()
        .unwrap()
        .contains("after"));
    assert_eq!(review["created_at"], 2);
    assert_eq!(review["updated_at"], 3);
    assert_eq!(review["result"]["validation"]["status"], "not_run");
    assert_eq!(review["next_action"], "review_and_accept");
    let result_id = review["result"]["result_id"].as_str().unwrap().to_string();

    let (accept_status, accepted) = post_connector(
        &fixture,
        "/api/console/result/accept",
        &fixture.credential,
        serde_json::json!({ "task_id": task_id, "result_id": result_id }),
    )
    .await;
    assert_eq!(accept_status, StatusCode::OK);
    assert_eq!(accepted["decision"], "accepted");
    assert_eq!(
        fs::read_to_string(fixture.root.join("README.md")).unwrap(),
        "after\n"
    );

    let recorded = fixture.recorded_requests.lock().unwrap().clone();
    assert!(!recorded.is_empty());
    for path in &recorded {
        assert!(
            path.starts_with("/api/console/"),
            "console call reached a non-console route: {path}"
        );
    }
}

#[tokio::test]
async fn console_requires_a_credential() {
    // The bare 401 depends on open-anonymous mode being off: with a leaked
    // WEBCODEX_ALLOW_ANONYMOUS the middleware would mint an anonymous context
    // and the project surface gate would answer 403 instead.
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let fixture = authenticated_project_fixture().await;
    let response = TestClient::post("http://localhost/api/console/tasks")
        .json(&serde_json::json!({}))
        .send(&fixture.service)
        .await;
    assert_eq!(response_status(&response), StatusCode::UNAUTHORIZED);
    assert!(
        fixture.recorded_requests.lock().unwrap().is_empty(),
        "an unauthenticated console request must not pass AuthMiddleware"
    );
}

#[tokio::test]
async fn console_rejects_a_cross_project_credential() {
    let fixture = authenticated_project_fixture().await;
    let (_other_temp, other_root, other_state) = repo("console-other-project");
    let other_options = options(other_root, other_state.clone());
    setup(&other_options).unwrap();
    let wrong_credential =
        read_private_value(&other_state.join("credentials/connector-key")).unwrap();

    let (status, _body) = post_connector(
        &fixture,
        "/api/console/tasks",
        &wrong_credential,
        serde_json::json!({}),
    )
    .await;

    assert!(
        status.is_client_error(),
        "a different project's credential must be refused, got {status}"
    );
    assert!(fixture.recorded_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn console_enforces_json_and_same_origin_guard() {
    let fixture = authenticated_project_fixture().await;

    let mut wrong_media = TestClient::post("http://localhost/api/console/tasks")
        .bearer_auth(&fixture.credential)
        .text("{}")
        .send(&fixture.service)
        .await;
    assert_eq!(
        response_status(&wrong_media),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        wrong_media
            .take_json::<serde_json::Value>()
            .await
            .unwrap_or_default()["error"]["code"],
        "unsupported_media_type"
    );

    let mut cross_origin = TestClient::post("http://localhost/api/console/result/accept")
        .bearer_auth(&fixture.credential)
        .json(&serde_json::json!({ "task_id": "wc_task_aa11223344556677889900aabbccddee" }))
        .add_header("origin", "http://attacker.example", true)
        .send(&fixture.service)
        .await;
    assert_eq!(response_status(&cross_origin), StatusCode::FORBIDDEN);
    assert_eq!(
        cross_origin
            .take_json::<serde_json::Value>()
            .await
            .unwrap_or_default()["error"]["code"],
        "cross_origin_denied"
    );
    let mut response = TestClient::post("http://localhost/api/console/tasks")
        .bearer_auth(&fixture.credential)
        .raw_json("{ this is not valid json")
        .send(&fixture.service)
        .await;
    assert_eq!(response_status(&response), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .take_json::<serde_json::Value>()
            .await
            .unwrap_or_default()["error"]["code"],
        "invalid_arguments"
    );
    let (status, body) = post_connector(
        &fixture,
        "/api/console/tasks",
        &fixture.credential,
        serde_json::json!({ "include_completed": false, "smuggled": true }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_arguments");
    let response = TestClient::post("http://localhost/api/console/not-allowlisted")
        .bearer_auth(&fixture.credential)
        .json(&serde_json::json!({}))
        .send(&fixture.service)
        .await;
    assert_eq!(response_status(&response), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn console_accept_requires_result_id_and_stale_identity_has_no_effect() {
    let fixture = authenticated_project_fixture().await;
    let task_id = seed_ready_console(
        &fixture,
        "wc_task_dd11223344556677889900aabbccddee",
        "wc_run_cd",
        2,
    );

    let (missing_status, missing) = post_connector(
        &fixture,
        "/api/console/result/accept",
        &fixture.credential,
        serde_json::json!({ "task_id": task_id }),
    )
    .await;
    assert_eq!(missing_status, StatusCode::BAD_REQUEST);
    assert_eq!(missing["error"]["code"], "invalid_arguments");

    let (stale_status, stale) = post_connector(
        &fixture,
        "/api/console/result/accept",
        &fixture.credential,
        serde_json::json!({ "task_id": task_id, "result_id": "wc_result_stale00000" }),
    )
    .await;
    assert_eq!(stale_status, StatusCode::CONFLICT);
    assert_eq!(stale["error"]["code"], "result_changed");
    assert_eq!(stale["error"]["retryable"], false);
    assert_eq!(stale["error"]["user_action_required"], true);
    assert_eq!(
        fs::read_to_string(fixture.root.join("README.md")).unwrap(),
        "fixture\n"
    );
    let task = fixture
        .db
        .local_connector_task(&task_id, &fixture.connector.context().project_id)
        .unwrap();
    assert_eq!(
        task.task_status,
        webcodex_store::ConnectorTaskState::ReadyForReview
    );
}

#[test]
fn console_is_not_a_model_facing_capability() {
    let names = crate::connector_runtime::surface::CAPABILITY_NAMES;
    assert_eq!(names.len(), 14);
    assert!(names
        .iter()
        .all(|name| !crate::connector_runtime::surface::route_for(name)
            .is_some_and(|route| route.starts_with("/api/console/"))));
}

#[test]
fn gitignore_hygiene_fact_grades_setup_before_the_first_task() {
    let (_temp, root, _state) = repo("hygiene");
    // The fixture repo has a clean status but no .gitignore: warn about the
    // gap before build artifacts poison workspace provenance.
    let missing = gitignore_hygiene_fact(&root);
    assert_eq!(missing.status, ReadinessStatus::Warn);
    assert_eq!(missing.code, "gitignore_missing");

    // Untracked build artifacts outrank the missing file: they are the exact
    // provenance poison, and the summary names them.
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join("target/junk.o"), "x").unwrap();
    let poisoned = gitignore_hygiene_fact(&root);
    assert_eq!(poisoned.status, ReadinessStatus::Warn);
    assert_eq!(poisoned.code, "untracked_build_artifacts");
    assert!(poisoned.summary.contains("target/"), "{}", poisoned.summary);

    // A .gitignore covering the artifacts settles both complaints.
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    git(&["add", ".gitignore"], &root);
    let clean = gitignore_hygiene_fact(&root);
    assert_eq!(clean.status, ReadinessStatus::Pass);
    assert_eq!(clean.code, "gitignore_present");
}
