use super::*;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tempfile::TempDir;
use webcodex_core::plugin::PluginGatewayResponsePayload;

static FAKE_PLUGIN_CHECK: OnceLock<Mutex<Weak<FakeBinary>>> = OnceLock::new();

struct FakeBinary {
    _temp: TempDir,
    path: PathBuf,
}

fn fake_binary() -> Arc<FakeBinary> {
    let cache = FAKE_PLUGIN_CHECK.get_or_init(|| Mutex::new(Weak::new()));
    let mut cached = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(binary) = cached.upgrade() {
        return binary;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join(format!(
        "webcodex-plugin-check-fake{}",
        env::consts::EXE_SUFFIX
    ));
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/webcodex_runner/fake_plugin.rs");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let result = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-name=webcodex_plugin_check_fake")
        .arg(source)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let binary = Arc::new(FakeBinary {
        _temp: temp,
        path: output,
    });
    *cached = Arc::downgrade(&binary);
    binary
}

struct CheckFixture {
    manager: Arc<PluginManager>,
    config_path: PathBuf,
    marker: PathBuf,
    fake: Arc<FakeBinary>,
    _temp: TempDir,
}

impl CheckFixture {
    fn new(candidate_scenario: &str, timeout_secs: u64) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("marker.log");
        let fake = fake_binary();
        let startup_plugins = PluginConfig {
            request_timeout_secs: timeout_secs,
            providers: vec![PluginProviderConfig {
                id: "fake".to_string(),
                name: "Fake Plugin".to_string(),
                command: fake.path.to_string_lossy().into_owned(),
                args: vec!["normal".to_string(), marker.to_string_lossy().into_owned()],
                cwd: Some(temp.path().to_string_lossy().into_owned()),
                profile: None,
                timeout_secs: Some(timeout_secs),
            }],
        };
        let config_path = temp.path().join("runner.toml");
        write_runner_toml(
            &config_path,
            temp.path(),
            &fake.path,
            &marker,
            candidate_scenario,
            timeout_secs,
        );
        let config = runner_config(startup_plugins, ShellConfig::default(), temp.path());
        let manager = Arc::new(PluginManager::new(&config, config_path.clone()));
        Self {
            manager,
            config_path,
            marker,
            fake,
            _temp: temp,
        }
    }

    fn check(&self) -> PluginGatewayResponse {
        self.manager.handle(PluginGatewayRequest::Check {
            provider_id: "fake".to_string(),
        })
    }

    fn rewrite_candidate(&self, scenario: &str, timeout_secs: u64) {
        write_runner_toml(
            &self.config_path,
            self._temp.path(),
            &self.fake.path,
            &self.marker,
            scenario,
            timeout_secs,
        );
    }

    fn marker_count(&self, value: &str) -> usize {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == value)
            .count()
    }

    fn marker_pids(&self, prefix: &str) -> Vec<u32> {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(prefix)?.parse().ok())
            .collect()
    }
}

fn checked_report(response: PluginGatewayResponse) -> PluginCheckReport {
    assert_eq!(response.dispatch_state, PluginDispatchState::Completed);
    assert!(response.error.is_none());
    let Some(PluginGatewayResponsePayload::Checked { report }) = response.payload else {
        panic!("missing Plugin check report");
    };
    report
}

fn provider_state(manager: &PluginManager) -> Vec<PluginProviderView> {
    let response = manager.handle(PluginGatewayRequest::ProvidersList);
    let Some(PluginGatewayResponsePayload::Providers { providers }) = response.payload else {
        panic!("missing provider state: {:?}", response.error);
    };
    providers
}

fn provider_tools(manager: &PluginManager, provider: &PluginProviderView) -> Vec<PluginTool> {
    let response = manager.handle(PluginGatewayRequest::ToolsList {
        provider_id: provider.provider_id.clone(),
        provider_instance_id: provider.provider_instance_id.clone(),
    });
    let Some(PluginGatewayResponsePayload::Tools { tools }) = response.payload else {
        panic!("missing provider tools: {:?}", response.error);
    };
    tools
}

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

#[test]
fn check_success_is_disposable_and_preserves_committed_plugin_state() {
    let fixture = CheckFixture::new("check_success_tree", 2);
    let state_before = provider_state(&fixture.manager);

    let report = checked_report(fixture.check());
    assert!(report.ready);
    assert_eq!(report.phase, PluginCheckPhase::Ready);
    assert_eq!(report.tool_count, 1);
    assert_eq!(report.tools[0].name, "echo");
    assert_eq!(
        fixture.marker_count("call"),
        0,
        "check must never call tools/call"
    );
    assert_eq!(provider_state(&fixture.manager), state_before);

    let candidate_pid = fixture
        .marker_pids("candidate-pid:")
        .into_iter()
        .next()
        .unwrap();
    let descendant_pid = fixture
        .marker_pids("descendant-pid:")
        .into_iter()
        .next()
        .unwrap();
    assert!(wait_until(Duration::from_secs(2), || {
        !crate::job_manager_tests::process_running(candidate_pid)
            && !crate::job_manager_tests::process_running(descendant_pid)
    }));
}

#[test]
fn check_observes_edited_v2_without_replacing_current_v1() {
    let fixture = CheckFixture::new("normal", 2);
    let current_v1 = provider_state(&fixture.manager).remove(0);
    let dynamic_v1 = current_v1.provider_instance_id.clone();
    let v1_schema = provider_tools(&fixture.manager, &current_v1)[0].schema_observation();
    let state_before_check = provider_state(&fixture.manager);

    fixture.rewrite_candidate("check_v2", 2);
    let report = checked_report(fixture.check());
    assert!(report.ready);
    assert_eq!(report.tools[0].name, "echo_v2");
    assert_eq!(provider_state(&fixture.manager), state_before_check);

    let v1_call = fixture.manager.handle(PluginGatewayRequest::ToolsCall {
        provider_id: "fake".to_string(),
        provider_instance_id: dynamic_v1.clone(),
        name: "echo".to_string(),
        arguments: json!({"value":"still-v1"}),
        expected_schema: v1_schema.clone(),
    });
    assert!(
        v1_call.error.is_none(),
        "check must not disturb current committed v1"
    );

    let reload_v2 = fixture.manager.handle(PluginGatewayRequest::Reload);
    let Some(PluginGatewayResponsePayload::Reloaded { providers, .. }) = reload_v2.payload else {
        panic!("v2 reload failed: {:?}", reload_v2.error);
    };
    let dynamic_v2 = providers[0].provider_instance_id.clone();
    assert_ne!(dynamic_v2, dynamic_v1);
    let tools = fixture.manager.handle(PluginGatewayRequest::ToolsList {
        provider_id: "fake".to_string(),
        provider_instance_id: dynamic_v2.clone(),
    });
    let Some(PluginGatewayResponsePayload::Tools { tools }) = tools.payload else {
        panic!("v2 tools/list failed: {:?}", tools.error);
    };
    assert_eq!(tools[0].name, "echo_v2");

    let calls_before_stale = fixture.marker_count("call");
    let stale_v1 = fixture.manager.handle(PluginGatewayRequest::ToolsCall {
        provider_id: "fake".to_string(),
        provider_instance_id: dynamic_v1,
        name: "echo".to_string(),
        arguments: json!({"value":"stale-v1"}),
        expected_schema: v1_schema,
    });
    assert_eq!(stale_v1.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        stale_v1.error.as_ref().unwrap().code,
        "stale_plugin_provider"
    );
    assert_eq!(
        fixture.marker_count("call"),
        calls_before_stale,
        "a stale provider instance must never dispatch into the new provider"
    );

    let v2_list_again = fixture.manager.handle(PluginGatewayRequest::ToolsList {
        provider_id: "fake".to_string(),
        provider_instance_id: dynamic_v2,
    });
    assert!(v2_list_again.error.is_none());
    fixture.manager.shutdown();
}

#[test]
fn initialize_eof_detail_is_actionable_without_inventing_a_root_cause() {
    let detail = initialize_failure_detail("plugin_eof");
    assert!(detail.contains("Plugin protocol output ended before initialize completed"));
    assert!(detail.contains("process may have exited or closed stdout"));
    assert!(detail.contains("configured command and arguments"));
    assert!(detail.contains("For a generated TypeScript Plugin"));
    assert!(detail.contains("npm run build"));
    assert!(detail.contains("dist/plugin.js"));
    assert!(!detail.contains("entrypoint_missing"));
    assert!(!detail.contains("Cannot find module"));
}

#[test]
fn initialize_eof_keeps_raw_stderr_runner_local() {
    let fixture = CheckFixture::new("check_init_crash_stderr", 2);
    let response = fixture.check();
    let encoded = serde_json::to_string(&response).unwrap();
    assert!(!encoded.contains("diagnostic-only-secret-looking-initialize-stderr"));
    assert!(!encoded.contains(fixture._temp.path().to_string_lossy().as_ref()));

    let report = checked_report(response);
    assert!(!report.ready);
    assert_eq!(report.phase, PluginCheckPhase::Initialize);
    assert_eq!(report.code.as_deref(), Some("plugin_eof"));
    assert_eq!(
        report.detail.as_deref(),
        Some(initialize_failure_detail("plugin_eof"))
    );
    assert!(report.diagnostic.is_none());

    let local = fixture
        .manager
        .local_check_stderr_diagnostics("fake")
        .expect("initialize failure should retain bounded Runner-local stderr");
    assert!(local
        .lines
        .iter()
        .any(|line| line.text == "diagnostic-only-secret-looking-initialize-stderr"));
}

#[test]
#[ignore = "runner real-process lane: disposable plugin checks clean up failed candidate process trees"]
fn runner_real_process_plugin_check_failures_are_structured_diagnostic_results_and_cleanup_process_trees(
) {
    for (scenario, timeout_secs, phase, code) in [
        (
            "check_bad_version_tree",
            2,
            PluginCheckPhase::Initialize,
            "plugin_protocol_version_mismatch",
        ),
        (
            "check_init_timeout",
            1,
            PluginCheckPhase::Initialize,
            "plugin_timeout",
        ),
        (
            "check_init_crash",
            2,
            PluginCheckPhase::Initialize,
            "plugin_eof",
        ),
        (
            "check_invalid_tools",
            2,
            PluginCheckPhase::Validation,
            "plugin_tools_list_invalid",
        ),
        (
            "check_oversized_schema",
            2,
            PluginCheckPhase::Validation,
            "plugin_tools_list_invalid",
        ),
        (
            "check_unsupported_schema",
            2,
            PluginCheckPhase::Validation,
            "plugin_tools_list_invalid",
        ),
    ] {
        let fixture = CheckFixture::new(scenario, timeout_secs);
        let report = checked_report(fixture.check());
        assert!(!report.ready, "{scenario}");
        assert_eq!(report.phase, phase, "{scenario}");
        assert_eq!(report.code.as_deref(), Some(code), "{scenario}");
        assert!(report.detail.is_some(), "{scenario}");
        assert!(report.tools.is_empty(), "{scenario}");

        for pid in fixture.marker_pids("candidate-pid:") {
            assert!(wait_until(Duration::from_secs(2), || {
                !crate::job_manager_tests::process_running(pid)
            }));
        }
        for pid in fixture.marker_pids("descendant-pid:") {
            assert!(wait_until(Duration::from_secs(2), || {
                !crate::job_manager_tests::process_running(pid)
            }));
        }
    }
}

#[test]
fn check_tool_validation_failures_have_safe_actionable_diagnostics() {
    for (scenario, code, tool, field) in [
        (
            "check_malformed_tools_list",
            "tools_list_result_malformed",
            None,
            None,
        ),
        (
            "check_duplicate_tools",
            "duplicate_tool_name",
            Some("echo"),
            Some("name"),
        ),
        (
            "check_invalid_tool_name",
            "invalid_tool_name",
            None,
            Some("name"),
        ),
        (
            "check_invalid_tools",
            "input_schema_invalid",
            Some("echo"),
            Some("inputSchema"),
        ),
        (
            "check_oversized_schema",
            "schema_bounds_exceeded",
            Some("echo"),
            Some("inputSchema"),
        ),
        (
            "check_unsupported_schema",
            "schema_keyword_unsupported",
            Some("echo"),
            Some("inputSchema"),
        ),
    ] {
        let fixture = CheckFixture::new(scenario, 2);
        let report = checked_report(fixture.check());
        assert!(!report.ready, "{scenario}");
        assert_eq!(report.phase, PluginCheckPhase::Validation, "{scenario}");
        assert_eq!(
            report.code.as_deref(),
            Some("plugin_tools_list_invalid"),
            "{scenario}"
        );
        let diagnostic = report.diagnostic.as_ref().expect("validation diagnostic");
        assert_eq!(diagnostic.code, code, "{scenario}");
        assert_eq!(diagnostic.tool.as_deref(), tool, "{scenario}");
        assert_eq!(diagnostic.field.as_deref(), field, "{scenario}");

        let encoded = serde_json::to_string(&report).unwrap();
        for forbidden in [
            "command",
            "argv",
            "\"cwd\"",
            "\"env\"",
            "stderr",
            "PID",
            "runner_instance_id",
            "provider_instance_id",
            "\"properties\"",
            "\"type\":\"object\"",
            "example.invalid",
            "$ref",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "{scenario} diagnostic leaked {forbidden}: {encoded}"
            );
        }
        for pid in fixture.marker_pids("candidate-pid:") {
            assert!(wait_until(Duration::from_secs(2), || {
                !crate::job_manager_tests::process_running(pid)
            }));
        }
    }
}

#[test]
fn check_reports_executable_config_and_large_catalog_without_sensitive_details() {
    let fixture = CheckFixture::new("normal", 2);
    write_runner_toml(
        &fixture.config_path,
        fixture._temp.path(),
        Path::new("webcodex-plugin-definitely-missing-for-check"),
        &fixture.marker,
        "normal",
        2,
    );
    let missing = checked_report(fixture.check());
    assert!(!missing.ready);
    assert_eq!(missing.phase, PluginCheckPhase::Executable);
    assert_eq!(
        missing.code.as_deref(),
        Some("plugin_executable_unavailable")
    );

    write_runner_toml_without_plugins(&fixture.config_path, fixture._temp.path());
    let absent = checked_report(fixture.check());
    assert_eq!(absent.phase, PluginCheckPhase::Config);
    assert_eq!(absent.code.as_deref(), Some("plugin_not_configured"));

    fixture.rewrite_candidate("check_startup_large_schema", 2);
    let shape = checked_report(fixture.check());
    assert!(shape.ready);
    assert_eq!(shape.tool_count, 1);
    assert_eq!(shape.tools[0].name, "echo");

    fixture.rewrite_candidate("stderr", 2);
    let response = fixture.check();
    let encoded = serde_json::to_string(&response).unwrap();
    assert!(!encoded.contains("diagnostic-only-secret-looking-stderr"));
    assert!(!encoded.contains(fixture._temp.path().to_string_lossy().as_ref()));
    let local = fixture
        .manager
        .local_check_stderr_diagnostics("fake")
        .expect("local check stderr projection");
    assert!(local
        .lines
        .iter()
        .any(|line| line.text == "diagnostic-only-secret-looking-stderr"));
    let report = checked_report(response);
    assert!(report.ready);

    fixture.rewrite_candidate("check_stderr_at_list", 2);
    let late_response = fixture.check();
    let late_encoded = serde_json::to_string(&late_response).unwrap();
    assert!(!late_encoded.contains("diagnostic-written-before-list-response"));
    let late_local = fixture
        .manager
        .local_check_stderr_diagnostics("fake")
        .expect("completed check stderr projection");
    assert!(late_local
        .lines
        .iter()
        .any(|line| line.text == "diagnostic-written-before-list-response"));
    let late_report = checked_report(late_response);
    assert!(late_report.ready);
}

#[test]
fn candidate_gate_serializes_check_and_reload_without_blocking_current_providers() {
    let fixture = CheckFixture::new("normal", 2);
    let current_v1 = provider_state(&fixture.manager).remove(0);
    let dynamic_v1 = current_v1.provider_instance_id.clone();
    let schema = provider_tools(&fixture.manager, &current_v1)[0].schema_observation();

    fixture.rewrite_candidate("candidate_block_list_tree", 10);
    let _ = fs::remove_file(fixture.marker.with_extension("release"));
    let manager = Arc::clone(&fixture.manager);
    let check_a = std::thread::spawn(move || {
        manager.handle(PluginGatewayRequest::Check {
            provider_id: "fake".to_string(),
        })
    });
    assert!(wait_until(Duration::from_secs(2), || {
        fixture.marker_count("candidate-blocked") == 1
    }));

    let check_b = fixture.check();
    assert_eq!(check_b.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(check_b.error.as_ref().unwrap().code, "plugin_check_busy");
    let reload = fixture.manager.handle(PluginGatewayRequest::Reload);
    assert_eq!(reload.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(reload.error.as_ref().unwrap().code, "plugin_reload_busy");

    let current_call = fixture.manager.handle(PluginGatewayRequest::ToolsCall {
        provider_id: "fake".to_string(),
        provider_instance_id: dynamic_v1.clone(),
        name: "echo".to_string(),
        arguments: json!({"value":"during-check"}),
        expected_schema: schema.clone(),
    });
    assert!(current_call.error.is_none());
    assert!(fixture
        .manager
        .handle(PluginGatewayRequest::ProvidersList)
        .error
        .is_none());

    fs::write(fixture.marker.with_extension("release"), b"release").unwrap();
    let report = checked_report(check_a.join().unwrap());
    assert!(report.ready);
    let providers = provider_state(&fixture.manager);
    assert_eq!(providers[0].provider_instance_id, dynamic_v1);
    for pid in fixture.marker_pids("descendant-pid:") {
        assert!(wait_until(Duration::from_secs(2), || {
            !crate::job_manager_tests::process_running(pid)
        }));
    }
    fixture.manager.shutdown();
}

#[test]
#[ignore = "runner real-process lane: plugin shutdown interrupts blocked candidate and reaps its tree"]
fn runner_real_process_plugin_shutdown_interrupts_blocked_check_candidate_and_reaps_tree() {
    let fixture = CheckFixture::new("candidate_block_list_tree", 10);
    let _ = fs::remove_file(fixture.marker.with_extension("release"));
    let manager = Arc::clone(&fixture.manager);
    let check = std::thread::spawn(move || {
        manager.handle(PluginGatewayRequest::Check {
            provider_id: "fake".to_string(),
        })
    });
    assert!(wait_until(Duration::from_secs(2), || {
        fixture.marker_count("candidate-blocked") == 1
            && !fixture.marker_pids("descendant-pid:").is_empty()
    }));

    let started = Instant::now();
    fixture.manager.shutdown();
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "shutdown waited for the candidate gate"
    );
    let response = check.join().unwrap();
    assert_eq!(response.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "plugin_manager_stopping"
    );
    for pid in fixture.marker_pids("descendant-pid:") {
        assert!(wait_until(Duration::from_secs(2), || {
            !crate::job_manager_tests::process_running(pid)
        }));
    }
}

fn runner_config(
    plugins: PluginConfig,
    shell: ShellConfig,
    project_registry_dir: &Path,
) -> RunnerConfig {
    use super::super::config::{
        default_websocket_connect_timeout_secs, AcpConfig, McpGatewayConfig, RunnerPolicy,
        SkillsConfig, SshConfig, ToolProvidersConfig,
    };
    RunnerConfig {
        server_url: "http://127.0.0.1:8000".to_string(),
        token: "test-token".to_string(),
        client_id: "plugin-check-test".to_string(),
        display_name: None,
        owner: Some("alice".to_string()),
        hostname: None,
        host_context: None,
        project_registry_dir: Some(project_registry_dir.to_path_buf()),
        legacy_projects_dir: None,
        poll_interval_ms: 1000,
        capabilities: None,
        max_concurrent_jobs: None,
        policy: RunnerPolicy::default(),
        transport: None,
        websocket_connect_timeout_secs: default_websocket_connect_timeout_secs(),
        quic: None,
        shell,
        skills: SkillsConfig::default(),
        ssh: SshConfig::default(),
        tool_providers: ToolProvidersConfig::default(),
        mcp_gateway: McpGatewayConfig::default(),
        plugins,
        acp: AcpConfig::default(),
    }
}

fn write_runner_toml(
    path: &Path,
    project_registry_dir: &Path,
    executable: &Path,
    marker: &Path,
    scenario: &str,
    request_timeout_secs: u64,
) {
    fn quoted(value: &Path) -> String {
        format!("{:?}", value.to_string_lossy().as_ref())
    }
    fs::write(
        path,
        format!(
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"test-token\"\nclient_id = \"plugin-check-test\"\nproject_registry_dir = {}\n\n[plugins]\nrequest_timeout_secs = {request_timeout_secs}\n\n[[plugins.providers]]\nid = \"fake\"\nname = \"Fake Plugin\"\ncommand = {}\nargs = [\"{}\", {}]\ncwd = {}\ntimeout_secs = {request_timeout_secs}\n",
            quoted(project_registry_dir),
            quoted(executable),
            scenario,
            quoted(marker),
            quoted(project_registry_dir),
        ),
    )
    .unwrap();
}

fn write_runner_toml_without_plugins(path: &Path, project_registry_dir: &Path) {
    fn quoted(value: &Path) -> String {
        format!("{:?}", value.to_string_lossy().as_ref())
    }
    fs::write(
        path,
        format!(
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"test-token\"\nclient_id = \"plugin-check-test\"\nproject_registry_dir = {}\n\n[plugins]\nrequest_timeout_secs = 2\n",
            quoted(project_registry_dir),
        ),
    )
    .unwrap();
}
