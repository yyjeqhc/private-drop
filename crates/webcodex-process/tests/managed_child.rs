//! Integration tests for `webcodex-process`.
//!
//! These tests run the real `process_tree_helper` binary on both Windows and
//! Unix. Liveness is probed via platform-native APIs (OpenProcess +
//! GetExitCodeProcess on Windows, `kill(pid, 0)` on Unix) rather than by
//! shelling out to `tasklist` / `ps`, so the tests are self-contained.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use webcodex_process::{GracefulTermination, ManagedChild, SpawnOptions};

/// Path to the compiled helper binary, provided by Cargo for integration tests.
fn helper() -> &'static str {
    env!("CARGO_BIN_EXE_process_tree_helper")
}

/// Reads newline-delimited lines from a pipe on a background thread so tests
/// never block indefinitely. The thread exits when the pipe hits EOF.
struct LineReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl LineReader {
    fn new<R: Read + Send + 'static>(mut reader: R) -> Self {
        let (tx, rx) = channel();
        let join = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            loop {
                let mut line = Vec::new();
                loop {
                    match reader.read(&mut byte) {
                        Ok(0) => {
                            // EOF. Flush any partial trailing line, then stop.
                            if !line.is_empty() {
                                let _ = tx.send(line);
                            }
                            return;
                        }
                        Ok(1) => {
                            line.push(byte[0]);
                            if line.ends_with(b"\n") {
                                break;
                            }
                        }
                        Ok(_) => {} // cannot happen for a single-byte buffer
                        Err(_) => return,
                    }
                }
                let _ = tx.send(line);
            }
        });
        Self {
            rx,
            join: Some(join),
        }
    }

    fn recv_line(&self, timeout: Duration) -> Result<Vec<u8>, String> {
        match self.rx.recv_timeout(timeout) {
            Ok(line) => Ok(line),
            Err(RecvTimeoutError::Timeout) => Err("timed out reading line".to_string()),
            Err(RecvTimeoutError::Disconnected) => {
                Err("reader hit EOF before the expected line".to_string())
            }
        }
    }

    fn assert_no_eof(&self, timeout: Duration) -> Result<(), String> {
        match self.rx.recv_timeout(timeout) {
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                Err("stdout reached EOF while a grandchild still owned the pipe".to_string())
            }
            Ok(line) => Err(format!(
                "unexpected additional stdout before termination: {}",
                String::from_utf8_lossy(&line)
            )),
        }
    }

    fn wait_for_eof(mut self, timeout: Duration) -> Result<(), String> {
        match self.rx.recv_timeout(timeout) {
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(join) = self.join.take() {
                    join.join()
                        .map_err(|_| "stdout reader thread panicked".to_string())?;
                }
                Ok(())
            }
            Err(RecvTimeoutError::Timeout) => Err("timed out waiting for stdout EOF".to_string()),
            Ok(line) => Err(format!(
                "unexpected additional stdout while waiting for EOF: {}",
                String::from_utf8_lossy(&line)
            )),
        }
    }
}

/// A unique temp path for a marker or pid file, removed on drop.
fn unique_temp_path(tag: &str) -> CleanupPath {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("wc-proc-{tag}-{}-{nanos}", std::process::id()));
    CleanupPath(path)
}

struct CleanupPath(PathBuf);

impl Drop for CleanupPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl std::ops::Deref for CleanupPath {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

/// Poll `alive(pid)` until it flips to the target value or `timeout` elapses.
fn wait_for_liveness(pid: u32, should_be_alive: bool, timeout: Duration, tag: &str) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let alive = process_alive(pid);
        if alive == should_be_alive {
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "wait_for_liveness({tag}): pid {pid} still alive={alive}, wanted alive={should_be_alive}"
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_until_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "manual real-process lifecycle: waits for parent-disappearance EOF"]
fn inherited_stdin_lease_detects_parent_process_disappearance() {
    let marker = unique_temp_path("parent-lease-eof");
    let mut parent = Command::new(helper());
    parent
        .args(["spawn-parent-lease-child", marker.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut parent = parent.spawn().expect("spawn parent-lease owner");
    let status = parent.wait().expect("wait parent-lease owner");
    assert!(status.success(), "fixture parent should exit cleanly");
    assert!(
        wait_until_file(&marker, Duration::from_secs(5)),
        "child did not observe stdin EOF after its exact owner process disappeared"
    );
}

/// Spawn the helper in `mode`. When `capture_stdout` is set, returns a
/// [`LineReader`] fed from the child's piped stdout.
fn spawn_helper(
    mode: &str,
    args: &[&str],
    capture_stdout: bool,
) -> (ManagedChild, Option<LineReader>) {
    let mut cmd = Command::new(helper());
    cmd.arg(mode).args(args);
    if capture_stdout {
        cmd.stdout(Stdio::piped());
    }
    let mut managed = ManagedChild::spawn(&mut cmd).expect("ManagedChild::spawn");
    let reader = if capture_stdout {
        let stdout = managed
            .child_mut()
            .stdout
            .take()
            .expect("captured stdout pipe");
        Some(LineReader::new(BufReader::new(stdout)))
    } else {
        None
    };
    (managed, reader)
}

fn parse_pid(line: &[u8], prefix: &str) -> u32 {
    let text = String::from_utf8_lossy(line);
    let value = text
        .trim()
        .strip_prefix(prefix)
        .and_then(|s| s.strip_prefix('='))
        .expect("pid line prefix");
    value.trim().parse().expect("pid number")
}

/// Spawn a `spawn-grandchild` helper and return the managed child, grandchild
/// pid, and a live reader for the inherited stdout pipe.
fn spawn_tree_with_grandchild(marker: &Path) -> (ManagedChild, u32, LineReader) {
    let (managed, reader) = spawn_helper(
        "spawn-grandchild",
        &[marker.to_str().unwrap(), "3", "60"],
        true,
    );
    let reader = reader.expect("line reader");
    let gc_line = reader
        .recv_line(Duration::from_secs(5))
        .expect("grandchild pid line");
    let gc_pid = parse_pid(&gc_line, "GRANDCHILD_PID");
    (managed, gc_pid, reader)
}

// ---------------------------------------------------------------------------
// 1. Normal completion
// ---------------------------------------------------------------------------

#[test]
#[ignore = "manual real-process lifecycle: waits for a real child and tree exit"]
fn normal_completion() {
    let (mut managed, _) = spawn_helper("sleep", &["1", "7"], false);
    let status = managed.wait().expect("wait direct child");
    assert_eq!(
        status.code(),
        Some(7),
        "direct child should exit with code 7"
    );
    assert!(
        managed
            .wait_tree_exit(Duration::from_secs(10))
            .expect("wait tree"),
        "tree should be empty after the only child exits"
    );
}

// ---------------------------------------------------------------------------
// 2. Explicit terminate of the whole tree
// ---------------------------------------------------------------------------

#[test]
#[ignore = "manual real-process lifecycle: validates process-tree termination and liveness"]
fn explicit_terminate_kills_tree() {
    let marker = unique_temp_path("explicit-term");
    let (mut managed, gc_pid, reader) = spawn_tree_with_grandchild(&marker);

    // The marker has a 3s delay; it must not exist shortly after spawn.
    assert!(
        !wait_until_file(&marker, Duration::from_millis(500)),
        "marker should not appear before its delay"
    );

    // The direct child exits immediately after spawning the grandchild.
    let status = managed.wait().expect("wait direct child");
    assert!(status.success(), "direct child should exit 0");
    assert!(
        process_alive(gc_pid),
        "grandchild {gc_pid} should still be alive after direct child exits"
    );

    managed.terminate_tree().expect("terminate tree");
    assert!(
        wait_for_liveness(gc_pid, false, Duration::from_secs(10), "gc-after-terminate"),
        "grandchild survived terminate_tree"
    );
    assert!(
        managed
            .wait_tree_exit(Duration::from_secs(10))
            .expect("wait tree empty"),
        "tree should be empty after terminate_tree"
    );
    reader
        .wait_for_eof(Duration::from_secs(10))
        .expect("stdout should close after explicit terminate");
    assert!(
        !marker.exists(),
        "delayed marker must never appear after explicit terminate"
    );
}

// ---------------------------------------------------------------------------
// 3. Drop kills the whole tree
// ---------------------------------------------------------------------------

#[test]
#[ignore = "manual real-process lifecycle: validates drop-driven process-tree teardown"]
fn drop_kills_tree() {
    let marker = unique_temp_path("drop-kill");
    let (mut managed, gc_pid, reader) = spawn_tree_with_grandchild(&marker);

    let _ = managed.wait().expect("wait direct child");
    assert!(
        process_alive(gc_pid),
        "grandchild should be alive before drop"
    );
    drop(managed);
    reader
        .wait_for_eof(Duration::from_secs(10))
        .expect("stdout should close after drop");
    assert!(
        wait_for_liveness(gc_pid, false, Duration::from_secs(10), "gc-after-drop"),
        "grandchild survived ManagedChild::drop"
    );
    assert!(
        !marker.exists(),
        "delayed marker must never appear after drop"
    );
}

// ---------------------------------------------------------------------------
// 4. Direct child exits before the grandchild
// ---------------------------------------------------------------------------

#[test]
#[ignore = "manual real-process lifecycle: depends on direct-child/grandchild scheduling"]
fn direct_child_exits_before_grandchild() {
    let marker = unique_temp_path("direct-before-gc");
    let (mut managed, gc_pid, reader) = spawn_tree_with_grandchild(&marker);

    // The direct child exits quickly on its own.
    let status = managed.wait().expect("wait direct child");
    assert!(status.success(), "direct child should exit 0");

    // wait() returned but the tree is still alive: this is the crux of the
    // "wait direct child != wait whole tree" semantic.
    assert!(
        process_alive(gc_pid),
        "grandchild must still be alive after direct child wait()"
    );
    assert!(
        !managed
            .wait_tree_exit(Duration::from_millis(200))
            .expect("wait tree"),
        "wait_tree_exit must report the tree as NOT empty while the grandchild runs"
    );

    // Terminate explicitly and confirm the tree empties.
    managed.terminate_tree().expect("terminate tree");
    assert!(
        wait_for_liveness(gc_pid, false, Duration::from_secs(10), "gc-final"),
        "grandchild survived explicit terminate"
    );
    assert!(
        managed
            .wait_tree_exit(Duration::from_secs(10))
            .expect("tree empty"),
        "tree should be empty after explicit terminate"
    );
    reader
        .wait_for_eof(Duration::from_secs(10))
        .expect("stdout should close after terminate");
    assert!(!marker.exists(), "marker must not appear (delay 3s)");
}

// ---------------------------------------------------------------------------
// 5. stdout EOF risk
// ---------------------------------------------------------------------------

#[test]
#[ignore = "manual real-process lifecycle: uses a negative EOF timing window"]
fn stdout_eof_does_not_false_trigger() {
    let marker = unique_temp_path("stdout-eof");
    let (mut managed, gc_pid, reader) = spawn_tree_with_grandchild(&marker);

    // The direct child has exited, but its grandchild inherited the stdout
    // writer. EOF must therefore not arrive while the grandchild is alive.
    let status = managed.wait().expect("wait direct child");
    assert!(status.success());
    assert!(process_alive(gc_pid), "grandchild should still be alive");
    reader
        .assert_no_eof(Duration::from_millis(300))
        .expect("stdout must remain open while the grandchild holds it");

    managed.terminate_tree().expect("terminate tree");
    reader
        .wait_for_eof(Duration::from_secs(10))
        .expect("stdout EOF should arrive after terminating the whole tree");
    assert!(managed
        .wait_tree_exit(Duration::from_secs(10))
        .expect("wait tree empty"));
    assert!(!marker.exists(), "grandchild marker must not appear");
}

#[test]
#[ignore = "manual real-process lifecycle: validates child liveness after drop"]
fn drop_kills_and_reaps_running_direct_child() {
    let (managed, _) = spawn_helper("sleep", &["60", "0"], false);
    let pid = managed.id();
    assert!(process_alive(pid), "direct child should start alive");
    drop(managed);
    assert!(
        wait_for_liveness(pid, false, Duration::from_secs(10), "direct-after-drop"),
        "running direct child survived or remained unreaped after drop"
    );
}

#[test]
fn managed_child_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ManagedChild>();
}

// ---------------------------------------------------------------------------
// 6. Repeated terminate
// ---------------------------------------------------------------------------

#[test]
#[ignore = "manual real-process lifecycle: exercises repeated process-tree teardown"]
fn repeated_terminate_is_idempotent() {
    let marker = unique_temp_path("repeat-term");
    let (mut managed, gc_pid, reader) = spawn_tree_with_grandchild(&marker);
    let _ = gc_pid;

    managed.terminate_tree().expect("first terminate");
    // Second call must not panic and should succeed (tree already gone).
    managed.terminate_tree().expect("second terminate");
    assert!(
        managed
            .wait_tree_exit(Duration::from_secs(10))
            .expect("tree empty"),
        "tree should be empty after repeated terminate"
    );
    reader
        .wait_for_eof(Duration::from_secs(10))
        .expect("stdout should close after repeated terminate");
    assert!(!marker.exists(), "marker must not appear");
}

// ---------------------------------------------------------------------------
// 7. spawn failure
// ---------------------------------------------------------------------------

#[test]
fn spawn_failure_is_clean() {
    let mut cmd = Command::new("definitely-not-a-real-executable-xyz");
    let result = ManagedChild::spawn(&mut cmd);
    let err = match result {
        Ok(_) => panic!("spawn of nonexistent executable must fail"),
        Err(e) => e,
    };
    assert!(
        err.kind() == std::io::ErrorKind::NotFound,
        "expected NotFound, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 8. SpawnOptions path
// ---------------------------------------------------------------------------

#[test]
#[ignore = "manual real-process lifecycle: waits for a real helper process"]
fn spawn_with_default_options() {
    let mut cmd = Command::new(helper());
    cmd.arg("sleep").arg("1").arg("0");
    let mut managed =
        ManagedChild::spawn_with_options(&mut cmd, SpawnOptions::default()).expect("spawn");
    let status = managed.wait().expect("wait");
    assert!(status.success(), "helper should exit 0");
}

#[cfg(windows)]
#[test]
#[ignore = "manual real-process timing: waits on Windows process scheduling"]
fn reusable_command_is_not_left_suspended() {
    let mut cmd = Command::new(helper());
    cmd.arg("sleep").arg("0").arg("0");
    let mut managed = ManagedChild::spawn(&mut cmd).expect("managed spawn");
    assert!(managed.wait().expect("managed wait").success());
    drop(managed);

    let mut reused = cmd.spawn().expect("reuse command directly");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match reused.try_wait().expect("try_wait reused child") {
            Some(status) => {
                assert!(status.success(), "reused command should exit normally");
                break;
            }
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = reused.kill();
                let _ = reused.wait();
                panic!("reused Command was left suspended by ManagedChild::spawn");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Graceful termination request
// ---------------------------------------------------------------------------

/// Unix: a graceful request terminates an ordinary process tree and can be
/// followed by a bounded tree wait.
#[cfg(unix)]
#[test]
#[ignore = "manual real-process lifecycle: validates Unix process-group termination"]
fn graceful_request_terminates_tree_and_tree_wait_completes() {
    let marker = unique_temp_path("graceful-request");
    let (mut managed, gc_pid, reader) = spawn_tree_with_grandchild(&marker);

    let _ = managed.wait().expect("wait direct child");
    assert!(
        process_alive(gc_pid),
        "grandchild should still be alive before the graceful request"
    );

    match managed
        .request_terminate_tree()
        .expect("graceful request must succeed on unix")
    {
        GracefulTermination::Requested => {}
        GracefulTermination::AlreadyExited => {
            panic!("tree unexpectedly already exited before the graceful request")
        }
        GracefulTermination::Unsupported => {
            panic!("graceful termination reported unsupported on unix")
        }
    }

    assert!(
        wait_for_liveness(gc_pid, false, Duration::from_secs(10), "gc-after-graceful"),
        "grandchild survived SIGTERM to its process group"
    );
    assert!(
        managed
            .wait_tree_exit(Duration::from_secs(10))
            .expect("bounded tree wait"),
        "tree should be empty after the graceful request"
    );
    reader
        .wait_for_eof(Duration::from_secs(10))
        .expect("stdout should close after the graceful request");
    assert!(
        !marker.exists(),
        "delayed marker must never appear after the graceful request"
    );
}

/// Windows: a graceful request returns `Unsupported` without killing anything,
/// and the child remains owned so it can subsequently be terminated with
/// `terminate_tree()`.
#[cfg(windows)]
#[test]
#[ignore = "manual real-process lifecycle: validates Windows ownership after graceful request"]
fn graceful_request_is_unsupported_and_child_stays_owned() {
    let marker = unique_temp_path("graceful-unsupported");
    let (mut managed, gc_pid, reader) = spawn_tree_with_grandchild(&marker);

    let _ = managed.wait().expect("wait direct child");
    assert!(
        process_alive(gc_pid),
        "grandchild should still be alive before the graceful request"
    );

    assert_eq!(
        managed
            .request_terminate_tree()
            .expect("graceful request must not error on windows"),
        GracefulTermination::Unsupported
    );
    assert!(
        process_alive(gc_pid),
        "graceful Unsupported must not kill anything in the tree"
    );

    // The child remains owned: force termination still works afterwards.
    managed.terminate_tree().expect("terminate tree");
    assert!(
        wait_for_liveness(gc_pid, false, Duration::from_secs(10), "gc-after-terminate"),
        "grandchild survived terminate_tree after graceful Unsupported"
    );
    assert!(
        managed
            .wait_tree_exit(Duration::from_secs(10))
            .expect("bounded tree wait"),
        "tree should be empty after terminate_tree"
    );
    reader
        .wait_for_eof(Duration::from_secs(10))
        .expect("stdout should close after terminate_tree");
    assert!(
        !marker.exists(),
        "delayed marker must never appear after terminate_tree"
    );
}

/// Repeated calls and an already-exited tree must not panic.
#[test]
#[ignore = "manual real-process lifecycle: waits for live and exited helper generations"]
fn graceful_request_repeated_and_already_exited_do_not_panic() {
    // Repeated calls on a live tree: results are defined by the platform but a
    // panic (from an unexpected Err) is the failure being tested.
    let (mut managed, _) = spawn_helper("sleep", &["1", "0"], false);
    let first = managed.request_terminate_tree();
    let second = managed.request_terminate_tree();
    // Both calls must at least succeed as an I/O result.
    let _ = first.expect("first graceful request returned an error");
    let _ = second.expect("second graceful request returned an error");

    // An already-exited tree: wait for natural exit, then request again.
    let (mut exited, _) = spawn_helper("sleep", &["0", "0"], false);
    let _ = exited.wait().expect("wait direct child");
    assert!(
        exited
            .wait_tree_exit(Duration::from_secs(10))
            .expect("bounded tree wait"),
        "tree should be empty after the only child exits"
    );
    let result = exited
        .request_terminate_tree()
        .expect("graceful request on an exited tree returned an error");
    #[cfg(unix)]
    assert_eq!(
        result,
        GracefulTermination::AlreadyExited,
        "once whole-tree exit is confirmed, Unix must not probe or signal that numeric pgid again"
    );
    #[cfg(windows)]
    let _ = result;
}

/// Once the owned Unix generation is authoritatively known empty, its numeric
/// process-group id is no longer valid kill authority. This is the stale-PID /
/// PID-reuse fence Desktop relies on by retaining the ManagedChild generation
/// rather than remembering and later targeting a pid/pgid integer.
#[cfg(unix)]
#[test]
#[ignore = "manual real-process lifecycle: validates post-exit Unix process-group authority"]
fn confirmed_generation_never_reuses_numeric_pgid_as_kill_authority() {
    let (mut managed, _) = spawn_helper("sleep", &["0", "0"], false);
    let stale_numeric_identity = managed.id();
    let _ = managed.wait().expect("wait direct child");
    assert!(managed
        .wait_tree_exit(Duration::from_secs(10))
        .expect("confirm whole-tree exit"));

    assert_eq!(
        managed
            .request_terminate_tree()
            .expect("post-exit graceful request"),
        GracefulTermination::AlreadyExited,
        "confirmed generation {stale_numeric_identity} must not probe or signal its old numeric pgid"
    );
    managed
        .terminate_tree()
        .expect("post-exit force cleanup is idempotent and must not retarget the numeric pgid");
}

/// ManagedChild must preserve the platform's normal `Command::spawn` behavior
/// for an executable text file without a shebang. Linux reports ENOEXEC while
/// macOS's standard-library spawn path executes it through the platform shell;
/// process-group ownership must not change either platform's baseline behavior.
#[cfg(unix)]
#[test]
fn spawn_preserves_platform_enoexec_behavior() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("not-an-executable-format");
    std::fs::write(&executable, "exit 0\n").unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).unwrap();

    let baseline = Command::new(&executable).spawn();
    let mut managed_command = Command::new(&executable);
    let managed = ManagedChild::spawn(&mut managed_command);
    match (baseline, managed) {
        (Ok(mut baseline), Ok(mut managed)) => {
            assert_eq!(
                baseline.wait().expect("wait baseline").success(),
                managed.wait().expect("wait managed").success(),
                "ManagedChild must preserve successful platform script fallback behavior"
            );
        }
        (Err(baseline), Err(managed)) => {
            assert_eq!(managed.kind(), baseline.kind());
            assert_eq!(managed.raw_os_error(), baseline.raw_os_error());
        }
        (baseline, managed) => panic!(
            "ManagedChild changed platform spawn behavior: baseline={:?} managed={:?}",
            baseline
                .map(|child| child.id())
                .map_err(|error| (error.kind(), error.raw_os_error())),
            managed
                .map(|child| child.id())
                .map_err(|error| (error.kind(), error.raw_os_error()))
        ),
    }
}
/// `try_tree_exit` is the non-blocking tree probe used by Runner shutdown.
#[test]
#[ignore = "manual real-process lifecycle: probes a live process tree"]
fn try_tree_exit_tracks_tree_liveness() {
    let (mut managed, _) = spawn_helper("sleep", &["60", "0"], false);
    assert!(!managed.try_tree_exit().expect("live tree probe"));
    managed.terminate_tree().expect("terminate tree");
    assert!(managed
        .wait_tree_exit(Duration::from_secs(10))
        .expect("wait tree"));
    assert!(managed.try_tree_exit().expect("empty tree probe"));
    let _ = managed.try_wait();
}

#[test]
#[ignore = "manual real-process lifecycle: waits for EOF and zombie-only tree state"]
fn unreaped_direct_child_is_not_a_live_tree_member() {
    let (mut managed, reader) = spawn_helper("sleep", &["0", "0"], true);
    reader
        .expect("captured stdout reader")
        .wait_for_eof(Duration::from_secs(5))
        .expect("direct child should close stdout when it exits");

    assert!(
        managed
            .wait_tree_exit(Duration::from_secs(1))
            .expect("zombie-only tree probe"),
        "an exited but unreaped direct child must not keep the process tree live"
    );
    assert!(
        managed.try_wait().expect("reap direct child").is_some(),
        "the direct child should still have been unreaped until try_wait"
    );
}

// ---------------------------------------------------------------------------
// Platform-native liveness probing
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
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
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    ok == 1 && exit_code == 259 // 259 == STILL_ACTIVE
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 is an existence probe; the pid comes from our own test
    // helper.
    (unsafe { libc::kill(pid as i32, 0) }) == 0
}

// ---------------------------------------------------------------------------
// 9. Stress
// ---------------------------------------------------------------------------

/// Repeatedly spawn a tree, wait for the grandchild to be ready, terminate,
/// and confirm the tree empties. Run separately:
/// `cargo test -p webcodex-process stress -- --ignored --nocapture`.
#[test]
#[ignore = "stress: run separately via --ignored"]
fn stress_spawn_terminate_cycles() {
    let rounds = 20;
    let mut ok = 0;
    for i in 0..rounds {
        let marker = unique_temp_path(&format!("stress-{i}"));
        let (mut managed, gc_pid, reader) = spawn_tree_with_grandchild(&marker);

        let _ = managed.wait().expect("wait direct child");
        assert!(
            process_alive(gc_pid),
            "round {i}: grandchild should be alive before terminate"
        );

        managed.terminate_tree().expect("terminate tree");
        if !wait_for_liveness(
            gc_pid,
            false,
            Duration::from_secs(10),
            &format!("round-{i}"),
        ) {
            panic!("round {i}: grandchild survived terminate_tree");
        }
        if !managed
            .wait_tree_exit(Duration::from_secs(10))
            .expect("wait tree")
        {
            panic!("round {i}: tree did not empty after terminate");
        }
        reader
            .wait_for_eof(Duration::from_secs(10))
            .expect("stress stdout should close");
        assert!(!marker.exists(), "round {i}: marker must not appear");
        drop(managed);
        ok += 1;
    }
    assert_eq!(ok, rounds, "all stress rounds must succeed");
}
