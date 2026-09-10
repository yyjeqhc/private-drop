use super::*;
use crate::webcodex_runner::config::restart_required_fields;

#[test]
fn runner_config_accepts_legacy_projects_dir_alias_and_normalizes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    let registry = tmp.path().join("projects.d");
    std::fs::write(
        &path,
        format!(
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\nprojects_dir = {:?}\n[policy]\nallow_cwd_anywhere = true\n",
            registry.to_string_lossy()
        ),
    )
    .unwrap();
    let cfg = load_config(&path).unwrap();
    assert_eq!(
        cfg.project_registry_dir.as_deref(),
        Some(registry.as_path())
    );
    assert!(cfg.legacy_projects_dir.is_none());
}

#[test]
fn runner_config_alias_only_migration_does_not_require_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runner.toml");
    let registry = tmp.path().join("projects.d");
    let render = |field: &str| {
        format!(
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\n{field} = {:?}\n[policy]\nallow_cwd_anywhere = true\n",
            registry.to_string_lossy()
        )
    };

    std::fs::write(&path, render("projects_dir")).unwrap();
    let legacy = load_config(&path).unwrap();
    std::fs::write(&path, render("project_registry_dir")).unwrap();
    let canonical = load_config(&path).unwrap();

    assert!(legacy.legacy_projects_dir.is_none());
    assert!(canonical.legacy_projects_dir.is_none());
    assert_eq!(legacy.project_registry_dir, canonical.project_registry_dir);
    assert!(restart_required_fields(&legacy, &canonical).is_empty());
}

#[test]
fn runner_config_rejects_new_and_legacy_registry_fields_together() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    let current = tmp.path().join("project-registry");
    let legacy = tmp.path().join("projects.d");
    std::fs::write(
        &path,
        format!(
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\nproject_registry_dir = {:?}\nprojects_dir = {:?}\n[policy]\nallow_cwd_anywhere = true\n",
            current.to_string_lossy(),
            legacy.to_string_lossy()
        ),
    )
    .unwrap();
    let error = load_config(&path).unwrap_err();
    assert!(error.contains("cannot both be configured"), "{error}");
}

#[test]
fn runner_config_defaults_transport_to_websocket_without_quic_section() {
    // No transport field and no [quic] section: default stays websocket.
    let toml = r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
"#;
    let cfg: RunnerConfig = toml::from_str(toml).unwrap();
    assert!(cfg.transport.is_none());
    assert!(cfg.quic.is_none());
    assert_eq!(effective_transport(&cfg), TRANSPORT_WEBSOCKET);
    assert_eq!(
        cfg.websocket_connect_timeout_secs,
        default_websocket_connect_timeout_secs()
    );
    assert_eq!(
        auto_transport_plan(&cfg),
        vec![TRANSPORT_WEBSOCKET, TRANSPORT_POLLING]
    );
    assert_eq!(cfg.mcp_gateway.request_timeout_secs, 30);
    assert!(cfg.mcp_gateway.providers.is_empty());
}

#[test]
fn runner_config_skill_roots_default_empty_and_accept_absolute_paths() {
    let omitted: RunnerConfig = toml::from_str(
        "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\n",
    )
    .unwrap();
    assert!(omitted.skills.roots.is_empty());

    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("runner.toml");
    let live_root = tmp.path().join("live-skills");
    std::fs::write(
        &config_path,
        format!(
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\nproject_registry_dir = \"project-registry\"\n[skills]\nroots = [{:?}]\n[policy]\nallow_cwd_anywhere = true\n",
            live_root.to_string_lossy().as_ref()
        ),
    )
    .unwrap();
    let configured = load_config(&config_path).unwrap();
    assert_eq!(configured.skills.roots, vec![live_root]);
}

#[test]
fn runner_config_skill_roots_validation_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("runner.toml");
    let render = |roots: &str| {
        format!(
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\nproject_registry_dir = \"project-registry\"\n[skills]\nroots = [{roots}]\n[policy]\nallow_cwd_anywhere = true\n"
        )
    };

    std::fs::write(&config_path, render("\"relative/skills\"")).unwrap();
    let error = load_config(&config_path).unwrap_err();
    assert!(
        error.contains("absolute paths without parent traversal"),
        "{error}"
    );

    let absolute = std::env::current_dir().unwrap().join("skill-root");
    let encoded = format!("{:?}", absolute.to_string_lossy().as_ref());
    let too_many = std::iter::repeat_n(encoded, 17)
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(&config_path, render(&too_many)).unwrap();
    let error = load_config(&config_path).unwrap_err();
    assert!(error.contains("at most 16 entries"), "{error}");

    let too_long = std::env::current_dir()
        .unwrap()
        .join("x".repeat(crate::webcodex_runner::config::MAX_CONFIGURED_SKILL_ROOT_PATH_BYTES + 1));
    std::fs::write(
        &config_path,
        render(&format!("{:?}", too_long.to_string_lossy().as_ref())),
    )
    .unwrap();
    let error = load_config(&config_path).unwrap_err();
    assert!(error.contains("at most 4096 bytes"), "{error}");
}

#[test]
fn runner_config_rejects_zero_websocket_connect_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
project_registry_dir = "project-registry"
websocket_connect_timeout_secs = 0
"#,
    )
    .unwrap();

    let err = load_config(&path).unwrap_err();
    assert!(
        err.contains("websocket_connect_timeout_secs must be > 0"),
        "{err}"
    );
}

#[test]
fn runner_config_bounds_polling_idle_floor_but_not_unused_websocket_value() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");

    for transport in [TRANSPORT_POLLING, TRANSPORT_AUTO] {
        std::fs::write(
            &path,
            format!(
                "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\ntransport = \"{transport}\"\npoll_interval_ms = {}\nproject_registry_dir = \"project-registry\"\n[policy]\nallow_cwd_anywhere = true\n",
                webcodex_runner_config::MAX_POLL_INTERVAL_MS + 1
            ),
        )
        .unwrap();
        let error = load_config(&path).unwrap_err();
        assert!(error.contains("must be <= 30000"), "{transport}: {error}");

        std::fs::write(
            &path,
            format!(
                "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\ntransport = \"{transport}\"\npoll_interval_ms = {}\nproject_registry_dir = \"project-registry\"\n[policy]\nallow_cwd_anywhere = true\n",
                webcodex_runner_config::MAX_POLL_INTERVAL_MS
            ),
        )
        .unwrap();
        load_config(&path).unwrap();
    }

    std::fs::write(
        &path,
        format!(
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"t\"\nclient_id = \"oe\"\ntransport = \"websocket\"\npoll_interval_ms = {}\nproject_registry_dir = \"project-registry\"\n[policy]\nallow_cwd_anywhere = true\n",
            webcodex_runner_config::MAX_POLL_INTERVAL_MS + 1
        ),
    )
    .unwrap();
    load_config(&path).unwrap();
}

#[test]
fn runner_config_ignores_obsolete_temporary_projects_root_without_runtime_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runner.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
project_registry_dir = "project-registry"
temporary_projects_root = "obsolete-and-relative"
[policy]
allow_cwd_anywhere = false
"#,
    )
    .unwrap();

    let cfg = load_config(&path).unwrap();
    assert_eq!(cfg.client_id, "oe");
    assert!(!cfg.policy.allow_cwd_anywhere);
}

#[test]
fn runner_config_accepts_transport_quic_with_quic_section() {
    let toml = r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
transport = "quic"

[quic]
server_addr = "v4.example.test:8443"
server_name = "v4.example.test"
"#;
    let cfg: RunnerConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.transport.as_deref(), Some("quic"));
    let quic = cfg.quic.expect("quic section");
    assert_eq!(quic.server_addr, "v4.example.test:8443");
    assert_eq!(quic.server_name, "v4.example.test");
    // Defaults applied.
    assert_eq!(quic.alpn, "webcodex-runner/1");
    assert_eq!(quic.connect_timeout_secs, 10);
    assert_eq!(quic.keepalive_interval_secs, 20);
}

#[test]
fn runner_config_accepts_transport_auto() {
    let toml = r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
transport = "auto"
"#;
    let cfg: RunnerConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.transport.as_deref(), Some(TRANSPORT_AUTO));
    assert_eq!(effective_transport(&cfg), TRANSPORT_AUTO);
    assert_eq!(
        auto_transport_plan(&cfg),
        vec![TRANSPORT_WEBSOCKET, TRANSPORT_POLLING]
    );
}

#[test]
fn auto_transport_plan_tries_quic_then_websocket_then_polling() {
    let mut cfg = test_config(PathBuf::from("/tmp/x"));
    cfg.transport = Some(TRANSPORT_AUTO.to_string());
    cfg.quic = Some(quic_client_config());
    assert_eq!(
        auto_transport_plan(&cfg),
        vec![TRANSPORT_QUIC, TRANSPORT_WEBSOCKET, TRANSPORT_POLLING]
    );
}

#[test]
fn strict_quic_transport_still_requires_quic_section() {
    let mut cfg = test_config(PathBuf::from("/tmp/x"));
    cfg.transport = Some(TRANSPORT_QUIC.to_string());
    let err = resolve_quic_config(&cfg).unwrap_err();
    assert!(err.contains("transport=quic requires a [quic] section"));
    assert_eq!(effective_transport(&cfg), TRANSPORT_QUIC);
}

#[test]
fn resolve_quic_config_errors_when_section_missing() {
    let mut cfg = test_config(PathBuf::from("/tmp/x"));
    cfg.transport = Some("quic".to_string());
    let err = resolve_quic_config(&cfg).unwrap_err();
    assert!(err.contains("[quic]"), "err was: {err}");
}

#[test]
fn resolve_quic_config_errors_when_server_addr_or_name_missing() {
    let mut cfg = test_config(PathBuf::from("/tmp/x"));
    cfg.transport = Some("quic".to_string());

    // Missing server_addr.
    cfg.quic = Some(QuicClientConfig {
        server_addr: "  ".to_string(),
        server_name: "v4.example.test".to_string(),
        alpn: default_quic_alpn(),
        connect_timeout_secs: 10,
        keepalive_interval_secs: 20,
    });
    let err = resolve_quic_config(&cfg).unwrap_err();
    assert!(err.contains("server_addr"), "err was: {err}");

    // Missing server_name.
    cfg.quic = Some(QuicClientConfig {
        server_addr: "v4.example.test:8443".to_string(),
        server_name: String::new(),
        alpn: default_quic_alpn(),
        connect_timeout_secs: 10,
        keepalive_interval_secs: 20,
    });
    let err = resolve_quic_config(&cfg).unwrap_err();
    assert!(err.contains("server_name"), "err was: {err}");
}

#[test]
fn resolve_quic_config_rejects_keepalive_outside_supported_range() {
    let mut cfg = test_config(PathBuf::from("/tmp/x"));
    cfg.transport = Some(TRANSPORT_QUIC.to_string());

    let mut zero = quic_client_config();
    zero.keepalive_interval_secs = 0;
    cfg.quic = Some(zero);
    assert_eq!(
        resolve_quic_config(&cfg).unwrap_err(),
        "[quic] keepalive_interval_secs must be > 0"
    );

    let mut oversized = quic_client_config();
    oversized.keepalive_interval_secs = 26;
    cfg.quic = Some(oversized);
    assert_eq!(
        resolve_quic_config(&cfg).unwrap_err(),
        "[quic] keepalive_interval_secs must be <= 25"
    );

    let mut upper_bound = quic_client_config();
    upper_bound.keepalive_interval_secs = 25;
    cfg.quic = Some(upper_bound);
    assert_eq!(
        resolve_quic_config(&cfg).unwrap().keepalive_interval_secs,
        25
    );
}

#[test]
fn resolve_quic_config_accepts_valid_section() {
    let mut cfg = test_config(PathBuf::from("/tmp/x"));
    cfg.transport = Some("quic".to_string());
    cfg.quic = Some(quic_client_config());
    let resolved = resolve_quic_config(&cfg).unwrap();
    assert_eq!(resolved.server_addr, "v4.example.test:8443");
    assert_eq!(resolved.server_name, "v4.example.test");
}

#[test]
fn resolve_quic_server_addrs_accepts_hostname_port() {
    let addrs = resolve_quic_server_addrs("localhost:8443").unwrap();
    assert!(addrs.iter().any(|addr| addr.port() == 8443));
}

#[test]
fn resolve_quic_server_addrs_rejects_missing_port() {
    let err = resolve_quic_server_addrs("localhost").unwrap_err();
    assert!(err.contains("failed to resolve"), "err was: {err}");
}

#[test]
fn quic_client_bind_addr_matches_remote_address_family() {
    let v4: SocketAddr = "127.0.0.1:8443".parse().unwrap();
    let v6: SocketAddr = "[::1]:8443".parse().unwrap();
    assert!(quic_client_bind_addr_for(v4).is_ipv4());
    assert!(quic_client_bind_addr_for(v6).is_ipv6());
}

#[test]
fn runner_cli_help_and_version_exit_before_runtime() {
    let _guard = test_env_lock();
    match parse_runner_args(["--help"]).unwrap() {
        RunnerCliAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("Usage: webcodex-runner"));
            assert!(!stdout.contains("webcodex-runner init"));
            assert!(stderr.is_empty());
        }
        other => panic!("expected help exit, got {other:?}"),
    }
    match parse_runner_args(["--version"]).unwrap() {
        RunnerCliAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            assert_eq!(code, 0);
            assert!(stdout.starts_with(&format!(
                "webcodex-runner {} (commit ",
                env!("CARGO_PKG_VERSION")
            )));
            assert!(stdout.trim_end().ends_with(')'));
            assert_ne!(
                stdout,
                format!("webcodex-runner {}\n", env!("CARGO_PKG_VERSION"))
            );
            assert!(stderr.is_empty());
        }
        other => panic!("expected version exit, got {other:?}"),
    }
}

#[test]
fn runner_cli_has_no_init_alias() {
    let _guard = test_env_lock();
    let error = parse_runner_args(["init"]).unwrap_err();
    assert!(error.contains("unknown argument: init"));
}

#[test]
fn runner_version_output_includes_build_metadata() {
    let _guard = test_env_lock();
    match parse_runner_args(["-V"]).unwrap() {
        RunnerCliAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("commit "));
            assert!(stdout.starts_with("webcodex-runner "));
            assert!(stderr.is_empty());
        }
        other => panic!("expected version exit, got {other:?}"),
    }
}

#[test]
fn runner_cli_legacy_runtime_args_are_preserved() {
    let _guard = test_env_lock();
    let action = parse_runner_args(["--config", "/tmp/agent.toml", "--once"]).unwrap();
    assert_eq!(
        action,
        RunnerCliAction::Run {
            config_path: PathBuf::from("/tmp/agent.toml"),
            once: true,
            stop_on_stdin_eof: false,
        }
    );
}

#[test]
fn runner_parent_liveness_is_explicit_opt_in() {
    let _guard = test_env_lock();
    let action =
        parse_runner_args(["--config", "/tmp/runner.toml", "--stop-on-stdin-eof"]).unwrap();
    assert_eq!(
        action,
        RunnerCliAction::Run {
            config_path: PathBuf::from("/tmp/runner.toml"),
            once: false,
            stop_on_stdin_eof: true,
        }
    );
}

#[test]
fn runner_cli_config_env_prefers_runner_name_and_keeps_legacy_alias_fail_closed() {
    let _guard = test_env_lock();
    let _env = EnvGuard::new()
        .set("WEBCODEX_RUNNER_CONFIG", "/tmp/runner.toml")
        .remove("WEBCODEX_AGENT_CONFIG");
    assert_eq!(
        parse_runner_args(std::iter::empty::<&str>()).unwrap(),
        RunnerCliAction::Run {
            config_path: PathBuf::from("/tmp/runner.toml"),
            once: false,
            stop_on_stdin_eof: false,
        }
    );
    drop(_env);

    let _legacy = EnvGuard::new()
        .remove("WEBCODEX_RUNNER_CONFIG")
        .set("WEBCODEX_AGENT_CONFIG", "/tmp/agent.toml");
    assert_eq!(
        parse_runner_args(std::iter::empty::<&str>()).unwrap(),
        RunnerCliAction::Run {
            config_path: PathBuf::from("/tmp/agent.toml"),
            once: false,
            stop_on_stdin_eof: false,
        }
    );
    drop(_legacy);

    let _ambiguous = EnvGuard::new()
        .set("WEBCODEX_RUNNER_CONFIG", "/tmp/runner.toml")
        .set("WEBCODEX_AGENT_CONFIG", "/tmp/agent.toml");
    let error = parse_runner_args(std::iter::empty::<&str>()).unwrap_err();
    assert!(error.contains("cannot both be set"));

    assert_eq!(
        parse_runner_args(["--config", "/tmp/explicit.toml"]).unwrap(),
        RunnerCliAction::Run {
            config_path: PathBuf::from("/tmp/explicit.toml"),
            once: false,
            stop_on_stdin_eof: false,
        },
        "an explicit --config path must not be blocked by conflicting default-path env aliases"
    );
    assert_eq!(
        parse_runner_args(["--profile", "special"]).unwrap(),
        RunnerCliAction::Run {
            config_path: client_profile_runner_config("special").unwrap(),
            once: false,
            stop_on_stdin_eof: false,
        },
        "an explicit profile must not be blocked by conflicting default-path env aliases"
    );
}

#[test]
fn runner_profile_config_resolution_accepts_legacy_only_and_rejects_dual_files() {
    let _guard = test_env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new()
        .set("XDG_CONFIG_HOME", tmp.path())
        .set("APPDATA", tmp.path())
        .set("USERPROFILE", tmp.path())
        .remove("WEBCODEX_RUNNER_CONFIG")
        .remove("WEBCODEX_AGENT_CONFIG");
    let profile_dir = tmp.path().join("webcodex/clients/special");
    std::fs::create_dir_all(&profile_dir).unwrap();

    assert_eq!(
        client_profile_runner_config("special").unwrap(),
        profile_dir.join("runner.toml")
    );
    std::fs::write(profile_dir.join("agent.toml"), "legacy").unwrap();
    assert_eq!(
        client_profile_runner_config("special").unwrap(),
        profile_dir.join("agent.toml")
    );
    std::fs::write(profile_dir.join("runner.toml"), "current").unwrap();
    let error = client_profile_runner_config("special").unwrap_err();
    assert!(error.contains("refusing to guess"));
}

#[test]
fn runner_cli_profile_derives_default_config_path() {
    let _guard = test_env_lock();
    let action = parse_runner_args(["--profile", "special"]).unwrap();
    assert_eq!(
        action,
        RunnerCliAction::Run {
            config_path: client_profile_runner_config("special").unwrap(),
            once: false,
            stop_on_stdin_eof: false,
        }
    );
}

#[test]
fn runner_cli_explicit_config_overrides_profile() {
    let _guard = test_env_lock();
    let action =
        parse_runner_args(["--profile", "special", "--config", "/tmp/agent.toml"]).unwrap();
    assert_eq!(
        action,
        RunnerCliAction::Run {
            config_path: PathBuf::from("/tmp/agent.toml"),
            once: false,
            stop_on_stdin_eof: false,
        }
    );
}

#[test]
fn runner_cli_rejects_unsafe_profile() {
    let _guard = test_env_lock();
    let err = parse_runner_args(["--profile", "../x"]).unwrap_err();
    assert_eq!(err, CLIENT_PROFILE_ERROR);
}

#[test]
fn empty_tokens_config_parser_accepts_empty_and_whitespace_token() {
    for token in ["", "   "] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runner.toml");
        std::fs::write(
                &path,
                format!(
                    "server_url = \"http://127.0.0.1:8000\"\ntoken = \"{}\"\nclient_id = \"open-agent\"\nproject_registry_dir = \"project-registry\"\n[policy]\nallow_cwd_anywhere = true\nallowed_roots = [\".\"]\n",
                    token
                ),
            )
            .unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.token, token);
        assert_eq!(non_empty_token(&cfg.token), None);
    }
}

#[test]
fn runner_config_host_context_is_normalized_closed_and_restart_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runner.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[host_context]
role = " server_host "
runtime = " Prefer this Runner for Server-host operations. "
service = "Use the ordinary host-local service mechanism."

[policy]
allow_cwd_anywhere = true
allowed_roots = ["."]
"#,
    )
    .unwrap();
    let cfg = load_config(&path).unwrap();
    let context = cfg.host_context.as_ref().expect("host context");
    assert_eq!(context.role.as_deref(), Some("server_host"));
    assert_eq!(
        context.runtime.as_deref(),
        Some("Prefer this Runner for Server-host operations.")
    );

    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"
[host_context]
role = "server_host"
arbitrary = "not allowed"
[policy]
allow_cwd_anywhere = true
"#,
    )
    .unwrap();
    let err = load_config(&path).unwrap_err();
    assert!(err.contains("failed to parse config"), "{err}");
    assert!(err.contains("arbitrary"), "{err}");
}

#[test]
fn runner_config_without_shell_section_parses() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[policy]
allow_cwd_anywhere = true
allowed_roots = ["."]
"#,
    )
    .unwrap();

    let cfg = load_config(&path).unwrap();
    assert_eq!(cfg.shell, ShellConfig::default());
    assert_eq!(cfg.shell.max_persistent_shells, 8);
    assert_eq!(cfg.shell.persistent_shell_idle_timeout_secs, 30 * 60);
}

#[test]
fn runner_config_persistent_shell_limits_are_validated() {
    let mut shell = ShellConfig {
        max_persistent_shells: 0,
        ..Default::default()
    };
    assert!(validate_shell_config(&shell)
        .unwrap_err()
        .contains("max_persistent_shells"));

    shell.max_persistent_shells = 65;
    assert!(validate_shell_config(&shell)
        .unwrap_err()
        .contains("max_persistent_shells"));

    shell.max_persistent_shells = 8;
    shell.persistent_shell_idle_timeout_secs = 0;
    assert!(validate_shell_config(&shell)
        .unwrap_err()
        .contains("persistent_shell_idle_timeout_secs"));

    shell.persistent_shell_idle_timeout_secs = 86_401;
    assert!(validate_shell_config(&shell)
        .unwrap_err()
        .contains("persistent_shell_idle_timeout_secs"));
}

#[test]
fn runner_config_loads_named_ssh_resources_without_authentication_material() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[policy]
allowed_roots = ["."]

[ssh.resources.tmp]
host = "tmp"
default_cwd = "/opt/webcodex-edge"

[ssh.resources.no_default]
host = "ops-alias"
"#,
    )
    .unwrap();

    let cfg = load_config(&path).unwrap();
    let tmp = cfg.ssh.resources.get("tmp").unwrap();
    assert_eq!(tmp.host, "tmp");
    assert_eq!(tmp.default_cwd.as_deref(), Some("/opt/webcodex-edge"));
    assert_eq!(
        cfg.ssh
            .resources
            .get("no_default")
            .and_then(|resource| resource.default_cwd.as_deref()),
        None
    );
}

#[test]
fn runner_config_shell_profiles_parse() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[policy]
allow_cwd_anywhere = true
allowed_roots = ["."]

[shell]
default_profile = "rust"

[shell.profiles.rust]
description = "Rust development tools"
program = "sh"
args = ["-c"]
init_script = '''
export RUST_BACKTRACE=1
'''

[shell.profiles.rust.env]
PATH = "/root/.cargo/bin:/usr/bin:/bin"
CARGO_HOME = "/root/.cargo"
RUSTUP_HOME = "/root/.rustup"

[shell.profiles.py-venv]
description = "Project-local Python virtual environment"
program = "bash"
args = ["-lc"]
init_script = '''
source .venv/bin/activate
'''
"#,
    )
    .unwrap();

    let cfg = load_config(&path).unwrap();
    assert_eq!(cfg.shell.default_profile.as_deref(), Some("rust"));
    let rust = cfg.shell.profiles.get("rust").unwrap();
    assert_eq!(rust.description.as_deref(), Some("Rust development tools"));
    assert_eq!(rust.program.as_deref(), Some("sh"));
    assert_eq!(rust.args.as_ref().unwrap(), &vec!["-c".to_string()]);
    assert_eq!(
        rust.env.get("CARGO_HOME").map(String::as_str),
        Some("/root/.cargo")
    );
    assert!(rust
        .init_script
        .as_deref()
        .unwrap()
        .contains("RUST_BACKTRACE=1"));
    assert!(cfg.shell.profiles.contains_key("py-venv"));
}

#[test]
fn runner_config_shell_default_profile_must_exist() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[policy]
allow_cwd_anywhere = true
allowed_roots = ["."]

[shell]
default_profile = "missing"

[shell.profiles.rust]
program = "sh"
"#,
    )
    .unwrap();

    let err = load_config(&path).unwrap_err();
    assert!(err.contains("default_profile"), "{err}");
    assert!(err.contains("missing"), "{err}");
}

#[test]
fn runner_config_shell_profile_name_must_be_safe() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[policy]
allow_cwd_anywhere = true
allowed_roots = ["."]

[shell.profiles."bad/name"]
program = "sh"
"#,
    )
    .unwrap();

    let err = load_config(&path).unwrap_err();
    assert!(err.contains("shell profile name"), "{err}");
    assert!(err.contains("slash"), "{err}");
}

#[test]
fn runner_config_shell_profile_type_errors_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[policy]
allow_cwd_anywhere = true

[shell.profiles.rust]
args = "-c"
"#,
    )
    .unwrap();

    let err = load_config(&path).unwrap_err();
    assert!(err.contains("failed to parse config"), "{err}");
    assert!(err.contains("args"), "{err}");
}

#[test]
fn runner_config_shell_profile_env_type_errors_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    std::fs::write(
        &path,
        r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[policy]
allow_cwd_anywhere = true

[shell.profiles.rust.env]
PATH = ["/root/.cargo/bin"]
"#,
    )
    .unwrap();

    let err = load_config(&path).unwrap_err();
    assert!(err.contains("failed to parse config"), "{err}");
    assert!(err.contains("env"), "{err}");
}

#[test]
fn runner_config_shell_errors_do_not_include_init_script_body() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    let secret = "DO_NOT_LEAK_THIS_INLINE_SCRIPT_BODY";
    std::fs::write(
        &path,
        format!(
            r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"
project_registry_dir = "project-registry"

[policy]
allow_cwd_anywhere = true
allowed_roots = ["."]

[shell]
default_profile = "missing"

[shell.profiles.rust]
init_script = '''
export SECRET={}
'''
"#,
            secret
        ),
    )
    .unwrap();

    let err = load_config(&path).unwrap_err();
    assert!(err.contains("default_profile"), "{err}");
    assert!(!err.contains(secret), "{err}");
}

#[test]
fn runner_project_toml_parse_sorts_hook_names() {
    let project = parse_runner_project_toml(
        r#"
id = "webcodex"
path = "/root/git/webcodex"
kind = "rust"
shell_profile = "rust"

[hooks]
precommit = ["cargo test"]
doctor = ["git status --short"]
"#,
    )
    .unwrap();
    let summary = runner_project_summary(&project, 123456, false);
    assert_eq!(summary.id, "webcodex");
    assert_eq!(summary.name.as_deref(), Some("webcodex"));
    assert_eq!(summary.path, "/root/git/webcodex");
    assert_eq!(summary.kind.as_deref(), Some("rust"));
    assert_eq!(summary.registration_source.as_deref(), Some("explicit"));
    assert_eq!(summary.hooks, vec!["doctor", "precommit"]);
    assert_eq!(summary.updated_at, 123456);
    assert_eq!(summary.git_branch, None);
    assert_eq!(project.shell_profile.as_deref(), Some("rust"));
}

#[test]
fn runner_project_toml_rejects_invalid_id() {
    let err = parse_runner_project_toml(
        r#"
id = "bad id"
path = "/tmp/webcodex"
"#,
    )
    .unwrap_err();
    assert!(err.contains("ASCII letters"));
}

#[test]
fn runner_project_toml_hints_when_server_projects_format_is_used() {
    let err = parse_runner_project_toml(
        r#"
[projects.smoke]
path = "/root/webcodex-smoke"
"#,
    )
    .unwrap_err();
    assert!(err.contains("missing field"), "{err}");
    assert!(err.contains("server projects.toml"), "{err}");
    assert!(
        err.contains("Runner project registration records must use top-level fields"),
        "{err}"
    );
    assert!(err.contains("id = \"smoke\""), "{err}");
    assert!(err.contains("path = \"/path/to/repo\""), "{err}");
}

#[test]
fn runner_project_toml_rejects_invalid_shell_profile() {
    let err = parse_runner_project_toml(
        r#"
id = "demo"
path = "/tmp/webcodex"
shell_profile = "../rust"
"#,
    )
    .unwrap_err();
    assert!(err.contains("project.shell_profile"), "{err}");
}

#[test]
fn missing_project_registry_dir_returns_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing-project-registry");
    let projects = load_runner_project_summaries_from_dir(&missing);
    assert!(projects.is_empty());
}

#[test]
fn phase_e2_max_concurrent_jobs_uses_valid_configured_value() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path().join("config/project-registry"));
    assert_eq!(DEFAULT_MAX_CONCURRENT_JOBS, 4);
    assert_eq!(max_concurrent_jobs(&cfg), DEFAULT_MAX_CONCURRENT_JOBS);

    for value in [1, 4, 8, 64] {
        cfg.max_concurrent_jobs = Some(value);
        assert_eq!(max_concurrent_jobs(&cfg), value);
    }
}

#[test]
fn runner_config_rejects_max_concurrent_jobs_outside_valid_range() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    for value in [0, 65] {
        std::fs::write(
            &path,
            format!(
                r#"server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
project_registry_dir = "project-registry"
max_concurrent_jobs = {value}

[policy]
allow_cwd_anywhere = true
"#
            ),
        )
        .unwrap();
        let error = load_config(&path).unwrap_err();
        assert!(
            error.contains("max_concurrent_jobs must be between 1 and 64"),
            "configured={value}: {error}"
        );
    }
}

#[test]
fn polling_dispatch_and_job_execution_concurrency_have_independent_contracts() {
    assert_eq!(POLLING_DISPATCH_MAX_IN_FLIGHT, 4);
    assert_eq!(DEFAULT_MAX_CONCURRENT_JOBS, 4);
    // These happen to share a numeric default today, but polling capacity is
    // ordinary request admission while max_concurrent_jobs is a configured,
    // queue-aware JobManager limit advertised by Runner registration.
}

fn mcp_test_toml_path(path: &std::path::Path) -> String {
    serde_json::to_string(path.to_string_lossy().as_ref()).unwrap()
}

#[test]
fn runner_config_accepts_static_literal_mcp_gateway_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    let executable = mcp_test_toml_path(&std::env::current_exe().unwrap());
    let cwd_value = tmp.path().to_string_lossy().into_owned();
    let cwd = serde_json::to_string(&cwd_value).unwrap();
    std::fs::write(
        &path,
        format!(
            r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
project_registry_dir = "project-registry"

[policy]
allowed_roots = ["."]

[mcp]
request_timeout_secs = 7

[[mcp.providers]]
id = "local-tools"
name = "Local tools"
executable = {executable}
args = ["--stdio", "$HOME", "$(id)"]
cwd = {cwd}
env_from_env = {{ GITHUB_TOKEN = "GITHUB_TOKEN", HOME = "HOME" }}
timeout_secs = 5
"#
        ),
    )
    .unwrap();

    let config = load_config(&path).unwrap();
    assert_eq!(config.mcp_gateway.request_timeout_secs, 7);
    assert_eq!(config.mcp_gateway.providers.len(), 1);
    assert_eq!(config.mcp_gateway.providers[0].timeout_secs, Some(5));
    assert_eq!(
        config.mcp_gateway.providers[0].args,
        ["--stdio", "$HOME", "$(id)"]
    );
    assert_eq!(
        config.mcp_gateway.providers[0].cwd.as_deref(),
        Some(cwd_value.as_str())
    );
    assert_eq!(
        config.mcp_gateway.providers[0]
            .env_from_env
            .get("GITHUB_TOKEN")
            .map(String::as_str),
        Some("GITHUB_TOKEN")
    );
}

#[test]
fn runner_config_mcp_gateway_provider_timeout_defaults_to_gateway_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("agent.toml");
    let executable = mcp_test_toml_path(&std::env::current_exe().unwrap());
    std::fs::write(
        &path,
        format!(
            r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
project_registry_dir = "project-registry"

[policy]
allowed_roots = ["."]

[mcp]
request_timeout_secs = 11

[[mcp.providers]]
id = "inherits"
name = "Inherits"
executable = {executable}
"#
        ),
    )
    .unwrap();

    let config = load_config(&path).unwrap();
    assert_eq!(config.mcp_gateway.request_timeout_secs, 11);
    assert_eq!(config.mcp_gateway.providers[0].timeout_secs, None);
}

#[test]
fn runner_config_rejects_unsafe_or_ambiguous_mcp_gateway_identity() {
    let executable = mcp_test_toml_path(&std::env::current_exe().unwrap());
    for (providers, expected) in [
        (
            r#"
[[mcp.providers]]
id = "local"
name = "Local"
executable = "relative-mcp"
"#
            .to_string(),
            "executable must be an absolute path",
        ),
        (
            format!(
                r#"
[[mcp.providers]]
id = "bad-timeout"
name = "Bad timeout"
executable = {executable}
timeout_secs = 121
"#
            ),
            "timeout_secs must be between 1 and 120",
        ),
        (
            format!(
                r#"
[[mcp.providers]]
id = "duplicate"
name = "First"
executable = {executable}

[[mcp.providers]]
id = "duplicate"
name = "Second"
executable = {executable}
"#
            ),
            "provider ids must be unique",
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            format!(
                r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
project_registry_dir = "project-registry"

[policy]
allowed_roots = ["."]

[mcp]
request_timeout_secs = 30
{providers}
"#
            ),
        )
        .unwrap();
        let error = load_config(&path).unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
}
