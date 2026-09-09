use super::*;
#[cfg(windows)]
use crate::webcodex_runner::config::default_windows_shell_program_for_path;

/// Write a shell init script for this platform's default shell into `dir`
/// that exports `name=value`, and return its path.
#[cfg(windows)]
fn write_init_script(dir: &Path, name: &str, value: &str) -> PathBuf {
    let init = dir.join("init.ps1");
    // PowerShell 5.1 decodes .ps1 files with the system ANSI code page unless
    // a UTF-8 BOM is present; the BOM keeps non-ASCII script bodies intact.
    let mut content = "\u{FEFF}".to_string();
    content.push_str(&format!("$env:{name} = '{}'\n", value.replace('\'', "''")));
    std::fs::write(&init, content).unwrap();
    init
}

#[cfg(not(windows))]
fn write_init_script(dir: &Path, name: &str, value: &str) -> PathBuf {
    let init = dir.join("init.sh");
    std::fs::write(&init, format!("export {name}={value}\n")).unwrap();
    init
}

#[test]
fn shell_config_default_preserves_sh_c_behavior() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &ShellConfig::default(),
        Some(&cwd),
        &shell_echo("default-ok"),
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout.as_deref(), Some("default-ok"));
}

#[test]
fn shell_config_default_shell_is_platform_native() {
    let shell = ShellConfig::default();
    #[cfg(windows)]
    {
        // Prefer PowerShell 7 when it is discoverable, while preserving the
        // native Windows PowerShell fallback and avoiding any sh/Git Bash/WSL
        // dependency for ordinary configured-shell work.
        assert_eq!(
            shell.program,
            default_windows_shell_program_for_path(std::env::var_os("PATH").as_deref())
        );
        assert!(shell.args.iter().any(|arg| arg == "-Command"));
        assert!(
            shell.args.iter().any(|arg| arg == "-NoProfile"),
            "default must not load the user's interactive profile"
        );
        let result = run_shell(
            &unrestricted_test_policy(),
            &shell,
            None,
            &shell_echo("power-default-ok"),
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("power-default-ok"));
    }
    #[cfg(not(windows))]
    {
        assert_eq!(shell.program, "sh");
        assert_eq!(shell.args, vec!["-c".to_string()]);
    }
}

#[cfg(windows)]
#[test]
fn shell_config_windows_default_prefers_pwsh_and_falls_back() {
    let pwsh_dir = tempfile::tempdir().unwrap();
    std::fs::write(pwsh_dir.path().join("pwsh.exe"), b"").unwrap();
    let pwsh_path = std::env::join_paths([pwsh_dir.path()]).unwrap();
    assert_eq!(
        default_windows_shell_program_for_path(Some(pwsh_path.as_os_str())),
        "pwsh.exe"
    );

    let fallback_dir = tempfile::tempdir().unwrap();
    let fallback_path = std::env::join_paths([fallback_dir.path()]).unwrap();
    assert_eq!(
        default_windows_shell_program_for_path(Some(fallback_path.as_os_str())),
        "powershell.exe"
    );
}

#[cfg(unix)]
#[test]
fn shell_config_path_prepend_discovers_fake_executable() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let exe = bin_dir.join("webcodex-fake-tool");
    std::fs::write(&exe, "#!/bin/sh\nprintf fake-tool-ok\n").unwrap();
    let mut perms = std::fs::metadata(&exe).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&exe, perms).unwrap();
    let shell = ShellConfig {
        path_prepend: vec![bin_dir],
        ..ShellConfig::default()
    };
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &shell,
        Some(&cwd),
        "webcodex-fake-tool",
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout.as_deref(), Some("fake-tool-ok"));
}

#[cfg(windows)]
#[test]
fn shell_config_path_prepend_discovers_fake_executable_and_keeps_windows_path() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let bin_dir = tmp.path().join("bin dir");
    std::fs::create_dir(&bin_dir).unwrap();
    // PowerShell executes .cmd files from PATH through cmd.exe; `<nul
    // set /p` writes exactly the payload with no trailing newline, and the
    // explicit `exit /b 0` keeps cmd's errorlevel (set /p with an empty
    // variable name leaves it 1) from leaking into $LASTEXITCODE.
    let exe = bin_dir.join("webcodex-fake-tool.cmd");
    std::fs::write(
        &exe,
        "@echo off\r\n<nul set /p \"=fake-tool-ok\"\r\nexit /b 0\r\n",
    )
    .unwrap();
    let shell = ShellConfig {
        path_prepend: vec![bin_dir],
        ..ShellConfig::default()
    };
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &shell,
        Some(&cwd),
        "webcodex-fake-tool.cmd",
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.stdout.as_deref(), Some("fake-tool-ok"), "{result:?}");

    // path_prepend must extend the inherited Windows PATH (spelled `Path` in
    // the process block), not replace it: the prepended directory comes
    // first and the System32 entries survive.
    let result = run_shell(
        &cfg.policy,
        &shell,
        Some(&cwd),
        &shell_env_var("PATH"),
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    let path = result.stdout.unwrap();
    let dir_text = tmp.path().join("bin dir").to_string_lossy().to_string();
    assert!(
        path.starts_with(&dir_text),
        "prepended directory is not first in PATH: {path}"
    );
    assert!(
        path.contains("System32"),
        "inherited Windows PATH lost: {path}"
    );
}

#[test]
fn shell_config_dialect_field_parses_and_validates() {
    use crate::webcodex_runner::config::ShellDialect;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runner.toml");
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
dialect = "powershell"

[shell.profiles.custom]
dialect = "posix"
program = "sh"
args = ["-c"]
"#,
    )
    .unwrap();
    let cfg = load_config(&path).unwrap();
    assert_eq!(cfg.shell.dialect, Some(ShellDialect::PowerShell));
    assert_eq!(
        cfg.shell.profiles.get("custom").unwrap().dialect,
        Some(ShellDialect::Posix)
    );

    // Unknown dialect values are rejected explicitly at parse time.
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
dialect = "cmd"
"#,
    )
    .unwrap();
    let err = load_config(&path).unwrap_err();
    assert!(err.contains("powershell"), "{err}");
    assert!(err.contains("cmd"), "{err}");
}

#[test]
fn shell_config_default_environment_is_inherited() {
    let _guard = test_env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    let _env = EnvGuard::new().set("WEBCODEX_INHERITED_TEST", "inherited-ok");
    let result = run_shell(
        &cfg.policy,
        &ShellConfig::default(),
        Some(&cwd),
        &shell_env_var("WEBCODEX_INHERITED_TEST"),
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.stdout.as_deref(), Some("inherited-ok"));
}

#[test]
fn shell_config_env_values_are_available() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let shell = ShellConfig {
        env: HashMap::from([("WEBCODEX_TEST_VALUE".to_string(), "env-ok".to_string())]),
        ..ShellConfig::default()
    };
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &shell,
        Some(&cwd),
        &shell_env_var("WEBCODEX_TEST_VALUE"),
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout.as_deref(), Some("env-ok"));
}

#[test]
fn shell_config_init_script_is_sourced() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let init = write_init_script(tmp.path(), "WEBCODEX_INIT_TEST", "init-ok");
    let shell = ShellConfig {
        init_script: Some(init),
        ..ShellConfig::default()
    };
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &shell,
        Some(&cwd),
        &shell_env_var("WEBCODEX_INIT_TEST"),
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout.as_deref(), Some("init-ok"));
}

#[test]
fn shell_config_init_script_awkward_path_is_sourced() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    // Space, single quote, and non-ASCII characters in the init script path.
    let init_dir = tmp.path().join("init dir '脚本");
    std::fs::create_dir_all(&init_dir).unwrap();
    let init = write_init_script(&init_dir, "WEBCODEX_INIT_TEST", "init-ok");
    let shell = ShellConfig {
        init_script: Some(init),
        ..ShellConfig::default()
    };
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &shell,
        Some(&cwd),
        &shell_env_var("WEBCODEX_INIT_TEST"),
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.stdout.as_deref(), Some("init-ok"));
}

#[test]
fn shell_job_init_script_failure_blocks_command_and_reports_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    // Windows: `exit 3` inside the dot-sourced script terminates the process
    // with 3. Unix: `false` fails the sourced script so `&&` short-circuits
    // with exit 1. Either way the requested command must never run.
    #[cfg(windows)]
    let (init_content, expected_exit) = ("exit 3\n", Some(3));
    #[cfg(not(windows))]
    let (init_content, expected_exit) = ("false\n", Some(1));
    let init = if cfg!(windows) {
        tmp.path().join("fail.ps1")
    } else {
        tmp.path().join("fail.sh")
    };
    std::fs::write(&init, init_content).unwrap();
    let shell = ShellConfig {
        init_script: Some(init),
        ..ShellConfig::default()
    };
    let cwd = tmp.path().to_string_lossy().to_string();
    let marker = tmp.path().join("command-ran.txt");
    let result = run_shell(
        &cfg.policy,
        &shell,
        Some(&cwd),
        &shell_write_file(&marker),
        None,
        10,
        None,
    );
    assert!(
        !marker.exists(),
        "command ran despite init script failure: {result:?}"
    );
    assert_eq!(result.exit_code, expected_exit, "{result:?}");
    assert!(result.error.is_none(), "{result:?}");
}

#[test]
fn shell_config_bash_like_args_are_respected_when_available() {
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let shell = ShellConfig {
        program: "/bin/bash".to_string(),
        args: vec!["-lc".to_string()],
        ..ShellConfig::default()
    };
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &shell,
        Some(&cwd),
        "[[ 1 -eq 1 ]] && printf bash-ok",
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout.as_deref(), Some("bash-ok"));
}
