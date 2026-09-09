// Every test in this module is Unix-only; the glob import is only needed there.
#[cfg(unix)]
use super::super::support::*;

#[cfg(unix)]
fn server_env(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let path = tmp.path().join("webcodex.env");
    std::fs::write(
        &path,
        "WEBCODEX_ADDR=127.0.0.1:8080\nWEBCODEX_TOKEN=secret-never-inline\n",
    )
    .unwrap();
    path
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn install_service_generates_expected_unit_without_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = server_env(&tmp);
    let opts = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--working-directory",
        "/var/lib/webcodex",
        "--user",
        "webcodex",
        "--group",
        "webcodex",
        "--dry-run",
    ]))
    .unwrap();
    let unit = run_server_install_service(opts).unwrap();
    assert!(unit.contains("[Unit]\nDescription=WebCodex Runtime\n"));
    assert!(unit.contains(&format!("EnvironmentFile={}\n", env_file.display())));
    assert!(unit.contains("ListenStream=127.0.0.1:8080\n"));
    assert!(unit.contains("FileDescriptorName=webcodex-http\n"));
    assert!(unit.contains("Service=webcodex.service\n"));
    assert!(unit.contains("# /etc/systemd/system/webcodex.socket\n"));
    assert!(unit.contains("ExecStart=\"/usr/local/bin/webcodex-server\"\n"));
    assert!(unit.contains("TimeoutStopSec=330s\n"));
    assert!(
        webcodex::SERVER_SYSTEMD_TIMEOUT_STOP_SECS
            > webcodex::SERVER_GRACEFUL_SHUTDOWN_TIMEOUT_SECS
    );
    assert!(unit.contains("WorkingDirectory=/var/lib/webcodex\n"));
    assert!(unit.contains("User=webcodex\n"));
    assert!(unit.contains("Group=webcodex\n"));
    assert!(unit.contains("WantedBy=multi-user.target\n"));
    assert!(!unit.contains("WEBCODEX_TOKEN"));
    assert!(!unit.contains("secret-never-inline"));
    assert!(!unit.contains("wc_boot_"));
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn user_runner_unit_uses_user_target_without_identity_directives_or_root_workdir() {
    let opts = parse_runner_install_service_with_identity(
        &args(&[
            "--scope",
            "user",
            "--config",
            "/home/alice/.config/webcodex/runner.toml",
            "--service-file",
            "/home/alice/.config/systemd/user/webcodex-runner.service",
            "--bin",
            "/home/alice/.local/bin/webcodex-runner",
            "--working-directory",
            "/home/alice",
            "--dry-run",
        ]),
        false,
    )
    .unwrap();
    let unit = run_runner_install_service(opts).unwrap();
    assert!(unit.contains("WantedBy=default.target\n"));
    assert!(!unit.contains("network-online.target"));
    assert!(unit.contains("WorkingDirectory=/home/alice\n"));
    assert!(!unit.contains("WorkingDirectory=/root\n"));
    assert!(!unit.contains("\nUser="));
    assert!(!unit.contains("\nGroup="));
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn explicitly_allowed_root_runner_is_visibly_marked() {
    let opts = parse_runner_install_service_with_identity(
        &args(&[
            "--scope",
            "system",
            "--bin",
            "/opt/webcodex/bin/webcodex-runner",
            "--working-directory",
            "/root",
            "--allow-root-runner",
            "--dry-run",
        ]),
        true,
    )
    .unwrap();
    let unit = run_runner_install_service(opts).unwrap();
    assert!(unit.contains("WARNING: --allow-root-runner was explicitly accepted"));
    assert!(unit.contains("WorkingDirectory=/root\n"));
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn install_service_refuses_overwrite_unless_requested() {
    // Preflight is intentionally satisfied here so this remains an overwrite test.
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let service_file = tmp.path().join("webcodex.service");
    let env_file = server_env(&tmp);
    let binary = tmp.path().join("webcodex-server");
    std::fs::write(&binary, "test binary").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&service_file, "old").unwrap();
    let opts = parse_server_install_service(&args(&[
        "--bin",
        binary.to_str().unwrap(),
        "--env-file",
        env_file.to_str().unwrap(),
        "--working-directory",
        tmp.path().to_str().unwrap(),
        "--service-file",
        service_file.to_str().unwrap(),
    ]))
    .unwrap();
    let err = run_server_install_service(opts).unwrap_err();
    assert!(err.contains("already exists"));
}

#[cfg(unix)]
#[test]
fn server_install_working_directory_follows_env_data_unless_explicit() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("server.env");
    let data_dir = tmp.path().join("custom-data");
    std::fs::write(
        &env_file,
        format!(
            "WEBCODEX_ADDR=127.0.0.1:9090\nWEBCODEX_DATA={}\n",
            data_dir.display()
        ),
    )
    .unwrap();
    let from_env = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--dry-run",
    ]))
    .unwrap();
    assert_eq!(from_env.working_directory, data_dir);

    let explicit = tmp.path().join("explicit-data");
    let overridden = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--working-directory",
        explicit.to_str().unwrap(),
        "--dry-run",
    ]))
    .unwrap();
    assert_eq!(overridden.working_directory, explicit);
}

#[cfg(unix)]
#[test]
fn server_install_preflight_rejects_missing_workdir_before_service_mutation() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("server.env");
    let missing_workdir = tmp.path().join("missing-data");
    std::fs::write(
        &env_file,
        format!(
            "WEBCODEX_ADDR=127.0.0.1:9090\nWEBCODEX_DATA={}\n",
            missing_workdir.display()
        ),
    )
    .unwrap();
    let binary = tmp.path().join("webcodex-server");
    std::fs::write(&binary, "test binary").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let service_file = tmp.path().join("webcodex.service");
    let opts = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        binary.to_str().unwrap(),
        "--service-file",
        service_file.to_str().unwrap(),
    ]))
    .unwrap();
    let error = run_server_install_service(opts).unwrap_err();
    assert!(error.contains("WorkingDirectory"), "{error}");
    assert!(error.contains("does not exist"), "{error}");
    assert!(
        !service_file.exists(),
        "preflight must precede unit mutation"
    );
    assert!(!service_file.with_extension("socket").exists());
}

#[cfg(unix)]
#[test]
fn server_install_preflight_rejects_missing_and_non_executable_binary() {
    // Binary checks are local and must fail before systemd is contacted.
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let env_file = server_env(&tmp);
    let service_file = tmp.path().join("webcodex.service");
    let missing = tmp.path().join("missing-server");
    let missing_opts = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        missing.to_str().unwrap(),
        "--working-directory",
        tmp.path().to_str().unwrap(),
        "--service-file",
        service_file.to_str().unwrap(),
    ]))
    .unwrap();
    let error = run_server_install_service(missing_opts).unwrap_err();
    assert!(error.contains("binary"), "{error}");
    assert!(error.contains("does not exist"), "{error}");
    assert!(!service_file.exists());

    let binary = tmp.path().join("not-executable-server");
    std::fs::write(&binary, "test binary").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644)).unwrap();
    let non_exec_opts = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        binary.to_str().unwrap(),
        "--working-directory",
        tmp.path().to_str().unwrap(),
        "--service-file",
        service_file.to_str().unwrap(),
    ]))
    .unwrap();
    let error = run_server_install_service(non_exec_opts).unwrap_err();
    assert!(error.contains("not executable"), "{error}");
    assert!(!service_file.exists());
}

#[cfg(unix)]
#[test]
fn server_install_preflight_rejects_unknown_user_and_group() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let env_file = server_env(&tmp);
    let binary = tmp.path().join("webcodex-server");
    std::fs::write(&binary, "test binary").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let service_file = tmp.path().join("webcodex.service");

    let unknown_user = "webcodex-o1-user-that-must-not-exist";
    let user_opts = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        binary.to_str().unwrap(),
        "--working-directory",
        tmp.path().to_str().unwrap(),
        "--service-file",
        service_file.to_str().unwrap(),
        "--user",
        unknown_user,
    ]))
    .unwrap();
    let error = run_server_install_service(user_opts).unwrap_err();
    assert!(error.contains(&format!("User={unknown_user}")), "{error}");
    assert!(!service_file.exists());

    let unknown_group = "webcodex-o1-group-that-must-not-exist";
    let group_opts = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        binary.to_str().unwrap(),
        "--working-directory",
        tmp.path().to_str().unwrap(),
        "--service-file",
        service_file.to_str().unwrap(),
        "--group",
        unknown_group,
    ]))
    .unwrap();
    let error = run_server_install_service(group_opts).unwrap_err();
    assert!(error.contains(&format!("Group={unknown_group}")), "{error}");
    assert!(!service_file.exists());
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn install_service_dry_run_and_output_work_without_systemd() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = server_env(&tmp);
    let dry = parse_server_install_service(&args(&[
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--env-file",
        env_file.to_str().unwrap(),
        "--dry-run",
    ]))
    .unwrap();
    assert!(run_server_install_service(dry)
        .unwrap()
        .contains("ExecStart=\"/usr/local/bin/webcodex-server\""));

    let out = parse_server_install_service(&args(&[
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--env-file",
        env_file.to_str().unwrap(),
        "--output",
        "-",
        "--json",
    ]))
    .unwrap();
    let json: Value = serde_json::from_str(&run_server_install_service(out).unwrap()).unwrap();
    assert_eq!(json["dry_run"], true);
    assert!(json["units"]["service"]
        .as_str()
        .unwrap()
        .contains("[Service]"));
    assert!(json["units"]["socket"]
        .as_str()
        .unwrap()
        .contains("[Socket]"));
    assert_eq!(json["listen"], "127.0.0.1:8080");
}

#[cfg(unix)]
#[test]
fn server_socket_rendering_uses_env_address_custom_sibling_and_no_start_projection() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("server.env");
    std::fs::write(
        &env_file,
        "WEBCODEX_ADDR=127.0.0.1:9090\nWEBCODEX_TOKEN=hidden\n",
    )
    .unwrap();
    let service_file = tmp.path().join("custom-webcodex.service");
    let opts = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--service-file",
        service_file.to_str().unwrap(),
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--no-start",
        "--output",
        "-",
        "--json",
    ]))
    .unwrap();
    let json: Value = serde_json::from_str(&run_server_install_service(opts).unwrap()).unwrap();
    assert_eq!(json["no_start"], true);
    assert_eq!(json["listen"], "127.0.0.1:9090");
    assert_eq!(
        json["socket_file"],
        service_file
            .with_extension("socket")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(json["service_unit"], "custom-webcodex.service");
    assert_eq!(json["socket_unit"], "custom-webcodex.socket");
    assert!(json["units"]["service"]
        .as_str()
        .unwrap()
        .contains("Requires=custom-webcodex.socket\n"));
    assert!(json["units"]["service"]
        .as_str()
        .unwrap()
        .contains("TimeoutStopSec=330s\n"));
    assert!(json["units"]["socket"]
        .as_str()
        .unwrap()
        .contains("Service=custom-webcodex.service\n"));
    assert!(!json.to_string().contains("hidden"));
}

#[cfg(unix)]
#[test]
fn server_service_actions_target_custom_service_and_sibling_socket() {
    let custom = "/etc/systemd/system/webcodex-canary.service";
    for command in ["start", "stop", "restart", "logs"] {
        let opts =
            parse_server_service_action(command, &args(&["--service-file", custom])).unwrap();
        assert_eq!(opts.service_file, std::path::PathBuf::from(custom));
        assert_eq!(opts.unit, "webcodex-canary.service");
    }
    let uninstall =
        parse_server_service_action("uninstall", &args(&["--service-file", custom, "--confirm"]))
            .unwrap();
    assert_eq!(uninstall.service_file, std::path::PathBuf::from(custom));
    assert_eq!(uninstall.unit, "webcodex-canary.service");
}

#[cfg(unix)]
#[test]
fn server_socket_rendering_fails_closed_on_missing_or_malformed_address() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing.env");
    let opts = parse_server_install_service(&args(&[
        "--env-file",
        missing.to_str().unwrap(),
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--dry-run",
    ]))
    .unwrap();
    let error = run_server_install_service(opts).unwrap_err();
    assert!(error.contains("does not exist"), "{error}");

    let malformed = tmp.path().join("malformed.env");
    std::fs::write(&malformed, "WEBCODEX_ADDR=localhost:not-a-port\n").unwrap();
    let opts = parse_server_install_service(&args(&[
        "--env-file",
        malformed.to_str().unwrap(),
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--dry-run",
    ]))
    .unwrap();
    let error = run_server_install_service(opts).unwrap_err();
    assert!(error.contains("systemd ListenStream"), "{error}");

    let absent_key = tmp.path().join("no-address.env");
    std::fs::write(&absent_key, "WEBCODEX_TOKEN=hidden\n").unwrap();
    let opts = parse_server_install_service(&args(&[
        "--env-file",
        absent_key.to_str().unwrap(),
        "--bin",
        "/usr/local/bin/webcodex-server",
        "--dry-run",
    ]))
    .unwrap();
    let error = run_server_install_service(opts).unwrap_err();
    assert!(error.contains("does not define WEBCODEX_ADDR"), "{error}");
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn runner_install_service_generates_expected_unit_without_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("runner.toml");
    std::fs::write(&config, "token = \"agent_secret_should_not_print\"\n").unwrap();
    let opts = parse_runner_install_service(&args(&[
        "--scope",
        "system",
        "--config",
        config.to_str().unwrap(),
        "--bin",
        "/opt/webcodex/bin/webcodex-runner",
        "--working-directory",
        "/srv/webcodex",
        "--user",
        "webcodex",
        "--group",
        "webcodex",
        "--dry-run",
    ]))
    .unwrap();
    let unit = run_runner_install_service(opts).unwrap();
    assert!(unit.contains("[Unit]\nDescription=WebCodex Runner\n"));
    assert!(unit.contains("After=network-online.target\n"));
    assert!(unit.contains("Wants=network-online.target\n"));
    assert!(unit.contains(&format!(
        "ExecStart=\"/opt/webcodex/bin/webcodex-runner\" \"--config\" \"{}\"\n",
        config.display()
    )));
    assert!(unit.contains("ExecReload=/bin/kill -HUP $MAINPID\n"));
    assert!(unit.contains("Restart=always\n"));
    assert!(unit.contains("RestartSec=5s\n"));
    assert!(unit.contains("StandardOutput=journal\n"));
    assert!(unit.contains("StandardError=journal\n"));
    assert!(unit.contains("Environment=RUST_LOG=info\n"));
    assert!(unit.contains("WorkingDirectory=/srv/webcodex\n"));
    assert!(unit.contains("User=webcodex\n"));
    assert!(unit.contains("Group=webcodex\n"));
    assert!(!unit.contains("agent_secret_should_not_print"));
    assert!(!unit.contains("Authorization"));
    assert!(!unit.contains("token ="));
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn runner_install_service_refuses_overwrite_unless_requested() {
    let tmp = tempfile::tempdir().unwrap();
    let service_file = tmp.path().join("webcodex-runner.service");
    std::fs::write(&service_file, "old").unwrap();
    let opts = parse_runner_install_service(&args(&[
        "--scope",
        "system",
        "--user",
        "webcodex",
        "--working-directory",
        "/srv/webcodex",
        "--config",
        "/etc/webcodex/runner.toml",
        "--bin",
        "/opt/webcodex/bin/webcodex-runner",
        "--service-file",
        service_file.to_str().unwrap(),
    ]))
    .unwrap();
    let err = run_runner_install_service(opts).unwrap_err();
    assert!(err.contains("already exists"));
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn runner_install_service_dry_run_and_output_work_without_systemd() {
    let dry = parse_runner_install_service(&args(&[
        "--scope",
        "system",
        "--user",
        "webcodex",
        "--working-directory",
        "/srv/webcodex",
        "--config",
        "/etc/webcodex/runner.toml",
        "--bin",
        "/opt/webcodex/bin/webcodex-runner",
        "--dry-run",
    ]))
    .unwrap();
    assert!(run_runner_install_service(dry).unwrap().contains(
        "ExecStart=\"/opt/webcodex/bin/webcodex-runner\" \"--config\" \"/etc/webcodex/runner.toml\""
    ));

    let out = parse_runner_install_service(&args(&[
        "--scope",
        "system",
        "--user",
        "webcodex",
        "--working-directory",
        "/srv/webcodex",
        "--config",
        "/etc/webcodex/runner.toml",
        "--bin",
        "/opt/webcodex/bin/webcodex-runner",
        "--output",
        "-",
        "--json",
    ]))
    .unwrap();
    let json: Value = serde_json::from_str(&run_runner_install_service(out).unwrap()).unwrap();
    assert_eq!(json["dry_run"], true);
    assert!(json["unit"].as_str().unwrap().contains(
        "ExecStart=\"/opt/webcodex/bin/webcodex-runner\" \"--config\" \"/etc/webcodex/runner.toml\""
    ));
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn systemd_unit_rendering_quotes_paths_and_rejects_invalid_fields_in_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("env files").join("web\"codex\\main.env");
    std::fs::create_dir_all(env_file.parent().unwrap()).unwrap();
    std::fs::write(&env_file, "WEBCODEX_ADDR=127.0.0.1:8080\n").unwrap();
    let server = parse_server_install_service(&args(&[
        "--env-file",
        env_file.to_str().unwrap(),
        "--bin",
        "/opt/web codex/bin/webcodex-server%p",
        "--working-directory",
        "/var/lib/web codex\\work",
        "--user",
        "web_codex-1.service",
        "--group",
        "web.group-1",
        "--dry-run",
    ]))
    .unwrap();
    let unit = run_server_install_service(server).unwrap();
    assert!(unit.contains("env\\x20files/web\\x22codex\\x5cmain.env"));
    assert!(unit.contains("ExecStart=\"/opt/web codex/bin/webcodex-server%%p\""));
    assert!(unit.contains("WorkingDirectory=/var/lib/web\\x20codex\\x5cwork"));
    assert!(unit.contains("User=web_codex-1.service"));
    assert!(unit.contains("Group=web.group-1"));

    for (flag, value, field) in [
        ("--user", "bad user", "User"),
        ("--group", "bad/group", "Group"),
        (
            "--bin",
            "/opt/webcodex/bin/server\nInjected=yes",
            "ExecStart",
        ),
        ("--env-file", "/etc/webcodex/a\rb", "EnvironmentFile"),
        (
            "--working-directory",
            "/var/lib/web\0codex",
            "WorkingDirectory",
        ),
    ] {
        let opts = parse_server_install_service(&args(&[
            "--bin",
            "/usr/local/bin/webcodex-server",
            flag,
            value,
            "--dry-run",
        ]))
        .unwrap();
        let error = run_server_install_service(opts).unwrap_err();
        assert!(error.contains(field), "{error}");
    }
}

#[cfg(target_os = "linux")]
fn verify_systemd_units(units: &[(&str, &str)]) {
    // Other CLI tests temporarily replace PATH under the shared env-test lock.
    // Keep both systemd-analyze probes in one stable environment so the
    // availability check cannot race a later `verify` spawn.
    let _guard = env_test_guard();
    let available = std::process::Command::new("systemd-analyze")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !available {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut paths = Vec::with_capacity(units.len());
    for (name, unit) in units {
        let path = tmp.path().join(name);
        std::fs::write(&path, unit).unwrap();
        paths.push(path);
    }
    let mut command = std::process::Command::new("systemd-analyze");
    command.arg("verify");
    command.args(&paths);
    let output = command.output().unwrap();
    let names = units
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        output.status.success(),
        "systemd-analyze verify failed for {names}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn generated_server_and_runner_units_pass_systemd_analyze_verify() {
    let tmp = tempfile::tempdir().unwrap();
    let server_bin = tmp.path().join("webcodex-server");
    let runner_bin = tmp.path().join("webcodex-runner");
    make_executable(&server_bin);
    make_executable(&runner_bin);
    let env_file = server_env(&tmp);

    let server = parse_server_install_service(&args(&[
        "--bin",
        server_bin.to_str().unwrap(),
        "--env-file",
        env_file.to_str().unwrap(),
        "--working-directory",
        "/var/lib/webcodex",
        "--output",
        "-",
        "--json",
    ]))
    .unwrap();
    let server_json: Value =
        serde_json::from_str(&run_server_install_service(server).unwrap()).unwrap();
    let service_unit = server_json["units"]["service"].as_str().unwrap();
    let socket_unit = server_json["units"]["socket"].as_str().unwrap();
    let service_name = server_json["service_unit"].as_str().unwrap();
    let socket_name = server_json["socket_unit"].as_str().unwrap();
    assert!(service_unit.contains(&format!("EnvironmentFile={}\n", env_file.display())));
    assert!(service_unit.contains("WorkingDirectory=/var/lib/webcodex\n"));
    assert!(service_unit.contains("webcodex-server"));
    assert!(socket_unit.contains("ListenStream=127.0.0.1:8080\n"));
    verify_systemd_units(&[(service_name, service_unit), (socket_name, socket_unit)]);

    let config = tmp.path().join("runner.toml");
    std::fs::write(&config, "server_url = \"http://127.0.0.1\"\n").unwrap();
    let agent = parse_runner_install_service(&args(&[
        "--scope",
        "system",
        "--user",
        "webcodex",
        "--bin",
        runner_bin.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--working-directory",
        "/var/lib/webcodex",
        "--dry-run",
    ]))
    .unwrap();
    let agent_unit = run_runner_install_service(agent).unwrap();
    assert!(agent_unit.contains("webcodex-runner\" \"--config\""));
    assert!(agent_unit.contains("WorkingDirectory=/var/lib/webcodex\n"));
    verify_systemd_units(&[("webcodex-runner-default.service", &agent_unit)]);
}

#[cfg(target_os = "linux")]
#[test]
fn special_supported_paths_pass_systemd_analyze_verify() {
    let tmp = tempfile::tempdir().unwrap();
    let server_bin = tmp.path().join("webcodex server%p");
    let runner_bin = tmp.path().join("webcodex runner%p");
    make_executable(&server_bin);
    make_executable(&runner_bin);

    let working = tmp.path().join("work space\"slash\\percent%p");
    std::fs::create_dir(&working).unwrap();
    let env_file = tmp.path().join("env space\"slash\\percent%p.env");
    std::fs::write(&env_file, "WEBCODEX_ADDR=127.0.0.1:8080\n").unwrap();
    let config = tmp.path().join("config space\"slash\\percent%p.toml");
    std::fs::write(&config, "server_url = \"http://127.0.0.1\"\n").unwrap();

    let server = parse_server_install_service(&args(&[
        "--bin",
        server_bin.to_str().unwrap(),
        "--env-file",
        env_file.to_str().unwrap(),
        "--working-directory",
        working.to_str().unwrap(),
        "--output",
        "-",
        "--json",
    ]))
    .unwrap();
    let server_json: Value =
        serde_json::from_str(&run_server_install_service(server).unwrap()).unwrap();
    let server_unit = server_json["units"]["service"].as_str().unwrap();
    let socket_unit = server_json["units"]["socket"].as_str().unwrap();
    let service_name = server_json["service_unit"].as_str().unwrap();
    let socket_name = server_json["socket_unit"].as_str().unwrap();
    assert!(server_unit.contains("\\x20"));
    assert!(server_unit.contains("\\x22"));
    assert!(server_unit.contains("\\x5c"));
    assert!(server_unit.contains("%%p"));
    verify_systemd_units(&[(service_name, server_unit), (socket_name, socket_unit)]);

    let agent = parse_runner_install_service(&args(&[
        "--scope",
        "system",
        "--user",
        "webcodex",
        "--bin",
        runner_bin.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--working-directory",
        working.to_str().unwrap(),
        "--dry-run",
    ]))
    .unwrap();
    let agent_unit = run_runner_install_service(agent).unwrap();
    assert!(agent_unit.contains("\\\"slash\\\\percent%%p.toml"));
    verify_systemd_units(&[("webcodex-runner-special.service", &agent_unit)]);
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn executable_program_rejects_quote_and_backslash_in_dry_run() {
    for path in [
        "/opt/webcodex/web\"codex-server",
        "/opt/webcodex/web\\codex-server",
    ] {
        let opts = parse_server_install_service(&args(&["--bin", path, "--dry-run"])).unwrap();
        let error = run_server_install_service(opts).unwrap_err();
        assert!(error.contains("executable path"), "{error}");
    }
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn runner_output_mode_rejects_invalid_unit_fields() {
    let opts = parse_runner_install_service(&args(&[
        "--scope",
        "system",
        "--user",
        "webcodex",
        "--working-directory",
        "/srv/webcodex",
        "--config",
        "/etc/webcodex/runner.toml\nEnvironment=BAD=1",
        "--bin",
        "/opt/webcodex/bin/webcodex-runner",
        "--output",
        "-",
    ]))
    .unwrap();
    let error = run_runner_install_service(opts).unwrap_err();
    assert!(error.contains("ExecStart --config"));
    assert!(!error.contains("Environment=BAD=1"));
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn runner_status_parses_runner_toml_without_printing_token_and_systemd_unknown() {
    let _guard = env_test_guard();
    let _env = EnvGuard::new().set_os("PATH", OsString::new());
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("runner.toml");
    let secret = "agent_status_secret_1234567890";
    std::fs::write(
        &config,
        format!(
            r#"
server_url = "https://example.test"
token = "{secret}"
client_id = "alice-laptop"
owner = "alice"
display_name = "Alice Laptop"
transport = "websocket"
project_registry_dir = "/etc/webcodex/project-registry"

[policy]
allowed_roots = ["/srv/projects"]
"#
        ),
    )
    .unwrap();
    let opts = parse_runner_status(&args(&[
        "--scope",
        "system",
        "--config",
        config.to_str().unwrap(),
        "--json",
    ]))
    .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = rt.block_on(run_runner_status(opts)).unwrap();
    assert!(!output.contains(secret));
    let json: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["service"]["unit"], "webcodex-runner.service");
    assert!(json["service"].get("legacy_unit").is_none());
    assert_eq!(json["service"]["active"], "unknown");
    assert_eq!(json["service"]["enabled"], "unknown");
    assert_eq!(json["config"]["client_id"], "alice-laptop");
    assert_eq!(json["config"]["owner"], "alice");
    assert_eq!(json["config"]["allowed_roots"]["count"], 1);
    assert!(json.get("token").is_none());
    assert!(json["config"].get("token").is_none());
}

/// Unix-only: systemd service unit semantics with Unix absolute-path
/// rules. On Windows the systemd service feature fails closed.
#[cfg(unix)]
#[test]
fn runner_status_rejects_runner_transport_token_in_user_runtime_token_file_without_leaking_it() {
    let _guard = env_test_guard();
    let _env = EnvGuard::new().set_os("PATH", OsString::new());
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("runner.toml");
    std::fs::write(
        &config,
        "server_url = \"https://example.test\"\nclient_id = \"alice\"\n",
    )
    .unwrap();
    let token_file = tmp.path().join("webcodex-user-token");
    let secret = "wc_agent_do_not_echo_status_0123456789";
    std::fs::write(&token_file, format!("{secret}\n")).unwrap();
    let opts = parse_runner_status(&args(&[
        "--scope",
        "system",
        "--config",
        config.to_str().unwrap(),
        "--user-token-file",
        token_file.to_str().unwrap(),
    ]))
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime.block_on(run_runner_status(opts)).unwrap_err();
    assert!(error.contains("Runner transport token"), "{error}");
    assert!(error.contains("webcodex-user-token"), "{error}");
    assert!(!error.contains(secret));
}

#[cfg(unix)]
#[test]
fn hosted_profile_runner_bin_never_falls_back_to_systemd_without_marker() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = env_test_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config_home = tmp.path().join("config");
    let state_home = tmp.path().join("state");
    let fake_bin = tmp.path().join("bin");
    let systemctl_called = tmp.path().join("systemctl-called");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let fake_systemctl = fake_bin.join("systemctl");
    std::fs::write(
        &fake_systemctl,
        format!("#!/bin/sh\n: > {:?}\n", systemctl_called),
    )
    .unwrap();
    std::fs::set_permissions(&fake_systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();

    let _env = EnvGuard::new()
        .set_os("HOME", home.into_os_string())
        .set_os("XDG_CONFIG_HOME", config_home.into_os_string())
        .set_os("XDG_STATE_HOME", state_home.into_os_string())
        .set_os("PATH", fake_bin.into_os_string());
    let opts = parse_runner_service_action(
        "restart",
        &args(&["--profile", "hosted", "--bin", "/tmp/webcodex-dev-runner"]),
    )
    .unwrap();
    let error = run_runner_service(opts).unwrap_err();
    assert!(error.contains("existing hosted connect profile"), "{error}");
    assert!(!systemctl_called.exists());
}

#[cfg(unix)]
#[test]
fn hosted_profile_status_uses_xdg_config_and_never_invokes_systemctl() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = env_test_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config_home = tmp.path().join("config");
    let state_home = tmp.path().join("state");
    let profile_config = config_home.join("webcodex/clients/hosted/runner.toml");
    let profile_state = state_home.join("webcodex/clients/hosted");
    let fake_bin = tmp.path().join("bin");
    let systemctl_called = tmp.path().join("systemctl-called");
    std::fs::create_dir_all(profile_config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&profile_state).unwrap();
    std::fs::create_dir_all(&fake_bin).unwrap();
    std::fs::write(
        &profile_config,
        format!(
            "server_url = \"\"\ntoken = \"hosted-shared-key\"\nclient_id = \"hosted\"\nproject_registry_dir = {:?}\n",
            profile_config.parent().unwrap().join("project-registry")
        ),
    )
    .unwrap();
    std::fs::write(
        crate::webcodex_cli::local_runner_profile_marker(&profile_state),
        "profile = \"hosted\"\n",
    )
    .unwrap();
    let fake_systemctl = fake_bin.join("systemctl");
    std::fs::write(
        &fake_systemctl,
        format!("#!/bin/sh\n: > {:?}\n", systemctl_called),
    )
    .unwrap();
    std::fs::set_permissions(&fake_systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();

    let _env = EnvGuard::new()
        .set_os("HOME", home.into_os_string())
        .set_os("XDG_CONFIG_HOME", config_home.into_os_string())
        .set_os("XDG_STATE_HOME", state_home.into_os_string())
        .set_os("PATH", fake_bin.into_os_string());
    let opts = parse_runner_status_with_identity(&args(&["--profile", "hosted"]), true).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime.block_on(run_runner_status(opts));

    let output = output.unwrap();
    assert!(output.contains("runner mode:          hosted local process"));
    let json_opts =
        parse_runner_status_with_identity(&args(&["--profile", "hosted", "--json"]), true).unwrap();
    let json_output = runtime.block_on(run_runner_status(json_opts)).unwrap();
    let json_output: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    assert_eq!(
        json_output["config"]["project_registry_dir"],
        profile_config
            .parent()
            .unwrap()
            .join("project-registry")
            .to_string_lossy()
            .as_ref()
    );
    assert!(json_output["config"].get("projects_dir").is_none());
    assert!(!systemctl_called.exists());
}

/// Unix-only: systemd service status semantics. On Windows the systemd
/// service feature fails closed.
#[cfg(unix)]
#[tokio::test]
async fn runner_status_detects_current_client_online_and_runner_token_boundary() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        for i in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 16384];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            tx.send(request.clone()).unwrap();
            if i == 0 {
                let body = r#"{"success":true,"output":{"agents":{"clients":[{"client_id":"alice-laptop","connected":true,"status":"online"}]}}}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            } else {
                let body = r#"{"error":"forbidden"}"#;
                write!(
                    stream,
                    "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        }
    });
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("runner.toml");
    std::fs::write(
        &config,
        r#"
server_url = "http://127.0.0.1:1"
token = "agent_config_secret_abcdef"
client_id = "alice-laptop"
owner = "alice"
transport = "websocket"
"#,
    )
    .unwrap();
    let user_token_file = tmp.path().join("webcodex-user-token");
    let runner_token_file = tmp.path().join("webcodex-runner-token");
    std::fs::write(&user_token_file, "pat_online_secret_1234567890\n").unwrap();
    std::fs::write(&runner_token_file, "runner_boundary_secret_1234567890\n").unwrap();
    let opts = parse_runner_status(&args(&[
        "--scope",
        "system",
        "--config",
        config.to_str().unwrap(),
        "--server-url",
        &format!("http://{}", addr),
        "--no-system-proxy",
        "--user-token-file",
        user_token_file.to_str().unwrap(),
        "--runner-token-file",
        runner_token_file.to_str().unwrap(),
    ]))
    .unwrap();
    let output = run_runner_status(opts).await.unwrap();
    handle.join().unwrap();
    let first_request = rx.recv().unwrap();
    let second_request = rx.recv().unwrap();
    assert!(first_request
        .to_ascii_lowercase()
        .contains("authorization: bearer pat_online_secret_1234567890"));
    assert!(second_request
        .to_ascii_lowercase()
        .contains("authorization: bearer runner_boundary_secret_1234567890"));
    for secret in [
        "agent_config_secret_abcdef",
        "pat_online_secret_1234567890",
        "runner_boundary_secret_1234567890",
    ] {
        assert!(!output.contains(secret));
    }
    assert!(output.contains("client online:        yes"));
    assert!(output.contains("Runner token boundary: PASS"));
}
