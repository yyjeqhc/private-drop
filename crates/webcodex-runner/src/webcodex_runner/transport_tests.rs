use super::super::config::{RunnerPolicy, ShellConfig};
use super::*;
use crate::runner_protocol::{RunnerCapabilities, RunnerRequest, RUNNER_PROTOCOL_GENERATION_V2};
#[cfg(unix)]
use crate::POLLING_DISPATCH_MAX_IN_FLIGHT;
use futures_util::{SinkExt, StreamExt};
use std::io::{Read, Write};
use std::net::{TcpListener as StdTcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;
use std::thread;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

fn test_runner_config(server_url: String) -> RunnerConfig {
    RunnerConfig {
        server_url,
        token: "test-token".to_string(),
        client_id: "oe".to_string(),
        display_name: Some("OE agent".to_string()),
        owner: Some("tester".to_string()),
        hostname: Some("oe-host".to_string()),
        host_context: None,
        project_registry_dir: None,
        legacy_projects_dir: None,
        deprecated_temporary_projects_root: None,
        poll_interval_ms: 10,
        capabilities: Some(RunnerCapabilities {
            git: true,
            ..RunnerCapabilities::default()
        }),
        max_concurrent_jobs: Some(1),
        // Transport tests run jobs in a temp dir and are not about the
        // filesystem boundary; RunnerPolicy::default() is fail-closed.
        policy: RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        },
        transport: Some(TRANSPORT_WEBSOCKET.to_string()),
        websocket_connect_timeout_secs:
            crate::webcodex_runner::default_websocket_connect_timeout_secs(),
        quic: None,
        shell: ShellConfig::default(),
        ssh: Default::default(),
        tool_providers: Default::default(),
        mcp_gateway: Default::default(),
        plugins: Default::default(),
        acp: Default::default(),
    }
}

fn polling_runner_config(server_url: String, project_registry_dir: PathBuf) -> RunnerConfig {
    let mut cfg = test_runner_config(server_url);
    cfg.transport = Some(TRANSPORT_POLLING.to_string());
    cfg.project_registry_dir = Some(project_registry_dir);
    cfg
}

fn synthetic_project_summary(index: usize, path_bytes: Option<usize>) -> RunnerProjectSummary {
    let path = match path_bytes {
        Some(bytes) => format!("/{}", "x".repeat(bytes.saturating_sub(1))),
        None => format!("/tmp/project-{index:04}"),
    };
    RunnerProjectSummary {
        id: format!("project-{index:04}"),
        name: Some(format!("Project {index:04}")),
        path,
        allow_patch: true,
        kind: None,
        registration_source: None,
        description: None,
        hooks: Vec::new(),
        disabled: false,
        revision: Some(format!("sha256:{index:064x}")),
        git_branch: None,
        git_head: None,
        git_dirty: None,
        updated_at: index as i64,
        shell_profile: None,
    }
}

fn inventory_status(
    state: &str,
    generation: &str,
    total_reported: usize,
    total_synced: usize,
) -> ShellProjectInventoryStatus {
    ShellProjectInventoryStatus {
        sync_state: state.to_string(),
        generation: Some(generation.to_string()),
        total_reported: Some(total_reported),
        total_synced,
        last_error_code: None,
        last_sync_at: Some(1),
        max_summaries_per_page: PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
        max_serialized_bytes_per_page: PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES,
    }
}

fn write_synthetic_project_configs(project_registry_dir: &Path, root: &Path, count: usize) {
    std::fs::create_dir_all(project_registry_dir).unwrap();
    for index in 0..count {
        let path = root.join(format!("project-{index:04}"));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            project_registry_dir.join(format!("project-{index:04}.toml")),
            format!(
                "id = \"project-{index:04}\"\nname = \"Project {index:04}\"\npath = {:?}\nallow_patch = true\n",
                path.to_string_lossy()
            ),
        )
        .unwrap();
    }
}

fn test_runtime(cfg: &RunnerConfig) -> RunnerRuntimeState {
    RunnerRuntimeState::new(cfg, PathBuf::new())
}

fn wait_for_path(path: &Path, deadline: Instant, context: &str) {
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {context}: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn runtime_shutdown_is_fast_ordered_and_runs_once_without_resources() {
    let cfg = test_runner_config("http://127.0.0.1:1".to_string());
    let runtime =
        RunnerRuntimeState::with_shutdown_budget(&cfg, PathBuf::new(), Duration::from_millis(300));
    let started = Instant::now();
    let first = runtime.shutdown();
    let second = runtime.shutdown();
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "empty shutdown was not fast"
    );
    assert_eq!(runtime.coordinator.run_count(), 1);
    assert_eq!(first.phases, second.phases);
    assert_eq!(
        first
            .phases
            .iter()
            .map(|phase| phase.phase)
            .collect::<Vec<_>>(),
        vec![
            "signal_received",
            "stop_accepting_work",
            "config_reload_stop",
            "queued_jobs_cancel",
            "active_jobs_signal",
            "active_jobs_drain",
            "external_providers_stop",
            "lsp_servers_stop",
            "background_threads_join",
            "shutdown_complete",
        ]
    );
    let lines = first.log_lines();
    assert_eq!(lines.len(), 1, "idle shutdown should stay concise");
    assert!(lines[0].starts_with("webcodex-runner shutdown complete "));
}

#[test]
fn runtime_completion_log_follows_bounded_background_cleanup() {
    let cfg = test_runner_config("http://127.0.0.1:1".to_string());
    let runtime =
        RunnerRuntimeState::with_shutdown_budget(&cfg, PathBuf::new(), Duration::from_millis(500));
    let (background_ready_tx, background_ready_rx) = std::sync::mpsc::channel();
    let (background_release_tx, background_release_rx) = std::sync::mpsc::channel();
    runtime.register_background_thread(thread::spawn(move || {
        background_ready_tx.send(()).unwrap();
        background_release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test must release background cleanup");
    }));
    background_ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("background cleanup fixture did not start");

    let shutdown_runtime = runtime.clone();
    let (report_tx, report_rx) = std::sync::mpsc::channel();
    let shutdown = thread::spawn(move || {
        report_tx.send(shutdown_runtime.shutdown()).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while !runtime.config.is_stopping() {
        assert!(
            Instant::now() < deadline,
            "shutdown never entered stop-accepting phase"
        );
        thread::yield_now();
    }
    assert!(
        matches!(
            report_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ),
        "shutdown completed while registered background cleanup was still blocked"
    );
    background_release_tx.send(()).unwrap();
    let report = report_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("shutdown did not complete after background cleanup was released");
    shutdown.join().unwrap();

    let background = report
        .phases
        .iter()
        .find(|phase| phase.phase == "background_threads_join")
        .unwrap();
    assert_eq!(
        background.status,
        super::super::shutdown::ShutdownPhaseStatus::Completed
    );
    let lines = report.log_lines();
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with("webcodex-runner shutdown complete "),
        "completion log was not last"
    );
}

#[test]
fn runtime_shutdown_global_budget_bounds_unjoinable_background_thread() {
    let cfg = test_runner_config("http://127.0.0.1:1".to_string());
    let budget = Duration::from_millis(80);
    let runtime = RunnerRuntimeState::with_shutdown_budget(&cfg, PathBuf::new(), budget);
    runtime.register_background_thread(thread::spawn(|| {
        thread::sleep(Duration::from_millis(400));
    }));

    let started = Instant::now();
    let report = runtime.shutdown();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(250),
        "global shutdown budget was not enforced: {elapsed:?}"
    );
    let background = report
        .phases
        .iter()
        .find(|phase| phase.phase == "background_threads_join")
        .unwrap();
    assert_eq!(
        background.status,
        super::super::shutdown::ShutdownPhaseStatus::TimedOut
    );
    assert_eq!(runtime.coordinator.run_count(), 1);
    assert!(
        report
            .log_lines()
            .last()
            .unwrap()
            .starts_with("webcodex-runner shutdown complete "),
        "completion must be emitted after the timed-out cleanup attempt"
    );
}

#[test]
fn runtime_shutdown_wakes_and_joins_reload_listener() {
    let cfg = test_runner_config("http://127.0.0.1:1".to_string());
    let runtime =
        RunnerRuntimeState::with_shutdown_budget(&cfg, PathBuf::new(), Duration::from_millis(500));
    let config = Arc::clone(&runtime.config);
    runtime.register_reload_thread(thread::spawn(move || {
        while !config.is_stopping() {
            thread::yield_now();
        }
    }));
    let report = runtime.shutdown();
    let reload = report
        .phases
        .iter()
        .find(|phase| phase.phase == "config_reload_stop")
        .unwrap();
    assert_eq!(
        reload.status,
        super::super::shutdown::ShutdownPhaseStatus::Completed
    );
    assert_eq!(reload.resources, 1);
}

fn test_project(id: &str) -> RunnerProjectSummary {
    RunnerProjectSummary {
        id: id.to_string(),
        name: Some(id.to_string()),
        path: format!("/tmp/{}", id),
        allow_patch: true,
        kind: Some("repo".to_string()),
        registration_source: None,
        description: None,
        hooks: vec!["check".to_string()],
        disabled: false,
        revision: None,
        git_branch: None,
        git_head: None,
        git_dirty: None,
        updated_at: 123,
        shell_profile: None,
    }
}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).expect("read request");
        assert!(n > 0, "client closed before sending a complete request");
        buf.extend_from_slice(&chunk[..n]);
        let Some(end) = header_end(&buf) else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buf[..end]);
        let expected = end + 4 + content_length(&headers);
        if buf.len() >= expected {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn read_async_http_headers(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut bytes = Vec::new();
    loop {
        assert!(bytes.len() < 64 * 1024, "test HTTP header exceeded bound");
        let mut byte = [0u8; 1];
        let read = stream.read(&mut byte).await.unwrap();
        assert!(read > 0, "peer closed before HTTP headers completed");
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

fn request_path(request: &str) -> &str {
    request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
}

fn write_http_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        status,
        content_type,
        body.len(),
        body
    )
    .unwrap();
}

fn start_polling_http_server(
    poll_status: &str,
    poll_content_type: &str,
    poll_body: &str,
    once: bool,
) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let poll_count = Arc::new(AtomicUsize::new(0));
    let server_poll_count = Arc::clone(&poll_count);
    let poll_status = poll_status.to_string();
    let poll_content_type = poll_content_type.to_string();
    let poll_body = poll_body.to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        assert_eq!(request_path(&request), "/api/shell/agent/register");
        let response = register_inventory_support_response();
        write_http_response(
            &mut stream,
            response.status,
            response.content_type,
            &response.body,
        );

        if once {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert_eq!(request_path(&request), "/api/shell/agent/poll");
            let body = request
                .find("\r\n\r\n")
                .map(|index| &request[index + 4..])
                .unwrap_or_default();
            let response = project_inventory_poll_response(body)
                .expect("once-mode first poll must publish canonical project inventory");
            server_poll_count.fetch_add(1, Ordering::SeqCst);
            write_http_response(
                &mut stream,
                response.status,
                response.content_type,
                &response.body,
            );
        } else {
            let mut stream = accept_business_poll(&listener);
            server_poll_count.fetch_add(1, Ordering::SeqCst);
            write_http_response(&mut stream, &poll_status, &poll_content_type, &poll_body);
        }
    });
    (format!("http://{}", addr), poll_count, server)
}

fn start_auto_fallback_http_server(
    poll_status: &str,
    poll_content_type: &str,
    poll_body: &str,
) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let poll_count = Arc::new(AtomicUsize::new(0));
    let server_poll_count = Arc::clone(&poll_count);
    let poll_status = poll_status.to_string();
    let poll_content_type = poll_content_type.to_string();
    let poll_body = poll_body.to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        assert_eq!(request_path(&request), "/api/agents/ws");
        write_http_response(
            &mut stream,
            "503 Service Unavailable",
            "text/plain",
            "websocket unavailable",
        );

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        assert_eq!(request_path(&request), "/api/shell/agent/register");
        let response = register_inventory_support_response();
        write_http_response(
            &mut stream,
            response.status,
            response.content_type,
            &response.body,
        );

        let mut stream = accept_business_poll(&listener);
        server_poll_count.fetch_add(1, Ordering::SeqCst);
        write_http_response(&mut stream, &poll_status, &poll_content_type, &poll_body);

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        assert_eq!(request_path(&request), "/api/shell/agent/register");
        let response = register_inventory_support_response();
        write_http_response(
            &mut stream,
            response.status,
            response.content_type,
            &response.body,
        );

        let mut stream = accept_business_poll(&listener);
        server_poll_count.fetch_add(1, Ordering::SeqCst);
        write_http_response(
            &mut stream,
            "404 Not Found",
            "application/json",
            r#"{"success":false,"error":"poll endpoint missing"}"#,
        );
    });
    (format!("http://{}", addr), poll_count, server)
}

fn run_polling_runner_against_server(
    poll_status: &str,
    poll_content_type: &str,
    poll_body: &str,
    once: bool,
) -> (Result<(), String>, usize) {
    let (server_url, poll_count, server) =
        start_polling_http_server(poll_status, poll_content_type, poll_body, once);
    let tmp = tempfile::tempdir().unwrap();
    let cfg = polling_runner_config(server_url, tmp.path().join("project-registry"));
    let runtime = test_runtime(&cfg);
    let shutdown = Arc::new(AtomicBool::new(false));
    let failsafe = Arc::clone(&shutdown);
    let (failsafe_cancel_tx, failsafe_cancel_rx) = std::sync::mpsc::channel();
    let failsafe_thread = thread::spawn(move || {
        if failsafe_cancel_rx
            .recv_timeout(Duration::from_secs(2))
            .is_err()
        {
            failsafe.store(true, Ordering::SeqCst);
        }
    });
    let result = run_polling_runner_with_shutdown(cfg, once, "inst-poll-test", shutdown, &runtime);
    let _ = failsafe_cancel_tx.send(());
    failsafe_thread.join().unwrap();
    server.join().unwrap();
    (result, poll_count.load(Ordering::SeqCst))
}

/// Scripted step for the sequential fake agent HTTP server. Each step
/// asserts the endpoint the runner is expected to call next, which is
/// what distinguishes "kept polling" from "re-registered or resubmitted".
enum ScriptStep {
    Register,
    RegisterResponse {
        status: &'static str,
        body: &'static str,
    },
    RegisterTypedResponse {
        status: &'static str,
        content_type: &'static str,
        body: &'static str,
    },
    PollDeliver(&'static str),
    #[cfg(unix)]
    PollDeliverRequest(RunnerRequest),
    PollEmpty,
    PollResponse {
        status: &'static str,
        body: &'static str,
    },
    PollTypedResponse {
        status: &'static str,
        content_type: &'static str,
        body: &'static str,
    },
    PollOversized {
        declared_len: usize,
    },
    PollClose,
    Result {
        status: &'static str,
        body: &'static str,
    },
}

impl ScriptStep {
    fn expected_path(&self) -> &'static str {
        match self {
            Self::Register | Self::RegisterResponse { .. } | Self::RegisterTypedResponse { .. } => {
                "/api/shell/agent/register"
            }
            Self::PollDeliver(_)
            | Self::PollEmpty
            | Self::PollResponse { .. }
            | Self::PollTypedResponse { .. }
            | Self::PollOversized { .. }
            | Self::PollClose => "/api/shell/agent/poll",
            #[cfg(unix)]
            Self::PollDeliverRequest(_) => "/api/shell/agent/poll",
            Self::Result { .. } => "/api/shell/agent/result",
        }
    }
}

struct ScriptedServer {
    server_url: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    shutdown: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

struct ConcurrentHttpResponse {
    status: &'static str,
    content_type: &'static str,
    body: String,
}

impl ConcurrentHttpResponse {
    fn json(body: impl Into<String>) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: body.into(),
        }
    }
}

struct ConcurrentPollingServer {
    server_url: String,
    done: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl ConcurrentPollingServer {
    fn finish(self) {
        self.done.store(true, Ordering::SeqCst);
        self.handle.join().unwrap();
    }
}

struct PollingRunnerHandle {
    result_rx: std::sync::mpsc::Receiver<Result<(), String>>,
    handle: thread::JoinHandle<()>,
}

impl PollingRunnerHandle {
    fn assert_pending(&self, context: &str) {
        match self.result_rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("{context}: polling runner disconnected before reporting a result")
            }
            Ok(result) => panic!("{context}: polling runner returned early: {result:?}"),
        }
    }

    fn finish(self, timeout: Duration, context: &str) -> Result<(), String> {
        match self.result_rx.recv_timeout(timeout) {
            Ok(result) => {
                if self.handle.join().is_err() {
                    panic!("{context}: polling runner thread panicked after reporting its result");
                }
                result
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if self.handle.join().is_err() {
                    panic!("{context}: polling runner thread panicked before reporting its result");
                }
                panic!("{context}: polling runner exited without reporting a result");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("{context}: polling runner exceeded hard timeout {timeout:?}");
            }
        }
    }
}

fn spawn_polling_runner(
    cfg: RunnerConfig,
    runtime: RunnerRuntimeState,
    once: bool,
    instance_id: &str,
    shutdown: Arc<AtomicBool>,
) -> PollingRunnerHandle {
    let instance_id = instance_id.to_string();
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let result = run_polling_runner_with_shutdown(cfg, once, &instance_id, shutdown, &runtime);
        let _ = result_tx.send(result);
    });
    PollingRunnerHandle { result_rx, handle }
}

fn start_concurrent_polling_server(
    handler: Arc<dyn Fn(&str, &str) -> ConcurrentHttpResponse + Send + Sync>,
) -> ConcurrentPollingServer {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let server_done = Arc::clone(&done);
    let handle = thread::spawn(move || {
        let mut connections = Vec::new();
        while !server_done.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // The listener is nonblocking only so this fixture can poll
                    // its shutdown flag. Accepted connections use the blocking
                    // read_http_request helper with a bounded read timeout; make
                    // that contract explicit because Windows may inherit the
                    // listener's nonblocking mode onto accepted sockets.
                    stream.set_nonblocking(false).unwrap();
                    let handler = Arc::clone(&handler);
                    connections.push(thread::spawn(move || {
                        let request = read_http_request(&mut stream);
                        let path = request_path(&request).to_string();
                        let body = request
                            .find("\r\n\r\n")
                            .map(|index| request[index + 4..].to_string())
                            .unwrap_or_default();
                        let response = with_project_inventory_ack(&body, handler(&path, &body));
                        write_http_response(
                            &mut stream,
                            response.status,
                            response.content_type,
                            &response.body,
                        );
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("concurrent polling server accept failed: {error}"),
            }
        }
        for connection in connections {
            connection.join().unwrap();
        }
    });
    ConcurrentPollingServer {
        server_url: format!("http://{addr}"),
        done,
        handle,
    }
}

fn sync_file_request(request_id: &str) -> RunnerRequest {
    RunnerRequest {
        request_id: request_id.to_string(),
        client_id: "oe".to_string(),
        kind: "file_read".to_string(),
        job_id: None,
        cwd: None,
        path: None,
        content: None,
        max_bytes: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: None,
        end_line: None,
        create_dirs: false,
        command: String::new(),
        process: None,
        script: None,
        stdin: None,
        timeout_secs: 5,
        requested_by: "tester".to_string(),
        created_at: 0,
        validation: None,
        lsp: None,
        job_context: None,
        mcp_gateway: None,
        plugin_gateway: None,
        coding_agent: None,
        persistent_shell: None,
    }
}

#[cfg(unix)]
fn polling_shell_request(request_id: &str, cwd: &Path, command: String) -> RunnerRequest {
    RunnerRequest {
        request_id: request_id.to_string(),
        client_id: "oe".to_string(),
        kind: "run_shell".to_string(),
        job_id: None,
        cwd: Some(cwd.to_string_lossy().into_owned()),
        path: None,
        content: None,
        max_bytes: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: None,
        end_line: None,
        create_dirs: false,
        command,
        process: None,
        script: None,
        stdin: None,
        timeout_secs: 10,
        requested_by: "tester".to_string(),
        created_at: 0,
        validation: None,
        lsp: None,
        job_context: None,
        mcp_gateway: None,
        plugin_gateway: None,
        coding_agent: None,
        persistent_shell: None,
    }
}

#[cfg(unix)]
fn polling_job_request(
    request_id: &str,
    job_id: &str,
    cwd: &Path,
    command: String,
) -> RunnerRequest {
    let mut request = polling_shell_request(request_id, cwd, command);
    request.kind = "start_job".to_string();
    request.job_id = Some(job_id.to_string());
    request.job_context = Some(crate::test_job_context(cwd, Vec::new()));
    request
}

#[cfg(unix)]
fn polling_persistent_shell_request(
    request_id: &str,
    action: &str,
    shell_id: &str,
    command: Option<String>,
) -> RunnerRequest {
    let mut request = sync_file_request(request_id);
    request.kind = "persistent_shell".to_string();
    request.command = command.clone().unwrap_or_default();
    request.timeout_secs = 30;
    request.persistent_shell = Some(crate::runner_protocol::PersistentShellRequest {
        action: action.to_string(),
        shell_id: shell_id.to_string(),
        workflow_session_id: "wc_sess_polling_e1".to_string(),
        runtime_project_id: "agent:oe:demo".to_string(),
        cwd: None,
        shell: Some("bash".to_string()),
        command,
        timeout_secs: Some(30),
        purpose: Some("test".to_string()),
    });
    request
}

#[cfg(unix)]
fn posix_quote(value: &Path) -> String {
    super::super::shell::shell_quote(&value.to_string_lossy())
}

#[cfg(unix)]
fn gated_marker_command(started: &Path, release: &Path, marker: &Path, value: &str) -> String {
    format!(
        "printf '%s\\n' '{}' >> {}; : > {}; while [ ! -f {} ]; do sleep 0.01; done; printf '%s\\n' '{}'",
        value,
        posix_quote(marker),
        posix_quote(started),
        posix_quote(release),
        value,
    )
}

fn poll_delivery_response(request: Option<&RunnerRequest>) -> ConcurrentHttpResponse {
    let request = request
        .map(serde_json::to_value)
        .transpose()
        .unwrap()
        .unwrap_or(serde_json::Value::Null);
    ConcurrentHttpResponse::json(
        serde_json::json!({"success": true, "request": request, "error": null}).to_string(),
    )
}

fn register_success_response() -> ConcurrentHttpResponse {
    register_inventory_support_response()
}

fn register_inventory_support_response() -> ConcurrentHttpResponse {
    ConcurrentHttpResponse::json(
        serde_json::json!({
            "success": true,
            "client": {
                "client_id": "oe",
                "agent_instance_id": "inst-project-inventory",
                "status": "online",
                "connected": true,
                "last_seen": 1,
                "capabilities": {},
                "pending_requests": 0,
                "projects": [],
                "agent_protocol_generation": RUNNER_PROTOCOL_GENERATION_V2.get(),
                "project_inventory": {
                    "sync_state": "pending",
                    "generation": null,
                    "total_reported": null,
                    "total_synced": 0,
                    "last_error_code": null,
                    "last_sync_at": null,
                    "max_summaries_per_page": PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
                    "max_serialized_bytes_per_page": PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES
                }
            },
            "error": null
        })
        .to_string(),
    )
}

fn poll_inventory_response(status: &ShellProjectInventoryStatus) -> ConcurrentHttpResponse {
    ConcurrentHttpResponse::json(
        serde_json::json!({
            "success": true,
            "request": null,
            "error": null,
            "project_inventory": status
        })
        .to_string(),
    )
}

fn project_inventory_poll_response(body: &str) -> Option<ConcurrentHttpResponse> {
    let payload: serde_json::Value = serde_json::from_str(body).ok()?;
    let page = payload.get("project_inventory_page")?.as_object()?;
    let generation = page.get("generation")?.as_str()?;
    let total_reported = page.get("total_reported")?.as_u64()? as usize;
    let complete = page.get("complete")?.as_bool()?;
    let page_index = page.get("page_index")?.as_u64()? as usize;
    let page_len = page.get("projects")?.as_array()?.len();
    let total_synced = if complete {
        total_reported
    } else {
        (page_index * PROJECT_INVENTORY_PAGE_MAX_SUMMARIES + page_len).min(total_reported)
    };
    Some(poll_inventory_response(&inventory_status(
        if complete { "complete" } else { "in_progress" },
        generation,
        total_reported,
        total_synced,
    )))
}

fn with_project_inventory_ack(
    request_body: &str,
    mut response: ConcurrentHttpResponse,
) -> ConcurrentHttpResponse {
    if response.status != "200 OK" || response.content_type != "application/json" {
        return response;
    }
    let Some(inventory_response) = project_inventory_poll_response(request_body) else {
        return response;
    };
    let Ok(mut response_json) = serde_json::from_str::<serde_json::Value>(&response.body) else {
        return response;
    };
    let Some(response_object) = response_json.as_object_mut() else {
        return response;
    };
    if response_object.contains_key("project_inventory") {
        return response;
    }
    let inventory_json: serde_json::Value = serde_json::from_str(&inventory_response.body).unwrap();
    response_object.insert(
        "project_inventory".to_string(),
        inventory_json["project_inventory"].clone(),
    );
    response.body = response_json.to_string();
    response
}

fn accept_business_poll(listener: &StdTcpListener) -> TcpStream {
    loop {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        assert_eq!(request_path(&request), "/api/shell/agent/poll");
        let body = request
            .find("\r\n\r\n")
            .map(|index| &request[index + 4..])
            .unwrap_or_default();
        if let Some(response) = project_inventory_poll_response(body) {
            write_http_response(
                &mut stream,
                response.status,
                response.content_type,
                &response.body,
            );
            continue;
        }
        return stream;
    }
}

fn result_success_response() -> ConcurrentHttpResponse {
    ConcurrentHttpResponse::json(r#"{"success":true}"#)
}

fn polling_offline_success_response() -> ConcurrentHttpResponse {
    ConcurrentHttpResponse::json(r#"{"success":true,"error":null}"#)
}

#[cfg(unix)]
fn job_update_success_response() -> ConcurrentHttpResponse {
    ConcurrentHttpResponse::json(r#"{"success":true,"job":null,"error":null}"#)
}

fn accept_with_deadline(listener: &StdTcpListener, deadline: Duration) -> TcpStream {
    let start = Instant::now();
    listener.set_nonblocking(true).unwrap();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                listener.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    start.elapsed() < deadline,
                    "scripted server timed out waiting for the next agent request"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("scripted server accept failed: {e}"),
        }
    }
}

fn start_scripted_runner_server(steps: Vec<ScriptStep>) -> ScriptedServer {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = Arc::clone(&shutdown);
    let handle = thread::spawn(move || {
        let mut steps = steps.into_iter().map(Some).collect::<Vec<_>>();
        let mut remaining = steps.len();
        while remaining > 0 {
            let mut stream = accept_with_deadline(&listener, Duration::from_secs(8));
            let request = read_http_request(&mut stream);
            let path = request_path(&request).to_string();
            let body = request
                .find("\r\n\r\n")
                .map(|index| request[index + 4..].to_string())
                .unwrap_or_default();
            if path == "/api/shell/agent/poll" {
                if let Some(response) = project_inventory_poll_response(&body) {
                    write_http_response(
                        &mut stream,
                        response.status,
                        response.content_type,
                        &response.body,
                    );
                    continue;
                }
            }
            recorded.lock().unwrap().push((path.clone(), body.clone()));
            let Some(index) = steps.iter().position(|step| {
                step.as_ref()
                    .is_some_and(|step| step.expected_path() == path)
            }) else {
                // Normal background dispatch may issue the next poll before a
                // fast worker reaches its scripted result response. When the
                // remaining script contains only result work for that turn,
                // answer the speculative poll as empty without consuming or
                // reordering the result script.
                if path == "/api/shell/agent/poll"
                    && steps.iter().any(|step| {
                        step.as_ref()
                            .is_some_and(|step| step.expected_path() == "/api/shell/agent/result")
                    })
                {
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"{"success":true,"request":null,"error":null}"#,
                    );
                    continue;
                }
                panic!("scripted server has no remaining response for {path}");
            };
            let step = steps[index].take().expect("matched scripted step");
            remaining -= 1;
            match step {
                ScriptStep::Register => {
                    assert_eq!(path, "/api/shell/agent/register");
                    let response = register_inventory_support_response();
                    write_http_response(
                        &mut stream,
                        response.status,
                        response.content_type,
                        &response.body,
                    );
                }
                ScriptStep::RegisterResponse { status, body } => {
                    assert_eq!(path, "/api/shell/agent/register");
                    write_http_response(&mut stream, status, "application/json", body);
                }
                ScriptStep::RegisterTypedResponse {
                    status,
                    content_type,
                    body,
                } => {
                    assert_eq!(path, "/api/shell/agent/register");
                    write_http_response(&mut stream, status, content_type, body);
                }
                ScriptStep::PollDeliver(request_id) => {
                    assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                    let request_json =
                        serde_json::to_string(&sync_file_request(request_id)).unwrap();
                    let body = format!(
                        r#"{{"success":true,"request":{},"error":null}}"#,
                        request_json
                    );
                    write_http_response(&mut stream, "200 OK", "application/json", &body);
                }
                #[cfg(unix)]
                ScriptStep::PollDeliverRequest(request) => {
                    assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                    let request_json = serde_json::to_string(&request).unwrap();
                    let body = format!(
                        r#"{{"success":true,"request":{},"error":null}}"#,
                        request_json
                    );
                    write_http_response(&mut stream, "200 OK", "application/json", &body);
                }
                ScriptStep::PollEmpty => {
                    assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"{"success":true,"request":null,"error":null}"#,
                    );
                }
                ScriptStep::PollResponse { status, body } => {
                    assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                    write_http_response(&mut stream, status, "application/json", body);
                }
                ScriptStep::PollTypedResponse {
                    status,
                    content_type,
                    body,
                } => {
                    assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                    write_http_response(&mut stream, status, content_type, body);
                }
                ScriptStep::PollOversized { declared_len } => {
                    assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                    write!(
                            stream,
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            declared_len
                        )
                        .unwrap();
                }
                ScriptStep::PollClose => {
                    assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                    // Dropping the accepted socket without a response
                    // exercises connection-closed / early-EOF recovery.
                }
                ScriptStep::Result { status, body } => {
                    assert_eq!(
                        path, "/api/shell/agent/result",
                        "expected result submission, got {path}"
                    );
                    write_http_response(&mut stream, status, "application/json", body);
                }
            }
        }
        server_shutdown.store(true, Ordering::SeqCst);
    });
    ScriptedServer {
        server_url: format!("http://{}", addr),
        requests,
        shutdown,
        handle,
    }
}

fn run_polling_runner_against_scripted_server(
    server: &ScriptedServer,
    once: bool,
) -> Result<(), String> {
    let runner_shutdown = Arc::clone(&server.shutdown);
    let failsafe_shutdown = Arc::clone(&server.shutdown);
    let server_url = server.server_url.clone();
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let runner = thread::spawn(move || {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = polling_runner_config(server_url, tmp.path().join("project-registry"));
        let runtime = test_runtime(&cfg);
        let result =
            run_polling_runner_with_shutdown(cfg, once, "inst-script", runner_shutdown, &runtime);
        let _ = result_tx.send(result);
    });
    match result_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(result) => {
            runner
                .join()
                .expect("scripted polling runner thread panicked after reporting its result");
            result
        }
        Err(error @ std::sync::mpsc::RecvTimeoutError::Timeout) => {
            failsafe_shutdown.store(true, Ordering::SeqCst);
            panic!("scripted polling runner exceeded hard timeout: {error}");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            runner
                .join()
                .expect("scripted polling runner thread panicked before reporting its result");
            panic!("scripted polling runner exited without reporting a result");
        }
    }
}

fn recorded_result_bodies(requests: &Mutex<Vec<(String, String)>>) -> Vec<String> {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/api/shell/agent/result")
        .map(|(_, body)| body.clone())
        .collect()
}

fn recorded_paths(requests: &Mutex<Vec<(String, String)>>) -> Vec<String> {
    requests
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _)| path.clone())
        .collect()
}

fn recorded_path_count(requests: &Mutex<Vec<(String, String)>>, expected: &str) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == expected)
        .count()
}

#[cfg(unix)]
#[test]
fn polling_long_ordinary_dispatch_does_not_pin_and_results_stay_correlated_exactly_once() {
    let temp = tempfile::tempdir().unwrap();
    let started_a = temp.path().join("a-started");
    let release_a = temp.path().join("a-release");
    let marker_a = temp.path().join("a-marker");
    let request_a = polling_shell_request(
        "req-slow-a",
        temp.path(),
        gated_marker_command(&started_a, &release_a, &marker_a, "dispatch-a"),
    );
    let request_b = polling_shell_request(
        "req-fast-b",
        temp.path(),
        "printf '%s\\n' 'dispatch-b'".to_string(),
    );

    let poll_count = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let runner_shutdown = Arc::new(AtomicBool::new(false));
    let (event_tx, event_rx) = std::sync::mpsc::channel::<String>();
    let handler = {
        let poll_count = Arc::clone(&poll_count);
        let results = Arc::clone(&results);
        let runner_shutdown = Arc::clone(&runner_shutdown);
        let event_tx = event_tx.clone();
        let request_a = request_a.clone();
        let request_b = request_b.clone();
        Arc::new(move |path: &str, body: &str| match path {
            "/api/shell/agent/register" => register_success_response(),
            "/api/shell/agent/poll" => {
                let index = poll_count.fetch_add(1, Ordering::SeqCst);
                match index {
                    0 => poll_delivery_response(Some(&request_a)),
                    1 => {
                        let _ = event_tx.send("poll-b".to_string());
                        poll_delivery_response(Some(&request_b))
                    }
                    _ => poll_delivery_response(None),
                }
            }
            "/api/shell/agent/result" => {
                let body: serde_json::Value = serde_json::from_str(body).unwrap();
                let request_id = body["request_id"].as_str().unwrap().to_string();
                results.lock().unwrap().push(body);
                let _ = event_tx.send(format!("result-{request_id}"));
                if request_id == "req-slow-a" {
                    runner_shutdown.store(true, Ordering::SeqCst);
                }
                result_success_response()
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected polling test endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    let cfg = polling_runner_config(
        server.server_url.clone(),
        temp.path().join("project-registry"),
    );
    let runtime = test_runtime(&cfg);
    let runner = spawn_polling_runner(
        cfg,
        runtime.clone(),
        false,
        "inst-e1-correlation",
        Arc::clone(&runner_shutdown),
    );

    assert_eq!(
        event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        "poll-b",
        "the next poll must reach the Server before A is released"
    );
    wait_for_path(
        &started_a,
        Instant::now() + Duration::from_secs(5),
        "slow request A to start",
    );
    assert_eq!(
        event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        "result-req-fast-b",
        "B must complete while A is still blocked"
    );
    assert!(!release_a.exists());
    std::fs::write(&release_a, "release\n").unwrap();
    assert_eq!(
        event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        "result-req-slow-a"
    );

    runner
        .finish(Duration::from_secs(10), "polling correlation runner")
        .expect("polling runner should shut down cleanly");
    server.finish();

    let results = results.lock().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["request_id"], "req-fast-b");
    assert_eq!(results[0]["stdout"], "dispatch-b\n");
    assert_eq!(results[1]["request_id"], "req-slow-a");
    assert_eq!(results[1]["stdout"], "dispatch-a\n");
    assert_eq!(
        std::fs::read_to_string(&marker_a).unwrap().lines().count(),
        1,
        "poll continuation and out-of-order completion must not replay A"
    );
    assert!(poll_count.load(Ordering::SeqCst) >= 2);
    assert_eq!(runtime.dispatches.active(), 0);
    assert_eq!(runtime.background_threads.pending(), 0);
}

#[cfg(unix)]
#[test]
fn polling_dispatch_bound_backpressures_without_a_local_pending_queue() {
    let temp = tempfile::tempdir().unwrap();
    let mut requests = Vec::new();
    let mut started = Vec::new();
    let mut releases = Vec::new();
    let mut markers = Vec::new();
    for label in ["a", "b", "c", "d", "e"] {
        let started_path = temp.path().join(format!("{label}-started"));
        let release_path = temp.path().join(format!("{label}-release"));
        let marker_path = temp.path().join(format!("{label}-marker"));
        requests.push(polling_shell_request(
            &format!("req-bound-{label}"),
            temp.path(),
            gated_marker_command(
                &started_path,
                &release_path,
                &marker_path,
                &format!("bound-{label}"),
            ),
        ));
        started.push(started_path);
        releases.push(release_path);
        markers.push(marker_path);
    }

    let poll_count = Arc::new(AtomicUsize::new(0));
    let result_count = Arc::new(AtomicUsize::new(0));
    let runner_shutdown = Arc::new(AtomicBool::new(false));
    let (fifth_poll_tx, fifth_poll_rx) = std::sync::mpsc::sync_channel(1);
    let handler = {
        let poll_count = Arc::clone(&poll_count);
        let result_count = Arc::clone(&result_count);
        let runner_shutdown = Arc::clone(&runner_shutdown);
        let requests = requests.clone();
        Arc::new(move |path: &str, _body: &str| match path {
            "/api/shell/agent/register" => register_success_response(),
            "/api/shell/agent/poll" => {
                let index = poll_count.fetch_add(1, Ordering::SeqCst);
                if index == POLLING_DISPATCH_MAX_IN_FLIGHT {
                    let _ = fifth_poll_tx.send(());
                }
                poll_delivery_response(requests.get(index))
            }
            "/api/shell/agent/result" => {
                if result_count.fetch_add(1, Ordering::SeqCst) + 1 == requests.len() {
                    runner_shutdown.store(true, Ordering::SeqCst);
                }
                result_success_response()
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected polling bound endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    let cfg = polling_runner_config(
        server.server_url.clone(),
        temp.path().join("project-registry"),
    );
    let runtime = test_runtime(&cfg);
    let runner = spawn_polling_runner(
        cfg,
        runtime.clone(),
        false,
        "inst-polling-bound",
        Arc::clone(&runner_shutdown),
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    for path in &started[..POLLING_DISPATCH_MAX_IN_FLIGHT] {
        wait_for_path(
            path,
            deadline,
            "polling workers up to the fixed bound to start",
        );
    }
    assert_eq!(runtime.dispatches.active(), POLLING_DISPATCH_MAX_IN_FLIGHT);
    assert!(
        fifth_poll_rx
            .recv_timeout(Duration::from_millis(200))
            .is_err(),
        "the Runner dequeued an N+1 request while all polling dispatch slots were occupied"
    );

    std::fs::write(&releases[0], "release\n").unwrap();
    fifth_poll_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("releasing one slot must allow exactly the N+1 poll");
    wait_for_path(
        &started[POLLING_DISPATCH_MAX_IN_FLIGHT],
        Instant::now() + Duration::from_secs(5),
        "N+1 polling worker to start",
    );
    assert_eq!(
        runtime.dispatches.active(),
        POLLING_DISPATCH_MAX_IN_FLIGHT,
        "active polling dispatches exceeded the fixed bound"
    );
    for release in &releases[1..] {
        std::fs::write(release, "release\n").unwrap();
    }

    runner
        .finish(Duration::from_secs(10), "bounded polling runner")
        .expect("bounded polling runner should shut down cleanly");
    server.finish();
    assert_eq!(result_count.load(Ordering::SeqCst), requests.len());
    for marker in markers {
        assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    }
    assert_eq!(runtime.dispatches.active(), 0);
    assert_eq!(runtime.background_threads.pending(), 0);
}

#[cfg(unix)]
#[test]
fn polling_job_start_dispatches_behind_one_long_ordinary_request() {
    let temp = tempfile::tempdir().unwrap();
    let started_a = temp.path().join("ordinary-started");
    let release_a = temp.path().join("ordinary-release");
    let marker_a = temp.path().join("ordinary-marker");
    let job_marker = temp.path().join("job-marker");
    let request_a = polling_shell_request(
        "req-ordinary",
        temp.path(),
        gated_marker_command(&started_a, &release_a, &marker_a, "ordinary"),
    );
    let request_b = polling_job_request(
        "req-job-start",
        "job-behind-ordinary",
        temp.path(),
        format!("printf '%s\\n' 'job-ran' > {}", posix_quote(&job_marker)),
    );

    let poll_count = Arc::new(AtomicUsize::new(0));
    let runner_shutdown = Arc::new(AtomicBool::new(false));
    let ordinary_done = Arc::new(AtomicBool::new(false));
    let job_done = Arc::new(AtomicBool::new(false));
    let (job_tx, job_rx) = std::sync::mpsc::sync_channel(1);
    let handler = {
        let poll_count = Arc::clone(&poll_count);
        let runner_shutdown = Arc::clone(&runner_shutdown);
        let ordinary_done = Arc::clone(&ordinary_done);
        let job_done = Arc::clone(&job_done);
        let request_a = request_a.clone();
        let request_b = request_b.clone();
        Arc::new(move |path: &str, body: &str| match path {
            "/api/shell/agent/register" => register_success_response(),
            "/api/shell/agent/poll" => {
                let index = poll_count.fetch_add(1, Ordering::SeqCst);
                match index {
                    0 => poll_delivery_response(Some(&request_a)),
                    1 => poll_delivery_response(Some(&request_b)),
                    _ => poll_delivery_response(None),
                }
            }
            "/api/shell/agent/job_update" => {
                let update: serde_json::Value = serde_json::from_str(body).unwrap();
                if update["job_id"] == "job-behind-ordinary"
                    && update["finished"] == true
                    && !job_done.swap(true, Ordering::SeqCst)
                {
                    let _ = job_tx.send(());
                    if ordinary_done.load(Ordering::SeqCst) {
                        runner_shutdown.store(true, Ordering::SeqCst);
                    }
                }
                job_update_success_response()
            }
            "/api/shell/agent/result" => {
                ordinary_done.store(true, Ordering::SeqCst);
                if job_done.load(Ordering::SeqCst) {
                    runner_shutdown.store(true, Ordering::SeqCst);
                }
                result_success_response()
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected Job-behind-ordinary endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    let cfg = polling_runner_config(
        server.server_url.clone(),
        temp.path().join("project-registry"),
    );
    let runtime = test_runtime(&cfg);
    let runner = spawn_polling_runner(
        cfg,
        runtime.clone(),
        false,
        "inst-e1-job",
        Arc::clone(&runner_shutdown),
    );

    job_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("Job B must finish while ordinary A remains gated");
    assert!(started_a.exists());
    assert!(!release_a.exists());
    assert_eq!(std::fs::read_to_string(&job_marker).unwrap(), "job-ran\n");
    std::fs::write(&release_a, "release\n").unwrap();

    runner
        .finish(Duration::from_secs(10), "Job-behind-ordinary runner")
        .expect("Job-behind-ordinary runner should shut down cleanly");
    server.finish();
    assert_eq!(
        std::fs::read_to_string(&marker_a).unwrap().lines().count(),
        1
    );
    assert_eq!(runtime.dispatches.active(), 0);
    assert_eq!(runtime.background_threads.pending(), 0);
}

#[cfg(unix)]
#[test]
fn polling_once_waits_for_its_tracked_ordinary_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let started = temp.path().join("once-started");
    let release = temp.path().join("once-release");
    let marker = temp.path().join("once-marker");
    let request = polling_shell_request(
        "req-once-slow",
        temp.path(),
        gated_marker_command(&started, &release, &marker, "once"),
    );
    let poll_count = Arc::new(AtomicUsize::new(0));
    let result_count = Arc::new(AtomicUsize::new(0));
    let handler = {
        let poll_count = Arc::clone(&poll_count);
        let result_count = Arc::clone(&result_count);
        let request = request.clone();
        Arc::new(move |path: &str, _body: &str| match path {
            "/api/shell/agent/register" => register_success_response(),
            "/api/shell/agent/poll" => {
                let index = poll_count.fetch_add(1, Ordering::SeqCst);
                poll_delivery_response((index == 0).then_some(&request))
            }
            "/api/shell/agent/result" => {
                result_count.fetch_add(1, Ordering::SeqCst);
                result_success_response()
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected polling --once endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    let cfg = polling_runner_config(
        server.server_url.clone(),
        temp.path().join("project-registry"),
    );
    let runtime = test_runtime(&cfg);
    let runner = spawn_polling_runner(
        cfg,
        runtime.clone(),
        true,
        "inst-e1-once",
        Arc::new(AtomicBool::new(false)),
    );

    wait_for_path(
        &started,
        Instant::now() + Duration::from_secs(5),
        "--once request to start",
    );
    runner.assert_pending("--once returned while its ordinary dispatch was still active");
    std::fs::write(&release, "release\n").unwrap();
    runner
        .finish(Duration::from_secs(5), "--once polling runner")
        .expect("--once runner should complete successfully");
    server.finish();

    assert_eq!(poll_count.load(Ordering::SeqCst), 1);
    assert_eq!(result_count.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert!(runtime
        .dispatches
        .wait_until(Instant::now() + Duration::from_secs(1)));
    assert!(
        runtime.background_threads.pending() <= 1,
        "the --once worker must remain in the shutdown-owned registry"
    );
    runtime.shutdown();
    assert_eq!(runtime.background_threads.pending(), 0);
}

#[cfg(unix)]
#[test]
fn polling_once_preserves_job_manager_drain_before_exit() {
    let temp = tempfile::tempdir().unwrap();
    let started = temp.path().join("once-job-started");
    let release = temp.path().join("once-job-release");
    let marker = temp.path().join("once-job-marker");
    let request = polling_job_request(
        "req-once-job",
        "job-once-drain",
        temp.path(),
        gated_marker_command(&started, &release, &marker, "once-job"),
    );
    let poll_count = Arc::new(AtomicUsize::new(0));
    let (terminal_tx, terminal_rx) = std::sync::mpsc::sync_channel(1);
    let handler = {
        let poll_count = Arc::clone(&poll_count);
        let request = request.clone();
        Arc::new(move |path: &str, body: &str| match path {
            "/api/shell/agent/register" => register_success_response(),
            "/api/shell/agent/poll" => {
                let index = poll_count.fetch_add(1, Ordering::SeqCst);
                poll_delivery_response((index == 0).then_some(&request))
            }
            "/api/shell/agent/job_update" => {
                let update: serde_json::Value = serde_json::from_str(body).unwrap();
                if update["finished"] == true {
                    let _ = terminal_tx.send(());
                }
                job_update_success_response()
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected polling --once Job endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    let cfg = polling_runner_config(
        server.server_url.clone(),
        temp.path().join("project-registry"),
    );
    let runtime = test_runtime(&cfg);
    let runner = spawn_polling_runner(
        cfg,
        runtime.clone(),
        true,
        "inst-e1-once-job",
        Arc::new(AtomicBool::new(false)),
    );

    wait_for_path(
        &started,
        Instant::now() + Duration::from_secs(5),
        "--once Job to start",
    );
    runner.assert_pending("--once returned before JobManager drained its active Job");
    std::fs::write(&release, "release\n").unwrap();
    terminal_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("--once Job terminal update");
    runner
        .finish(Duration::from_secs(5), "--once Job polling runner")
        .expect("--once Job runner should complete successfully");
    server.finish();

    assert_eq!(poll_count.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert!(!runtime.jobs.has_work());
    runtime.shutdown();
}

#[cfg(unix)]
#[test]
fn polling_shutdown_with_active_background_dispatch_is_bounded_and_non_replaying() {
    let temp = tempfile::tempdir().unwrap();
    let started = temp.path().join("shutdown-started");
    let never_release = temp.path().join("shutdown-release");
    let marker = temp.path().join("shutdown-marker");
    let request = polling_shell_request(
        "req-shutdown-active",
        temp.path(),
        gated_marker_command(&started, &never_release, &marker, "shutdown"),
    );
    let poll_count = Arc::new(AtomicUsize::new(0));
    let result_bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let handler = {
        let poll_count = Arc::clone(&poll_count);
        let result_bodies = Arc::clone(&result_bodies);
        let request = request.clone();
        Arc::new(move |path: &str, body: &str| match path {
            "/api/shell/agent/register" => register_success_response(),
            "/api/shell/agent/poll" => {
                let index = poll_count.fetch_add(1, Ordering::SeqCst);
                poll_delivery_response((index == 0).then_some(&request))
            }
            "/api/shell/agent/result" => {
                result_bodies
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(body).unwrap());
                result_success_response()
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected active-shutdown endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    let cfg = polling_runner_config(
        server.server_url.clone(),
        temp.path().join("project-registry"),
    );
    let runtime =
        RunnerRuntimeState::with_shutdown_budget(&cfg, PathBuf::new(), Duration::from_secs(2));
    let shutdown = Arc::new(AtomicBool::new(false));
    let runner = spawn_polling_runner(
        cfg,
        runtime.clone(),
        false,
        "inst-e1-shutdown",
        Arc::clone(&shutdown),
    );

    wait_for_path(
        &started,
        Instant::now() + Duration::from_secs(5),
        "shutdown fixture dispatch to start",
    );
    let shutdown_started = Instant::now();
    shutdown.store(true, Ordering::SeqCst);
    runner
        .finish(Duration::from_secs(5), "active-shutdown polling runner")
        .expect("active-shutdown runner should exit cleanly");
    assert!(
        shutdown_started.elapsed() < Duration::from_secs(3),
        "shutdown exceeded its bounded cleanup budget"
    );
    let polls_after_completion = poll_count.load(Ordering::SeqCst);
    server.finish();
    assert_eq!(
        poll_count.load(Ordering::SeqCst),
        polls_after_completion,
        "polling continued after shutdown completed"
    );

    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert_eq!(runtime.dispatches.active(), 0);
    assert_eq!(runtime.background_threads.pending(), 0);
    let results = result_bodies.lock().unwrap();
    assert!(results.len() <= 1);
    if let Some(result) = results.first() {
        assert_ne!(
            result["command_execution_state"], "not_started",
            "shutdown after dispatch must not rewrite lifecycle truth as pre-start"
        );
    }
}

#[test]
fn polling_background_project_operation_invalidates_the_project_cache() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let project_registry_dir = temp.path().join("project-registry");
    std::fs::create_dir_all(&project).unwrap();
    let mut request = sync_file_request("req-register-project");
    request.kind = "register_project".to_string();
    request.stdin = Some(
        serde_json::json!({
            "id": "e1-project",
            "name": "E1 project",
            "path": project,
            "allow_patch": true
        })
        .to_string(),
    );

    let poll_count = Arc::new(AtomicUsize::new(0));
    let project_result_seen = Arc::new(AtomicBool::new(false));
    let refreshed_seen = Arc::new(AtomicBool::new(false));
    let runner_shutdown = Arc::new(AtomicBool::new(false));
    let (refreshed_tx, refreshed_rx) = std::sync::mpsc::sync_channel(1);
    let handler = {
        let poll_count = Arc::clone(&poll_count);
        let project_result_seen = Arc::clone(&project_result_seen);
        let refreshed_seen = Arc::clone(&refreshed_seen);
        let runner_shutdown = Arc::clone(&runner_shutdown);
        let request = request.clone();
        Arc::new(move |path: &str, body: &str| match path {
            "/api/shell/agent/register" => register_success_response(),
            "/api/shell/agent/poll" => {
                let payload: serde_json::Value = serde_json::from_str(body).unwrap();
                if let Some(response) = project_inventory_poll_response(body) {
                    let refreshed = payload["project_inventory_page"]["projects"]
                        .as_array()
                        .is_some_and(|projects| {
                            projects.iter().any(|project| project["id"] == "e1-project")
                        });
                    if refreshed
                        && project_result_seen.load(Ordering::SeqCst)
                        && !refreshed_seen.swap(true, Ordering::SeqCst)
                    {
                        let _ = refreshed_tx.send(());
                        runner_shutdown.store(true, Ordering::SeqCst);
                    }
                    response
                } else {
                    assert!(
                        payload["projects"].is_null(),
                        "ordinary business poll must not carry project inventory"
                    );
                    let index = poll_count.fetch_add(1, Ordering::SeqCst);
                    poll_delivery_response((index == 0).then_some(&request))
                }
            }
            "/api/shell/agent/result" => {
                project_result_seen.store(true, Ordering::SeqCst);
                result_success_response()
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected project-cache endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    // Windows service accounts can have std::env::temp_dir() under
    // C:\Windows\SystemTemp; production policy correctly rejects project
    // roots under C:\Windows unless an explicit allowed_roots entry
    // authorizes them. This test exercises polling project-cache
    // invalidation, not project-root safety policy, so authorize this
    // test's own temp root explicitly.
    let mut cfg = polling_runner_config(server.server_url.clone(), project_registry_dir.clone());
    cfg.policy.allowed_roots = vec![temp.path().to_path_buf()];
    let runtime = test_runtime(&cfg);
    let runner = spawn_polling_runner(
        cfg,
        runtime.clone(),
        false,
        "inst-e1-project-cache",
        Arc::clone(&runner_shutdown),
    );

    // The register_project round trip is an actual project operation (it may
    // spawn git); on a loaded runner the poll that observes the refreshed
    // cache can arrive well after a few seconds, so budget generously.
    refreshed_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("a later poll must carry refreshed project metadata");
    runner
        .finish(Duration::from_secs(30), "project-cache polling runner")
        .expect("project-cache runner should shut down cleanly");
    server.finish();
    assert!(project_registry_dir.join("e1-project.toml").exists());
    assert!(poll_count.load(Ordering::SeqCst) >= 2);
    assert!(refreshed_seen.load(Ordering::SeqCst));
}

#[cfg(unix)]
#[test]
fn polling_persistent_shell_exec_remains_responsive_to_close() {
    #[derive(Default)]
    struct PersistentState {
        open_delivered: bool,
        open_done: bool,
        exec_delivered: bool,
        close_delivered: bool,
        result_ids: Vec<String>,
    }

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let project_registry_dir = temp.path().join("project-registry");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&project_registry_dir).unwrap();
    std::fs::write(
        project_registry_dir.join("demo.toml"),
        format!(
            "id = \"demo\"\npath = {:?}\nallow_patch = true\n",
            project.to_string_lossy()
        ),
    )
    .unwrap();
    let started = project.join("persistent-started");
    let marker = project.join("persistent-marker");
    let shell_id = "wc_shell_polling_e1";
    let open = polling_persistent_shell_request("req-ps-open", "open", shell_id, None);
    let exec = polling_persistent_shell_request(
        "req-ps-exec",
        "exec",
        shell_id,
        Some(format!(
            "printf '%s\\n' 'ran' >> {}; : > {}; sleep 30",
            posix_quote(&marker),
            posix_quote(&started)
        )),
    );
    let close = polling_persistent_shell_request("req-ps-close", "close", shell_id, None);
    let state = Arc::new(Mutex::new(PersistentState::default()));
    let allow_close = Arc::new(AtomicBool::new(false));
    let runner_shutdown = Arc::new(AtomicBool::new(false));
    let handler = {
        let state = Arc::clone(&state);
        let allow_close = Arc::clone(&allow_close);
        let runner_shutdown = Arc::clone(&runner_shutdown);
        let open = open.clone();
        let exec = exec.clone();
        let close = close.clone();
        Arc::new(move |path: &str, body: &str| match path {
            "/api/shell/agent/register" => register_success_response(),
            "/api/shell/agent/poll" => {
                let mut state = state.lock().unwrap();
                if !state.open_delivered {
                    state.open_delivered = true;
                    poll_delivery_response(Some(&open))
                } else if state.open_done && !state.exec_delivered {
                    state.exec_delivered = true;
                    poll_delivery_response(Some(&exec))
                } else if state.exec_delivered
                    && allow_close.load(Ordering::SeqCst)
                    && !state.close_delivered
                {
                    state.close_delivered = true;
                    poll_delivery_response(Some(&close))
                } else {
                    poll_delivery_response(None)
                }
            }
            "/api/shell/agent/persistent_shell_result" => {
                let result: serde_json::Value = serde_json::from_str(body).unwrap();
                let request_id = result["request_id"].as_str().unwrap().to_string();
                let mut state = state.lock().unwrap();
                if request_id == "req-ps-open" {
                    state.open_done = true;
                }
                state.result_ids.push(request_id);
                if state.result_ids.iter().any(|id| id == "req-ps-exec")
                    && state.result_ids.iter().any(|id| id == "req-ps-close")
                {
                    runner_shutdown.store(true, Ordering::SeqCst);
                }
                result_success_response()
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected persistent-shell polling endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    let cfg = polling_runner_config(server.server_url.clone(), project_registry_dir);
    let runtime = test_runtime(&cfg);
    let runner = spawn_polling_runner(
        cfg,
        runtime.clone(),
        false,
        "inst-e1-persistent",
        Arc::clone(&runner_shutdown),
    );

    wait_for_path(
        &started,
        Instant::now() + Duration::from_secs(5),
        "persistent-shell exec to start",
    );
    allow_close.store(true, Ordering::SeqCst);
    runner
        .finish(Duration::from_secs(10), "persistent-shell polling runner")
        .expect("persistent-shell polling runner should shut down cleanly");
    server.finish();

    let mut result_ids = state.lock().unwrap().result_ids.clone();
    result_ids.sort();
    assert_eq!(
        result_ids,
        vec![
            "req-ps-close".to_string(),
            "req-ps-exec".to_string(),
            "req-ps-open".to_string(),
        ]
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert_eq!(runtime.persistent_shells.active_count(), 0);
    assert_eq!(runtime.dispatches.active(), 0);
    assert_eq!(runtime.background_threads.pending(), 0);
}

#[test]
fn polling_502_reregisters_once_then_processes_request() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "502 Bad Gateway",
            body: "<html>\n<h1>Bad Gateway</h1>\n</html>",
        },
        ScriptStep::Register,
        ScriptStep::PollDeliver("req-after-502"),
        ScriptStep::Result {
            status: "200 OK",
            body: r#"{"success":true}"#,
        },
        ScriptStep::PollEmpty,
    ]);
    let started = Instant::now();
    run_polling_runner_against_scripted_server(&server, false)
        .expect("a transient poll 502 must recover");
    server.handle.join().unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(450),
        "poll recovery skipped its first backoff"
    );
    let paths = recorded_paths(&server.requests);
    assert_eq!(
        &paths[..3],
        &[
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
        ],
        "the first transient poll failure refreshes the same-instance session once"
    );
    assert_eq!(
        recorded_path_count(&server.requests, "/api/shell/agent/register"),
        2,
        "background dispatch must not create a registration storm"
    );
    assert!(
        recorded_path_count(&server.requests, "/api/shell/agent/poll") >= 3,
        "polling must resume after recovery and continue around result delivery"
    );
    let results = recorded_result_bodies(&server.requests);
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("req-after-502"), "{results:?}");
}

#[test]
fn polling_503_and_504_stay_live_without_registration_storm() {
    for status in ["503 Service Unavailable", "504 Gateway Timeout"] {
        let server = start_scripted_runner_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status,
                body: "proxy unavailable",
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        run_polling_runner_against_scripted_server(&server, false)
            .expect("gateway failure must recover");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ],
            "status {status}"
        );
    }
}

#[test]
fn polling_connection_closed_enters_session_recovery() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollClose,
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("a closed poll connection must recover");
    server.handle.join().unwrap();
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_transient_after_successful_inventory_recovery_starts_a_new_episode() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "502 Bad Gateway",
            body: "bad gateway",
        },
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "503 Service Unavailable",
            body: "unavailable",
        },
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "504 Gateway Timeout",
            body: "timeout",
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    let started = Instant::now();
    run_polling_runner_against_scripted_server(&server, false)
        .expect("gateway failures separated by successful inventory recovery must remain live");
    let elapsed = started.elapsed();
    server.handle.join().unwrap();

    assert!(
        elapsed >= Duration::from_millis(1_350),
        "repeated recovery episodes did not apply their 500ms delays: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "bounded recovery took unexpectedly long: {elapsed:?}"
    );
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ],
        "a successful canonical inventory poll ends the current recovery episode"
    );
}

#[test]
fn polling_truncated_json_recovers_and_stays_live() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "200 OK",
            body: r#"{"success":true,"request":"#,
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("an incomplete poll response must recover");
    server.handle.join().unwrap();
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_register_truncated_json_recovers_and_stays_live() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::RegisterResponse {
            status: "200 OK",
            body: r#"{"success":true,"client":"#,
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("an incomplete register response must recover");
    server.handle.join().unwrap();
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_http_200_html_bad_gateway_recovers() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollTypedResponse {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            body: "<!doctype html>\n<html><h1>Bad Gateway</h1></html>",
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("a poll proxy error page must recover");
    server.handle.join().unwrap();
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_register_http_200_html_service_unavailable_recovers() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::RegisterTypedResponse {
            status: "200 OK",
            content_type: "text/html",
            body: "<html>\n<h1>Service Unavailable</h1>\n</html>",
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("a register proxy error page must recover");
    server.handle.join().unwrap();
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_complete_schema_mismatch_is_terminal_without_recovery() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "200 OK",
            body: r#"{"success":"yes","request":null,"error":null}"#,
        },
    ]);
    let error = run_polling_runner_against_scripted_server(&server, false)
        .expect_err("a complete incompatible poll response must stop");
    server.handle.join().unwrap();
    assert!(
        error.contains("poll response incompatible with server protocol"),
        "{error}"
    );
    assert!(error.contains("serde_category=data"), "{error}");
    assert!(!error.contains("\"yes\""), "{error}");
    assert_eq!(
        recorded_paths(&server.requests),
        vec!["/api/shell/agent/register", "/api/shell/agent/poll"],
        "protocol incompatibility must neither re-register nor poll again"
    );
}

#[test]
fn polling_unknown_json_shape_is_terminal_without_recovery() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "200 OK",
            body: r#"{"unexpected":true}"#,
        },
    ]);
    let error = run_polling_runner_against_scripted_server(&server, false)
        .expect_err("an unknown complete poll shape must stop");
    server.handle.join().unwrap();
    assert!(
        error.contains("poll response incompatible with server protocol"),
        "{error}"
    );
    assert!(error.contains("serde_category=data"), "{error}");
    assert!(!error.contains("unexpected"), "{error}");
    assert_eq!(
        recorded_paths(&server.requests),
        vec!["/api/shell/agent/register", "/api/shell/agent/poll"]
    );
}

#[test]
fn polling_register_complete_schema_mismatch_is_terminal_without_retry() {
    let server = start_scripted_runner_server(vec![ScriptStep::RegisterResponse {
        status: "200 OK",
        body: r#"{"success":"yes","client":null,"error":null}"#,
    }]);
    let error = run_polling_runner_against_scripted_server(&server, false)
        .expect_err("a complete incompatible register response must stop");
    server.handle.join().unwrap();
    assert!(
        error.contains("register response incompatible with server protocol"),
        "{error}"
    );
    assert!(error.contains("serde_category=data"), "{error}");
    assert!(!error.contains("\"yes\""), "{error}");
    assert_eq!(
        recorded_paths(&server.requests),
        vec!["/api/shell/agent/register"]
    );
}

#[test]
fn polling_register_unknown_json_shape_is_terminal_without_retry() {
    let server = start_scripted_runner_server(vec![ScriptStep::RegisterResponse {
        status: "200 OK",
        body: r#"{"unexpected":true}"#,
    }]);
    let error = run_polling_runner_against_scripted_server(&server, false)
        .expect_err("an unknown complete register shape must stop");
    server.handle.join().unwrap();
    assert!(
        error.contains("register response incompatible with server protocol"),
        "{error}"
    );
    assert!(error.contains("serde_category=data"), "{error}");
    assert!(!error.contains("unexpected"), "{error}");
    assert_eq!(
        recorded_paths(&server.requests),
        vec!["/api/shell/agent/register"]
    );
}

#[test]
fn polling_oversized_response_is_terminal_without_loading_the_body() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollOversized {
            declared_len: crate::RUNNER_HTTP_RESPONSE_BODY_MAX_BYTES + 1,
        },
    ]);
    let error = run_polling_runner_against_scripted_server(&server, false)
        .expect_err("an oversized poll response must stop at the protocol boundary");
    server.handle.join().unwrap();
    assert!(
        error.contains("poll response incompatible with server protocol"),
        "{error}"
    );
    assert!(
        error.contains("declared response body exceeds limit_bytes=33554432"),
        "{error}"
    );
    assert_eq!(
        recorded_paths(&server.requests),
        vec!["/api/shell/agent/register", "/api/shell/agent/poll"]
    );
}

#[test]
fn polling_404_and_non_session_400_are_terminal_without_retry() {
    for (status, body, expected) in [
        (
            "404 Not Found",
            r#"{"success":false,"error":"missing"}"#,
            "poll endpoint missing or incompatible server",
        ),
        (
            "400 Bad Request",
            r#"{"success":false,"error":"invalid poll payload"}"#,
            "server permanently rejected polling",
        ),
    ] {
        let server = start_scripted_runner_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse { status, body },
        ]);
        let error = run_polling_runner_against_scripted_server(&server, false)
            .expect_err("permanent poll failure must stop");
        server.handle.join().unwrap();
        assert!(error.contains(expected), "{error}");
        assert_eq!(
            recorded_paths(&server.requests),
            vec!["/api/shell/agent/register", "/api/shell/agent/poll"],
            "status {status} must not retry or re-register"
        );
    }
}

#[test]
fn polling_unknown_session_reregisters_then_resumes() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "400 Bad Request",
            body: r#"{"success":false,"error":"unknown shell client: oe"}"#,
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("an explicitly missing polling session must re-register");
    server.handle.join().unwrap();
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_initial_register_502_recovers_without_supervisor_restart() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::RegisterResponse {
            status: "502 Bad Gateway",
            body: "<html>bad gateway</html>",
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    let started = Instant::now();
    run_polling_runner_against_scripted_server(&server, false)
        .expect("initial transient register failure must recover");
    server.handle.join().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(450));
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_recovery_register_502_retries_then_resumes() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "502 Bad Gateway",
            body: "bad gateway",
        },
        ScriptStep::RegisterResponse {
            status: "502 Bad Gateway",
            body: "bad gateway",
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("transient recovery register failure must recover");
    server.handle.join().unwrap();
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
            "/api/shell/agent/register",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_active_instance_lease_conflict_waits_then_registers() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::RegisterResponse {
            status: "400 Bad Request",
            body: r#"{"success":false,"error":"agent client oe is already online with a different instance"}"#,
        },
        ScriptStep::Register,
        ScriptStep::PollEmpty,
    ]);
    let started = Instant::now();
    run_polling_runner_against_scripted_server(&server, false)
        .expect("temporary active-instance lease must be retried");
    server.handle.join().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(450));
    assert_eq!(
        recorded_paths(&server.requests),
        vec![
            "/api/shell/agent/register",
            "/api/shell/agent/register",
            "/api/shell/agent/poll",
        ]
    );
}

#[test]
fn polling_register_auth_404_and_identity_mismatch_are_terminal() {
    for (status, body, expected) in [
        (
            "401 Unauthorized",
            r#"{"success":false,"error":"invalid token"}"#,
            "authentication failed",
        ),
        (
            "403 Forbidden",
            r#"{"success":false,"error":"forbidden"}"#,
            "authentication failed",
        ),
        (
            "404 Not Found",
            r#"{"success":false,"error":"missing"}"#,
            "endpoint missing or incompatible server",
        ),
        (
            "400 Bad Request",
            r#"{"success":false,"error":"agent token owner is 'alice'; cannot register owner 'bob'"}"#,
            "server rejected /api/shell/agent/register request",
        ),
        (
            "400 Bad Request",
            r#"{"success":false,"error":"agent client identity is unavailable"}"#,
            "server rejected /api/shell/agent/register request",
        ),
    ] {
        let server =
            start_scripted_runner_server(vec![ScriptStep::RegisterResponse { status, body }]);
        let error = run_polling_runner_against_scripted_server(&server, false)
            .expect_err("fatal register response must stop");
        server.handle.join().unwrap();
        assert!(error.contains(expected), "{error}");
        assert_eq!(
            recorded_paths(&server.requests),
            vec!["/api/shell/agent/register"],
            "status {status} must not retry"
        );
    }
}

#[test]
fn polling_once_retries_transient_registration_until_canonical_inventory_completes() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::RegisterResponse {
            status: "502 Bad Gateway",
            body: "bad gateway",
        },
        ScriptStep::Register,
    ]);
    run_polling_runner_against_scripted_server(&server, true)
        .expect("--once must complete after registration and canonical inventory synchronization");
    server.handle.join().unwrap();
    assert_eq!(
        recorded_paths(&server.requests),
        vec!["/api/shell/agent/register", "/api/shell/agent/register",]
    );
}

#[test]
fn polling_shutdown_interrupts_session_recovery_without_extra_request() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "502 Bad Gateway",
            body: "bad gateway",
        },
    ]);
    let started = Instant::now();
    run_polling_runner_against_scripted_server(&server, false)
        .expect("shutdown during recovery must be a clean exit");
    let elapsed = started.elapsed();
    server.handle.join().unwrap();
    assert!(
        elapsed < Duration::from_secs(1),
        "shutdown did not interrupt recovery promptly: {elapsed:?}"
    );
    assert_eq!(
        recorded_paths(&server.requests),
        vec!["/api/shell/agent/register", "/api/shell/agent/poll"],
        "shutdown must not leak a re-register request"
    );
}

#[test]
fn polling_shutdown_uses_the_process_coordinator_once() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollResponse {
            status: "502 Bad Gateway",
            body: "bad gateway",
        },
    ]);
    let temp = tempfile::tempdir().unwrap();
    let cfg = polling_runner_config(
        server.server_url.clone(),
        temp.path().join("project-registry"),
    );
    let runtime =
        RunnerRuntimeState::with_shutdown_budget(&cfg, PathBuf::new(), Duration::from_millis(500));
    run_polling_runner_with_shutdown(
        cfg,
        false,
        "inst-coordinator",
        Arc::clone(&server.shutdown),
        &runtime,
    )
    .unwrap();
    server.handle.join().unwrap();
    runtime.shutdown();
    assert_eq!(runtime.coordinator.run_count(), 1);
}

#[test]
fn polling_result_permanent_400_is_dropped_once_and_polling_continues() {
    #[cfg(unix)]
    let marker_temp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let marker = marker_temp.path().join("permanent-marker");
    #[cfg(unix)]
    let delivery = ScriptStep::PollDeliverRequest(polling_shell_request(
        "req-expired",
        marker_temp.path(),
        format!("printf '%s\\n' 'ran' >> {}", posix_quote(&marker)),
    ));
    #[cfg(not(unix))]
    let delivery = ScriptStep::PollDeliver("req-expired");
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        delivery,
        ScriptStep::Result {
            status: "400 Bad Request",
            body: r#"{"success":false,"error":"unknown or expired shell request: req-expired"}"#,
        },
        // The old path treated this as general retryable recovery, adding
        // a sleep and re-register before the next poll. The next call must
        // now be a poll, with neither recovery churn nor resubmission.
        ScriptStep::PollDeliver("req-next"),
        ScriptStep::Result {
            status: "200 OK",
            body: r#"{"success":true}"#,
        },
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("script completion should shut the polling runner down cleanly");
    server.handle.join().unwrap();

    let result_bodies = recorded_result_bodies(&server.requests);
    assert_eq!(
        result_bodies.len(),
        2,
        "the permanently rejected request result must be submitted once"
    );
    let mut result_ids = result_bodies
        .iter()
        .map(|body| {
            serde_json::from_str::<serde_json::Value>(body).unwrap()["request_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    result_ids.sort();
    assert_eq!(
        result_ids,
        vec!["req-expired".to_string(), "req-next".to_string()],
        "out-of-order dispatch must still submit each request result once"
    );
    assert_eq!(
        recorded_path_count(&server.requests, "/api/shell/agent/register"),
        1,
        "permanent result rejection must not re-register"
    );
    assert!(
        recorded_path_count(&server.requests, "/api/shell/agent/poll") >= 3,
        "both requests and a later empty turn must be polled"
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().lines().count(),
        1,
        "permanent result rejection must not replay child execution"
    );
}

#[test]
fn polling_result_transient_500_retries_same_payload_then_succeeds() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollDeliver("req-transient"),
        ScriptStep::Result {
            status: "500 Internal Server Error",
            body: r#"{"success":false,"error":"temporary backend failure"}"#,
        },
        ScriptStep::Result {
            status: "200 OK",
            body: r#"{"success":true}"#,
        },
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("script completion should shut the polling runner down cleanly");
    server.handle.join().unwrap();

    let result_bodies = recorded_result_bodies(&server.requests);
    assert_eq!(
        result_bodies.len(),
        2,
        "a transient failure must retry the same result payload"
    );
    assert!(
        result_bodies[0].contains("req-transient"),
        "{result_bodies:?}"
    );
    assert!(
        result_bodies[1].contains("req-transient"),
        "{result_bodies:?}"
    );
}

#[test]
fn polling_result_503_retries_same_payload_then_continues() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollDeliver("req-503"),
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
        ScriptStep::Result {
            status: "200 OK",
            body: r#"{"success":true}"#,
        },
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("script completion should shut the polling runner down cleanly");
    server.handle.join().unwrap();

    let result_bodies = recorded_result_bodies(&server.requests);
    assert_eq!(result_bodies.len(), 2);
    assert_eq!(
        result_bodies[0], result_bodies[1],
        "503 must retry the exact result body"
    );
    assert!(result_bodies[0].contains("req-503"), "{result_bodies:?}");
    assert_eq!(
        recorded_path_count(&server.requests, "/api/shell/agent/register"),
        1,
        "503 recovery must neither re-register nor stop polling"
    );
    assert!(
        recorded_path_count(&server.requests, "/api/shell/agent/poll") >= 2,
        "polling must continue while the worker retries result delivery"
    );
}

#[test]
fn polling_result_server_unavailable_retry_exhaustion_drops_then_continues() {
    #[cfg(unix)]
    let marker_temp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let marker = marker_temp.path().join("exhaustion-marker");
    #[cfg(unix)]
    let delivery = ScriptStep::PollDeliverRequest(polling_shell_request(
        "req-exhausted",
        marker_temp.path(),
        format!("printf '%s\\n' 'ran' >> {}", posix_quote(&marker)),
    ));
    #[cfg(not(unix))]
    let delivery = ScriptStep::PollDeliver("req-exhausted");
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        delivery,
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("script completion should shut the polling runner down cleanly");
    server.handle.join().unwrap();

    let result_bodies = recorded_result_bodies(&server.requests);
    assert_eq!(
        result_bodies.len(),
        RESULT_SUBMIT_RETRY_BACKOFF.len() + 1,
        "retry exhaustion must stop after the fixed total attempt count"
    );
    assert!(
        result_bodies.iter().all(|body| body == &result_bodies[0]),
        "every bounded retry must use the exact original payload"
    );
    assert_eq!(
        recorded_path_count(&server.requests, "/api/shell/agent/register"),
        1,
        "exhaustion must release the result and poll without re-registering"
    );
    assert!(
        recorded_path_count(&server.requests, "/api/shell/agent/poll") >= 2,
        "polling must stay live while the bounded result retry runs"
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().lines().count(),
        1,
        "transient result retry exhaustion must not replay child execution"
    );
}

#[test]
fn submit_result_503_retry_exhaustion_returns_dropped_outcome() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
        ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        },
    ]);
    let sink = RunnerSink::Http(HttpSendConfig {
        client: Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        server_url: server.server_url.clone(),
        token: "test-token".to_string(),
        client_id: "oe".to_string(),
        runner_instance_id: "inst-exhausted".to_string(),
        shutdown: Arc::new(AtomicBool::new(false)),
    });
    let outcome = sink
        .submit_result(
            "req-exhausted-outcome".to_string(),
            CommandResult {
                exit_code: Some(0),
                stdout: Some("done".to_string()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            },
        )
        .unwrap();
    server.handle.join().unwrap();

    assert_eq!(outcome, ResultSubmission::DroppedAfterRetryExhaustion);
    assert_eq!(
        recorded_result_bodies(&server.requests).len(),
        RESULT_SUBMIT_RETRY_BACKOFF.len() + 1
    );
}

#[test]
fn submit_result_retry_backoff_is_shutdown_aware() {
    let server = start_scripted_runner_server(vec![ScriptStep::Result {
        status: "503 Service Unavailable",
        body: r#"{"success":false,"error":"temporary gateway failure"}"#,
    }]);
    let sink = RunnerSink::Http(HttpSendConfig {
        client: Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        server_url: server.server_url.clone(),
        token: "test-token".to_string(),
        client_id: "oe".to_string(),
        runner_instance_id: "inst-shutdown".to_string(),
        shutdown: Arc::new(AtomicBool::new(true)),
    });
    let started = Instant::now();
    let error = sink
        .submit_result(
            "req-shutdown".to_string(),
            CommandResult {
                exit_code: Some(0),
                stdout: Some("done".to_string()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            },
        )
        .expect_err("shutdown must interrupt the retry backoff");
    server.handle.join().unwrap();

    assert!(matches!(error, SubmitResultError::Shutdown(_)), "{error:?}");
    assert_eq!(recorded_result_bodies(&server.requests).len(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown-aware result backoff did not return promptly"
    );
}

#[test]
fn result_submission_gateway_and_connection_classes_are_transient() {
    for status in [
        reqwest::StatusCode::BAD_GATEWAY,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        reqwest::StatusCode::GATEWAY_TIMEOUT,
    ] {
        let error = RunnerHttpError::status(RUNNER_RESULT_PATH, status, "{}");
        assert_eq!(error.kind, RunnerHttpErrorKind::ServerUnavailable);
        assert_eq!(
            result_http_error_disposition(&error.kind),
            ResultHttpErrorDisposition::RetryTransient,
            "status {status} must enter bounded result retry"
        );
    }
    assert_eq!(
        result_http_error_disposition(&RunnerHttpErrorKind::ServerUnavailable),
        ResultHttpErrorDisposition::RetryTransient,
        "connection-refused/reset/closed classification must enter bounded retry"
    );
    for kind in [
        RunnerHttpErrorKind::Status,
        RunnerHttpErrorKind::RequestTimeout,
        RunnerHttpErrorKind::Request,
        RunnerHttpErrorKind::DecodeTransient,
    ] {
        assert_eq!(
            result_http_error_disposition(&kind),
            ResultHttpErrorDisposition::RetryTransient,
            "{kind:?} must enter bounded result retry"
        );
    }
    assert_eq!(
        result_http_error_disposition(&RunnerHttpErrorKind::ClientRejected),
        ResultHttpErrorDisposition::RejectPermanent
    );
    assert_eq!(
        result_http_error_disposition(&RunnerHttpErrorKind::Auth),
        ResultHttpErrorDisposition::FatalAuth
    );
    assert_eq!(
        result_http_error_disposition(&RunnerHttpErrorKind::NotFound),
        ResultHttpErrorDisposition::FatalProtocol
    );
    assert_eq!(
        result_http_error_disposition(&RunnerHttpErrorKind::ProtocolDecode),
        ResultHttpErrorDisposition::FatalProtocol
    );
    assert_eq!(
        result_http_error_disposition(&RunnerHttpErrorKind::Config),
        ResultHttpErrorDisposition::FatalConfig
    );
}

#[test]
fn polling_result_401_and_403_are_terminal_auth_errors_without_credentials() {
    for status in ["401 Unauthorized", "403 Forbidden"] {
        let server = start_scripted_runner_server(vec![
            ScriptStep::Register,
            ScriptStep::PollDeliver("req-auth"),
            ScriptStep::Result {
                status,
                body: r#"{"success":false,"error":"unauthorized token=SECRET-BODY-TOKEN"}"#,
            },
        ]);
        let error = run_polling_runner_against_scripted_server(&server, false)
            .expect_err("auth rejection on result submission must stop the agent");
        server.handle.join().unwrap();

        assert_eq!(
            recorded_result_bodies(&server.requests).len(),
            1,
            "an auth failure must not retry the result"
        );
        assert!(
            error.contains("authentication failed for /api/shell/agent/result"),
            "{error}"
        );
        assert!(error.contains("check agent token/config"), "{error}");
        assert!(!error.contains("test-token"), "{error}");
        assert!(!error.contains("SECRET-BODY-TOKEN"), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn polling_fatal_background_submission_reaches_control_without_reexecution() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("fatal-marker");
    let request = polling_shell_request(
        "req-fatal-background",
        temp.path(),
        format!("printf '%s\\n' 'ran' >> {}", posix_quote(&marker)),
    );
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollDeliverRequest(request),
        ScriptStep::Result {
            status: "401 Unauthorized",
            body: r#"{"success":false,"error":"unauthorized"}"#,
        },
    ]);
    let error = run_polling_runner_against_scripted_server(&server, false)
        .expect_err("fatal background result submission must stop polling control");
    server.handle.join().unwrap();

    assert!(
        error.contains("authentication failed for /api/shell/agent/result"),
        "{error}"
    );
    assert_eq!(recorded_result_bodies(&server.requests).len(), 1);
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().lines().count(),
        1,
        "fatal result submission must not replay the command"
    );
}

#[test]
fn polling_result_404_is_terminal_protocol_error_without_retry() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollDeliver("req-missing-endpoint"),
        ScriptStep::Result {
            status: "404 Not Found",
            body: r#"{"success":false,"error":"token=SECRET-BODY-TOKEN"}"#,
        },
    ]);
    let error = run_polling_runner_against_scripted_server(&server, false)
        .expect_err("missing result endpoint must stop the polling agent");
    server.handle.join().unwrap();

    assert_eq!(
        recorded_result_bodies(&server.requests).len(),
        1,
        "404 must not retry the result"
    );
    assert!(
        error.contains("endpoint missing or incompatible server for /api/shell/agent/result"),
        "{error}"
    );
    assert!(!error.contains("test-token"), "{error}");
    assert!(!error.contains("SECRET-BODY-TOKEN"), "{error}");
}

#[test]
fn polling_result_success_submits_once_and_continues() {
    let server = start_scripted_runner_server(vec![
        ScriptStep::Register,
        ScriptStep::PollDeliver("req-success"),
        ScriptStep::Result {
            status: "200 OK",
            body: r#"{"success":true}"#,
        },
        ScriptStep::PollEmpty,
    ]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("script completion should shut the polling runner down cleanly");
    server.handle.join().unwrap();

    let result_bodies = recorded_result_bodies(&server.requests);
    assert_eq!(result_bodies.len(), 1);
    assert!(
        result_bodies[0].contains("req-success"),
        "{result_bodies:?}"
    );
    assert_eq!(
        recorded_path_count(&server.requests, "/api/shell/agent/register"),
        1
    );
    assert!(
        recorded_path_count(&server.requests, "/api/shell/agent/poll") >= 2,
        "successful result delivery must not pin the next poll"
    );
}

#[test]
fn permanent_rejection_log_line_is_bounded_and_redacted() {
    let token = "DO_NOT_LEAK_THIS_TOKEN";
    let noisy_error = format!(
            "server rejected /api/shell/agent/result request: HTTP 400 Bad Request: token={} url=https://host/path?token={}\n{}",
            token,
            token,
            "<html><body>huge proxy page</body></html>".repeat(200)
        );
    let line = permanent_result_rejection_log_line("req-noisy", &noisy_error, token);
    assert!(line.contains("request_id=req-noisy"), "{line}");
    assert!(!line.contains(token), "{line}");
    assert!(!line.contains("?token="), "{line}");
    assert!(!line.contains('\n'), "{line}");
    assert!(
        line.chars().count() < 320,
        "log line not bounded: {} chars",
        line.chars().count()
    );
}

#[test]
fn dropped_result_log_line_is_bounded_and_redacted() {
    let token = "DO_NOT_LEAK_THIS_TOKEN";
    let line = dropped_result_log_line(
        &format!("req-\n{}{}", token, "x".repeat(500)),
        RESULT_SUBMIT_RETRY_BACKOFF.len() + 1,
        &format!(
            "server unavailable token={} {}",
            token,
            "<html>proxy response</html>".repeat(200)
        ),
        token,
    );
    assert!(line.contains("attempts=4"), "{line}");
    assert!(!line.contains(token), "{line}");
    assert!(!line.contains('\n'), "{line}");
    assert!(
        line.chars().count() < 520,
        "log line not bounded: {} chars",
        line.chars().count()
    );
}

async fn read_register(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> crate::runner_protocol::RunnerRegisterRequest {
    let msg = ws
        .next()
        .await
        .expect("agent sent register")
        .expect("register message is ok");
    match RunnerEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap() {
        RunnerEnvelope::Register { payload, .. } => payload,
        other => panic!("expected register envelope, got {}", other.kind()),
    }
}

async fn send_registered_ack(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) {
    let client = serde_json::from_value(serde_json::json!({
        "client_id": "oe",
        "agent_instance_id": "inst-test",
        "status": "online",
        "connected": true,
        "last_seen": 1,
        "capabilities": {},
        "pending_requests": 0,
        "projects": [],
        "agent_protocol_generation": RUNNER_PROTOCOL_GENERATION_V2.get(),
        "project_inventory": {
            "sync_state": "pending",
            "generation": null,
            "total_reported": null,
            "total_synced": 0,
            "last_error_code": null,
            "last_sync_at": null,
            "max_summaries_per_page": PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
            "max_serialized_bytes_per_page": PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES
        }
    }))
    .unwrap();
    let ack = RunnerEnvelope::Registered {
        success: true,
        client: Some(client),
        error: None,
    };
    ws.send(WsMessage::Text(ack.to_json().unwrap().into()))
        .await
        .unwrap();

    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("agent did not publish canonical project inventory after registration")
            .expect("websocket closed before project inventory publication")
            .expect("project inventory message is valid");
        if !msg.is_text() {
            continue;
        }
        let page = match RunnerEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap() {
            RunnerEnvelope::ProjectInventoryPage { page } => page,
            other => panic!(
                "expected project inventory page after registered ack, got {}",
                other.kind()
            ),
        };
        let status = inventory_status(
            if page.complete {
                "complete"
            } else {
                "in_progress"
            },
            &page.generation,
            page.total_reported,
            if page.complete {
                page.total_reported
            } else {
                (((page.page_index as usize) + 1) * PROJECT_INVENTORY_PAGE_MAX_SUMMARIES)
                    .min(page.total_reported)
            },
        );
        let complete = page.complete;
        ws.send(WsMessage::Text(
            RunnerEnvelope::ProjectInventoryStatus { status }
                .to_json()
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
        if complete {
            break;
        }
    }
}

async fn send_register_rejected_ack(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) {
    let ack = RunnerEnvelope::Registered {
        success: false,
        client: None,
        error: Some("unauthorized".to_string()),
    };
    ws.send(WsMessage::Text(ack.to_json().unwrap().into()))
        .await
        .unwrap();
}

fn start_job_request(cwd: &Path, command: &str) -> RunnerRequest {
    RunnerRequest {
        request_id: "req-active-job".to_string(),
        client_id: "oe".to_string(),
        kind: "start_job".to_string(),
        job_id: Some("job-active".to_string()),
        cwd: Some(cwd.to_string_lossy().to_string()),
        path: None,
        content: None,
        max_bytes: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: None,
        end_line: None,
        create_dirs: false,
        command: command.to_string(),
        process: None,
        script: None,
        stdin: None,
        timeout_secs: 5,
        requested_by: "tester".to_string(),
        created_at: 0,
        validation: None,
        lsp: None,
        job_context: Some(crate::test_job_context(cwd, Vec::new())),
        mcp_gateway: None,
        plugin_gateway: None,
        coding_agent: None,
        persistent_shell: None,
    }
}

#[test]
fn reconnect_backoff_is_bounded_exponential() {
    let mut backoff = RetryBackoff::new(&RECONNECT_BACKOFF_STEPS);
    assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    assert_eq!(backoff.next_delay(), Duration::from_secs(2));
    assert_eq!(backoff.next_delay(), Duration::from_secs(5));
    assert_eq!(backoff.next_delay(), Duration::from_secs(10));
    assert_eq!(backoff.next_delay(), Duration::from_secs(30));
    assert_eq!(backoff.next_delay(), Duration::from_secs(30));
    backoff.reset();
    assert_eq!(backoff.next_delay(), Duration::from_secs(1));
}

#[test]
fn polling_idle_backoff_progression_cap_and_request_reset() {
    let mut backoff = PollingIdleBackoff::new(Duration::from_secs(1));
    assert_eq!(
        polling_idle_delay(&mut backoff, false),
        Some(Duration::from_secs(1))
    );
    assert_eq!(
        polling_idle_delay(&mut backoff, false),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        polling_idle_delay(&mut backoff, false),
        Some(Duration::from_secs(5))
    );
    assert_eq!(
        polling_idle_delay(&mut backoff, false),
        Some(Duration::from_secs(5))
    );

    assert_eq!(polling_idle_delay(&mut backoff, true), None);
    assert_eq!(
        polling_idle_delay(&mut backoff, false),
        Some(Duration::from_secs(1))
    );

    let mut custom = PollingIdleBackoff::new(Duration::from_secs(3));
    assert_eq!(custom.next_delay(), Duration::from_secs(3));
    assert_eq!(custom.next_delay(), Duration::from_secs(5));
    assert_eq!(custom.next_delay(), Duration::from_secs(5));

    let mut above_default_cap = PollingIdleBackoff::new(Duration::from_secs(60));
    assert_eq!(above_default_cap.next_delay(), Duration::from_secs(60));
    assert_eq!(above_default_cap.next_delay(), Duration::from_secs(60));
}

#[test]
fn polling_recovery_backoff_is_bounded_and_resets() {
    let mut backoff = RetryBackoff::new(&POLLING_RECOVERY_BACKOFF_STEPS);
    assert_eq!(backoff.next_delay(), Duration::from_millis(500));
    assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    assert_eq!(backoff.next_delay(), Duration::from_secs(2));
    assert_eq!(backoff.next_delay(), Duration::from_secs(5));
    assert_eq!(backoff.next_delay(), Duration::from_secs(10));
    assert_eq!(backoff.next_delay(), Duration::from_secs(10));
    backoff.reset();
    assert_eq!(backoff.next_delay(), Duration::from_millis(500));
}

#[test]
fn polling_lease_conflict_retry_has_a_finite_total_wait() {
    let mut backoff = RetryBackoff::new(&POLLING_RECOVERY_BACKOFF_STEPS);
    assert_eq!(
        next_lease_conflict_delay(
            &mut backoff,
            POLLING_LEASE_CONFLICT_MAX_WAIT - Duration::from_millis(250)
        ),
        Some(Duration::from_millis(250))
    );
    assert_eq!(
        next_lease_conflict_delay(&mut backoff, POLLING_LEASE_CONFLICT_MAX_WAIT),
        None
    );
}

#[test]
fn transport_error_classification_separates_transient_and_fatal() {
    let transient = classify_session_error("websocket connect failed: connection refused");
    assert!(!transient.is_fatal(), "{transient}");

    let proxy_network =
        RunnerTransportError::transient("websocket connect failed: proxy TCP connect failed");
    assert!(matches!(proxy_network, RunnerTransportError::Transient(_)));

    let fatal = classify_session_error("register rejected by server: unauthorized");
    assert!(fatal.is_fatal(), "{fatal}");

    let fatal =
        classify_session_error("quic connect failed: certificate verify failed; check server_name");
    assert!(fatal.is_fatal(), "{fatal}");
}

#[test]
fn stream_supervisor_once_semantics_are_explicit_and_shared() {
    for (mode, transport) in [
        (
            StreamSupervisorMode::Strict(StreamTransport::WebSocket),
            StreamTransport::WebSocket,
        ),
        (
            StreamSupervisorMode::Strict(StreamTransport::Quic),
            StreamTransport::Quic,
        ),
        (StreamSupervisorMode::Auto, StreamTransport::WebSocket),
        (StreamSupervisorMode::Auto, StreamTransport::Quic),
    ] {
        assert_eq!(
            decide_stream_session(
                mode,
                transport,
                true,
                Ok(RunnerSessionExit::TransportDisconnected)
            ),
            StreamSessionDecision::Complete { shutdown: false },
            "{mode:?} {transport:?} must stop after a completed once session"
        );
    }

    for transport in [StreamTransport::WebSocket, StreamTransport::Quic] {
        assert!(matches!(
            decide_stream_session(
                StreamSupervisorMode::Strict(transport),
                transport,
                true,
                Err(classify_session_error("connection refused")),
            ),
            StreamSessionDecision::Fatal(error) if error == "connection refused"
        ));
    }

    assert!(matches!(
        decide_stream_session(
            StreamSupervisorMode::Auto,
            StreamTransport::Quic,
            true,
            Err(classify_session_error("connection refused")),
        ),
        StreamSessionDecision::TryNext(RunnerTransportError::Transient(error))
            if error == "connection refused"
    ));
    assert!(matches!(
        decide_stream_session(
            StreamSupervisorMode::Auto,
            StreamTransport::WebSocket,
            true,
            Err(classify_session_error("connection refused")),
        ),
        StreamSessionDecision::Fatal(error) if error == "connection refused"
    ));
}

#[test]
fn stream_supervisor_reconnect_and_auto_fallback_semantics_are_shared() {
    for transport in [StreamTransport::WebSocket, StreamTransport::Quic] {
        assert!(matches!(
            decide_stream_session(
                StreamSupervisorMode::Strict(transport),
                transport,
                false,
                Ok(RunnerSessionExit::TransportDisconnected),
            ),
            StreamSessionDecision::Reconnect(None)
        ));
        assert!(matches!(
            decide_stream_session(
                StreamSupervisorMode::Strict(transport),
                transport,
                false,
                Err(classify_session_error("connection refused")),
            ),
            StreamSessionDecision::Reconnect(Some(RunnerTransportError::Transient(error)))
                if error == "connection refused"
        ));
        assert!(matches!(
            decide_stream_session(
                StreamSupervisorMode::Auto,
                transport,
                false,
                Ok(RunnerSessionExit::TransportDisconnected),
            ),
            StreamSessionDecision::Reconnect(None)
        ));
        assert!(matches!(
            decide_stream_session(
                StreamSupervisorMode::Auto,
                transport,
                false,
                Err(classify_session_error("connection refused")),
            ),
            StreamSessionDecision::TryNext(RunnerTransportError::Transient(error))
                if error == "connection refused"
        ));
        assert!(matches!(
            decide_stream_session(
                StreamSupervisorMode::Auto,
                transport,
                false,
                Err(classify_session_error(
                    "register rejected by server: unauthorized",
                )),
            ),
            StreamSessionDecision::Fatal(error) if error.contains("register rejected")
        ));
    }
}

#[test]
fn websocket_proxy_configuration_errors_are_mode_sensitive() {
    let unsupported = parse_http_proxy_endpoint("socks5://proxy.test:1080")
        .expect_err("unsupported proxy scheme must fail configuration");
    assert!(matches!(
        decide_stream_session(
            StreamSupervisorMode::Strict(StreamTransport::WebSocket),
            StreamTransport::WebSocket,
            false,
            Err(unsupported),
        ),
        StreamSessionDecision::Fatal(error) if error.contains("proxy scheme is unsupported")
    ));

    let auth = parse_http_proxy_endpoint("http://proxy-user:proxy-pass@proxy.test:8080")
        .expect_err("proxy auth URL must fail configuration");
    assert!(matches!(
        decide_stream_session(
            StreamSupervisorMode::Strict(StreamTransport::WebSocket),
            StreamTransport::WebSocket,
            false,
            Err(auth),
        ),
        StreamSessionDecision::Fatal(error) if error.contains("proxy authentication is unsupported")
    ));

    let unsupported = parse_http_proxy_endpoint("socks5://proxy.test:1080")
        .expect_err("unsupported proxy scheme must fail configuration");
    assert!(matches!(
        decide_stream_session(
            StreamSupervisorMode::Auto,
            StreamTransport::WebSocket,
            false,
            Err(unsupported),
        ),
        StreamSessionDecision::TryNext(RunnerTransportError::ProxyConfiguration(error))
            if error.contains("proxy scheme is unsupported")
    ));

    let proxy_auth_required = RunnerTransportError::proxy_configuration(
        "websocket connect failed: proxy CONNECT returned HTTP 407",
    );
    assert!(matches!(
        decide_stream_session(
            StreamSupervisorMode::Strict(StreamTransport::WebSocket),
            StreamTransport::WebSocket,
            false,
            Err(proxy_auth_required),
        ),
        StreamSessionDecision::Fatal(error) if error.contains("HTTP 407")
    ));

    let proxy_auth_required = RunnerTransportError::proxy_configuration(
        "websocket connect failed: proxy CONNECT returned HTTP 407",
    );
    assert!(matches!(
        decide_stream_session(
            StreamSupervisorMode::Auto,
            StreamTransport::WebSocket,
            false,
            Err(proxy_auth_required),
        ),
        StreamSessionDecision::TryNext(RunnerTransportError::ProxyConfiguration(error))
            if error.contains("HTTP 407")
    ));

    let network =
        RunnerTransportError::transient("websocket connect failed: proxy TCP connect failed");
    assert!(matches!(
        decide_stream_session(
            StreamSupervisorMode::Strict(StreamTransport::WebSocket),
            StreamTransport::WebSocket,
            false,
            Err(network),
        ),
        StreamSessionDecision::Reconnect(Some(RunnerTransportError::Transient(error)))
            if error.contains("proxy TCP connect failed")
    ));
}

#[test]
fn auto_log_lines_are_concise_and_redacted() {
    assert_eq!(
        auto_quic_not_configured_log_line(),
        "webcodex-runner transport auto: quic not configured; skipping"
    );
    assert_eq!(
        auto_trying_log_line(TRANSPORT_WEBSOCKET),
        "webcodex-runner transport auto: websocket trying"
    );
    assert_eq!(
        auto_trying_log_line(TRANSPORT_POLLING),
        "webcodex-runner transport auto: polling trying"
    );

    let token = "DO_NOT_LEAK_THIS_TOKEN";
    let concise = concise_log_error(
        "websocket connect failed: token=DO_NOT_LEAK_THIS_TOKEN\nwhile connecting",
        token,
    );
    assert!(!concise.contains(token), "{concise}");
    assert!(!concise.contains('\n'), "{concise}");
}

#[test]
fn registered_log_includes_actual_transport_without_url_query_or_token() {
    let token = "DO_NOT_LEAK_THIS_TOKEN";
    let mut cfg = test_runner_config(format!(
        "https://webcodex.example.test/agent/path?token={}",
        token
    ));
    cfg.token = token.to_string();
    cfg.transport = Some(TRANSPORT_AUTO.to_string());

    let line = registered_log_line(&cfg, TRANSPORT_POLLING, 11);
    assert!(line.contains("client_id=oe"), "{line}");
    assert!(
        line.contains("server=https://webcodex.example.test"),
        "{line}"
    );
    assert!(line.contains("preferred_transport=auto"), "{line}");
    assert!(line.contains("actual_transport=polling"), "{line}");
    assert!(line.contains("projects=11"), "{line}");
    assert!(!line.contains(token), "{line}");
    assert!(!line.contains("/agent/path"), "{line}");
    assert!(!line.contains("?token="), "{line}");
}

#[test]
fn auto_websocket_failure_falls_back_to_polling() {
    let (server_url, poll_count, server) =
        start_auto_fallback_http_server("502 Bad Gateway", "text/html", "<html>bad gateway</html>");
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_runner_config(server_url);
    cfg.transport = Some(TRANSPORT_AUTO.to_string());
    cfg.project_registry_dir = Some(tmp.path().join("project-registry"));
    cfg.websocket_connect_timeout_secs = 1;

    let runtime = test_runtime(&cfg);
    let err = run_auto_runner(cfg, false, "inst-auto-fallback", &runtime)
        .expect_err("terminal polling 404 should stop after recovering the first 502");
    server.join().unwrap();
    assert_eq!(poll_count.load(Ordering::SeqCst), 2);
    assert!(
        err.contains("poll endpoint missing or incompatible server"),
        "{err}"
    );
}

#[test]
fn polling_502_html_is_transient_and_sanitized() {
    let nginx_html = "<html>\n<head><title>502 Bad Gateway</title></head>\n<body>\n<center><h1>502 Bad Gateway</h1></center>\n<hr><center>nginx/1.31.1</center>\n</body>\n</html>";
    let error = RunnerHttpError::status(
        "/api/shell/agent/poll",
        reqwest::StatusCode::BAD_GATEWAY,
        nginx_html,
    );
    let poll_error = crate::PollError::from_http(error, "oe");
    assert_eq!(
        poll_error.recovery_action(),
        PollingRecoveryAction::RetryPoll
    );
    let message = poll_error.to_string();
    assert!(
        message.contains(
            "server unavailable while polling /api/shell/agent/poll: HTTP 502 Bad Gateway"
        ),
        "{message}"
    );
    assert!(!message.contains("<html"), "{message}");
    assert!(!message.contains("nginx/1.31.1"), "{message}");
    assert!(!message.contains("<center><h1>502 Bad Gateway</h1></center>"));
}

#[test]
fn polling_503_and_504_are_transient_server_unavailable() {
    for (status, expected) in [
        (
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "server unavailable while polling /api/shell/agent/poll: HTTP 503 Service Unavailable",
        ),
        (
            reqwest::StatusCode::GATEWAY_TIMEOUT,
            "server unavailable while polling /api/shell/agent/poll: HTTP 504 Gateway Timeout",
        ),
    ] {
        let error = RunnerHttpError::status("/api/shell/agent/poll", status, "proxy unavailable");
        let poll_error = crate::PollError::from_http(error, "oe");
        assert_eq!(
            poll_error.recovery_action(),
            PollingRecoveryAction::RetryPoll
        );
        let message = poll_error.to_string();
        assert!(message.contains(expected), "{message}");
        assert!(!message.contains("proxy unavailable"), "{message}");
    }
}

#[test]
fn polling_401_and_403_are_terminal_auth_errors() {
    for (status, expected) in [
            (
                "401 Unauthorized",
                "authentication failed while polling /api/shell/agent/poll: HTTP 401 Unauthorized; check agent token/config",
            ),
            (
                "403 Forbidden",
                "authentication failed while polling /api/shell/agent/poll: HTTP 403 Forbidden; check agent token/config",
            ),
        ] {
            let (result, poll_count) = run_polling_runner_against_server(
                status,
                "application/json",
                r#"{"error":"unauthorized"}"#,
                false,
            );
            let error = result.expect_err("auth poll response must stop the foreground agent");

            assert_eq!(poll_count, 1);
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("unauthorized\""), "{error}");
        }
}

#[test]
fn polling_once_completes_canonical_inventory_without_extra_business_poll() {
    let (result, poll_count) = run_polling_runner_against_server(
        "200 OK",
        "application/json",
        r#"{"success":true,"request":null,"error":null}"#,
        true,
    );

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(poll_count, 1);
}

#[test]
fn polling_register_and_ordinary_poll_both_omit_inline_projects() {
    let server = start_scripted_runner_server(vec![ScriptStep::Register, ScriptStep::PollEmpty]);
    run_polling_runner_against_scripted_server(&server, false)
        .expect("empty polling turn should stop cleanly with scripted shutdown");
    server.handle.join().unwrap();

    let requests = server.requests.lock().unwrap();
    let register: serde_json::Value = serde_json::from_str(&requests[0].1).unwrap();
    let poll: serde_json::Value = serde_json::from_str(&requests[1].1).unwrap();
    assert!(
        register["projects"].is_null(),
        "generation-2 registration must not carry inline projects"
    );
    assert!(
        poll["projects"].is_null(),
        "ordinary poll must not revive inline project refresh"
    );
}

#[test]
fn polling_graceful_shutdown_sends_instance_scoped_offline_notice() {
    let poll_seen = Arc::new(AtomicBool::new(false));
    let offline_seen = Arc::new(AtomicBool::new(false));
    let seen_poll = Arc::clone(&poll_seen);
    let seen_offline = Arc::clone(&offline_seen);
    let server = start_concurrent_polling_server(Arc::new(move |path, body| match path {
        "/api/shell/agent/register" => register_inventory_support_response(),
        "/api/shell/agent/poll" => {
            seen_poll.store(true, Ordering::SeqCst);
            ConcurrentHttpResponse::json(r#"{"success":true,"request":null,"error":null}"#)
        }
        "/api/shell/agent/offline" => {
            let value: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(value["client_id"], "oe");
            assert_eq!(value["agent_instance_id"], "inst-offline");
            seen_offline.store(true, Ordering::SeqCst);
            ConcurrentHttpResponse::json(r#"{"success":true,"error":null}"#)
        }
        other => panic!("unexpected polling shutdown request: {other}"),
    }));
    let tmp = tempfile::tempdir().unwrap();
    let cfg = polling_runner_config(
        server.server_url.clone(),
        tmp.path().join("project-registry"),
    );
    let runtime = test_runtime(&cfg);
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = spawn_polling_runner(cfg, runtime, false, "inst-offline", Arc::clone(&shutdown));

    let deadline = Instant::now() + Duration::from_secs(5);
    while !poll_seen.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        poll_seen.load(Ordering::SeqCst),
        "polling Runner never became active"
    );
    shutdown.store(true, Ordering::SeqCst);
    handle
        .finish(Duration::from_secs(5), "polling graceful offline")
        .unwrap();
    assert!(offline_seen.load(Ordering::SeqCst));
    server.finish();
}

#[test]
fn project_inventory_pager_honors_count_byte_and_ack_boundaries() {
    for count in [64usize, 65, 100, 256, 1024] {
        let projects = (0..count)
            .map(|index| synthetic_project_summary(index, None))
            .collect::<Vec<_>>();
        let mut sync = ProjectInventorySync::new(projects);
        let generation = sync.generation().to_string();
        let mut total = 0usize;
        let mut pages = 0usize;
        loop {
            let page = sync.current_page().unwrap().expect("next inventory page");
            assert!(page.projects.len() <= PROJECT_INVENTORY_PAGE_MAX_SUMMARIES);
            assert!(
                serde_json::to_vec(&page).unwrap().len()
                    <= PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES
            );
            total += page.projects.len();
            pages += 1;
            let state = if page.complete {
                "complete"
            } else {
                "in_progress"
            };
            let done = sync
                .acknowledge(&inventory_status(state, &generation, count, total))
                .unwrap();
            if done {
                break;
            }
        }
        assert_eq!(total, count);
        assert_eq!(
            pages,
            count.div_ceil(PROJECT_INVENTORY_PAGE_MAX_SUMMARIES),
            "count {count} should use deterministic bounded pages"
        );
    }

    let projects = (0..PROJECT_INVENTORY_PAGE_MAX_SUMMARIES)
        .map(|index| synthetic_project_summary(index, Some(4096)))
        .collect::<Vec<_>>();
    let mut sync = ProjectInventorySync::new(projects);
    let generation = sync.generation().to_string();
    let mut total = 0usize;
    let mut pages = 0usize;
    loop {
        let page = sync.current_page().unwrap().unwrap();
        let serialized = serde_json::to_vec(&page).unwrap();
        assert!(serialized.len() <= PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES);
        assert!(page.projects.len() < PROJECT_INVENTORY_PAGE_MAX_SUMMARIES);
        total += page.projects.len();
        pages += 1;
        let state = if page.complete {
            "complete"
        } else {
            "in_progress"
        };
        if sync
            .acknowledge(&inventory_status(
                state,
                &generation,
                PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
                total,
            ))
            .unwrap()
        {
            break;
        }
    }
    assert!(
        pages > 1,
        "serialized-byte bound should split the 64-summary batch"
    );
    assert_eq!(total, PROJECT_INVENTORY_PAGE_MAX_SUMMARIES);

    let mut sync = ProjectInventorySync::new(
        (0..65)
            .map(|index| synthetic_project_summary(index, None))
            .collect(),
    );
    let page = sync.current_page().unwrap().unwrap();
    let duplicate = sync.current_page().unwrap().unwrap();
    assert_eq!(
        serde_json::to_vec(&page).unwrap(),
        serde_json::to_vec(&duplicate).unwrap()
    );
    let mismatch = sync
        .acknowledge(&inventory_status("in_progress", "stale-generation", 65, 64))
        .unwrap_err();
    assert_eq!(mismatch, "project_inventory_ack_generation_mismatch");
}

#[tokio::test]
async fn streaming_project_inventory_retry_preserves_pending_page_after_backpressure() {
    let projects = (0..65)
        .map(|index| synthetic_project_summary(index, None))
        .collect::<Vec<_>>();
    let mut sync = Some(ProjectInventorySync::new(projects));
    let expected = sync
        .as_mut()
        .unwrap()
        .current_page()
        .unwrap()
        .expect("first pending page");
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(RunnerEnvelope::Ping { ts: 1 }).unwrap();

    try_queue_project_inventory_page(StreamTransport::WebSocket, &mut sync, &tx);
    assert!(matches!(
        rx.recv().await,
        Some(RunnerEnvelope::Ping { ts: 1 })
    ));

    try_queue_project_inventory_page(StreamTransport::WebSocket, &mut sync, &tx);
    let retried = match rx.recv().await {
        Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
        other => panic!("expected retried project inventory page, got {other:?}"),
    };
    assert_eq!(
        serde_json::to_vec(&retried).unwrap(),
        serde_json::to_vec(&expected).unwrap(),
        "backpressure retry must resend the exact pending page without advancing"
    );
}

#[tokio::test]
async fn streaming_project_inventory_staging_capacity_retries_exact_page_for_websocket_and_quic() {
    for transport in [StreamTransport::WebSocket, StreamTransport::Quic] {
        let projects = (0..100)
            .map(|index| synthetic_project_summary(index, None))
            .collect::<Vec<_>>();
        let mut sync = Some(ProjectInventorySync::new(projects));
        let generation = sync.as_ref().unwrap().generation().to_string();
        let expected_page0 = sync
            .as_mut()
            .unwrap()
            .current_page()
            .unwrap()
            .expect("first inventory page");
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut retry_backoff = RetryBackoff::new(&PROJECT_INVENTORY_STAGING_RETRY_BACKOFF_STEPS);

        try_queue_project_inventory_page(transport, &mut sync, &tx);
        let sent_page0 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected initial project inventory page, got {other:?}"),
        };
        assert_eq!(
            serde_json::to_vec(&sent_page0).unwrap(),
            serde_json::to_vec(&expected_page0).unwrap()
        );

        let capacity = ShellProjectInventoryStatus {
            sync_state: "degraded".to_string(),
            generation: Some("previous-authoritative-generation".to_string()),
            total_reported: Some(7),
            total_synced: 7,
            last_error_code: Some("project_inventory_staging_capacity".to_string()),
            last_sync_at: Some(1),
            max_summaries_per_page: PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
            max_serialized_bytes_per_page: PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES,
        };
        let delay = handle_project_inventory_status(
            transport,
            capacity.clone(),
            &mut sync,
            &tx,
            &mut retry_backoff,
        );
        assert_eq!(
            delay,
            ProjectInventoryStatusAction::RetryExactAfter(Duration::from_secs(1))
        );
        assert!(
            rx.try_recv().is_err(),
            "capacity failure must not busy-loop"
        );
        let still_pending = sync
            .as_mut()
            .unwrap()
            .current_page()
            .unwrap()
            .expect("capacity failure keeps page 0 pending");
        assert_eq!(
            serde_json::to_vec(&still_pending).unwrap(),
            serde_json::to_vec(&expected_page0).unwrap(),
            "capacity failure must not advance cursor or replace the logical snapshot"
        );

        // Simulate the bounded retry timer firing. The retry is the exact same
        // page 0, and repeated transient pressure advances only the backoff.
        try_queue_project_inventory_page(transport, &mut sync, &tx);
        let retry_page0 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected retried project inventory page, got {other:?}"),
        };
        assert_eq!(
            serde_json::to_vec(&retry_page0).unwrap(),
            serde_json::to_vec(&expected_page0).unwrap()
        );
        let second_delay = handle_project_inventory_status(
            transport,
            capacity,
            &mut sync,
            &tx,
            &mut retry_backoff,
        );
        assert_eq!(
            second_delay,
            ProjectInventoryStatusAction::RetryExactAfter(Duration::from_secs(2))
        );

        try_queue_project_inventory_page(transport, &mut sync, &tx);
        let accepted_retry_page0 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected second retried page 0, got {other:?}"),
        };
        assert_eq!(
            serde_json::to_vec(&accepted_retry_page0).unwrap(),
            serde_json::to_vec(&expected_page0).unwrap()
        );
        let accepted_page0 = inventory_status(
            "in_progress",
            &generation,
            100,
            expected_page0.projects.len(),
        );
        assert_eq!(
            handle_project_inventory_status(
                transport,
                accepted_page0,
                &mut sync,
                &tx,
                &mut retry_backoff,
            ),
            ProjectInventoryStatusAction::None
        );
        let page1 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected page 1 after accepted retry, got {other:?}"),
        };
        assert_eq!(page1.page_index, 1);
        assert_eq!(page1.generation, generation);
        assert_eq!(page1.snapshot_sequence, expected_page0.snapshot_sequence);
        assert!(page1.complete);

        let complete = inventory_status("complete", &generation, 100, 100);
        assert_eq!(
            handle_project_inventory_status(
                transport,
                complete,
                &mut sync,
                &tx,
                &mut retry_backoff,
            ),
            ProjectInventoryStatusAction::None
        );
        assert!(
            sync.is_none(),
            "final acknowledgement must complete the sync"
        );

        // Permanent/malformed Server failures remain fail-closed and never
        // enter the streaming retry loop even when their generation differs.
        let mut permanent_sync = Some(ProjectInventorySync::new(
            (0..65)
                .map(|index| synthetic_project_summary(index, None))
                .collect(),
        ));
        permanent_sync
            .as_mut()
            .unwrap()
            .current_page()
            .unwrap()
            .unwrap();
        let permanent = ShellProjectInventoryStatus {
            sync_state: "degraded".to_string(),
            generation: Some("unrelated-generation".to_string()),
            total_reported: None,
            total_synced: 0,
            last_error_code: Some("project_inventory_page_too_large".to_string()),
            last_sync_at: Some(1),
            max_summaries_per_page: PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
            max_serialized_bytes_per_page: PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES,
        };
        assert_eq!(
            handle_project_inventory_status(
                transport,
                permanent,
                &mut permanent_sync,
                &tx,
                &mut retry_backoff,
            ),
            ProjectInventoryStatusAction::None
        );
        assert!(permanent_sync.is_none());
    }
}

#[tokio::test]
async fn streaming_stale_generation_resnapshots_current_projects_for_websocket_and_quic() {
    for transport in [StreamTransport::WebSocket, StreamTransport::Quic] {
        let temp = tempfile::tempdir().unwrap();
        let project_registry_dir = temp.path().join("project-registry");
        let project_root = temp.path().join("projects");
        write_synthetic_project_configs(&project_registry_dir, &project_root, 100);

        let mut cfg = test_runner_config("http://127.0.0.1:1".to_string());
        cfg.project_registry_dir = Some(project_registry_dir.clone());
        let runtime = test_runtime(&cfg);
        let mut initial_cache = RunnerProjectCache::default();
        let initial_projects = runtime.project_summaries(&mut initial_cache, &cfg);
        assert_eq!(initial_projects.len(), 100);

        let initial_sync = ProjectInventorySync::new(initial_projects);
        let generation_a = initial_sync.generation().to_string();
        let sequence_a = initial_sync.snapshot_sequence;
        let mut coordinator = StreamingProjectInventoryCoordinator::new(Some(initial_sync));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        coordinator.queue_pending(transport, &tx);
        let page_a0 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected generation A page 0, got {other:?}"),
        };
        assert_eq!(page_a0.generation, generation_a);
        assert_eq!(page_a0.snapshot_sequence, sequence_a);
        assert_eq!(page_a0.page_index, 0);
        assert!(!page_a0.complete);

        // Page 0 was accepted, so generation A is now staged on the Server.
        coordinator.handle_status(
            transport,
            inventory_status("in_progress", &generation_a, 100, page_a0.projects.len()),
            &cfg,
            &runtime,
            &tx,
        );
        let page_a1 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected generation A page 1, got {other:?}"),
        };
        assert_eq!(page_a1.generation, generation_a);
        assert_eq!(page_a1.page_index, 1);

        // Model a successful project mutation after its result was submitted:
        // project-registry now has 101 entries. The event-driven dirty signal may
        // eagerly create generation B before the Server-side dynamic projection
        // has retired A, so B alone is not sufficient for correctness.
        let added_root = project_root.join("new-project");
        std::fs::create_dir_all(&added_root).unwrap();
        std::fs::write(
            project_registry_dir.join("new-project.toml"),
            format!(
                "id = \"new-project\"\nname = \"New Project\"\npath = {:?}\nallow_patch = true\n",
                added_root.to_string_lossy()
            ),
        )
        .unwrap();
        coordinator.refresh_from_current_projects(
            transport,
            &cfg,
            &runtime,
            &tx,
            "project_inventory_local_project_mutation",
        );
        let page_b0 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected eager generation B page 0, got {other:?}"),
        };
        let generation_b = page_b0.generation.clone();
        let sequence_b = page_b0.snapshot_sequence;
        assert_ne!(generation_b, generation_a);
        assert!(sequence_b > sequence_a);
        assert_eq!(page_b0.page_index, 0);
        let observed_b = &coordinator.sync.as_ref().unwrap().projects;
        assert_eq!(observed_b.len(), 101);
        assert!(observed_b
            .iter()
            .any(|project| project.id == "project-0000"));
        assert!(observed_b
            .iter()
            .any(|project| project.id == "project-0099"));
        assert!(observed_b.iter().any(|project| project.id == "new-project"));

        // The authoritative dynamic projection can race after B/page0 and retire
        // it too. A stale status is therefore the synchronization fence: abandon
        // the old logical snapshot and re-observe project-registry as generation C.
        coordinator.handle_status(
            transport,
            ShellProjectInventoryStatus {
                sync_state: "complete".to_string(),
                generation: None,
                total_reported: Some(1),
                total_synced: 1,
                last_error_code: Some("project_inventory_stale_generation".to_string()),
                last_sync_at: Some(1),
                max_summaries_per_page: PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
                max_serialized_bytes_per_page: PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES,
            },
            &cfg,
            &runtime,
            &tx,
        );
        let page_c0 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected fresh generation C page 0, got {other:?}"),
        };
        let generation_c = page_c0.generation.clone();
        let sequence_c = page_c0.snapshot_sequence;
        assert_ne!(generation_c, generation_a);
        assert_ne!(generation_c, generation_b);
        assert!(sequence_c > sequence_b);
        assert_eq!(page_c0.page_index, 0);
        assert_eq!(page_c0.total_reported, 101);
        let observed_c = &coordinator.sync.as_ref().unwrap().projects;
        assert_eq!(observed_c.len(), 101);
        assert!(observed_c
            .iter()
            .any(|project| project.id == "project-0000"));
        assert!(observed_c
            .iter()
            .any(|project| project.id == "project-0099"));
        assert!(observed_c.iter().any(|project| project.id == "new-project"));

        coordinator.handle_status(
            transport,
            inventory_status("in_progress", &generation_c, 101, page_c0.projects.len()),
            &cfg,
            &runtime,
            &tx,
        );
        let page_c1 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected generation C final page, got {other:?}"),
        };
        assert_eq!(page_c1.generation, generation_c);
        assert_eq!(page_c1.snapshot_sequence, sequence_c);
        assert_eq!(page_c1.page_index, 1);
        assert!(page_c1.complete);
        coordinator.handle_status(
            transport,
            inventory_status("complete", &generation_c, 101, 101),
            &cfg,
            &runtime,
            &tx,
        );
        assert!(coordinator.sync.is_none());
        assert!(coordinator.retry_at().is_none());
        assert!(
            rx.try_recv().is_err(),
            "stale generation must not be resent"
        );

        runtime.shutdown();
    }
}

#[tokio::test]
async fn streaming_delayed_success_ack_does_not_discard_current_sync() {
    for transport in [StreamTransport::WebSocket, StreamTransport::Quic] {
        let projects_a = (0..100)
            .map(|index| synthetic_project_summary(index, None))
            .collect::<Vec<_>>();
        let sync_a = ProjectInventorySync::new(projects_a);
        let generation_a = sync_a.generation().to_string();
        let mut coordinator = StreamingProjectInventoryCoordinator::new(Some(sync_a));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cfg = test_runner_config("http://127.0.0.1:1".to_string());
        let runtime = test_runtime(&cfg);

        coordinator.queue_pending(transport, &tx);
        let page_a0 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected generation A page 0, got {other:?}"),
        };
        coordinator.handle_status(
            transport,
            inventory_status("in_progress", &generation_a, 100, page_a0.projects.len()),
            &cfg,
            &runtime,
            &tx,
        );
        let page_a1 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected generation A page 1, got {other:?}"),
        };
        assert_eq!(page_a1.page_index, 1);

        // A duplicated/delayed success acknowledgement for page 0 is older than
        // the pending page 1 cursor. It must not be interpreted as a permanent
        // progress mismatch and discard the still-current generation A sync.
        coordinator.handle_status(
            transport,
            inventory_status("in_progress", &generation_a, 100, page_a0.projects.len()),
            &cfg,
            &runtime,
            &tx,
        );
        let current_a = coordinator
            .sync
            .as_mut()
            .expect("generation A remains active");
        assert_eq!(current_a.generation(), generation_a);
        assert_eq!(
            serde_json::to_vec(&current_a.current_page().unwrap().unwrap()).unwrap(),
            serde_json::to_vec(&page_a1).unwrap(),
            "delayed page-0 acknowledgement must leave page 1 pending"
        );
        assert!(rx.try_recv().is_err());

        // A local mutation can replace A with a fresh generation B before A's
        // already-enqueued normal acknowledgement arrives. That old successful
        // acknowledgement belongs to A and must not tear down B.
        let projects_b = (0..101)
            .map(|index| synthetic_project_summary(index, None))
            .collect::<Vec<_>>();
        coordinator.sync = Some(ProjectInventorySync::new(projects_b));
        coordinator.retry_backoff.reset();
        coordinator.retry_at = None;
        coordinator.queue_pending(transport, &tx);
        let page_b0 = match rx.recv().await {
            Some(RunnerEnvelope::ProjectInventoryPage { page }) => page,
            other => panic!("expected generation B page 0, got {other:?}"),
        };
        let generation_b = page_b0.generation.clone();
        assert_ne!(generation_b, generation_a);

        // A transient capacity response owns a retry deadline for B. An old
        // successful A acknowledgement must not cancel that unrelated timer.
        coordinator.handle_status(
            transport,
            ShellProjectInventoryStatus {
                sync_state: "degraded".to_string(),
                generation: Some("previous-authoritative-generation".to_string()),
                total_reported: Some(1),
                total_synced: 1,
                last_error_code: Some("project_inventory_staging_capacity".to_string()),
                last_sync_at: Some(1),
                max_summaries_per_page: PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
                max_serialized_bytes_per_page: PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES,
            },
            &cfg,
            &runtime,
            &tx,
        );
        let retry_at_before_delayed_ack = coordinator
            .retry_at()
            .expect("capacity response schedules generation B retry");

        coordinator.handle_status(
            transport,
            inventory_status("in_progress", &generation_a, 100, page_a0.projects.len()),
            &cfg,
            &runtime,
            &tx,
        );
        assert_eq!(
            coordinator
                .sync
                .as_ref()
                .map(ProjectInventorySync::generation),
            Some(generation_b.as_str()),
            "delayed generation-A acknowledgement must not discard generation B"
        );
        assert_eq!(
            coordinator.retry_at(),
            Some(retry_at_before_delayed_ack),
            "delayed acknowledgement must not clear generation B capacity backoff"
        );
        assert!(rx.try_recv().is_err());
        runtime.shutdown();
    }
}

#[tokio::test]
async fn streaming_permanent_inventory_error_does_not_fresh_resnapshot() {
    for transport in [StreamTransport::WebSocket, StreamTransport::Quic] {
        let cfg = test_runner_config("http://127.0.0.1:1".to_string());
        let runtime = test_runtime(&cfg);
        let sync = ProjectInventorySync::new(
            (0..65)
                .map(|index| synthetic_project_summary(index, None))
                .collect(),
        );
        let mut coordinator = StreamingProjectInventoryCoordinator::new(Some(sync));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        coordinator.queue_pending(transport, &tx);
        assert!(matches!(
            rx.recv().await,
            Some(RunnerEnvelope::ProjectInventoryPage { .. })
        ));

        coordinator.handle_status(
            transport,
            ShellProjectInventoryStatus {
                sync_state: "degraded".to_string(),
                generation: Some("unrelated-generation".to_string()),
                total_reported: None,
                total_synced: 0,
                last_error_code: Some("project_inventory_page_too_large".to_string()),
                last_sync_at: Some(1),
                max_summaries_per_page: PROJECT_INVENTORY_PAGE_MAX_SUMMARIES,
                max_serialized_bytes_per_page: PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES,
            },
            &cfg,
            &runtime,
            &tx,
        );

        assert!(coordinator.sync.is_none());
        assert!(coordinator.retry_at().is_none());
        assert!(
            rx.try_recv().is_err(),
            "permanent inventory failure must not create a fresh generation loop"
        );
        runtime.shutdown();
    }
}

#[test]
fn polling_once_startup_with_100_projects_registers_liveness_then_completes_paged_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let project_registry_dir = temp.path().join("project-registry");
    let project_root = temp.path().join("projects");
    write_synthetic_project_configs(&project_registry_dir, &project_root, 100);

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let next_page = Arc::new(AtomicUsize::new(0));
    let runner_shutdown = Arc::new(AtomicBool::new(false));
    let handler = {
        let seen = Arc::clone(&seen);
        let next_page = Arc::clone(&next_page);
        let runner_shutdown = Arc::clone(&runner_shutdown);
        Arc::new(move |path: &str, body: &str| match path {
            "/api/shell/agent/register" => {
                let payload: serde_json::Value = serde_json::from_str(body).unwrap();
                assert!(payload.get("projects").is_none());
                assert!(payload.get("agent_protocol_version").is_none());
                assert_eq!(
                    payload["agent_protocol_generation"],
                    RUNNER_PROTOCOL_GENERATION_V2.get()
                );
                assert!(payload["capabilities"]["shell"].is_boolean());
                register_inventory_support_response()
            }
            "/api/shell/agent/poll" => {
                let payload: serde_json::Value = serde_json::from_str(body).unwrap();
                let page = &payload["project_inventory_page"];
                assert!(
                    page.is_object(),
                    "paged inventory should begin immediately after register"
                );
                let page_index = page["page_index"].as_u64().unwrap() as usize;
                assert_eq!(page_index, next_page.load(Ordering::SeqCst));
                let generation = page["generation"].as_str().unwrap().to_string();
                let projects = page["projects"].as_array().unwrap();
                assert!(projects.len() <= PROJECT_INVENTORY_PAGE_MAX_SUMMARIES);
                assert!(
                    serde_json::to_vec(page).unwrap().len()
                        <= PROJECT_INVENTORY_PAGE_MAX_SERIALIZED_BYTES
                );
                seen.lock().unwrap().extend(
                    projects
                        .iter()
                        .map(|project| project["id"].as_str().unwrap().to_string()),
                );
                let synced = seen.lock().unwrap().len();
                let complete = page["complete"].as_bool().unwrap();
                next_page.fetch_add(1, Ordering::SeqCst);
                if complete {
                    runner_shutdown.store(true, Ordering::SeqCst);
                }
                poll_inventory_response(&inventory_status(
                    if complete { "complete" } else { "in_progress" },
                    &generation,
                    100,
                    synced,
                ))
            }
            "/api/shell/agent/offline" => polling_offline_success_response(),
            other => panic!("unexpected project-inventory polling endpoint: {other}"),
        })
    };
    let server = start_concurrent_polling_server(handler);
    let mut cfg = polling_runner_config(server.server_url.clone(), project_registry_dir);
    cfg.policy.allowed_roots = vec![temp.path().to_path_buf()];
    let runtime = test_runtime(&cfg);
    let result = run_polling_runner_with_shutdown(
        cfg,
        true,
        "inst-project-inventory",
        Arc::clone(&runner_shutdown),
        &runtime,
    );
    assert!(
        result.is_ok(),
        "Runner should remain online through inventory sync: {result:?}"
    );
    server.finish();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 100);
    assert_eq!(seen.first().map(String::as_str), Some("project-0000"));
    assert!(seen.iter().any(|id| id == "project-0050"));
    assert_eq!(seen.last().map(String::as_str), Some("project-0099"));
    assert_eq!(next_page.load(Ordering::SeqCst), 2);
}

#[test]
fn polling_project_refresh_is_periodic_and_invalidation_is_immediate() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = polling_runner_config(
        "http://127.0.0.1:1".to_string(),
        temp.path().join("project-registry"),
    );
    let shutdown = AtomicBool::new(false);
    let mut project_cache = RunnerProjectCache::default();
    let start = Instant::now();
    let _ = project_cache.get_with_shutdown(&cfg, Some(&shutdown));
    let mut refresh = PollingProjectRefresh::new(start);

    assert!(polling_projects_for_poll(
        &refresh,
        &mut project_cache,
        &cfg,
        &shutdown,
        start + POLLING_PROJECT_REFRESH_INTERVAL - Duration::from_millis(1),
    )
    .is_none());
    assert!(polling_projects_for_poll(
        &refresh,
        &mut project_cache,
        &cfg,
        &shutdown,
        start + POLLING_PROJECT_REFRESH_INTERVAL,
    )
    .is_some());

    refresh.mark_sent(start + POLLING_PROJECT_REFRESH_INTERVAL);
    project_cache.invalidate();
    assert!(polling_projects_for_poll(
        &refresh,
        &mut project_cache,
        &cfg,
        &shutdown,
        start + POLLING_PROJECT_REFRESH_INTERVAL + Duration::from_millis(1),
    )
    .is_some());
}

#[test]
fn polling_shutdown_interrupts_retry_sleep() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let trigger = Arc::clone(&shutdown);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        trigger.store(true, Ordering::SeqCst);
    });

    let started = Instant::now();
    assert!(sleep_or_shutdown(Duration::from_secs(5), shutdown.as_ref()));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "shutdown-aware polling sleep did not return promptly"
    );
}

#[test]
fn websocket_proxy_env_precedence_and_no_proxy_bypass() {
    use std::ffi::OsString;

    let values = std::collections::HashMap::from([
        (
            "HTTPS_PROXY",
            OsString::from("http://https-upper.test:8001"),
        ),
        (
            "https_proxy",
            OsString::from("http://https-lower.test:8002"),
        ),
        ("HTTP_PROXY", OsString::from("http://http-upper.test:8003")),
        ("http_proxy", OsString::from("http://http-lower.test:8004")),
        ("ALL_PROXY", OsString::from("http://all-upper.test:8005")),
        ("all_proxy", OsString::from("http://all-lower.test:8006")),
    ]);
    let wss =
        websocket_proxy_from_env_with("wss://example.test/ws", |name| values.get(name).cloned())
            .unwrap()
            .unwrap();
    assert_eq!(wss.host, "https-upper.test");
    assert_eq!(wss.port, 8001);
    let ws =
        websocket_proxy_from_env_with("ws://example.test/ws", |name| values.get(name).cloned())
            .unwrap()
            .unwrap();
    assert_eq!(ws.host, "http-upper.test");
    assert_eq!(ws.port, 8003);

    let fallback_values = std::collections::HashMap::from([
        (
            "https_proxy",
            OsString::from("http://https-lower-only.test:8101"),
        ),
        ("ALL_PROXY", OsString::from("http://all-fallback.test:8102")),
    ]);
    let wss = websocket_proxy_from_env_with("wss://example.test/ws", |name| {
        fallback_values.get(name).cloned()
    })
    .unwrap()
    .unwrap();
    assert_eq!(wss.host, "https-lower-only.test");
    assert_eq!(wss.port, 8101);

    let all_only = std::collections::HashMap::from([(
        "all_proxy",
        OsString::from("http://all-lower-only.test:8103"),
    )]);
    let ws =
        websocket_proxy_from_env_with("ws://example.test/ws", |name| all_only.get(name).cloned())
            .unwrap()
            .unwrap();
    assert_eq!(ws.host, "all-lower-only.test");
    assert_eq!(ws.port, 8103);

    for (url, no_proxy, bypass) in [
        ("ws://localhost/ws", "localhost", true),
        ("ws://127.0.0.1/ws", "127.0.0.1", true),
        ("wss://api.example.com/ws", "api.example.com", true),
        ("wss://deep.example.com/ws", ".example.com", true),
        (
            "wss://api.example.com:8443/ws",
            "api.example.com:8443",
            true,
        ),
        ("wss://api.example.com/ws", "api.example.com:8443", false),
        ("wss://anything.test/ws", "*", true),
    ] {
        let values = std::collections::HashMap::from([
            ("HTTP_PROXY", OsString::from("http://proxy.test:8080")),
            ("HTTPS_PROXY", OsString::from("http://proxy.test:8080")),
            ("NO_PROXY", OsString::from(no_proxy)),
        ]);
        let selected =
            websocket_proxy_from_env_with(url, |name| values.get(name).cloned()).unwrap();
        assert_eq!(selected.is_none(), bypass, "url={url} no_proxy={no_proxy}");
    }

    let lower_no_proxy = std::collections::HashMap::from([
        ("HTTP_PROXY", OsString::from("http://proxy.test:8080")),
        ("no_proxy", OsString::from("localhost")),
    ]);
    assert!(websocket_proxy_from_env_with("ws://localhost/ws", |name| {
        lower_no_proxy.get(name).cloned()
    })
    .unwrap()
    .is_none());
}

#[test]
fn websocket_proxy_ipv6_hosts_are_canonical() {
    use std::ffi::OsString;

    let proxy = parse_http_proxy_endpoint("http://[::1]:8080").unwrap();
    assert_eq!(proxy.host, "::1");
    assert_eq!(proxy.port, 8080);

    let (target_host, target_port) =
        websocket_target_endpoint("wss://[2001:db8::1]/api/agents/ws").unwrap();
    assert_eq!(target_host, "2001:db8::1");
    assert_eq!(target_port, 443);
    let authority = target_authority(&target_host, target_port);
    assert_eq!(authority, "[2001:db8::1]:443");
    assert!(!authority.contains("[["), "{authority}");

    let values = std::collections::HashMap::from([
        ("HTTP_PROXY", OsString::from("http://proxy.test:8080")),
        ("NO_PROXY", OsString::from("::1")),
    ]);
    assert!(
        websocket_proxy_from_env_with("ws://[::1]/api/agents/ws", |name| {
            values.get(name).cloned()
        })
        .unwrap()
        .is_none()
    );
}

#[test]
fn websocket_proxy_invalid_configuration_is_sanitized() {
    use std::ffi::OsString;

    let proxy_secret = "PROXY_PASSWORD_DO_NOT_LEAK";
    let query_secret = "PROXY_QUERY_DO_NOT_LEAK";
    let raw = format!("http://proxy-user:{proxy_secret}@proxy.test:8080/?token={query_secret}");
    let values = std::collections::HashMap::from([("HTTP_PROXY", OsString::from(raw))]);
    let error =
        websocket_proxy_from_env_with("ws://server.test/ws", |name| values.get(name).cloned())
            .expect_err("credentialed proxy URL is intentionally unsupported");
    assert!(matches!(error, RunnerTransportError::ProxyConfiguration(_)));
    let error = error.to_string();
    assert!(
        error.contains("proxy authentication is unsupported"),
        "{error}"
    );
    assert!(!error.contains("proxy-user"), "{error}");
    assert!(!error.contains(proxy_secret), "{error}");
    assert!(!error.contains(query_secret), "{error}");
}

#[tokio::test]
async fn websocket_http_proxy_connect_tunnels_websocket_handshake() {
    use tokio::io::AsyncWriteExt;

    let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target_listener.local_addr().unwrap();
    let seen_auth = Arc::new(Mutex::new(None::<String>));
    let server_seen_auth = Arc::clone(&seen_auth);
    let target = tokio::spawn(async move {
        let (stream, _) = target_listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(
            stream,
            move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                  response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            *server_seen_auth.lock().unwrap() = request
                .headers()
                .get(tokio_tungstenite::tungstenite::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            Ok(response)
        })
        .await
        .unwrap();
        let _ = ws.send(WsMessage::Close(None)).await;
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut downstream, _) = proxy_listener.accept().await.unwrap();
        let connect = read_async_http_headers(&mut downstream).await;
        let expected = format!("CONNECT {target_addr} HTTP/1.1");
        assert!(connect.starts_with(&expected), "{connect}");
        assert!(
            !connect.to_ascii_lowercase().contains("authorization:"),
            "{connect}"
        );
        assert!(!connect.contains("SERVER_TOKEN_DO_NOT_LEAK"), "{connect}");
        let mut upstream = tokio::net::TcpStream::connect(target_addr).await.unwrap();
        downstream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
        connect
    });

    let token = "SERVER_TOKEN_DO_NOT_LEAK";
    let ws_url = format!("ws://{target_addr}/api/agents/ws");
    let request = build_ws_request(&ws_url, token).unwrap();
    let endpoint = HttpProxyEndpoint {
        host: "127.0.0.1".to_string(),
        port: proxy_addr.port(),
    };
    let ws = tokio::time::timeout(
        Duration::from_secs(5),
        connect_websocket_request_with_proxy(request, &ws_url, Some(&endpoint), token),
    )
    .await
    .expect("proxy websocket connect timed out")
    .expect("proxy websocket connect failed");
    drop(ws);

    let connect = tokio::time::timeout(Duration::from_secs(5), proxy)
        .await
        .expect("proxy tunnel task did not finish")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), target)
        .await
        .expect("target websocket task did not finish")
        .unwrap();
    assert!(connect.starts_with(&format!("CONNECT {target_addr} HTTP/1.1")));
    assert_eq!(
        seen_auth.lock().unwrap().as_deref(),
        Some("Bearer SERVER_TOKEN_DO_NOT_LEAK")
    );
}

#[tokio::test]
async fn websocket_proxy_ipv6_target_connect_authority_has_single_brackets() {
    use tokio::io::AsyncWriteExt;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let connect = read_async_http_headers(&mut stream).await;
        assert!(
            connect.starts_with("CONNECT [2001:db8::1]:443 HTTP/1.1"),
            "{connect}"
        );
        assert!(!connect.contains("[[2001:db8::1]]"), "{connect}");
        stream
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        connect
    });

    let ws_url = "wss://[2001:db8::1]/api/agents/ws";
    let request = build_ws_request(ws_url, "SERVER_TOKEN_DO_NOT_LEAK").unwrap();
    let endpoint = HttpProxyEndpoint {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
    };
    let error = connect_websocket_request_with_proxy(
        request,
        ws_url,
        Some(&endpoint),
        "SERVER_TOKEN_DO_NOT_LEAK",
    )
    .await
    .expect_err("synthetic proxy must reject the IPv6 target");
    let connect = proxy.await.unwrap();

    assert!(connect.starts_with("CONNECT [2001:db8::1]:443 HTTP/1.1"));
    assert!(
        matches!(error, RunnerTransportError::Transient(_)),
        "{error}"
    );
}

#[tokio::test]
async fn websocket_wss_proxy_uses_connect_before_target_tls() {
    use tokio::io::AsyncWriteExt;

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let server_token = "SERVER_TOKEN_DO_NOT_LEAK";
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = proxy_listener.accept().await.unwrap();
        let connect = read_async_http_headers(&mut stream).await;
        assert!(
            connect.starts_with("CONNECT server.test:443 HTTP/1.1"),
            "{connect}"
        );
        assert!(
            !connect.to_ascii_lowercase().contains("authorization:"),
            "{connect}"
        );
        assert!(!connect.contains(server_token), "{connect}");
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        connect
    });

    let ws_url = "wss://server.test/api/agents/ws";
    let request = build_ws_request(ws_url, server_token).unwrap();
    let endpoint = HttpProxyEndpoint {
        host: "127.0.0.1".to_string(),
        port: proxy_addr.port(),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        connect_websocket_request_with_proxy(request, ws_url, Some(&endpoint), server_token),
    )
    .await
    .expect("wss proxy CONNECT test timed out")
    .expect_err("the synthetic tunnel closes before target TLS can complete");
    let connect = proxy.await.unwrap();

    assert!(connect.starts_with("CONNECT server.test:443 HTTP/1.1"));
    let error = error.to_string();
    assert!(error.contains("websocket connect failed"), "{error}");
    assert!(!error.contains(server_token), "{error}");
}

#[tokio::test]
async fn websocket_proxy_network_failure_remains_transient() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);
    });
    let endpoint = HttpProxyEndpoint {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
    };
    let error = http_proxy_connect_tunnel(&endpoint, "server.test", 443)
        .await
        .expect_err("closed proxy connection must fail");
    peer.await.unwrap();
    assert!(
        matches!(error, RunnerTransportError::Transient(_)),
        "{error}"
    );
}

#[tokio::test]
async fn websocket_proxy_connect_rejects_non_success_without_leaking_secrets() {
    use tokio::io::AsyncWriteExt;

    let proxy_secret = "PROXY_RESPONSE_SECRET_DO_NOT_LEAK";
    let server_token = "SERVER_TOKEN_DO_NOT_LEAK";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let connect = read_async_http_headers(&mut stream).await;
        assert!(!connect.contains(server_token), "{connect}");
        let response = format!(
            "HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: {}\r\n\r\n{}",
            proxy_secret.len(),
            proxy_secret
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });

    let ws_url = "ws://127.0.0.1:9/api/agents/ws";
    let request = build_ws_request(ws_url, server_token).unwrap();
    let endpoint = HttpProxyEndpoint {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        connect_websocket_request_with_proxy(request, ws_url, Some(&endpoint), server_token),
    )
    .await
    .expect("non-success CONNECT test timed out")
    .expect_err("non-2xx CONNECT status must fail");
    proxy.await.unwrap();
    assert!(
        matches!(&error, RunnerTransportError::ProxyConfiguration(_)),
        "{error}"
    );
    let error = error.to_string();
    assert!(error.contains("HTTP 407"), "{error}");
    assert!(!error.contains(proxy_secret), "{error}");
    assert!(!error.contains(server_token), "{error}");
}

#[tokio::test]
async fn websocket_proxy_connect_response_header_is_bounded_and_redacted() {
    use tokio::io::AsyncWriteExt;

    let proxy_secret = "PROXY_RESPONSE_SECRET_DO_NOT_LEAK";
    let server_token = "SERVER_TOKEN_DO_NOT_LEAK";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let connect = read_async_http_headers(&mut stream).await;
        assert!(!connect.contains(server_token), "{connect}");
        let mut response = format!("HTTP/1.1 200 OK\r\nX-Secret: {proxy_secret}\r\n").into_bytes();
        response.extend(std::iter::repeat_n(
            b'x',
            WS_PROXY_CONNECT_HEADER_MAX_BYTES + 1024,
        ));
        let _ = stream.write_all(&response).await;
    });

    let ws_url = "ws://127.0.0.1:9/api/agents/ws";
    let request = build_ws_request(ws_url, server_token).unwrap();
    let endpoint = HttpProxyEndpoint {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        connect_websocket_request_with_proxy(request, ws_url, Some(&endpoint), server_token),
    )
    .await
    .expect("bounded CONNECT response test timed out")
    .expect_err("oversized CONNECT headers must fail");
    proxy.await.unwrap();
    let error = error.to_string();
    assert!(error.contains("response headers exceeded"), "{error}");
    assert!(!error.contains(proxy_secret), "{error}");
    assert!(!error.contains(server_token), "{error}");
}

#[test]
fn quic_transport_config_applies_keepalive_without_changing_application_ping() {
    let quic = QuicClientConfig {
        server_addr: "127.0.0.1:8443".to_string(),
        server_name: "localhost".to_string(),
        alpn: crate::webcodex_runner::default_quic_alpn(),
        connect_timeout_secs: 10,
        keepalive_interval_secs: 17,
    };
    let transport = build_quic_transport_config(&quic).unwrap();
    let rendered = format!("{transport:?}");
    assert!(
        rendered.contains("max_idle_timeout: Some(30000)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("keep_alive_interval: Some(17s)"),
        "{rendered}"
    );
    assert_eq!(QUIC_PING_INTERVAL, Duration::from_secs(30));
}

#[tokio::test]
async fn quic_graceful_writer_waits_for_flush_and_stuck_writer_is_bounded() {
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (flushed_tx, flushed_rx) = tokio::sync::oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        let _ = release_rx.await;
        let _ = flushed_tx.send(());
        StreamWriterExit::GracefulClose
    });
    let finish = finish_quic_writer(Some(writer), true, Duration::from_secs(1));
    tokio::pin!(finish);
    tokio::select! {
        _ = &mut finish => panic!("graceful QUIC finish returned before writer flush"),
        _ = tokio::task::yield_now() => {}
    }
    release_tx.send(()).unwrap();
    assert!(
        finish.await,
        "graceful writer must report a flushed Goodbye"
    );
    flushed_rx.await.expect("writer flush marker");

    let stuck = tokio::spawn(async { std::future::pending::<StreamWriterExit>().await });
    let stuck_graceful = tokio::time::timeout(
        Duration::from_millis(100),
        finish_quic_writer(Some(stuck), true, Duration::from_millis(10)),
    )
    .await
    .expect("stuck graceful QUIC writer must be bounded");
    assert!(!stuck_graceful);

    let broken = tokio::spawn(async { std::future::pending::<StreamWriterExit>().await });
    let broken_graceful = tokio::time::timeout(
        Duration::from_millis(100),
        finish_quic_writer(Some(broken), false, Duration::from_secs(1)),
    )
    .await
    .expect("non-graceful QUIC writer teardown must abort promptly");
    assert!(!broken_graceful);
}

#[tokio::test]
async fn quic_graceful_close_waits_for_peer_within_remaining_budget() {
    let (peer_closed_tx, peer_closed_rx) = tokio::sync::oneshot::channel::<()>();
    let wait = wait_for_quic_peer_close(
        async {
            let _ = peer_closed_rx.await;
        },
        Duration::from_secs(1),
    );
    tokio::pin!(wait);

    tokio::select! {
        _ = &mut wait => panic!("graceful QUIC close skipped the peer-close grace period"),
        _ = tokio::task::yield_now() => {}
    }

    peer_closed_tx.send(()).unwrap();
    wait.await;

    tokio::time::timeout(
        Duration::from_millis(100),
        wait_for_quic_peer_close(std::future::pending::<()>(), Duration::from_millis(10)),
    )
    .await
    .expect("non-responsive QUIC peer grace wait must remain bounded");
}

#[tokio::test]
async fn streaming_writer_failure_terminates_pending_reader_for_ws_and_quic() {
    for transport in [StreamTransport::WebSocket, StreamTransport::Quic] {
        let cfg = test_runner_config("http://127.0.0.1:9".to_string());
        let runtime = test_runtime(&cfg);
        let registered_jobs = ShellJobInventory::default();
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<RunnerEnvelope>(WS_OUTGOING_CAPACITY);
        let (_read_tx, read_rx) = tokio::sync::mpsc::channel::<StreamRead>(1);
        let writer_task = tokio::spawn(async { StreamWriterExit::TransportFailed });

        let exit = tokio::time::timeout(
            Duration::from_millis(250),
            serve_registered_stream(
                transport,
                &cfg,
                "inst-writer-fail",
                &registered_jobs,
                out_tx,
                RegisteredStream::Test { reader: read_rx },
                writer_task,
                None,
                &runtime,
                std::future::pending::<()>(),
            ),
        )
        .await
        .expect("writer failure must not wait for pending reader")
        .expect("writer failure is a transport disconnect, not a session error");
        assert_eq!(
            exit,
            RunnerSessionExit::TransportDisconnected,
            "{transport:?}"
        );
    }
}

#[tokio::test]
async fn streaming_graceful_writer_completion_is_not_a_failure_signal() {
    let cfg = test_runner_config("http://127.0.0.1:9".to_string());
    let runtime = test_runtime(&cfg);
    let registered_jobs = ShellJobInventory::default();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<RunnerEnvelope>(WS_OUTGOING_CAPACITY);
    let (_read_tx, read_rx) = tokio::sync::mpsc::channel::<StreamRead>(1);
    let writer_task = tokio::spawn(async move {
        while let Some(env) = out_rx.recv().await {
            if matches!(env, RunnerEnvelope::Goodbye { .. }) {
                return StreamWriterExit::GracefulClose;
            }
        }
        StreamWriterExit::ChannelClosed
    });

    let exit = serve_registered_stream(
        StreamTransport::WebSocket,
        &cfg,
        "inst-graceful",
        &registered_jobs,
        out_tx,
        RegisteredStream::Test { reader: read_rx },
        writer_task,
        None,
        &runtime,
        async {},
    )
    .await
    .expect("graceful shutdown must not be classified as writer failure");
    assert_eq!(exit, RunnerSessionExit::Shutdown);
}

#[tokio::test]
async fn websocket_close_returns_transport_disconnect_not_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _register = read_register(&mut ws).await;
        send_registered_ack(&mut ws).await;
        ws.send(WsMessage::Close(None)).await.unwrap();
        if let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if msg.is_text() {
                let env = RunnerEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap();
                assert!(
                    !matches!(env, RunnerEnvelope::Goodbye { .. }),
                    "ordinary transport disconnect must not send Goodbye"
                );
            }
        }
    });

    let cfg = test_runner_config(format!("http://{}", addr));
    let exit = tokio::time::timeout(
        Duration::from_secs(5),
        websocket_session(
            &cfg,
            vec![test_project("close-test")],
            "inst-close",
            &test_runtime(&cfg),
        ),
    )
    .await
    .expect("session completed")
    .expect("session should not error");

    assert_eq!(exit, RunnerSessionExit::TransportDisconnected);
    assert_ne!(exit, RunnerSessionExit::Shutdown);
    server.await.unwrap();
}

#[tokio::test]
async fn websocket_disconnect_with_active_job_returns_without_waiting_for_job() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let request = start_job_request(cwd.path(), "sleep 2");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _register = read_register(&mut ws).await;
        send_registered_ack(&mut ws).await;
        ws.send(WsMessage::Text(
            RunnerEnvelope::Request { request }
                .to_json()
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let msg = ws.next().await.unwrap().unwrap();
                if !msg.is_text() {
                    continue;
                }
                match RunnerEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap() {
                    RunnerEnvelope::JobUpdate { payload }
                        if payload.job_id == "job-active" && !payload.finished =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("agent did not report active job");

        ws.send(WsMessage::Close(None)).await.unwrap();
    });

    let cfg = test_runner_config(format!("http://{}", addr));
    let started = Instant::now();
    let exit = tokio::time::timeout(
        Duration::from_millis(900),
        websocket_session(
            &cfg,
            vec![test_project("active-job-test")],
            "inst-active-job",
            &test_runtime(&cfg),
        ),
    )
    .await
    .expect("session must return promptly after disconnect despite active job")
    .expect("session should not error");

    assert_eq!(exit, RunnerSessionExit::TransportDisconnected);
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "session waited for the active job instead of reconnecting"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn websocket_register_rejected_is_fatal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _register = read_register(&mut ws).await;
        send_register_rejected_ack(&mut ws).await;
    });

    let cfg = test_runner_config(format!("http://{}", addr));
    let error = websocket_session(
        &cfg,
        vec![test_project("reject-test")],
        "inst-reject",
        &test_runtime(&cfg),
    )
    .await
    .expect_err("register rejection must error");
    let classified = classify_session_error(error);
    assert!(classified.is_fatal(), "{classified}");
    server.await.unwrap();
}

#[tokio::test]
async fn strict_websocket_transient_connect_failure_reconnects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (first_stream, _) = listener.accept().await.unwrap();
        drop(first_stream);

        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _register = read_register(&mut ws).await;
        send_register_rejected_ack(&mut ws).await;
    });

    let cfg = test_runner_config(format!("http://{}", addr));
    let runtime = test_runtime(&cfg);
    let started = Instant::now();
    let runner = tokio::task::spawn_blocking(move || {
        run_websocket_runner(cfg, false, "inst-retry", &runtime)
    });
    let error = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("strict websocket retry did not finish after fatal register rejection")
        .unwrap()
        .expect_err("register rejection after reconnect must be fatal");

    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "strict websocket did not wait for reconnect backoff after transient connect failure"
    );
    assert!(error.contains("register rejected"), "{error}");
    server.await.unwrap();
}

#[tokio::test]
async fn strict_websocket_once_stops_after_first_registered_disconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _register = read_register(&mut ws).await;
        send_registered_ack(&mut ws).await;
        ws.send(WsMessage::Close(None)).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1_500), listener.accept())
                .await
                .is_err(),
            "--once must not open a reconnecting websocket session"
        );
    });

    let cfg = test_runner_config(format!("http://{}", addr));
    let runtime = test_runtime(&cfg);
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || run_websocket_runner(cfg, true, "inst-once", &runtime)),
    )
    .await
    .expect("websocket --once did not stop after the first disconnect")
    .unwrap()
    .expect("registered websocket --once disconnect should be successful");
    server.await.unwrap();
}

#[tokio::test]
async fn auto_websocket_register_rejected_is_fatal_without_polling_fallback() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _register = read_register(&mut ws).await;
        send_register_rejected_ack(&mut ws).await;
    });

    let mut cfg = test_runner_config(format!("http://{}", addr));
    cfg.transport = Some(TRANSPORT_AUTO.to_string());
    let runtime = test_runtime(&cfg);
    let runner = tokio::task::spawn_blocking(move || {
        run_auto_runner(cfg, false, "inst-auto-reject", &runtime)
    });
    let error = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("auto websocket register rejection did not return")
        .unwrap()
        .expect_err("fatal register rejection must not fall back to polling");

    assert!(error.contains("register rejected"), "{error}");
    server.await.unwrap();
}

#[tokio::test]
async fn websocket_disconnect_loop_reregisters_identity_generation_and_capabilities() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (reg_tx, mut reg_rx) = mpsc::channel(2);
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let register = read_register(&mut ws).await;
            reg_tx.send(register).await.unwrap();
            send_registered_ack(&mut ws).await;
            ws.send(WsMessage::Close(None)).await.unwrap();
        }
    });

    let cfg = test_runner_config(format!("http://{}", addr));
    let projects = vec![test_project("repo-one")];
    for instance in ["inst-reconnect", "inst-reconnect"] {
        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            websocket_session(&cfg, projects.clone(), instance, &test_runtime(&cfg)),
        )
        .await
        .expect("session completed")
        .expect("session should not error");
        assert_eq!(exit, RunnerSessionExit::TransportDisconnected);
    }

    let first = reg_rx.recv().await.expect("first register");
    let second = reg_rx.recv().await.expect("second register");
    for register in [first, second] {
        assert_eq!(register.client_id, "oe");
        assert_eq!(register.runner_instance_id, "inst-reconnect");
        assert_eq!(
            register.runner_protocol_generation,
            RUNNER_PROTOCOL_GENERATION_V2
        );
        let caps = register.capabilities;
        assert!(caps.shell);
        assert!(caps.file_read);
        assert!(caps.file_write);
        assert!(caps.jobs);
        assert!(caps.async_jobs);
        assert!(caps.async_shell_jobs);
        assert!(caps.git);
    }

    server.await.unwrap();
}

#[tokio::test]
async fn websocket_reconnect_backoff_is_interrupted_by_process_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _register = read_register(&mut ws).await;
        send_registered_ack(&mut ws).await;
        ws.send(WsMessage::Close(None)).await.unwrap();
        closed_tx.send(()).unwrap();
    });

    let cfg = test_runner_config(format!("http://{}", addr));
    let runtime =
        RunnerRuntimeState::with_shutdown_budget(&cfg, PathBuf::new(), Duration::from_millis(500));
    let runner_runtime = runtime.clone();
    let runner = tokio::task::spawn_blocking(move || {
        run_websocket_runner(cfg, false, "inst-backoff-shutdown", &runner_runtime)
    });
    closed_rx.await.unwrap();
    let started = Instant::now();
    runtime.request_shutdown_signal();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("websocket reconnect backoff ignored shutdown")
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "websocket reconnect shutdown was delayed"
    );
    assert_eq!(runtime.coordinator.run_count(), 1);
    server.await.unwrap();
}

#[test]
fn quic_connect_or_reconnect_wait_is_interrupted_by_process_shutdown() {
    let mut cfg = test_runner_config("https://localhost".to_string());
    cfg.transport = Some(TRANSPORT_QUIC.to_string());
    cfg.quic = Some(QuicClientConfig {
        server_addr: "127.0.0.1:9".to_string(),
        server_name: "localhost".to_string(),
        alpn: crate::webcodex_runner::default_quic_alpn(),
        connect_timeout_secs: 10,
        keepalive_interval_secs: 20,
    });
    let runtime =
        RunnerRuntimeState::with_shutdown_budget(&cfg, PathBuf::new(), Duration::from_millis(500));
    let trigger_runtime = runtime.clone();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_millis(75));
        trigger_runtime.request_shutdown_signal();
    });
    let started = Instant::now();
    run_quic_runner(cfg, false, "inst-quic-shutdown", &runtime)
        .expect("QUIC process shutdown should be a normal exit");
    trigger.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "QUIC connect/reconnect wait ignored shutdown"
    );
    assert_eq!(runtime.coordinator.run_count(), 1);
}

#[tokio::test]
async fn websocket_process_shutdown_exits_gracefully() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (registered_tx, registered_rx) = oneshot::channel();
    let (goodbye_tx, goodbye_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _register = read_register(&mut ws).await;
        send_registered_ack(&mut ws).await;
        ws.send(WsMessage::Text(
            serde_json::to_string(&RunnerEnvelope::Ping { ts: 1 })
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("agent did not enter the registered session")
            .expect("stream open")
            .expect("pong message ok");
        assert!(matches!(
            RunnerEnvelope::from_slice(pong.into_text().unwrap().as_bytes()).unwrap(),
            RunnerEnvelope::Pong { ts: 1 }
        ));
        registered_tx.send(()).unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("agent did not send shutdown goodbye")
            .expect("stream open")
            .expect("message ok");
        match RunnerEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap() {
            RunnerEnvelope::Goodbye { reason } => goodbye_tx.send(reason).unwrap(),
            other => panic!("expected goodbye, got {}", other.kind()),
        }
    });

    let cfg = test_runner_config(format!("http://{}", addr));
    let runtime = test_runtime(&cfg);
    let session_runtime = runtime.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let session = tokio::spawn(async move {
        websocket_session_with_shutdown(
            &cfg,
            vec![test_project("shutdown-test")],
            "inst-shutdown",
            &session_runtime,
            async {
                let _ = shutdown_rx.await;
            },
        )
        .await
    });

    registered_rx.await.unwrap();
    shutdown_tx.send(()).unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("shutdown completed")
        .unwrap()
        .expect("session should not error");
    assert_eq!(exit, RunnerSessionExit::Shutdown);
    assert_eq!(
        goodbye_rx.await.unwrap().as_deref(),
        Some("process shutdown")
    );
    server.await.unwrap();
    runtime.shutdown();
    runtime.shutdown();
    assert_eq!(runtime.coordinator.run_count(), 1);
}
