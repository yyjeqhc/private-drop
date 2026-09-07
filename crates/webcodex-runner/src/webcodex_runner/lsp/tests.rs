use super::super::test_support::{fake_server_path, wait_until};
use super::*;
use serde_json::{json, Value};
use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    // Drop the supervisor before the temporary directory so the fake server
    // can persist its graceful-exit marker during supervisor Drop.
    supervisor: LspSupervisor,
    _temp: TempDir,
    root: PathBuf,
    marker: PathBuf,
    exit_marker: PathBuf,
}

impl Fixture {
    fn new(scenario: &str) -> Self {
        Self::with_limits(scenario, 4, Duration::from_secs(60))
    }

    fn with_limits(scenario: &str, maximum: usize, idle_ttl: Duration) -> Self {
        Self::with_config(
            scenario,
            maximum,
            idle_ttl,
            Duration::from_millis(300),
            true,
        )
    }

    /// Fixture for tests that pin explicit `cleanup_idle` return values; the
    /// background reaper would race those assertions.
    fn with_manual_cleanup(scenario: &str, maximum: usize, idle_ttl: Duration) -> Self {
        Self::with_config(
            scenario,
            maximum,
            idle_ttl,
            Duration::from_millis(300),
            false,
        )
    }

    fn with_config(
        scenario: &str,
        maximum: usize,
        idle_ttl: Duration,
        shutdown_timeout: Duration,
        background_reaper: bool,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        let marker = temp.path().join("starts.marker");
        let exit_marker = temp.path().join("exit.marker");
        let command = LspCommand::new(fake_server_path().as_os_str().to_owned())
            .arg(scenario)
            .arg(marker.as_os_str())
            .arg(exit_marker.as_os_str())
            .env("WEBCODEX_LSP_FAKE", "1");
        let supervisor = LspSupervisor::new(LspSupervisorConfig {
            commands: HashMap::from([(LspServerKind::RustAnalyzer, command)]),
            max_servers_per_project: 1,
            max_servers_per_agent: maximum,
            request_timeout: Duration::from_millis(300),
            initialize_timeout: Duration::from_secs(2),
            shutdown_timeout,
            idle_ttl,
            background_reaper,
        });
        Self {
            supervisor,
            _temp: temp,
            root,
            marker,
            exit_marker,
        }
    }

    fn starts(&self) -> usize {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("start:"))
            .count()
    }

    fn start_pids(&self) -> Vec<u32> {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("start:"))
            .filter_map(|rest| rest.split(':').next())
            .filter_map(|pid| pid.parse().ok())
            .collect()
    }

    fn descendant_pids(&self) -> Vec<u32> {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("descendant:"))
            .filter_map(|pid| pid.parse().ok())
            .collect()
    }
}

#[test]
fn lsp_supervisor_is_lazy_and_reuses_one_process_for_concurrent_project_calls() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("normal");
    assert!(!fixture.marker.exists());
    let barrier = Arc::new(Barrier::new(7));
    let handles = (0..6)
        .map(|index| {
            let supervisor = fixture.supervisor.clone();
            let root = fixture.root.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                supervisor
                    .request(
                        &root,
                        LspServerKind::RustAnalyzer,
                        "fake/echo",
                        json!({"index": index}),
                    )
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    for handle in handles {
        assert_eq!(handle.join().unwrap()["method"], "fake/echo");
    }
    assert_eq!(fixture.starts(), 1);
    let first = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let second = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.process_id(), second.process_id());
}

#[test]
fn concurrent_document_refresh_uses_one_monotonic_version() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("normal");
    let document_path = fixture.root.join("main.rs");
    fs::write(&document_path, "fn initial() {}\n").unwrap();
    let uri = Url::from_file_path(&document_path).unwrap().to_string();
    fixture
        .supervisor
        .prepare_document(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            &uri,
            "rust",
            "fn initial() {}\n",
        )
        .unwrap();

    let barrier = Arc::new(Barrier::new(9));
    let handles = (0..8)
        .map(|_| {
            let supervisor = fixture.supervisor.clone();
            let root = fixture.root.clone();
            let uri = uri.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                supervisor.prepare_document(
                    &root,
                    LspServerKind::RustAnalyzer,
                    &uri,
                    "rust",
                    "fn refreshed() {}\n",
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    assert!(
        wait_until(Duration::from_secs(5), || {
            fs::read_to_string(&fixture.marker)
                .unwrap_or_default()
                .lines()
                .any(|line| line.starts_with("didChange:"))
        }),
        "fake LSP server never observed the document change"
    );
    let marker = fs::read_to_string(&fixture.marker).unwrap();
    assert_eq!(
        marker
            .lines()
            .filter(|line| line.starts_with("didOpen:"))
            .count(),
        1,
        "{marker}"
    );
    let changes = marker
        .lines()
        .filter(|line| line.starts_with("didChange:"))
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 1, "{marker}");
    assert!(changes[0].contains("\"version\":2"), "{marker}");
}

#[test]
fn failed_did_change_does_not_advance_document_state() {
    let _serial = super::super::serialize_fake_lsp_test();
    let documents = Mutex::new(HashMap::new());
    let initial = DocumentOpen {
        uri: "file:///workspace/main.rs",
        language_id: "rust",
        text: "fn initial() {}\n",
    };
    synchronize_document_state(&documents, initial, |method, _| {
        assert_eq!(method, "textDocument/didOpen");
        Ok(())
    })
    .unwrap();

    let changed = DocumentOpen {
        text: "fn changed() {}\n",
        ..initial
    };
    let error = synchronize_document_state(&documents, changed, |method, _| {
        assert_eq!(method, "textDocument/didChange");
        Err(LspError::WriterFailed("injected failure".to_string()))
    })
    .unwrap_err();
    assert!(matches!(error, LspError::WriterFailed(_)));
    let unchanged = lock_unpoison(&documents)[initial.uri];
    assert_eq!(unchanged.version, 1);
    assert_eq!(
        unchanged.content_fingerprint,
        document_fingerprint(initial.text)
    );

    let version = synchronize_document_state(&documents, changed, |method, params| {
        assert_eq!(method, "textDocument/didChange");
        assert_eq!(params["textDocument"]["version"], 2);
        Ok(())
    })
    .unwrap();
    assert_eq!(version, 2);
}

#[test]
fn document_version_overflow_is_safe_and_state_is_fingerprint_only() {
    let _serial = super::super::serialize_fake_lsp_test();
    assert!(std::mem::size_of::<OpenDocumentState>() <= 40);
    let uri = "file:///workspace/main.rs";
    let initial_text = "fn initial() {}\n";
    let documents = Mutex::new(HashMap::from([(
        uri.to_string(),
        OpenDocumentState {
            version: i32::MAX,
            content_fingerprint: document_fingerprint(initial_text),
        },
    )]));
    let error = synchronize_document_state(
        &documents,
        DocumentOpen {
            uri,
            language_id: "rust",
            text: "fn changed() {}\n",
        },
        |_, _| panic!("overflow must be rejected before notification"),
    )
    .unwrap_err();
    assert!(matches!(error, LspError::ProtocolError(_)));
    assert_eq!(lock_unpoison(&documents)[uri].version, i32::MAX);
}

#[test]
fn diagnostics_cache_is_latest_value_bounded_and_counts_malformed_notifications() {
    let _serial = super::super::serialize_fake_lsp_test();
    let cache = DiagnosticsCache::default();
    cache.record_publish_diagnostics(Some(&json!({"uri": 7, "diagnostics": []})));
    cache.record_publish_diagnostics(Some(&json!({"uri": "file:///bad.rs"})));
    assert_eq!(cache.malformed_notification_count(), 2);

    let oversized = (0..(MAX_DIAGNOSTICS_PER_DOCUMENT + 7))
        .map(|index| json!({"message": index}))
        .collect::<Vec<_>>();
    cache.record_publish_diagnostics(Some(&json!({
        "uri": "file:///workspace/latest.rs",
        "version": 1,
        "diagnostics": oversized,
    })));
    {
        let state = lock_unpoison(&cache.state);
        let publication = &state.publications["file:///workspace/latest.rs"];
        assert_eq!(publication.diagnostics.len(), MAX_DIAGNOSTICS_PER_DOCUMENT);
        assert_eq!(
            publication.raw_diagnostics_count,
            MAX_DIAGNOSTICS_PER_DOCUMENT + 7
        );
    }
    cache.record_publish_diagnostics(Some(&json!({
        "uri": "file:///workspace/latest.rs",
        "version": 2,
        "diagnostics": [],
    })));
    {
        let state = lock_unpoison(&cache.state);
        let latest = &state.publications["file:///workspace/latest.rs"];
        assert_eq!(latest.version, Some(2));
        assert!(latest.diagnostics.is_empty());
        assert_eq!(latest.raw_diagnostics_count, 0);
    }

    let bounded_cache = DiagnosticsCache::default();
    for index in 0..=MAX_DIAGNOSTIC_DOCUMENTS {
        bounded_cache.record_publish_diagnostics(Some(&json!({
            "uri": format!("file:///workspace/{index}.rs"),
            "diagnostics": [],
        })));
    }
    let state = lock_unpoison(&bounded_cache.state);
    assert_eq!(state.publications.len(), MAX_DIAGNOSTIC_DOCUMENTS);
    assert!(!state.publications.contains_key("file:///workspace/0.rs"));
}

#[test]
fn workspace_symbol_timeout_uses_operation_budget_only_for_rust() {
    let ordinary = Duration::from_secs(3);
    let operation_budget = Duration::from_secs(17);
    assert_eq!(
        workspace_symbol_request_timeout(LspServerKind::RustAnalyzer, ordinary, operation_budget),
        operation_budget
    );
    assert_eq!(
        workspace_symbol_request_timeout(LspServerKind::Gopls, ordinary, operation_budget),
        ordinary
    );
    assert_eq!(
        workspace_symbol_request_timeout(LspServerKind::Pyright, ordinary, operation_budget),
        ordinary
    );
    assert_eq!(
        workspace_symbol_request_timeout(
            LspServerKind::TypeScriptLanguageServer,
            ordinary,
            operation_budget
        ),
        ordinary
    );
}

#[test]
fn server_status_cache_malformed_notification_clears_stale_readiness() {
    let cache = ServerStatusCache::default();
    cache.record(Some(&json!({"health": "ok", "quiescent": true})));
    assert!(cache
        .wait_for_quiescent_ok(Instant::now() + Duration::from_millis(10))
        .is_ok());

    cache.record(Some(&json!({"health": "ok"})));
    let error = cache.wait_for_quiescent_ok(Instant::now()).unwrap_err();
    assert!(matches!(error, LspError::RequestTimeout { .. }));
}

#[test]
fn diagnostics_cache_wait_has_version_generation_and_timeout_semantics() {
    let _serial = super::super::serialize_fake_lsp_test();
    let cache = DiagnosticsCache::default();
    let uri = "file:///workspace/main.rs";
    let no_cache = cache
        .wait_for_publication(uri, 1, 0, Instant::now())
        .unwrap();
    assert!(no_cache.0.is_none());
    assert!(no_cache.1);

    cache.record_publish_diagnostics(Some(&json!({
        "uri": uri,
        "version": 0,
        "diagnostics": [],
    })));
    let baseline = cache.generation();
    let stale = cache
        .wait_for_publication(uri, 1, baseline, Instant::now())
        .unwrap();
    assert_eq!(stale.0.unwrap().version, Some(0));
    assert!(stale.1);

    let version_match = cache
        .wait_for_publication(uri, 0, baseline, Instant::now())
        .unwrap();
    assert!(!version_match.1);

    let before = cache.generation();
    cache.record_publish_diagnostics(Some(&json!({
        "uri": uri,
        "diagnostics": [],
    })));
    let new_generation = cache
        .wait_for_publication(uri, 1, before, Instant::now())
        .unwrap();
    assert!(!new_generation.1);
}

#[test]
fn diagnostics_cache_is_cleared_with_server_instance_restart() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::with_manual_cleanup("diagnostics_one", 4, Duration::ZERO);
    let document_path = fixture.root.join("main.rs");
    let text = "fn main() {}\n";
    fs::write(&document_path, text).unwrap();
    let uri = Url::from_file_path(&document_path).unwrap().to_string();
    let first = fixture
        .supervisor
        .document_diagnostics(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            &uri,
            "rust",
            text,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
    assert!(first.publication.is_some());
    let first_server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    assert!(first_server.diagnostics.generation() > 0);
    drop(first_server);
    assert_eq!(fixture.supervisor.cleanup_idle(), 1);

    let second_server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    assert_eq!(second_server.diagnostics.generation(), 0);
    assert!(lock_unpoison(&second_server.diagnostics.state)
        .publications
        .is_empty());
}

#[test]
fn lsp_supervisor_uses_distinct_processes_for_distinct_projects() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("normal");
    let second_root = fixture._temp.path().join("second-project");
    fs::create_dir(&second_root).unwrap();
    let first = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let second = fixture
        .supervisor
        .server_for_test(&second_root, LspServerKind::RustAnalyzer)
        .unwrap();
    assert_ne!(first.process_id(), second.process_id());
    assert_eq!(fixture.starts(), 2);
}

#[test]
fn lsp_supervisor_enforces_runner_capacity() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::with_limits("normal", 1, Duration::from_secs(60));
    let second_root = fixture._temp.path().join("second-project");
    fs::create_dir(&second_root).unwrap();
    fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    assert!(matches!(
        fixture
            .supervisor
            .server_for_test(&second_root, LspServerKind::RustAnalyzer),
        Err(LspError::CapacityExceeded { limit: 1 })
    ));
}

#[test]
fn lsp_jsonrpc_handles_interleaved_notifications_and_multiple_request_ids() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("interleaved");
    for method in ["fake/one", "fake/two", "fake/three"] {
        let result = fixture
            .supervisor
            .request(
                &fixture.root,
                LspServerKind::RustAnalyzer,
                method,
                json!({}),
            )
            .unwrap();
        assert_eq!(result["method"], method);
    }
    assert_eq!(fixture.starts(), 1);
}

#[test]
fn lsp_jsonrpc_surfaces_errors_and_ignores_unknown_response_ids() {
    let _serial = super::super::serialize_fake_lsp_test();
    let errors = Fixture::new("json_error");
    let error = errors
        .supervisor
        .request(
            &errors.root,
            LspServerKind::RustAnalyzer,
            "fake/error",
            json!({}),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        LspError::JsonRpc {
            code: -32001,
            ref message,
            data: Some(_)
        } if message == "fake failure"
    ));

    let unknown = Fixture::new("unknown_id");
    let result = unknown
        .supervisor
        .request(
            &unknown.root,
            LspServerKind::RustAnalyzer,
            "fake/known",
            json!({}),
        )
        .unwrap();
    assert_eq!(result["method"], "fake/known");
}

#[test]
fn lsp_jsonrpc_replies_method_not_found_to_server_requests() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("server_request");
    let result = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/clientRequest",
            json!({}),
        )
        .unwrap();
    assert_eq!(result["method"], "fake/clientRequest");
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    assert_eq!(server.status(), LspServerStatus::Running);
}

#[test]
fn lsp_request_timeout_sends_cancel_request() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("timeout_cancel");
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    // initialize used id=1; next business request is id=2.
    let error = fixture
        .supervisor
        .request_with_timeout(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/timeout",
            json!({}),
            Duration::from_millis(40),
        )
        .unwrap_err();
    assert!(matches!(error, LspError::RequestTimeout { .. }));
    assert_eq!(server.pending_count(), 0);
    assert!(wait_until(Duration::from_secs(1), || {
        fs::read_to_string(&fixture.marker)
            .unwrap_or_default()
            .contains("cancel:")
    }));
    let marker = fs::read_to_string(&fixture.marker).unwrap();
    let cancel_line = marker
        .lines()
        .find(|line| line.starts_with("cancel:"))
        .expect("cancelRequest should reach the fake server");
    assert!(
        cancel_line.contains(r#""method":"$/cancelRequest""#)
            || cancel_line.contains(r#""method": "$/cancelRequest""#),
        "cancel line: {cancel_line}"
    );
    // params.id must match the timed-out request id (2 after initialize=1).
    assert!(
        cancel_line.contains(r#""id":2"#) || cancel_line.contains(r#""id": 2"#),
        "cancel line should reference request id 2: {cancel_line}"
    );
    assert_eq!(server.status(), LspServerStatus::Running);
    // Late unknown responses must not corrupt pending state.
    assert_eq!(server.pending_count(), 0);
}

#[test]
fn lsp_pending_request_receives_server_exit_error() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("crash_request");
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let error = server
        .request("fake/crash", json!({}), Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(error, LspError::ServerExited);
    assert_eq!(server.pending_count(), 0);
    assert_eq!(server.status(), LspServerStatus::Crashed);
}

#[test]
fn lsp_supervisor_restarts_once_then_succeeds() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("restart_then_success");
    let result = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/restart",
            json!({}),
        )
        .unwrap();
    assert_eq!(result["method"], "fake/restart");
    assert_eq!(fixture.starts(), 2);
}

#[test]
fn lsp_supervisor_never_restarts_more_than_once_per_call() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("restart_exhausted");
    let error = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/crash",
            json!({}),
        )
        .unwrap_err();
    assert!(
        matches!(error, LspError::RestartExhausted(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(fixture.starts(), 2);
}

#[test]
fn lsp_supervisor_restarts_malformed_alive_process_once_then_succeeds() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("malformed_alive_then_success");
    let result = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/echo",
            json!({}),
        )
        .unwrap();
    assert_eq!(result["method"], "fake/echo");
    assert_eq!(fixture.starts(), 2);
    // First process must have been reaped even though it stayed alive after
    // emitting malformed JSON.
    let pids = fixture.start_pids();
    assert_eq!(pids.len(), 2);
    assert!(wait_until(Duration::from_secs(2), || !process_exists(
        pids[0]
    )));
    assert!(process_exists(pids[1]));
}

#[test]
fn lsp_supervisor_malformed_alive_exhausts_restart_without_timeout() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("malformed_alive_always");
    let started = Instant::now();
    let error = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/echo",
            json!({}),
        )
        .unwrap_err();
    assert!(
        matches!(error, LspError::RestartExhausted(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(fixture.starts(), 2);
    // Must not degrade into waiting full request timeouts for a dead reader.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "restart path took too long: {:?}",
        started.elapsed()
    );
    for pid in fixture.start_pids() {
        assert!(wait_until(Duration::from_secs(2), || !process_exists(pid)));
    }
}

#[test]
fn lsp_initialize_failure_consumes_the_single_restart_budget() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("initialize_exit");
    let error = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/unreachable",
            json!({}),
        )
        .unwrap_err();
    assert!(
        matches!(error, LspError::RestartExhausted(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(fixture.starts(), 2);
}

#[test]
fn lsp_initialize_pre_exit_with_stderr_surfaces_component_missing_diagnostic() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("missing_component_stderr");
    let error = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/unreachable",
            json!({}),
        )
        .unwrap_err();
    let message = error.to_string();
    assert!(
        matches!(error, LspError::RestartExhausted(_)),
        "unexpected error: {error:?}"
    );
    assert!(
        message
            .contains("rust-analyzer component is not installed for the active rustup toolchain"),
        "missing component diagnostic not surfaced: {message}"
    );
    assert!(
        !message.contains("/root/") && !message.contains("file://"),
        "diagnostic must not leak absolute paths: {message}"
    );
    assert_eq!(fixture.starts(), 2);
}

/// The rustup proxy shim and toolchain layout are Unix-only; the whole test
/// is skipped on Windows rather than exiting early from a `#[cfg(not(unix))]`
/// block (which left the remainder of the body unreachable there).
#[cfg(unix)]
#[test]
fn lsp_rustup_proxy_without_component_is_not_available() {
    let _env_lock = crate::tests::test_env_lock();
    let _serial = super::super::serialize_fake_lsp_test();
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let rustup_home = temp.path().join("rustup");
    let toolchain = "stable-x86_64-unknown-linux-gnu";
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(rustup_home.join("toolchains").join(toolchain).join("bin")).unwrap();
    fs::write(
        rustup_home.join("settings.toml"),
        format!("default_toolchain = \"{toolchain}\"\n"),
    )
    .unwrap();
    // Mimic cargo-bin rustup shims: rust-analyzer -> rustup.
    let rustup_bin = bin.join("rustup");
    fs::write(&rustup_bin, b"#!/bin/sh\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&rustup_bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&rustup_bin, perms).unwrap();
        std::os::unix::fs::symlink("rustup", bin.join("rust-analyzer")).unwrap();
    }

    let command = LspCommand::new(bin.join("rust-analyzer"));
    // Point detection at the fixture rustup home without spawning anything.
    let _env = crate::tests::EnvGuard::new()
        .set("RUSTUP_HOME", &rustup_home)
        .remove("RUSTUP_TOOLCHAIN");
    let available_missing = command.is_available(LspServerKind::RustAnalyzer);

    // Installing the component binary under the active toolchain restores
    // availability without needing to execute the proxy.
    let component = rustup_home
        .join("toolchains")
        .join(toolchain)
        .join("bin")
        .join("rust-analyzer");
    fs::write(&component, b"#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&component).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&component, perms).unwrap();
    }
    let available_installed = command.is_available(LspServerKind::RustAnalyzer);

    assert!(
        !available_missing,
        "rustup shim without component must not report available"
    );
    assert!(
        available_installed,
        "rustup shim with installed component must report available"
    );
}

#[cfg(windows)]
#[test]
fn rustup_home_falls_back_to_userprofile_on_windows() {
    let _env_lock = crate::tests::test_env_lock();
    let _serial = super::super::serialize_fake_lsp_test();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::tests::EnvGuard::new()
        .remove("RUSTUP_HOME")
        .remove("HOME")
        .set("USERPROFILE", temp.path());
    let detected = rustup_home_dir();

    assert_eq!(detected, Some(temp.path().join(".rustup")));
}

#[test]
fn generic_startup_stderr_summary_compacts_bounds_or_none() {
    let _serial = super::super::serialize_fake_lsp_test();
    // Language-specific classification (e.g. rustup component missing) is
    // owned by the profile's `startup_stderr_classifier`; the generic
    // fallback compacts control characters to spaces, trims, and bounds.
    let summary = generic_startup_stderr_summary("  language server crashed on boot \n").unwrap();
    assert_eq!(summary, "language server crashed on boot");
    assert!(generic_startup_stderr_summary("   \n\t  ").is_none());
    let long = "x".repeat(200);
    let bounded = generic_startup_stderr_summary(&long).unwrap();
    // 160 retained characters plus the ellipsis marker.
    assert_eq!(bounded.chars().count(), 161);
    assert!(bounded.ends_with('…'));
}

#[test]
fn lsp_exit_immediately_after_initialize_is_detected_and_bounded() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("exit_after_initialize");
    let error = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/unreachable",
            json!({}),
        )
        .unwrap_err();
    assert!(
        matches!(error, LspError::RestartExhausted(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(fixture.starts(), 2);
}

#[test]
fn lsp_malformed_json_and_invalid_content_length_are_distinct() {
    let _serial = super::super::serialize_fake_lsp_test();
    let malformed = Fixture::new("malformed_json");
    let server = malformed
        .supervisor
        .server_for_test(&malformed.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let error = server
        .request("fake/malformed", json!({}), Duration::from_secs(1))
        .unwrap_err();
    assert!(matches!(error, LspError::MalformedMessage(_)));

    let invalid = Fixture::new("invalid_length");
    let server = invalid
        .supervisor
        .server_for_test(&invalid.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let error = server
        .request("fake/invalid", json!({}), Duration::from_secs(1))
        .unwrap_err();
    assert!(matches!(error, LspError::ProtocolError(_)));
}

#[test]
fn lsp_position_encoding_uses_server_capability_or_utf16_default() {
    let _serial = super::super::serialize_fake_lsp_test();
    for (scenario, expected) in [
        ("utf8", PositionEncoding::Utf8),
        ("utf16", PositionEncoding::Utf16),
        ("utf32", PositionEncoding::Utf32),
        ("normal", PositionEncoding::Utf16),
    ] {
        let fixture = Fixture::new(scenario);
        let server = fixture
            .supervisor
            .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
            .unwrap();
        assert_eq!(server.position_encoding(), expected, "scenario={scenario}");
    }
}

#[test]
fn lsp_initialize_uses_constrained_rust_analyzer_profile() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("normal");
    let _server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let marker = fs::read_to_string(&fixture.marker).unwrap();
    let initialize_line = marker
        .lines()
        .find(|line| line.starts_with("initialize:"))
        .expect("fake server should record the initialize request body");
    let body_json = initialize_line
        .strip_prefix("initialize:")
        .expect("initialize: prefix");
    let body: serde_json::Value =
        serde_json::from_str(body_json).expect("initialize body must be valid JSON");
    assert_eq!(body["method"], "initialize");

    let params = body
        .get("params")
        .expect("initialize request must include params");
    let options = params
        .get("initializationOptions")
        .expect("initializationOptions must be present for the constrained profile");

    // Fail if any safety field is removed, restored to defaults, or nested wrong.
    assert_eq!(
        options.pointer("/cargo/buildScripts/enable"),
        Some(&json!(false)),
        "cargo.buildScripts.enable must be false: {options}"
    );
    assert_eq!(
        options.pointer("/cargo/noDeps"),
        Some(&json!(true)),
        "cargo.noDeps must be true: {options}"
    );
    assert_eq!(
        options.pointer("/procMacro/enable"),
        Some(&json!(false)),
        "procMacro.enable must be false: {options}"
    );
    assert_eq!(
        options.get("checkOnSave"),
        Some(&json!(false)),
        "checkOnSave must be false: {options}"
    );
    assert_eq!(
        options.pointer("/files/watcher"),
        Some(&json!("server")),
        "files.watcher must be \"server\": {options}"
    );
    assert_eq!(
        options.pointer("/cachePriming/enable"),
        Some(&json!(false)),
        "cachePriming.enable must be false: {options}"
    );

    let canonical = fs::canonicalize(&fixture.root).unwrap();
    let expected_root_uri = Url::from_directory_path(&canonical).unwrap().to_string();
    assert_eq!(
        params.get("rootUri").and_then(Value::as_str),
        Some(expected_root_uri.as_str()),
        "rootUri must be the canonical project root"
    );

    let encodings = params
        .pointer("/capabilities/general/positionEncodings")
        .and_then(Value::as_array)
        .expect("positionEncodings capability must be present");
    let encoding_strings: Vec<&str> = encodings.iter().filter_map(Value::as_str).collect();
    assert!(
        encoding_strings.contains(&"utf-8")
            && encoding_strings.contains(&"utf-16")
            && encoding_strings.contains(&"utf-32"),
        "positionEncodings must include utf-8, utf-16, and utf-32: {encodings:?}"
    );
    assert_eq!(
        params.pointer("/capabilities/experimental/serverStatusNotification"),
        Some(&json!(true)),
        "rust-analyzer workspace readiness must request serverStatus notifications"
    );
}

/// Start the fake server under `kind` and return the `initializationOptions`
/// it recorded from the `initialize` request. Lets per-language security
/// profiles be asserted without the real language server installed.
fn captured_initialize_options(kind: LspServerKind) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    let marker = temp.path().join("starts.marker");
    let exit_marker = temp.path().join("exit.marker");
    let command = LspCommand::new(fake_server_path().as_os_str().to_owned())
        .arg("normal")
        .arg(marker.as_os_str())
        .arg(exit_marker.as_os_str());
    let supervisor = LspSupervisor::new(LspSupervisorConfig {
        commands: HashMap::from([(kind, command)]),
        request_timeout: Duration::from_millis(300),
        initialize_timeout: Duration::from_secs(2),
        shutdown_timeout: Duration::from_millis(300),
        ..LspSupervisorConfig::default()
    });
    let _server = supervisor.server_for_test(&root, kind).unwrap();
    let marker_text = fs::read_to_string(&marker).unwrap();
    let line = marker_text
        .lines()
        .find(|line| line.starts_with("initialize:"))
        .expect("fake server records the initialize body");
    let body: Value = serde_json::from_str(line.strip_prefix("initialize:").unwrap()).unwrap();
    body.pointer("/params/initializationOptions")
        .cloned()
        .expect("initializationOptions present")
}

#[test]
fn lsp_initialize_uses_constrained_pyright_profile() {
    let _serial = super::super::serialize_fake_lsp_test();
    let options = captured_initialize_options(LspServerKind::Pyright);
    // openFilesOnly bounds analysis; pyright never executes project code, so
    // the code-execution boundary needs no build-script/proc-macro toggles.
    assert_eq!(
        options.pointer("/python/analysis/diagnosticMode"),
        Some(&json!("openFilesOnly")),
        "{options}"
    );
    assert_eq!(
        options.pointer("/python/analysis/typeCheckingMode"),
        Some(&json!("basic")),
        "{options}"
    );
    assert_eq!(
        options.pointer("/python/analysis/autoImportCompletions"),
        Some(&json!(false)),
        "{options}"
    );
    assert_eq!(
        options.pointer("/python/analysis/useLibraryCodeForTypes"),
        Some(&json!(true)),
        "{options}"
    );
}

#[test]
fn lsp_initialize_uses_constrained_typescript_profile() {
    let _serial = super::super::serialize_fake_lsp_test();
    let options = captured_initialize_options(LspServerKind::TypeScriptLanguageServer);
    // disableAutomaticTypingAcquisition is the network boundary (no @types
    // downloads from npm) — the analog to rust-analyzer's cargo.noDeps.
    assert_eq!(
        options.pointer("/disableAutomaticTypingAcquisition"),
        Some(&json!(true)),
        "{options}"
    );
    assert_eq!(
        options.pointer("/preferences/includePackageJsonAutoImports"),
        Some(&json!("off")),
        "{options}"
    );
    assert_eq!(
        options.pointer("/hostInfo"),
        Some(&json!("webcodex-runner")),
        "{options}"
    );
}

#[test]
fn lsp_initialize_uses_constrained_gopls_profile() {
    let _serial = super::super::serialize_fake_lsp_test();
    let options = captured_initialize_options(LspServerKind::Gopls);
    assert_eq!(
        options.pointer("/buildFlags"),
        Some(&json!(["-mod=readonly"])),
        "{options}"
    );
    for (key, expected) in [
        ("GOPROXY", "off"),
        ("GOSUMDB", "off"),
        ("GOTOOLCHAIN", "local"),
        ("GOPRIVATE", ""),
        ("GONOPROXY", "none"),
        ("GOVCS", "*:off"),
    ] {
        assert_eq!(
            options.pointer(&format!("/env/{key}")),
            Some(&json!(expected)),
            "{key}: {options}"
        );
    }
    assert_eq!(
        options.pointer("/vulncheck"),
        Some(&json!("Off")),
        "{options}"
    );
    assert_eq!(
        options.pointer("/symbolScope"),
        Some(&json!("workspace")),
        "{options}"
    );
    for lens in [
        "generate",
        "regenerate_cgo",
        "run_govulncheck",
        "tidy",
        "upgrade_dependency",
        "vendor",
    ] {
        assert_eq!(
            options.pointer(&format!("/codelenses/{lens}")),
            Some(&json!(false)),
            "{lens}: {options}"
        );
    }
}

#[test]
fn gopls_process_environment_overrides_ambient_network_settings() {
    let _serial = super::super::serialize_fake_lsp_test();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    let marker = temp.path().join("starts.marker");
    let exit_marker = temp.path().join("exit.marker");
    let command = LspCommand::new(fake_server_path().as_os_str().to_owned())
        .arg("capture_safety_env")
        .arg(marker.as_os_str())
        .arg(exit_marker.as_os_str())
        .env("GOPROXY", "https://proxy.invalid")
        .env("GOSUMDB", "sum.invalid")
        .env("GOTOOLCHAIN", "auto")
        .env("GOPRIVATE", "corp.invalid")
        .env("GONOPROXY", "corp.invalid")
        .env("GOVCS", "*:all")
        .env("HTTPS_PROXY", "https://proxy.invalid")
        .env("https_proxy", "https://proxy.invalid");
    let supervisor = LspSupervisor::new(LspSupervisorConfig {
        commands: HashMap::from([(LspServerKind::Gopls, command)]),
        request_timeout: Duration::from_millis(300),
        initialize_timeout: Duration::from_secs(2),
        shutdown_timeout: Duration::from_millis(300),
        ..LspSupervisorConfig::default()
    });
    let _server = supervisor
        .server_for_test(&root, LspServerKind::Gopls)
        .unwrap();
    let marker_text = fs::read_to_string(marker).unwrap();
    for expected in [
        "env:GOPROXY=off",
        "env:GOSUMDB=off",
        "env:GOTOOLCHAIN=local",
        "env:GOPRIVATE=",
        "env:GONOPROXY=none",
        "env:GOVCS=*:off",
        "env:HTTP_PROXY=http://127.0.0.1:0",
        "env:HTTPS_PROXY=http://127.0.0.1:0",
        "env:ALL_PROXY=http://127.0.0.1:0",
        "env:NO_PROXY=localhost,127.0.0.1,::1",
        "env:http_proxy=http://127.0.0.1:0",
        "env:https_proxy=http://127.0.0.1:0",
        "env:all_proxy=http://127.0.0.1:0",
        "env:no_proxy=localhost,127.0.0.1,::1",
    ] {
        assert!(
            marker_text.lines().any(|line| line == expected),
            "{marker_text}"
        );
    }
}

#[test]
fn lsp_default_args_apply_to_env_and_path_but_not_configured() {
    let _serial = super::super::serialize_fake_lsp_test();
    let supervisor = LspSupervisor::default();
    // An env override must resolve to a concrete program: on Windows the
    // file must exist and pass the native/PATHEXT rules (fail closed), on
    // Unix any absolute path is accepted. A real temp-dir file is valid on
    // both platforms.
    let bin = tempfile::tempdir().unwrap();
    let env_program = bin.path().join("pyright-langserver");
    std::fs::write(&env_program, b"x").unwrap();
    // Pyright resolves from the env override with `--stdio` appended.
    let (from_env, source) = supervisor
        .resolve_command_from_sources(
            LspServerKind::Pyright,
            Some(OsString::from(&env_program)),
            Some(OsStr::new("")),
        )
        .unwrap();
    assert_eq!(source, crate::lsp_bridge::LspCommandSource::Environment);
    assert_eq!(from_env.args, vec![OsString::from("--stdio")]);

    // rust-analyzer declares no default args.
    let rust_program = bin.path().join("rust-analyzer");
    std::fs::write(&rust_program, b"x").unwrap();
    let (rust, _) = supervisor
        .resolve_command_from_sources(
            LspServerKind::RustAnalyzer,
            Some(OsString::from(&rust_program)),
            Some(OsStr::new("")),
        )
        .unwrap();
    assert!(rust.args.is_empty(), "{:?}", rust.args);

    // gopls also speaks stdio with no default arguments.
    let gopls_program = bin.path().join("gopls");
    std::fs::write(&gopls_program, b"x").unwrap();
    let (gopls, _) = supervisor
        .resolve_command_from_sources(
            LspServerKind::Gopls,
            Some(OsString::from(&gopls_program)),
            Some(OsStr::new("")),
        )
        .unwrap();
    assert!(gopls.args.is_empty(), "{:?}", gopls.args);

    // An explicitly configured command is used verbatim — no default args.
    let configured = LspSupervisor::new(LspSupervisorConfig {
        commands: HashMap::from([(LspServerKind::Pyright, LspCommand::new("/custom/pyright"))]),
        ..LspSupervisorConfig::default()
    });
    let (command, source) = configured
        .resolve_command_from_sources(LspServerKind::Pyright, None, Some(OsStr::new("")))
        .unwrap();
    assert_eq!(source, crate::lsp_bridge::LspCommandSource::Configured);
    assert!(command.args.is_empty(), "{:?}", command.args);
}

#[test]
#[ignore = "runner real-process lane: crashed LSP child reap latency"]
fn runner_real_process_lsp_crashed_connection_reaps_immediately_without_full_shutdown_deadline() {
    let _serial = super::super::serialize_fake_lsp_test();
    // Crashed-but-alive child must not wait the full shutdown timeout before
    // kill/wait. Use a deliberately large budget so the difference is obvious.
    let shutdown_timeout = Duration::from_secs(1);
    let fixture = Fixture::with_config(
        "malformed_alive_always",
        4,
        Duration::from_secs(3600),
        shutdown_timeout,
        false,
    );
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let pid = server.process_id();
    let error = server
        .request("fake/malformed", json!({}), Duration::from_secs(1))
        .unwrap_err();
    assert!(matches!(error, LspError::MalformedMessage(_)));
    assert_eq!(server.status(), LspServerStatus::Crashed);
    assert!(
        process_exists(pid),
        "precondition: child stays alive after malformed response"
    );

    let started = Instant::now();
    // cleanup_idle reaps unusable Running slots via the shared shutdown path.
    assert_eq!(fixture.supervisor.cleanup_idle(), 1);
    let elapsed = started.elapsed();

    // configured timeout = 1s; expected completion well under half that budget
    // with normal scheduling tolerance (not a tight flaky ms boundary).
    assert!(
        elapsed < Duration::from_millis(500),
        "crashed connection reap took {elapsed:?}, expected well under {shutdown_timeout:?}"
    );
    assert!(
        elapsed < shutdown_timeout,
        "crashed connection must not consume the full shutdown deadline: {elapsed:?}"
    );
    assert!(wait_until(Duration::from_secs(2), || !process_exists(pid)));
    assert_eq!(fixture.supervisor.server_count_for_test(), 0);
}

#[test]
fn lsp_stderr_capture_is_bounded() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("stderr_flood");
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    assert!(wait_until(Duration::from_secs(1), || server.stderr_len() > 0));
    assert!(server.stderr_len() <= MAX_STDERR_BYTES);
}

#[test]
#[ignore = "runner real-process lane: LSP graceful leader exit reaps surviving descendant"]
fn runner_real_process_lsp_graceful_leader_exit_still_reaps_surviving_descendant() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::with_config(
        "shutdown_descendant",
        4,
        Duration::from_secs(60),
        Duration::from_secs(1),
        false,
    );
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let direct_pid = server.process_id();
    assert!(wait_until(Duration::from_secs(1), || !fixture
        .descendant_pids()
        .is_empty()));
    let descendant_pid = fixture.descendant_pids()[0];
    assert!(process_exists(direct_pid));
    assert!(process_exists(descendant_pid));

    let outcome = fixture
        .supervisor
        .shutdown_until(Instant::now() + Duration::from_secs(1));
    assert_eq!(outcome.servers, 1);
    assert_eq!(outcome.timed_out, 0, "whole-tree shutdown timed out");
    assert_eq!(
        outcome.failures, 0,
        "whole-tree shutdown reported a failure"
    );
    assert!(wait_until(Duration::from_secs(1), || !process_exists(
        direct_pid
    )));
    assert!(wait_until(Duration::from_secs(1), || !process_exists(
        descendant_pid
    )));
}

#[test]
#[ignore = "runner real-process lane: LSP shutdown and Drop reap child process"]
fn runner_real_process_lsp_shutdown_and_drop_reap_the_child_process() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("normal");
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let pid = server.process_id();
    fixture.supervisor.shutdown();
    assert!(wait_until(Duration::from_secs(1), || fixture
        .exit_marker
        .exists()));
    assert!(wait_until(Duration::from_secs(1), || !process_exists(pid)));

    let dropped = Fixture::new("normal");
    let server = dropped
        .supervisor
        .server_for_test(&dropped.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let pid = server.process_id();
    let Fixture {
        supervisor,
        _temp,
        root: _,
        marker: _,
        exit_marker,
    } = dropped;
    drop(server);
    drop(supervisor);
    assert!(wait_until(Duration::from_secs(1), || exit_marker.exists()));
    assert!(wait_until(Duration::from_secs(1), || !process_exists(pid)));
    drop(_temp);
}

#[test]
#[ignore = "runner real-process lane: hanging LSP shutdown honors one deadline"]
fn runner_real_process_lsp_shutdown_uses_single_deadline_against_hanging_server() {
    let _serial = super::super::serialize_fake_lsp_test();
    // Shutdown timeout 200ms. Multiplied waits would approach 600–800ms+.
    let shutdown_timeout = Duration::from_millis(200);
    let fixture = Fixture::with_config(
        "shutdown_hang",
        4,
        Duration::from_secs(60),
        shutdown_timeout,
        false,
    );
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let pid = server.process_id();
    // Keep a pending waiter so shutdown must fail_pending as well.
    let pending = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || server.request("fake/hang", json!({}), Duration::from_secs(5)))
    };
    assert!(wait_until(Duration::from_secs(1), || server
        .pending_count()
        > 0));

    let started = Instant::now();
    fixture.supervisor.shutdown();
    let elapsed = started.elapsed();

    // Single deadline + scheduling slack, far below 3–4× the configured timeout.
    assert!(
        elapsed < shutdown_timeout + Duration::from_millis(400),
        "shutdown took {elapsed:?}, budget was {shutdown_timeout:?}"
    );
    assert!(
        elapsed < shutdown_timeout.saturating_mul(3),
        "shutdown looked like stacked timeouts: {elapsed:?}"
    );
    assert!(wait_until(Duration::from_secs(1), || !process_exists(pid)));
    let pending_result = pending.join().unwrap();
    assert!(
        matches!(
            pending_result,
            Err(LspError::ServerExited) | Err(LspError::RequestTimeout { .. })
        ),
        "pending request should be woken: {pending_result:?}"
    );
}

#[test]
#[ignore = "runner real-process lane: multiple hanging LSP children share one shutdown deadline"]
fn runner_real_process_lsp_multiple_hanging_servers_share_one_supervisor_deadline() {
    let _serial = super::super::serialize_fake_lsp_test();
    let shutdown_timeout = Duration::from_millis(200);
    let fixture = Fixture::with_config(
        "shutdown_hang",
        4,
        Duration::from_secs(60),
        shutdown_timeout,
        false,
    );
    let parent = fixture.root.parent().unwrap();
    let roots = [
        fixture.root.clone(),
        parent.join("project-two"),
        parent.join("project-three"),
    ];
    for root in roots.iter().skip(1) {
        fs::create_dir(root).unwrap();
    }
    let servers = roots
        .iter()
        .map(|root| {
            fixture
                .supervisor
                .server_for_test(root, LspServerKind::RustAnalyzer)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let pids = servers
        .iter()
        .map(|server| server.process_id())
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 3);

    let started = Instant::now();
    let outcome = fixture
        .supervisor
        .shutdown_until(Instant::now() + shutdown_timeout);
    let elapsed = started.elapsed();
    assert_eq!(outcome.servers, 3);
    assert!(
        elapsed < Duration::from_millis(450),
        "three slots stacked independent shutdown budgets: {elapsed:?}"
    );
    for pid in pids {
        assert!(
            wait_until(Duration::from_secs(1), || !process_exists(pid)),
            "LSP child {pid} survived shared-deadline shutdown"
        );
    }

    let drop_started = Instant::now();
    drop(servers);
    assert!(
        drop_started.elapsed() < Duration::from_millis(100),
        "explicit shutdown was followed by a second Drop wait"
    );
    let Fixture {
        supervisor,
        _temp,
        root: _,
        marker: _,
        exit_marker: _,
    } = fixture;
    let supervisor_drop_started = Instant::now();
    drop(supervisor);
    assert!(
        supervisor_drop_started.elapsed() < Duration::from_millis(100),
        "supervisor Drop re-armed the configured shutdown timeout"
    );
    drop(_temp);
}

#[test]
fn lsp_reaper_timeout_does_not_rearm_supervisor_drop_budget() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::with_config(
        "normal",
        4,
        Duration::from_secs(60),
        Duration::from_secs(1),
        true,
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let exited = Arc::new(AtomicBool::new(false));
    let exited_in_thread = Arc::clone(&exited);
    let handle = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(2));
        exited_in_thread.store(true, Ordering::SeqCst);
    });
    fixture
        .supervisor
        .inner
        .reaper_started
        .store(true, Ordering::SeqCst);
    *lock_unpoison(&fixture.supervisor.inner.reaper_thread) = Some(handle);
    ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let outcome = fixture
        .supervisor
        .shutdown_until(Instant::now() + Duration::from_millis(20));
    assert!(outcome.reaper_timed_out);

    let Fixture {
        supervisor,
        _temp,
        root: _,
        marker: _,
        exit_marker: _,
    } = fixture;
    let drop_started = Instant::now();
    drop(supervisor);
    assert!(
        drop_started.elapsed() < Duration::from_millis(150),
        "SupervisorInner::drop re-armed the configured shutdown timeout: {:?}",
        drop_started.elapsed()
    );

    release_tx.send(()).unwrap();
    assert!(wait_until(Duration::from_secs(1), || exited.load(Ordering::SeqCst)));
    drop(_temp);
}

#[test]
#[ignore = "runner real-process lane: LSP initialize-timeout child cleanup budget"]
fn runner_real_process_lsp_initialize_timeout_cleanup_uses_configured_shutdown_budget() {
    let _serial = super::super::serialize_fake_lsp_test();
    let shutdown_timeout = Duration::from_millis(150);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    let marker = temp.path().join("starts.marker");
    let exit_marker = temp.path().join("exit.marker");
    let command = LspCommand::new(fake_server_path().as_os_str().to_owned())
        .arg("initialize_hang")
        .arg(marker.as_os_str())
        .arg(exit_marker.as_os_str());
    let supervisor = LspSupervisor::new(LspSupervisorConfig {
        commands: HashMap::from([(LspServerKind::RustAnalyzer, command)]),
        max_servers_per_project: 1,
        max_servers_per_agent: 4,
        request_timeout: Duration::from_millis(300),
        initialize_timeout: Duration::from_millis(80),
        shutdown_timeout,
        idle_ttl: Duration::from_secs(60),
        background_reaper: false,
    });

    let started = Instant::now();
    let error = supervisor
        .request(&root, LspServerKind::RustAnalyzer, "fake/nope", json!({}))
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        matches!(error, LspError::RestartExhausted(_)),
        "unexpected error: {error:?}"
    );
    // The supervisor consumes its one restart after the first initialize
    // failure. The child-owned start marker is not an authoritative attempt
    // counter: on Windows a newly spawned process can remain unscheduled past
    // this intentionally tiny initialize deadline and be killed before main()
    // writes the marker. RestartExhausted proves the second attempt was
    // consumed; this test owns only the configured cleanup-budget invariant.
    // Both attempts must stay well below using the multi-second default.
    assert!(
        elapsed < Duration::from_secs(2),
        "initialize cleanup used an oversized budget: {elapsed:?}"
    );
    assert!(
        elapsed < DEFAULT_SHUTDOWN_TIMEOUT.saturating_mul(2),
        "cleanup appears to use DEFAULT_SHUTDOWN_TIMEOUT: {elapsed:?}"
    );
    for line in fs::read_to_string(&marker).unwrap_or_default().lines() {
        if let Some(rest) = line.strip_prefix("start:") {
            if let Some(pid) = rest.split(':').next().and_then(|p| p.parse::<u32>().ok()) {
                assert!(wait_until(Duration::from_secs(2), || !process_exists(pid)));
            }
        }
    }
    drop(supervisor);
    drop(temp);
}

#[test]
#[ignore = "runner real-process lane: explicit idle LSP cleanup reaps child"]
fn runner_real_process_lsp_idle_cleanup_is_explicit_and_bounded() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::with_manual_cleanup("normal", 4, Duration::ZERO);
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let pid = server.process_id();
    drop(server);
    assert_eq!(fixture.supervisor.cleanup_idle(), 1);
    assert!(wait_until(Duration::from_secs(1), || !process_exists(pid)));
}

#[test]
fn lsp_idle_cleanup_skips_active_pending_requests() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::with_manual_cleanup("timeout", 4, Duration::ZERO);
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let pid = server.process_id();
    let server_for_request = Arc::clone(&server);
    let handle = std::thread::spawn(move || {
        server_for_request.request("fake/timeout", json!({}), Duration::from_millis(250))
    });
    assert!(wait_until(Duration::from_secs(1), || server
        .pending_count()
        > 0));
    assert_eq!(fixture.supervisor.cleanup_idle(), 0);
    assert_eq!(fixture.supervisor.server_count_for_test(), 1);
    let error = handle.join().unwrap().unwrap_err();
    assert!(matches!(error, LspError::RequestTimeout { .. }));
    assert_eq!(server.pending_count(), 0);
    // After the request completes, idle TTL=0 allows cleanup.
    assert_eq!(fixture.supervisor.cleanup_idle(), 1);
    assert!(wait_until(Duration::from_secs(1), || !process_exists(pid)));
    assert_eq!(fixture.supervisor.server_count_for_test(), 0);
}

#[test]
#[ignore = "runner real-process lane: idle cleanup reaps crashed-but-live LSP child"]
fn runner_real_process_lsp_idle_cleanup_reaps_crashed_alive_server_immediately() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture =
        Fixture::with_manual_cleanup("malformed_alive_always", 4, Duration::from_secs(3600));
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let pid = server.process_id();
    let error = server
        .request("fake/malformed", json!({}), Duration::from_secs(1))
        .unwrap_err();
    assert!(matches!(error, LspError::MalformedMessage(_)));
    assert_eq!(server.status(), LspServerStatus::Crashed);
    // Process may still be alive, but connection is unusable — cleanup must
    // ignore the long idle TTL and free capacity immediately.
    assert!(process_exists(pid));
    assert_eq!(fixture.supervisor.cleanup_idle(), 1);
    assert!(wait_until(Duration::from_secs(2), || !process_exists(pid)));
    assert_eq!(fixture.supervisor.server_count_for_test(), 0);
}

#[test]
#[ignore = "runner real-process lane: background LSP reaper reclaims idle child capacity"]
fn runner_real_process_lsp_background_reaper_reclaims_idle_capacity_without_explicit_cleanup() {
    let _serial = super::super::serialize_fake_lsp_test();
    // Production agents never call cleanup_idle directly; idle_ttl must be
    // honored by the built-in background reaper or capacity leaks forever.
    let fixture = Fixture::with_limits("normal", 1, Duration::from_millis(100));
    let server = fixture
        .supervisor
        .server_for_test(&fixture.root, LspServerKind::RustAnalyzer)
        .unwrap();
    let pid = server.process_id();
    drop(server);
    assert_eq!(fixture.supervisor.server_count_for_test(), 1);
    assert!(
        wait_until(Duration::from_secs(5), || fixture
            .supervisor
            .server_count_for_test()
            == 0),
        "background reaper must reclaim the idle server after idle_ttl"
    );
    assert!(wait_until(Duration::from_secs(2), || !process_exists(pid)));
    // Freed capacity must be reusable: at max_servers_per_agent=1 a second
    // project start only succeeds because the idle slot was reclaimed.
    let second_root = fixture._temp.path().join("project-second");
    fs::create_dir(&second_root).unwrap();
    fixture
        .supervisor
        .server_for_test(&second_root, LspServerKind::RustAnalyzer)
        .expect("capacity must recover after background reaping");
}

#[test]
fn lsp_project_root_is_canonical_and_external_uris_are_not_trusted() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("normal");
    let canonical = fs::canonicalize(&fixture.root).unwrap();
    let result = fixture
        .supervisor
        .request(
            &fixture.root,
            LspServerKind::RustAnalyzer,
            "fake/root",
            json!({}),
        )
        .unwrap();
    // The server process must run with the canonical project root as its
    // working directory. Windows reports the current directory to child
    // processes in ordinary DOS form (`C:\...`) even when the process was
    // spawned with the extended-length form (`\\?\C:\...`) that
    // `fs::canonicalize` returns, so compare resolved identity rather than
    // string form.
    let reported_cwd = result["cwd"].as_str().expect("fake server cwd");
    assert_eq!(
        fs::canonicalize(reported_cwd)
            .expect("server cwd must be accessible")
            .display()
            .to_string(),
        canonical.display().to_string(),
        "server cwd must resolve to the canonical project root"
    );
    let marker = fs::read_to_string(&fixture.marker).unwrap();
    let root_uri = Url::from_directory_path(&canonical).unwrap().to_string();
    assert!(marker.contains(&root_uri));

    let inside = fixture.root.join("inside.rs");
    fs::write(&inside, "fn main() {}\n").unwrap();
    let inside_uri = Url::from_file_path(&inside).unwrap();
    assert!(matches!(
        classify_uri_against_project_root(&canonical, inside_uri.as_str()),
        ProjectUriClassification::InsideProject(_)
    ));
    // The same file identified through its canonicalized (extended-length on
    // Windows) path must round-trip to a normal file URI that classifies as
    // inside the project again.
    let canonical_inside = fs::canonicalize(&inside).unwrap();
    let canonical_inside_uri = Url::from_file_path(&canonical_inside).unwrap();
    assert!(matches!(
        classify_uri_against_project_root(&canonical, canonical_inside_uri.as_str()),
        ProjectUriClassification::InsideProject(_)
    ));
    let outside = fixture._temp.path().join("outside.rs");
    fs::write(&outside, "outside\n").unwrap();
    let outside_uri = Url::from_file_path(outside).unwrap();
    assert_eq!(
        classify_uri_against_project_root(&canonical, outside_uri.as_str()),
        ProjectUriClassification::OutsideProject
    );
    // POSIX-style absolute file URIs (e.g. stdlib locations returned by
    // language servers) must stay external. On Windows the url crate cannot
    // map them to a local path at all; either way they are outside the
    // project boundary and must never be trusted as project-relative.
    assert_eq!(
        classify_uri_against_project_root(
            &canonical,
            "file:///usr/lib/rustlib/src/rust/library/core/src/lib.rs"
        ),
        ProjectUriClassification::OutsideProject
    );
    assert_eq!(
        classify_uri_against_project_root(&canonical, "https://example.test/file.rs"),
        ProjectUriClassification::Unsupported
    );
}

#[test]
fn lsp_rejects_missing_or_non_directory_project_roots_before_spawn() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fixture = Fixture::new("normal");
    let missing = fixture._temp.path().join("missing");
    assert!(matches!(
        fixture.supervisor.request(
            &missing,
            LspServerKind::RustAnalyzer,
            "fake/nope",
            json!({})
        ),
        Err(LspError::InvalidProjectRoot(_))
    ));
    let file = fixture._temp.path().join("file");
    fs::write(&file, "not a directory").unwrap();
    assert!(matches!(
        fixture
            .supervisor
            .request(&file, LspServerKind::RustAnalyzer, "fake/nope", json!({})),
        Err(LspError::InvalidProjectRoot(_))
    ));
    assert!(!fixture.marker.exists());
}

#[test]
fn lsp_command_resolution_uses_explicit_env_then_path_without_shell() {
    let _serial = super::super::serialize_fake_lsp_test();
    let fake = fake_server_path();
    let explicit = LspSupervisor::new(LspSupervisorConfig {
        commands: HashMap::from([(
            LspServerKind::RustAnalyzer,
            LspCommand::new(fake.as_os_str().to_owned()),
        )]),
        ..LspSupervisorConfig::default()
    });
    let explicit_info = explicit
        .resolve_command_info(LspServerKind::RustAnalyzer)
        .unwrap();
    assert!(explicit_info.available);
    assert_eq!(
        explicit_info.source,
        crate::lsp_bridge::LspCommandSource::Configured
    );

    let supervisor = LspSupervisor::default();
    let (from_env, env_source) = supervisor
        .resolve_command_from_sources(
            LspServerKind::RustAnalyzer,
            Some(fake.as_os_str().to_owned()),
            Some(OsStr::new("")),
        )
        .unwrap();
    assert_eq!(from_env.program, fake.as_os_str());
    assert_eq!(env_source, crate::lsp_bridge::LspCommandSource::Environment);

    let path_dir = tempfile::tempdir().unwrap();
    let analyzer = path_dir.path().join("rust-analyzer");
    fs::copy(fake, &analyzer).unwrap();
    let path = env::join_paths([path_dir.path()]).unwrap();
    let (from_path, path_source) = supervisor
        .resolve_command_from_sources(LspServerKind::RustAnalyzer, None, Some(&path))
        .unwrap();
    assert_eq!(from_path.program, analyzer.as_os_str());
    assert_eq!(path_source, crate::lsp_bridge::LspCommandSource::Path);
    let empty_path = tempfile::tempdir().unwrap();
    assert!(supervisor
        .resolve_command_from_sources(
            LspServerKind::RustAnalyzer,
            None,
            Some(empty_path.path().as_os_str())
        )
        .is_none());

    let spaced = tempfile::tempdir().unwrap();
    let program = spaced.path().join("fake server with spaces");
    fs::hard_link(fake, &program).unwrap();
    let project = tempfile::tempdir().unwrap();
    let marker = spaced.path().join("marker");
    let exit_marker = spaced.path().join("exit");
    let supervisor = LspSupervisor::new(LspSupervisorConfig {
        commands: HashMap::from([(
            LspServerKind::RustAnalyzer,
            LspCommand::new(program)
                .arg("normal")
                .arg(marker.as_os_str())
                .arg(exit_marker.as_os_str()),
        )]),
        shutdown_timeout: Duration::from_millis(300),
        initialize_timeout: Duration::from_secs(2),
        ..LspSupervisorConfig::default()
    });
    let value = supervisor
        .request(
            project.path(),
            LspServerKind::RustAnalyzer,
            "fake/direct-command",
            json!({}),
        )
        .unwrap();
    assert_eq!(value["method"], "fake/direct-command");
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0u32;
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    unsafe { CloseHandle(handle) };
    ok == 1 && exit_code == 259
}

#[cfg(target_os = "macos")]
fn process_exists(pid: u32) -> bool {
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

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_exists(_pid: u32) -> bool {
    false
}
