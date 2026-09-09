use super::*;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use tempfile::TempDir;
use webcodex_core::plugin::{
    PluginContent, PluginGatewayResponsePayload, PluginProviderView, PluginSchemaObservation,
    PLUGIN_MAX_ARGUMENT_BYTES,
};

static FAKE_PLUGIN: OnceLock<Mutex<Weak<FakeBinary>>> = OnceLock::new();

struct FakeBinary {
    _temp: TempDir,
    path: PathBuf,
}

fn fake_binary() -> Arc<FakeBinary> {
    let cache = FAKE_PLUGIN.get_or_init(|| Mutex::new(Weak::new()));
    let mut cached = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(binary) = cached.upgrade() {
        return binary;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = temp
        .path()
        .join(format!("webcodex-plugin-fake{}", env::consts::EXE_SUFFIX));
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/webcodex_runner/fake_plugin.rs");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let result = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-name=webcodex_plugin_fake")
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

struct Fixture {
    manager: Arc<PluginManager>,
    marker: PathBuf,
    provider: PluginProviderView,
    schema: Option<PluginSchemaObservation>,
    _fake: Arc<FakeBinary>,
    _temp: TempDir,
}

impl Fixture {
    fn new(scenario: &str, timeout_secs: u64) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("marker.log");
        let fake = fake_binary();
        let provider = PluginProviderConfig {
            id: "fake".to_string(),
            name: "Fake Plugin".to_string(),
            command: fake.path.to_string_lossy().into_owned(),
            args: vec![scenario.to_string(), marker.to_string_lossy().into_owned()],
            cwd: Some(temp.path().to_string_lossy().into_owned()),
            profile: None,
            timeout_secs: Some(timeout_secs),
        };
        let config = runner_config(
            PluginConfig {
                request_timeout_secs: timeout_secs,
                providers: vec![provider],
            },
            ShellConfig::default(),
            temp.path(),
        );
        let manager = Arc::new(PluginManager::new(&config, temp.path().join("runner.toml")));
        let provider = current_providers(&manager).into_iter().next().unwrap();
        let schema = (provider.status == "ready")
            .then(|| current_tools(&manager, &provider)[0].schema_observation());
        Self {
            manager,
            marker,
            provider,
            schema,
            _fake: fake,
            _temp: temp,
        }
    }

    fn list(&self) -> PluginGatewayResponse {
        self.manager.handle(PluginGatewayRequest::ToolsList {
            provider_id: self.provider.provider_id.clone(),
            provider_instance_id: self.provider.provider_instance_id.clone(),
        })
    }

    fn call(&self) -> PluginGatewayResponse {
        self.call_with_arguments(json!({"value":"hello"}))
    }

    fn call_with_arguments(&self, arguments: Value) -> PluginGatewayResponse {
        self.manager.handle(PluginGatewayRequest::ToolsCall {
            provider_id: self.provider.provider_id.clone(),
            provider_instance_id: self.provider.provider_instance_id.clone(),
            name: "echo".to_string(),
            arguments,
            expected_schema: self.schema.clone().expect("ready fixture schema"),
        })
    }

    fn marker_count(&self, value: &str) -> usize {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == value)
            .count()
    }

    fn marker_pid(&self, prefix: &str) -> Option<u32> {
        fs::read_to_string(&self.marker)
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix(prefix)?.parse().ok())
    }
}

fn current_providers(manager: &PluginManager) -> Vec<PluginProviderView> {
    let response = manager.handle(PluginGatewayRequest::ProvidersList);
    let Some(PluginGatewayResponsePayload::Providers { providers }) = response.payload else {
        panic!("missing providers: {:?}", response.error);
    };
    providers
}

fn current_tools(manager: &PluginManager, provider: &PluginProviderView) -> Vec<PluginTool> {
    let response = manager.handle(PluginGatewayRequest::ToolsList {
        provider_id: provider.provider_id.clone(),
        provider_instance_id: provider.provider_instance_id.clone(),
    });
    let Some(PluginGatewayResponsePayload::Tools { tools }) = response.payload else {
        panic!("missing tools: {:?}", response.error);
    };
    tools
}

fn maximum_bounded_arguments() -> Value {
    let empty = json!({"value":""});
    let overhead = serde_json::to_vec(&empty).unwrap().len();
    let arguments = json!({"value":"x".repeat(PLUGIN_MAX_ARGUMENT_BYTES - overhead)});
    assert_eq!(
        serde_json::to_vec(&arguments).unwrap().len(),
        PLUGIN_MAX_ARGUMENT_BYTES
    );
    arguments
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

fn runner_config(
    plugins: PluginConfig,
    shell: ShellConfig,
    project_registry_dir: &Path,
) -> RunnerConfig {
    use super::super::config::{
        default_websocket_connect_timeout_secs, AcpConfig, McpGatewayConfig, RunnerPolicy,
        SshConfig, ToolProvidersConfig,
    };
    RunnerConfig {
        server_url: "http://127.0.0.1:8000".to_string(),
        token: "test-token".to_string(),
        client_id: "plugin-test".to_string(),
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
        ssh: SshConfig::default(),
        tool_providers: ToolProvidersConfig::default(),
        mcp_gateway: McpGatewayConfig::default(),
        plugins,
        acp: AcpConfig::default(),
    }
}

#[test]
fn initial_committed_provider_is_eager_persistent_and_reused() {
    let fixture = Fixture::new("normal", 2);
    assert_eq!(fixture.marker_count("start"), 1);
    assert_eq!(fixture.marker_count("initialize"), 1);
    assert_eq!(
        fixture.marker_count("list"),
        1,
        "initial provider admission must list eagerly"
    );
    assert_eq!(fixture.provider.status, "ready");
    let tools = current_tools(&fixture.manager, &fixture.provider);
    assert_eq!(tools.len(), 1);

    assert!(fixture.list().error.is_none());
    for expected in ["call-1", "call-2"] {
        let response = fixture.call();
        let Some(PluginGatewayResponsePayload::ToolResult { result }) = response.payload else {
            panic!("missing result: {:?}", response.error);
        };
        assert_eq!(
            result.content,
            vec![PluginContent::Text {
                text: expected.to_string()
            }]
        );
    }
    assert_eq!(
        fixture.marker_count("start"),
        1,
        "provider must never silently respawn"
    );
    assert_eq!(fixture.marker_count("call"), 2);
    assert_eq!(
        fixture.marker_count("list"),
        1,
        "tools/list is an admission operation and must not repeat during discovery or calls"
    );
}

#[test]
fn bad_version_and_invalid_initial_provider_do_not_block_manager() {
    for (scenario, code) in [
        ("bad_version", "plugin_protocol_version_mismatch"),
        ("invalid_tools", "plugin_tools_list_invalid"),
        ("malformed", "plugin_malformed_json"),
        ("oversized_message", "plugin_message_too_large"),
    ] {
        let fixture = Fixture::new(scenario, 2);
        assert_eq!(fixture.provider.status, "failed", "{scenario}");
        assert_eq!(
            fixture.provider.error_code.as_deref(),
            Some(code),
            "{scenario}"
        );
    }
}

#[test]
fn provider_catalog_is_not_limited_by_removed_first_class_schema_bound() {
    let fixture = Fixture::new("check_startup_large_schema", 2);
    assert_eq!(fixture.provider.status, "ready");
    assert_eq!(fixture.provider.error_code, None);
    assert_eq!(current_tools(&fixture.manager, &fixture.provider).len(), 1);
}

#[test]
fn same_provider_instance_catalog_is_frozen_and_never_relists() {
    let fixture = Fixture::new("schema_change", 2);
    let response = fixture.call();
    assert!(response.error.is_none());
    assert_eq!(fixture.marker_count("call"), 1);
    assert_eq!(fixture.marker_count("list"), 1);
    let listed = fixture.list();
    let Some(PluginGatewayResponsePayload::Tools { tools }) = listed.payload else {
        panic!("missing frozen tools: {:?}", listed.error);
    };
    assert_eq!(
        tools[0].input_schema["properties"]["value"]["type"],
        "string"
    );
    assert_eq!(fixture.marker_count("list"), 1);
}

#[test]
fn tools_call_uses_full_timeout_without_catalog_relist() {
    let fixture = Fixture::new("split_timeout", 1);
    let response = fixture.call();
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(fixture.marker_count("call"), 1);
    assert_eq!(fixture.marker_count("list"), 1);
}

#[test]
fn invalid_input_schema_is_not_started_and_provider_sees_no_call() {
    let fixture = Fixture::new("normal", 2);
    let response = fixture.call_with_arguments(json!({"value": 7}));
    assert_eq!(response.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "plugin_arguments_schema_invalid"
    );
    assert_eq!(fixture.marker_count("call"), 0);
    assert_eq!(fixture.marker_count("list"), 1);

    let valid = fixture.call_with_arguments(json!({"value": "valid"}));
    assert!(valid.error.is_none());
    assert_eq!(fixture.marker_count("call"), 1);
}

#[test]
fn output_schema_violation_is_completed_and_retires_provider() {
    let fixture = Fixture::new("output_schema_invalid", 2);
    let response = fixture.call();
    assert_eq!(response.dispatch_state, PluginDispatchState::Completed);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "plugin_output_schema_violation"
    );
    assert_eq!(fixture.marker_count("call"), 1);
    let retired = fixture.call();
    assert_eq!(retired.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        retired.error.as_ref().unwrap().code,
        "plugin_provider_unavailable"
    );
}

#[test]
#[ignore = "runner real-process lane: plugin blocked-stdin deadline and provider-tree retirement"]
fn runner_real_process_plugin_blocking_stdin_write_respects_total_deadline_and_retires_provider_tree(
) {
    let fixture = Fixture::new("block_after_preflight_tree", 1);
    let started = Instant::now();
    let response = fixture.call_with_arguments(maximum_bounded_arguments());
    let elapsed = started.elapsed();

    assert_eq!(response.dispatch_state, PluginDispatchState::OutcomeUnknown);
    assert_eq!(response.error.as_ref().unwrap().code, "plugin_timeout");
    assert!(
        elapsed < Duration::from_millis(2500),
        "blocked stdin write exceeded the provider deadline plus termination slack: {elapsed:?}"
    );
    assert_eq!(fixture.marker_count("stdin-blocked"), 1);
    assert_eq!(fixture.marker_count("call"), 0);

    let descendant_pid = fixture
        .marker_pid("descendant-pid:")
        .expect("fixture descendant pid");
    assert!(wait_until(Duration::from_secs(1), || {
        !crate::job_manager_tests::process_running(descendant_pid)
    }));

    let retired = fixture.call();
    assert_eq!(retired.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        retired.error.as_ref().unwrap().code,
        "plugin_provider_unavailable"
    );

    let shutdown_started = Instant::now();
    fixture.manager.shutdown();
    assert!(
        shutdown_started.elapsed() < Duration::from_millis(1500),
        "shutdown of an already-retired blocked writer must remain bounded"
    );
}

#[test]
#[ignore = "runner real-process lane: plugin shutdown terminates blocked provider process tree"]
fn runner_real_process_plugin_shutdown_terminates_process_tree_while_effectful_stdin_write_is_blocked(
) {
    let fixture = Fixture::new("block_after_preflight_tree", 10);
    let manager = Arc::clone(&fixture.manager);
    let provider = fixture.provider.clone();
    let schema = fixture.schema.clone().unwrap();
    let arguments = maximum_bounded_arguments();
    let (sender, receiver) = mpsc::channel();
    let request = std::thread::spawn(move || {
        let response = manager.handle(PluginGatewayRequest::ToolsCall {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
            name: "echo".to_string(),
            arguments,
            expected_schema: schema,
        });
        let _ = sender.send(response);
    });

    assert!(wait_until(Duration::from_secs(2), || {
        fixture.marker_count("stdin-blocked") == 1
    }));
    assert!(wait_until(Duration::from_secs(2), || {
        fixture.marker_pid("descendant-pid:").is_some()
    }));
    // Give the writer worker an opportunity to enter OS pipe backpressure after
    // the preflight response has been delivered.
    std::thread::sleep(Duration::from_millis(50));

    let shutdown_started = Instant::now();
    fixture.manager.shutdown();
    let shutdown_elapsed = shutdown_started.elapsed();
    assert!(
        shutdown_elapsed < Duration::from_millis(2500),
        "shutdown waited on the request/session lock: {shutdown_elapsed:?}"
    );

    let response = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("blocked request must be released by process-tree termination");
    request.join().unwrap();
    assert_eq!(response.dispatch_state, PluginDispatchState::OutcomeUnknown);
    assert!(matches!(
        response.error.as_ref().map(|error| error.code.as_str()),
        Some("plugin_manager_stopping") | Some("plugin_stdin_failed") | Some("plugin_eof")
    ));
    let descendant_pid = fixture.marker_pid("descendant-pid:").unwrap();
    assert!(wait_until(Duration::from_secs(1), || {
        !crate::job_manager_tests::process_running(descendant_pid)
    }));
}

#[test]
fn crash_after_effect_send_is_outcome_unknown_and_instance_is_retired() {
    let fixture = Fixture::new("crash", 2);
    let first = fixture.call();
    assert_eq!(first.dispatch_state, PluginDispatchState::OutcomeUnknown);
    assert_eq!(first.error.as_ref().unwrap().code, "plugin_eof");
    let second = fixture.call();
    assert_eq!(second.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        second.error.as_ref().unwrap().code,
        "plugin_provider_unavailable"
    );
    assert_eq!(fixture.marker_count("start"), 1);
    assert_eq!(fixture.marker_count("call"), 1);
}

#[test]
fn unsupported_result_is_completed_and_retires_protocol_broken_instance() {
    let fixture = Fixture::new("bad_result", 2);
    let response = fixture.call();
    assert_eq!(response.dispatch_state, PluginDispatchState::Completed);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "plugin_result_invalid"
    );
    assert_eq!(
        fixture.list().error.as_ref().unwrap().code,
        "plugin_provider_unavailable"
    );
}

#[test]
fn stderr_is_diagnostic_only_and_never_enters_catalog_or_result() {
    let fixture = Fixture::new("stderr", 2);
    assert!(wait_until(Duration::from_secs(1), || {
        fixture
            .manager
            .local_stderr_diagnostics(
                &fixture.provider.provider_id,
                &fixture.provider.provider_instance_id,
            )
            .is_some_and(|snapshot| {
                snapshot
                    .lines
                    .iter()
                    .any(|line| line.text == "diagnostic-only-secret-looking-stderr")
            })
    }));
    let catalog = serde_json::to_string(&fixture.list()).unwrap();
    assert!(!catalog.contains("diagnostic-only-secret-looking-stderr"));
    let result = serde_json::to_string(&fixture.call()).unwrap();
    assert!(!result.contains("diagnostic-only-secret-looking-stderr"));
}

#[test]
fn stderr_flood_is_bounded_and_does_not_block_stdout_protocol() {
    let fixture = Fixture::new("stderr_flood", 2);
    let response = fixture.call();
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(fixture.marker_count("list"), 1);
    assert_eq!(fixture.marker_count("call"), 1);
    assert!(wait_until(Duration::from_secs(1), || {
        fixture
            .manager
            .local_stderr_diagnostics(
                &fixture.provider.provider_id,
                &fixture.provider.provider_instance_id,
            )
            .is_some_and(|snapshot| !snapshot.lines.is_empty())
    }));
    let snapshot = fixture
        .manager
        .local_stderr_diagnostics(
            &fixture.provider.provider_id,
            &fixture.provider.provider_instance_id,
        )
        .unwrap();
    assert!(snapshot.lines.len() <= PLUGIN_STDERR_MAX_LINES);
    assert!(snapshot.aggregate_bytes <= PLUGIN_STDERR_MAX_BYTES);
    assert!(snapshot
        .lines
        .iter()
        .all(|line| line.text.len() <= PLUGIN_STDERR_MAX_LINE_BYTES && line.truncated));
    let catalog = serde_json::to_string(&fixture.list()).unwrap();
    assert!(!catalog.contains("stderr-flood"));
}

#[test]
fn provider_busy_is_not_started() {
    let fixture = Fixture::new("slow", 2);
    let manager = Arc::clone(&fixture.manager);
    let provider = fixture.provider.clone();
    let schema = fixture.schema.clone().unwrap();
    let first = std::thread::spawn(move || {
        manager.handle(PluginGatewayRequest::ToolsCall {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
            name: "echo".to_string(),
            arguments: json!({"value":"first"}),
            expected_schema: schema,
        })
    });
    assert!(wait_until(Duration::from_secs(1), || {
        fixture.marker_count("call") == 1
    }));
    let discovery = fixture.list();
    assert!(
        discovery.error.is_none(),
        "frozen catalog discovery must not contend on the live provider connection"
    );
    let second_call = fixture.call();
    assert_eq!(second_call.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        second_call.error.as_ref().unwrap().code,
        "plugin_provider_busy"
    );
    assert!(first.join().unwrap().error.is_none());
}

#[test]
fn prepared_environment_reuses_shell_env_default_profile_and_clears_sensitive_values() {
    use super::super::config::ShellProfileConfig;
    use std::collections::BTreeMap;
    let _guard = crate::tests::test_env_lock();
    let _env = crate::tests::EnvGuard::new().set("WEBCODEX_AGENT_TOKEN", "must-not-leak");
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker.log");
    let fake = fake_binary();
    let mut shell = ShellConfig::default();
    shell
        .env
        .insert("WEBCODEX_PLUGIN_TEST_ENV".to_string(), "base".to_string());
    shell.default_profile = Some("plugin".to_string());
    shell.profiles = BTreeMap::from([(
        "plugin".to_string(),
        ShellProfileConfig {
            env: BTreeMap::from([(
                "WEBCODEX_PLUGIN_TEST_ENV".to_string(),
                "profile-ready".to_string(),
            )]),
            ..ShellProfileConfig::default()
        },
    )]);
    let plugins = PluginConfig {
        request_timeout_secs: 2,
        providers: vec![PluginProviderConfig {
            id: "fake".to_string(),
            name: "Fake Plugin".to_string(),
            command: fake.path.to_string_lossy().into_owned(),
            args: vec![
                "execution_context".to_string(),
                marker.to_string_lossy().into_owned(),
                temp.path().to_string_lossy().into_owned(),
            ],
            cwd: Some(temp.path().to_string_lossy().into_owned()),
            profile: None,
            timeout_secs: None,
        }],
    };
    let config = runner_config(plugins, shell, temp.path());
    let manager = PluginManager::new(&config, temp.path().join("runner.toml"));
    assert_eq!(current_providers(&manager)[0].status, "ready");
    let markers = fs::read_to_string(marker).unwrap();
    assert!(markers.contains("profile-env-ok"));
    assert!(markers.contains("sensitive-env-cleared"));
    assert!(markers.contains("cwd-ok"));
}

#[test]
fn bare_plugin_command_resolves_from_prepared_path_with_explicit_profile() {
    use super::super::config::ShellProfileConfig;
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake = fake_binary();
    let command_name = format!("wc-plugin-fixture{}", env::consts::EXE_SUFFIX);
    let installed = bin.join(&command_name);
    fs::copy(&fake.path, &installed).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&installed).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&installed, permissions).unwrap();
    }
    let marker = temp.path().join("marker.log");
    let mut shell = ShellConfig::default();
    shell.path_prepend.push(bin);
    shell.profiles.insert(
        "native".to_string(),
        ShellProfileConfig {
            env: BTreeMap::from([(
                "WEBCODEX_PLUGIN_TEST_ENV".to_string(),
                "profile-ready".to_string(),
            )]),
            ..ShellProfileConfig::default()
        },
    );
    let plugins = PluginConfig {
        // This regression exercises prepared-PATH resolution, not timeout
        // behavior. Keep provider startup tolerant of a heavily loaded CI host.
        request_timeout_secs: 10,
        providers: vec![PluginProviderConfig {
            id: "fake".to_string(),
            name: "Fake Plugin".to_string(),
            command: command_name,
            args: vec!["normal".to_string(), marker.to_string_lossy().into_owned()],
            cwd: Some(temp.path().to_string_lossy().into_owned()),
            profile: Some("native".to_string()),
            timeout_secs: None,
        }],
    };
    let config = runner_config(plugins, shell, temp.path());
    let manager = PluginManager::new(&config, temp.path().join("runner.toml"));
    let provider = current_providers(&manager).remove(0);
    assert_eq!(provider.status, "ready", "{provider:?}");
    assert_eq!(
        fs::read_to_string(marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "start")
            .count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn plugin_profile_init_script_is_captured_into_native_child_environment() {
    use super::super::config::ShellProfileConfig;
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker.log");
    let fake = fake_binary();
    let mut shell = ShellConfig::default();
    shell.profiles.insert(
        "native".to_string(),
        ShellProfileConfig {
            init_script: Some("export WEBCODEX_PLUGIN_TEST_ENV=profile-ready".to_string()),
            ..ShellProfileConfig::default()
        },
    );
    let plugins = PluginConfig {
        request_timeout_secs: 2,
        providers: vec![PluginProviderConfig {
            id: "fake".to_string(),
            name: "Fake Plugin".to_string(),
            command: fake.path.to_string_lossy().into_owned(),
            args: vec![
                "execution_context".to_string(),
                marker.to_string_lossy().into_owned(),
                temp.path().to_string_lossy().into_owned(),
            ],
            cwd: Some(temp.path().to_string_lossy().into_owned()),
            profile: Some("native".to_string()),
            timeout_secs: None,
        }],
    };
    let config = runner_config(plugins, shell, temp.path());
    let manager = PluginManager::new(&config, temp.path().join("runner.toml"));
    assert_eq!(current_providers(&manager)[0].status, "ready");
    let markers = fs::read_to_string(marker).unwrap();
    assert!(markers.contains("profile-env-ok"));
}

#[test]
fn reload_replaces_committed_provider_and_invalidates_old_instance() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker.log");
    let fake = fake_binary();
    let plugins = PluginConfig {
        request_timeout_secs: 2,
        providers: vec![PluginProviderConfig {
            id: "fake".to_string(),
            name: "Fake Plugin".to_string(),
            command: fake.path.to_string_lossy().into_owned(),
            args: vec![
                "schema_change".to_string(),
                marker.to_string_lossy().into_owned(),
            ],
            cwd: Some(temp.path().to_string_lossy().into_owned()),
            profile: None,
            timeout_secs: None,
        }],
    };
    let config_path = temp.path().join("runner.toml");
    write_runner_toml(&config_path, temp.path(), &fake.path, &marker, "normal");
    let config = runner_config(plugins, ShellConfig::default(), temp.path());
    let manager = PluginManager::new(&config, config_path);
    let before = current_providers(&manager).remove(0);
    let old_instance = before.provider_instance_id.clone();
    let reloaded = manager.handle(PluginGatewayRequest::Reload);
    let Some(PluginGatewayResponsePayload::Reloaded {
        providers,
        failures,
    }) = reloaded.payload
    else {
        panic!("reload failed: {:?}", reloaded.error);
    };
    assert!(failures.is_empty());
    let new_instance = providers[0].provider_instance_id.clone();
    assert_ne!(new_instance, old_instance);
    let stale = manager.handle(PluginGatewayRequest::ToolsList {
        provider_id: "fake".to_string(),
        provider_instance_id: old_instance,
    });
    assert_eq!(stale.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(stale.error.as_ref().unwrap().code, "stale_plugin_provider");
    let current = manager.handle(PluginGatewayRequest::ToolsList {
        provider_id: "fake".to_string(),
        provider_instance_id: new_instance,
    });
    assert!(current.error.is_none());
}

#[test]
fn explicit_reload_restarts_provider_when_config_is_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker.log");
    let fake = fake_binary();
    let config_path = temp.path().join("runner.toml");
    write_runner_toml(&config_path, temp.path(), &fake.path, &marker, "normal");
    let config = super::super::config::load_config(&config_path).unwrap();
    let manager = PluginManager::new(&config, config_path);
    let old_instance = current_providers(&manager)[0].provider_instance_id.clone();

    // The executable/script may have changed on disk even though runner.toml
    // did not. An explicit plugin_tool reload must therefore re-instantiate it.
    let reloaded = manager.handle(PluginGatewayRequest::Reload);
    let Some(PluginGatewayResponsePayload::Reloaded {
        providers,
        failures,
    }) = reloaded.payload
    else {
        panic!("reload failed: {:?}", reloaded.error);
    };
    assert!(failures.is_empty());
    assert_ne!(providers[0].provider_instance_id, old_instance);
}

#[test]
fn concurrent_reload_is_busy_while_existing_calls_continue_and_later_reload_wins() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker.log");
    let release = marker.with_extension("release");
    let fake = fake_binary();
    let plugins = PluginConfig {
        request_timeout_secs: 2,
        providers: vec![PluginProviderConfig {
            id: "fake".to_string(),
            name: "Fake Plugin".to_string(),
            command: fake.path.to_string_lossy().into_owned(),
            args: vec!["normal".to_string(), marker.to_string_lossy().into_owned()],
            cwd: Some(temp.path().to_string_lossy().into_owned()),
            profile: None,
            timeout_secs: None,
        }],
    };
    let config_path = temp.path().join("runner.toml");
    write_runner_toml(&config_path, temp.path(), &fake.path, &marker, "normal");
    let config = runner_config(plugins, ShellConfig::default(), temp.path());
    let manager = Arc::new(PluginManager::new(&config, config_path.clone()));
    let current_v1 = current_providers(&manager).remove(0);
    let dynamic_v1 = current_v1.provider_instance_id.clone();
    let expected_schema = current_tools(&manager, &current_v1)[0].schema_observation();

    let _ = fs::remove_file(&release);
    write_runner_toml_with_timeout(
        &config_path,
        temp.path(),
        &fake.path,
        &marker,
        "reload_block_list",
        10,
    );
    let reload_manager = Arc::clone(&manager);
    let reload_a = std::thread::spawn(move || reload_manager.handle(PluginGatewayRequest::Reload));
    assert!(wait_until(Duration::from_secs(2), || {
        fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .any(|line| line == "reload-blocked")
    }));

    // A has already read the old config and is blocked preparing its candidate.
    // Publish a different config while A owns the reload gate.
    write_runner_toml_with_timeout(
        &config_path,
        temp.path(),
        &fake.path,
        &marker,
        "reload_new",
        2,
    );
    let busy_started = Instant::now();
    let busy = manager.handle(PluginGatewayRequest::Reload);
    assert_eq!(busy.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(busy.error.as_ref().unwrap().code, "plugin_reload_busy");
    assert!(
        busy_started.elapsed() < Duration::from_secs(1),
        "a second reload must fail busy instead of waiting for candidate preparation"
    );

    let current = manager.handle(PluginGatewayRequest::ProvidersList);
    let Some(PluginGatewayResponsePayload::Providers { providers }) = current.payload else {
        panic!("current dynamic provider list failed: {:?}", current.error);
    };
    assert_eq!(providers[0].provider_instance_id, dynamic_v1);
    let call_while_reloading = manager.handle(PluginGatewayRequest::ToolsCall {
        provider_id: "fake".to_string(),
        provider_instance_id: dynamic_v1.clone(),
        name: "echo".to_string(),
        arguments: json!({"value":"during-reload"}),
        expected_schema: expected_schema.clone(),
    });
    assert!(
        call_while_reloading.error.is_none(),
        "candidate preparation must not hold the committed-state lock or block the current provider"
    );

    fs::write(&release, b"release").unwrap();
    let completed_a = reload_a.join().unwrap();
    let Some(PluginGatewayResponsePayload::Reloaded { providers, .. }) = completed_a.payload else {
        panic!("reload A failed after release: {:?}", completed_a.error);
    };
    let dynamic_a = providers[0].provider_instance_id.clone();
    assert_ne!(dynamic_a, dynamic_v1);
    assert_eq!(
        fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "reload-new-start")
            .count(),
        0,
        "busy reload B must not have started a candidate from the new config"
    );

    let reload_c = manager.handle(PluginGatewayRequest::Reload);
    let Some(PluginGatewayResponsePayload::Reloaded { providers, .. }) = reload_c.payload else {
        panic!("later reload C failed: {:?}", reload_c.error);
    };
    let dynamic_c = providers[0].provider_instance_id.clone();
    assert_ne!(dynamic_c, dynamic_a);
    assert_eq!(
        fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "reload-new-start")
            .count(),
        1,
        "reload C must read and prepare the new config after A releases the gate"
    );
    let final_view = manager.handle(PluginGatewayRequest::ProvidersList);
    let Some(PluginGatewayResponsePayload::Providers { providers }) = final_view.payload else {
        panic!("final provider list failed: {:?}", final_view.error);
    };
    assert_eq!(providers[0].provider_instance_id, dynamic_c);
    manager.shutdown();
}

#[test]
fn generic_config_activation_holds_plugin_gate_through_followup_commit() {
    let fixture = Fixture::new("normal", 2);
    let plugins = {
        let committed = fixture
            .manager
            .committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        committed.config.clone()
    };
    let candidate = runner_config(plugins, ShellConfig::default(), fixture._temp.path());
    let manager = Arc::clone(&fixture.manager);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let activation = std::thread::spawn(move || {
        manager.apply_config_candidate_and_then(&candidate, || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
    });

    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("generic activation follow-up commit must start");
    let concurrent = fixture.manager.handle(PluginGatewayRequest::Reload);
    assert_eq!(concurrent.dispatch_state, PluginDispatchState::NotStarted);
    assert_eq!(
        concurrent.error.as_ref().map(|error| error.code.as_str()),
        Some("plugin_reload_busy")
    );
    release_tx.send(()).unwrap();
    assert!(activation.join().unwrap().is_ok());
}

#[test]
fn plugin_committed_environment_ignores_unrelated_shell_runtime_controls() {
    let fixture = Fixture::new("normal", 2);
    let plugins = fixture
        .manager
        .committed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .config
        .clone();
    let initial = current_providers(&fixture.manager)[0]
        .provider_instance_id
        .clone();

    let mut unrelated_shell = ShellConfig::default();
    unrelated_shell.max_persistent_shells += 1;
    unrelated_shell.persistent_shell_idle_timeout_secs += 1;
    let mut unused_profile = super::super::config::ShellProfileConfig::default();
    unused_profile.env.insert(
        "WEBCODEX_UNUSED_PLUGIN_PROFILE".to_string(),
        "v1".to_string(),
    );
    unrelated_shell
        .profiles
        .insert("unused".to_string(), unused_profile);
    let unrelated = runner_config(plugins.clone(), unrelated_shell, fixture._temp.path());
    fixture
        .manager
        .apply_config_candidate_and_then(&unrelated, || {})
        .unwrap();
    assert_eq!(
        current_providers(&fixture.manager)[0].provider_instance_id,
        initial,
        "generic persistent-shell controls and unreferenced profiles must not replace the Plugin provider"
    );

    let mut relevant_shell = ShellConfig::default();
    relevant_shell.default_profile = Some("plugin".to_string());
    relevant_shell.profiles.insert(
        "plugin".to_string(),
        super::super::config::ShellProfileConfig::default(),
    );
    let relevant = runner_config(
        plugins.clone(),
        relevant_shell.clone(),
        fixture._temp.path(),
    );
    fixture
        .manager
        .apply_config_candidate_and_then(&relevant, || {})
        .unwrap();
    let profiled = current_providers(&fixture.manager)[0]
        .provider_instance_id
        .clone();
    assert_ne!(
        profiled, initial,
        "selecting the default Plugin profile must replace the provider"
    );

    relevant_shell
        .profiles
        .get_mut("plugin")
        .unwrap()
        .env
        .insert("WEBCODEX_PLUGIN_ENV_TEST".to_string(), "v2".to_string());
    let changed_profile = runner_config(plugins, relevant_shell, fixture._temp.path());
    fixture
        .manager
        .apply_config_candidate_and_then(&changed_profile, || {})
        .unwrap();
    assert_ne!(
        current_providers(&fixture.manager)[0].provider_instance_id,
        profiled,
        "referenced Plugin profile changes must create a new provider instance"
    );
}

#[test]
fn shutdown_during_blocked_reload_does_not_wait_for_gate_or_allow_late_commit() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker.log");
    let fake = fake_binary();
    let startup_plugins = PluginConfig {
        request_timeout_secs: 2,
        providers: vec![PluginProviderConfig {
            id: "fake".to_string(),
            name: "Fake Plugin".to_string(),
            command: fake.path.to_string_lossy().into_owned(),
            args: vec!["normal".to_string(), marker.to_string_lossy().into_owned()],
            cwd: Some(temp.path().to_string_lossy().into_owned()),
            profile: None,
            timeout_secs: None,
        }],
    };
    let config_path = temp.path().join("runner.toml");
    write_runner_toml_with_timeout(
        &config_path,
        temp.path(),
        &fake.path,
        &marker,
        "reload_block_list",
        10,
    );
    let config = runner_config(startup_plugins, ShellConfig::default(), temp.path());
    let manager = Arc::new(PluginManager::new(&config, config_path));
    let reload_manager = Arc::clone(&manager);
    let reload = std::thread::spawn(move || reload_manager.handle(PluginGatewayRequest::Reload));
    assert!(wait_until(Duration::from_secs(2), || {
        fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .any(|line| line == "reload-blocked")
    }));

    let shutdown_started = Instant::now();
    manager.shutdown();
    assert!(
        shutdown_started.elapsed() < Duration::from_millis(1500),
        "shutdown must not wait for the reload gate held by candidate preparation"
    );
    let reload_response = reload.join().unwrap();
    assert_eq!(
        reload_response.dispatch_state,
        PluginDispatchState::NotStarted
    );
    assert_eq!(
        reload_response.error.as_ref().unwrap().code,
        "plugin_manager_stopping"
    );
    assert_eq!(
        fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "reload-released")
            .count(),
        0,
        "the blocked candidate must terminate from manager stopping rather than reaching commit"
    );
}

#[test]
fn failed_candidate_keeps_previous_instance_and_removal_has_no_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker.log");
    let fake = fake_binary();
    let plugins = PluginConfig {
        request_timeout_secs: 2,
        providers: vec![PluginProviderConfig {
            id: "fake".to_string(),
            name: "Fake Plugin".to_string(),
            command: fake.path.to_string_lossy().into_owned(),
            args: vec!["normal".to_string(), marker.to_string_lossy().into_owned()],
            cwd: Some(temp.path().to_string_lossy().into_owned()),
            profile: None,
            timeout_secs: None,
        }],
    };
    let config_path = temp.path().join("runner.toml");
    write_runner_toml(&config_path, temp.path(), &fake.path, &marker, "normal");
    let config = runner_config(plugins, ShellConfig::default(), temp.path());
    let manager = PluginManager::new(&config, config_path.clone());
    let committed_instance = current_providers(&manager)[0].provider_instance_id.clone();

    write_runner_toml(
        &config_path,
        temp.path(),
        &fake.path,
        &marker,
        "bad_version",
    );
    let failed_reload = manager.handle(PluginGatewayRequest::Reload);
    let Some(PluginGatewayResponsePayload::Reloaded {
        providers,
        failures,
        ..
    }) = failed_reload.payload
    else {
        panic!(
            "failed candidate reload response missing: {:?}",
            failed_reload.error
        );
    };
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].provider_id, "fake");
    assert_eq!(
        providers[0].provider_instance_id, committed_instance,
        "a failed candidate must not destroy the previous committed instance"
    );

    write_runner_toml_without_plugins(&config_path, temp.path());
    let removal = manager.handle(PluginGatewayRequest::Reload);
    let Some(PluginGatewayResponsePayload::Reloaded { providers, .. }) = removal.payload else {
        panic!("removal reload failed: {:?}", removal.error);
    };
    assert!(
        providers.is_empty(),
        "removed provider must disappear from committed view"
    );
    let stale = manager.handle(PluginGatewayRequest::ToolsList {
        provider_id: "fake".to_string(),
        provider_instance_id: committed_instance,
    });
    assert_eq!(
        stale.error.as_ref().map(|error| error.code.as_str()),
        Some("stale_plugin_provider"),
        "provider removal must never fall back to an earlier provider instance"
    );
}

#[test]
fn runner_config_reload_and_plugin_state_commit_as_one_active_generation() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker.log");
    let fake = fake_binary();
    let config_path = temp.path().join("runner.toml");
    write_runner_toml(&config_path, temp.path(), &fake.path, &marker, "normal");
    let config = super::super::config::load_config(&config_path).unwrap();
    let runtime = super::super::config::ReloadableRunnerConfig::new(config, config_path.clone());

    let v1 = current_providers(runtime.plugins())[0].clone();
    assert_eq!(runtime.snapshot().generation, 1);

    write_runner_toml(&config_path, temp.path(), &fake.path, &marker, "check_v2");
    let applied = runtime.reload_config(1);
    assert_eq!(applied.valid, Some(true));
    assert_eq!(applied.current_generation, Some(2));
    assert!(!applied.restart_required);
    assert!(applied.restart_required_fields.is_empty());
    assert_eq!(runtime.snapshot().generation, 2);
    let v2 = current_providers(runtime.plugins())[0].clone();
    assert_ne!(v2.provider_instance_id, v1.provider_instance_id);
    assert_eq!(current_tools(runtime.plugins(), &v2)[0].name, "echo_v2");

    write_runner_toml(
        &config_path,
        temp.path(),
        &fake.path,
        &marker,
        "bad_version",
    );
    let rejected = runtime.reload_config(2);
    assert_eq!(rejected.valid, Some(false));
    assert_eq!(rejected.current_generation, Some(2));
    assert_eq!(rejected.error_code.as_deref(), Some("plugin_reload_failed"));
    assert_eq!(runtime.snapshot().generation, 2);
    let still_v2 = current_providers(runtime.plugins())[0].clone();
    assert_eq!(still_v2.provider_instance_id, v2.provider_instance_id);
    assert_eq!(
        current_tools(runtime.plugins(), &still_v2)[0].name,
        "echo_v2"
    );
}

fn write_runner_toml(
    path: &Path,
    project_registry_dir: &Path,
    executable: &Path,
    marker: &Path,
    scenario: &str,
) {
    write_runner_toml_with_timeout(path, project_registry_dir, executable, marker, scenario, 2);
}

fn write_runner_toml_with_timeout(
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
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"test-token\"\nclient_id = \"plugin-test\"\nproject_registry_dir = {}\n\n[plugins]\nrequest_timeout_secs = {request_timeout_secs}\n\n[[plugins.providers]]\nid = \"fake\"\nname = \"Fake Plugin\"\ncommand = {}\nargs = [\"{}\", {}]\ncwd = {}\n",
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
            "server_url = \"http://127.0.0.1:8000\"\ntoken = \"test-token\"\nclient_id = \"plugin-test\"\nproject_registry_dir = {}\n\n[plugins]\nrequest_timeout_secs = 2\n",
            quoted(project_registry_dir),
        ),
    )
    .unwrap();
}
