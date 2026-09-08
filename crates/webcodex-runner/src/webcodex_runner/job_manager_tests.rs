use super::*;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::webcodex_runner::detached_job::{
    handoff_detached_job, DetachedJobPhase, DetachedJobStore, DetachedLaunchSpec,
    DetachedStartRequest,
};
use serde_json::json;
use std::ffi::OsString;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use tempfile::TempDir;

fn retained_terminal_job(job_id: &str, ended_at: i64) -> RunningJob {
    let mut snapshot = test_job_snapshot(job_id);
    snapshot.status = "completed".to_string();
    snapshot.ended_at = Some(ended_at);
    snapshot.exit_code = Some(0);
    snapshot.duration_ms = Some(1);
    RunningJob {
        client_id: "test-agent".to_string(),
        runner_instance_id: "test-instance".to_string(),
        snapshot,
        child: None,
        stop_requested: Arc::new(AtomicBool::new(false)),
        slot_reserved: false,
    }
}

#[test]
fn job_reconciliation_inventory_prioritizes_active_and_bounds_terminal_history() {
    let manager = JobManager::new(1);
    let now = chrono::Utc::now().timestamp();
    let mut active = test_job_snapshot("active-original-job");
    active.created_at = now - 100;
    active.context.command_preview = "safe preview".to_string();
    lock_unpoison(&manager.jobs).insert(
        active.job_id.clone(),
        RunningJob {
            client_id: "test-agent".to_string(),
            runner_instance_id: "test-instance".to_string(),
            snapshot: active,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );
    for index in 0..(JOB_INVENTORY_MAX_TERMINAL_JOBS + 8) {
        let job_id = format!("terminal-{index}");
        lock_unpoison(&manager.jobs).insert(
            job_id.clone(),
            retained_terminal_job(&job_id, now - index as i64),
        );
    }
    let expired_id = "terminal-expired";
    lock_unpoison(&manager.jobs).insert(
        expired_id.to_string(),
        retained_terminal_job(expired_id, now - JOB_TERMINAL_RETENTION_SECS),
    );

    let inventory = manager.inventory();
    assert!(inventory.active_complete);
    assert_eq!(inventory.jobs[0].job_id, "active-original-job");
    assert_eq!(
        inventory
            .jobs
            .iter()
            .filter(|snapshot| runner_job_is_terminal(&snapshot.status))
            .count(),
        JOB_INVENTORY_MAX_TERMINAL_JOBS
    );
    assert_eq!(
        inventory
            .jobs
            .iter()
            .filter(|snapshot| runner_job_is_active(&snapshot.status))
            .count(),
        1
    );
    assert!(!inventory
        .jobs
        .iter()
        .any(|snapshot| snapshot.job_id == expired_id));
    assert!(inventory
        .jobs
        .iter()
        .skip(1)
        .all(|snapshot| runner_job_is_terminal(&snapshot.status)));
}

#[test]
fn job_reconciliation_inventory_drops_terminal_payload_before_active_jobs() {
    let manager = JobManager::new(1);
    let now = chrono::Utc::now().timestamp();
    let mut active = test_job_snapshot("active-safe-metadata");
    active.context.command_preview = "safe preview".to_string();
    lock_unpoison(&manager.jobs).insert(
        active.job_id.clone(),
        RunningJob {
            client_id: "test-agent".to_string(),
            runner_instance_id: "test-instance".to_string(),
            snapshot: active,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );
    let tail = "x\n".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES / 2);
    for index in 0..JOB_INVENTORY_MAX_TERMINAL_JOBS {
        let job_id = format!("large-terminal-{index}");
        let mut job = retained_terminal_job(&job_id, now - index as i64);
        job.snapshot.stdout = ShellJobStreamSnapshot {
            tail: tail.clone(),
            first_retained_line: 1,
            next_line: 1 + tail.lines().count(),
            truncated: false,
        };
        job.snapshot.stderr = job.snapshot.stdout.clone();
        lock_unpoison(&manager.jobs).insert(job_id, job);
    }

    let inventory = manager.inventory();
    assert_eq!(inventory.jobs[0].job_id, "active-safe-metadata");
    assert!(
        inventory.jobs.len() < JOB_INVENTORY_MAX_TERMINAL_JOBS + 1,
        "terminal snapshots must yield before the active record"
    );
    let encoded = serde_json::to_vec(&inventory).unwrap();
    assert!(encoded.len() <= JOB_INVENTORY_MAX_SERIALIZED_BYTES);
    assert!(!String::from_utf8(encoded)
        .unwrap()
        .contains("super-secret-raw-command"));
}

#[test]
fn job_reconciliation_local_snapshot_advances_before_best_effort_send() {
    let manager = JobManager::new(1);
    let mut snapshot = test_job_snapshot("offline-terminal-job");
    snapshot.context.validation_steps = vec!["check".to_string()];
    lock_unpoison(&manager.jobs).insert(
        snapshot.job_id.clone(),
        RunningJob {
            client_id: "test-agent".to_string(),
            runner_instance_id: "test-instance".to_string(),
            snapshot,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    tx.try_send(RunnerEnvelope::Ping { ts: 17 }).unwrap();
    manager.install_sink(RunnerSink::WebSocket {
        tx,
        client_id: "test-agent".to_string(),
        runner_instance_id: "test-instance".to_string(),
    });
    manager.update_and_send(
        "offline-terminal-job",
        RunnerJobDelta {
            status: "running".to_string(),
            stdout_chunk: Some("one\n".to_string()),
            validation_progress: Some(ShellJobValidationProgress {
                completed: 0,
                current_step: Some("check".to_string()),
                failed_step: None,
            }),
            ..Default::default()
        },
    );
    let first = recv_job_update(&mut rx, Duration::from_secs(2), "incremental update");
    assert_eq!(first.update_seq, Some(2));
    assert!(first.stdout_chunk.is_none());
    let first_logs = first
        .log_snapshot
        .expect("sequenced update has authoritative logs");
    assert_eq!(first_logs.stdout.tail, "one\n");
    assert_eq!(first_logs.stdout.next_line, 2);

    drop(rx);
    manager.update_and_send(
        "offline-terminal-job",
        RunnerJobDelta {
            status: "completed".to_string(),
            stdout_chunk: Some("two\n".to_string()),
            exit_code: Some(0),
            duration_ms: Some(25),
            validation_progress: Some(ShellJobValidationProgress {
                completed: 1,
                current_step: None,
                failed_step: None,
            }),
            finished: true,
            ..Default::default()
        },
    );
    let inventory = manager.inventory();
    let retained = inventory
        .jobs
        .iter()
        .find(|snapshot| snapshot.job_id == "offline-terminal-job")
        .expect("terminal snapshot remains after transport send fails");
    assert_eq!(retained.status, "completed");
    assert_eq!(retained.update_seq, 3);
    assert_eq!(retained.stdout.tail, "one\ntwo\n");
    assert_eq!(retained.stdout.next_line, 3);
    assert_eq!(retained.exit_code, Some(0));
    assert_eq!(retained.duration_ms, Some(25));
    assert_eq!(
        retained.validation_progress,
        Some(ShellJobValidationProgress {
            completed: 1,
            current_step: None,
            failed_step: None,
        })
    );
    let record = lock_unpoison(&manager.jobs)
        .get("offline-terminal-job")
        .cloned()
        .unwrap();
    assert!(record.child.is_none());
    assert!(!record.slot_reserved);

    manager.update_and_send(
        "offline-terminal-job",
        RunnerJobDelta {
            status: "running".to_string(),
            stdout_chunk: Some("late\n".to_string()),
            ..Default::default()
        },
    );
    let immutable_terminal = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "offline-terminal-job")
        .unwrap();
    assert_eq!(immutable_terminal.status, "completed");
    assert_eq!(immutable_terminal.update_seq, 3);
    assert_eq!(immutable_terminal.stdout.tail, "one\ntwo\n");

    let mut registered_inventory = manager.inventory();
    let (reconnected_tx, mut reconnected_rx) = tokio::sync::mpsc::channel(4);
    manager.install_sink(RunnerSink::WebSocket {
        tx: reconnected_tx,
        client_id: "test-agent".to_string(),
        runner_instance_id: "test-instance".to_string(),
    });
    manager.replay_snapshots_since(&registered_inventory);
    assert!(wait_until(Duration::from_secs(2), || {
        !lock_unpoison(&manager.pending_job_updates).contains_key("offline-terminal-job")
    }));
    while reconnected_rx.try_recv().is_ok() {}

    let (fresh_tx, mut fresh_rx) = tokio::sync::mpsc::channel(4);
    manager.install_sink(RunnerSink::WebSocket {
        tx: fresh_tx,
        client_id: "test-agent".to_string(),
        runner_instance_id: "test-instance".to_string(),
    });
    manager.replay_snapshots_since(&registered_inventory);
    assert!(
        fresh_rx.try_recv().is_err(),
        "unchanged register snapshots need no network replay"
    );
    registered_inventory
        .jobs
        .iter_mut()
        .find(|snapshot| snapshot.job_id == "offline-terminal-job")
        .unwrap()
        .update_seq -= 1;
    manager.replay_snapshots_since(&registered_inventory);
    let replay = recv_job_update(
        &mut fresh_rx,
        Duration::from_secs(2),
        "post-register replay",
    );
    assert_eq!(replay.job_id, "offline-terminal-job");
    assert_eq!(replay.update_seq, Some(3));
    assert!(replay.finished);
    let logs = replay.log_snapshot.expect("authoritative replay logs");
    assert_eq!(logs.stdout.tail, "one\ntwo\n");
    assert_eq!(logs.stdout.next_line, 3);

    manager.stop("offline-terminal-job").unwrap();
    let stopped_race = recv_job_update(
        &mut fresh_rx,
        Duration::from_secs(2),
        "stop racing a lost terminal update replays the terminal snapshot",
    );
    assert_eq!(stopped_race.status, "completed");
    assert_eq!(stopped_race.update_seq, Some(3));
    assert!(stopped_race.finished);
}

#[test]
fn job_reconciliation_utf8_log_tail_is_bounded_with_absolute_cursor() {
    let emoji = "🙂".as_bytes();
    let mut decoder = OutputTextDecoder::new(OutputTextSource::LocalProcess);
    assert!(decoder.push(&emoji[..2], false).is_empty());
    assert_eq!(decoder.push(&emoji[2..], false), "🙂");
    assert!(decoder.push(&[], true).is_empty());

    let mut stream = ShellJobStreamSnapshot::default();
    let chunk = "🙂\n".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES / 2);
    append_runner_stream(&mut stream, Some(&chunk));
    assert!(stream.tail.len() <= JOB_SNAPSHOT_STREAM_MAX_BYTES);
    assert!(stream.truncated);
    assert!(stream.first_retained_line > 1);
    assert_eq!(
        stream.next_line,
        stream
            .first_retained_line
            .saturating_add(stream.tail.lines().count())
    );
    assert!(std::str::from_utf8(stream.tail.as_bytes()).is_ok());

    let mut long_partial = ShellJobStreamSnapshot::default();
    append_runner_stream(
        &mut long_partial,
        Some(&format!(
            "first\nsecond\n{}",
            "z".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES + 1)
        )),
    );
    assert_eq!(long_partial.first_retained_line, 3);
    assert_eq!(long_partial.next_line, 4);
    let observed_next = long_partial.next_line;
    trim_runner_stream_to(&mut long_partial, 0);
    assert!(long_partial.tail.is_empty());
    assert_eq!(long_partial.first_retained_line, observed_next);
    assert_eq!(long_partial.next_line, observed_next);
}

#[test]
fn validation_wait_failure_is_executor_owned_without_a_failed_check() {
    let error = std::io::Error::other("synthetic wait failure");
    let encoded = wait_failure_error(true, &error);
    assert_eq!(encoded, VALIDATION_STEP_WAIT_FAILED_CODE);
    assert_eq!(
        validation_failed_step("failed", Some(&encoded), "check"),
        None
    );

    let ordinary = wait_failure_error(false, &error);
    assert_eq!(ordinary, "failed to wait job: synthetic wait failure");
    assert_eq!(
        validation_failed_step("failed", Some("check exited non-zero"), "check"),
        Some("check".to_string())
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn detached_job_request(
    cwd: &std::path::Path,
    job_id: &str,
    scenario: &str,
    mut env: Vec<(String, String)>,
) -> DetachedStartRequest {
    env.push((
        "WEBCODEX_DETACHED_JOB_MANAGER_SCENARIO".to_string(),
        scenario.to_string(),
    ));
    DetachedStartRequest {
        job_id: job_id.to_string(),
        request_id: format!("request-{job_id}"),
        client_id: "detached-agent".to_string(),
        runner_instance_id: "old-runner-instance".to_string(),
        context: ShellJobContext {
            runtime_project_id: Some("agent:detached-agent:project".to_string()),
            workflow_session_id: None,
            ssh_resource: None,
            project_cwd: Some(cwd.to_string_lossy().into_owned()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            purpose: Some("test".to_string()),
            shell: Some("direct_argv".to_string()),
            command_preview: "detached test process".to_string(),
            validation_steps: Vec::new(),
            validation: None,
            structured_execution: Some(runner_protocol::ShellJobStructuredExecutionMetadata {
                execution_source: "run_process".to_string(),
                language: None,
                script_bytes: None,
                arg_count: 3,
                stdin_present: false,
                validation_identity: None,
                validation_tool: None,
                assertion_name: None,
            }),
        },
        launch: DetachedLaunchSpec {
            process: runner_protocol::ShellProcessArgv {
                executable: std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                args: vec![
                    "--exact".to_string(),
                    "job_manager_tests::detached_job_payload_subprocess_entrypoint".to_string(),
                    "--nocapture".to_string(),
                ],
            },
            cwd: Some(cwd.to_string_lossy().into_owned()),
            stdin: None,
            env,
            timeout_secs: 30,
        },
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn detached_job_payload_subprocess_entrypoint() {
    let Ok(scenario) = std::env::var("WEBCODEX_DETACHED_JOB_MANAGER_SCENARIO") else {
        return;
    };
    match scenario.as_str() {
        "terminal_output" => {
            let marker = PathBuf::from(std::env::var_os("MARKER").unwrap());
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(marker)
                .unwrap();
            writeln!(file, "run").unwrap();
            file.flush().unwrap();
            println!("before");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            println!("after");
        }
        "sleep" => {
            println!("ready");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
        "pid_sleep" => {
            let marker = PathBuf::from(std::env::var_os("PID_MARKER").unwrap());
            std::fs::write(marker, std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
        other => panic!("unknown detached JobManager payload scenario: {other}"),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn detached_recovery_uses_same_inventory_and_observes_terminal_output() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("runs");
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = detached_job_request(
        temp.path(),
        "detached-recovery-terminal",
        "terminal_output",
        vec![("MARKER".to_string(), marker.to_string_lossy().into_owned())],
    );
    let outcome = handoff_detached_job(&store, request.clone()).unwrap();
    assert!(matches!(
        outcome,
        crate::webcodex_runner::detached_job::DetachedHandoffOutcome::Accepted { .. }
    ));
    assert!(wait_until(Duration::from_secs(5), || store
        .read(&request.job_id)
        .is_ok_and(|record| record.phase == DetachedJobPhase::Running)));

    let manager = JobManager::new(1);
    assert_eq!(
        manager
            .recover_detached_jobs(store.clone(), "detached-agent", "new-runner-instance")
            .unwrap(),
        1
    );
    let recovered = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == request.job_id)
        .unwrap();
    assert_eq!(recovered.status, "running");
    let local = lock_unpoison(&manager.jobs)
        .get(&request.job_id)
        .cloned()
        .unwrap();
    assert_eq!(local.runner_instance_id, "new-runner-instance");
    assert!(local.child.is_none());
    assert!(lock_unpoison(&manager.detached_jobs).contains_key(&request.job_id));

    assert!(wait_until(Duration::from_secs(10), || manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == request.job_id)
        .is_some_and(|snapshot| snapshot.status == "completed")));
    let terminal = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == request.job_id)
        .unwrap();
    assert_eq!(terminal.status, "completed");
    assert!(terminal.stdout.tail.contains("before\n"));
    assert!(terminal.stdout.tail.contains("after\n"));
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert!(!lock_unpoison(&manager.detached_jobs).contains_key(&request.job_id));
    wait_for_job_workers(&manager);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn detached_recovery_runner_shutdown_preserves_supervisor_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = detached_job_request(
        temp.path(),
        "detached-recovery-shutdown",
        "sleep",
        Vec::new(),
    );
    let _ = handoff_detached_job(&store, request.clone()).unwrap();
    assert!(wait_until(Duration::from_secs(5), || store
        .read(&request.job_id)
        .is_ok_and(|record| record.phase == DetachedJobPhase::Running)));
    let manager = JobManager::new(1);
    manager
        .recover_detached_jobs(store.clone(), "detached-agent", "new-runner-instance")
        .unwrap();
    assert!(
        !manager.has_work(),
        "detached ownership must not block current Runner-owned work drain"
    );
    let supervisor_pid = store.read(&request.job_id).unwrap().supervisor.unwrap().pid;
    assert!(process_running(supervisor_pid));

    manager.stop_accepting_work();
    let batch = manager.signal_all_for_shutdown();
    assert_eq!(batch.running, 0);
    assert!(batch.targets.is_empty());
    assert!(!store.read(&request.job_id).unwrap().stop_requested);
    drop(manager);
    assert!(wait_until(Duration::from_secs(2), || process_running(
        supervisor_pid
    )));
    let still_running = store.read(&request.job_id).unwrap();
    assert!(!still_running.stop_requested);

    store
        .request_stop(&request.job_id, &still_running.execution_id)
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || store
        .read(&request.job_id)
        .is_ok_and(|record| record.phase == DetachedJobPhase::Terminal)));
    assert_eq!(
        store
            .read(&request.job_id)
            .unwrap()
            .terminal
            .unwrap()
            .status,
        "stopped"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn detached_recovery_stop_uses_durable_control_without_managed_child() {
    let temp = tempfile::tempdir().unwrap();
    let pid_marker = temp.path().join("payload.pid");
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = detached_job_request(
        temp.path(),
        "detached-recovery-stop",
        "pid_sleep",
        vec![(
            "PID_MARKER".to_string(),
            pid_marker.to_string_lossy().into_owned(),
        )],
    );
    let _ = handoff_detached_job(&store, request.clone()).unwrap();
    let payload_pid = wait_for_pid_marker(
        &pid_marker,
        Instant::now() + Duration::from_secs(5),
        "detached recovery payload",
    );
    assert!(process_running(payload_pid));

    let manager = JobManager::new(1);
    manager
        .recover_detached_jobs(store.clone(), "detached-agent", "new-runner-instance")
        .unwrap();
    assert!(lock_unpoison(&manager.jobs)
        .get(&request.job_id)
        .unwrap()
        .child
        .is_none());
    manager.stop(&request.job_id).unwrap();
    let terminal = store.read(&request.job_id).unwrap();
    assert_eq!(terminal.phase, DetachedJobPhase::Terminal);
    assert!(terminal.stop_requested);
    assert_eq!(terminal.terminal.unwrap().status, "stopped");
    assert!(wait_until(Duration::from_secs(5), || !process_running(
        payload_pid
    )));
    wait_for_job_workers(&manager);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn detached_recovery_observer_start_failure_retains_durable_control_and_stop_routing() {
    let temp = tempfile::tempdir().unwrap();
    let pid_marker = temp.path().join("payload.pid");
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = detached_job_request(
        temp.path(),
        "detached-observer-start-failure",
        "pid_sleep",
        vec![(
            "PID_MARKER".to_string(),
            pid_marker.to_string_lossy().into_owned(),
        )],
    );
    let _ = handoff_detached_job(&store, request.clone()).unwrap();
    let payload_pid = wait_for_pid_marker(
        &pid_marker,
        Instant::now() + Duration::from_secs(5),
        "detached observer start-failure payload",
    );
    assert!(process_running(payload_pid));

    let manager = JobManager::new(1);
    manager
        .fail_detached_observer_spawn
        .store(true, Ordering::SeqCst);
    assert_eq!(
        manager
            .recover_detached_jobs(store.clone(), "detached-agent", "replacement-instance")
            .unwrap(),
        1
    );
    assert!(lock_unpoison(&manager.detached_jobs).contains_key(&request.job_id));
    assert!(lock_unpoison(&manager.jobs).contains_key(&request.job_id));
    assert!(
        !manager.has_work(),
        "detached durable ownership must remain excluded from Runner work drain"
    );
    manager.stop_accepting_work();
    let shutdown = manager.signal_all_for_shutdown();
    assert_eq!(shutdown.running, 0);
    assert!(shutdown.targets.is_empty());
    assert!(!store.read(&request.job_id).unwrap().stop_requested);

    manager.stop(&request.job_id).unwrap();
    let terminal = store.read(&request.job_id).unwrap();
    assert!(terminal.stop_requested);
    assert_eq!(terminal.phase, DetachedJobPhase::Terminal);
    assert_eq!(terminal.terminal.unwrap().status, "stopped");
    assert!(wait_until(Duration::from_secs(5), || !process_running(
        payload_pid
    )));
    assert!(!lock_unpoison(&manager.detached_jobs).contains_key(&request.job_id));
    wait_for_job_workers(&manager);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn detached_recovery_observer_marks_later_supervisor_loss() {
    let temp = tempfile::tempdir().unwrap();
    let pid_marker = temp.path().join("payload.pid");
    let store = DetachedJobStore::new(temp.path().join("state"));
    let request = detached_job_request(
        temp.path(),
        "detached-recovery-supervisor-loss",
        "pid_sleep",
        vec![(
            "PID_MARKER".to_string(),
            pid_marker.to_string_lossy().into_owned(),
        )],
    );
    let _ = handoff_detached_job(&store, request.clone()).unwrap();
    let payload_pid = wait_for_pid_marker(
        &pid_marker,
        Instant::now() + Duration::from_secs(5),
        "detached supervisor-loss payload",
    );
    let supervisor_pid = store.read(&request.job_id).unwrap().supervisor.unwrap().pid;

    let manager = JobManager::new(1);
    manager
        .recover_detached_jobs(store.clone(), "detached-agent", "new-runner-instance")
        .unwrap();
    assert!(process_running(supervisor_pid));
    assert!(process_running(payload_pid));

    // Test-only fault: kill the exact PID just read from the durable record.
    // Production recovery never signals by PID; it only observes native start
    // identity plus the lifetime lock and persists supervisor_lost.
    unsafe {
        libc::kill(supervisor_pid as i32, libc::SIGKILL);
    }
    assert!(wait_until(Duration::from_secs(5), || !process_running(
        supervisor_pid
    )));
    assert!(wait_until(Duration::from_secs(5), || !process_running(
        payload_pid
    )));
    assert!(wait_until(Duration::from_secs(5), || manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == request.job_id)
        .is_some_and(|snapshot| {
            snapshot.status == "lost"
                && snapshot.command_execution_state
                    == Some(ShellCommandExecutionState::OutcomeUnknown)
        })));
    assert_eq!(
        store
            .read(&request.job_id)
            .unwrap()
            .terminal
            .unwrap()
            .status,
        "supervisor_lost"
    );
    wait_for_job_workers(&manager);
}

#[cfg(target_os = "linux")]
#[test]
fn job_manager_stop_terminates_the_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = configured_shell_job_command(
        &ShellConfig::default(),
        "sleep 60 & echo $! > descendant.pid; wait",
    )
    .unwrap();
    command
        .current_dir(temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = Arc::new(Mutex::new(ManagedChild::spawn(&mut command).unwrap()));
    let leader_pid = child.lock().unwrap().id();
    let pid_file = temp.path().join("descendant.pid");
    let descendant_pid = wait_for_pid_marker(
        &pid_file,
        Instant::now() + Duration::from_secs(5),
        "process-group descendant",
    );
    assert!(process_running(leader_pid));
    assert!(process_running(descendant_pid));

    let manager = JobManager::new(1);
    let stop_requested = Arc::new(AtomicBool::new(false));
    manager.jobs.lock().unwrap().insert(
        "process-group-job".into(),
        RunningJob {
            client_id: "test-agent".into(),
            runner_instance_id: "test-instance".into(),
            snapshot: test_job_snapshot("process-group-job"),
            child: Some(child.clone()),
            stop_requested: stop_requested.clone(),
            slot_reserved: true,
        },
    );
    manager.stop("process-group-job").unwrap();
    assert!(stop_requested.load(Ordering::SeqCst));

    assert!(
        wait_until(Duration::from_secs(5), || {
            child.lock().unwrap().try_wait().unwrap().is_some()
                && !process_running(descendant_pid)
        }),
        "process-group cancellation did not terminate leader {leader_pid} and descendant {descendant_pid} within the deadline"
    );
    assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    assert!(
        !process_running(descendant_pid),
        "descendant {descendant_pid} survived process-group cancellation"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn job_shutdown_reaps_a_sigterm_responsive_child() {
    let temp = tempfile::tempdir().unwrap();
    let ready = temp.path().join("ready");
    let mut command = configured_shell_job_command(
        &ShellConfig::default(),
        "trap 'exit 0' TERM; : > ready; while :; do sleep 1; done",
    )
    .unwrap();
    command
        .current_dir(temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = Arc::new(Mutex::new(ManagedChild::spawn(&mut command).unwrap()));
    let leader_pid = child.lock().unwrap().id();
    assert!(wait_until(Duration::from_secs(5), || ready.exists()));
    let manager = JobManager::new(1);
    let stop_requested = Arc::new(AtomicBool::new(false));
    lock_unpoison(&manager.jobs).insert(
        "term-responsive".into(),
        RunningJob {
            client_id: "test-agent".into(),
            runner_instance_id: "test-instance".into(),
            snapshot: test_job_snapshot("term-responsive"),
            child: Some(Arc::clone(&child)),
            stop_requested: Arc::clone(&stop_requested),
            slot_reserved: true,
        },
    );

    manager.stop_accepting_work();
    let batch = manager.signal_all_for_shutdown();
    let outcome = manager.drain_shutdown(batch, Instant::now() + Duration::from_millis(800));
    assert_eq!(outcome.resources, 1);
    assert_eq!(outcome.timed_out, 0);
    assert!(stop_requested.load(Ordering::SeqCst));
    assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    assert!(!process_running(leader_pid));
}

#[cfg(target_os = "linux")]
#[test]
fn job_shutdown_escalates_ignored_sigterm_for_parent_and_descendant() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = configured_shell_job_command(
        &ShellConfig::default(),
        "trap '' TERM; sleep 60 & echo $! > descendant.pid; wait",
    )
    .unwrap();
    command
        .current_dir(temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = Arc::new(Mutex::new(ManagedChild::spawn(&mut command).unwrap()));
    let leader_pid = child.lock().unwrap().id();
    let pid_file = temp.path().join("descendant.pid");
    let descendant_pid = wait_for_pid_marker(
        &pid_file,
        Instant::now() + Duration::from_secs(5),
        "SIGTERM-ignoring descendant",
    );
    assert!(process_running(leader_pid));
    assert!(process_running(descendant_pid));

    let manager = JobManager::new(1);
    let stop_requested = Arc::new(AtomicBool::new(false));
    lock_unpoison(&manager.jobs).insert(
        "term-ignoring".into(),
        RunningJob {
            client_id: "test-agent".into(),
            runner_instance_id: "test-instance".into(),
            snapshot: test_job_snapshot("term-ignoring"),
            child: Some(Arc::clone(&child)),
            stop_requested: Arc::clone(&stop_requested),
            slot_reserved: true,
        },
    );
    let started = Instant::now();
    manager.stop_accepting_work();
    let batch = manager.signal_all_for_shutdown();
    let outcome = manager.drain_shutdown(batch, Instant::now() + Duration::from_millis(900));
    let elapsed = started.elapsed();

    assert_eq!(outcome.resources, 1);
    assert_eq!(outcome.timed_out, 0);
    assert!(stop_requested.load(Ordering::SeqCst));
    assert!(
        elapsed < Duration::from_millis(1100),
        "job shutdown exceeded its absolute deadline: {elapsed:?}"
    );
    assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    assert!(!process_running(leader_pid));
    assert!(
        wait_until(Duration::from_secs(1), || !process_running(descendant_pid)),
        "descendant survived process-group SIGKILL"
    );
}

#[test]
fn poisoned_job_mutex_does_not_panic_shutdown() {
    let manager = JobManager::new(1);
    let jobs = Arc::clone(&manager.jobs);
    let poisoned = std::thread::spawn(move || {
        let _guard = jobs.lock().unwrap();
        panic!("poison jobs mutex");
    });
    assert!(poisoned.join().is_err());

    manager.stop_accepting_work();
    assert_eq!(manager.cancel_queued_for_shutdown(), 0);
    let batch = manager.signal_all_for_shutdown();
    let outcome = manager.drain_shutdown(batch, Instant::now() + Duration::from_millis(50));
    assert_eq!(outcome.resources, 0);
    assert_eq!(outcome.timed_out, 0);
}

/// One run of the fail-fast plan, plus the side effect the plan must not have.
#[cfg(unix)]
struct FailFastAttempt {
    updates: Vec<RunnerJobUpdateRequest>,
    test_step_ran: bool,
}

fn recv_envelope_until(
    runtime: &tokio::runtime::Runtime,
    rx: &mut tokio::sync::mpsc::Receiver<RunnerEnvelope>,
    deadline: Instant,
) -> Result<Option<RunnerEnvelope>, tokio::time::error::Elapsed> {
    runtime.block_on(async {
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), rx.recv()).await
    })
}

/// Drain ordered JobUpdates until one reports `finished`, the channel closes,
/// or one absolute wall-clock deadline expires. Unrelated envelopes are ignored.
fn collect_job_updates(
    rx: &mut tokio::sync::mpsc::Receiver<RunnerEnvelope>,
    timeout: Duration,
) -> Vec<RunnerJobUpdateRequest> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("JobUpdate collection runtime");
    let deadline = Instant::now() + timeout;
    let mut updates: Vec<RunnerJobUpdateRequest> = Vec::new();
    loop {
        if updates.last().is_some_and(|update| update.finished) {
            break;
        }
        match recv_envelope_until(&runtime, rx, deadline) {
            Ok(Some(RunnerEnvelope::JobUpdate { payload })) => updates.push(payload),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    updates
}

fn recv_job_update(
    rx: &mut tokio::sync::mpsc::Receiver<RunnerEnvelope>,
    timeout: Duration,
    label: &str,
) -> RunnerJobUpdateRequest {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("JobUpdate receive runtime");
    let deadline = Instant::now() + timeout;
    loop {
        match recv_envelope_until(&runtime, rx, deadline) {
            Ok(Some(RunnerEnvelope::JobUpdate { payload })) => return payload,
            Ok(Some(_)) => {}
            Ok(None) => panic!("channel closed while waiting for {label}"),
            Err(_) => panic!("timed out waiting for {label}"),
        }
    }
}

/// Standalone native-argv helper used by the typed Job tests. The fixture can
/// append a PID/nonce marker before sleeping, so any cancel-and-restart
/// promotion would leave two durable start lines.
struct StructuredProcessHelper {
    _temp: TempDir,
    path: PathBuf,
}

static STRUCTURED_PROCESS_HELPER: OnceLock<Arc<StructuredProcessHelper>> = OnceLock::new();

fn structured_process_helper() -> Arc<StructuredProcessHelper> {
    STRUCTURED_PROCESS_HELPER
        .get_or_init(|| {
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/process_argv_helper.rs");
            let temp = tempfile::tempdir().unwrap();
            let output = temp.path().join(format!(
                "structured-process-helper{}",
                std::env::consts::EXE_SUFFIX
            ));
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
            let result = Command::new(rustc)
                .arg("--edition=2021")
                .arg("--crate-name=webcodex_structured_process_helper")
                .arg(source)
                .arg("-o")
                .arg(&output)
                .output()
                .expect("run rustc for structured process helper");
            assert!(
                result.status.success(),
                "structured process helper compilation failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            Arc::new(StructuredProcessHelper {
                _temp: temp,
                path: output,
            })
        })
        .clone()
}

fn structured_process_context(
    cwd: &Path,
    arg_count: usize,
    stdin_present: bool,
) -> ShellJobContext {
    let mut context = test_job_context(cwd, Vec::new());
    context.shell = Some("direct_argv".to_string());
    context.command_preview = "structured process test".to_string();
    context.structured_execution = Some(runner_protocol::ShellJobStructuredExecutionMetadata {
        execution_source: "run_process".to_string(),
        language: None,
        script_bytes: None,
        arg_count,
        stdin_present,
        validation_identity: None,
        validation_tool: None,
        assertion_name: None,
    });
    context
}

fn detached_process_context(cwd: &Path, arg_count: usize, stdin_present: bool) -> ShellJobContext {
    let mut context = structured_process_context(cwd, arg_count, stdin_present);
    context.command_preview = format!("detached process ({arg_count} args)");
    context
        .structured_execution
        .as_mut()
        .expect("structured detached metadata")
        .execution_source = "run_detached_process".to_string();
    context
}

#[cfg(unix)]
fn structured_script_context(
    cwd: &Path,
    language: runner_protocol::ShellScriptLanguage,
    script_bytes: usize,
    arg_count: usize,
    stdin_present: bool,
) -> ShellJobContext {
    let mut context = test_job_context(cwd, Vec::new());
    context.shell = Some(language.as_str().to_string());
    context.command_preview = format!(
        "{} script ({script_bytes} bytes, {arg_count} args)",
        language.as_str()
    );
    context.structured_execution = Some(runner_protocol::ShellJobStructuredExecutionMetadata {
        execution_source: "run_script".to_string(),
        language: Some(language),
        script_bytes: Some(script_bytes),
        arg_count,
        stdin_present,
        validation_identity: None,
        validation_tool: None,
        assertion_name: None,
    });
    context
}

fn structured_test_sink(
    client_id: &str,
    instance_id: &str,
) -> (RunnerSink, tokio::sync::mpsc::Receiver<RunnerEnvelope>) {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    (
        RunnerSink::WebSocket {
            tx,
            client_id: client_id.to_string(),
            runner_instance_id: instance_id.to_string(),
        },
        rx,
    )
}

fn enqueue_structured_process_job(
    manager: &JobManager,
    sink: RunnerSink,
    cwd: &Path,
    job_id: &str,
    executable: &Path,
    args: Vec<String>,
    stdin: Option<String>,
    timeout_secs: u64,
    sandbox: Option<&str>,
) {
    enqueue_structured_process_job_with_policy(
        manager,
        sink,
        cwd,
        job_id,
        executable,
        args,
        stdin,
        timeout_secs,
        sandbox,
        RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn enqueue_structured_process_job_with_policy(
    manager: &JobManager,
    sink: RunnerSink,
    cwd: &Path,
    job_id: &str,
    executable: &Path,
    args: Vec<String>,
    stdin: Option<String>,
    timeout_secs: u64,
    sandbox: Option<&str>,
    policy: RunnerPolicy,
) {
    let context = structured_process_context(cwd, args.len(), stdin.is_some());
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            policy,
            ShellConfig::default(),
            SshConfig::default(),
            cwd.join("project-registry"),
            serde_json::from_value(json!({
                "request_id": format!("request-{job_id}"),
                "client_id": "structured-agent",
                "kind": "start_process_job",
                "job_id": job_id,
                "cwd": cwd,
                "command": "",
                "process": {
                    "executable": executable,
                    "args": args,
                },
                "stdin": stdin,
                "timeout_secs": timeout_secs,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "sandbox": sandbox,
                "job_context": context,
            }))
            .unwrap(),
        ),
    );
}

fn enqueue_detached_process_job(
    manager: &JobManager,
    sink: RunnerSink,
    cwd: &Path,
    job_id: &str,
    executable: &Path,
    args: Vec<String>,
    stdin: Option<String>,
    timeout_secs: u64,
) {
    let context = detached_process_context(cwd, args.len(), stdin.is_some());
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            ShellConfig::default(),
            SshConfig::default(),
            cwd.join("project-registry"),
            serde_json::from_value(json!({
                "request_id": format!("request-{job_id}"),
                "client_id": "structured-agent",
                "kind": "start_detached_process_job",
                "job_id": job_id,
                "cwd": cwd,
                "command": "",
                "process": {
                    "executable": executable,
                    "args": args,
                },
                "stdin": stdin,
                "timeout_secs": timeout_secs,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": context,
            }))
            .unwrap(),
        ),
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn detached_process_jobmanager_initiation_handoffs_once_and_stops_via_durable_control() {
    let temp = tempfile::tempdir().unwrap();
    let state_root = temp.path().join("detached-state");
    let store = DetachedJobStore::new(state_root.clone());
    let helper = structured_process_helper();
    let marker = temp.path().join("starts.log");
    let job_id = "detached-jobmanager-initiation";
    let nonce = "phase3-init-once";
    let args = vec![
        "mark-sleep".to_string(),
        marker.to_string_lossy().into_owned(),
        nonce.to_string(),
        "30000".to_string(),
    ];
    let manager = JobManager::new(1);
    *lock_unpoison(&manager.detached_store_root_override) = Some(state_root);
    let (sink, _rx) = structured_test_sink("structured-agent", "inst");

    enqueue_detached_process_job(
        &manager,
        sink,
        temp.path(),
        job_id,
        &helper.path,
        args,
        None,
        60,
    );
    assert!(wait_until(Duration::from_secs(5), || marker.exists()));
    assert!(wait_until(Duration::from_secs(5), || store
        .read(job_id)
        .is_ok_and(|record| record.phase == DetachedJobPhase::Running)));
    let running = store.read(job_id).unwrap();
    assert_eq!(running.context.command_preview, "detached process (4 args)");
    assert_eq!(
        running
            .context
            .structured_execution
            .as_ref()
            .unwrap()
            .execution_source,
        "run_detached_process"
    );
    assert!(wait_until(Duration::from_secs(5), || lock_unpoison(
        &manager.detached_jobs
    )
    .get(job_id)
    .is_some_and(
        |detached| detached.execution_id == running.execution_id
    )));
    let starts = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(starts.lines().count(), 1);
    assert!(starts.contains(nonce));
    let payload_pid: u32 = starts
        .lines()
        .next()
        .unwrap()
        .split(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(process_running(payload_pid));

    manager.stop(job_id).unwrap();
    let terminal = store.read(job_id).unwrap();
    assert_eq!(terminal.phase, DetachedJobPhase::Terminal);
    assert!(terminal.stop_requested);
    assert_eq!(terminal.terminal.unwrap().status, "stopped");
    assert!(wait_until(Duration::from_secs(5), || !process_running(
        payload_pid
    )));
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    assert!(!lock_unpoison(&manager.detached_jobs).contains_key(job_id));
    wait_for_job_workers(&manager);
}

#[cfg(unix)]
fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn enqueue_shell_job(
    manager: &JobManager,
    sink: &RunnerSink,
    cwd: &Path,
    job_id: &str,
    command: String,
    timeout_secs: u64,
) {
    manager.enqueue(
        sink.clone(),
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            ShellConfig::default(),
            SshConfig::default(),
            cwd.join("project-registry"),
            serde_json::from_value(json!({
                "request_id": format!("request-{job_id}"),
                "client_id": "backpressure-agent",
                "kind": "start_job",
                "job_id": job_id,
                "cwd": cwd,
                "command": command,
                "timeout_secs": timeout_secs,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": test_job_context(cwd, Vec::new()),
            }))
            .unwrap(),
        ),
    );
}

struct GatedStructuredJob {
    job_id: String,
    started: PathBuf,
    active: PathBuf,
    release: PathBuf,
}

impl GatedStructuredJob {
    fn new(root: &Path, job_id: &str) -> Self {
        Self {
            job_id: job_id.to_string(),
            started: root.join(format!("{job_id}.started")),
            active: root.join(format!("{job_id}.active")),
            release: root.join(format!("{job_id}.release")),
        }
    }

    fn args(&self) -> Vec<String> {
        vec![
            "gate".to_string(),
            self.started.to_string_lossy().into_owned(),
            self.active.to_string_lossy().into_owned(),
            self.release.to_string_lossy().into_owned(),
            self.job_id.clone(),
        ]
    }

    fn release(&self) {
        std::fs::write(&self.release, "release\n").unwrap();
    }
}

fn enqueue_gated_structured_job(
    manager: &JobManager,
    sink: &RunnerSink,
    cwd: &Path,
    helper: &StructuredProcessHelper,
    job: &GatedStructuredJob,
) {
    enqueue_structured_process_job(
        manager,
        sink.clone(),
        cwd,
        &job.job_id,
        &helper.path,
        job.args(),
        None,
        20,
        None,
    );
}

fn enqueue_gated_structured_job_for_project(
    manager: &JobManager,
    sink: &RunnerSink,
    cwd: &Path,
    helper: &StructuredProcessHelper,
    job: &GatedStructuredJob,
    runtime_project_id: &str,
) {
    let mut context = structured_process_context(cwd, job.args().len(), false);
    context.runtime_project_id = Some(runtime_project_id.to_string());
    manager.enqueue(
        sink.clone(),
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            ShellConfig::default(),
            SshConfig::default(),
            cwd.join("project-registry"),
            serde_json::from_value(json!({
                "request_id": format!("request-{}", job.job_id),
                "client_id": "structured-agent",
                "kind": "start_process_job",
                "job_id": &job.job_id,
                "cwd": cwd,
                "command": "",
                "process": {
                    "executable": &helper.path,
                    "args": job.args(),
                },
                "timeout_secs": 20,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": context,
            }))
            .unwrap(),
        ),
    );
}

fn active_gated_children(jobs: &[GatedStructuredJob]) -> usize {
    jobs.iter().filter(|job| job.active.exists()).count()
}

fn wait_for_all_started(jobs: &[GatedStructuredJob]) {
    assert!(
        wait_until(Duration::from_secs(10), || jobs
            .iter()
            .all(|job| job.started.exists())),
        "gated children did not all start: {:?}",
        jobs.iter()
            .map(|job| (&job.job_id, job.started.exists()))
            .collect::<Vec<_>>()
    );
}

fn wait_for_job_workers(manager: &JobManager) {
    assert!(
        manager.wait_for_workers(Instant::now() + Duration::from_secs(10)),
        "Job workers did not finish"
    );
}

fn assert_gated_job_started_once(job: &GatedStructuredJob) {
    let starts = std::fs::read_to_string(&job.started).unwrap();
    assert_eq!(
        starts.lines().count(),
        1,
        "{} started more than once",
        job.job_id
    );
    assert!(starts.contains(&job.job_id));
}

#[test]
fn phase_e2_default_four_gates_fifth_and_promotes_same_job_once() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let (sink, _rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(DEFAULT_MAX_CONCURRENT_JOBS);
    let jobs = (1..=5)
        .map(|index| GatedStructuredJob::new(temp.path(), &format!("default-{index}")))
        .collect::<Vec<_>>();

    for job in &jobs {
        enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, job);
    }
    wait_for_all_started(&jobs[..4]);
    // The started marker is written just before the active marker, so the
    // child can still be in that window when wait_for_all_started returns;
    // poll for the active set instead of asserting it synchronously.
    assert!(
        wait_until(Duration::from_secs(10), || active_gated_children(&jobs)
            == 4),
        "simultaneously started children failed to reach the default of 4"
    );
    assert!(
        !jobs[4].started.exists(),
        "the fifth child started before a Job slot opened"
    );
    let queued = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == jobs[4].job_id)
        .expect("original fifth Job remains queryable");
    assert_eq!(queued.status, "agent_queued");
    assert_eq!(queued.request_id, "request-default-5");

    jobs[0].release();
    assert!(
        wait_until(Duration::from_secs(10), || jobs[4].active.exists()),
        "the original fifth Job was not promoted"
    );
    assert!(!jobs[0].active.exists());
    assert_eq!(
        active_gated_children(&jobs),
        4,
        "simultaneously started children exceeded or failed to reach the default"
    );
    assert!(
        wait_until(Duration::from_secs(10), || manager
            .inventory()
            .jobs
            .iter()
            .any(
                |snapshot| snapshot.job_id == jobs[4].job_id && snapshot.status == "running"
            )),
        "promoted fifth child became active before its Runner running state was published"
    );
    let promoted = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == jobs[4].job_id)
        .expect("promoted fifth Job retains its record");
    assert_eq!(promoted.status, "running");
    assert_eq!(promoted.request_id, "request-default-5");
    assert_eq!(promoted.activity, Some(process_running_activity()));

    for job in &jobs[1..] {
        job.release();
    }
    wait_for_job_workers(&manager);
    for job in &jobs {
        assert_gated_job_started_once(job);
        let snapshot = manager
            .inventory()
            .jobs
            .into_iter()
            .find(|snapshot| snapshot.job_id == job.job_id)
            .unwrap();
        assert_eq!(snapshot.status, "completed");
        assert_eq!(snapshot.request_id, format!("request-{}", job.job_id));
        assert!(snapshot.activity.is_none());
    }
}

#[test]
fn runner_job_slots_are_shared_across_runtime_projects() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let (sink, _rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let project_a = GatedStructuredJob::new(temp.path(), "project-a-running");
    let project_b = GatedStructuredJob::new(temp.path(), "project-b-queued");

    enqueue_gated_structured_job_for_project(
        &manager,
        &sink,
        temp.path(),
        &helper,
        &project_a,
        "agent:structured-agent:project-a",
    );
    enqueue_gated_structured_job_for_project(
        &manager,
        &sink,
        temp.path(),
        &helper,
        &project_b,
        "agent:structured-agent:project-b",
    );
    wait_for_all_started(std::slice::from_ref(&project_a));
    assert!(
        wait_until(Duration::from_secs(10), || project_a.active.exists()),
        "project A Job never became active"
    );
    assert!(!project_b.started.exists());
    assert!(
        wait_until(Duration::from_secs(10), || manager
            .inventory()
            .jobs
            .iter()
            .any(
                |snapshot| snapshot.job_id == project_a.job_id && snapshot.status == "running"
            )),
        "project A child became active before its Runner running state was published"
    );

    let inventory = manager.inventory();
    assert!(inventory.active_complete);
    let running = inventory
        .jobs
        .iter()
        .find(|snapshot| snapshot.job_id == project_a.job_id)
        .unwrap();
    assert_eq!(running.status, "running");
    assert_eq!(
        running.context.runtime_project_id.as_deref(),
        Some("agent:structured-agent:project-a")
    );
    let queued = inventory
        .jobs
        .iter()
        .find(|snapshot| snapshot.job_id == project_b.job_id)
        .unwrap();
    assert_eq!(queued.status, "agent_queued");
    assert_eq!(queued.job_id, project_b.job_id);
    assert_eq!(
        queued.context.runtime_project_id.as_deref(),
        Some("agent:structured-agent:project-b")
    );

    project_a.release();
    assert!(
        wait_until(Duration::from_secs(10), || project_b.active.exists()),
        "project B queued Job did not claim the shared Runner slot"
    );
    assert!(
        wait_until(Duration::from_secs(10), || manager
            .inventory()
            .jobs
            .iter()
            .any(
                |snapshot| snapshot.job_id == project_b.job_id && snapshot.status == "running"
            )),
        "project B child became active before its Runner running state was published"
    );
    let promoted = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == project_b.job_id)
        .unwrap();
    assert_eq!(promoted.status, "running");
    assert_eq!(promoted.job_id, project_b.job_id);
    assert_eq!(
        promoted.context.runtime_project_id.as_deref(),
        Some("agent:structured-agent:project-b")
    );

    project_b.release();
    wait_for_job_workers(&manager);
    assert_gated_job_started_once(&project_a);
    assert_gated_job_started_once(&project_b);
}

#[test]
fn phase_e2_explicit_limit_one_serializes_jobs_strictly() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let (sink, _rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let first = GatedStructuredJob::new(temp.path(), "serial-a");
    let second = GatedStructuredJob::new(temp.path(), "serial-b");

    enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, &first);
    enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, &second);
    wait_for_all_started(std::slice::from_ref(&first));
    assert!(
        wait_until(Duration::from_secs(10), || first.active.exists()),
        "the first Job never became active"
    );
    assert!(!second.active.exists());
    assert!(!second.started.exists());

    first.release();
    wait_for_all_started(std::slice::from_ref(&second));
    assert!(!first.active.exists());
    // The started marker is written just before the active marker, so the
    // child can still be in that window when wait_for_all_started returns;
    // poll until the second Job is actually active.
    assert!(
        wait_until(Duration::from_secs(10), || second.active.exists()),
        "limit=1 did not serialize execution: the second Job never became active"
    );
    second.release();
    wait_for_job_workers(&manager);
    assert_gated_job_started_once(&first);
    assert_gated_job_started_once(&second);
}

#[test]
fn phase_e2_explicit_higher_limit_starts_all_eight_children() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let (sink, _rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(8);
    let jobs = (1..=8)
        .map(|index| GatedStructuredJob::new(temp.path(), &format!("higher-{index}")))
        .collect::<Vec<_>>();

    for job in &jobs {
        enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, job);
    }
    wait_for_all_started(&jobs);
    // The started marker is written just before the active marker, so the
    // child can still be in that window when wait_for_all_started returns;
    // poll for the active set instead of asserting it synchronously.
    assert!(
        wait_until(Duration::from_secs(10), || active_gated_children(&jobs)
            == 8),
        "explicit limit=8 was capped at a smaller default"
    );
    for job in &jobs {
        job.release();
    }
    wait_for_job_workers(&manager);
    for job in &jobs {
        assert_gated_job_started_once(job);
    }
}

#[test]
fn phase_e2_single_client_queue_promotes_fifo() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let (sink, _rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let first = GatedStructuredJob::new(temp.path(), "fifo-a");
    let second = GatedStructuredJob::new(temp.path(), "fifo-b");
    let third = GatedStructuredJob::new(temp.path(), "fifo-c");

    for job in [&first, &second, &third] {
        enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, job);
    }
    wait_for_all_started(std::slice::from_ref(&first));
    assert!(!second.started.exists());
    assert!(!third.started.exists());

    first.release();
    wait_for_all_started(std::slice::from_ref(&second));
    assert!(!third.started.exists(), "C started before queued B");
    second.release();
    wait_for_all_started(std::slice::from_ref(&third));
    third.release();
    wait_for_job_workers(&manager);
    for job in [&first, &second, &third] {
        assert_gated_job_started_once(job);
    }
}

#[test]
fn phase_e2_prestart_structured_failure_releases_slot_for_queued_job() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let (sink, _rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    manager.install_sink(sink.clone());

    let failed_job_id = "prestart-failure";
    let mut failed_snapshot = test_job_snapshot(failed_job_id);
    failed_snapshot.status = "agent_queued".to_string();
    failed_snapshot.started_at = None;
    failed_snapshot.update_seq = 1;
    failed_snapshot.context = structured_process_context(temp.path(), 0, false);
    lock_unpoison(&manager.jobs).insert(
        failed_job_id.to_string(),
        RunningJob {
            client_id: "structured-agent".to_string(),
            runner_instance_id: "structured-instance".to_string(),
            snapshot: failed_snapshot,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );

    let queued = GatedStructuredJob::new(temp.path(), "after-prestart-failure");
    enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, &queued);
    assert!(!queued.started.exists());
    let failed_request: RunnerRequest = serde_json::from_value(json!({
        "request_id": "request-prestart-failure",
        "client_id": "structured-agent",
        "kind": "start_process_job",
        "job_id": failed_job_id,
        "cwd": temp.path(),
        "command": "",
        "process": {
            "executable": temp.path().join("missing-prestart-executable"),
            "args": [],
        },
        "timeout_secs": 20,
        "requested_by": "test",
        "created_at": chrono::Utc::now().timestamp(),
        "job_context": structured_process_context(temp.path(), 0, false),
    }))
    .unwrap();
    manager.start_structured_job(PendingJobStart::from_wire(
        1,
        RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        },
        ShellConfig::default(),
        SshConfig::default(),
        temp.path().join("project-registry"),
        failed_request,
    ));

    assert!(
        wait_until(Duration::from_secs(10), || queued.started.exists()),
        "queued Job stayed blocked after the reserved pre-start slot failed"
    );
    let failed = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == failed_job_id)
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(
        failed.command_execution_state,
        Some(ShellCommandExecutionState::NotStarted)
    );
    assert!(
        !lock_unpoison(&manager.jobs)
            .get(failed_job_id)
            .unwrap()
            .slot_reserved
    );

    queued.release();
    wait_for_job_workers(&manager);
    assert_gated_job_started_once(&queued);
}

#[test]
fn phase_e2_stopped_queued_job_never_executes_after_slot_release() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let (sink, _rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let first = GatedStructuredJob::new(temp.path(), "queued-stop-a");
    let stopped = GatedStructuredJob::new(temp.path(), "queued-stop-b");

    enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, &first);
    enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, &stopped);
    wait_for_all_started(std::slice::from_ref(&first));
    assert!(!stopped.started.exists());
    manager.stop(&stopped.job_id).unwrap();
    let stopped_snapshot = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == stopped.job_id)
        .unwrap();
    assert_eq!(stopped_snapshot.status, "stopped");
    assert_eq!(
        stopped_snapshot.command_execution_state,
        Some(ShellCommandExecutionState::NotStarted)
    );

    first.release();
    wait_for_job_workers(&manager);
    assert!(
        !stopped.started.exists(),
        "stopped queued Job spawned after a slot opened"
    );
    assert!(!stopped.active.exists());
    assert!(lock_unpoison(&manager.queued)
        .iter()
        .all(|entry| entry.operation.job_id() != stopped.job_id));
}

#[cfg(unix)]
#[test]
fn phase_e2_validation_job_shares_the_same_job_manager_slot_limit() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let (sink, _rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let blocker = GatedStructuredJob::new(temp.path(), "validation-slot-blocker");
    enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, &blocker);
    wait_for_all_started(std::slice::from_ref(&blocker));

    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let cargo = bin.join("cargo");
    let validation_marker = temp.path().join("validation.started");
    std::fs::write(
        &cargo,
        format!(
            "#!/bin/sh\nprintf '%s\\n' validation > \"{}\"\n",
            validation_marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o700)).unwrap();
    let steps = vec![ShellJobValidationStep {
        name: "check".to_string(),
        program: "cargo".to_string(),
        args: vec!["check".to_string(), "--all-targets".to_string()],
        env: Vec::new(),
    }];
    let mut shell = ShellConfig::default();
    shell.path_prepend.push(bin);
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            shell,
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": "request-validation-shared-slot",
                "client_id": "structured-agent",
                "kind": "start_validation_job",
                "job_id": "validation-shared-slot",
                "cwd": temp.path(),
                "command": serde_json::to_string(&steps).unwrap(),
                "timeout_secs": 20,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": test_job_context(temp.path(), vec!["check".to_string()]),
            }))
            .unwrap(),
        ),
    );
    assert!(!validation_marker.exists());
    assert_eq!(
        manager
            .inventory()
            .jobs
            .iter()
            .find(|snapshot| snapshot.job_id == "validation-shared-slot")
            .unwrap()
            .status,
        "agent_queued"
    );

    blocker.release();
    assert!(
        wait_until(Duration::from_secs(10), || validation_marker.exists()),
        "validation Job did not start after the shared slot opened"
    );
    wait_for_job_workers(&manager);
    let validation = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "validation-shared-slot")
        .unwrap();
    assert_eq!(validation.status, "completed");
    assert_eq!(
        validation.validation_progress,
        Some(ShellJobValidationProgress {
            completed: 1,
            current_step: None,
            failed_step: None,
        })
    );
}

#[test]
fn structured_process_job_executes_exactly_once_and_reconciles_the_same_job() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let gated = GatedStructuredJob::new(temp.path(), "structured-once");
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    enqueue_gated_structured_job(&manager, &sink, temp.path(), &helper, &gated);

    assert!(wait_until(Duration::from_secs(30), || gated
        .active
        .exists()));
    assert!(
        wait_until(Duration::from_secs(30), || manager
            .inventory()
            .jobs
            .iter()
            .any(
                |snapshot| snapshot.job_id == "structured-once" && snapshot.status == "running"
            )),
        "started child did not become running in reconciliation inventory"
    );
    let active = manager.inventory();
    let active = active
        .jobs
        .iter()
        .find(|snapshot| snapshot.job_id == "structured-once")
        .expect("same active Job is in reconciliation inventory");
    assert_eq!(active.status, "running");
    assert_eq!(active.command_execution_state, None);
    assert_eq!(
        active
            .context
            .structured_execution
            .as_ref()
            .unwrap()
            .execution_source,
        "run_process"
    );

    gated.release();
    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    let final_update = updates.last().expect("structured process terminal update");
    assert!(final_update.finished, "{final_update:?}");
    assert_eq!(final_update.status, "completed", "{final_update:?}");
    assert_eq!(final_update.exit_code, Some(0), "{final_update:?}");
    assert_eq!(
        final_update.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert!(final_update
        .log_snapshot
        .as_ref()
        .unwrap()
        .stdout
        .tail
        .contains(&gated.job_id));
    assert_gated_job_started_once(&gated);

    let retained = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-once")
        .unwrap();
    assert_eq!(retained.status, "completed");
    assert_eq!(
        retained.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(retained.request_id, "request-structured-once");
}

#[cfg(unix)]
#[test]
fn chatty_job_and_queued_job_progress_while_stream_transport_is_full() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let queued_marker = temp.path().join("queued-progressed");
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(RunnerEnvelope::Ping { ts: 1 }).unwrap();
    let sink = RunnerSink::WebSocket {
        tx,
        client_id: "backpressure-agent".into(),
        runner_instance_id: "backpressure-instance".into(),
    };
    let manager = JobManager::new(1);

    enqueue_shell_job(
        &manager,
        &sink,
        temp.path(),
        "chatty-backpressure",
        format!("{} chatty 256", sh_quote(&helper.path)),
        30,
    );
    enqueue_shell_job(
        &manager,
        &sink,
        temp.path(),
        "queued-after-chatty",
        format!(
            "{} mark {}",
            sh_quote(&helper.path),
            sh_quote(&queued_marker)
        ),
        30,
    );

    assert!(
        wait_until(Duration::from_secs(30), || {
            let inventory = manager.inventory();
            let chatty_terminal = inventory.jobs.iter().any(|snapshot| {
                snapshot.job_id == "chatty-backpressure" && runner_job_is_terminal(&snapshot.status)
            });
            chatty_terminal && queued_marker.exists()
        }),
        "a full transport queue backpressured child output capture or queued-job progression"
    );
    wait_for_job_workers(&manager);

    let inventory = manager.inventory();
    let chatty = inventory
        .jobs
        .iter()
        .find(|snapshot| snapshot.job_id == "chatty-backpressure")
        .expect("chatty job retained");
    assert_eq!(chatty.status, "completed", "{chatty:?}");
    assert!(chatty.stdout.truncated);
    assert!(chatty.stderr.truncated);
    assert!(chatty.stdout.tail.len() <= JOB_SNAPSHOT_STREAM_MAX_BYTES);
    assert!(chatty.stderr.tail.len() <= JOB_SNAPSHOT_STREAM_MAX_BYTES);
    assert!(chatty.stdout.next_line > chatty.stdout.first_retained_line);
    assert!(chatty.stderr.next_line > chatty.stderr.first_retained_line);
    let queued = inventory
        .jobs
        .iter()
        .find(|snapshot| snapshot.job_id == "queued-after-chatty")
        .expect("queued job retained");
    assert_eq!(queued.status, "completed", "{queued:?}");

    assert!(matches!(rx.try_recv(), Ok(RunnerEnvelope::Ping { ts: 1 })));
}

#[test]
fn output_only_delivery_coalescing_preserves_authoritative_snapshot_invariants() {
    let manager = JobManager::new(1);
    let mut snapshot = test_job_snapshot("coalesced-output");
    snapshot.status = "agent_queued".to_string();
    snapshot.started_at = None;
    lock_unpoison(&manager.jobs).insert(
        snapshot.job_id.clone(),
        RunningJob {
            client_id: "test-agent".into(),
            runner_instance_id: "test-instance".into(),
            snapshot,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(RunnerEnvelope::Ping { ts: 7 }).unwrap();
    manager.install_sink(RunnerSink::WebSocket {
        tx,
        client_id: "test-agent".into(),
        runner_instance_id: "test-instance".into(),
    });

    manager.update_and_send(
        "coalesced-output",
        RunnerJobDelta {
            status: "running".to_string(),
            stdout_chunk: Some("start 🙂\n".to_string()),
            ..Default::default()
        },
    );
    let chunk = "🙂\n".repeat(1024);
    for _ in 0..100 {
        manager.update_and_send(
            "coalesced-output",
            RunnerJobDelta {
                status: "running".to_string(),
                stdout_chunk: Some(chunk.clone()),
                ..Default::default()
            },
        );
    }
    {
        let pending = lock_unpoison(&manager.pending_job_updates);
        let queue = pending.get("coalesced-output").expect("pending delivery");
        assert_eq!(queue.required.len(), 1);
        assert!(queue.output_only.is_some());
        assert!(queue.required.len() <= JOB_UPDATE_REQUIRED_PENDING_MAX);
    }

    manager.update_and_send(
        "coalesced-output",
        RunnerJobDelta {
            status: "completed".to_string(),
            stdout_chunk: Some("done 🙂\n".to_string()),
            exit_code: Some(0),
            duration_ms: Some(10),
            finished: true,
            ..Default::default()
        },
    );
    let retained = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "coalesced-output")
        .unwrap();
    assert_eq!(retained.status, "completed");
    assert_eq!(retained.update_seq, 103);
    assert_eq!(retained.stdout.next_line, 102_403);
    assert!(retained.stdout.tail.len() <= JOB_SNAPSHOT_STREAM_MAX_BYTES);
    assert!(retained.stdout.truncated);
    assert!(std::str::from_utf8(retained.stdout.tail.as_bytes()).is_ok());
    {
        let pending = lock_unpoison(&manager.pending_job_updates);
        let queue = pending
            .get("coalesced-output")
            .expect("terminal pending delivery");
        assert_eq!(queue.required.len(), 2);
        assert!(queue.output_only.is_none());
    }
    manager.resend_snapshot("coalesced-output");
    manager.resend_snapshot("coalesced-output");
    {
        let pending = lock_unpoison(&manager.pending_job_updates);
        let queue = pending
            .get("coalesced-output")
            .expect("deduplicated terminal replay");
        assert_eq!(queue.required.len(), 2);
        assert_eq!(queue.required.back().unwrap().update_seq, 103);
    }

    manager.update_and_send(
        "coalesced-output",
        RunnerJobDelta {
            status: "running".to_string(),
            stdout_chunk: Some("late\n".to_string()),
            ..Default::default()
        },
    );
    let immutable = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "coalesced-output")
        .unwrap();
    assert_eq!(immutable.update_seq, 103);
    assert_eq!(immutable.stdout.next_line, 102_403);

    assert!(matches!(rx.try_recv(), Ok(RunnerEnvelope::Ping { ts: 7 })));
    let updates = collect_job_updates(&mut rx, Duration::from_secs(5));
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert_eq!(updates[0].status, "running");
    assert_eq!(updates[0].update_seq, Some(2));
    assert_eq!(updates[1].status, "completed");
    assert_eq!(updates[1].update_seq, Some(103));
    assert!(updates[1].finished);
    let first_logs = updates[0].log_snapshot.as_ref().unwrap();
    let terminal_logs = updates[1].log_snapshot.as_ref().unwrap();
    assert_eq!(first_logs.stdout.next_line, 102_403);
    assert_eq!(terminal_logs.stdout.next_line, 102_403);
    assert!(std::str::from_utf8(first_logs.stdout.tail.as_bytes()).is_ok());
}

#[test]
fn structured_process_job_preserves_large_literal_argv_without_shell_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let shell_marker = temp.path().join("shell-marker");
    let values = vec![
        "a".repeat(4_500),
        "b".repeat(4_500),
        format!("$(touch {})", shell_marker.display()),
        format!("; touch {}", shell_marker.display()),
    ];
    let mut args = vec!["argv".to_string()];
    args.extend(values.clone());
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    enqueue_structured_process_job(
        &manager,
        sink,
        temp.path(),
        "structured-large-argv",
        &helper.path,
        args,
        None,
        30,
        None,
    );
    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    let final_update = updates.last().expect("large argv terminal update");
    assert_eq!(final_update.status, "completed", "{final_update:?}");
    assert_eq!(
        final_update.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    let stdout = &final_update.log_snapshot.as_ref().unwrap().stdout.tail;
    for value in &values {
        assert!(stdout.contains(value));
    }
    assert!(!shell_marker.exists(), "shell-looking argv was interpreted");
}

#[test]
fn structured_process_job_terminal_lifecycle_distinguishes_prestart_nonzero_and_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    for (job_id, executable, args, timeout_secs, status, state, exit_code) in [
        (
            "structured-missing",
            temp.path().join("missing-executable"),
            Vec::new(),
            5,
            "failed",
            ShellCommandExecutionState::NotStarted,
            None,
        ),
        (
            "structured-nonzero",
            helper.path.clone(),
            vec!["exit".to_string(), "19".to_string()],
            30,
            "failed",
            ShellCommandExecutionState::Completed,
            Some(19),
        ),
        (
            "structured-timeout",
            helper.path.clone(),
            // Sleep far longer than the 10s timeout so the timeout provably
            // fires while the process is still running. A shorter sleep would
            // let the helper exit cleanly and the job complete instead.
            vec!["sleep".to_string(), "60000".to_string()],
            10,
            "timeout",
            ShellCommandExecutionState::TimedOut,
            Some(-1),
        ),
    ] {
        let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
        let manager = JobManager::new(1);
        let started = Instant::now();
        enqueue_structured_process_job(
            &manager,
            sink,
            temp.path(),
            job_id,
            &executable,
            args,
            None,
            timeout_secs,
            None,
        );
        // Quick completion fixtures use a 30-second execution budget so loaded
        // Windows runners cannot misclassify process startup as a timeout. The
        // collection window must outlast that budget while the dedicated timeout
        // case still proves the original ten-second timeout contract below.
        let updates = collect_job_updates(&mut rx, Duration::from_secs(45));
        let final_update = updates.last().expect("terminal lifecycle update");
        assert_eq!(final_update.status, status, "{final_update:?}");
        assert_eq!(
            final_update.command_execution_state,
            Some(state),
            "{final_update:?}"
        );
        assert_eq!(final_update.exit_code, exit_code, "{final_update:?}");
        if state == ShellCommandExecutionState::TimedOut {
            // The timeout budget must be generous enough that a freshly
            // spawned helper can actually start before it expires: on a
            // loaded runner, process initialization alone can take several
            // seconds, and a 1s budget would kill the child before it ran.
            // The original ten-second budget must still be honored exactly:
            // the reported duration is the timeout value itself (plus at most
            // one poll interval), never reset by the lifecycle.
            let duration_ms = final_update.duration_ms.expect("timeout duration");
            assert!(
                (9_850..=10_250).contains(&duration_ms),
                "the ten-second original timeout was reset or extended: {duration_ms} ms"
            );
            assert!(
                started.elapsed() < Duration::from_secs(25),
                "the original total timeout was extended: {:?}",
                started.elapsed()
            );
        }
        if state == ShellCommandExecutionState::NotStarted {
            assert!(
                !updates.iter().any(|update| update.status == "running"),
                "pre-spawn failure must never claim the child started"
            );
        }
    }
}

#[test]
fn structured_process_job_drains_large_output_without_log_observation_and_runs_once() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let marker = temp.path().join("chatty-starts.log");
    let nonce = format!("chatty-{}", uuid::Uuid::new_v4());
    // Deliberately retain the sink receiver without consuming it. Durable Job
    // execution must not depend on Server/model log observation to drain the
    // child OS pipes.
    let (sink, _unread_rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let output_limit = 16 * 1024;
    enqueue_structured_process_job_with_policy(
        &manager,
        sink,
        temp.path(),
        "structured-chatty",
        &helper.path,
        vec![
            "mark-chatty".to_string(),
            marker.to_string_lossy().into_owned(),
            nonce.clone(),
            "512".to_string(),
        ],
        None,
        10,
        None,
        RunnerPolicy {
            allow_cwd_anywhere: true,
            max_output_bytes: output_limit,
            ..RunnerPolicy::default()
        },
    );

    wait_for_job_workers(&manager);
    let snapshot = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-chatty")
        .expect("same durable Job remains observable at terminal");
    assert_eq!(snapshot.status, "completed", "{snapshot:?}");
    assert_eq!(snapshot.request_id, "request-structured-chatty");
    assert_eq!(
        snapshot.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(snapshot.exit_code, Some(0));
    for (name, stream, tail_byte) in [
        ("stdout", &snapshot.stdout, b'x'),
        ("stderr", &snapshot.stderr, b'y'),
    ] {
        assert!(
            stream.tail.len() <= output_limit,
            "{name} exceeded policy bound"
        );
        assert!(
            stream.tail.starts_with("[output truncated]\n"),
            "{name}: {:?}",
            stream.tail
        );
        assert!(stream.tail.as_bytes().iter().any(|byte| *byte == tail_byte));
        assert!(std::str::from_utf8(stream.tail.as_bytes()).is_ok());
    }
    let starts = std::fs::read_to_string(marker).unwrap();
    assert_eq!(starts.lines().count(), 1, "structured Job was redispatched");
    assert!(starts.contains(&nonce));
}

#[test]
fn structured_process_job_stop_after_large_output_is_bounded_and_exact_once() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let marker = temp.path().join("chatty-stop-starts.log");
    let ready = temp.path().join("chatty-stop-ready");
    let nonce = format!("chatty-stop-{}", uuid::Uuid::new_v4());
    let (sink, _unread_rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let output_limit = 16 * 1024;
    enqueue_structured_process_job_with_policy(
        &manager,
        sink,
        temp.path(),
        "structured-chatty-stop",
        &helper.path,
        vec![
            "mark-chatty-sleep".to_string(),
            marker.to_string_lossy().into_owned(),
            ready.to_string_lossy().into_owned(),
            nonce.clone(),
            "512".to_string(),
            "60000".to_string(),
        ],
        None,
        30,
        None,
        RunnerPolicy {
            allow_cwd_anywhere: true,
            max_output_bytes: output_limit,
            ..RunnerPolicy::default()
        },
    );
    assert!(
        wait_until(Duration::from_secs(10), || ready.exists()),
        "child never finished writing output far beyond pipe capacity"
    );

    manager.stop("structured-chatty-stop").unwrap();
    wait_for_job_workers(&manager);
    let snapshot = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-chatty-stop")
        .unwrap();
    assert_eq!(snapshot.status, "stopped", "{snapshot:?}");
    assert_eq!(snapshot.request_id, "request-structured-chatty-stop");
    assert_eq!(
        snapshot.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    for stream in [&snapshot.stdout, &snapshot.stderr] {
        assert!(stream.tail.len() <= output_limit);
        assert!(stream.tail.starts_with("[output truncated]\n"));
        assert!(std::str::from_utf8(stream.tail.as_bytes()).is_ok());
    }
    let starts = std::fs::read_to_string(marker).unwrap();
    assert_eq!(starts.lines().count(), 1, "stop redispatched the Job");
    assert!(starts.contains(&nonce));
}

#[test]
fn structured_process_job_timeout_after_large_output_is_bounded_and_exact_once() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let marker = temp.path().join("chatty-timeout-starts.log");
    let ready = temp.path().join("chatty-timeout-ready");
    let nonce = format!("chatty-timeout-{}", uuid::Uuid::new_v4());
    let (sink, _unread_rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let output_limit = 16 * 1024;
    enqueue_structured_process_job_with_policy(
        &manager,
        sink,
        temp.path(),
        "structured-chatty-timeout",
        &helper.path,
        vec![
            "mark-chatty-sleep".to_string(),
            marker.to_string_lossy().into_owned(),
            ready.to_string_lossy().into_owned(),
            nonce.clone(),
            "512".to_string(),
            "60000".to_string(),
        ],
        None,
        5,
        None,
        RunnerPolicy {
            allow_cwd_anywhere: true,
            max_output_bytes: output_limit,
            ..RunnerPolicy::default()
        },
    );
    assert!(
        wait_until(Duration::from_secs(10), || ready.exists()),
        "child never finished writing output far beyond pipe capacity"
    );

    wait_for_job_workers(&manager);
    let snapshot = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-chatty-timeout")
        .unwrap();
    assert_eq!(snapshot.status, "timeout", "{snapshot:?}");
    assert_eq!(snapshot.request_id, "request-structured-chatty-timeout");
    assert_eq!(
        snapshot.command_execution_state,
        Some(ShellCommandExecutionState::TimedOut)
    );
    assert!(snapshot.stdout.tail.len() <= output_limit);
    assert!(snapshot.stderr.tail.len() <= output_limit);
    assert!(snapshot.stdout.tail.starts_with("[output truncated]\n"));
    assert!(snapshot
        .stderr
        .tail
        .contains("command timed out after 5 seconds"));
    assert!(std::str::from_utf8(snapshot.stdout.tail.as_bytes()).is_ok());
    assert!(std::str::from_utf8(snapshot.stderr.tail.as_bytes()).is_ok());
    let starts = std::fs::read_to_string(marker).unwrap();
    assert_eq!(starts.lines().count(), 1, "timeout redispatched the Job");
    assert!(starts.contains(&nonce));
}

#[test]
fn structured_process_job_shutdown_after_large_output_reaps_the_same_execution() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let marker = temp.path().join("chatty-shutdown-starts.log");
    let ready = temp.path().join("chatty-shutdown-ready");
    let nonce = format!("chatty-shutdown-{}", uuid::Uuid::new_v4());
    let (sink, _unread_rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    enqueue_structured_process_job_with_policy(
        &manager,
        sink,
        temp.path(),
        "structured-chatty-shutdown",
        &helper.path,
        vec![
            "mark-chatty-sleep".to_string(),
            marker.to_string_lossy().into_owned(),
            ready.to_string_lossy().into_owned(),
            nonce.clone(),
            "512".to_string(),
            "60000".to_string(),
        ],
        None,
        30,
        None,
        RunnerPolicy {
            allow_cwd_anywhere: true,
            max_output_bytes: 16 * 1024,
            ..RunnerPolicy::default()
        },
    );
    assert!(wait_until(Duration::from_secs(10), || ready.exists()));

    manager.stop_all();
    wait_for_job_workers(&manager);
    let snapshot = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-chatty-shutdown")
        .unwrap();
    assert_eq!(snapshot.status, "stopped", "{snapshot:?}");
    assert_eq!(snapshot.request_id, "request-structured-chatty-shutdown");
    assert_eq!(
        snapshot.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert!(snapshot.stdout.tail.len() <= 16 * 1024);
    assert!(snapshot.stderr.tail.len() <= 16 * 1024);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        1,
        "shutdown redispatched the structured Job"
    );
    assert!(std::fs::read_to_string(&marker).unwrap().contains(&nonce));
}

#[test]
fn structured_process_job_handoff_observation_does_not_reset_the_original_total_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let marker = temp.path().join("timeout-started.log");
    let nonce = format!("timeout-{}", uuid::Uuid::new_v4());
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let original_start = Instant::now();
    enqueue_structured_process_job(
        &manager,
        sink,
        temp.path(),
        "structured-original-timeout",
        &helper.path,
        vec![
            "mark-sleep".to_string(),
            marker.to_string_lossy().into_owned(),
            nonce.clone(),
            "60000".to_string(),
        ],
        None,
        10,
        None,
    );
    // The timeout budget must be generous enough that a freshly spawned
    // helper can actually start before it expires: on a loaded runner,
    // process initialization alone (spawn return to first instruction) can
    // take several seconds, and a 1s budget would kill the child before it
    // ever wrote the marker. The helper sleeps 60s, so the 10s timeout fires
    // while the process is provably still running. The assertions below
    // still prove the original timeout is not reset at handoff.
    assert!(wait_until(Duration::from_secs(30), || marker.exists()));

    // The child can write its marker before the parent task is rescheduled to
    // publish the post-spawn `running` state. That race is visible under a
    // heavily parallel test load, so wait only for that state publication;
    // inventory observation remains read-only and cannot replace the process
    // or reset its original deadline.
    assert!(wait_until(Duration::from_secs(2), || {
        manager.inventory().jobs.into_iter().any(|snapshot| {
            snapshot.job_id == "structured-original-timeout" && snapshot.status == "running"
        })
    }));
    let handoff = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-original-timeout")
        .expect("the same active Job is exposed at handoff");
    assert_eq!(handoff.status, "running");
    assert_eq!(handoff.command_execution_state, None);

    let observation_timeout = Duration::from_secs(25).saturating_sub(original_start.elapsed());
    let updates = collect_job_updates(&mut rx, observation_timeout);
    let final_update = updates.last().expect("original timeout terminal update");
    assert_eq!(final_update.status, "timeout", "{final_update:?}");
    assert_eq!(
        final_update.command_execution_state,
        Some(ShellCommandExecutionState::TimedOut)
    );
    let duration_ms = final_update.duration_ms.expect("timeout duration");
    assert!(
        (9_850..=10_250).contains(&duration_ms),
        "the ten-second original timeout was reset at handoff: {duration_ms} ms"
    );
    // The job must still complete within the original ten-second budget plus
    // process-startup slack; a handoff that reset the deadline to a larger
    // value would overshoot this.
    assert!(
        original_start.elapsed() < Duration::from_secs(25),
        "handoff extended the original total timeout"
    );
    let starts = std::fs::read_to_string(marker).unwrap();
    assert_eq!(starts.lines().count(), 1);
    assert!(starts.contains(&nonce));
}

#[test]
fn stop_terminates_a_structured_process_job_without_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let marker = temp.path().join("stop-started.log");
    let nonce = format!("stop-{}", uuid::Uuid::new_v4());
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    enqueue_structured_process_job(
        &manager,
        sink,
        temp.path(),
        "structured-stop",
        &helper.path,
        vec![
            "mark-sleep".to_string(),
            marker.to_string_lossy().into_owned(),
            nonce.clone(),
            "5000".to_string(),
        ],
        None,
        30,
        None,
    );
    assert!(wait_until(Duration::from_secs(30), || marker.exists()));
    manager.stop("structured-stop").unwrap();
    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    let final_update = updates.last().expect("structured stop terminal update");
    assert_eq!(final_update.status, "stopped", "{final_update:?}");
    assert_eq!(
        final_update.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert_eq!(
        updates.iter().filter(|update| update.finished).count(),
        1,
        "stop must terminate the existing execution once"
    );
}

#[cfg(windows)]
#[test]
fn phase_f_windows_structured_job_normalizes_oem_stdout_and_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let expected_path = temp.path().join("expected-oem.txt");
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    enqueue_structured_process_job(
        &manager,
        sink,
        temp.path(),
        "phase-f-oem-job",
        &helper.path,
        vec![
            "windows-oem-output".to_string(),
            expected_path.to_string_lossy().into_owned(),
        ],
        None,
        10,
        None,
    );
    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    let final_update = updates.last().expect("OEM Job terminal update");
    let expected = std::fs::read_to_string(expected_path).unwrap();
    let logs = final_update.log_snapshot.as_ref().unwrap();
    assert_eq!(final_update.status, "failed");
    assert_eq!(final_update.exit_code, Some(23));
    assert_eq!(
        final_update.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(logs.stdout.tail, expected);
    assert_eq!(logs.stderr.tail, expected);
}

#[cfg(windows)]
#[test]
fn phase_f_windows_shell_job_stream_reconstructs_split_utf8_and_oem() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let marker = temp.path().join("split-started");
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let helper = helper.path.to_string_lossy().replace('\'', "''");
    let marker_arg = marker.to_string_lossy().replace('\'', "''");
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            ShellConfig::default(),
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": "request-phase-f-stream",
                "client_id": "structured-agent",
                "kind": "start_job",
                "job_id": "phase-f-stream",
                "cwd": temp.path(),
                "command": format!("& '{helper}' windows-utf8-split-output '{marker_arg}'"),
                "timeout_secs": 30,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": test_job_context(temp.path(), Vec::new()),
            }))
            .unwrap(),
        ),
    );
    assert!(
        wait_until(Duration::from_secs(30), || marker.exists()),
        "UTF-8 split-output fixture did not start"
    );
    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    let final_update = updates.last().expect("stream Job terminal update");
    assert_eq!(final_update.status, "completed", "{final_update:?}");
    assert_eq!(
        final_update.log_snapshot.as_ref().unwrap().stdout.tail,
        "split 中 🙂\n"
    );
    assert!(marker.exists());

    let expected_path = temp.path().join("split-oem-expected");
    let oem_marker = temp.path().join("split-oem-started");
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let expected_arg = expected_path.to_string_lossy().replace('\'', "''");
    let marker_arg = oem_marker.to_string_lossy().replace('\'', "''");
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            ShellConfig::default(),
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": "request-phase-f-oem-stream",
                "client_id": "structured-agent",
                "kind": "start_job",
                "job_id": "phase-f-oem-stream",
                "cwd": temp.path(),
                "command": format!(
                    "& '{helper}' windows-oem-split-output '{expected_arg}' '{marker_arg}'"
                ),
                "timeout_secs": 30,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": test_job_context(temp.path(), Vec::new()),
            }))
            .unwrap(),
        ),
    );
    assert!(
        wait_until(Duration::from_secs(30), || oem_marker.exists()),
        "OEM split-output fixture did not start"
    );
    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    let final_update = updates.last().expect("OEM stream Job terminal update");
    let logs = final_update.log_snapshot.as_ref().unwrap();
    let expected = std::fs::read_to_string(expected_path).unwrap();
    assert_eq!(final_update.status, "completed", "{final_update:?}");
    assert_eq!(logs.stdout.tail, expected);
    assert_eq!(logs.stderr.tail, expected);
    assert!(oem_marker.exists());
}

#[cfg(windows)]
#[test]
fn phase_f_windows_structured_job_stop_retains_unicode_and_runs_once() {
    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let marker = temp.path().join("stop-output-started.log");
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    enqueue_structured_process_job(
        &manager,
        sink,
        temp.path(),
        "phase-f-stop",
        &helper.path,
        vec![
            "windows-mark-output-sleep".to_string(),
            marker.to_string_lossy().into_owned(),
            "10000".to_string(),
        ],
        None,
        30,
        None,
    );
    assert!(wait_until(Duration::from_secs(30), || marker.exists()));
    manager.stop("phase-f-stop").unwrap();
    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    let final_update = updates.last().expect("stop terminal update");
    let logs = final_update.log_snapshot.as_ref().unwrap();
    assert_eq!(final_update.status, "stopped", "{final_update:?}");
    assert_eq!(
        final_update.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert!(logs.stdout.tail.contains("partial 中文 🙂\n"));
    assert!(logs.stderr.tail.contains("partial 中文 🙂\n"));
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert_eq!(updates.iter().filter(|update| update.finished).count(), 1);
}

#[cfg(unix)]
#[test]
fn structured_script_job_keeps_its_temporary_file_until_terminal_then_removes_it() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("script-started.log");
    let observed_path = temp.path().join("script-path");
    let script = "printf '%s\\n' \"$$\" >> \"$1\"\nprintf '%s' \"$0\" > \"$2\"\nsleep 1\ntest -f \"$0\"\nprintf 'same-script-complete\\n'\n";
    let args = vec![
        marker.to_string_lossy().into_owned(),
        observed_path.to_string_lossy().into_owned(),
    ];
    let context = structured_script_context(
        temp.path(),
        runner_protocol::ShellScriptLanguage::Sh,
        script.len(),
        args.len(),
        false,
    );
    let (sink, mut rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    let request: RunnerRequest = serde_json::from_value(json!({
        "request_id": "request-structured-script",
        "client_id": "structured-agent",
        "kind": "start_script_job",
        "job_id": "structured-script",
        "cwd": temp.path(),
        "command": "",
        "script": {
            "language": "sh",
            "script": script,
            "args": args,
        },
        "timeout_secs": 5,
        "requested_by": "test",
        "created_at": chrono::Utc::now().timestamp(),
        "job_context": context,
    }))
    .unwrap();
    assert!(request.command.is_empty());
    assert!(request.process.is_none());
    assert_eq!(request.script.as_ref().unwrap().script, script);
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            ShellConfig::default(),
            SshConfig::default(),
            temp.path().join("project-registry"),
            request,
        ),
    );

    assert!(wait_until(Duration::from_secs(30), || {
        marker.exists() && observed_path.exists()
    }));
    let temporary_script = PathBuf::from(std::fs::read_to_string(&observed_path).unwrap());
    assert!(
        temporary_script.exists(),
        "the Runner-owned script file was removed while its one execution was still running"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    let active = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-script")
        .unwrap();
    assert_eq!(active.status, "running");
    let durable = serde_json::to_string(&active).unwrap();
    assert!(!durable.contains(script));
    assert!(!durable.contains(marker.to_string_lossy().as_ref()));

    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    let final_update = updates.last().expect("script terminal update");
    assert_eq!(final_update.status, "completed", "{final_update:?}");
    assert_eq!(
        final_update.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert!(final_update
        .log_snapshot
        .as_ref()
        .unwrap()
        .stdout
        .tail
        .contains("same-script-complete"));
    assert!(
        !temporary_script.exists(),
        "the Runner-owned script file was not removed after terminal completion"
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
}

#[cfg(unix)]
#[test]
fn structured_script_job_drains_large_output_without_log_observation_and_runs_once() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("script-chatty-starts.log");
    let stdout_payload = "x".repeat(4096);
    let stderr_payload = "y".repeat(4096);
    let script = format!(
        "printf '%s\\n' \"$$\" >> \"$1\"\nout='{stdout_payload}'\nerr='{stderr_payload}'\ni=0\nwhile [ \"$i\" -lt 512 ]; do\n  printf '%s' \"$out\"\n  printf '%s' \"$err\" >&2\n  i=$((i + 1))\ndone\n"
    );
    let args = vec![marker.to_string_lossy().into_owned()];
    let context = structured_script_context(
        temp.path(),
        runner_protocol::ShellScriptLanguage::Sh,
        script.len(),
        args.len(),
        false,
    );
    // Keep the transport receiver alive but unread. No model-side log read or
    // per-chunk consumer is allowed to be necessary for child progress.
    let (sink, _unread_rx) = structured_test_sink("structured-agent", "structured-instance");
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                max_output_bytes: 16 * 1024,
                ..RunnerPolicy::default()
            },
            ShellConfig::default(),
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": "request-structured-script-chatty",
                "client_id": "structured-agent",
                "kind": "start_script_job",
                "job_id": "structured-script-chatty",
                "cwd": temp.path(),
                "command": "",
                "script": {
                    "language": "sh",
                    "script": script,
                    "args": args,
                },
                "timeout_secs": 10,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": context,
            }))
            .unwrap(),
        ),
    );

    wait_for_job_workers(&manager);
    let snapshot = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-script-chatty")
        .expect("same durable script Job remains observable at terminal");
    assert_eq!(snapshot.status, "completed", "{snapshot:?}");
    assert_eq!(snapshot.request_id, "request-structured-script-chatty");
    assert_eq!(
        snapshot.command_execution_state,
        Some(ShellCommandExecutionState::Completed)
    );
    assert_eq!(snapshot.exit_code, Some(0));
    let output_limit = 16 * 1024;
    for (name, stream, tail_byte) in [
        ("stdout", &snapshot.stdout, b'x'),
        ("stderr", &snapshot.stderr, b'y'),
    ] {
        assert!(
            stream.tail.len() <= output_limit,
            "{name} exceeded policy bound"
        );
        assert!(
            stream.tail.starts_with("[output truncated]\n"),
            "{name}: {:?}",
            stream.tail
        );
        assert!(stream.tail.as_bytes().iter().any(|byte| *byte == tail_byte));
        assert!(std::str::from_utf8(stream.tail.as_bytes()).is_ok());
    }
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().lines().count(),
        1,
        "structured script Job was redispatched"
    );
}
#[test]
fn structured_job_snapshot_preserves_post_spawn_outcome_unknown() {
    let manager = JobManager::new(1);
    let mut snapshot = test_job_snapshot("structured-uncertain");
    snapshot.status = "running".to_string();
    snapshot.started_at = Some(snapshot.created_at + 1);
    snapshot.context.structured_execution =
        Some(runner_protocol::ShellJobStructuredExecutionMetadata {
            execution_source: "run_process".to_string(),
            language: None,
            script_bytes: None,
            arg_count: 0,
            stdin_present: false,
            validation_identity: None,
            validation_tool: None,
            assertion_name: None,
        });
    lock_unpoison(&manager.jobs).insert(
        snapshot.job_id.clone(),
        RunningJob {
            client_id: "structured-agent".to_string(),
            runner_instance_id: "structured-instance".to_string(),
            snapshot,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );
    manager.update_and_send(
        "structured-uncertain",
        RunnerJobDelta {
            status: "lost".to_string(),
            error: Some("post-spawn result unavailable".to_string()),
            command_execution_state: Some(ShellCommandExecutionState::OutcomeUnknown),
            finished: true,
            ..Default::default()
        },
    );
    let retained = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "structured-uncertain")
        .unwrap();
    assert_eq!(retained.status, "lost");
    assert_eq!(
        retained.command_execution_state,
        Some(ShellCommandExecutionState::OutcomeUnknown)
    );
    assert!(retained.started_at.is_some());
}
/// The outcome the executor emits when a step could not be spawned at all.
///
/// This is a modeled result, not a bug — `validation_spawn_failure_is_
/// infrastructure_without_failed_assertion` pins it, and the connector treats
/// it as infrastructure rather than as a failed assertion. Recognising it here
/// keeps a machine-level spawn failure from being read as a fail-fast
/// regression.
#[cfg(unix)]
fn is_validation_spawn_failure(update: &RunnerJobUpdateRequest) -> bool {
    update.finished
        && update.status == "failed"
        && update.exit_code.is_none()
        && update.error.as_deref() == Some(VALIDATION_STEP_SPAWN_FAILED_CODE)
}

#[cfg(unix)]
fn describe_update(update: &RunnerJobUpdateRequest) -> String {
    format!(
        "status={:?} finished={} exit_code={:?} error={:?} progress={:?}",
        update.status, update.finished, update.exit_code, update.error, update.validation_progress
    )
}

#[cfg(unix)]
fn run_fail_fast_validation_job(attempt: usize) -> FailFastAttempt {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh\ncase \"$1\" in\nfmt) echo 'format passed';;\ncheck) exit 7;;\ntest) touch should-not-run;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let sink = RunnerSink::WebSocket {
        tx,
        client_id: "validation-agent".into(),
        runner_instance_id: "validation-instance".into(),
    };
    let steps = vec![
        ShellJobValidationStep {
            name: "format".into(),
            program: "cargo".into(),
            args: vec!["fmt".into(), "--".into(), "--check".into()],
            env: Vec::new(),
        },
        ShellJobValidationStep {
            name: "check".into(),
            program: "cargo".into(),
            args: vec!["check".into(), "--all-targets".into()],
            env: Vec::new(),
        },
        ShellJobValidationStep {
            name: "test".into(),
            program: "cargo".into(),
            args: vec!["test".into()],
            env: Vec::new(),
        },
    ];
    let mut shell = ShellConfig::default();
    shell.path_prepend.push(bin);
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                // These tests run jobs in a temp dir; the boundary itself is
                // covered separately, and RunnerPolicy::default() is fail-closed.
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            shell,
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": format!("validation-request-{attempt}"),
                "client_id": "validation-agent",
                "kind": "start_validation_job",
                "job_id": format!("validation-job-{attempt}"),
                "cwd": temp.path(),
                "command": serde_json::to_string(&steps).unwrap(),
                // Two `sh` one-liners. A timeout here would mean a hang, not a busy
                // machine, which is the point of the gap between this and the
                // collector deadline below.
                "timeout_secs": 60,
                "requested_by": "test",
                "created_at": 1,
                "job_context": test_job_context(
                    temp.path(),
                    steps.iter().map(|step| step.name.clone()).collect(),
                )
            }))
            .unwrap(),
        ),
    );
    let updates = collect_job_updates(&mut rx, Duration::from_secs(120));
    FailFastAttempt {
        test_step_ran: temp.path().join("should-not-run").exists(),
        updates,
    }
}

#[cfg(unix)]
#[test]
fn validation_job_exposes_activity_during_silent_step_and_clears_terminal() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let cargo = bin.join("cargo");
    let started = temp.path().join("silent-validation.started");
    let release = temp.path().join("silent-validation.release");
    std::fs::write(
        &cargo,
        format!(
            "#!/bin/sh\n: > {}\nwhile [ ! -e {} ]; do sleep 0.05; done\n",
            sh_quote(&started),
            sh_quote(&release),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o700)).unwrap();

    let steps = vec![ShellJobValidationStep {
        name: "check".into(),
        program: "cargo".into(),
        args: vec!["check".into(), "--all-targets".into()],
        env: Vec::new(),
    }];
    let mut shell = ShellConfig::default();
    shell.path_prepend.push(bin);
    let (sink, _rx) = structured_test_sink("validation-agent", "validation-instance");
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            shell,
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": "silent-validation-request",
                "client_id": "validation-agent",
                "kind": "start_validation_job",
                "job_id": "silent-validation-job",
                "cwd": temp.path(),
                "command": serde_json::to_string(&steps).unwrap(),
                "timeout_secs": 30,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": test_job_context(temp.path(), vec!["check".to_string()]),
            }))
            .unwrap(),
        ),
    );

    assert!(
        wait_until(Duration::from_secs(10), || started.exists()),
        "silent validation step did not start"
    );
    let expected = Some(ShellJobActivity {
        state: ShellJobActivityState::Working,
        phase: ShellJobActivityPhase::ValidationCheck,
        source: ShellJobActivitySource::ValidationPlan,
    });
    let running = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "silent-validation-job")
        .unwrap();
    assert_eq!(running.status, "running");
    assert!(running.stdout.tail.is_empty());
    assert!(running.stderr.tail.is_empty());
    assert_eq!(running.activity, expected);

    std::thread::sleep(Duration::from_millis(150));
    let still_silent = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "silent-validation-job")
        .unwrap();
    assert!(still_silent.stdout.tail.is_empty());
    assert!(still_silent.stderr.tail.is_empty());
    assert_eq!(still_silent.activity, expected);

    std::fs::write(&release, b"go").unwrap();
    wait_for_job_workers(&manager);
    let terminal = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "silent-validation-job")
        .unwrap();
    assert_eq!(terminal.status, "completed");
    assert_eq!(terminal.activity, None);
}

#[test]
fn arbitrary_process_output_cannot_forge_cargo_activity() {
    let manager = JobManager::new(1);
    let mut snapshot = test_job_snapshot("activity-spoof-job");
    snapshot.activity = Some(process_running_activity());
    lock_unpoison(&manager.jobs).insert(
        snapshot.job_id.clone(),
        RunningJob {
            client_id: "test-agent".into(),
            runner_instance_id: "test-instance".into(),
            snapshot,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );
    manager.record_update(
        "activity-spoof-job",
        RunnerJobDelta {
            stdout_chunk: Some("Compiling forged-package\n".into()),
            stderr_chunk: Some("Blocking waiting for file lock on build directory\n".into()),
            ..Default::default()
        },
    );
    let retained = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "activity-spoof-job")
        .unwrap();
    assert_eq!(retained.activity, Some(process_running_activity()));

    let cargo_step = ShellJobValidationStep {
        name: "check".into(),
        program: "cargo".into(),
        args: vec!["check".into(), "--all-targets".into()],
        env: Vec::new(),
    };
    assert_eq!(
        cargo_activity_from_stderr(&cargo_step, "Compiling webcodex-core\n"),
        Some(ShellJobActivity {
            state: ShellJobActivityState::Working,
            phase: ShellJobActivityPhase::CargoCompiling,
            source: ShellJobActivitySource::CargoOutput,
        })
    );
    let non_cargo_step = ShellJobValidationStep {
        name: "check".into(),
        program: "sh".into(),
        args: vec!["-c".into(), "true".into()],
        env: Vec::new(),
    };
    assert_eq!(
        cargo_activity_from_stderr(&non_cargo_step, "Compiling forged-package\n"),
        None
    );
}

#[test]
fn cargo_activity_returns_to_validation_plan_after_fine_phase_ends() {
    let manager = JobManager::new(1);
    let step = ShellJobValidationStep {
        name: "test".into(),
        program: "cargo".into(),
        args: vec!["test".into()],
        env: Vec::new(),
    };
    let validation_activity = validation_step_activity(&step);
    let mut snapshot = test_job_snapshot("cargo-current-activity");
    snapshot.context.validation_steps = vec!["test".into()];
    snapshot.validation_progress = Some(ShellJobValidationProgress {
        completed: 0,
        current_step: Some("test".into()),
        failed_step: None,
    });
    snapshot.activity = Some(validation_activity);
    lock_unpoison(&manager.jobs).insert(
        snapshot.job_id.clone(),
        RunningJob {
            client_id: "test-agent".into(),
            runner_instance_id: "test-instance".into(),
            snapshot,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );

    let compiling = cargo_activity_from_stderr(&step, "Compiling webcodex v0.3.9\n").unwrap();
    manager.record_update(
        "cargo-current-activity",
        RunnerJobDelta {
            status: "running".into(),
            activity: Some(compiling),
            ..Default::default()
        },
    );
    assert_eq!(
        manager
            .inventory()
            .jobs
            .into_iter()
            .find(|job| job.job_id == "cargo-current-activity")
            .unwrap()
            .activity,
        Some(compiling)
    );

    let phase_ended = cargo_activity_from_stderr(
        &step,
        "Finished `test` profile [unoptimized] target(s) in 0.10s\nRunning unittests src/lib.rs\n",
    )
    .unwrap();
    assert_eq!(phase_ended, validation_activity);
    manager.record_update(
        "cargo-current-activity",
        RunnerJobDelta {
            status: "running".into(),
            activity: Some(phase_ended),
            ..Default::default()
        },
    );
    assert_eq!(
        manager
            .inventory()
            .jobs
            .into_iter()
            .find(|job| job.job_id == "cargo-current-activity")
            .unwrap()
            .activity,
        Some(validation_activity)
    );

    manager.record_update(
        "cargo-current-activity",
        RunnerJobDelta {
            status: "completed".into(),
            exit_code: Some(0),
            validation_progress: Some(ShellJobValidationProgress {
                completed: 1,
                current_step: None,
                failed_step: None,
            }),
            finished: true,
            ..Default::default()
        },
    );
    let terminal = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|job| job.job_id == "cargo-current-activity")
        .unwrap();
    assert_eq!(terminal.status, "completed");
    assert_eq!(terminal.activity, None);
}

#[test]
fn activity_only_delivery_coalesces_without_consuming_required_semantic_queue() {
    let manager = JobManager::new(1);
    let check = ShellJobValidationStep {
        name: "check".into(),
        program: "cargo".into(),
        args: vec!["check".into(), "--all-targets".into()],
        env: Vec::new(),
    };
    let test = ShellJobValidationStep {
        name: "test".into(),
        program: "cargo".into(),
        args: vec!["test".into()],
        env: Vec::new(),
    };
    let mut snapshot = test_job_snapshot("activity-backpressure");
    snapshot.context.validation_steps = vec!["check".into(), "test".into()];
    snapshot.validation_progress = Some(ShellJobValidationProgress {
        completed: 0,
        current_step: Some("check".into()),
        failed_step: None,
    });
    snapshot.activity = Some(validation_step_activity(&check));
    lock_unpoison(&manager.jobs).insert(
        snapshot.job_id.clone(),
        RunningJob {
            client_id: "test-agent".into(),
            runner_instance_id: "test-instance".into(),
            snapshot,
            child: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(RunnerEnvelope::Ping { ts: 11 }).unwrap();
    manager.install_sink(RunnerSink::WebSocket {
        tx,
        client_id: "test-agent".into(),
        runner_instance_id: "test-instance".into(),
    });

    let compiling = ShellJobActivity {
        state: ShellJobActivityState::Working,
        phase: ShellJobActivityPhase::CargoCompiling,
        source: ShellJobActivitySource::CargoOutput,
    };
    let checking = ShellJobActivity {
        state: ShellJobActivityState::Working,
        phase: ShellJobActivityPhase::CargoChecking,
        source: ShellJobActivitySource::CargoOutput,
    };
    for index in 0..64 {
        manager.update_and_send(
            "activity-backpressure",
            RunnerJobDelta {
                status: "running".into(),
                activity: Some(if index % 2 == 0 { compiling } else { checking }),
                ..Default::default()
            },
        );
    }
    {
        let pending = lock_unpoison(&manager.pending_job_updates);
        let queue = pending
            .get("activity-backpressure")
            .expect("coalesced activity pending delivery");
        assert!(queue.required.is_empty());
        assert_eq!(queue.output_only.as_ref().unwrap().activity, Some(checking));
        assert!(!queue.suspended_until_reconciliation);
    }
    assert_eq!(
        manager
            .inventory()
            .jobs
            .into_iter()
            .find(|job| job.job_id == "activity-backpressure")
            .unwrap()
            .activity,
        Some(checking)
    );

    manager.update_and_send(
        "activity-backpressure",
        RunnerJobDelta {
            status: "running".into(),
            validation_progress: Some(ShellJobValidationProgress {
                completed: 1,
                current_step: Some("test".into()),
                failed_step: None,
            }),
            activity: Some(validation_step_activity(&test)),
            ..Default::default()
        },
    );
    {
        let pending = lock_unpoison(&manager.pending_job_updates);
        let queue = pending.get("activity-backpressure").unwrap();
        assert_eq!(queue.required.len(), 1);
        assert!(queue.output_only.is_none());
        assert!(!queue.suspended_until_reconciliation);
    }

    for index in 0..64 {
        manager.update_and_send(
            "activity-backpressure",
            RunnerJobDelta {
                status: "running".into(),
                activity: Some(if index % 2 == 0 { checking } else { compiling }),
                ..Default::default()
            },
        );
    }
    {
        let pending = lock_unpoison(&manager.pending_job_updates);
        let queue = pending.get("activity-backpressure").unwrap();
        assert_eq!(queue.required.len(), 1);
        assert_eq!(
            queue.output_only.as_ref().unwrap().activity,
            Some(compiling)
        );
        assert!(!queue.suspended_until_reconciliation);
    }
    assert_eq!(
        manager
            .inventory()
            .jobs
            .into_iter()
            .find(|job| job.job_id == "activity-backpressure")
            .unwrap()
            .activity,
        Some(compiling)
    );

    manager.update_and_send(
        "activity-backpressure",
        RunnerJobDelta {
            status: "completed".into(),
            exit_code: Some(0),
            validation_progress: Some(ShellJobValidationProgress {
                completed: 2,
                current_step: None,
                failed_step: None,
            }),
            finished: true,
            ..Default::default()
        },
    );
    {
        let pending = lock_unpoison(&manager.pending_job_updates);
        let queue = pending.get("activity-backpressure").unwrap();
        assert_eq!(queue.required.len(), 2);
        assert!(queue.output_only.is_none());
        assert!(!queue.suspended_until_reconciliation);
    }
    let terminal = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|job| job.job_id == "activity-backpressure")
        .unwrap();
    assert_eq!(terminal.status, "completed");
    assert_eq!(terminal.activity, None);

    assert!(matches!(rx.try_recv(), Ok(RunnerEnvelope::Ping { ts: 11 })));
    let updates = collect_job_updates(&mut rx, Duration::from_secs(5));
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert_eq!(
        updates[0].validation_progress,
        Some(ShellJobValidationProgress {
            completed: 1,
            current_step: Some("test".into()),
            failed_step: None,
        })
    );
    assert_eq!(updates[0].activity, Some(validation_step_activity(&test)));
    assert_eq!(updates[1].status, "completed");
    assert_eq!(updates[1].activity, None);
    assert!(updates[1].finished);
}

#[cfg(unix)]
#[test]
fn noisy_validation_progress_delivery_stays_ordered_after_transport_backpressure() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let helper = structured_process_helper();
    let steps_log = temp.path().join("validation-steps.log");
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> {}\nexec {} chatty 96\n",
            sh_quote(&steps_log),
            sh_quote(&helper.path)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o700)).unwrap();

    let steps = vec![
        ShellJobValidationStep {
            name: "format".into(),
            program: "cargo".into(),
            args: vec!["fmt".into(), "--".into(), "--check".into()],
            env: Vec::new(),
        },
        ShellJobValidationStep {
            name: "check".into(),
            program: "cargo".into(),
            args: vec!["check".into(), "--all-targets".into()],
            env: Vec::new(),
        },
        ShellJobValidationStep {
            name: "test".into(),
            program: "cargo".into(),
            args: vec!["test".into()],
            env: Vec::new(),
        },
    ];
    let mut shell = ShellConfig::default();
    shell.path_prepend.push(bin);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(RunnerEnvelope::Ping { ts: 9 }).unwrap();
    let sink = RunnerSink::WebSocket {
        tx,
        client_id: "validation-agent".into(),
        runner_instance_id: "validation-instance".into(),
    };
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            shell,
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": "validation-backpressure-request",
                "client_id": "validation-agent",
                "kind": "start_validation_job",
                "job_id": "validation-backpressure-job",
                "cwd": temp.path(),
                "command": serde_json::to_string(&steps).unwrap(),
                "timeout_secs": 60,
                "requested_by": "test",
                "created_at": chrono::Utc::now().timestamp(),
                "job_context": test_job_context(
                    temp.path(),
                    steps.iter().map(|step| step.name.clone()).collect(),
                )
            }))
            .unwrap(),
        ),
    );

    assert!(
        wait_until(Duration::from_secs(30), || {
            manager.inventory().jobs.iter().any(|snapshot| {
                snapshot.job_id == "validation-backpressure-job" && snapshot.status == "completed"
            })
        }),
        "validation job did not finish locally while transport remained full"
    );
    wait_for_job_workers(&manager);
    assert_eq!(
        std::fs::read_to_string(&steps_log)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        vec!["fmt", "check", "test"]
    );
    {
        let pending = lock_unpoison(&manager.pending_job_updates);
        let queue = pending
            .get("validation-backpressure-job")
            .expect("validation semantic updates remain pending");
        assert_eq!(queue.required.len(), 4);
        assert!(queue.required.len() <= JOB_UPDATE_REQUIRED_PENDING_MAX);
        assert!(queue.output_only.is_none());
    }

    assert!(matches!(rx.try_recv(), Ok(RunnerEnvelope::Ping { ts: 9 })));
    let updates = collect_job_updates(&mut rx, Duration::from_secs(10));
    assert_eq!(updates.len(), 4, "{updates:?}");
    let expected_steps = ["format", "check", "test"];
    let mut previous_completed = 0usize;
    let mut previous_sequence = 0u64;
    let mut previous_stdout_cursor = 0usize;
    for update in &updates {
        let sequence = update.update_seq.expect("sequenced validation update");
        assert!(sequence > previous_sequence, "{updates:?}");
        previous_sequence = sequence;
        let progress = update
            .validation_progress
            .as_ref()
            .expect("validation progress");
        assert!(progress.completed >= previous_completed, "{updates:?}");
        assert!(
            progress.completed <= previous_completed.saturating_add(1),
            "server live validation progress would reject a skipped step: {updates:?}"
        );
        previous_completed = progress.completed;
        if update.finished {
            assert_eq!(progress.completed, expected_steps.len());
            assert!(progress.current_step.is_none());
            assert_eq!(update.activity, None);
        } else {
            assert_eq!(
                progress.current_step.as_deref(),
                expected_steps.get(progress.completed).copied()
            );
            assert_eq!(
                update.activity,
                Some(validation_step_activity(&steps[progress.completed]))
            );
        }
        let stdout_cursor = update
            .log_snapshot
            .as_ref()
            .expect("authoritative logs")
            .stdout
            .next_line;
        assert!(stdout_cursor >= previous_stdout_cursor);
        previous_stdout_cursor = stdout_cursor;
    }
    let final_update = updates.last().unwrap();
    assert_eq!(final_update.status, "completed");
    assert_eq!(final_update.exit_code, Some(0));
    assert!(final_update.finished);
}

#[cfg(unix)]
#[test]
fn validation_job_progress_is_executor_owned_and_fail_fast() {
    // Spawning a step can fail for reasons that belong to the machine rather
    // than to the state machine — `fork` returning EAGAIN under a loaded test
    // suite, or ETXTBSY on a script written moments earlier. The executor
    // reports that as `validation_step_spawn_failed` with no exit code, which
    // is the correct answer to a question this test is not asking. Retrying
    // those attempts is what keeps a busy machine from deciding whether the
    // fail-fast contract holds; asserting on the first attempt is not.
    const ATTEMPTS: usize = 3;
    let mut spawn_failures = Vec::new();

    for attempt in 0..ATTEMPTS {
        let FailFastAttempt {
            updates,
            test_step_ran,
        } = run_fail_fast_validation_job(attempt);
        let final_update = updates.last().unwrap_or_else(|| {
            panic!("attempt {attempt}: validation job emitted no updates before the deadline")
        });
        assert!(
            final_update.finished,
            "attempt {attempt}: job never finished; last update was {}",
            describe_update(final_update)
        );
        if is_validation_spawn_failure(final_update) {
            spawn_failures.push(format!(
                "attempt {attempt}: {}",
                describe_update(final_update)
            ));
            continue;
        }

        assert_eq!(
            final_update.status,
            "failed",
            "attempt {attempt}: {}",
            describe_update(final_update)
        );
        assert_eq!(
            final_update.exit_code,
            Some(7),
            "attempt {attempt}: the failing step's exit code must reach the final update: {}",
            describe_update(final_update)
        );
        assert_eq!(
            final_update.validation_progress,
            Some(ShellJobValidationProgress {
                completed: 1,
                current_step: None,
                failed_step: Some("check".into()),
            }),
            "attempt {attempt}: {}",
            describe_update(final_update)
        );
        assert!(
            updates.iter().any(|update| {
                update.validation_progress
                    == Some(ShellJobValidationProgress {
                        completed: 1,
                        current_step: Some("check".into()),
                        failed_step: None,
                    })
            }),
            "attempt {attempt}: no update announced 'check' as the running step; saw {:?}",
            updates.iter().map(describe_update).collect::<Vec<_>>()
        );
        assert!(
            !test_step_ran,
            "attempt {attempt}: the plan ran 'test' after 'check' failed"
        );
        return;
    }

    panic!(
        "every attempt failed to spawn a validation step, so the fail-fast path \
         was never exercised:\n{}",
        spawn_failures.join("\n")
    );
}

#[cfg(unix)]
#[test]
fn validation_spawn_failure_is_infrastructure_without_failed_assertion() {
    let temp = tempfile::tempdir().unwrap();
    let mut shell = ShellConfig::default();
    shell.env.insert(
        "PATH".to_string(),
        temp.path().to_string_lossy().into_owned(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let sink = RunnerSink::WebSocket {
        tx,
        client_id: "validation-agent".into(),
        runner_instance_id: "validation-instance".into(),
    };
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                // These tests run jobs in a temp dir; the boundary itself is
                // covered separately, and RunnerPolicy::default() is fail-closed.
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            shell,
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": "spawn-failure-request",
                "client_id": "validation-agent",
                "kind": "start_validation_job",
                "job_id": "spawn-failure-job",
                "cwd": temp.path(),
                "command": serde_json::to_string(&[ShellJobValidationStep {
                    name: "check".into(),
                    program: "cargo".into(),
                    args: vec!["check".into(), "--all-targets".into()],
                env: Vec::new(),
                }]).unwrap(),
                "timeout_secs": 10,
                "requested_by": "test",
                "created_at": 1,
                "job_context": test_job_context(temp.path(), vec!["check".to_string()])
            }))
            .unwrap(),
        ),
    );
    let update = collect_job_updates(&mut rx, Duration::from_secs(5))
        .into_iter()
        .find(|update| update.finished)
        .expect("validation spawn failure update");
    assert!(update.finished);
    assert_eq!(update.status, "failed");
    assert_eq!(update.exit_code, None);
    assert_eq!(
        update.error.as_deref(),
        Some(VALIDATION_STEP_SPAWN_FAILED_CODE)
    );
    assert_eq!(
        update.validation_progress,
        Some(ShellJobValidationProgress {
            completed: 0,
            current_step: None,
            failed_step: None,
        })
    );
}

#[cfg(unix)]
#[test]
fn python_module_probe_reports_tool_unavailable_without_running_recipe() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let python = temp.path().join("python");
    let probe_output = temp.path().join("module");
    std::fs::write(
        &python,
        "#!/bin/sh\nprintf '%s' \"$4\" > \"$PROBE_OUTPUT\"\nexit 42\n",
    )
    .unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut shell = ShellConfig::default();
    shell.env.insert(
        "PATH".to_string(),
        temp.path().to_string_lossy().into_owned(),
    );
    shell.env.insert(
        "PROBE_OUTPUT".to_string(),
        probe_output.to_string_lossy().into_owned(),
    );
    let step = ShellJobValidationStep {
        name: "test".into(),
        program: "python".into(),
        args: ["-B", "-m", "unittest", "discover", "-v"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        env: Vec::new(),
    };
    assert!(!validation_module_available(
        &shell,
        None,
        temp.path(),
        &step,
        None,
    ));
    assert_eq!(std::fs::read_to_string(&probe_output).unwrap(), "unittest");
    assert!(!temp.path().join("recipe-ran").exists());
}

/// Platform-native liveness probe for job descendants, so the tree tests never
/// shell out to `tasklist`/`ps`/PowerShell.
#[cfg(windows)]
pub(super) fn process_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: OpenProcess returns a handle or NULL; NULL means the pid no
    // longer exists (or is inaccessible, which also means not ours).
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0u32;
    // SAFETY: `handle` is valid; `exit_code` is a valid out-param.
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    // SAFETY: close the handle we opened.
    unsafe { CloseHandle(handle) };
    ok == 1 && exit_code == 259 // 259 == STILL_ACTIVE
}

#[cfg(target_os = "macos")]
pub(super) fn process_running(pid: u32) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as libc::c_int,
        )
    };
    if bytes == size as libc::c_int {
        let info = unsafe { info.assume_init() };
        return info.pbi_status != libc::SZOMB;
    }
    !(bytes == 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(super) fn process_running(pid: u32) -> bool {
    // SAFETY: signal 0 is an existence probe; the pid comes from our own
    // helper subprocess.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(target_os = "linux")]
pub(super) fn process_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .is_some_and(|state| state != 'Z')
}

/// Poll `condition` until it holds or `timeout` elapses. The fixtures here
/// wait on marker files written by freshly spawned helper processes; under a
/// fully parallel `cargo test` on the small GitHub CI runners, process
/// startup can take well over a few seconds, so callers pass a generous 30s
/// budget (matching the helper's own 30s gate).
fn wait_until(timeout: Duration, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    condition()
}

fn wait_for_pid_marker(path: &Path, deadline: Instant, tag: &str) -> u32 {
    loop {
        let observed = std::fs::read_to_string(path)
            .map_err(|error| format!("read failed: {error}"))
            .and_then(|text| {
                text.trim()
                    .parse::<u32>()
                    .map_err(|error| format!("invalid PID contents {text:?}: {error}"))
            });
        match observed {
            Ok(pid) => return pid,
            Err(error) => {
                let now = Instant::now();
                if now >= deadline {
                    panic!(
                        "timed out waiting for {tag} PID marker {}: {error}",
                        path.display()
                    );
                }
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(10)),
                );
            }
        }
    }
}

/// Poll `process_running(pid)` until the process is gone or `timeout` elapses.
fn wait_for_process_exit(pid: u32, timeout: Duration, tag: &str) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_running(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!("wait_for_process_exit({tag}): pid {pid} still running");
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// JobManager process-tree fixtures and tests
//
// The job tree tests run the real `process_tree_helper` binary from
// `webcodex-process` (compiled at test time with rustc, exactly like the LSP
// and MCP fake servers), so the same scenarios run on Windows and Unix without
// cmd, PowerShell, or bash. Liveness is probed with the platform-native
// `process_running` above. Every test reaps the tree it starts before
// returning, so no helper process is left behind.
// ---------------------------------------------------------------------------

/// Compiled copy of the `process_tree_helper` fixture, kept alive for the whole
/// test process so its binary path never disappears under a running grandchild.
struct JobTreeHelper {
    _temp: TempDir,
    path: PathBuf,
}

static JOB_TREE_HELPER: OnceLock<Arc<JobTreeHelper>> = OnceLock::new();

fn job_tree_helper() -> Arc<JobTreeHelper> {
    JOB_TREE_HELPER
        .get_or_init(|| {
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../webcodex-process/src/bin/process_tree_helper.rs");
            let temp = tempfile::tempdir().unwrap();
            let output = temp.path().join(format!(
                "process-tree-helper{}",
                std::env::consts::EXE_SUFFIX
            ));
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
            let result = Command::new(rustc)
                .arg("--edition=2021")
                .arg("--crate-name=webcodex_job_tree_helper")
                .arg(&source)
                .arg("-o")
                .arg(&output)
                .output()
                .expect("run rustc for job tree helper");
            assert!(
                result.status.success(),
                "job tree helper compilation failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            Arc::new(JobTreeHelper {
                _temp: temp,
                path: output,
            })
        })
        .clone()
}

/// A background line reader over a job's captured stdout. Each complete line is
/// delivered as `Line`, and a final `Eof` marks the pipe closing (which a
/// descendant holding the write end would otherwise delay indefinitely).
enum JobTreeOut {
    Line(Vec<u8>),
    Eof,
}

/// Spawn the helper in `mode`, capturing its stdout and piping it through a
/// background line reader. The helper's descendants inherit the stdout write
/// end, so `Eof` only arrives once the whole tree is gone.
fn spawn_helper_raw(mode: &str, args: &[&str]) -> (ManagedChild, mpsc::Receiver<JobTreeOut>) {
    let helper = job_tree_helper();
    let mut cmd = Command::new(&helper.path);
    cmd.arg(mode).args(args);
    cmd.stdout(Stdio::piped());
    let mut managed = ManagedChild::spawn(&mut cmd).expect("spawn job tree helper");
    let (tx, rx) = mpsc::sync_channel(64);
    let stdout = managed
        .child_mut()
        .stdout
        .take()
        .expect("job tree helper piped stdout");
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => {
                    if !line.is_empty() {
                        let _ = tx.send(JobTreeOut::Line(std::mem::take(&mut line)));
                    }
                    let _ = tx.send(JobTreeOut::Eof);
                    return;
                }
                Ok(1) => {
                    line.push(byte[0]);
                    if line.ends_with(b"\n") {
                        let _ = tx.send(JobTreeOut::Line(std::mem::take(&mut line)));
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = tx.send(JobTreeOut::Eof);
                    return;
                }
            }
        }
    });
    (managed, rx)
}

fn read_grandchild_pid(rx: &mpsc::Receiver<JobTreeOut>) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out reading GRANDCHILD_PID from job stdout");
        }
        match rx.recv_timeout(remaining.min(Duration::from_secs(1))) {
            Ok(JobTreeOut::Line(line)) => {
                let text = String::from_utf8_lossy(&line);
                if let Some(value) = text.trim().strip_prefix("GRANDCHILD_PID=") {
                    return value.trim().parse().expect("grandchild pid number");
                }
            }
            Ok(JobTreeOut::Eof) => panic!("job stdout reached EOF before GRANDCHILD_PID"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("job stdout reader disconnected before GRANDCHILD_PID")
            }
        }
    }
}

fn wait_for_stdout_eof(rx: &mpsc::Receiver<JobTreeOut>, timeout: Duration, tag: &str) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!("wait_for_stdout_eof({tag}): no EOF within {timeout:?}");
            return false;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(JobTreeOut::Eof) => return true,
            Ok(JobTreeOut::Line(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
    }
}

fn extract_grandchild_pid(text: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("GRANDCHILD_PID=")?
            .trim()
            .parse::<u32>()
            .ok()
    })
}

fn insert_running_job(
    manager: &JobManager,
    job_id: &str,
    child: Option<Arc<Mutex<ManagedChild>>>,
) -> Arc<AtomicBool> {
    let stop_requested = Arc::new(AtomicBool::new(false));
    lock_unpoison(&manager.jobs).insert(
        job_id.to_string(),
        RunningJob {
            client_id: "tree-agent".to_string(),
            runner_instance_id: "tree-instance".to_string(),
            snapshot: test_job_snapshot(job_id),
            child,
            stop_requested: Arc::clone(&stop_requested),
            slot_reserved: true,
        },
    );
    stop_requested
}

struct RunningJobTreeFixture {
    parent_pid: u32,
    grandchild_pid: u32,
    output: mpsc::Receiver<JobTreeOut>,
}

impl RunningJobTreeFixture {
    fn assert_terminated(&self, timeout: Duration, tag: &str) {
        assert!(
            wait_for_process_exit(self.parent_pid, timeout, &format!("{tag}-parent")),
            "{tag} parent survived tree termination"
        );
        assert!(
            wait_for_process_exit(self.grandchild_pid, timeout, &format!("{tag}-grandchild")),
            "{tag} descendant survived tree termination"
        );
        assert!(
            wait_for_stdout_eof(&self.output, timeout, &format!("{tag}-eof")),
            "{tag} stdout did not reach EOF after tree termination"
        );
    }
}

fn seed_running_job_tree(
    manager: &JobManager,
    job_id: &str,
    marker: &Path,
) -> (RunningJobTreeFixture, Arc<AtomicBool>) {
    let (managed, output) = spawn_helper_raw(
        "spawn-grandchild-keepalive",
        &[marker.to_str().unwrap(), "3", "60", "60"],
    );
    let parent_pid = managed.id();
    let grandchild_pid = read_grandchild_pid(&output);
    let child = Arc::new(Mutex::new(managed));
    let stop_requested = insert_running_job(manager, job_id, Some(child));
    (
        RunningJobTreeFixture {
            parent_pid,
            grandchild_pid,
            output,
        },
        stop_requested,
    )
}

/// An explicit stop terminates the whole job process tree, including a
/// descendant that inherited the stdout pipe, and the stdout reader reaches
/// EOF instead of blocking forever.
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_job_stop_terminates_whole_tree_including_descendant() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("stop-grandchild.marker");
    let manager = JobManager::new(1);
    let (tree, stop_requested) = seed_running_job_tree(&manager, "stop-tree-job", &marker);
    assert!(
        process_running(tree.parent_pid),
        "job parent should be running before stop"
    );
    assert!(
        process_running(tree.grandchild_pid),
        "job descendant should be running before stop"
    );
    manager.stop("stop-tree-job").expect("stop job");
    assert!(stop_requested.load(Ordering::SeqCst));

    tree.assert_terminated(Duration::from_secs(5), "explicit-stop");
    assert!(
        !marker.exists(),
        "delayed grandchild marker must never appear after stop"
    );
}

/// The job worker's cleanup sequence (bounded tree wait, force terminate, then
/// reader join) must kill an orphaned descendant that keeps the stdout pipe
/// open, and the output reader must reach EOF instead of being detached.
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_job_cleanup_after_parent_exit_terminates_descendant_and_reaches_eof() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("orphan.marker");
    let helper = job_tree_helper();
    let mut cmd = Command::new(&helper.path);
    cmd.arg("spawn-grandchild")
        .arg(marker.to_str().unwrap())
        .arg("3")
        .arg("60");
    cmd.stdout(Stdio::piped());
    let mut managed = ManagedChild::spawn(&mut cmd).expect("spawn job tree helper");
    let (tx, rx) = mpsc::sync_channel::<OutputChunk>(64);
    let stdout = managed.child_mut().stdout.take().expect("piped stdout");
    let readers = vec![spawn_reader(
        stdout,
        tx.clone(),
        true,
        OutputTextSource::LocalProcess,
    )];
    drop(tx);
    let child = Arc::new(Mutex::new(managed));

    // The direct child exits on its own right after spawning the grandchild;
    // collect its pid line from the production reader's chunk stream.
    let mut accumulated = String::new();
    let read_deadline = Instant::now() + Duration::from_secs(5);
    let grandchild_pid = loop {
        let remaining = read_deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for GRANDCHILD_PID in job stdout"
        );
        match rx.recv_timeout(remaining) {
            Ok(OutputChunk::Stdout(text)) => {
                accumulated.push_str(&text);
                if let Some(pid) = extract_grandchild_pid(&accumulated) {
                    break pid;
                }
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for GRANDCHILD_PID in job stdout");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("job stdout disconnected before GRANDCHILD_PID was observed");
            }
        }
    };
    assert!(
        wait_until(Duration::from_secs(30), || lock_unpoison(&child)
            .try_wait()
            .unwrap()
            .is_some()),
        "job parent should exit on its own"
    );
    assert!(
        process_running(grandchild_pid),
        "grandchild must still be alive after the parent exits"
    );

    // This is exactly what the job worker runs after the direct child's status
    // is decided.
    cleanup_managed_tree(&child);
    let detached = join_reader_threads_until(readers, Instant::now() + Duration::from_secs(1));
    assert_eq!(
        detached, 0,
        "stdout reader must finish on EOF instead of being detached"
    );
    assert!(
        wait_for_process_exit(grandchild_pid, Duration::from_secs(5), "orphan-grandchild"),
        "orphaned grandchild survived tree cleanup"
    );
    assert!(
        !marker.exists(),
        "delayed grandchild marker must never appear after cleanup"
    );
}

/// A shutdown drain terminates every running job's whole tree, leaves a
/// completed job untouched, and is bounded.
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_job_stop_all_terminates_all_trees_and_preserves_completed_jobs() {
    let manager = JobManager::new(2);
    let completed_stop = Arc::new(AtomicBool::new(false));
    {
        let mut snapshot = test_job_snapshot("completed-job");
        snapshot.status = "completed".to_string();
        lock_unpoison(&manager.jobs).insert(
            "completed-job".to_string(),
            RunningJob {
                client_id: "tree-agent".to_string(),
                runner_instance_id: "tree-instance".to_string(),
                snapshot,
                child: None,
                stop_requested: Arc::clone(&completed_stop),
                slot_reserved: false,
            },
        );
    }

    let temp = tempfile::tempdir().unwrap();
    let marker_a = temp.path().join("stop-all-a.marker");
    let marker_b = temp.path().join("stop-all-b.marker");
    let (tree_a, _) = seed_running_job_tree(&manager, "running-a", &marker_a);
    let (tree_b, _) = seed_running_job_tree(&manager, "running-b", &marker_b);

    manager.stop_all();

    let completed = lock_unpoison(&manager.jobs)
        .get("completed-job")
        .cloned()
        .unwrap();
    assert_eq!(completed.snapshot.status, "completed");
    assert!(
        !completed_stop.load(Ordering::SeqCst),
        "a terminal job must not be signalled during shutdown"
    );
    tree_a.assert_terminated(Duration::from_secs(5), "stop-all-a");
    tree_b.assert_terminated(Duration::from_secs(5), "stop-all-b");
}

/// Repeated stops are idempotent: the second stop must not panic and must not
/// leave the tree running.
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_job_stop_twice_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("twice.marker");
    let manager = JobManager::new(1);
    let (tree, _) = seed_running_job_tree(&manager, "twice-job", &marker);

    manager.stop("twice-job").expect("first stop");
    manager
        .stop("twice-job")
        .expect("second stop must be idempotent");

    tree.assert_terminated(Duration::from_secs(5), "stop-twice");
}

/// Stopping a job whose tree already exited naturally must not panic and must
/// report success.
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_job_stop_after_natural_exit_does_not_panic() {
    let (managed, _rx) = spawn_helper_raw("sleep", &["0", "0"]);
    let parent_pid = managed.id();
    let child = Arc::new(Mutex::new(managed));
    assert!(
        wait_until(Duration::from_secs(30), || lock_unpoison(&child)
            .try_wait()
            .unwrap()
            .is_some()),
        "job should exit on its own"
    );
    let manager = JobManager::new(1);
    insert_running_job(&manager, "exited-job", Some(child));
    manager.stop("exited-job").expect("stop after natural exit");
    assert!(wait_for_process_exit(
        parent_pid,
        Duration::from_secs(5),
        "exited-parent"
    ));
}

/// Dropping the last real JobManager owner must terminate an active tree even
/// while a worker clone still holds the jobs map and ManagedChild Arc.
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_last_job_manager_owner_drop_terminates_running_tree_with_worker_clone_alive()
{
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("drop.marker");
    let manager = JobManager::new(1);
    let second_owner = manager.clone();
    let worker_manager = manager.clone_for_worker();
    let (tree, stop_requested) = seed_running_job_tree(&manager, "drop-job", &marker);

    drop(manager);
    assert!(process_running(tree.parent_pid) && process_running(tree.grandchild_pid));
    assert!(!stop_requested.load(Ordering::SeqCst));

    drop(second_owner);
    assert!(worker_manager.shutting_down.load(Ordering::SeqCst));
    assert!(stop_requested.load(Ordering::SeqCst));
    tree.assert_terminated(Duration::from_secs(5), "last-owner-drop");
    drop(worker_manager);
}

/// Cleanup on an already-exited tree must be a no-op that never panics.
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_cleanup_managed_tree_on_exited_tree_does_not_panic() {
    let (managed, _rx) = spawn_helper_raw("sleep", &["0", "0"]);
    let child = Arc::new(Mutex::new(managed));
    assert!(wait_until(Duration::from_secs(30), || lock_unpoison(
        &child
    )
    .try_wait()
    .unwrap()
    .is_some()));
    cleanup_managed_tree(&child);
    assert!(
        lock_unpoison(&child).try_tree_exit().unwrap(),
        "cleanup must leave an already-exited tree confirmed empty"
    );
}

/// A user stop racing Runner shutdown must not deadlock or panic, and both
/// paths must converge on a fully-terminated tree.
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_job_stop_racing_shutdown_does_not_panic() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("race.marker");
    let manager = JobManager::new(1);
    let (tree, _) = seed_running_job_tree(&manager, "race-job", &marker);

    let stop_manager = manager.clone();
    let stopper = std::thread::spawn(move || {
        let _ = stop_manager.stop("race-job");
    });
    manager.stop_accepting_work();
    let batch = manager.signal_all_for_shutdown();
    let outcome = manager.drain_shutdown(batch, Instant::now() + Duration::from_secs(2));
    stopper.join().expect("stop thread must not panic");
    assert!(outcome.resources >= 1);

    tree.assert_terminated(Duration::from_secs(5), "stop-shutdown-race");
}

/// A job timeout must terminate the whole tree (parent shell, helper, and the
/// helper's descendant) and publish exactly one `timeout` completion. Requires
/// a real `sh` for the full worker path, so it runs on Linux.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "runner real-process lane: spawns the JobManager process-tree fixture"]
fn runner_real_process_job_timeout_terminates_the_whole_tree() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("timeout-grandchild.marker");
    let helper = job_tree_helper();
    // `sh -c '<helper> spawn-grandchild-keepalive <marker> 3 60 60'` so the
    // job's direct child spawns a grandchild and keeps running until timeout.
    let command = format!(
        "{} spawn-grandchild-keepalive {} 3 60 60",
        helper.path.display(),
        marker.display()
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let sink = RunnerSink::WebSocket {
        tx,
        client_id: "timeout-agent".into(),
        runner_instance_id: "timeout-instance".into(),
    };
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        PendingJobStart::from_wire(
            1,
            RunnerPolicy {
                allow_cwd_anywhere: true,
                ..RunnerPolicy::default()
            },
            ShellConfig::default(),
            SshConfig::default(),
            temp.path().join("project-registry"),
            serde_json::from_value(json!({
                "request_id": "timeout-request",
                "client_id": "timeout-agent",
                "kind": "start_job",
                "job_id": "timeout-job",
                "cwd": temp.path(),
                "command": command,
                "timeout_secs": 1,
                "requested_by": "test",
                "created_at": 1,
                "job_context": test_job_context(temp.path(), Vec::new())
            }))
            .unwrap(),
        ),
    );

    let updates = collect_job_updates(&mut rx, Duration::from_secs(20));
    let final_update = updates.last().expect("timeout job should finish");
    assert!(final_update.finished, "{}", describe_update(final_update));
    assert_eq!(final_update.status, "timeout", "{final_update:?}");
    assert_eq!(final_update.exit_code, Some(-1), "{final_update:?}");
    assert!(
        final_update
            .error
            .as_deref()
            .is_some_and(|error| error.contains("timed out")),
        "{final_update:?}"
    );
    assert_eq!(
        updates.iter().filter(|update| update.finished).count(),
        1,
        "timeout must produce exactly one finished update"
    );

    // The helper's captured stdout carries the grandchild pid, which must be
    // gone after the timeout terminates the whole tree.
    let stdout_tail = final_update
        .log_snapshot
        .as_ref()
        .map(|log| log.stdout.tail.clone())
        .unwrap_or_default();
    let grandchild_pid =
        extract_grandchild_pid(&stdout_tail).expect("GRANDCHILD_PID in job stdout");
    assert!(
        wait_for_process_exit(grandchild_pid, Duration::from_secs(5), "timeout-grandchild"),
        "grandchild survived job timeout"
    );
    assert!(
        !marker.exists(),
        "delayed grandchild marker must never appear after timeout"
    );
}
