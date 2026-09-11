use super::support::*;
use crate::webcodex_cli::plugin::{parse_plugin_command, run_plugin_command, PluginCommand};
use crate::webcodex_cli::plugin_init::{
    parse_plugin_init, run_plugin_init, PluginInitOptions, PLUGIN_INIT_SDK_VERSION,
};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::net::TcpStream;
use std::sync::mpsc;

#[derive(Clone)]
enum TestResponse {
    Json(u16, Value),
    Raw(u16, &'static str, &'static str),
    Drop,
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        500 => "Internal Server Error",
        _ => "Response",
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).unwrap();
        assert!(
            n > 0,
            "client closed before sending a complete HTTP request"
        );
        bytes.extend_from_slice(&chunk[..n]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + content_length {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

fn spawn_plugin_server(
    response: TestResponse,
) -> (String, mpsc::Sender<()>, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let request = read_http_request(&mut stream);
                    requests.push(request);
                    match &response {
                        TestResponse::Json(status, value) => {
                            let body = serde_json::to_string(value).unwrap();
                            write!(
                                stream,
                                "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                status,
                                status_reason(*status),
                                body.len(),
                                body
                            )
                            .unwrap();
                        }
                        TestResponse::Raw(status, content_type, body) => {
                            write!(
                                stream,
                                "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                status,
                                status_reason(*status),
                                content_type,
                                body.len(),
                                body
                            )
                            .unwrap();
                        }
                        TestResponse::Drop => {}
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    thread::yield_now();
                }
                Err(error) => panic!("plugin test server accept failed: {error}"),
            }
        }
        requests
    });
    (format!("http://{addr}"), stop_tx, handle)
}

fn request_body(request: &str) -> Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("HTTP request should contain a body separator");
    serde_json::from_str(body).unwrap()
}

fn request_authorization(request: &str) -> Option<&str> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim())
    })
}

fn runtime_success(output: Value) -> TestResponse {
    TestResponse::Json(
        200,
        json!({
            "success": true,
            "error": null,
            "output": output,
        }),
    )
}

fn safe_delete_tool() -> Value {
    json!({
        "name": "safe_delete",
        "title": "Safe delete",
        "description": "Move one path to Trash.",
        "inputSchema": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {"outcome": {"type": "string"}},
            "required": ["outcome"],
            "additionalProperties": false
        },
        "annotations": {
            "readOnlyHint": false,
            "destructiveHint": true,
            "idempotentHint": false,
            "openWorldHint": true
        }
    })
}

fn parse_for_server(
    command: &str,
    server_url: &str,
    identity: &[&str],
    json_output: bool,
) -> PluginCommand {
    let mut args = identity
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    args.extend([
        "--server-url".to_string(),
        server_url.to_string(),
        "--no-system-proxy".to_string(),
    ]);
    if json_output {
        args.push("--json".to_string());
    }
    parse_plugin_command(command, &args).unwrap()
}

async fn run_once(
    command: &str,
    identity: &[&str],
    response: TestResponse,
    json_output: bool,
) -> (
    Result<crate::webcodex_cli::plugin::PluginCommandOutput, String>,
    Vec<String>,
) {
    let (server_url, stop_tx, handle) = spawn_plugin_server(response);
    let command = parse_for_server(command, &server_url, identity, json_output);
    let output = run_plugin_command(command).await;
    stop_tx.send(()).unwrap();
    let requests = handle.join().unwrap();
    (output, requests)
}

#[test]
fn plugin_help_and_root_usage_expose_phase_three_surface() {
    let root = cli_exit(["--help"]).unwrap();
    assert!(root
        .lines()
        .any(|line| line.trim_start().starts_with("plugin ")));

    let help = cli_exit(["plugin", "--help"]).unwrap();
    for command in ["init", "list", "describe", "check", "reload"] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(command)),
            "{help}"
        );
    }
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("call ")),
        "{help}"
    );
    assert!(help.contains("init is local-only"), "{help}");
    assert!(help.contains("plugin:inspect"), "{help}");
    assert!(help.contains("plugin:manage"), "{help}");
}

#[test]
fn plugin_init_help_documents_local_scaffold_contract() {
    let help = cli_exit(["plugin", "init", "--help"]).unwrap();
    for expected in [
        "plugin init <DIRECTORY> [--id PROVIDER_ID]",
        "local-only",
        "0.1.0",
        "no Server request",
        "never overwrites user data",
    ] {
        assert!(help.contains(expected), "missing {expected:?}: {help}");
    }
    assert!(!help.contains("--server-url"), "{help}");
    assert!(!help.contains("--token"), "{help}");
}

#[test]
fn plugin_init_parser_uses_derived_or_explicit_canonical_provider_id() {
    match cli_action(["plugin", "init", "echo-plugin"]) {
        CliAction::PluginInit(opts) => {
            assert_eq!(opts.directory, PathBuf::from("echo-plugin"));
            assert_eq!(opts.provider_id, "echo-plugin");
        }
        other => panic!("unexpected plugin init parse: {other:?}"),
    }

    match cli_action(["plugin", "init", "Example Plugin", "--id", "echo.plugin_1"]) {
        CliAction::PluginInit(opts) => {
            assert_eq!(opts.directory, PathBuf::from("Example Plugin"));
            assert_eq!(opts.provider_id, "echo.plugin_1");
        }
        other => panic!("unexpected explicit-id plugin init parse: {other:?}"),
    }
}

#[test]
fn plugin_init_parser_rejects_invalid_ids_and_network_operator_flags() {
    for (args, expected) in [
        (
            vec!["plugin", "init", "Invalid Name"],
            "supply --id PROVIDER_ID",
        ),
        (
            vec!["plugin", "init", "example", "--id", "Invalid-ID"],
            "invalid --id provider id",
        ),
        (
            vec![
                "plugin",
                "init",
                "example",
                "--server-url",
                "http://127.0.0.1:9",
            ],
            "unknown plugin init flag: --server-url",
        ),
        (
            vec!["plugin", "init", "example", "--token", "secret"],
            "unknown plugin init flag: --token",
        ),
        (
            vec!["plugin", "init", "example", "--runner", "special"],
            "unknown plugin init flag: --runner",
        ),
        (
            vec!["plugin", "init", "example", "--json"],
            "unknown plugin init flag: --json",
        ),
    ] {
        match cli_action(args.iter().copied()) {
            CliAction::Exit { code, stderr, .. } => {
                assert_eq!(code, 2, "{args:?}: {stderr}");
                assert!(stderr.contains(expected), "{args:?}: {stderr}");
            }
            other => panic!("invalid plugin init unexpectedly parsed: {other:?}"),
        }
    }
}

#[test]
fn plugin_init_creates_exact_public_sdk_scaffold_without_executing_dependencies() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("echo-plugin");
    let output = run_plugin_init(PluginInitOptions {
        directory: destination.clone(),
        provider_id: "echo-plugin".to_string(),
    })
    .unwrap();

    assert!(output.contains("Provider id: echo-plugin"), "{output}");
    assert!(output.contains("@yyjeqhc/webcodex-plugin-sdk@0.1.0"));
    let root_entries = std::fs::read_dir(&destination)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        root_entries,
        [
            ".gitignore",
            "README.md",
            "package.json",
            "src",
            "tsconfig.json"
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    );
    let src_entries = std::fs::read_dir(destination.join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        src_entries,
        [OsString::from("plugin.ts")].into_iter().collect()
    );

    let package_text = std::fs::read_to_string(destination.join("package.json")).unwrap();
    let package: Value = serde_json::from_str(&package_text).unwrap();
    assert_eq!(PLUGIN_INIT_SDK_VERSION, "0.1.0");
    assert_eq!(package["private"], true);
    assert_eq!(package["type"], "module");
    assert_eq!(package["engines"]["node"], ">=18");
    assert_eq!(
        package["dependencies"]["@yyjeqhc/webcodex-plugin-sdk"],
        PLUGIN_INIT_SDK_VERSION
    );
    for forbidden in ["file:", "workspace:", "git+", "latest", "^0.1.0", "~0.1.0"] {
        assert!(!package_text.contains(forbidden), "{package_text}");
    }
    assert!(!package_text.contains("/root/git/webcodex"));

    let plugin = std::fs::read_to_string(destination.join("src/plugin.ts")).unwrap();
    assert!(plugin.contains("from \"@yyjeqhc/webcodex-plugin-sdk\""));
    for api in [
        "defineTool",
        "definePlugin",
        "runPlugin",
        "schema",
        "textResult",
    ] {
        assert!(
            plugin.contains(api),
            "generated plugin missing {api}: {plugin}"
        );
    }
    assert!(plugin.contains("readOnlyHint: true"));
    assert!(!plugin.contains("JSON-RPC"));
    assert!(!plugin.contains("child_process"));
    assert!(!plugin.contains("fetch("));

    let readme = std::fs::read_to_string(destination.join("README.md")).unwrap();
    assert!(readme.contains("npm install"));
    assert!(readme.contains("npm run build"));
    assert!(readme.contains("id = \"echo-plugin\""));
    assert!(readme.contains("/absolute/path/to/PLUGIN_DIRECTORY/dist/plugin.js"));
    assert!(readme.contains("webcodex plugin check --runner <runner> --plugin echo-plugin"));
    assert!(readme.contains("--tool echo"));
    assert!(!destination.join("node_modules").exists());
    assert!(!destination.join("package-lock.json").exists());
    assert!(!destination.join("dist").exists());
}

#[test]
fn plugin_init_accepts_an_existing_empty_directory() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("existing-empty");
    std::fs::create_dir(&destination).unwrap();
    run_plugin_init(PluginInitOptions {
        directory: destination.clone(),
        provider_id: "existing-empty".to_string(),
    })
    .unwrap();
    assert!(destination.join("src/plugin.ts").is_file());
}

#[test]
fn plugin_init_rejects_nonempty_destination_without_modifying_existing_files() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("occupied");
    std::fs::create_dir(&destination).unwrap();
    let sentinel = destination.join("keep.txt");
    std::fs::write(&sentinel, "preserve me").unwrap();

    let error = run_plugin_init(PluginInitOptions {
        directory: destination.clone(),
        provider_id: "occupied".to_string(),
    })
    .unwrap_err();
    assert!(error.contains("not empty"), "{error}");
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "preserve me");
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
}

#[test]
fn plugin_init_rejects_file_destination_without_modifying_it() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("occupied");
    std::fs::write(&destination, "preserve me").unwrap();
    let error = run_plugin_init(PluginInitOptions {
        directory: destination.clone(),
        provider_id: "occupied".to_string(),
    })
    .unwrap_err();
    assert!(error.contains("ordinary directory"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&destination).unwrap(),
        "preserve me"
    );
}

#[cfg(unix)]
#[test]
fn plugin_init_rejects_symlink_destination_without_touching_target() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    let destination = temp.path().join("linked-plugin");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, &destination).unwrap();

    let error = run_plugin_init(PluginInitOptions {
        directory: destination.clone(),
        provider_id: "linked-plugin".to_string(),
    })
    .unwrap_err();
    assert!(error.contains("symlink"), "{error}");
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    assert!(std::fs::symlink_metadata(&destination)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn plugin_init_runs_without_network_or_token_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("offline-plugin");
    run_plugin_init(
        parse_plugin_init(&[
            destination.to_string_lossy().to_string(),
            "--id".to_string(),
            "offline-plugin".to_string(),
        ])
        .unwrap(),
    )
    .unwrap();
    assert!(destination.join("package.json").is_file());
    assert!(!destination.join("node_modules").exists());
    assert!(!destination.join("package-lock.json").exists());
}

#[test]
fn plugin_command_help_documents_exact_identity_and_reload_scope() {
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["plugin", "list", "--help"],
            &["--runner RUNNER", "--plugin PLUGIN", "plugin:inspect"],
        ),
        (
            &["plugin", "describe", "--help"],
            &[
                "--runner RUNNER",
                "--plugin PLUGIN",
                "--tool TOOL",
                "plugin:inspect",
            ],
        ),
        (
            &["plugin", "check", "--help"],
            &[
                "--runner RUNNER",
                "--plugin PLUGIN",
                "disposable",
                "plugin:manage",
            ],
        ),
        (
            &["plugin", "reload", "--help"],
            &[
                "--runner RUNNER",
                "complete provider",
                "no per-provider",
                "plugin:manage",
            ],
        ),
    ];
    for (args, expected) in cases {
        let help = cli_exit(args.iter().copied()).unwrap();
        for needle in *expected {
            assert!(
                help.contains(needle),
                "help for {args:?} missing {needle:?}: {help}"
            );
        }
    }
}

#[test]
fn plugin_parser_accepts_only_canonical_identity_shapes() {
    match cli_action(["plugin", "list", "--json"]) {
        CliAction::Plugin(PluginCommand::List(opts)) => {
            assert!(opts.runner.is_none());
            assert!(opts.plugin.is_none());
            assert!(opts.common.json);
        }
        other => panic!("unexpected plugin list parse: {other:?}"),
    }
    match cli_action([
        "plugin",
        "list",
        "--runner",
        "special",
        "--plugin",
        "safe-delete",
    ]) {
        CliAction::Plugin(PluginCommand::List(opts)) => {
            assert_eq!(opts.runner.as_deref(), Some("special"));
            assert_eq!(opts.plugin.as_deref(), Some("safe-delete"));
        }
        other => panic!("unexpected scoped plugin list parse: {other:?}"),
    }
    match cli_action([
        "plugin",
        "describe",
        "--runner",
        "special",
        "--plugin",
        "safe-delete",
        "--tool",
        "safe_delete",
    ]) {
        CliAction::Plugin(PluginCommand::Describe(opts)) => {
            assert_eq!(opts.runner, "special");
            assert_eq!(opts.plugin, "safe-delete");
            assert_eq!(opts.tool, "safe_delete");
        }
        other => panic!("unexpected plugin describe parse: {other:?}"),
    }
}

#[test]
fn plugin_parser_requires_exact_targets_and_rejects_invented_surfaces() {
    let failures: &[(&[&str], &str)] = &[
        (
            &["plugin", "list", "--plugin", "safe-delete"],
            "requires --runner",
        ),
        (
            &[
                "plugin",
                "describe",
                "--plugin",
                "safe-delete",
                "--tool",
                "safe_delete",
            ],
            "requires --runner",
        ),
        (
            &["plugin", "check", "--runner", "special"],
            "requires --plugin",
        ),
        (&["plugin", "reload"], "requires --runner"),
        (
            &[
                "plugin",
                "reload",
                "--runner",
                "special",
                "--plugin",
                "safe-delete",
            ],
            "unknown plugin reload flag: --plugin",
        ),
        (&["plugin", "call"], "unknown plugin subcommand: call"),
        (&["plugin", "init"], "plugin init requires DIRECTORY"),
        (&["plugin", "unknown"], "unknown plugin subcommand: unknown"),
    ];
    for (args, expected) in failures {
        match cli_action(args.iter().copied()) {
            CliAction::Exit {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 2, "{args:?}: {stderr}");
                assert!(stdout.is_empty());
                assert!(stderr.contains(expected), "{args:?}: {stderr}");
            }
            other => panic!("{args:?} unexpectedly parsed: {other:?}"),
        }
    }
}

#[test]
fn plugin_parser_rejects_duplicate_and_invalid_flags_without_echoing_token() {
    let secret = "wc_pat_plugin_parser_secret_0123456789";
    for args in [
        vec!["plugin", "list", "--runner", "special", "--runner", "other"],
        vec!["plugin", "list", "--json", "--json"],
        vec!["plugin", "list", "--tool", "safe_delete"],
        vec![
            "plugin",
            "check",
            "--runner",
            "special",
            "--plugin",
            "safe-delete",
            "--oauth-local-plugins",
        ],
        vec![
            "plugin",
            "check",
            "--runner",
            "special",
            "--plugin",
            "safe-delete",
            "--token",
            secret,
            "--bad",
        ],
    ] {
        match cli_action(args.iter().copied()) {
            CliAction::Exit { code, stderr, .. } => {
                assert_eq!(code, 2, "{args:?}: {stderr}");
                assert!(!stderr.contains(secret), "{stderr}");
            }
            other => panic!("invalid plugin args unexpectedly parsed: {other:?}"),
        }
    }
}

#[tokio::test]
async fn plugin_list_request_mapping_is_exact_for_all_three_scopes() {
    let cases = [
        (vec![], json!({"action": "list"}), json!({"runners": []})),
        (
            vec!["--runner", "special"],
            json!({"action": "list", "runner": "special"}),
            json!({"runner": "special", "plugins": []}),
        ),
        (
            vec!["--runner", "special", "--plugin", "safe-delete"],
            json!({"action": "list", "runner": "special", "plugin": "safe-delete"}),
            json!({
                "runner": "special",
                "plugin": "safe-delete",
                "name": "Safe Delete",
                "status": "ready",
                "errorCode": null,
                "toolCount": 0,
                "tools": []
            }),
        ),
    ];
    for (identity, expected_params, output) in cases {
        let (result, requests) = run_once("list", &identity, runtime_success(output), false).await;
        assert_eq!(result.unwrap().exit_code, 0);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            request_body(&requests[0]),
            json!({"tool": "plugin_tool", "params": expected_params})
        );
    }
}

#[tokio::test]
async fn plugin_describe_check_and_reload_map_to_one_canonical_runtime_call() {
    let cases = [
        (
            "describe",
            vec![
                "--runner",
                "special",
                "--plugin",
                "safe-delete",
                "--tool",
                "safe_delete",
            ],
            json!({"action": "describe", "runner": "special", "plugin": "safe-delete", "tool": "safe_delete"}),
            json!({
                "runner": "special",
                "plugin": "safe-delete",
                "pluginName": "Safe Delete",
                "tool": safe_delete_tool(),
                "binding": "wc_pbind_test"
            }),
        ),
        (
            "check",
            vec!["--runner", "special", "--plugin", "safe-delete"],
            json!({"action": "check", "runner": "special", "plugin": "safe-delete"}),
            json!({
                "runner": "special", "plugin": "safe-delete", "ready": true, "phase": "ready",
                "toolCount": 1, "tools": [{"name":"safe_delete","title":"Safe delete"}]
            }),
        ),
        (
            "reload",
            vec!["--runner", "special"],
            json!({"action": "reload", "runner": "special"}),
            json!({"runner":"special","plugins":[],"failures":[]}),
        ),
    ];
    for (command, identity, params, output) in cases {
        let (result, requests) = run_once(command, &identity, runtime_success(output), false).await;
        assert_eq!(result.unwrap().exit_code, 0);
        assert_eq!(requests.len(), 1, "{command}");
        assert_eq!(
            request_body(&requests[0]),
            json!({"tool": "plugin_tool", "params": params}),
            "{command}"
        );
    }
}

#[tokio::test]
async fn plugin_bearer_token_is_header_only_and_never_rendered() {
    let secret = "wc_pat_plugin_header_secret_0123456789";
    let (server_url, stop_tx, handle) =
        spawn_plugin_server(runtime_success(json!({"runners": []})));
    let args = vec![
        "--server-url".to_string(),
        server_url,
        "--no-system-proxy".to_string(),
        "--token".to_string(),
        secret.to_string(),
        "--json".to_string(),
    ];
    let output = run_plugin_command(parse_plugin_command("list", &args).unwrap())
        .await
        .unwrap();
    stop_tx.send(()).unwrap();
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        request_authorization(&requests[0]),
        Some(&format!("Bearer {secret}")[..])
    );
    assert_eq!(
        request_body(&requests[0]),
        json!({"tool":"plugin_tool","params":{"action":"list"}})
    );
    assert!(!requests[0]
        .split_once("\r\n\r\n")
        .unwrap()
        .1
        .contains(secret));
    assert!(!output.stdout.contains(secret));
}

#[tokio::test]
async fn plugin_rejects_runner_transport_token_before_any_request() {
    let secret = "wc_agent_plugin_wrong_credential_0123456789";
    let (server_url, stop_tx, handle) =
        spawn_plugin_server(runtime_success(json!({"runners": []})));
    let args = vec![
        "--server-url".to_string(),
        server_url,
        "--no-system-proxy".to_string(),
        "--token".to_string(),
        secret.to_string(),
    ];
    let error = run_plugin_command(parse_plugin_command("list", &args).unwrap())
        .await
        .unwrap_err();
    stop_tx.send(()).unwrap();
    let requests = handle.join().unwrap();
    assert!(requests.is_empty());
    assert!(error.contains("Runner transport token"), "{error}");
    assert!(!error.contains(secret), "{error}");
}

#[tokio::test]
async fn plugin_http_401_and_403_fail_with_command_specific_scope_guidance() {
    let cases = [
        ("list", vec![], 401, "user/API bearer credential"),
        (
            "describe",
            vec![
                "--runner",
                "special",
                "--plugin",
                "safe-delete",
                "--tool",
                "safe_delete",
            ],
            403,
            "plugin:inspect",
        ),
        (
            "check",
            vec!["--runner", "special", "--plugin", "safe-delete"],
            403,
            "plugin:manage",
        ),
        ("reload", vec!["--runner", "special"], 403, "plugin:manage"),
    ];
    for (command, identity, status, expected) in cases {
        let response = TestResponse::Json(status, json!({"error":"scope rejected"}));
        let (result, requests) = run_once(command, &identity, response, false).await;
        assert_eq!(requests.len(), 1);
        let error = result.unwrap_err();
        assert!(error.contains(expected), "{command}: {error}");
        if status == 403 && matches!(command, "check" | "reload") {
            assert!(error.contains("--oauth-local-plugins"), "{error}");
            assert!(error.contains("does not grant plugin:manage"), "{error}");
        }
    }
}

#[tokio::test]
async fn plugin_json_output_preserves_canonical_output_without_wrapper() {
    let canonical = json!({
        "runner": "special",
        "plugin": "safe-delete",
        "pluginName": "Safe Delete",
        "tool": safe_delete_tool(),
        "binding": "wc_pbind_exact"
    });
    let (result, requests) = run_once(
        "describe",
        &[
            "--runner",
            "special",
            "--plugin",
            "safe-delete",
            "--tool",
            "safe_delete",
        ],
        runtime_success(canonical.clone()),
        true,
    )
    .await;
    assert_eq!(requests.len(), 1);
    let output = result.unwrap();
    assert_eq!(output.exit_code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&output.stdout).unwrap(),
        canonical
    );
}

#[tokio::test]
async fn plugin_human_rendering_is_bounded_and_ignores_unknown_raw_stderr_field() {
    let long_name = format!("{}\nINJECTED", "x".repeat(5000));
    let output = json!({
        "runner":"special",
        "plugin":"safe-delete",
        "ready":false,
        "phase":"tools_list",
        "code":"plugin_schema_invalid",
        "detail": long_name,
        "diagnostic":{"code":"schema_invalid","field":"inputSchema"},
        "toolCount":0,
        "tools":[],
        "stderr":"RAW_PROVIDER_SECRET"
    });
    let (result, _) = run_once(
        "check",
        &["--runner", "special", "--plugin", "safe-delete"],
        runtime_success(output),
        false,
    )
    .await;
    let result = result.unwrap();
    assert_eq!(result.exit_code, 2);
    assert!(result.stdout.contains("Status: not ready"));
    assert!(result.stdout.contains("Code: plugin_schema_invalid"));
    assert!(!result.stdout.contains("\nINJECTED"));
    assert!(!result.stdout.contains("RAW_PROVIDER_SECRET"));
    assert!(
        result.stdout.chars().count() < 5000,
        "output was not bounded"
    );
}

#[tokio::test]
async fn plugin_check_and_reload_known_rejections_have_deterministic_nonzero_exit() {
    let (check, _) = run_once(
        "check",
        &["--runner", "special", "--plugin", "broken"],
        runtime_success(json!({
            "runner":"special","plugin":"broken","ready":false,"phase":"validation",
            "code":"plugin_schema_invalid","detail":"schema rejected","toolCount":0,"tools":[]
        })),
        false,
    )
    .await;
    assert_eq!(check.unwrap().exit_code, 2);

    let (reload, _) = run_once(
        "reload",
        &["--runner", "special"],
        runtime_success(json!({
            "runner":"special",
            "plugins":[{"plugin":"safe-delete","name":"Safe Delete","status":"ready","errorCode":null}],
            "failures":[{"provider_id":"broken","code":"plugin_schema_invalid"}]
        })),
        false,
    )
    .await;
    let reload = reload.unwrap();
    assert_eq!(reload.exit_code, 2);
    assert!(
        reload.stdout.contains("Status: rejected"),
        "{}",
        reload.stdout
    );
    assert!(
        reload.stdout.contains("plugin_schema_invalid"),
        "{}",
        reload.stdout
    );
}

#[tokio::test]
async fn plugin_runtime_tool_failure_preserves_canonical_failure_for_json_scripts() {
    let canonical_failure = json!({
        "error": {"code":"runner_not_found","message":"Runner is not available"},
        "failure_kind":"runner_not_found",
        "dispatch_certainty":"not_started",
        "recovery":"Re-list caller-visible Plugin Runners."
    });
    let response = TestResponse::Json(
        400,
        json!({"success":false,"error":"Runner is not available","output":canonical_failure.clone()}),
    );
    let (result, requests) = run_once("list", &["--runner", "missing"], response, true).await;
    assert_eq!(requests.len(), 1);
    let result = result.unwrap();
    assert_eq!(result.exit_code, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&result.stdout).unwrap(),
        canonical_failure
    );
}

#[tokio::test]
async fn plugin_check_transport_failure_posts_once_and_reports_uncertainty() {
    let (result, requests) = run_once(
        "check",
        &["--runner", "special", "--plugin", "safe-delete"],
        TestResponse::Drop,
        false,
    )
    .await;
    assert_eq!(
        requests.len(),
        1,
        "check transport failure must not be retried"
    );
    let error = result.unwrap_err();
    assert!(error.contains("outcome may be unknown"), "{error}");
    assert!(error.contains("Do not retry automatically"), "{error}");
    assert!(error.contains("observe current Plugin state"), "{error}");
}

#[tokio::test]
async fn plugin_reload_malformed_post_send_response_posts_once_and_reports_uncertainty() {
    let (result, requests) = run_once(
        "reload",
        &["--runner", "special"],
        TestResponse::Raw(200, "application/json", "{"),
        false,
    )
    .await;
    assert_eq!(
        requests.len(),
        1,
        "reload ambiguous response must not be retried"
    );
    let error = result.unwrap_err();
    assert!(error.contains("outcome may be unknown"), "{error}");
    assert!(error.contains("Do not retry automatically"), "{error}");
    assert!(!error.contains("definitely not applied"), "{error}");
}

#[tokio::test]
async fn plugin_observation_transport_failure_is_not_misrepresented_as_management_uncertainty() {
    let (result, requests) = run_once("list", &[], TestResponse::Drop, false).await;
    assert_eq!(requests.len(), 1);
    let error = result.unwrap_err();
    assert!(error.contains("plugin list request failed"), "{error}");
    assert!(!error.contains("outcome may be unknown"), "{error}");
}
