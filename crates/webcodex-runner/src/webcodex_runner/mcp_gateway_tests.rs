use super::*;
use crate::mcp_gateway::{
    McpGatewayContent, McpGatewayResponsePayload, McpGatewaySchemaObservation,
    MCP_GATEWAY_MAX_MESSAGE_BYTES, MCP_GATEWAY_MAX_RESULT_BYTES,
};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tempfile::TempDir;

static FAKE_SERVER: OnceLock<Mutex<Weak<FakeBinary>>> = OnceLock::new();
const TEST_PARALLEL_TIMEOUT_FLOOR_SECS: u64 = 10;
const TEST_INTENTIONAL_TIMEOUT_SECS: u64 = 5;

struct FakeBinary {
    _temp: TempDir,
    path: PathBuf,
}

fn fake_binary() -> Arc<FakeBinary> {
    let cache = FAKE_SERVER.get_or_init(|| Mutex::new(Weak::new()));
    let mut cached = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(binary) = cached.upgrade() {
        return binary;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join(format!(
        "webcodex-mcp-bridge-fake{}",
        env::consts::EXE_SUFFIX
    ));
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/webcodex_runner/fake_mcp_gateway.rs");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let result = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-name=webcodex_mcp_gateway_fake")
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
    manager: McpGatewayManager,
    marker: PathBuf,
    _fake: Arc<FakeBinary>,
    _temp: TempDir,
}

impl Fixture {
    fn new(scenario: &str, timeout_secs: u64) -> Self {
        // Windows CI can spawn many fake providers in parallel. Keep ordinary
        // fixture deadlines above that startup jitter; timing-specific tests use
        // with_provider_timeout directly to preserve a deliberately short bound.
        Self::with_provider_timeout(
            scenario,
            timeout_secs.max(TEST_PARALLEL_TIMEOUT_FLOOR_SECS),
            None,
        )
    }

    fn with_provider_timeout(
        scenario: &str,
        default_timeout_secs: u64,
        provider_timeout_secs: Option<u64>,
    ) -> Self {
        Self::with_execution_context(
            scenario,
            default_timeout_secs,
            provider_timeout_secs,
            None,
            BTreeMap::new(),
        )
    }

    fn with_execution_context(
        scenario: &str,
        default_timeout_secs: u64,
        provider_timeout_secs: Option<u64>,
        cwd: Option<String>,
        env_from_env: BTreeMap<String, String>,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("marker.log");
        let fake = fake_binary();
        let mut args = vec![scenario.to_string(), marker.to_string_lossy().to_string()];
        if let Some(cwd) = cwd.as_ref() {
            args.push(cwd.clone());
        }
        let manager = McpGatewayManager::new(&McpGatewayConfig {
            request_timeout_secs: default_timeout_secs,
            providers: vec![McpGatewayProviderConfig {
                id: "fake".to_string(),
                name: "Fake provider".to_string(),
                executable: fake.path.to_string_lossy().to_string(),
                args,
                cwd,
                env_from_env,
                timeout_secs: provider_timeout_secs,
            }],
        });
        Self {
            manager,
            marker,
            _fake: fake,
            _temp: temp,
        }
    }

    fn provider(&self) -> McpGatewayProvider {
        self.manager
            .provider_inventory()
            .into_iter()
            .next()
            .unwrap()
    }

    fn status(&self, provider: &McpGatewayProvider) -> McpGatewayResponse {
        self.manager.handle(McpGatewayRequest::ProviderStatus {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
        })
    }

    fn list(&self, provider: &McpGatewayProvider) -> McpGatewayResponse {
        self.manager.handle(McpGatewayRequest::ToolsList {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
        })
    }

    fn call(&self, provider: &McpGatewayProvider) -> McpGatewayResponse {
        self.manager.handle(McpGatewayRequest::ToolsCall {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
            name: "echo".to_string(),
            arguments: json!({"value": "hello"}),
            expected_schema: McpGatewaySchemaObservation {
                input_schema: json!({
                    "type": "object",
                    "properties": {"value": {"type": "string"}}
                }),
                output_schema: None,
                annotations: None,
            },
        })
    }

    fn marker_count(&self, value: &str) -> usize {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == value)
            .count()
    }
}

fn provider_state(response: McpGatewayResponse) -> McpGatewayProviderState {
    let Some(McpGatewayResponsePayload::ProviderStatus { state }) = response.payload else {
        panic!("provider status payload missing: {:?}", response.error);
    };
    state
}

#[test]
fn provider_status_is_passive_and_tracks_connection_lifecycle() {
    let fixture = Fixture::new("crash", 2);
    let provider = fixture.provider();

    assert_eq!(
        provider_state(fixture.status(&provider)),
        McpGatewayProviderState::NeverStarted
    );
    assert_eq!(fixture.marker_count("start"), 0);

    assert!(fixture.list(&provider).error.is_none());
    assert_eq!(
        provider_state(fixture.status(&provider)),
        McpGatewayProviderState::Healthy
    );
    assert_eq!(fixture.marker_count("start"), 1);

    let failed = fixture.call(&provider);
    assert_eq!(
        failed.dispatch_state,
        McpGatewayDispatchState::OutcomeUnknown
    );
    assert_eq!(
        provider_state(fixture.status(&provider)),
        McpGatewayProviderState::ConnectionRetired
    );
    assert_eq!(fixture.marker_count("start"), 1);

    assert!(fixture.list(&provider).error.is_none());
    assert_eq!(
        provider_state(fixture.status(&provider)),
        McpGatewayProviderState::Healthy
    );
    assert_eq!(fixture.marker_count("start"), 2);
}

#[test]
fn provider_status_reports_busy_without_waiting_or_starting_work() {
    let fixture = Fixture::new("normal", 2);
    let provider = fixture.provider();
    let entry = {
        let state = fixture.manager.state.read().unwrap();
        Arc::clone(state.providers.get("fake").unwrap())
    };
    let _guard = entry.session.lock().unwrap();

    assert_eq!(
        provider_state(fixture.status(&provider)),
        McpGatewayProviderState::Busy
    );
    assert_eq!(fixture.marker_count("start"), 0);
}

fn replacement_config(
    fixture: &Fixture,
    id: &str,
    name: &str,
    scenario: &str,
    request_timeout_secs: u64,
) -> McpGatewayConfig {
    McpGatewayConfig {
        request_timeout_secs,
        providers: vec![McpGatewayProviderConfig {
            id: id.to_string(),
            name: name.to_string(),
            executable: fixture._fake.path.to_string_lossy().into_owned(),
            args: vec![
                scenario.to_string(),
                fixture.marker.to_string_lossy().into_owned(),
            ],
            cwd: None,
            env_from_env: BTreeMap::new(),
            timeout_secs: None,
        }],
    }
}

#[test]
fn config_candidate_preserves_unchanged_provider_identity_and_connection() {
    let fixture = Fixture::new("normal", 2);
    let before = fixture.provider();
    assert!(fixture.list(&before).error.is_none());
    assert_eq!(fixture.marker_count("start"), 1);

    let summary = fixture
        .manager
        .apply_config_candidate(&replacement_config(
            &fixture,
            "fake",
            "Fake provider",
            "normal",
            17,
        ))
        .unwrap();
    assert_eq!(
        summary,
        McpGatewayReloadSummary {
            preserved: 1,
            replaced: 0,
            added: 0,
            removed: 0,
        }
    );
    let after = fixture.provider();
    assert_eq!(after.provider_instance_id, before.provider_instance_id);
    assert!(fixture.list(&before).error.is_none());
    assert_eq!(fixture.marker_count("start"), 1);
}

#[test]
fn config_candidate_replaces_changed_provider_without_retargeting_old_identity() {
    let fixture = Fixture::new("normal", 2);
    let before = fixture.provider();
    assert!(fixture.list(&before).error.is_none());
    assert_eq!(fixture.marker_count("start"), 1);

    let summary = fixture
        .manager
        .apply_config_candidate(&replacement_config(
            &fixture,
            "fake",
            "Renamed provider",
            "normal",
            10,
        ))
        .unwrap();
    assert_eq!(summary.replaced, 1);
    let after = fixture.provider();
    assert_ne!(after.provider_instance_id, before.provider_instance_id);
    let stale = fixture.list(&before);
    assert_eq!(stale.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(stale.error.as_ref().unwrap().code, "stale_provider");
    assert_eq!(
        provider_state(fixture.status(&after)),
        McpGatewayProviderState::NeverStarted
    );
    assert_eq!(fixture.marker_count("start"), 1);
    assert!(fixture.list(&after).error.is_none());
    assert_eq!(fixture.marker_count("start"), 2);
}

#[test]
fn config_candidate_does_not_wait_for_busy_retired_provider() {
    let fixture = Fixture::new("normal", 2);
    let before = fixture.provider();
    let entry = {
        let state = fixture.manager.state.read().unwrap();
        Arc::clone(state.providers.get("fake").unwrap())
    };
    let _guard = entry.session.lock().unwrap();

    let summary = fixture
        .manager
        .apply_config_candidate(&replacement_config(
            &fixture,
            "fake",
            "Changed while busy",
            "normal",
            10,
        ))
        .unwrap();
    assert_eq!(summary.replaced, 1);
    let after = fixture.provider();
    assert_ne!(after.provider_instance_id, before.provider_instance_id);
    assert_eq!(
        provider_state(fixture.status(&after)),
        McpGatewayProviderState::NeverStarted
    );
}

#[test]
fn config_candidate_adds_and_removes_provider_identities_atomically() {
    let fixture = Fixture::new("normal", 2);
    let before = fixture.provider();
    let summary = fixture
        .manager
        .apply_config_candidate(&replacement_config(
            &fixture,
            "replacement",
            "Replacement",
            "normal",
            10,
        ))
        .unwrap();
    assert_eq!(summary.added, 1);
    assert_eq!(summary.removed, 1);
    let inventory = fixture.manager.provider_inventory();
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].provider_id, "replacement");
    let stale = fixture.status(&before);
    assert_eq!(stale.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(stale.error.as_ref().unwrap().code, "stale_provider");
}

#[test]
fn persistent_provider_initializes_once_and_serves_repeated_calls() {
    let fixture = Fixture::new("normal", 5);
    let provider = fixture.provider();
    let listed = fixture.list(&provider);
    let Some(McpGatewayResponsePayload::Tools { tools }) = listed.payload else {
        panic!("tools payload missing: {:?}", listed.error);
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    for expected in ["call-1", "call-2"] {
        let response = fixture.call(&provider);
        let Some(McpGatewayResponsePayload::ToolResult { result }) = response.payload else {
            panic!("call payload missing: {:?}", response.error);
        };
        assert_eq!(
            result.content,
            vec![McpGatewayContent::Text {
                text: expected.to_string()
            }]
        );
    }
    assert_eq!(fixture.marker_count("start"), 1);
    assert_eq!(fixture.marker_count("initialize"), 1);
    assert_eq!(fixture.marker_count("initialized"), 1);
    assert_eq!(fixture.marker_count("call"), 2);
}

#[test]
fn large_tool_result_crosses_local_mcp_bridge_below_result_and_message_bounds() {
    assert_eq!(MCP_GATEWAY_MAX_RESULT_BYTES, 512 * 1024);
    assert_eq!(MCP_GATEWAY_MAX_MESSAGE_BYTES, 1024 * 1024);

    let fixture = Fixture::new("large_result", 3);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());
    let response = fixture.call(&provider);
    let Some(McpGatewayResponsePayload::ToolResult { result }) = response.payload else {
        panic!("large tool result missing: {:?}", response.error);
    };
    let [McpGatewayContent::Text { text }] = result.content.as_slice() else {
        panic!("unexpected large result content");
    };
    assert_eq!(text.len(), 192 * 1024);
    assert_eq!(
        result.structured_content.as_ref().unwrap()["payload"]
            .as_str()
            .unwrap()
            .len(),
        192 * 1024
    );
    assert!(fixture.list(&provider).error.is_none());
}

#[test]
fn schema_change_blocks_effectful_call_without_retiring_provider() {
    let fixture = Fixture::new("schema_change", 3);
    let provider = fixture.provider();
    let described = fixture.list(&provider);
    assert!(matches!(
        described.payload,
        Some(McpGatewayResponsePayload::Tools { .. })
    ));

    let response = fixture.call(&provider);
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(
        response.error.as_ref().map(|error| error.code.as_str()),
        Some("provider_schema_changed")
    );
    assert_eq!(fixture.marker_count("call"), 0);

    let listed_again = fixture.list(&provider);
    assert!(matches!(
        listed_again.payload,
        Some(McpGatewayResponsePayload::Tools { .. })
    ));
}

#[test]
fn tools_call_sends_only_gateway_owned_name_and_arguments() {
    let fixture = Fixture::new("meta_forbidden", 2);
    let provider = fixture.provider();
    let response = fixture.call(&provider);
    assert!(matches!(
        response.payload,
        Some(McpGatewayResponsePayload::ToolResult { .. })
    ));
    assert_eq!(fixture.marker_count("call"), 1);
}

#[test]
fn provider_execution_context_is_explicit_cleared_and_private() {
    let _guard = crate::tests::test_env_lock();
    let _env = crate::tests::EnvGuard::new()
        .set("GITHUB_TOKEN", "github-provider-secret-value")
        .set("WEBCODEX_MCP_MAPPED_SOURCE", "mapped-provider-secret-value")
        .set("WEBCODEX_MCP_UNLISTED", "must-not-reach-provider");
    let cwd = tempfile::tempdir().unwrap();
    let fixture = Fixture::with_execution_context(
        "execution_context",
        TEST_PARALLEL_TIMEOUT_FLOOR_SECS,
        None,
        Some(cwd.path().to_string_lossy().into_owned()),
        BTreeMap::from([
            ("GITHUB_TOKEN".to_string(), "GITHUB_TOKEN".to_string()),
            (
                "WEBCODEX_MCP_MAPPED_CHILD".to_string(),
                "WEBCODEX_MCP_MAPPED_SOURCE".to_string(),
            ),
        ]),
    );
    let provider = fixture.provider();
    let inventory = serde_json::to_string(&fixture.manager.provider_inventory()).unwrap();
    assert!(!inventory.contains("github-provider-secret-value"));
    assert!(!inventory.contains("mapped-provider-secret-value"));

    let response = fixture.list(&provider);
    assert!(response.error.is_none(), "{:?}", response.error);
    for marker in [
        "github-env-ok",
        "mapped-env-ok",
        "unlisted-env-cleared",
        "path-cleared",
        "cwd-ok",
    ] {
        assert_eq!(fixture.marker_count(marker), 1, "missing marker {marker}");
    }
    #[cfg(windows)]
    assert_eq!(
        fixture.marker_count("systemroot-bootstrap-ok"),
        1,
        "Windows MCP providers need the minimal SYSTEMROOT bootstrap after env_clear()"
    );
    let encoded = serde_json::to_string(&response).unwrap();
    assert!(!encoded.contains("github-provider-secret-value"));
    assert!(!encoded.contains("mapped-provider-secret-value"));
    assert!(!encoded.contains("must-not-reach-provider"));
}

#[test]
fn missing_mapped_source_fails_before_provider_spawn() {
    let _guard = crate::tests::test_env_lock();
    let _env = crate::tests::EnvGuard::new().remove("WEBCODEX_MCP_TEST_MISSING_SOURCE");
    let fixture = Fixture::with_execution_context(
        "normal",
        2,
        None,
        None,
        BTreeMap::from([(
            "PROVIDER_CREDENTIAL".to_string(),
            "WEBCODEX_MCP_TEST_MISSING_SOURCE".to_string(),
        )]),
    );
    let response = fixture.list(&fixture.provider());
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "provider_env_missing"
    );
    assert_eq!(fixture.marker_count("start"), 0);
}

#[test]
fn sensitive_runner_env_mapping_is_blocked_before_provider_spawn() {
    let fixture = Fixture::with_execution_context(
        "normal",
        2,
        None,
        None,
        BTreeMap::from([(
            "PROVIDER_CREDENTIAL".to_string(),
            "WEBCODEX_AGENT_TOKEN".to_string(),
        )]),
    );
    let response = fixture.list(&fixture.provider());
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "provider_env_forbidden"
    );
    assert_eq!(fixture.marker_count("start"), 0);
    assert!(!serde_json::to_string(&response)
        .unwrap()
        .contains("WEBCODEX_AGENT_TOKEN"));
}

#[test]
fn unavailable_provider_cwd_fails_before_provider_spawn() {
    let cwd_parent = tempfile::tempdir().unwrap();
    let missing = cwd_parent.path().join("missing-provider-cwd");
    let fixture = Fixture::with_execution_context(
        "normal",
        2,
        None,
        Some(missing.to_string_lossy().into_owned()),
        BTreeMap::new(),
    );
    let response = fixture.list(&fixture.provider());
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "provider_cwd_unavailable"
    );
    assert_eq!(fixture.marker_count("start"), 0);
}

#[test]
fn provider_notifications_are_consumed_without_retiring_the_session() {
    let fixture = Fixture::new("notifications", 2);
    let provider = fixture.provider();

    let listed = fixture.list(&provider);
    assert!(listed.error.is_none(), "{:?}", listed.error);
    // Let the post-response logging notification reach the bounded reader queue
    // so the next operation exercises the pre-dispatch drain path as well.
    std::thread::sleep(Duration::from_millis(25));

    let called = fixture.call(&provider);
    let Some(McpGatewayResponsePayload::ToolResult { result }) = called.payload else {
        panic!("call payload missing: {:?}", called.error);
    };
    assert_eq!(
        result.content,
        vec![McpGatewayContent::Text {
            text: "call-1".to_string()
        }]
    );
    assert!(fixture.list(&provider).error.is_none());
    assert_eq!(fixture.marker_count("start"), 1);
    assert_eq!(fixture.marker_count("initialize"), 1);
}

#[test]
fn provider_status_reaps_an_exited_connection_without_restarting_it() {
    let fixture = Fixture::new("exit_after_list", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());
    std::thread::sleep(Duration::from_millis(50));

    assert_eq!(
        provider_state(fixture.status(&provider)),
        McpGatewayProviderState::ConnectionRetired
    );
    assert_eq!(fixture.marker_count("start"), 1);
}

#[test]
fn provider_callback_after_dispatch_remains_unsupported_and_unknown() {
    let fixture = Fixture::new("callback", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());

    let response = fixture.call(&provider);
    assert_eq!(
        response.dispatch_state,
        McpGatewayDispatchState::OutcomeUnknown
    );
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "provider_callbacks_unsupported"
    );
    let recovered = fixture.list(&provider);
    assert!(recovered.error.is_none(), "{:?}", recovered.error);
    assert_eq!(fixture.marker_count("start"), 2);
    assert_eq!(fixture.marker_count("call"), 1);
}

#[test]
fn provider_notification_flood_is_bounded_and_reconnects_on_next_request() {
    let fixture = Fixture::new("notification_flood", 2);
    let provider = fixture.provider();

    let response = fixture.list(&provider);
    assert_eq!(
        response.dispatch_state,
        McpGatewayDispatchState::OutcomeUnknown
    );
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "provider_notification_flood"
    );
    let retried = fixture.list(&provider);
    assert_eq!(
        retried.error.as_ref().unwrap().code,
        "provider_notification_flood"
    );
    assert_eq!(fixture.marker_count("start"), 2);
}

#[test]
fn crash_is_outcome_unknown_but_later_request_reconnects_without_replay() {
    let fixture = Fixture::new("crash", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());
    let first = fixture.call(&provider);
    assert_eq!(
        first.dispatch_state,
        McpGatewayDispatchState::OutcomeUnknown
    );
    assert_eq!(first.error.as_ref().unwrap().code, "provider_eof");

    let recovered = fixture.list(&provider);
    assert!(recovered.error.is_none(), "{:?}", recovered.error);
    assert_eq!(
        fixture.provider().provider_instance_id,
        provider.provider_instance_id
    );
    assert_eq!(fixture.marker_count("start"), 2);
    assert_eq!(fixture.marker_count("initialize"), 2);
    assert_eq!(fixture.marker_count("call"), 1);
}

#[test]
fn initialization_failure_is_not_tool_dispatch_and_later_requests_retry_spawn() {
    for (scenario, code) in [
        ("init_crash", "provider_eof"),
        ("init_timeout", "provider_timeout"),
        ("init_missing_tools", "provider_initialize_invalid"),
    ] {
        let fixture = Fixture::with_provider_timeout(
            scenario,
            TEST_PARALLEL_TIMEOUT_FLOOR_SECS,
            Some(TEST_INTENTIONAL_TIMEOUT_SECS),
        );
        let provider = fixture.provider();
        let response = fixture.call(&provider);
        assert_eq!(
            response.dispatch_state,
            McpGatewayDispatchState::NotStarted,
            "{scenario}"
        );
        assert_eq!(response.error.as_ref().unwrap().code, code, "{scenario}");
        assert_eq!(fixture.marker_count("call"), 0, "{scenario}");
        assert_eq!(fixture.marker_count("start"), 1, "{scenario}");
        let retried = fixture.call(&provider);
        assert_eq!(
            retried.dispatch_state,
            McpGatewayDispatchState::NotStarted,
            "{scenario}"
        );
        assert_eq!(retried.error.as_ref().unwrap().code, code, "{scenario}");
        assert_eq!(fixture.marker_count("start"), 2, "{scenario}");
    }
}

#[test]
fn transient_initialization_failure_recovers_on_next_explicit_request() {
    let fixture = Fixture::new("init_crash_once", 2);
    let provider = fixture.provider();
    let first = fixture.list(&provider);
    assert_eq!(first.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(first.error.as_ref().unwrap().code, "provider_eof");

    let second = fixture.list(&provider);
    assert!(second.error.is_none(), "{:?}", second.error);
    assert_eq!(fixture.marker_count("start"), 2);
    assert_eq!(fixture.marker_count("initialize"), 2);
}

#[test]
fn reconnect_revalidates_schema_before_effectful_dispatch() {
    let fixture = Fixture::new("recover_schema_change", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());

    let first = fixture.call(&provider);
    assert_eq!(
        first.dispatch_state,
        McpGatewayDispatchState::OutcomeUnknown
    );
    assert_eq!(first.error.as_ref().unwrap().code, "provider_eof");
    assert_eq!(fixture.marker_count("call"), 1);

    let second = fixture.call(&provider);
    assert_eq!(second.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(
        second.error.as_ref().unwrap().code,
        "provider_schema_changed"
    );
    assert_eq!(fixture.marker_count("start"), 2);
    assert_eq!(
        fixture.marker_count("call"),
        1,
        "unknown call must not be replayed"
    );
}

#[test]
fn malformed_unknown_and_duplicate_responses_fail_closed() {
    for (scenario, code) in [
        ("malformed", "provider_malformed_json"),
        ("unknown_id", "provider_unknown_response_id"),
    ] {
        let fixture = Fixture::new(scenario, 2);
        let response = fixture.list(&fixture.provider());
        assert_eq!(response.error.as_ref().unwrap().code, code, "{scenario}");
        assert_eq!(
            response.dispatch_state,
            McpGatewayDispatchState::OutcomeUnknown
        );
    }

    let fixture = Fixture::new("duplicate_id", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());
    let response = fixture.call(&provider);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "provider_duplicate_response_id"
    );
}

#[test]
fn timeout_and_invalid_untrusted_outputs_are_bounded() {
    let timeout = Fixture::with_provider_timeout(
        "timeout",
        TEST_PARALLEL_TIMEOUT_FLOOR_SECS,
        Some(TEST_INTENTIONAL_TIMEOUT_SECS),
    );
    let provider = timeout.provider();
    assert!(timeout.list(&provider).error.is_none());
    let response = timeout.call(&provider);
    assert_eq!(response.error.as_ref().unwrap().code, "provider_timeout");
    assert_eq!(
        response.dispatch_state,
        McpGatewayDispatchState::OutcomeUnknown
    );
    let recovered = timeout.list(&provider);
    assert!(recovered.error.is_none(), "{:?}", recovered.error);
    assert_eq!(timeout.marker_count("start"), 2);
    assert_eq!(timeout.marker_count("call"), 1);

    for (scenario, operation, code, state) in [
        (
            "bad_tools",
            "list",
            "invalid_provider_tools",
            McpGatewayDispatchState::Completed,
        ),
        (
            "oversized_message",
            "list",
            "provider_message_too_large",
            McpGatewayDispatchState::OutcomeUnknown,
        ),
        (
            "bad_result",
            "call",
            "unsupported_provider_content",
            McpGatewayDispatchState::Completed,
        ),
        (
            "oversized_result",
            "call",
            "invalid_provider_result",
            McpGatewayDispatchState::Completed,
        ),
        (
            "paginated_list",
            "list",
            "provider_pagination_unsupported",
            McpGatewayDispatchState::Completed,
        ),
    ] {
        let fixture = Fixture::new(scenario, 2);
        let provider = fixture.provider();
        let response = if operation == "list" {
            fixture.list(&provider)
        } else {
            assert!(fixture.list(&provider).error.is_none());
            fixture.call(&provider)
        };
        assert_eq!(response.error.as_ref().unwrap().code, code, "{scenario}");
        assert_eq!(response.dispatch_state, state, "{scenario}");
    }
}

#[test]
fn correlated_invalid_result_does_not_retire_provider_instance() {
    let fixture = Fixture::new("bad_result", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());

    let response = fixture.call(&provider);
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::Completed);
    assert_eq!(
        response.error.as_ref().unwrap().code,
        "unsupported_provider_content"
    );

    // The response was correlated, so the stdio session is still synchronized.
    // A V1 result-format rejection must not turn the configured provider into a
    // permanently stale instance or force a hidden respawn.
    assert!(fixture.list(&provider).error.is_none());
    assert_eq!(fixture.marker_count("start"), 1);
    assert_eq!(fixture.marker_count("initialize"), 1);
}

#[test]
fn provider_timeout_override_and_default_fallback_are_enforced() {
    let inherited = Fixture::new("slow", 2);
    let inherited_provider = inherited.provider();
    assert!(inherited.list(&inherited_provider).error.is_none());
    assert!(inherited.call(&inherited_provider).error.is_none());

    let overridden = Fixture::with_provider_timeout(
        "slow",
        TEST_PARALLEL_TIMEOUT_FLOOR_SECS,
        Some(TEST_INTENTIONAL_TIMEOUT_SECS),
    );
    let overridden_provider = overridden.provider();
    assert!(overridden.list(&overridden_provider).error.is_none());
    let response = overridden.call(&overridden_provider);
    assert_eq!(
        response.dispatch_state,
        McpGatewayDispatchState::OutcomeUnknown
    );
    assert_eq!(response.error.as_ref().unwrap().code, "provider_timeout");
}

#[test]
fn correlated_jsonrpc_error_is_completed_without_retry() {
    let fixture = Fixture::new("rpc_error", 2);
    let provider = fixture.provider();
    assert!(fixture.list(&provider).error.is_none());
    let response = fixture.call(&provider);
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::Completed);
    assert_eq!(response.error.as_ref().unwrap().code, "provider_rpc_error");
    assert_eq!(fixture.marker_count("call"), 1);
}

#[test]
fn stale_provider_instance_fails_without_starting_child() {
    let fixture = Fixture::new("normal", 2);
    let mut provider = fixture.provider();
    provider.provider_instance_id = "stale".to_string();
    let response = fixture.list(&provider);
    assert_eq!(response.dispatch_state, McpGatewayDispatchState::NotStarted);
    assert_eq!(response.error.as_ref().unwrap().code, "stale_provider");
    assert_eq!(fixture.marker_count("start"), 0);
}
