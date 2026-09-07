use super::*;

#[test]
#[ignore = "runner real-process lane: spawns a real shell/process tree"]
fn runner_real_process_shell_job_normal_success_preserves_output_and_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().to_string();
    let helper = shell_tree_helper();
    let result = run_shell(
        &unrestricted_test_policy(),
        &shell_tree_test_shell(),
        Some(&cwd),
        &shell_tree_command(
            &helper,
            &["sleep".to_string(), "0".to_string(), "7".to_string()],
        ),
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, Some(7), "{result:?}");
    assert!(result.error.is_none(), "{result:?}");
    assert!(
        result
            .stdout
            .as_deref()
            .unwrap_or_default()
            .contains("VALIDATION_HELPER_STDOUT"),
        "{result:?}"
    );
    assert!(
        result
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("VALIDATION_HELPER_STDERR"),
        "{result:?}"
    );
    assert!(
        result.duration_ms.unwrap_or(u64::MAX) < 30_000,
        "unbounded shell run: {result:?}"
    );
}

#[test]
#[ignore = "runner real-process lane: spawns a real shell/process tree"]
fn runner_real_process_shell_job_timeout_kills_whole_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().to_string();
    let helper = shell_tree_helper();
    let markers = ShellTreeMarkers::in_dir(tmp.path(), "timeout");
    let timeout_secs = shell_tree_test_timeout_secs();

    let result = run_shell(
        &unrestricted_test_policy(),
        &shell_tree_test_shell(),
        Some(&cwd),
        &markers.keepalive_command(&helper),
        None,
        timeout_secs,
        None,
    );
    assert!(
        wait_until_file(&markers.parent, Duration::from_secs(15)),
        "tree markers were never written: {result:?}"
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
            .contains(&format!("command timed out after {timeout_secs} seconds")),
        "{result:?}"
    );
    markers.assert_tree_dead("timeout");
}

#[test]
#[ignore = "runner real-process lane: spawns a real shell/process tree"]
fn runner_real_process_shell_job_stop_kills_whole_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().to_string();
    let helper = shell_tree_helper();
    let markers = ShellTreeMarkers::in_dir(tmp.path(), "stop");
    let stop_requested = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop_requested);
    let stop_marker = markers.parent.clone();
    let stopper = std::thread::spawn(move || {
        let written = wait_until_file(&stop_marker, Duration::from_secs(15));
        stop_flag.store(true, Ordering::SeqCst);
        written
    });

    let result = run_shell(
        &unrestricted_test_policy(),
        &shell_tree_test_shell(),
        Some(&cwd),
        &markers.keepalive_command(&helper),
        None,
        60,
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
    markers.assert_tree_dead("stop");
}

#[test]
#[ignore = "runner real-process lane: spawns a real shell/process tree"]
fn runner_real_process_shell_job_parent_exit_first_descendant_holds_pipe() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().to_string();
    let helper = shell_tree_helper();
    let markers = ShellTreeMarkers::in_dir(tmp.path(), "orphan");

    // `spawn-descendant`: the helper spawns a sleeping descendant that
    // inherits the capture pipes, waits until it is provably alive, then
    // exits 0. The direct shell child therefore exits while the descendant
    // is still running and still holding the stdout/stderr write ends.
    let result = run_shell(
        &unrestricted_test_policy(),
        &shell_tree_test_shell(),
        Some(&cwd),
        &shell_tree_command(
            &helper,
            &[
                "spawn-descendant".to_string(),
                markers.parent.to_string_lossy().into_owned(),
                markers.alive.to_string_lossy().into_owned(),
                "120".to_string(),
            ],
        ),
        None,
        30,
        None,
    );

    // Direct-child exit code is preserved even though the descendant was
    // still alive at that point.
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(result.error.is_none(), "{result:?}");
    assert!(
        result
            .stdout
            .as_deref()
            .unwrap_or_default()
            .contains("DESCENDANT_PID="),
        "stdout did not reach EOF with the helper output: {result:?}"
    );
    // The descendant was sleeping (total 120s) when the direct child exited;
    // the whole-tree cleanup must terminate it instead of waiting for the
    // sleep to finish.
    let descendant = read_marker_pid(&markers.parent, "DESCENDANT_PID");
    assert!(
        wait_until_process_dead(descendant, Duration::from_secs(10), "orphan-descendant"),
        "descendant {descendant} survived whole-tree cleanup after direct child exit"
    );
    assert!(
        result.duration_ms.unwrap_or(u64::MAX) < 30_000,
        "runner waited for the descendant's natural sleep: {result:?}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: spawns a real shell/process tree"]
fn runner_real_process_shell_job_unix_graceful_sigterm_responsive_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().to_string();
    let helper = shell_tree_helper();

    // `sigterm-marker`: the helper installs a SIGTERM handler that writes
    // SIGTERM_HANDLED to the captured stdout. SIGKILL cannot be caught, so
    // the marker only appears when the graceful phase delivered SIGTERM and
    // the tree exited on its own — no force escalation required.
    let result = run_shell(
        &unrestricted_test_policy(),
        &ShellConfig::default(),
        Some(&cwd),
        &shell_tree_command(&helper, &["sigterm-marker".to_string(), "60".to_string()]),
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
            .stdout
            .as_deref()
            .unwrap_or_default()
            .contains("SIGTERM_HANDLED"),
        "graceful SIGTERM handler did not run: {result:?}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: spawns a real shell/process tree"]
fn runner_real_process_shell_job_unix_sigterm_resistant_tree_escalates() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().to_string();
    let helper = shell_tree_helper();
    let markers = ShellTreeMarkers::in_dir(tmp.path(), "resist");

    // `ignore-term-keepalive`: the helper (and its descendant) ignore
    // SIGTERM, so the 50ms graceful phase cannot end the tree; only the
    // force escalation (SIGKILL) finishes it, within the cleanup deadline.
    let result = run_shell(
        &unrestricted_test_policy(),
        &ShellConfig::default(),
        Some(&cwd),
        &shell_tree_command(
            &helper,
            &[
                "ignore-term-keepalive".to_string(),
                markers.parent.to_string_lossy().into_owned(),
                markers.alive.to_string_lossy().into_owned(),
                "120".to_string(),
            ],
        ),
        None,
        1,
        None,
    );
    assert!(
        wait_until_file(&markers.parent, Duration::from_secs(15)),
        "tree markers were never written: {result:?}"
    );
    assert_eq!(result.exit_code, Some(-1), "{result:?}");
    assert_eq!(
        result.error.as_deref(),
        Some("command timed out"),
        "{result:?}"
    );
    markers.assert_tree_dead("resist");
}

#[test]
#[ignore = "runner real-process lane: spawns a real shell/process tree"]
fn runner_real_process_shell_job_repeated_stop_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().to_string();
    let helper = shell_tree_helper();
    let stop_requested = Arc::new(AtomicBool::new(false));

    let first = ShellTreeMarkers::in_dir(tmp.path(), "repeat-1");
    let stop_flag = Arc::clone(&stop_requested);
    let stop_marker = first.parent.clone();
    let stopper = std::thread::spawn(move || {
        let written = wait_until_file(&stop_marker, Duration::from_secs(15));
        stop_flag.store(true, Ordering::SeqCst);
        written
    });
    let result = run_shell(
        &unrestricted_test_policy(),
        &shell_tree_test_shell(),
        Some(&cwd),
        &first.keepalive_command(&helper),
        None,
        60,
        Some(stop_requested.as_ref()),
    );
    assert!(stopper.join().expect("stopper thread panicked"));
    assert_eq!(result.exit_code, Some(-1), "{result:?}");
    assert_eq!(result.error.as_deref(), Some("job stopped"), "{result:?}");
    first.assert_tree_dead("repeat-1");

    // A second run against the same already-set flag must stop promptly and
    // clean up its own freshly spawned tree without a panic or deadlock.
    let second = ShellTreeMarkers::in_dir(tmp.path(), "repeat-2");
    let result = run_shell(
        &unrestricted_test_policy(),
        &shell_tree_test_shell(),
        Some(&cwd),
        &second.keepalive_command(&helper),
        None,
        60,
        Some(stop_requested.as_ref()),
    );
    assert_eq!(result.exit_code, Some(-1), "{result:?}");
    assert_eq!(result.error.as_deref(), Some("job stopped"), "{result:?}");
    if wait_until_file(&second.parent, Duration::from_secs(5)) {
        // If the tree got far enough to write markers, it must be dead.
        second.assert_tree_dead("repeat-2");
    }
}

#[test]
#[ignore = "runner real-process lane: spawns a real shell/process tree"]
fn runner_real_process_shell_job_timeout_racing_stop_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().to_string();
    let helper = shell_tree_helper();
    let markers = ShellTreeMarkers::in_dir(tmp.path(), "race");
    let timeout_secs = shell_tree_test_timeout_secs();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop_requested);
    let stop_marker = markers.parent.clone();
    let stopper = std::thread::spawn(move || {
        let written = wait_until_file(&stop_marker, Duration::from_secs(15));
        // Set the stop flag shortly before the timeout would fire; either
        // outcome (stop or timeout) is legitimate for the shell API.
        let delay = if cfg!(windows) { 3600 } else { 600 };
        std::thread::sleep(Duration::from_millis(delay));
        stop_flag.store(true, Ordering::SeqCst);
        written
    });

    let result = run_shell(
        &unrestricted_test_policy(),
        &shell_tree_test_shell(),
        Some(&cwd),
        &markers.keepalive_command(&helper),
        None,
        timeout_secs,
        Some(stop_requested.as_ref()),
    );

    assert!(stopper.join().expect("stopper thread panicked"));
    assert_eq!(result.exit_code, Some(-1), "{result:?}");
    assert!(
        matches!(
            result.error.as_deref(),
            Some("command timed out" | "job stopped")
        ),
        "unexpected race outcome: {result:?}"
    );
    markers.assert_tree_dead("race");
}

#[test]
fn shell_job_spawn_failure_preserves_error() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no-such-shell-program");
    let shell = ShellConfig {
        program: missing.to_string_lossy().into_owned(),
        ..ShellConfig::default()
    };
    let result = run_shell(
        &unrestricted_test_policy(),
        &shell,
        None,
        "true",
        None,
        10,
        None,
    );
    assert_eq!(result.exit_code, None, "{result:?}");
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.starts_with("failed to spawn command: "),
        "spawn error semantics changed: {result:?}"
    );
}
