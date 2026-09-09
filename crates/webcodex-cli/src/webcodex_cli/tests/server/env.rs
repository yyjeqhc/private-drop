use super::super::support::*;

#[test]
fn users_create_builds_admin_request_via_admin_cli() {
    // webcodex users create ... reuses admin_cli parsing.
    let action = cli_action(args(&[
        "users",
        "create",
        "--server-url",
        "https://example.test/",
        "--token",
        "fake-admin",
        "--username",
        "alice",
        "--display-name",
        "Alice",
        "--role",
        "user",
    ]));
    match action {
        CliAction::Admin(AdminCliCommand::UsersCreate(opts, user)) => {
            let req = build_admin_request(&AdminCliCommand::UsersCreate(opts, user)).unwrap();
            assert_eq!(req.server_url, "https://example.test");
            assert_eq!(req.path, "/api/users/create");
            assert_eq!(req.body["username"], "alice");
            assert_eq!(req.body["role"], "user");
        }
        other => panic!("expected Admin, got {other:?}"),
    }
}

#[test]
fn user_create_issue_credential_sets_request_field() {
    let action = cli_action(args(&[
        "users",
        "create",
        "--server-url",
        "https://example.test/",
        "--token",
        "fake-admin",
        "--username",
        "alice",
        "--issue-credential",
    ]));
    match action {
        CliAction::Admin(AdminCliCommand::UsersCreate(opts, user)) => {
            let req = build_admin_request(&AdminCliCommand::UsersCreate(opts, user)).unwrap();
            assert_eq!(req.path, "/api/users/create");
            assert_eq!(req.token, "fake-admin");
            assert_eq!(req.body["issue_credential"], true);
        }
        other => panic!("expected UsersCreate, got {other:?}"),
    }
}

#[test]
fn token_generate_api_prints_token_hash_and_prefix() {
    let action = cli_action(args(&["tokens", "generate", "--kind", "api"]));
    match action {
        CliAction::TokenGenerate(opts) => {
            let out = render_token_generate(opts);
            assert!(out.contains("Token:\nwc_pat_"));
            assert!(out.contains("\nHash:\nsha256:"));
            assert!(out.contains("\nPrefix:\nwc_pat_"));
            assert!(out.contains("generated offline and is not registered"));
            assert!(out.contains("use `webcodex connect`"));
        }
        other => panic!("expected TokenGenerate, got {other:?}"),
    }
}

#[test]
fn token_generate_runner_prints_legacy_compatible_token_prefix() {
    let action = cli_action(args(&["tokens", "generate", "--kind", "runner"]));
    match action {
        CliAction::TokenGenerate(opts) => {
            let out = render_token_generate(opts);
            assert!(out.contains("Token:\nwc_agent_"));
            assert!(out.contains("\nHash:\nsha256:"));
            assert!(out.contains("\nPrefix:\nwc_agent_"));
            assert!(out.contains("generated offline and is not registered"));
            assert!(out.contains("use `webcodex connect`"));
        }
        other => panic!("expected TokenGenerate, got {other:?}"),
    }
}

#[test]
fn token_generate_agent_kind_is_rejected() {
    match cli_action(args(&["tokens", "generate", "--kind", "agent"])) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(
                stderr.contains("--kind must be 'api' or 'runner'"),
                "{stderr}"
            );
        }
        other => panic!("expected rejected legacy token kind, got {other:?}"),
    }
}

#[test]
fn credential_resolution_priority_is_explicit_then_env_name_then_default_env() {
    let _guard = env_test_guard();
    let _env = EnvGuard::new()
        .set("WEBCODEX_ACCOUNT_CREDENTIAL", "wc_acct_default")
        .set("CUSTOM_ACCT", "wc_acct_custom");
    assert_eq!(
        resolve_account_credential(&Some("wc_acct_explicit".to_string()), &None).unwrap(),
        "wc_acct_explicit"
    );
    assert_eq!(
        resolve_account_credential(&None, &Some("CUSTOM_ACCT".to_string())).unwrap(),
        "wc_acct_custom"
    );
    assert_eq!(
        resolve_account_credential(&None, &None).unwrap(),
        "wc_acct_default"
    );
}

#[test]
fn token_register_hash_builds_hash_registration_request() {
    let action = cli_action(args(&[
        "tokens",
        "register-hash",
        "--server-url",
        "https://example.test",
        "--username",
        "alice",
        "--credential",
        "wc_acct_fake",
        "--name",
        "gpt-action",
        "--hash",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--prefix",
        "wc_pat_aaaaaaaa",
        "--scopes",
        "runtime:read,project:read",
    ]));
    match action {
        CliAction::Admin(AdminCliCommand::TokensRegisterHash(opts, t)) => {
            let req = build_admin_request(&AdminCliCommand::TokensRegisterHash(opts, t)).unwrap();
            assert_eq!(req.path, "/api/tokens/register_hash");
            assert_eq!(req.token, "wc_acct_fake");
            assert_eq!(req.body["username"], "alice");
            assert_eq!(req.body["name"], "gpt-action");
            assert_eq!(req.body["token_prefix"], "wc_pat_aaaaaaaa");
            assert_eq!(req.body["scopes"], json!(["runtime:read", "project:read"]));
        }
        other => panic!("expected TokensRegisterHash, got {other:?}"),
    }
}

#[test]
fn agent_token_register_hash_builds_hash_registration_request() {
    let action = cli_action(args(&[
        "runner-tokens",
        "register-hash",
        "--server-url",
        "https://example.test",
        "--username",
        "alice",
        "--credential",
        "wc_acct_fake",
        "--client-id",
        "alice-laptop",
        "--name",
        "alice laptop",
        "--hash",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--prefix",
        "wc_agent_aaaaaaa",
        "--scopes",
        "agent:register,agent:poll",
    ]));
    match action {
        CliAction::Admin(AdminCliCommand::RunnerTokensRegisterHash(opts, t)) => {
            let req =
                build_admin_request(&AdminCliCommand::RunnerTokensRegisterHash(opts, t)).unwrap();
            assert_eq!(req.path, "/api/agent-tokens/register_hash");
            assert_eq!(req.token, "wc_acct_fake");
            assert_eq!(req.body["username"], "alice");
            assert_eq!(req.body["client_id"], "alice-laptop");
            assert_eq!(req.body["name"], "alice laptop");
            assert_eq!(req.body["token_prefix"], "wc_agent_aaaaaaa");
            assert_eq!(req.body["scopes"], json!(["agent:register", "agent:poll"]));
            assert!(req.body.get("token").is_none());
        }
        other => panic!("expected RunnerTokensRegisterHash, got {other:?}"),
    }
}

#[test]
fn tokens_and_agent_tokens_commands_parse_to_admin() {
    let action = cli_action(args(&[
        "tokens",
        "create",
        "--server-url",
        "https://example.test",
        "--token",
        "fake-admin",
        "--username",
        "alice",
        "--name",
        "chatgpt-action",
        "--scope",
        "runtime:read",
        "--scope",
        "project:write",
    ]));
    assert!(matches!(
        action,
        CliAction::Admin(AdminCliCommand::TokensCreate(_, _))
    ));

    let action = cli_action(args(&[
        "runner-tokens",
        "create",
        "--server-url",
        "https://example.test",
        "--token",
        "fake-admin",
        "--username",
        "alice",
        "--client-id",
        "alice-laptop",
    ]));
    match action {
        CliAction::Admin(AdminCliCommand::RunnerTokensCreate(_, t)) => {
            // Default agent scopes applied.
            assert_eq!(
                t.scopes,
                SETUP_RUNNER_SCOPES
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            );
        }
        other => panic!("expected RunnerTokensCreate, got {other:?}"),
    }

    let list = cli_action(args(&[
        "runner-tokens",
        "list",
        "--server-url",
        "https://example.test",
        "--token",
        "fake-admin",
        "--username",
        "alice",
    ]));
    assert!(matches!(
        list,
        CliAction::Admin(AdminCliCommand::RunnerTokensList(_, _))
    ));

    let revoke = cli_action(args(&[
        "tokens",
        "revoke",
        "--server-url",
        "https://example.test",
        "--token",
        "fake-admin",
        "--username",
        "alice",
        "--token-id",
        "tok-1",
    ]));
    assert!(matches!(
        revoke,
        CliAction::Admin(AdminCliCommand::TokensRevoke(_, _))
    ));
}

#[tokio::test]
async fn token_create_local_does_not_send_plaintext_token_to_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request.starts_with("POST /api/tokens/register_hash "));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer wc_acct_fake"));
        assert!(request.contains(r#""token_hash":"sha256:"#));
        assert!(request.contains(r#""token_prefix":"wc_pat_"#));
        assert!(!request.contains(r#""token":"wc_pat_"#));
        let body = r#"{"success":true,"token":{"id":"tok-1","token_prefix":"wc_pat_fake"}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let out = run_token_create_local(TokenCreateLocalOptions {
        server_url: format!("http://{}", addr),
        username: "alice".to_string(),
        server_http: direct_server_http(),
        credential: Some("wc_acct_fake".to_string()),
        credential_env: None,
        name: Some("gpt-action".to_string()),
        scopes: SETUP_GPT_SCOPES.iter().map(|s| s.to_string()).collect(),
    })
    .await
    .unwrap();
    assert_eq!(out.matches("wc_pat_").count(), 1);
    handle.join().unwrap();
}

#[tokio::test]
async fn runner_token_create_local_does_not_send_plaintext_token_to_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request.starts_with("POST /api/agent-tokens/register_hash "));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer wc_acct_fake"));
        assert!(request.contains(r#""token_hash":"sha256:"#));
        assert!(request.contains(r#""token_prefix":"wc_agent_"#));
        assert!(request.contains(r#""client_id":"alice-laptop""#));
        assert!(!request.contains(r#""token":"wc_agent_"#));
        let body = r#"{"success":true,"token":{"id":"tok-1","token_prefix":"wc_agent_fake","allowed_client_id":"alice-laptop"}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let out = run_runner_token_create_local(RunnerTokenCreateLocalOptions {
        admin: AdminOptions {
            server_url: format!("http://{}", addr),
            server_http: direct_server_http(),
            credential: Some("wc_acct_fake".to_string()),
            ..AdminOptions::default()
        },
        username: "alice".to_string(),
        client_id: "alice-laptop".to_string(),
        name: Some("alice laptop".to_string()),
        scopes: SETUP_RUNNER_SCOPES.iter().map(|s| s.to_string()).collect(),
    })
    .await
    .unwrap();
    assert!(out.contains("Runner transport token created locally and registered with server."));
    assert!(out.contains("Client ID:\nalice-laptop"));
    assert_eq!(out.matches("wc_agent_").count(), 1);
    handle.join().unwrap();
}

// `WEBCODEX_ACCOUNT_CREDENTIAL` is the tested product behavior: it must stay
// set (and serialized against other env-mutating tests) for the whole async
// operation, so the env lock is held across the awaits by contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn runner_token_create_local_prefers_admin_token_over_default_account_credential() {
    let _guard = env_test_guard();
    let _env = EnvGuard::new().set("WEBCODEX_ACCOUNT_CREDENTIAL", "wc_acct_default");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer fake-admin"));
        let body = r#"{"success":true,"token":{"id":"tok-1","token_prefix":"wc_agent_fake","allowed_client_id":"alice-laptop"}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let out = run_runner_token_create_local(RunnerTokenCreateLocalOptions {
        admin: AdminOptions {
            server_url: format!("http://{}", addr),
            server_http: direct_server_http(),
            token: Some("fake-admin".to_string()),
            ..AdminOptions::default()
        },
        username: "alice".to_string(),
        client_id: "alice-laptop".to_string(),
        name: None,
        scopes: SETUP_RUNNER_SCOPES.iter().map(|s| s.to_string()).collect(),
    })
    .await
    .unwrap();
    assert_eq!(out.matches("wc_agent_").count(), 1);
    handle.join().unwrap();
}

// `WEBCODEX_ACCOUNT_CREDENTIAL` is the tested product behavior: it must stay
// set (and serialized against other env-mutating tests) for the whole async
// operation, so the env lock is held across the awaits by contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn runner_token_create_local_uses_default_account_credential() {
    let _guard = env_test_guard();
    let _env = EnvGuard::new().set("WEBCODEX_ACCOUNT_CREDENTIAL", "wc_acct_default");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer wc_acct_default"));
        let body = r#"{"success":true,"token":{"id":"tok-1","token_prefix":"wc_agent_fake","allowed_client_id":"alice-laptop"}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let out = run_runner_token_create_local(RunnerTokenCreateLocalOptions {
        admin: AdminOptions {
            server_url: format!("http://{}", addr),
            server_http: direct_server_http(),
            ..AdminOptions::default()
        },
        username: "alice".to_string(),
        client_id: "alice-laptop".to_string(),
        name: None,
        scopes: SETUP_RUNNER_SCOPES.iter().map(|s| s.to_string()).collect(),
    })
    .await
    .unwrap();
    assert_eq!(out.matches("wc_agent_").count(), 1);
    handle.join().unwrap();
}

fn cleared_proxy_env() -> EnvGuard {
    EnvGuard::new()
        .remove("HTTP_PROXY")
        .remove("HTTPS_PROXY")
        .remove("ALL_PROXY")
        .remove("NO_PROXY")
        .remove("http_proxy")
        .remove("https_proxy")
        .remove("all_proxy")
        .remove("no_proxy")
}

#[test]
fn server_http_proxy_validation_and_flag_conflict_are_fail_closed() {
    for proxy in [
        "http://127.0.0.1:7890",
        "http://proxy.example:80",
        "http://[::1]:7890",
    ] {
        ServerHttpOptions {
            proxy: Some(proxy.to_string()),
            no_system_proxy: false,
        }
        .validate()
        .unwrap();
    }
    for proxy in [
        "",
        "https://proxy.example:7890",
        "socks5://proxy.example:7890",
        "http://proxy.example",
        "http://@127.0.0.1:7890",
        "http://user@127.0.0.1:7890",
        "http://user:password@127.0.0.1:7890",
        "http://127.0.0.1:notaport",
        "http://127.0.0.1:7890/path",
        "http://127.0.0.1:7890?query=1",
        "http://127.0.0.1:7890#fragment",
    ] {
        let error = ServerHttpOptions {
            proxy: Some(proxy.to_string()),
            no_system_proxy: false,
        }
        .validate()
        .unwrap_err();
        if !proxy.is_empty() {
            assert!(!error.contains(proxy));
        }
        assert!(!error.contains("password"));
    }

    let action = cli_action(args(&[
        "connect",
        "http://server.invalid",
        "--proxy",
        "http://127.0.0.1:7890",
        "--no-system-proxy",
    ]));
    assert!(matches!(
        action,
        CliAction::Exit { code: 2, ref stderr, .. }
            if stderr.contains("--proxy and --no-system-proxy are mutually exclusive")
    ));
}

// The ambient HTTP_PROXY/NO_PROXY env is the tested product behavior: it must
// stay stable (and serialized against other env-mutating tests) for the whole
// async operation, so the env lock is held across the awaits by contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn default_server_http_uses_ambient_http_proxy() {
    let _guard = env_test_guard();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let proxy_url = format!("http://{proxy_addr}");
    let _env = cleared_proxy_env()
        .set("HTTP_PROXY", &proxy_url)
        .set("NO_PROXY", "");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request.starts_with("POST http://server.invalid:9/api/system-proxy HTTP/1.1"));
        let body = r#"{"proxied":true}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    let value = crate::webcodex_cli::post_json_unauthed(
        "http://server.invalid:9",
        &ServerHttpOptions::default(),
        "/api/system-proxy",
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(value["proxied"], true);
    handle.join().unwrap();
}

// The ambient HTTP_PROXY/NO_PROXY env is the tested product behavior: it must
// stay stable (and serialized against other env-mutating tests) for the whole
// async operation, so the env lock is held across the awaits by contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn ambient_no_proxy_bypasses_system_proxy() {
    let _guard = env_test_guard();
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let target_addr = target.local_addr().unwrap();
    let _env = cleared_proxy_env()
        .set("HTTP_PROXY", "http://127.0.0.1:1")
        .set("NO_PROXY", "127.0.0.1");
    let handle = thread::spawn(move || {
        let (mut stream, _) = target.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request.starts_with("POST /api/no-proxy-check HTTP/1.1"));
        let body = r#"{"direct":true}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    let value = crate::webcodex_cli::post_json_unauthed(
        &format!("http://{target_addr}"),
        &ServerHttpOptions::default(),
        "/api/no-proxy-check",
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(value["direct"], true);
    handle.join().unwrap();
}

// The ambient HTTP_PROXY/NO_PROXY env is part of the tested product behavior:
// it must stay stable (and serialized against other env-mutating tests) for
// the whole async operation, so the env lock is held across the awaits by
// contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn explicit_proxy_overrides_ambient_system_proxy() {
    let _guard = env_test_guard();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let explicit_proxy = format!("http://{proxy_addr}");
    let _env = cleared_proxy_env()
        .set("HTTP_PROXY", "http://127.0.0.1:1")
        .set("NO_PROXY", "");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request.starts_with("POST http://server.invalid:9/api/explicit-proxy HTTP/1.1"));
        let body = r#"{"explicit":true}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    let options = ServerHttpOptions {
        proxy: Some(explicit_proxy),
        no_system_proxy: false,
    };
    let value = crate::webcodex_cli::post_json_unauthed(
        "http://server.invalid:9",
        &options,
        "/api/explicit-proxy",
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(value["explicit"], true);
    handle.join().unwrap();
}

// The ambient HTTP_PROXY/NO_PROXY env is part of the tested product behavior:
// it must stay stable (and serialized against other env-mutating tests) for
// the whole async operation, so the env lock is held across the awaits by
// contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn no_system_proxy_forces_direct_connection() {
    let _guard = env_test_guard();
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let target_addr = target.local_addr().unwrap();
    let _env = cleared_proxy_env()
        .set("HTTP_PROXY", "http://127.0.0.1:1")
        .set("NO_PROXY", "");
    let handle = thread::spawn(move || {
        let (mut stream, _) = target.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request.starts_with("POST /api/direct-check HTTP/1.1"));
        let body = r#"{"direct":true}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    let options = ServerHttpOptions {
        proxy: None,
        no_system_proxy: true,
    };
    let value = crate::webcodex_cli::post_json_unauthed(
        &format!("http://{target_addr}"),
        &options,
        "/api/direct-check",
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(value["direct"], true);
    handle.join().unwrap();
}

// The ambient HTTP_PROXY env is part of the tested product behavior: it must
// stay stable (and serialized against other env-mutating tests) for the whole
// async operation, so the env lock is held across the awaits by contract.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn authenticated_explicit_proxy_request_redacts_bearer_from_error() {
    let _guard = env_test_guard();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let explicit_proxy = format!("http://{proxy_addr}");
    let _env = cleared_proxy_env().set("HTTP_PROXY", "http://127.0.0.1:1");
    let secret = "proxy-bearer-secret-do-not-leak";
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request.starts_with("POST http://server.invalid:9/api/auth-check HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer proxy-bearer-secret-do-not-leak"));
        let body = r#"{"error":"bad proxy-bearer-secret-do-not-leak"}"#;
        write!(
            stream,
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    let options = ServerHttpOptions {
        proxy: Some(explicit_proxy),
        no_system_proxy: false,
    };
    let error = crate::webcodex_cli::post_json_authed(crate::webcodex_cli::ApiCall {
        server_url: "http://server.invalid:9",
        server_http: &options,
        token: secret,
        path: "/api/auth-check",
        body: json!({}),
    })
    .await
    .unwrap_err();
    assert!(!error.contains(secret));
    assert!(error.contains("[redacted]"));
    handle.join().unwrap();
}

#[test]
fn token_prefix_never_exposes_full_token() {
    let p = token_prefix("wc_abcdef0123456789");
    assert!(p.ends_with('…'));
    assert!(!p.contains("0123456789"));
    assert_eq!(p, "wc_abcde…");
}
