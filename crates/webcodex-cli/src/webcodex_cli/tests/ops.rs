use super::support::*;
use crate::webcodex_cli::ops::{
    ops_exit_code, ops_projects_report, ops_runner_report, ops_runners_report,
    ops_smoke_preflight_report, ops_status_report, render_ops_runner, render_ops_status,
};
use crate::webcodex_cli::run_ops_command;
use std::time::Duration;

#[test]
fn ops_help_entrypoints_print_usage() {
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["ops", "--help"],
            &[
                "Usage: webcodex ops <COMMAND>",
                "status",
                "runners",
                "projects",
                "windows",
                "smoke-preflight",
                "--server-url URL",
                "--token TOKEN",
            ],
        ),
        (
            &["ops", "status", "--help"],
            &[
                "Usage: webcodex ops status",
                "--server-url URL",
                "--env-file PATH",
                "--token-file PATH",
                "--token TOKEN",
                "--json",
                "--strict",
            ],
        ),
        (
            &["ops", "runners", "--help"],
            &[
                "Usage: webcodex ops runners",
                "--server-url URL",
                "--env-file PATH",
                "--token-file PATH",
                "--token TOKEN",
                "--json",
                "--strict",
            ],
        ),
        (
            &["ops", "runner", "--help"],
            &[
                "Usage: webcodex ops runner",
                "--client-id CLIENT_ID",
                "--request-timeout-ms MS",
                "--server-url URL",
                "--token-file PATH",
                "--json",
                "--strict",
            ],
        ),
        (
            &["ops", "projects", "--help"],
            &[
                "Usage: webcodex ops projects",
                "--server-url URL",
                "--env-file PATH",
                "--token-file PATH",
                "--token TOKEN",
                "--json",
                "--strict",
            ],
        ),
        (
            &["ops", "windows", "--help"],
            &[
                "Usage: webcodex ops windows",
                "--project PROJECT_ID",
                "--limit COUNT",
                "--server-url URL",
                "--token-file PATH",
                "--json",
                "--strict",
            ],
        ),
        (
            &["ops", "smoke-preflight", "--help"],
            &[
                "Usage: webcodex ops smoke-preflight",
                "--project PROJECT_ID",
                "--server-url URL",
                "--env-file PATH",
                "--token-file PATH",
                "--token TOKEN",
                "--json",
                "--strict",
            ],
        ),
    ];

    for (args, expected) in cases {
        let out = cli_exit(args.iter().copied())
            .unwrap_or_else(|err| panic!("expected {args:?} help to exit successfully: {err}"));
        for needle in *expected {
            assert!(
                out.contains(needle),
                "help for {args:?} did not contain {needle:?}\n{out}"
            );
        }
    }
}

#[test]
fn ops_runners_is_canonical_and_agents_is_rejected() {
    match cli_action(["ops", "runners", "--json"]) {
        CliAction::Ops(OpsCommand::Runners(opts)) => assert!(opts.json),
        other => panic!("canonical ops runners path did not parse: {other:?}"),
    }
    match cli_action(["ops", "agents", "--json"]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(
                stderr.contains("unknown ops subcommand: agents"),
                "{stderr}"
            );
        }
        other => panic!("legacy ops agents alias must be rejected: {other:?}"),
    }
}

#[test]
fn top_level_help_mentions_ops() {
    let out = cli_exit(["--help"]).unwrap();
    assert!(out
        .lines()
        .any(|line| line.trim_start().starts_with("ops ")));
}

#[test]
fn ops_unknown_subcommand_is_clear() {
    match cli_action(["ops", "unknown"]) {
        CliAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            assert_eq!(code, 2);
            assert!(stdout.is_empty());
            assert!(stderr.contains("unknown ops subcommand: unknown"));
        }
        other => panic!("expected unknown ops subcommand exit, got {other:?}"),
    }
}

#[test]
fn ops_common_flags_parse_without_printing_token() {
    match cli_action([
        "ops",
        "status",
        "--server-url",
        "http://runtime.example",
        "--env-file",
        "/tmp/webcodex.env",
        "--token-file",
        "/tmp/token",
        "--token",
        "secret-token-value",
        "--json",
        "--strict",
    ]) {
        CliAction::Ops(OpsCommand::Status(opts)) => {
            assert_eq!(opts.server_url, "http://runtime.example");
            assert_eq!(
                opts.env_file.as_deref(),
                Some(Path::new("/tmp/webcodex.env"))
            );
            assert_eq!(opts.token_file.as_deref(), Some(Path::new("/tmp/token")));
            assert_eq!(opts.token.as_deref(), Some("secret-token-value"));
            assert!(opts.json);
            assert!(opts.strict);
        }
        other => panic!("expected ops status action, got {other:?}"),
    }
}

#[test]
fn ops_rejects_removed_server_url_alias() {
    match cli_action(["ops", "status", "--url", "http://runtime.example"]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(
                stderr.contains("unknown ops status flag: --url"),
                "{stderr}"
            );
        }
        other => panic!("removed --url alias still dispatched: {other:?}"),
    }
}

#[test]
fn ops_windows_requires_project_and_bounds_limit() {
    match cli_action(["ops", "windows", "--json"]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(stderr.contains("--project is required"), "{stderr}");
        }
        other => panic!("missing ops windows project should fail: {other:?}"),
    }
    match cli_action([
        "ops",
        "windows",
        "--project",
        "agent:msi:site",
        "--limit",
        "65",
    ]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(stderr.contains("--limit must be within 1..=64"), "{stderr}");
        }
        other => panic!("oversized ops windows limit should fail: {other:?}"),
    }
    match cli_action([
        "ops",
        "windows",
        "--project",
        "agent:msi:site",
        "--limit",
        "7",
        "--json",
    ]) {
        CliAction::Ops(OpsCommand::Windows(opts)) => {
            assert_eq!(opts.project, "agent:msi:site");
            assert_eq!(opts.limit, 7);
            assert!(opts.common.json);
        }
        other => panic!("ops windows did not parse: {other:?}"),
    }
}

#[test]
fn ops_smoke_preflight_requires_project() {
    match cli_action(["ops", "smoke-preflight", "--json"]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(stderr.contains("--project is required"));
        }
        other => panic!("expected missing project exit, got {other:?}"),
    }
}

#[test]
fn ops_runner_requires_exact_client_id() {
    match cli_action(["ops", "runner", "--json"]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(stderr.contains("--client-id is required"));
        }
        other => panic!("expected missing client-id exit, got {other:?}"),
    }

    match cli_action([
        "ops",
        "runner",
        "--client-id",
        "msi",
        "--server-url",
        "https://runtime.example",
        "--token-file",
        "/tmp/user-token",
        "--json",
    ]) {
        CliAction::Ops(OpsCommand::Runner(opts)) => {
            assert_eq!(opts.client_id, "msi");
            assert_eq!(opts.request_timeout_ms, 5_000);
            assert_eq!(opts.common.server_url, "https://runtime.example");
            assert_eq!(
                opts.common.token_file.as_deref(),
                Some(Path::new("/tmp/user-token"))
            );
            assert!(opts.common.json);
        }
        other => panic!("expected ops runner action, got {other:?}"),
    }
}

#[test]
fn ops_parser_errors_do_not_leak_token_value() {
    let secret = "secret-token-value";
    match cli_action(["ops", "status", "--token", secret, "--bad-flag"]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(stderr.contains("unknown ops status flag: --bad-flag"));
            assert!(!stderr.contains(secret));
        }
        other => panic!("expected parser error, got {other:?}"),
    }
}

#[tokio::test]
async fn ops_rejects_agent_token_from_env_file_without_leaking_it() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("webcodex.env");
    let secret = "wc_agent_do_not_echo_ops_env_file_0123456789";
    std::fs::write(&env_file, format!("WEBCODEX_TOKEN={secret}\n")).unwrap();
    let mut opts = ops_common_opts("http://127.0.0.1:1".to_string());
    opts.env_file = Some(env_file);

    let error = run_ops_command(OpsCommand::Status(opts)).await.unwrap_err();
    assert!(error.contains("Runner transport token"), "{error}");
    assert!(error.contains("webcodex-user-token"), "{error}");
    assert!(!error.contains(secret));
}

// `WEBCODEX_TOKEN` from the process env is the tested product behavior: it
// must stay set (and serialized against other env-mutating tests) for the
// whole async operation, so the env lock is held across the awaits by contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn ops_rejects_agent_token_from_process_env_without_leaking_it() {
    let _guard = env_test_guard();
    let secret = "wc_agent_do_not_echo_ops_process_env_0123456789";
    let _env = EnvGuard::new().set("WEBCODEX_TOKEN", secret);
    let opts = ops_common_opts("http://127.0.0.1:1".to_string());
    let error = run_ops_command(OpsCommand::Status(opts)).await.unwrap_err();
    assert!(error.contains("Runner transport token"), "{error}");
    assert!(error.contains("webcodex-user-token"), "{error}");
    assert!(!error.contains(secret));
}

#[tokio::test]
async fn ops_status_http_401_reports_auth_required_not_runtime_unreachable() {
    let output = run_ops_with_routes(
        OpsCommand::Status(ops_common_opts(String::new())),
        vec![(
            "/api/runtime/status",
            json_http_response(401, json!({"error": "missing token"})),
        )],
    )
    .await;
    assert!(output.contains("Overall: FAIL"), "{output}");
    assert!(output.contains("auth_required"), "{output}");
    assert!(output.contains("status: 401"), "{output}");
    assert!(!output.contains("runtime_unreachable"), "{output}");
}

#[tokio::test]
async fn ops_projects_http_401_uses_ops_report() {
    let output = run_ops_with_routes(
        OpsCommand::Projects(ops_common_opts(String::new())),
        vec![(
            "/api/projects/list",
            json_http_response(401, json!({"error": "missing token"})),
        )],
    )
    .await;
    assert!(output.contains("Overall: FAIL"), "{output}");
    assert!(output.contains("auth_required"), "{output}");
    assert!(output.contains("status: 401"), "{output}");
    assert!(!output.contains("projects list failed"), "{output}");
}

#[tokio::test]
async fn ops_smoke_preflight_projects_401_uses_ops_report() {
    let output = run_ops_with_routes(
        OpsCommand::SmokePreflight(OpsSmokePreflightOptions {
            common: ops_common_opts(String::new()),
            project: "agent:ops:smoke".to_string(),
        }),
        vec![
            (
                "/api/runtime/status",
                json_http_response(
                    200,
                    json!({"success": true, "output": runtime_status_fixture()}),
                ),
            ),
            (
                "/api/projects/list",
                json_http_response(401, json!({"error": "missing token"})),
            ),
        ],
    )
    .await;
    assert!(output.contains("Overall: FAIL"), "{output}");
    assert!(output.contains("auth_required"), "{output}");
    assert!(output.contains("endpoint: list_projects"), "{output}");
    assert!(!output.contains("projects list failed"), "{output}");
}

#[tokio::test]
async fn ops_http_403_reports_forbidden() {
    let mut opts = ops_common_opts(String::new());
    opts.token = Some("test-token".to_string());
    let output = run_ops_with_routes(
        OpsCommand::Status(opts),
        vec![(
            "/api/runtime/status",
            json_http_response(403, json!({"error": "forbidden"})),
        )],
    )
    .await;
    assert!(output.contains("Overall: FAIL"), "{output}");
    assert!(output.contains("forbidden"), "{output}");
    assert!(output.contains("status: 403"), "{output}");
    assert!(!output.contains("runtime_unreachable"), "{output}");
}

#[tokio::test]
async fn ops_connection_failure_reports_runtime_unreachable() {
    let (addr, handle) = spawn_connection_drop_server();
    let output = run_ops_command(OpsCommand::Status(ops_common_opts(format!(
        "http://{addr}"
    ))))
    .await
    .unwrap()
    .stdout;
    handle.join().unwrap();
    assert!(output.contains("Overall: FAIL"), "{output}");
    assert!(output.contains("runtime_unreachable"), "{output}");
}

#[test]
fn ops_strict_exit_code_follows_report_status() {
    let pass = ops_status_report("https://ops.example.test", &Some(runtime_status_fixture()));
    assert_eq!(pass.verdict.status, "pass");
    assert_eq!(ops_exit_code(true, pass.verdict.status), 0);

    let mut warn_runtime = runtime_status_fixture();
    warn_runtime["jobs"]["active_count"] = json!(1);
    let warn = ops_status_report("https://ops.example.test", &Some(warn_runtime));
    assert_eq!(warn.verdict.status, "warn");
    assert_eq!(ops_exit_code(true, warn.verdict.status), 0);

    let mut fail_runtime = runtime_status_fixture();
    fail_runtime["agents"]["online_count"] = json!(0);
    fail_runtime["agents"]["summary"]["online"] = json!(0);
    let fail = ops_status_report("https://ops.example.test", &Some(fail_runtime));
    assert_eq!(fail.verdict.status, "fail");
    assert_eq!(ops_exit_code(true, fail.verdict.status), 2);
    assert_eq!(ops_exit_code(false, fail.verdict.status), 0);
}

#[tokio::test]
async fn ops_http_error_output_does_not_leak_token_value() {
    let secret = "secret-token-value";
    let mut opts = ops_common_opts(String::new());
    opts.token = Some(secret.to_string());
    let output = run_ops_with_routes(
        OpsCommand::Status(opts),
        vec![(
            "/api/runtime/status",
            json_http_response(401, json!({"error": format!("bad token {secret}")})),
        )],
    )
    .await;
    assert!(output.contains("Overall: FAIL"), "{output}");
    assert!(output.contains("unauthorized"), "{output}");
    assert!(!output.contains(secret), "{output}");
}

fn runtime_status_fixture() -> Value {
    json!({
        "service": "webcodex",
        "version": "0.2.0",
        "build": {
            "git_commit": "15138884e3a8ddcf294cae98183ecaac37af7230",
            "git_dirty": false
        },
        "tools": {
            "count": 66
        },
        "jobs": {
            "active_count": 0
        },
        "agents": {
            "online_count": 1,
            "stale_count": 0,
            "summary": {
                "online": 1,
                "offline": 0,
                "stale": 0,
                "clients": [
                    {
                        "client_id": "ops-agent",
                        "status": "online",
                        "transport": "websocket",
                        "projects_count": 1,
                        "active_jobs": 0,
                        "pending_requests": 0,
                        "last_seen_age_secs": 2
                    }
                ]
            }
        },
        "projects": {
            "effective": {
                "status": "ok",
                "count": 1
            }
        }
    })
}

fn runner_runtime_status_fixture() -> Value {
    json!({
        "service": "webcodex",
        "build": {
            "git_commit": "server123456",
            "git_dirty": false
        },
        "focus": {
            "client_id": "msi",
            "connected": true,
            "status": "online",
            "agent_instance_id": "instance-new",
            "build": {
                "version": "0.3.8",
                "git_commit": "candidate1234",
                "git_dirty": false,
                "built_at": "1787554000"
            },
            "compatibility_status": "compatible",
            "source_alignment": {
                "status": "different",
                "git_commit_matches_server": false
            }
        }
    })
}

fn projects_fixture(recommended: bool) -> Value {
    json!({
        "count": 1,
        "recommended_for_smoke": if recommended { json!(["agent:ops:smoke"]) } else { json!([]) },
        "projects": [
            {
                "id": "agent:ops:smoke",
                "client_id": "ops",
                "agent_status": "online",
                "connected": true,
                "allow_patch": true,
                "path": "/srv/webcodex-smoke",
                "capabilities": {
                    "git_available": true,
                    "safe_smoke_project": true,
                    "recommended_for_smoke": recommended
                }
            }
        ]
    })
}

fn clean_show_changes_fixture() -> Value {
    json!({
        "clean": true,
        "git_available": true,
        "verdict": {
            "status": "pass",
            "blocking": false,
            "blocking_reasons": [],
            "warning_reasons": [],
            "suggested_next_actions": ["no action needed"]
        }
    })
}

fn clean_hygiene_fixture() -> Value {
    json!({
        "clean": true,
        "git_available": true,
        "counts": {
            "findings": 0,
            "low": 0
        },
        "verdict": {
            "status": "pass",
            "blocking": false,
            "blocking_reasons": [],
            "warning_reasons": [],
            "suggested_next_actions": ["no action needed"]
        }
    })
}

fn ops_common_opts(server_url: String) -> OpsCommonOptions {
    OpsCommonOptions {
        server_url,
        server_http: direct_server_http(),
        env_file: None,
        token_file: None,
        token: None,
        json: false,
        strict: false,
    }
}

#[derive(Clone)]
struct OpsHttpResponse {
    status: u16,
    content_type: String,
    body: String,
}

fn json_http_response(status: u16, body: Value) -> OpsHttpResponse {
    OpsHttpResponse {
        status,
        content_type: "application/json".to_string(),
        body: serde_json::to_string(&body).unwrap(),
    }
}

fn spawn_ops_route_server(
    routes: Vec<(&'static str, OpsHttpResponse)>,
) -> (
    String,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<Vec<String>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // On Windows an accepted socket inherits the listener's
                    // non-blocking mode; switch back to blocking so the read
                    // waits for the full request instead of panicking with
                    // WSAEWOULDBLOCK when the request has not fully arrived.
                    stream.set_nonblocking(false).unwrap();
                    let mut buf = [0u8; 16384];
                    let n = stream.read(&mut buf).unwrap();
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    requests.push(request.clone());
                    let first_line = request.lines().next().unwrap_or_default();
                    let response = routes
                        .iter()
                        .find(|(path, _)| first_line.starts_with(&format!("POST {path} ")))
                        .map(|(_, response)| response.clone())
                        .unwrap_or_else(|| {
                            json_http_response(404, json!({"error": "unexpected request"}))
                        });
                    write!(
                        stream,
                        "HTTP/1.1 {} OK\r\ncontent-type: {}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                        response.status,
                        response.content_type,
                        response.body.len(),
                        response.body
                    )
                    .unwrap();
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("fake ops route server accept failed: {err}"),
            }
        }
        requests
    });
    (format!("http://{}", addr), stop_tx, handle)
}

// Route fixtures may intentionally exercise the unauthenticated 401 contract.
// Serialize them with process-env credential tests and remove both ambient
// user/API credential aliases while preserving explicit per-command tokens.
#[allow(clippy::await_holding_lock)]
async fn run_ops_with_routes(
    command: OpsCommand,
    routes: Vec<(&'static str, OpsHttpResponse)>,
) -> String {
    let _env_guard = env_test_guard();
    let _env = EnvGuard::new()
        .remove("WEBCODEX_TOKEN")
        .remove("WEBCODEX_PAT");
    let (server_url, stop_tx, handle) = spawn_ops_route_server(routes);
    let command = match command {
        OpsCommand::Status(mut opts) => {
            opts.server_url = server_url;
            OpsCommand::Status(opts)
        }
        OpsCommand::Runners(mut opts) => {
            opts.server_url = server_url;
            OpsCommand::Runners(opts)
        }
        OpsCommand::Runner(mut opts) => {
            opts.common.server_url = server_url;
            OpsCommand::Runner(opts)
        }
        OpsCommand::Projects(mut opts) => {
            opts.server_url = server_url;
            OpsCommand::Projects(opts)
        }
        OpsCommand::Windows(mut opts) => {
            opts.common.server_url = server_url;
            OpsCommand::Windows(opts)
        }
        OpsCommand::SmokePreflight(mut opts) => {
            opts.common.server_url = server_url;
            OpsCommand::SmokePreflight(opts)
        }
    };
    let output = run_ops_command(command).await.unwrap().stdout;
    stop_tx.send(()).unwrap();
    handle.join().unwrap();
    output
}

#[tokio::test]
async fn ops_windows_reports_project_scoped_chatgpt_observation() {
    let output = run_ops_with_routes(
        OpsCommand::Windows(OpsWindowsOptions {
            common: ops_common_opts(String::new()),
            project: "agent:msi:site".to_string(),
            limit: 8,
        }),
        vec![(
            "/api/runtime-console/windows",
            json_http_response(
                200,
                json!({
                    "success": true,
                    "output": {
                        "returned": 1,
                        "total": 1,
                        "truncated": false,
                        "windows": [{
                            "client_window_key": "a".repeat(64),
                            "source": "openai-session",
                            "last_seen_at_ms": 1234,
                            "last_tool_call_at_ms": 1234,
                            "last_meaningful_activity_at_ms": 1234,
                            "active_count": 0,
                            "linked_session_count": 1,
                            "recorder_gap_count": 0
                        }]
                    }
                }),
            ),
        )],
    )
    .await;
    assert!(output.contains("Overall: PASS"), "{output}");
    assert!(output.contains("returned: 1"), "{output}");
    assert!(
        output.contains("last_meaningful_activity_at_ms=1234"),
        "{output}"
    );
}

fn smoke_preflight_opts(server_url: String, project: &str) -> OpsSmokePreflightOptions {
    OpsSmokePreflightOptions {
        common: OpsCommonOptions {
            server_url,
            server_http: direct_server_http(),
            env_file: None,
            token_file: None,
            token: Some("secret-smoke-token".to_string()),
            json: false,
            strict: false,
        },
        project: project.to_string(),
    }
}

fn spawn_smoke_preflight_server(
    projects: Value,
) -> (
    String,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<Vec<String>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // See spawn_ops_route_server: restore blocking mode on
                    // Windows where accepted sockets inherit non-blocking.
                    stream.set_nonblocking(false).unwrap();
                    let mut buf = [0u8; 16384];
                    let n = stream.read(&mut buf).unwrap();
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    requests.push(request.clone());
                    let first_line = request.lines().next().unwrap_or_default().to_string();
                    let body = if first_line.starts_with("POST /api/runtime/status ") {
                        json!({"success": true, "output": runtime_status_fixture()})
                    } else if first_line.starts_with("POST /api/projects/list ") {
                        json!({"success": true, "output": projects.clone()})
                    } else if request.contains(r#""tool":"show_changes""#) {
                        json!({"success": true, "output": clean_show_changes_fixture()})
                    } else if request.contains(r#""tool":"workspace_hygiene_check""#) {
                        json!({"success": true, "output": clean_hygiene_fixture()})
                    } else {
                        json!({"success": false, "error": "unexpected request"})
                    };
                    let body = serde_json::to_string(&body).unwrap();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("fake ops server accept failed: {err}"),
            }
        }
        requests
    });
    (format!("http://{}", addr), stop_tx, handle)
}

async fn run_smoke_preflight_with_projects(
    projects: Value,
    project: &str,
) -> (String, Vec<String>) {
    let (server_url, stop_tx, handle) = spawn_smoke_preflight_server(projects);
    let output = run_ops_command(OpsCommand::SmokePreflight(smoke_preflight_opts(
        server_url, project,
    )))
    .await
    .unwrap()
    .stdout;
    stop_tx.send(()).unwrap();
    let requests = handle.join().unwrap();
    (output, requests)
}

fn smoke_request_kinds(requests: &[String]) -> Vec<&'static str> {
    requests
        .iter()
        .map(|request| {
            let first_line = request.lines().next().unwrap_or_default();
            if first_line.starts_with("POST /api/runtime/status ") {
                "runtime_status"
            } else if first_line.starts_with("POST /api/projects/list ") {
                "projects_list"
            } else if request.contains(r#""tool":"show_changes""#) {
                "show_changes"
            } else if request.contains(r#""tool":"workspace_hygiene_check""#) {
                "workspace_hygiene_check"
            } else {
                "unexpected"
            }
        })
        .collect()
}

fn assert_no_workspace_preflight_tools(requests: &[String]) {
    let joined = requests.join("\n---\n");
    assert!(!joined.contains(r#""tool":"show_changes""#));
    assert!(!joined.contains(r#""tool":"workspace_hygiene_check""#));
}

#[test]
fn ops_status_runtime_ok_passes() {
    let runtime = Some(runtime_status_fixture());
    let report = ops_status_report("https://ops.example.test", &runtime);
    assert_eq!(report.verdict.status, "pass");
    assert_eq!(report.summary["tools"]["count"], 66);
    assert_eq!(
        report.source["runtime_commit"],
        "15138884e3a8ddcf294cae98183ecaac37af7230"
    );
}

#[test]
fn ops_status_tool_inventory_accepts_different_release_sizes() {
    for count in [1_u64, 47, 66, 135, 200] {
        let mut runtime = runtime_status_fixture();
        runtime["tools"] = json!({"count": count});
        let report = ops_status_report("https://ops.example.test", &Some(runtime.clone()));
        assert_eq!(report.verdict.status, "pass", "compact count {count}");
        runtime["tools"]["names"] =
            json!((0..count).map(|i| format!("tool_{i}")).collect::<Vec<_>>());
        let report = ops_status_report("https://ops.example.test", &Some(runtime));
        assert_eq!(report.verdict.status, "pass", "full count {count}");
        assert!(report.verdict.warning_reasons.is_empty());
    }
}

#[test]
fn ops_status_tool_inventory_rejects_missing_empty_or_inconsistent_data() {
    for tools in [
        Value::Null,
        json!({}),
        json!({"count": 0}),
        json!({"count": -1}),
        json!({"count": "135"}),
        json!({"count": 1.5}),
        json!({"count": 2, "names": ["one"]}),
        json!({"count": 2, "names": ["one", "one"]}),
        json!({"count": 1, "names": [""]}),
        json!({"count": 1, "names": [" "]}),
        json!({"count": 1, "names": [42]}),
        json!({"count": 1, "names": null}),
        json!({"count": 1, "names": "one"}),
    ] {
        let mut runtime = runtime_status_fixture();
        runtime["tools"] = tools;
        let report = ops_status_report("https://ops.example.test", &Some(runtime));
        assert_eq!(report.verdict.status, "fail");
        assert!(report.verdict.blocking);
        assert!(report
            .verdict
            .blocking_reasons
            .contains(&"malformed_tool_inventory".to_string()));
        assert_eq!(ops_exit_code(true, report.verdict.status), 2);
    }
}

#[test]
fn ops_status_no_online_agents_fails() {
    let mut runtime = runtime_status_fixture();
    runtime["agents"]["online_count"] = json!(0);
    runtime["agents"]["stale_count"] = json!(1);
    runtime["agents"]["summary"]["online"] = json!(0);
    runtime["agents"]["summary"]["stale"] = json!(1);
    runtime["agents"]["summary"]["clients"][0]["status"] = json!("stale");
    let report = ops_status_report("https://ops.example.test", &Some(runtime));
    assert_eq!(report.verdict.status, "fail");
    assert!(report
        .verdict
        .blocking_reasons
        .contains(&"no_online_agents".to_string()));
}

#[test]
fn ops_status_active_jobs_warns() {
    let mut runtime = runtime_status_fixture();
    runtime["jobs"]["active_count"] = json!(2);
    let report = ops_status_report("https://ops.example.test", &Some(runtime));
    assert_eq!(report.verdict.status, "warn");
    assert!(report
        .verdict
        .warning_reasons
        .contains(&"active_jobs:2".to_string()));
}

#[test]
fn ops_runners_maps_online_stale_and_jobs() {
    let mut runtime = runtime_status_fixture();
    runtime["agents"]["online_count"] = json!(1);
    runtime["agents"]["stale_count"] = json!(1);
    runtime["agents"]["summary"]["online"] = json!(1);
    runtime["agents"]["summary"]["stale"] = json!(1);
    runtime["agents"]["summary"]["clients"] = json!([
        {
            "client_id": "online",
            "status": "online",
            "transport": "quic",
            "projects_count": 2,
            "active_jobs": 1,
            "pending_requests": 0,
            "last_seen_age_secs": 1
        },
        {
            "client_id": "stale",
            "status": "stale",
            "transport": "polling",
            "projects_count": 1,
            "active_jobs": 0,
            "pending_requests": 1,
            "last_seen_age_secs": 120
        }
    ]);
    let report = ops_runners_report("https://ops.example.test", &Some(runtime));
    assert_eq!(report.verdict.status, "warn");
    assert_eq!(report.summary["online_count"], 1);
    assert_eq!(report.summary["stale_count"], 1);
    assert!(report.summary.get("offline_count").is_none());
    assert_eq!(report.summary["active_jobs"], 1);
}

#[test]
fn ops_runner_projects_only_exact_safe_runtime_identity() {
    let secret = "wc_pat_projection_must_not_leak_0123456789";
    let mut runtime = runner_runtime_status_fixture();
    runtime["credential_material"] = json!(secret);
    runtime["focus"]["credential_material"] = json!(secret);
    let report = ops_runner_report("https://ops.example.test", "msi", &Some(runtime));
    assert_eq!(report.verdict.status, "pass");
    assert_eq!(report.summary["client_id"], "msi");
    assert_eq!(report.summary["connected"], true);
    assert_eq!(report.summary["agent_instance_id"], "instance-new");
    assert_eq!(report.summary["build"]["git_commit"], "candidate1234");
    assert_eq!(report.summary["build"]["git_dirty"], false);
    assert_eq!(report.summary["source_alignment"]["status"], "different");
    let rendered = render_ops_runner(&report, true).unwrap();
    assert!(!rendered.contains(secret));
    assert!(!rendered.contains("credential_material"));
}

#[tokio::test]
async fn ops_runner_queries_exact_client_id_without_echoing_token() {
    let secret = "wc_pat_runner_query_secret_0123456789";
    let (server_url, stop_tx, handle) = spawn_ops_route_server(vec![(
        "/api/runtime/status",
        json_http_response(
            200,
            json!({"success": true, "output": runner_runtime_status_fixture()}),
        ),
    )]);
    let mut common = ops_common_opts(server_url);
    common.token = Some(secret.to_string());
    common.json = true;
    let output = run_ops_command(OpsCommand::Runner(OpsRunnerOptions {
        common,
        client_id: "msi".to_string(),
        request_timeout_ms: 5_000,
    }))
    .await
    .unwrap()
    .stdout;
    stop_tx.send(()).unwrap();
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains(r#""client_id":"msi""#));
    assert!(!output.contains(secret));
    assert!(output.contains("instance-new"));
    assert!(output.contains("candidate1234"));
}

#[test]
fn ops_projects_no_recommended_smoke_warns() {
    let projects = projects_fixture(false);
    let report = ops_projects_report("https://ops.example.test", Some(&projects));
    assert_eq!(report.verdict.status, "warn");
    assert!(report
        .verdict
        .warning_reasons
        .contains(&"no_recommended_smoke_project".to_string()));
}

#[test]
fn ops_projects_disconnected_and_stale_warns() {
    let projects = json!({
        "count": 3,
        "recommended_for_smoke": ["agent:ops:smoke"],
        "projects": [
            {
                "id": "agent:ops:smoke",
                "client_id": "ops",
                "agent_status": "online",
                "connected": true,
                "allow_patch": true,
                "path": "/srv/webcodex-smoke",
                "capabilities": {
                    "git_available": true,
                    "safe_smoke_project": true,
                    "recommended_for_smoke": true
                }
            },
            {
                "id": "agent:ops:disconnected",
                "client_id": "ops",
                "agent_status": "online",
                "connected": false,
                "allow_patch": true,
                "path": "/srv/webcodex-disconnected",
                "capabilities": {
                    "git_available": true,
                    "safe_smoke_project": false,
                    "recommended_for_smoke": false
                }
            },
            {
                "id": "agent:ops:stale",
                "client_id": "stale",
                "agent_status": "stale",
                "connected": true,
                "allow_patch": true,
                "path": "/srv/webcodex-stale",
                "capabilities": {
                    "git_available": true,
                    "safe_smoke_project": false,
                    "recommended_for_smoke": false
                }
            }
        ]
    });
    let report = ops_projects_report("https://ops.example.test", Some(&projects));
    assert_eq!(report.verdict.status, "warn");
    assert!(report
        .verdict
        .warning_reasons
        .contains(&"disconnected_projects:1".to_string()));
    assert!(report
        .verdict
        .warning_reasons
        .contains(&"stale_projects:1".to_string()));
}

#[test]
fn ops_projects_recommended_smoke_offline_warns() {
    let projects = json!({
        "count": 2,
        "recommended_for_smoke": ["agent:ops:offline-smoke"],
        "projects": [
            {
                "id": "agent:ops:online",
                "client_id": "ops",
                "agent_status": "online",
                "connected": true,
                "allow_patch": true,
                "path": "/srv/webcodex-online",
                "capabilities": {
                    "git_available": true,
                    "safe_smoke_project": false,
                    "recommended_for_smoke": false
                }
            },
            {
                "id": "agent:ops:offline-smoke",
                "client_id": "special",
                "agent_status": "stale",
                "connected": false,
                "allow_patch": true,
                "path": "/srv/webcodex-offline-smoke",
                "capabilities": {
                    "git_available": true,
                    "safe_smoke_project": true,
                    "recommended_for_smoke": true
                }
            }
        ]
    });
    let report = ops_projects_report("https://ops.example.test", Some(&projects));
    assert_eq!(report.verdict.status, "warn");
    assert!(report
        .verdict
        .warning_reasons
        .contains(&"recommended_smoke_offline:1".to_string()));
}

#[test]
fn ops_smoke_preflight_clean_project_passes() {
    let runtime = runtime_status_fixture();
    let projects = projects_fixture(true);
    let show_changes = clean_show_changes_fixture();
    let hygiene = clean_hygiene_fixture();
    let report = ops_smoke_preflight_report(
        "https://ops.example.test",
        "agent:ops:smoke",
        Some(&runtime),
        Some(&projects),
        Some(&show_changes),
        Some(&hygiene),
    );
    assert_eq!(report.verdict.status, "pass");
}

#[test]
fn ops_smoke_preflight_dirty_workspace_fails() {
    let runtime = runtime_status_fixture();
    let projects = projects_fixture(true);
    let mut show_changes = clean_show_changes_fixture();
    show_changes["clean"] = json!(false);
    show_changes["verdict"]["status"] = json!("fail");
    let hygiene = clean_hygiene_fixture();
    let report = ops_smoke_preflight_report(
        "https://ops.example.test",
        "agent:ops:smoke",
        Some(&runtime),
        Some(&projects),
        Some(&show_changes),
        Some(&hygiene),
    );
    assert_eq!(report.verdict.status, "fail");
    assert!(report
        .verdict
        .blocking_reasons
        .contains(&"workspace_dirty".to_string()));
}

#[test]
fn ops_smoke_preflight_online_non_recommended_project_warns() {
    let runtime = runtime_status_fixture();
    let projects = projects_fixture(false);
    let show_changes = clean_show_changes_fixture();
    let hygiene = clean_hygiene_fixture();
    let report = ops_smoke_preflight_report(
        "https://ops.example.test",
        "agent:ops:smoke",
        Some(&runtime),
        Some(&projects),
        Some(&show_changes),
        Some(&hygiene),
    );
    assert_eq!(report.verdict.status, "warn");
    assert!(report
        .verdict
        .warning_reasons
        .contains(&"project_not_recommended_for_smoke".to_string()));
}

#[test]
fn ops_json_and_human_outputs_do_not_contain_secret_values() {
    let secret = "secret-token-value";
    let mut runtime = runtime_status_fixture();
    runtime["agents"]["summary"]["clients"][0]["client_id"] = json!("safe-agent");
    let report = ops_status_report("https://ops.example.test", &Some(runtime));
    let json_output = render_ops_status(&report, true).unwrap();
    let human_output = render_ops_status(&report, false).unwrap();
    assert!(!json_output.contains(secret));
    assert!(!json_output.contains("WEBCODEX_TOKEN="));
    assert!(!human_output.contains(secret));
    assert!(!human_output.contains("WEBCODEX_TOKEN="));
    assert!(human_output.contains("online/stale: 1/0"), "{human_output}");
    assert!(!human_output.contains("online/offline/stale"));
    assert!(!json_output.contains("offline_count"));
}

#[tokio::test]
async fn ops_smoke_preflight_calls_only_read_only_endpoints() {
    let (output, requests) =
        run_smoke_preflight_with_projects(projects_fixture(true), "agent:ops:smoke").await;
    assert_eq!(requests.len(), 4);
    assert_eq!(
        smoke_request_kinds(&requests),
        vec![
            "runtime_status",
            "projects_list",
            "show_changes",
            "workspace_hygiene_check"
        ]
    );
    let joined = requests.join("\n---\n");
    assert!(joined.contains("POST /api/runtime/status "));
    assert!(joined.contains("POST /api/projects/list "));
    assert!(joined.contains(r#""tool":"show_changes""#));
    assert!(joined.contains(r#""tool":"workspace_hygiene_check""#));
    assert!(!joined.contains(r#""tool":"run_shell""#));
    assert!(!joined.contains(r#""tool":"run_job""#));
    assert!(!output.contains("secret-smoke-token"));
    assert!(output.contains("Overall: PASS"));
}

#[tokio::test]
async fn ops_smoke_preflight_project_missing_short_circuits() {
    let (output, requests) =
        run_smoke_preflight_with_projects(projects_fixture(true), "agent:ops:missing").await;
    assert_eq!(
        smoke_request_kinds(&requests),
        vec!["runtime_status", "projects_list"]
    );
    assert_no_workspace_preflight_tools(&requests);
    assert!(output.contains("Overall: FAIL"));
    assert!(output.contains("project_missing"));
}

#[tokio::test]
async fn ops_smoke_preflight_disconnected_project_short_circuits() {
    let mut projects = projects_fixture(true);
    projects["projects"][0]["connected"] = json!(false);
    projects["projects"][0]["agent_status"] = json!("stale");
    projects["projects"][0]["capabilities"]["recommended_for_smoke"] = json!(false);
    projects["projects"][0]["capabilities"]["safe_smoke_project"] = json!(false);
    let (output, requests) = run_smoke_preflight_with_projects(projects, "agent:ops:smoke").await;
    assert_eq!(
        smoke_request_kinds(&requests),
        vec!["runtime_status", "projects_list"]
    );
    assert_no_workspace_preflight_tools(&requests);
    assert!(output.contains("Overall: FAIL"));
    assert!(
        output.contains("project_disconnected") || output.contains("project_offline"),
        "{output}"
    );
    assert!(
        !output.contains("project_not_recommended_for_smoke"),
        "{output}"
    );
    assert!(
        !output.contains("project_not_safe_smoke_project"),
        "{output}"
    );
}

#[tokio::test]
async fn ops_smoke_preflight_non_git_project_short_circuits() {
    let mut projects = projects_fixture(true);
    projects["projects"][0]["capabilities"]["git_available"] = json!(false);
    let (output, requests) = run_smoke_preflight_with_projects(projects, "agent:ops:smoke").await;
    assert_eq!(
        smoke_request_kinds(&requests),
        vec!["runtime_status", "projects_list"]
    );
    assert_no_workspace_preflight_tools(&requests);
    assert!(output.contains("Overall: FAIL"));
    assert!(output.contains("project_git_unavailable"));
}
