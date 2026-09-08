use super::*;

#[test]
fn cwd_allowed_skips_missing_unrelated_root_when_later_root_matches() {
    let allowed = tempfile::tempdir().unwrap();
    let missing = allowed.path().join("deleted-project");
    let policy = RunnerPolicy {
        allow_cwd_anywhere: false,
        allowed_roots: vec![missing, allowed.path().to_path_buf()],
        ..RunnerPolicy::default()
    };

    cwd_allowed(&policy, allowed.path())
        .expect("a missing unrelated root must not block a later matching root");
}

#[test]
fn cwd_allowed_remains_fail_closed_when_no_existing_root_matches() {
    let allowed = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let missing = allowed.path().join("deleted-project");
    let policy = RunnerPolicy {
        allow_cwd_anywhere: false,
        allowed_roots: vec![missing, allowed.path().to_path_buf()],
        ..RunnerPolicy::default()
    };

    let error = cwd_allowed(&policy, outside.path()).unwrap_err();
    assert!(error.contains("outside allowed_roots"), "{error}");
}

#[cfg(windows)]
#[test]
fn shell_job_filters_sensitive_env_case_insensitive() {
    let _guard = test_env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    // The plain (non-profile) path removes sensitive keys from the child
    // environment; Windows removal must be case-insensitive like the OS.
    for spelling in [
        "WEBCODEX_TOKEN",
        "WebCodex_User_Token",
        "Authorization",
        "webcodex_agent_token",
    ] {
        let _env = EnvGuard::new().set(spelling, "secret-token");
        let result = run_shell(
            &cfg.policy,
            &ShellConfig::default(),
            Some(&cwd),
            &shell_if_else_env_present(spelling),
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("absent"), "{result:?}");
    }

    // A configured shell env must not be able to re-insert a secret after the
    // inherited environment was scrubbed. Exercise canonical and mixed-case
    // spellings because Windows environment names are case-insensitive.
    for spelling in ["WEBCODEX_TOKEN", "WebCodex_User_Token", "authorization"] {
        let shell = ShellConfig {
            env: HashMap::from([(spelling.to_string(), "configured-secret".to_string())]),
            ..ShellConfig::default()
        };
        let result = run_shell(
            &cfg.policy,
            &shell,
            Some(&cwd),
            &shell_if_else_env_present(spelling),
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(
            result.stdout.as_deref(),
            Some("absent"),
            "configured sensitive env leaked: {result:?}"
        );
    }
}

fn decoded_job_operation(request: RunnerRequest) -> RunnerJobOperation {
    match request.decode_operation().expect("valid typed Job fixture") {
        RunnerOperation::Job(operation) => operation,
        _ => panic!("expected Job operation"),
    }
}

#[test]
fn raw_shell_job_lifecycle_distinguishes_terminal_truth_without_error_text_matching() {
    assert_eq!(
        raw_shell_job_terminal_lifecycle("completed", Some(0)),
        ShellCommandExecutionState::Completed
    );
    assert_eq!(
        raw_shell_job_terminal_lifecycle("failed", Some(7)),
        ShellCommandExecutionState::Completed
    );
    assert_eq!(
        raw_shell_job_terminal_lifecycle("stopped", Some(-1)),
        ShellCommandExecutionState::Completed
    );
    assert_eq!(
        raw_shell_job_terminal_lifecycle("timeout", Some(-1)),
        ShellCommandExecutionState::TimedOut
    );
    assert_eq!(
        raw_shell_job_terminal_lifecycle("failed", None),
        ShellCommandExecutionState::OutcomeUnknown
    );
}

#[test]
fn raw_shell_job_prestart_rejection_is_explicitly_not_started() {
    let temp = tempfile::tempdir().unwrap();
    let operation = decoded_job_operation(shell_job_request(temp.path(), "printf ok"));
    assert_eq!(
        job_prestart_lifecycle(&operation),
        Some(ShellCommandExecutionState::NotStarted)
    );
}

#[test]
fn raw_shell_job_post_spawn_interruption_never_reuses_not_started_proof() {
    let temp = tempfile::tempdir().unwrap();
    let shell = decoded_job_operation(shell_job_request(temp.path(), "printf ok"));
    assert_eq!(
        post_spawn_interruption_lifecycle(&shell),
        Some(ShellCommandExecutionState::OutcomeUnknown)
    );

    let step = ShellJobValidationStep {
        name: "check".to_string(),
        program: "cargo".to_string(),
        args: vec!["check".to_string(), "--all-targets".to_string()],
        env: Vec::new(),
    };
    let mut validation = shell_job_request(temp.path(), "");
    validation.kind = "start_validation_job".to_string();
    validation.command = serde_json::to_string(&[step]).unwrap();
    validation.job_context.as_mut().unwrap().validation_steps = vec!["check".to_string()];
    let validation = decoded_job_operation(validation);
    assert_eq!(post_spawn_interruption_lifecycle(&validation), None);
}

#[test]
fn shell_job_success_and_failure_results_are_structured() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();

    let success = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        &format!("{}; {}", shell_echo("hello"), shell_echo_err("warn")),
        None,
        10,
        None,
    );
    assert_eq!(success.exit_code, Some(0));
    assert_eq!(success.stdout.as_deref(), Some("hello"));
    assert_eq!(success.stderr.as_deref(), Some("warn"));
    assert!(success.error.is_none());

    let failure = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        "exit 7",
        None,
        10,
        None,
    );
    assert_eq!(failure.exit_code, Some(7));
    assert!(failure.error.is_none());
}

#[test]
fn shell_job_writes_stdin_to_child() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();

    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        &shell_stdin_cat(),
        Some("stdin payload\n"),
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout.as_deref(), Some("stdin payload\n"));
    assert!(result.error.is_none());
}

#[cfg(unix)]
#[test]
fn shell_job_preserves_result_when_child_closes_stdin_early() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    // Larger than a pipe buffer, so write_all observes the closed reader
    // instead of winning the race by buffering the whole payload.
    let input = "unused payload\n".repeat(128 * 1024);

    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        "exec 0<&-; printf capability-unavailable; exit 23",
        Some(&input),
        10,
        None,
    );

    assert_eq!(result.exit_code, Some(23), "{result:?}");
    assert_eq!(result.stdout.as_deref(), Some("capability-unavailable"));
    assert!(result.error.is_none(), "{result:?}");
}

#[cfg(unix)]
#[test]
fn shell_job_rejects_cwd_symlink_escape() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), project.path().join("outside")).unwrap();
    let policy = RunnerPolicy {
        allow_cwd_anywhere: false,
        allowed_roots: vec![project.path().to_path_buf()],
        ..RunnerPolicy::default()
    };

    let result = run_shell(
        &policy,
        &ShellConfig::default(),
        Some(project.path().join("outside").to_string_lossy().as_ref()),
        "pwd",
        None,
        10,
        None,
    );

    assert_eq!(result.exit_code, None);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("outside allowed_roots")));
}

#[test]
#[ignore = "runner real-process lane: waits on a real shell timeout"]
fn runner_real_process_shell_job_timeout_returns_timeout_error() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();

    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        "sleep 2",
        None,
        1,
        None,
    );
    assert_eq!(result.exit_code, Some(-1));
    assert_eq!(result.error.as_deref(), Some("command timed out"));
    assert!(result
        .stderr
        .as_deref()
        .unwrap_or_default()
        .contains("command timed out after 1 seconds"));
}

#[cfg(unix)]
fn long_lived_descendant_command(pid_file: &Path) -> String {
    format!(
        "sleep 60 & descendant=$!; printf '%s' \"$descendant\" > {}; wait",
        shell_quote_path(pid_file)
    )
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: waits on a real shell timeout"]
fn runner_real_process_shell_job_timeout_reaps_descendant_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    let pid_file = tmp.path().join("timeout-descendant.pid");

    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        &long_lived_descendant_command(&pid_file),
        None,
        1,
        None,
    );

    assert_eq!(result.exit_code, Some(-1), "{result:?}");
    assert_eq!(
        result.error.as_deref(),
        Some("command timed out"),
        "{result:?}"
    );
    assert!(
        result
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("command timed out after 1 seconds"),
        "{result:?}"
    );
    assert_descendant_reaped(&pid_file);
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: waits on a real shell timeout"]
fn runner_real_process_shell_job_timeout_profile_reaps_descendant_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let shell = shell_with_profiles(Some("test"), vec![("test", ShellProfileConfig::default())]);
    let policy = unrestricted_test_policy();
    let cache = PreparedShellProfileCache::default();
    let cwd = tmp.path().to_string_lossy().to_string();
    let pid_file = tmp.path().join("profile-timeout-descendant.pid");

    // Exercise the production request path directly rather than the
    // test-only `run_shell` wrapper.
    let result = run_shell_with_profiles(
        1,
        &policy,
        &shell,
        tmp.path(),
        &cache,
        Some(&cwd),
        &long_lived_descendant_command(&pid_file),
        None,
        1,
        None,
    );

    assert_eq!(cache.len(), 1, "prepared profile path was not used");
    assert_eq!(result.exit_code, Some(-1), "{result:?}");
    assert_eq!(
        result.error.as_deref(),
        Some("command timed out"),
        "{result:?}"
    );
    assert_descendant_reaped(&pid_file);
}

#[cfg(windows)]
#[test]
fn shell_job_powershell_statement_error_is_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        "Write-Error 'expected failure'",
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(1), "{result:?}");
    assert!(result.error.is_none(), "{result:?}");
}

#[cfg(windows)]
#[test]
fn shell_job_powershell_last_success_overrides_stale_native_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        "cmd.exe /d /c exit 7; Write-Output 'final-ok'",
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(
        result.stdout.as_deref().map(str::trim_end),
        Some("final-ok"),
        "{result:?}"
    );
}

#[test]
fn shell_job_stop_flag_is_best_effort() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    let stop_requested = AtomicBool::new(true);

    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        "sleep 2",
        None,
        10,
        Some(&stop_requested),
    );
    assert_eq!(result.exit_code, Some(-1));
    assert_eq!(result.error.as_deref(), Some("job stopped"));
    assert!(result
        .stderr
        .as_deref()
        .unwrap_or_default()
        .contains("job stopped by request"));
}

#[cfg(unix)]
#[test]
fn shell_job_stop_reaps_descendant_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let cwd = tmp.path().to_string_lossy().to_string();
    let pid_file = tmp.path().join("stop-descendant.pid");
    let stop_requested = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop_requested);
    let stop_pid_file = pid_file.clone();
    let stopper = std::thread::spawn(move || {
        let created = wait_until(Duration::from_secs(2), || stop_pid_file.exists());
        stop_flag.store(true, Ordering::SeqCst);
        created
    });

    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        &long_lived_descendant_command(&pid_file),
        None,
        10,
        Some(stop_requested.as_ref()),
    );

    assert!(stopper.join().expect("stopper thread panicked"));
    assert_eq!(result.exit_code, Some(-1), "{result:?}");
    assert_eq!(result.error.as_deref(), Some("job stopped"), "{result:?}");
    assert!(
        result
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("job stopped by request"),
        "{result:?}"
    );
    assert_descendant_reaped(&pid_file);
}

#[test]
fn shell_job_stdout_stderr_are_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path().join("config/project-registry"));
    cfg.policy.max_output_bytes = 8;
    let cwd = tmp.path().to_string_lossy().to_string();

    let result = run_shell(
        &cfg.policy,
        &cfg.shell,
        Some(&cwd),
        &format!(
            "{}; {}",
            shell_echo("0123456789"),
            shell_echo_err("abcdefghij")
        ),
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(0));
    let stdout = result.stdout.unwrap();
    let stderr = result.stderr.unwrap();
    assert_eq!(stdout.len(), 8);
    assert!(stdout.starts_with("[...]\n"), "{stdout:?}");
    assert_eq!(stderr.len(), 8);
    assert!(stderr.starts_with("[...]\n"), "{stderr:?}");
}
