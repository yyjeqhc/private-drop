use super::support::*;

#[test]
fn bare_interactive_repo_defaults_to_share_only_on_supported_desktop_unix() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let nested = repo.join("src/nested");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();

    assert_eq!(
        first_run_default_args(Vec::new(), Some(&nested), "linux", true),
        vec!["share".to_string()]
    );
    assert_eq!(
        first_run_default_args(Vec::new(), Some(&nested), "macos", true),
        vec!["share".to_string()]
    );
    assert!(first_run_default_args(Vec::new(), Some(&nested), "windows", true).is_empty());
    assert!(first_run_default_args(Vec::new(), Some(&nested), "linux", false).is_empty());
    assert!(first_run_default_args(Vec::new(), Some(temp.path()), "linux", true).is_empty());

    let explicit = vec!["--help".to_string()];
    assert_eq!(
        first_run_default_args(explicit.clone(), Some(&nested), "linux", true),
        explicit
    );
}

#[test]
fn bare_interactive_linked_worktree_marker_also_defaults_to_share() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("worktree");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join(".git"), "gitdir: /tmp/example\n").unwrap();
    assert_eq!(
        first_run_default_args(Vec::new(), Some(&repo), "linux", true),
        vec!["share".to_string()]
    );
}

#[test]
fn cli_help_and_version_exit_before_dispatch() {
    match cli_action(["--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("Usage: webcodex"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
    match cli_action(["--version"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.starts_with(&format!("webcodex {} (commit ", env!("CARGO_PKG_VERSION"))));
            assert!(stdout.trim_end().ends_with(')'));
            assert_ne!(stdout, format!("webcodex {}\n", env!("CARGO_PKG_VERSION")));
        }
        other => panic!("expected version exit, got {other:?}"),
    }
}

#[test]
fn cli_version_output_includes_build_metadata() {
    match cli_action(["-V"]) {
        CliAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("commit "));
            assert!(stdout.starts_with("webcodex "));
            assert!(stderr.is_empty());
        }
        other => panic!("expected version exit, got {other:?}"),
    }
}

#[test]
fn project_doctor_and_hosted_connect_dispatch() {
    assert!(matches!(
        cli_action(["project", "register", "--config", "/tmp/runner.toml", "/tmp/repo"]),
        CliAction::ProjectRegister(opts)
            if opts.config == std::path::PathBuf::from("/tmp/runner.toml")
                && opts.project == std::path::PathBuf::from("/tmp/repo")
                && !opts.json
    ));
    assert!(matches!(
        cli_action([
            "project",
            "activate",
            "--config",
            "/tmp/runner.toml",
            "--user-token-file",
            "/tmp/user-token",
            "/tmp/repo-b",
            "--json",
        ]),
        CliAction::ProjectActivate(opts)
            if opts.config == std::path::PathBuf::from("/tmp/runner.toml")
                && opts.user_token_file == std::path::PathBuf::from("/tmp/user-token")
                && opts.project == std::path::PathBuf::from("/tmp/repo-b")
                && opts.json
    ));
    assert!(matches!(
        cli_action(["doctor"]),
        CliAction::Project(args) if args == ["doctor"]
    ));
    assert!(matches!(
        cli_action(["share", "--tunnel", "none"]),
        CliAction::Project(args) if args == ["share", "--tunnel", "none"]
    ));
    assert!(matches!(
        cli_action(["connect", "https://example.test", "--key", "shared-secret"]),
        CliAction::Connect(_)
    ));
}

#[test]
fn webcodex_cli_help_presents_primary_mental_model() {
    let stdout = cli_exit(["--help"]).unwrap();
    for section in [
        "Quick trial:",
        "Daily self-hosted setup:",
        "Existing Server:",
        "Project / diagnostics:",
        "Account:",
        "Advanced / operator:",
    ] {
        assert!(stdout.contains(section), "help missing {section}: {stdout}");
    }
    for command in [
        "pairing create",
        "project register",
        "project activate",
        "auth status",
        "tokens",
        "runner-tokens",
    ] {
        assert!(stdout.contains(command), "help missing {command}: {stdout}");
    }
    assert!(stdout.contains("webcodex server --help"));
    assert!(stdout.contains("webcodex runner --help"));
    assert!(!stdout.contains("setup single-user"));
    assert!(!stdout.contains("client enroll"));
    assert!(!stdout.contains("server init|install|run|start|stop|restart|status|logs|uninstall"));
    assert!(!stdout.contains("runner init|install|run|start|stop|restart|status|logs|uninstall"));
}

#[test]
fn project_registration_activation_and_login_help_prioritize_user_language() {
    let project_help = cli_exit(["project", "register", "--help"]).unwrap();
    assert!(project_help.contains("Add one existing project to a Runner configuration"));
    assert!(project_help.contains("Advanced: project_registry_dir"));
    assert!(project_help.contains("not a workspace root"));
    assert!(project_help.contains("allowed_roots"));
    let activate_help = cli_exit(["project", "activate", "--help"]).unwrap();
    assert!(activate_help.contains("Activate one explicitly selected local project"));
    assert!(activate_help.contains("canonical exact project root"));
    assert!(activate_help.contains("generation CAS"));
    assert!(activate_help.contains("--user-token-file PATH"));
    let login_help = cli_exit(["login", "--help"]).unwrap();
    assert!(login_help.contains("one-time login code"));
    assert!(login_help.contains("--project PATH"));
    assert!(login_help.contains("--code-stdin"));
    assert!(login_help.contains("projects may be added later"));

    assert!(matches!(
        cli_action([
            "login",
            "https://example.test",
            "--code",
            "wc_pair_example",
            "--allowed-root",
            "/tmp",
            "--project",
            "/tmp/repo",
        ]),
        CliAction::Login(opts)
            if opts.allowed_roots == vec![std::path::PathBuf::from("/tmp")]
                && opts.project == Some(std::path::PathBuf::from("/tmp/repo"))
    ));

    match cli_action([
        "login",
        "https://example.test",
        "--code",
        "wc_pair_example",
        "--code-stdin",
    ]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(stderr.contains("use only one of --code or --code-stdin"));
        }
        other => panic!("expected pairing-code source conflict, got {other:?}"),
    }
}

#[test]
fn foreground_run_banner_describes_lifetime_without_false_readiness() {
    let server = foreground_run_banner("Server");
    assert!(server.contains("Starting WebCodex Server in the foreground."));
    assert!(server.contains("Keep this terminal open."));
    assert!(server.contains("Ctrl-C stops the Server."));
    assert!(!server.contains("Server is running"));

    let runner = foreground_run_banner("Runner");
    assert!(runner.contains("Starting WebCodex Runner in the foreground."));
    assert!(runner.contains("Ctrl-C stops the Runner."));
    assert!(!runner.contains("Runner connected"));
}

#[test]
fn common_help_entrypoints_smoke() {
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["--help"],
            &[
                "Usage: webcodex [COMMAND]",
                "Quick trial:",
                "Daily self-hosted setup:",
                "Existing Server:",
                "Project / diagnostics:",
                "Advanced / operator:",
            ],
        ),
        (
            &["server", "--help"],
            &[
                "Usage: webcodex server <COMMAND>",
                "Commands:",
                "init",
                "install",
                "run",
                "uninstall",
            ],
        ),
        (
            &["runner", "--help"],
            &[
                "Usage: webcodex runner <COMMAND>",
                "Linux systemd Runner service",
                "foreground (all supported platforms)",
                "Linux systemd service commands",
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
fn top_level_help_prioritizes_primary_paths_without_hiding_operator_surface() {
    let out = cli_exit(["--help"]).unwrap();
    let quick = out.find("Quick trial:").unwrap();
    let daily = out.find("Daily self-hosted setup:").unwrap();
    let existing = out.find("Existing Server:").unwrap();
    let project = out.find("Project / diagnostics:").unwrap();
    let account = out.find("Account:").unwrap();
    let advanced = out.find("Advanced / operator:").unwrap();
    assert!(quick < daily && daily < existing && existing < project);
    assert!(project < account && account < advanced);
    assert!(out.contains("(no command)"));
    assert!(out.contains("Interactive Git repo shortcut for `share` on Linux/macOS"));
    assert!(out.contains("Temporarily share one project; ends when the command exits"));
    assert!(out
        .lines()
        .any(|line| line.trim_start().starts_with("tokens ")));
    assert!(out
        .lines()
        .any(|line| line.trim_start().starts_with("runner-tokens ")));
    assert!(!out
        .lines()
        .any(|line| line.trim_start().starts_with("token ")));
    assert!(!out
        .lines()
        .any(|line| line.trim_start().starts_with("agent-token ")));
}

#[test]
fn unified_project_and_auth_commands_dispatch() {
    for args in [
        &["setup", "--help"][..],
        &["status", "--help"][..],
        &["doctor", "--help"][..],
        &["task", "--help"][..],
        &["run", "--help"][..],
        &["share", "--help"][..],
    ] {
        assert!(matches!(
            cli_action(args.iter().copied()),
            CliAction::Project(_)
        ));
    }
    match cli_action(["auth", "status", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("Usage: webcodex auth status"));
        }
        other => panic!("expected auth status help, got {other:?}"),
    }
}

#[test]
fn webcodex_cli_runner_help_mentions_lifecycle_subcommands() {
    match cli_action(["runner", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            for command in [
                "init",
                "install",
                "run",
                "start",
                "stop",
                "restart",
                "status",
                "logs",
                "uninstall",
            ] {
                assert!(
                    stdout.contains(command),
                    "runner help missing {command}: {stdout}"
                );
            }
            assert!(stdout.contains("webcodex run"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
    match cli_action(["runner", "init", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("Usage: webcodex runner init"));
            assert!(stdout.contains("Stable Runner client id"));
            assert!(stdout.contains("Human-readable Runner name"));
            assert!(stdout.contains("runner.toml"));
        }
        other => panic!("expected Runner init help exit, got {other:?}"),
    }
    match cli_action(["runner", "install", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("--config PATH"));
            assert!(stdout.contains("--bin PATH"));
            assert!(stdout.contains("--scope user|system"));
            assert!(stdout.contains("--allow-root-runner"));
            assert!(stdout.contains("default: user for non-root"));
            assert!(stdout.contains("Tokens are never inlined"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
    match cli_action(["runner", "restart", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("--bin PATH"));
            assert!(stdout.contains("hosted profile"));
            assert!(stdout.contains("does not accept --bin"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
    match cli_action(["runner", "status", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("--user-token-file PATH"));
            assert!(stdout.contains("--runner-token-file PATH"));
            assert!(!stdout.contains("--agent-token-file PATH"));
            assert!(stdout.contains("--scope user|system"));
            assert!(stdout.contains("--service-file PATH"));
            assert!(stdout.contains("Runner config path"));
            assert!(stdout.contains("runner.toml"));
            assert!(stdout.contains("no tokens"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
}

#[test]
fn runner_namespace_owns_all_lifecycle_commands_and_agent_is_unknown() {
    let top_help = cli_exit(["--help"]).unwrap();
    assert!(top_help
        .lines()
        .any(|line| line.trim_start().starts_with("runner ")));
    assert!(!top_help.contains("runner init|install|run|start|stop|restart|status|logs|uninstall"));
    assert!(!top_help.contains("  agent init|install|run|start|stop|restart|status|logs|uninstall"));

    for command in [
        "init",
        "install",
        "run",
        "start",
        "stop",
        "restart",
        "status",
        "logs",
        "uninstall",
    ] {
        match cli_action(["runner", command, "--help"]) {
            CliAction::Exit { code, stdout, .. } => {
                assert_eq!(code, 0, "runner {command} help must dispatch");
                assert!(
                    stdout.contains("webcodex runner"),
                    "runner {command} help did not stay in Runner namespace: {stdout}"
                );
            }
            other => panic!("runner {command} did not dispatch to lifecycle help: {other:?}"),
        }

        match cli_action(["agent", command]) {
            CliAction::Exit {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 2);
                assert!(stdout.is_empty());
                assert!(
                    stderr.starts_with("unknown command: agent\n"),
                    "legacy agent namespace was still recognized for {command}: {stderr}"
                );
            }
            other => panic!("legacy agent {command} produced a lifecycle action: {other:?}"),
        }
    }
}

#[test]
fn existing_removed_lifecycle_stubs_stay_fail_closed() {
    for (args, replacement) in [
        (vec!["server", "up"], "webcodex server init"),
        (vec!["server", "install-service"], "webcodex server install"),
        (vec!["runner", "install-service"], "webcodex runner install"),
    ] {
        match cli_action(args) {
            CliAction::Exit {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 2);
                assert!(stdout.is_empty());
                assert!(stderr.contains("removed"), "{stderr}");
                assert!(stderr.contains(replacement), "{stderr}");
            }
            other => panic!("removed lifecycle path still dispatched: {other:?}"),
        }
    }
}

#[test]
fn removed_legacy_onboarding_paths_fail_closed_with_migration_guidance() {
    for (args, replacement) in [
        (
            vec!["setup", "single-user", "--help"],
            "webcodex pairing create",
        ),
        (vec!["client", "enroll", "--help"], "webcodex login"),
    ] {
        match cli_action(args) {
            CliAction::Exit {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 2);
                assert!(stdout.is_empty());
                assert!(stderr.contains("removed"), "{stderr}");
                assert!(stderr.contains(replacement), "{stderr}");
            }
            other => panic!("removed path still dispatched: {other:?}"),
        }
    }
}

#[test]
fn canonical_plural_local_credential_namespaces_dispatch() {
    assert!(matches!(
        cli_action([
            "tokens",
            "create-local",
            "--server-url",
            "https://example.test",
            "--username",
            "alice",
            "--credential",
            "wc_acct_example",
        ]),
        CliAction::TokenCreateLocal(_)
    ));
    assert!(matches!(
        cli_action([
            "runner-tokens",
            "create-local",
            "--server-url",
            "https://example.test",
            "--username",
            "alice",
            "--credential",
            "wc_acct_example",
            "--client-id",
            "runner-1",
        ]),
        CliAction::RunnerTokenCreateLocal(_)
    ));
}

#[test]
fn canonical_plural_admin_actions_dispatch_and_singular_groups_fail_closed() {
    assert!(matches!(
        cli_action([
            "tokens",
            "list",
            "--server-url",
            "https://example.test",
            "--username",
            "alice",
        ]),
        CliAction::Admin(_)
    ));
    assert!(matches!(
        cli_action([
            "runner-tokens",
            "list",
            "--server-url",
            "https://example.test",
            "--username",
            "alice",
        ]),
        CliAction::Admin(_)
    ));

    for (group, replacement) in [
        ("token", "webcodex tokens"),
        ("agent-token", "webcodex runner-tokens"),
    ] {
        match cli_action([group, "list"]) {
            CliAction::Exit {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 2);
                assert!(stdout.is_empty());
                assert!(stderr.contains("removed"), "{stderr}");
                assert!(stderr.contains(replacement), "{stderr}");
            }
            other => panic!("singular credential group still dispatched: {other:?}"),
        }
    }
}

#[test]
fn legacy_agent_tokens_alias_uses_canonical_runner_token_implementation() {
    assert!(matches!(
        cli_action([
            "agent-tokens",
            "create-local",
            "--server-url",
            "https://example.test",
            "--username",
            "alice",
            "--credential",
            "wc_acct_example",
            "--client-id",
            "runner-1",
        ]),
        CliAction::RunnerTokenCreateLocal(_)
    ));
    assert!(matches!(
        cli_action([
            "agent-tokens",
            "list",
            "--server-url",
            "https://example.test",
            "--username",
            "alice",
        ]),
        CliAction::Admin(_)
    ));
}

#[test]
fn users_namespace_is_plural_only() {
    assert!(matches!(
        cli_action(["users", "list", "--server-url", "https://example.test",]),
        CliAction::Admin(_)
    ));
    match cli_action(["user", "list"]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(stderr.starts_with("unknown command: user"), "{stderr}");
        }
        other => panic!("singular user namespace still dispatched: {other:?}"),
    }
}

#[test]
fn usage_lists_only_canonical_credential_group_spellings() {
    let stdout = cli_exit(["--help"]).unwrap();
    assert!(stdout
        .lines()
        .any(|line| line.trim_start().starts_with("tokens ")));
    assert!(stdout
        .lines()
        .any(|line| line.trim_start().starts_with("runner-tokens ")));
    assert!(!stdout
        .lines()
        .any(|line| line.trim_start().starts_with("token ")));
    assert!(!stdout
        .lines()
        .any(|line| line.trim_start().starts_with("agent-token ")));
    assert!(!stdout
        .lines()
        .any(|line| line.trim_start().starts_with("agent-tokens ")));
}

#[test]
fn removed_local_credential_flag_aliases_are_rejected() {
    for (args, flag) in [
        (
            vec!["tokens", "create-local", "--server", "https://example.test"],
            "--server",
        ),
        (vec!["tokens", "create-local", "--user", "alice"], "--user"),
        (
            vec!["runner-tokens", "create-local", "--admin-token", "secret"],
            "--admin-token",
        ),
        (
            vec![
                "runner-tokens",
                "create-local",
                "--admin-token-env",
                "TOKEN_ENV",
            ],
            "--admin-token-env",
        ),
    ] {
        match cli_action(args) {
            CliAction::Exit { code, stderr, .. } => {
                assert_eq!(code, 2);
                assert!(stderr.contains(flag), "{stderr}");
                assert!(stderr.contains("unknown"), "{stderr}");
            }
            other => panic!("removed flag {flag} still dispatched: {other:?}"),
        }
    }
}

#[test]
fn removed_user_selection_aliases_are_rejected() {
    for args in [
        vec!["connect", "https://example.test", "--username", "alice"],
        vec!["logout", "https://example.test", "--username", "alice"],
    ] {
        match cli_action(args) {
            CliAction::Exit { code, stderr, .. } => {
                assert_eq!(code, 2);
                assert!(stderr.contains("--username"), "{stderr}");
                assert!(stderr.contains("unknown"), "{stderr}");
            }
            other => panic!("removed --username alias still dispatched: {other:?}"),
        }
    }
}

#[test]
fn pairing_create_help_marks_client_id_optional_and_explains_matching_device() {
    match cli_action(["pairing", "create", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(
                stdout.contains("[--client-id CLIENT_ID]"),
                "optional client id is missing from synopsis: {stdout}"
            );
            assert!(stdout.contains("let the login device claim it"), "{stdout}");
            assert!(stdout.contains("same --device value"), "{stdout}");
        }
        other => panic!("expected help exit, got {other:?}"),
    }
}

#[test]
fn login_help_describes_root_and_non_root_default_directories() {
    match cli_action(["login", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("root /etc/webcodex"), "{stdout}");
            assert!(stdout.contains("non-root ~/.config/webcodex"), "{stdout}");
        }
        other => panic!("expected help exit, got {other:?}"),
    }
}

#[test]
fn login_print_mcp_config_and_json_are_mutually_exclusive() {
    // `--json --print-mcp-config` together is a parse-time error.
    match cli_action([
        "login",
        "https://example.com",
        "--code",
        "wc_pair_x",
        "--json",
        "--print-mcp-config",
    ]) {
        CliAction::Exit { code, stderr, .. } => {
            assert_eq!(code, 2);
            assert!(
                stderr.contains("mutually exclusive"),
                "expected mutual-exclusion error, got {stderr}"
            );
        }
        other => panic!("expected a parse error, got {other:?}"),
    }
    // `--print-mcp-config` alone dispatches to Login with the flag set.
    match cli_action([
        "login",
        "https://example.com",
        "--code",
        "wc_pair_x",
        "--print-mcp-config",
    ]) {
        CliAction::Login(opts) => {
            assert!(opts.print_mcp_config);
            assert!(!opts.json);
        }
        other => panic!("expected Login dispatch, got {other:?}"),
    }
}
