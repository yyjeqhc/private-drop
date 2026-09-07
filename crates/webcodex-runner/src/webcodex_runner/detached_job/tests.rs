use super::*;
use std::fs::OpenOptions;
use std::sync::{Mutex, OnceLock};

static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[test]
fn default_state_roots_are_scoped_by_server_and_client_identity() {
    let first =
        DetachedJobStore::default_root_for_runner("client-a", "https://server-a.example/").unwrap();
    let first_again =
        DetachedJobStore::default_root_for_runner("client-a", "https://server-a.example").unwrap();
    let other_client =
        DetachedJobStore::default_root_for_runner("client-b", "https://server-a.example").unwrap();
    let other_server =
        DetachedJobStore::default_root_for_runner("client-a", "https://server-b.example").unwrap();

    assert_eq!(first, first_again);
    assert_ne!(first, other_client);
    assert_ne!(first, other_server);
    assert_eq!(first.parent(), other_client.parent());
    assert_eq!(first.parent(), other_server.parent());
}

#[cfg(windows)]
#[test]
fn windows_atomic_state_replace_retries_transient_destination_sharing() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state.json");
    fs::write(&state, br#"{"value":1}"#).unwrap();
    let blocker = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&state)
        .unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(blocker);
    });

    atomic_write_json(&state, &serde_json::json!({"value": 2}), 1024).unwrap();
    release.join().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    assert_eq!(value["value"], 2);
}

#[test]
fn internal_mode_subprocess_entrypoint() {
    let Some(mode) = std::env::var_os("WEBCODEX_DETACHED_TEST_INTERNAL_MODE") else {
        return;
    };
    let args: Vec<String> = serde_json::from_str(
        &std::env::var("WEBCODEX_DETACHED_TEST_INTERNAL_ARGS").expect("internal args"),
    )
    .expect("decode internal args");
    let mut full = vec![mode.to_string_lossy().into_owned()];
    full.extend(args);
    let code = match run_internal_platform_mode(&full) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("detached test internal mode failed: {error}");
            1
        }
    };
    std::process::exit(code);
}

fn safe_context() -> ShellJobContext {
    ShellJobContext {
        runtime_project_id: Some("agent:test:project".to_string()),
        workflow_session_id: Some("wc_sess_test".to_string()),
        ssh_resource: None,
        project_cwd: Some("/tmp/project".to_string()),
        cwd: Some("/tmp/project".to_string()),
        purpose: Some("test".to_string()),
        shell: None,
        command_preview: "native test process".to_string(),
        validation_steps: Vec::new(),
        validation: None,
        structured_execution: None,
    }
}

fn test_request(executable: String, args: Vec<String>) -> DetachedStartRequest {
    DetachedStartRequest {
        job_id: format!("job_{}", Uuid::new_v4().simple()),
        request_id: format!("req_{}", Uuid::new_v4().simple()),
        client_id: "test-runner".to_string(),
        runner_instance_id: "test-instance".to_string(),
        context: safe_context(),
        launch: DetachedLaunchSpec {
            process: ShellProcessArgv { executable, args },
            cwd: None,
            stdin: None,
            env: Vec::new(),
            timeout_secs: 10,
        },
    }
}

#[cfg(any(unix, windows))]
#[test]
fn pre_accept_failure_advances_beyond_public_agent_queued_sequence() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = test_request("never-started".to_string(), Vec::new());
    let prepared = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        PrepareOutcome::Existing(_) => panic!("fresh detached prepare unexpectedly existed"),
    };
    assert_eq!(prepared.phase, DetachedJobPhase::Prepared);
    assert_eq!(prepared.update_seq, 1);

    let terminal = mark_pre_accept_failure(&store, &prepared, "preaccept blocked").unwrap();
    assert_eq!(terminal.phase, DetachedJobPhase::Terminal);
    assert_eq!(terminal.update_seq, 2);
    assert!(terminal.ownership_accepted_at_unix_ms.is_none());
    assert!(terminal.supervisor.is_none());
    assert!(terminal.tree_leader.is_none());
    assert_eq!(terminal.terminal.as_ref().unwrap().status, "handoff_failed");

    let snapshot = snapshot_from_detached_record(&terminal).unwrap();
    assert_eq!(snapshot.update_seq, 2);
    assert_eq!(snapshot.status, "failed");
    assert_eq!(
        snapshot.command_execution_state,
        Some(ShellCommandExecutionState::NotStarted)
    );
}

#[test]
fn durable_record_rejects_mixed_or_oversized_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = test_request("/bin/true".to_string(), Vec::new());
    let record = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        PrepareOutcome::Existing(_) => panic!("unexpected existing state"),
    };
    let state_path = store.state_path_for_job(&request.job_id);
    fs::write(&state_path, b"not-json").unwrap();
    assert!(store.read(&request.job_id).unwrap_err().contains("corrupt"));

    atomic_write_json(&state_path, &record, DETACHED_STATE_MAX_BYTES).unwrap();
    let mut stale = record.clone();
    stale.schema_version += 1;
    atomic_write_json(&state_path, &stale, DETACHED_STATE_MAX_BYTES).unwrap();
    assert!(store
        .read(&request.job_id)
        .unwrap_err()
        .contains("unsupported detached Job state schema"));
}

#[test]
fn durable_record_never_contains_ephemeral_launch_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let sentinel = "wc-detached-private-sentinel-9f3c0a";
    let digest = format!("{:x}", Sha256::digest(sentinel.as_bytes()));
    let mut request = test_request(format!("/tmp/{sentinel}"), vec![sentinel.to_string()]);
    request.launch.stdin = Some(sentinel.to_string());
    request
        .launch
        .env
        .push(("PRIVATE_TOKEN".to_string(), sentinel.to_string()));
    let _ = store.prepare(&request).unwrap();
    let bytes = fs::read(store.state_path_for_job(&request.job_id)).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        !text.contains(sentinel),
        "durable state leaked launch body: {text}"
    );
    assert!(
        !text.contains(&digest),
        "durable state leaked launch-body digest: {text}"
    );
    assert!(!text.contains("PRIVATE_TOKEN"));
}

#[test]
fn durable_state_root_has_a_hard_record_count_bound() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    for _ in 0..DETACHED_STATE_MAX_RECORDS {
        let request = test_request("/bin/true".to_string(), Vec::new());
        assert!(matches!(
            store.prepare(&request).unwrap(),
            PrepareOutcome::First(_)
        ));
    }
    let request = test_request("/bin/true".to_string(), Vec::new());
    let error = store.prepare(&request).unwrap_err();
    assert!(error.contains("state root is full"), "{error}");
    assert_eq!(
        store.scan_for_client("test-runner").unwrap().len(),
        DETACHED_STATE_MAX_RECORDS
    );
}

fn terminal_record(store: &DetachedJobStore) -> DetachedStartRequest {
    let request = test_request("/bin/true".to_string(), Vec::new());
    let prepared = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        PrepareOutcome::Existing(_) => panic!("fresh terminal fixture unexpectedly existed"),
    };
    store
        .update(&request.job_id, &prepared.execution_id, |record| {
            set_terminal(
                record,
                "completed",
                Some(0),
                None,
                record.created_at_unix_ms,
            );
            Ok(())
        })
        .unwrap();
    request
}

#[test]
fn terminal_reclamation_respects_retention_window_and_then_deletes() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = terminal_record(&store);
    let completed = store
        .read(&request.job_id)
        .unwrap()
        .terminal
        .unwrap()
        .completed_at_unix_ms;

    assert_eq!(
        store
            .reclaim_expired_terminal_records_locked(completed + TERMINAL_RETENTION_MS - 1)
            .unwrap(),
        1
    );
    assert!(store.read(&request.job_id).is_ok());

    assert_eq!(
        store
            .reclaim_expired_terminal_records_locked(completed + TERMINAL_RETENTION_MS)
            .unwrap(),
        0
    );
    assert!(store.read(&request.job_id).is_err());
}

#[cfg(unix)]
#[test]
fn accepted_active_record_is_never_reclaimed() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = make_payload_request("linger", Vec::new());
    let running = handoff_and_wait_running(&store, &request);
    assert!(running.ownership_accepted_at_unix_ms.is_some());
    assert_eq!(running.phase, DetachedJobPhase::Running);

    assert_eq!(
        store
            .reclaim_expired_terminal_records_locked(i64::MAX)
            .unwrap(),
        1
    );
    let retained = store.read(&request.job_id).unwrap();
    assert_eq!(retained.execution_id, running.execution_id);
    assert_eq!(retained.phase, DetachedJobPhase::Running);
    let stopped = store
        .request_stop(&request.job_id, &running.execution_id)
        .unwrap();
    assert!(stopped.stop_requested);
    let terminal = wait_for_terminal(&store, &request.job_id);
    assert_eq!(terminal.terminal.as_ref().unwrap().status, "stopped");
}

#[cfg(unix)]
#[test]
fn reclamation_fails_closed_on_corrupt_or_symlink_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = terminal_record(&store);
    let job_dir = store.job_dir(&request.job_id);
    let completed = store
        .read(&request.job_id)
        .unwrap()
        .terminal
        .unwrap()
        .completed_at_unix_ms;
    let unexpected_target = temp.path().join("do-not-delete");
    fs::write(&unexpected_target, b"sentinel").unwrap();
    std::os::unix::fs::symlink(&unexpected_target, job_dir.join("unexpected-link")).unwrap();

    let error = store
        .reclaim_expired_terminal_records_locked(completed + TERMINAL_RETENTION_MS)
        .unwrap_err();
    assert!(error.contains("symlink"), "{error}");
    assert_eq!(fs::read(&unexpected_target).unwrap(), b"sentinel");
    assert!(job_dir.exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn prepared_restart_residue_converges_to_not_started_without_payload() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let marker = temp.path().join("must-not-run");
    let request = make_payload_request(
        "count_once",
        vec![(
            "PAYLOAD_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        )],
    );
    let prepared = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        PrepareOutcome::Existing(_) => panic!("fresh pre-accept fixture unexpectedly existed"),
    };
    let reconciled = store
        .reconcile_after_runner_restart(prepared)
        .unwrap()
        .unwrap();
    assert_eq!(reconciled.phase, DetachedJobPhase::Terminal);
    assert_eq!(
        reconciled.terminal.as_ref().unwrap().status,
        "handoff_failed"
    );
    assert_eq!(
        snapshot_from_detached_record(&reconciled)
            .unwrap()
            .command_execution_state,
        Some(ShellCommandExecutionState::NotStarted)
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !marker.exists(),
        "pre-accept recovery must never spawn the payload"
    );
}

#[test]
fn expired_terminal_record_releases_capacity_for_new_prepare() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let expired = terminal_record(&store);
    let mut expired_record = store.read(&expired.job_id).unwrap();
    expired_record
        .terminal
        .as_mut()
        .unwrap()
        .completed_at_unix_ms = unix_ms().saturating_sub(TERMINAL_RETENTION_MS + 1);
    atomic_write_json(
        &store.state_path_for_job(&expired.job_id),
        &expired_record,
        DETACHED_STATE_MAX_BYTES,
    )
    .unwrap();

    for _ in 1..DETACHED_STATE_MAX_RECORDS {
        let request = test_request("/bin/true".to_string(), Vec::new());
        assert!(matches!(
            store.prepare(&request).unwrap(),
            PrepareOutcome::First(_)
        ));
    }
    let replacement = test_request("/bin/true".to_string(), Vec::new());
    assert!(matches!(
        store.prepare(&replacement).unwrap(),
        PrepareOutcome::First(_)
    ));
    assert!(store.read(&expired.job_id).is_err());
    assert!(store.read(&replacement.job_id).is_ok());
    assert_eq!(
        store.scan_for_client("test-runner").unwrap().len(),
        DETACHED_STATE_MAX_RECORDS
    );
}

#[test]
fn duplicate_prepare_keeps_one_execution_identity() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = test_request("/bin/true".to_string(), Vec::new());
    let first = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        _ => panic!("first prepare must claim"),
    };
    let second = match store.prepare(&request).unwrap() {
        PrepareOutcome::Existing(record) => record,
        _ => panic!("second prepare must reuse durable claim"),
    };
    assert_eq!(first.execution_id, second.execution_id);
    assert_eq!(first, second);
}

#[test]
fn output_tail_is_bounded_while_total_bytes_and_line_cursors_continue() {
    let mut output = DetachedOutputState::default();
    let text = "line\n".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES / 5 + 1024);
    append_output_tail(&mut output, text.len(), &text);
    assert_eq!(output.total_bytes, text.len() as u64);
    assert!(output.retained_bytes <= JOB_SNAPSHOT_STREAM_MAX_BYTES);
    assert_eq!(output.retained_bytes, output.tail.len());
    assert!(output.first_retained_line > 1);
    assert_eq!(output.next_line, 1 + text.lines().count());
    assert!(output.truncated);
    validate_output_state("stdout", &output).unwrap();

    let previous_next = output.next_line;
    append_output_tail(&mut output, 5, "tail\n");
    assert_eq!(output.next_line, previous_next + 1);
    validate_output_state("stdout", &output).unwrap();
}

#[test]
fn detached_job_persistence_preserves_historical_agent_instance_key() {
    let request = test_request("/bin/true".to_string(), Vec::new());
    let request_value = serde_json::to_value(&request).unwrap();
    assert_eq!(request_value["agent_instance_id"], "test-instance");
    assert!(request_value.get("runner_instance_id").is_none());

    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let record = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        _ => panic!("first prepare must claim"),
    };
    let record_value = serde_json::to_value(&record).unwrap();
    assert_eq!(record_value["agent_instance_id"], "test-instance");
    assert!(record_value.get("runner_instance_id").is_none());
    let decoded: DetachedJobRecord = serde_json::from_value(record_value).unwrap();
    assert_eq!(decoded.runner_instance_id, "test-instance");
}

#[test]
fn durable_record_bound_covers_worst_case_escaped_output_tails() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = test_request("/bin/true".to_string(), Vec::new());
    let mut record = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        _ => panic!("first prepare must claim"),
    };
    let escaped_tail = "\0".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES);
    record.stdout.tail = escaped_tail.clone();
    record.stdout.retained_bytes = record.stdout.tail.len();
    record.stdout.total_bytes = record.stdout.tail.len() as u64;
    record.stdout.first_retained_line = 17;
    record.stdout.next_line = 18;
    record.stdout.truncated = true;
    record.stderr.tail = escaped_tail;
    record.stderr.retained_bytes = record.stderr.tail.len();
    record.stderr.total_bytes = record.stderr.tail.len() as u64;
    record.stderr.first_retained_line = 23;
    record.stderr.next_line = 24;
    record.stderr.truncated = true;
    validate_record(&record).unwrap();
    let encoded = serde_json::to_vec(&record).unwrap();
    assert!(encoded.len() < DETACHED_STATE_MAX_BYTES);
    atomic_write_json(
        &store.state_path_for_job(&request.job_id),
        &record,
        DETACHED_STATE_MAX_BYTES,
    )
    .unwrap();
    assert_eq!(store.read(&request.job_id).unwrap(), record);
}

#[cfg(unix)]
#[test]
fn durable_state_and_lock_paths_reject_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let real_root = temp.path().join("real-root");
    fs::create_dir(&real_root).unwrap();
    let state_link = temp.path().join("state-link");
    std::os::unix::fs::symlink(&real_root, &state_link).unwrap();
    let error = ensure_private_dir(&state_link).unwrap_err();
    assert!(error.contains("not a symlink"), "{error}");

    let target = temp.path().join("lock-target");
    fs::write(&target, b"do-not-touch").unwrap();
    let lock_link = temp.path().join("lock-link");
    std::os::unix::fs::symlink(&target, &lock_link).unwrap();
    assert!(exclusive_lock(&lock_link, false).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"do-not-touch");
}

#[cfg(unix)]
#[test]
fn stale_watchdog_invocation_fails_before_tree_lock_creation() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = test_request("/bin/true".to_string(), Vec::new());
    let prepared = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        _ => panic!("first prepare must claim"),
    };
    let job_dir = store.job_dir(&request.job_id);
    let error = run_watchdog(
        &job_dir,
        &format!("birth_{}", Uuid::new_v4().simple()),
        &prepared.execution_id,
    )
    .unwrap_err();
    assert!(
        error.contains("stale detached watchdog invocation"),
        "{error}"
    );
    assert!(!job_dir.join(TREE_LOCK_FILE).exists());
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) performs a liveness/permission probe only.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

#[cfg(unix)]
fn wait_for_terminal(store: &DetachedJobStore, job_id: &str) -> DetachedJobRecord {
    assert!(wait_until(Duration::from_secs(15), || {
        store
            .read(job_id)
            .is_ok_and(|record| record.phase == DetachedJobPhase::Terminal)
    }));
    store.read(job_id).unwrap()
}

#[cfg(unix)]
fn payload_command(
    scenario: &str,
    env: Vec<(String, String)>,
) -> (String, Vec<String>, Vec<(String, String)>) {
    let executable = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let args = vec![
        "--exact".to_string(),
        "webcodex_runner::detached_job::tests::payload_subprocess_entrypoint".to_string(),
        "--nocapture".to_string(),
    ];
    let mut all_env = env;
    all_env.push((
        "WEBCODEX_DETACHED_PAYLOAD_SCENARIO".to_string(),
        scenario.to_string(),
    ));
    (executable, args, all_env)
}

#[cfg(unix)]
#[test]
fn payload_subprocess_entrypoint() {
    let Some(scenario) = std::env::var_os("WEBCODEX_DETACHED_PAYLOAD_SCENARIO") else {
        return;
    };
    match scenario.to_string_lossy().as_ref() {
        "delayed_marker" => {
            let marker = PathBuf::from(std::env::var_os("PAYLOAD_MARKER").unwrap());
            std::thread::sleep(Duration::from_millis(400));
            fs::write(marker, b"done").unwrap();
        }
        "count_once" => {
            let marker = PathBuf::from(std::env::var_os("PAYLOAD_MARKER").unwrap());
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(marker)
                .unwrap();
            writeln!(file, "run").unwrap();
            file.flush().unwrap();
            std::thread::sleep(Duration::from_secs(2));
        }
        "linger" => {
            std::thread::sleep(Duration::from_secs(60));
        }
        "output_flood" => {
            let stdout = vec![b'o'; JOB_SNAPSHOT_STREAM_MAX_BYTES * 5];
            let stderr = vec![b'e'; JOB_SNAPSHOT_STREAM_MAX_BYTES * 4];
            io::stdout().write_all(&stdout).unwrap();
            io::stdout().write_all(b"STDOUT_END\n").unwrap();
            io::stdout().flush().unwrap();
            io::stderr().write_all(&stderr).unwrap();
            io::stderr().write_all(b"STDERR_END\n").unwrap();
            io::stderr().flush().unwrap();
        }
        "tree" => {
            let parent_marker = PathBuf::from(std::env::var_os("PARENT_PID_MARKER").unwrap());
            let child_marker = PathBuf::from(std::env::var_os("CHILD_PID_MARKER").unwrap());
            fs::write(&parent_marker, std::process::id().to_string()).unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                    .arg("--exact")
                    .arg("webcodex_runner::detached_job::tests::payload_descendant_subprocess_entrypoint")
                    .arg("--nocapture")
                    .env_clear()
                    .env("WEBCODEX_DETACHED_DESCENDANT_MARKER", child_marker)
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            #[allow(clippy::zombie_processes)]
            let _child = child.spawn().unwrap();
            std::thread::sleep(Duration::from_secs(60));
        }
        other => panic!("unknown detached payload scenario: {other}"),
    }
}

#[cfg(unix)]
#[test]
fn payload_descendant_subprocess_entrypoint() {
    let Some(marker) = std::env::var_os("WEBCODEX_DETACHED_DESCENDANT_MARKER") else {
        return;
    };
    fs::write(marker, std::process::id().to_string()).unwrap();
    std::thread::sleep(Duration::from_secs(60));
}

#[cfg(unix)]
fn make_payload_request(scenario: &str, env: Vec<(String, String)>) -> DetachedStartRequest {
    let (executable, args, env) = payload_command(scenario, env);
    let mut request = test_request(executable, args);
    request.launch.env = env;
    request
}

#[cfg(unix)]
fn wait_for_running_record(store: &DetachedJobStore, job_id: &str) -> DetachedJobRecord {
    assert!(
        wait_until(Duration::from_secs(5), || store.read(job_id).is_ok_and(
            |record| {
                record.phase == DetachedJobPhase::Running
                    && record.ownership_accepted_at_unix_ms.is_some()
            }
        )),
        "detached execution never reached an accepted Running state: {job_id}"
    );
    store.read(job_id).unwrap()
}

#[cfg(unix)]
fn handoff_and_wait_running(
    store: &DetachedJobStore,
    request: &DetachedStartRequest,
) -> DetachedJobRecord {
    let outcome = handoff_detached_job(store, request.clone()).unwrap();
    assert!(matches!(outcome, DetachedHandoffOutcome::Accepted { .. }));
    wait_for_running_record(store, &request.job_id)
}

#[cfg(unix)]
fn run_accept_then_exit_owner(temp: &Path, state_root: &Path, request: &DetachedStartRequest) {
    let instruction = temp.join("accept-exit-owner.json");
    fs::write(
        &instruction,
        serde_json::to_vec(&(state_root.to_path_buf(), request.clone())).unwrap(),
    )
    .unwrap();
    let mut owner = Command::new(std::env::current_exe().unwrap());
    owner
        .arg("--exact")
        .arg("webcodex_runner::detached_job::tests::accept_then_exit_owner_subprocess_entrypoint")
        .arg("--nocapture")
        .env_clear()
        .env("WEBCODEX_DETACHED_ACCEPT_EXIT_INSTRUCTION", &instruction)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    assert!(owner.spawn().unwrap().wait().unwrap().success());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tree_payload_request(temp: &Path) -> (DetachedStartRequest, PathBuf, PathBuf) {
    let parent_marker = temp.join("parent.pid");
    let child_marker = temp.join("child.pid");
    let request = make_payload_request(
        "tree",
        vec![
            (
                "PARENT_PID_MARKER".to_string(),
                parent_marker.to_string_lossy().into_owned(),
            ),
            (
                "CHILD_PID_MARKER".to_string(),
                child_marker.to_string_lossy().into_owned(),
            ),
        ],
    );
    (request, parent_marker, child_marker)
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: detached supervisor outlives its owner process"]
fn runner_real_process_accepted_handoff_keeps_payload_alive_after_owner_process_exits() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let state_root = temp.path().join("state");
    let marker = temp.path().join("marker");
    let request = make_payload_request(
        "delayed_marker",
        vec![(
            "PAYLOAD_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        )],
    );
    let instruction = temp.path().join("owner.json");
    let result_path = temp.path().join("owner-result.json");
    fs::write(
        &instruction,
        serde_json::to_vec(&(state_root.clone(), request.clone(), result_path.clone())).unwrap(),
    )
    .unwrap();
    let mut owner = Command::new(std::env::current_exe().unwrap());
    owner
        .arg("--exact")
        .arg("webcodex_runner::detached_job::tests::owner_subprocess_entrypoint")
        .arg("--nocapture")
        .env_clear()
        .env("WEBCODEX_DETACHED_OWNER_INSTRUCTION", &instruction)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = owner.spawn().unwrap().wait().unwrap();
    assert!(status.success());
    assert!(result_path.exists());
    assert!(wait_until(Duration::from_secs(5), || marker.exists()));
    let store = DetachedJobStore::new(state_root);
    let terminal = wait_for_terminal(&store, &request.job_id);
    assert_eq!(terminal.terminal.as_ref().unwrap().status, "completed");
}

#[cfg(unix)]
#[test]
fn owner_subprocess_entrypoint() {
    let Some(path) = std::env::var_os("WEBCODEX_DETACHED_OWNER_INSTRUCTION") else {
        return;
    };
    let (root, request, result_path): (PathBuf, DetachedStartRequest, PathBuf) =
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let store = DetachedJobStore::new(root);
    let outcome = handoff_detached_job(&store, request).unwrap();
    fs::write(result_path, format!("{outcome:?}")).unwrap();
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: detached handoff survives owner loss after accept"]
fn runner_real_process_accepted_handoff_survives_owner_exit_before_ack() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let state_root = temp.path().join("state");
    let marker = temp.path().join("count");
    let request = make_payload_request(
        "count_once",
        vec![(
            "PAYLOAD_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        )],
    );
    run_accept_then_exit_owner(temp.path(), &state_root, &request);

    let store = DetachedJobStore::new(state_root);
    let terminal = wait_for_terminal(&store, &request.job_id);
    assert!(terminal.ownership_accepted_at_unix_ms.is_some());
    assert_eq!(terminal.terminal.as_ref().unwrap().status, "completed");
    let replay = handoff_detached_job(&store, request.clone()).unwrap();
    assert!(matches!(
        replay,
        DetachedHandoffOutcome::Accepted {
            reconciled_from_state: true,
            ..
        }
    ));
    let runs = fs::read_to_string(marker).unwrap();
    assert_eq!(runs.lines().count(), 1);
}

#[cfg(unix)]
#[test]
fn accept_then_exit_owner_subprocess_entrypoint() {
    let Some(path) = std::env::var_os("WEBCODEX_DETACHED_ACCEPT_EXIT_INSTRUCTION") else {
        return;
    };
    let (root, request): (PathBuf, DetachedStartRequest) =
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let store = DetachedJobStore::new(root);
    let prepared = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        _ => panic!("accept-exit owner expected a fresh durable claim"),
    };
    let job_dir = store.job_dir(&request.job_id);
    let supervisor_birth = format!("birth_{}", Uuid::new_v4().simple());
    let mut command = internal_mode_command(
        DETACHED_INTERNAL_SUPERVISOR,
        &[
            job_dir.to_string_lossy().into_owned(),
            prepared.execution_id.clone(),
            supervisor_birth,
        ],
    )
    .unwrap();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    make_new_session(&mut command);
    let mut child = command.spawn().unwrap();
    let mut child_stdin = child.stdin.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();
    let handshake = spawn_byte_reader(child_stderr);
    write_launch_frame(&mut child_stdin, &request.launch).unwrap();
    assert_eq!(
        handshake.recv_timeout(DETACHED_HANDOFF_TIMEOUT).unwrap(),
        HANDSHAKE_READY
    );
    child_stdin.write_all(&[HANDSHAKE_ACCEPT]).unwrap();
    child_stdin.flush().unwrap();
    // Exit without reading HANDSHAKE_ACCEPTED or reaping the direct
    // supervisor. This simulates lost Runner response/ownership after the
    // commit byte has been sent.
    std::process::exit(0);
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: detached handoff owns a real payload process"]
fn runner_real_process_duplicate_handoff_never_spawns_a_second_payload() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let marker = temp.path().join("count");
    let request = make_payload_request(
        "count_once",
        vec![(
            "PAYLOAD_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        )],
    );
    let first = handoff_detached_job(&store, request.clone()).unwrap();
    assert!(matches!(first, DetachedHandoffOutcome::Accepted { .. }));
    let second = handoff_detached_job(&store, request.clone()).unwrap();
    assert!(matches!(
        second,
        DetachedHandoffOutcome::Accepted {
            reconciled_from_state: true,
            ..
        }
    ));
    let _ = wait_for_terminal(&store, &request.job_id);
    let runs = fs::read_to_string(marker).unwrap();
    assert_eq!(runs.lines().count(), 1);
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: detached update sequencing observes a live payload"]
fn runner_real_process_durable_update_sequence_advances_and_duplicate_handoff_does_not() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    // Keep the payload alive until this test explicitly stops it. The previous
    // count_once fixture could finish before handoff_detached_job returned on a
    // loaded runner, making the first observation terminal and turning the
    // sequence assertion into a scheduler-speed assumption.
    let request = make_payload_request("linger", Vec::new());
    let running = handoff_and_wait_running(&store, &request);
    assert!(running.update_seq >= 3);
    let sequence = running.update_seq;
    let replay = handoff_detached_job(&store, request.clone()).unwrap();
    assert!(matches!(replay, DetachedHandoffOutcome::Accepted { .. }));
    assert_eq!(store.read(&request.job_id).unwrap().update_seq, sequence);
    let stopped = store
        .request_stop(&request.job_id, &running.execution_id)
        .unwrap();
    assert_eq!(stopped.update_seq, sequence + 1);
    let terminal = wait_for_terminal(&store, &request.job_id);
    assert_eq!(terminal.terminal.as_ref().unwrap().status, "stopped");
    assert!(terminal.update_seq > stopped.update_seq);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore = "runner real-process lane: restart reconciliation observes a live detached supervisor"]
fn runner_real_process_restart_scan_reconciles_live_detached_execution_without_respawn() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let state_root = temp.path().join("state");
    let marker = temp.path().join("count");
    let request = make_payload_request(
        "count_once",
        vec![(
            "PAYLOAD_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        )],
    );
    run_accept_then_exit_owner(temp.path(), &state_root, &request);

    let store = DetachedJobStore::new(state_root);
    let _running = wait_for_running_record(&store, &request.job_id);
    let records = store.scan_for_client(&request.client_id).unwrap();
    assert_eq!(records.len(), 1);
    let recovered = store
        .reconcile_after_runner_restart(records.into_iter().next().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(recovered.job_id, request.job_id);
    assert_eq!(recovered.phase, DetachedJobPhase::Running);
    let snapshot = snapshot_from_detached_record(&recovered).unwrap();
    assert_eq!(snapshot.status, "running");
    assert_eq!(snapshot.update_seq, recovered.update_seq);
    let terminal = wait_for_terminal(&store, &request.job_id);
    assert_eq!(terminal.terminal.as_ref().unwrap().status, "completed");
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 1);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore = "runner real-process lane: durable stop terminates a detached process tree"]
fn runner_real_process_durable_stop_request_terminates_exact_supervisor_owned_tree() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let (request, parent_marker, child_marker) = tree_payload_request(temp.path());
    let running = handoff_and_wait_running(&store, &request);
    assert!(wait_until(Duration::from_secs(5), || {
        parent_marker.exists() && child_marker.exists()
    }));
    let parent_pid: u32 = fs::read_to_string(&parent_marker).unwrap().parse().unwrap();
    let child_pid: u32 = fs::read_to_string(&child_marker).unwrap().parse().unwrap();
    let stopped = store
        .request_stop(&request.job_id, &running.execution_id)
        .unwrap();
    assert!(stopped.stop_requested);
    assert_eq!(stopped.update_seq, running.update_seq + 1);
    let terminal = wait_for_terminal(&store, &request.job_id);
    assert_eq!(terminal.terminal.as_ref().unwrap().status, "stopped");
    assert!(terminal.update_seq > stopped.update_seq);
    assert!(wait_until(Duration::from_secs(5), || !process_alive(
        parent_pid
    )));
    assert!(wait_until(Duration::from_secs(5), || !process_alive(
        child_pid
    )));
    let snapshot = snapshot_from_detached_record(&terminal).unwrap();
    assert_eq!(snapshot.status, "stopped");
    assert_eq!(
        snapshot.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
}

#[cfg(target_os = "linux")]
fn stale_native_start_identity() -> &'static str {
    "linux_start_0"
}

#[cfg(target_os = "macos")]
fn stale_native_start_identity() -> &'static str {
    "macos_start_0_0"
}

#[cfg(target_os = "macos")]
#[test]
fn macos_native_process_start_identity_is_stable() {
    let pid = std::process::id();
    let first = native_process_start_identity(pid).unwrap();
    let second = native_process_start_identity(pid).unwrap();
    assert!(first.starts_with("macos_start_"));
    assert_eq!(first, second);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore = "runner real-process lane: stale supervisor identity is checked against a live process"]
fn runner_real_process_stale_native_supervisor_identity_reconciles_to_lost_without_respawn() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = make_payload_request("linger", Vec::new());
    let mut running = handoff_and_wait_running(&store, &request);
    let real_supervisor_pid = running.supervisor.as_ref().unwrap().pid;
    running.supervisor.as_mut().unwrap().native_start_id =
        stale_native_start_identity().to_string();
    atomic_write_json(
        &store.state_path_for_job(&request.job_id),
        &running,
        DETACHED_STATE_MAX_BYTES,
    )
    .unwrap();
    let lost = store
        .reconcile_after_runner_restart(running)
        .unwrap()
        .unwrap();
    assert_eq!(lost.phase, DetachedJobPhase::Terminal);
    assert_eq!(lost.terminal.as_ref().unwrap().status, "supervisor_lost");
    let snapshot = snapshot_from_detached_record(&lost).unwrap();
    assert_eq!(snapshot.status, "lost");
    assert_eq!(
        snapshot.command_execution_state,
        Some(ShellCommandExecutionState::OutcomeUnknown)
    );
    // The replacement reconciler never signals by numeric PID. Clean up the
    // deliberately still-live test supervisor only after the assertion.
    unsafe {
        libc::kill(real_supervisor_pid as i32, libc::SIGKILL);
    }
    assert!(wait_until(Duration::from_secs(5), || !process_alive(
        real_supervisor_pid
    )));
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: detached supervisor drains a real child process"]
fn runner_real_process_supervisor_continuously_drains_and_bounds_both_output_streams() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = make_payload_request("output_flood", Vec::new());
    let _ = handoff_detached_job(&store, request.clone()).unwrap();
    let terminal = wait_for_terminal(&store, &request.job_id);
    assert!(terminal.stdout.total_bytes > (JOB_SNAPSHOT_STREAM_MAX_BYTES * 4) as u64);
    assert!(terminal.stderr.total_bytes > (JOB_SNAPSHOT_STREAM_MAX_BYTES * 3) as u64);
    assert!(terminal.stdout.truncated);
    assert!(terminal.stderr.truncated);
    assert!(terminal.stdout.retained_bytes <= JOB_SNAPSHOT_STREAM_MAX_BYTES);
    assert!(terminal.stderr.retained_bytes <= JOB_SNAPSHOT_STREAM_MAX_BYTES);
    assert!(terminal.stdout.tail.contains("STDOUT_END"));
    assert!(terminal.stderr.tail.contains("STDERR_END"));
}

#[cfg(unix)]
#[test]
#[ignore = "runner real-process lane: terminal persistence races a real detached child"]
fn runner_real_process_terminal_state_is_atomically_rereadable() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = make_payload_request("output_flood", Vec::new());
    let job_id = request.job_id.clone();
    let reader_store = store.clone();
    let reader = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut observed_state = false;
        loop {
            match reader_store.read(&job_id) {
                Ok(record) => {
                    observed_state = true;
                    if record.phase == DetachedJobPhase::Terminal {
                        return record;
                    }
                }
                Err(error) if !observed_state && error.contains("No such file") => {}
                Err(error) => panic!("atomic state read after first commit failed: {error}"),
            }
            assert!(Instant::now() < deadline, "terminal state timeout");
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let _ = handoff_detached_job(&store, request.clone()).unwrap();
    let observed = reader.join().unwrap();
    let reread = store.read(&request.job_id).unwrap();
    assert_eq!(observed, reread);
    assert_eq!(reread.phase, DetachedJobPhase::Terminal);
}

#[cfg(target_os = "linux")]
fn linux_child_pids(pid: u32) -> Vec<u32> {
    let mut children = Vec::new();
    let tasks = match fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(tasks) => tasks,
        Err(_) => return children,
    };
    for task in tasks.flatten() {
        let path = task.path().join("children");
        children.extend(
            fs::read_to_string(path)
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok()),
        );
    }
    children.sort_unstable();
    children.dedup();
    children
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "runner real-process lane: pre-accept disconnect controls a real supervisor process"]
fn runner_real_process_pre_accept_owner_disconnect_is_terminal_and_replay_never_spawns() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let marker = temp.path().join("must-not-run");
    let request = make_payload_request(
        "delayed_marker",
        vec![(
            "PAYLOAD_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        )],
    );
    let prepared = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        _ => panic!("first prepare"),
    };
    let job_dir = store.job_dir(&request.job_id);
    let supervisor_birth = format!("birth_{}", Uuid::new_v4().simple());
    let mut command = internal_mode_command(
        DETACHED_INTERNAL_SUPERVISOR,
        &[
            job_dir.to_string_lossy().into_owned(),
            prepared.execution_id.clone(),
            supervisor_birth,
        ],
    )
    .unwrap();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    make_new_session(&mut command);
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let handshake = spawn_byte_reader(stderr);
    write_launch_frame(&mut stdin, &request.launch).unwrap();
    assert_eq!(
        handshake.recv_timeout(DETACHED_HANDOFF_TIMEOUT).unwrap(),
        HANDSHAKE_READY
    );
    let children = linux_child_pids(child.id());
    assert!(
        children.is_empty(),
        "pre-accept supervisor must not start a tree helper or payload"
    );
    drop(stdin);
    assert!(wait_until(Duration::from_secs(5), || child
        .try_wait()
        .unwrap()
        .is_some()));
    let terminal = wait_for_terminal(&store, &request.job_id);
    assert!(terminal.ownership_accepted_at_unix_ms.is_none());
    assert!(terminal.tree_leader.is_none());
    assert_eq!(terminal.terminal.as_ref().unwrap().status, "handoff_failed");
    assert!(!marker.exists());

    let replay = handoff_detached_job(&store, request.clone()).unwrap();
    assert!(matches!(
        replay,
        DetachedHandoffOutcome::PreAcceptFailed { .. }
    ));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "runner real-process lane: pre-accept supervisor death exercises real process cleanup"]
fn runner_real_process_pre_accept_supervisor_death_leaves_no_internal_or_payload_orphan() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = make_payload_request("delayed_marker", Vec::new());
    let prepared = match store.prepare(&request).unwrap() {
        PrepareOutcome::First(record) => record,
        _ => panic!("first prepare"),
    };
    let job_dir = store.job_dir(&request.job_id);
    let supervisor_birth = format!("birth_{}", Uuid::new_v4().simple());
    let mut command = internal_mode_command(
        DETACHED_INTERNAL_SUPERVISOR,
        &[
            job_dir.to_string_lossy().into_owned(),
            prepared.execution_id.clone(),
            supervisor_birth,
        ],
    )
    .unwrap();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    make_new_session(&mut command);
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let handshake = spawn_byte_reader(stderr);
    write_launch_frame(&mut stdin, &request.launch).unwrap();
    assert_eq!(
        handshake.recv_timeout(DETACHED_HANDOFF_TIMEOUT).unwrap(),
        HANDSHAKE_READY
    );
    let supervisor_pid = child.id();
    let children = linux_child_pids(supervisor_pid);
    assert!(
        children.is_empty(),
        "pre-accept supervisor must not start a tree helper or payload"
    );
    unsafe {
        libc::kill(supervisor_pid as i32, libc::SIGKILL);
    }
    assert!(wait_until(Duration::from_secs(5), || child
        .try_wait()
        .unwrap()
        .is_some()));
    let record = store.read(&request.job_id).unwrap();
    assert!(record.ownership_accepted_at_unix_ms.is_none());
    assert!(record.tree_leader.is_none());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore = "runner real-process lane: supervisor death terminates a real detached process tree"]
fn runner_real_process_supervisor_death_terminates_payload_process_tree() {
    let _guard = test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let (request, parent_marker, child_marker) = tree_payload_request(temp.path());
    let running = handoff_and_wait_running(&store, &request);
    assert!(wait_until(Duration::from_secs(5), || {
        parent_marker.exists() && child_marker.exists()
    }));
    let parent_pid: u32 = fs::read_to_string(&parent_marker).unwrap().parse().unwrap();
    let child_pid: u32 = fs::read_to_string(&child_marker).unwrap().parse().unwrap();
    assert_eq!(running.phase, DetachedJobPhase::Running);
    let supervisor_pid = running.supervisor.as_ref().unwrap().pid;
    assert!(process_alive(supervisor_pid));
    assert!(process_alive(parent_pid));
    assert!(process_alive(child_pid));
    let job_dir = store.job_dir(&request.job_id);
    let supervisor_identity = running.supervisor.as_ref().unwrap();
    let tree_identity = running.tree_leader.as_ref().unwrap();
    assert_eq!(
        supervisor_identity.native_start_id,
        native_process_start_identity(supervisor_identity.pid).unwrap()
    );
    assert_eq!(
        tree_identity.native_start_id,
        native_process_start_identity(tree_identity.pid).unwrap()
    );
    assert!(lifetime_lock_is_held(
        &job_dir.join(SUPERVISOR_LOCK_FILE),
        &supervisor_identity.creation_id,
    )
    .unwrap());
    assert!(
        lifetime_lock_is_held(&job_dir.join(TREE_LOCK_FILE), &tree_identity.creation_id,).unwrap()
    );
    unsafe {
        libc::kill(supervisor_pid as i32, libc::SIGKILL);
    }
    assert!(wait_until(Duration::from_secs(10), || !process_alive(
        parent_pid
    )));
    assert!(wait_until(Duration::from_secs(10), || !process_alive(
        child_pid
    )));
    assert!(wait_until(Duration::from_secs(10), || {
        !process_alive(running.tree_leader.as_ref().unwrap().pid)
    }));
    assert!(wait_until(Duration::from_secs(10), || {
        lifetime_lock_is_held(
            &job_dir.join(SUPERVISOR_LOCK_FILE),
            &supervisor_identity.creation_id,
        ) == Ok(false)
    }));
    assert!(wait_until(Duration::from_secs(10), || {
        lifetime_lock_is_held(&job_dir.join(TREE_LOCK_FILE), &tree_identity.creation_id)
            == Ok(false)
    }));
}
