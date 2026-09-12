use crate::agent_wake::{
    ContinuationAdapter, ContinuationDispatchOutcome, ContinuationPreflight,
    ContinuationPreflightError,
};
use crate::db::{AgentWakeEnvelope, AgentWakeState};
use crate::tool_runtime::ToolRuntime;
use crate::{Database, RunnerRegistry};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct FakeHostAdapter {
    preflight_error: Option<&'static str>,
    outcome: ContinuationDispatchOutcome,
    preflight_count: AtomicUsize,
    envelopes: Mutex<Vec<AgentWakeEnvelope>>,
}

impl FakeHostAdapter {
    fn delivered() -> Self {
        Self {
            preflight_error: None,
            outcome: ContinuationDispatchOutcome::Delivered,
            preflight_count: AtomicUsize::new(0),
            envelopes: Mutex::new(Vec::new()),
        }
    }

    fn dispatch_count(&self) -> usize {
        self.envelopes.lock().unwrap().len()
    }

    fn latest_envelope(&self) -> AgentWakeEnvelope {
        self.envelopes.lock().unwrap().last().unwrap().clone()
    }
}

impl ContinuationAdapter for FakeHostAdapter {
    fn adapter_kind(&self) -> &'static str {
        "deterministic_fake"
    }

    fn preflight(
        &self,
        _continuation: &ContinuationPreflight,
    ) -> Result<(), ContinuationPreflightError> {
        self.preflight_count.fetch_add(1, Ordering::SeqCst);
        match self.preflight_error {
            Some(kind) => Err(ContinuationPreflightError::new(kind)),
            None => Ok(()),
        }
    }

    fn dispatch(&self, envelope: &AgentWakeEnvelope) -> ContinuationDispatchOutcome {
        self.envelopes.lock().unwrap().push(envelope.clone());
        self.outcome
    }
}

#[derive(Debug, Default)]
struct BlockingHostAdapter {
    entered: (Mutex<bool>, Condvar),
    release: (Mutex<bool>, Condvar),
    dispatch_count: AtomicUsize,
}

impl BlockingHostAdapter {
    fn wait_until_preflight(&self) {
        let (lock, ready) = &self.entered;
        let mut entered = lock.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !*entered {
            let timeout = deadline.saturating_duration_since(Instant::now());
            assert!(
                !timeout.is_zero(),
                "timed out waiting for blocking preflight"
            );
            let (next, result) = ready.wait_timeout(entered, timeout).unwrap();
            entered = next;
            assert!(
                !result.timed_out() || *entered,
                "timed out waiting for blocking preflight"
            );
        }
    }

    fn release_preflight(&self) {
        let (lock, ready) = &self.release;
        *lock.lock().unwrap() = true;
        ready.notify_all();
    }
}

impl ContinuationAdapter for BlockingHostAdapter {
    fn adapter_kind(&self) -> &'static str {
        "blocking_fake"
    }

    fn preflight(
        &self,
        _continuation: &ContinuationPreflight,
    ) -> Result<(), ContinuationPreflightError> {
        let (entered_lock, entered_ready) = &self.entered;
        *entered_lock.lock().unwrap() = true;
        entered_ready.notify_all();

        let (release_lock, release_ready) = &self.release;
        let mut released = release_lock.lock().unwrap();
        while !*released {
            released = release_ready.wait(released).unwrap();
        }
        Ok(())
    }

    fn dispatch(&self, _envelope: &AgentWakeEnvelope) -> ContinuationDispatchOutcome {
        self.dispatch_count.fetch_add(1, Ordering::SeqCst);
        ContinuationDispatchOutcome::Delivered
    }
}

fn wait_until(label: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if ready() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::park_timeout(Duration::from_millis(5));
    }
}

fn runtime_with_db(db: Arc<Database>) -> ToolRuntime {
    ToolRuntime::new_for_tests_with_runner_registry(Arc::new(RunnerRegistry::default()))
        .with_communication_database(db)
}

fn create_agent(
    runtime: &ToolRuntime,
    handle: &str,
    display_name: &str,
    description: &str,
    label: &str,
    key: &str,
) -> String {
    let result = runtime.create_agent_identity(
        None,
        handle.to_string(),
        display_name.to_string(),
        Some(description.to_string()),
        vec![label.to_string()],
        key.to_string(),
    );
    assert!(result.success, "{:?}", result.output);
    result.output["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn attach(runtime: &ToolRuntime, agent_id: &str, key: &str) -> (String, i64) {
    let result = runtime.attach_agent_endpoint(
        None,
        agent_id.to_string(),
        "Deterministic Host".to_string(),
        Some(format!("attachment-{key}")),
        key.to_string(),
    );
    assert!(result.success, "{:?}", result.output);
    assert_eq!(result.output["endpoint"]["wake_capable"], false);
    (
        result.output["endpoint"]["endpoint_id"]
            .as_str()
            .unwrap()
            .to_string(),
        result.output["endpoint"]["controller_generation"]
            .as_i64()
            .unwrap(),
    )
}

fn create_conversation(runtime: &ToolRuntime, agent_a: &str, agent_b: &str, key: &str) -> String {
    let result = runtime.create_conversation(
        None,
        Some("Natural durable conversation".to_string()),
        vec![agent_a.to_string(), agent_b.to_string()],
        key.to_string(),
    );
    assert!(result.success, "{:?}", result.output);
    result.output["conversation"]["conversation"]["conversation_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn post_as_agent(
    runtime: &ToolRuntime,
    conversation_id: &str,
    body: &str,
    author_agent_id: &str,
    endpoint_id: &str,
    controller_generation: i64,
    recipient_agent_id: &str,
    idempotency_key: Option<&str>,
    wake_reply_id: Option<&str>,
    reply_operation_index: Option<i64>,
) -> Value {
    let result = runtime.post_conversation_message(
        None,
        conversation_id.to_string(),
        body.to_string(),
        Some(author_agent_id.to_string()),
        Some(endpoint_id.to_string()),
        Some(controller_generation),
        Some(vec![recipient_agent_id.to_string()]),
        None,
        idempotency_key.map(ToOwned::to_owned),
        wake_reply_id.map(ToOwned::to_owned),
        reply_operation_index,
    );
    assert!(result.success, "{:?}", result.output);
    result.output
}

fn count(db: &Database, table: &str) -> i64 {
    assert!(matches!(
        table,
        "wc_conversation_messages" | "wc_agent_deliveries" | "wc_agent_wakes"
    ));
    db.conn_for_tests()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn endpoint_recovery_fingerprint(db: &Database, endpoint_id: &str) -> Option<String> {
    db.conn_for_tests()
        .query_row(
            "SELECT mcp_app_recovery_fingerprint FROM wc_agent_endpoints WHERE endpoint_id = ?1",
            [endpoint_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn endpoint_client_window_key(db: &Database, endpoint_id: &str) -> Option<String> {
    db.conn_for_tests()
        .query_row(
            "SELECT mcp_app_client_window_key FROM wc_agent_endpoints WHERE endpoint_id = ?1",
            [endpoint_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn wake_id_for(db: &Database, agent_id: &str) -> String {
    db.conn_for_tests()
        .query_row(
            "SELECT wake_id FROM wc_agent_wakes
             WHERE target_agent_id = ?1 AND state != 'consumed'
             ORDER BY created_at_unix_ms, wake_id LIMIT 1",
            [agent_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn bind_mcp_app(
    runtime: &ToolRuntime,
    agent_id: &str,
    endpoint_id: &str,
    generation: i64,
) -> String {
    let binding_id = format!("wc_host_binding_{}", uuid::Uuid::new_v4().simple());
    let result = runtime.agent_continuation_bind(
        None,
        agent_id.to_string(),
        endpoint_id.to_string(),
        generation,
        binding_id.clone(),
    );
    assert!(result.success, "{:?}", result.output);
    binding_id
}

fn acquire_mcp_app(
    runtime: &ToolRuntime,
    agent_id: &str,
    endpoint_id: &str,
    generation: i64,
    binding_id: &str,
) -> Value {
    let result = runtime.agent_continuation_wake_acquire(
        None,
        agent_id.to_string(),
        endpoint_id.to_string(),
        generation,
        binding_id.to_string(),
    );
    assert!(result.success, "{:?}", result.output);
    result.output
}

fn prepare_mcp_app(
    runtime: &ToolRuntime,
    agent_id: &str,
    endpoint_id: &str,
    generation: i64,
    binding_id: &str,
    wake_id: &str,
    attempt_id: &str,
) -> (Value, String) {
    let result = runtime.agent_continuation_wake_prepare(
        None,
        agent_id.to_string(),
        endpoint_id.to_string(),
        generation,
        binding_id.to_string(),
        wake_id.to_string(),
        attempt_id.to_string(),
    );
    assert!(result.success, "{:?}", result.output);
    let message = result.output["app_protocol"]["automatic_message"]
        .as_str()
        .unwrap()
        .to_string();
    (result.output, message)
}

fn resume_field(message: &str, field: &str) -> String {
    let prefix = format!("{field}=");
    message
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing {field} from continuation message"))
        .to_string()
}

struct McpContinuationFixture {
    _temp: tempfile::TempDir,
    db: Arc<Database>,
    runtime: ToolRuntime,
    sender: String,
    receiver: String,
    sender_endpoint: String,
    sender_generation: i64,
    receiver_endpoint: String,
    receiver_generation: i64,
    conversation_id: String,
}

fn mcp_continuation_fixture(stem: &str) -> McpContinuationFixture {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&temp.path().join(format!("{stem}.db"))).unwrap());
    let runtime = runtime_with_db(db.clone());
    let sender = create_agent(
        &runtime,
        &format!("{stem}-sender"),
        "MCP Sender",
        "PRIVATE sender description",
        "PRIVATE-sender-label",
        &format!("{stem}-sender-create"),
    );
    let receiver = create_agent(
        &runtime,
        &format!("{stem}-receiver"),
        "MCP Receiver",
        "PRIVATE receiver description",
        "PRIVATE-receiver-label",
        &format!("{stem}-receiver-create"),
    );
    let (sender_endpoint, sender_generation) =
        attach(&runtime, &sender, &format!("{stem}-sender-endpoint"));
    let (receiver_endpoint, receiver_generation) =
        attach(&runtime, &receiver, &format!("{stem}-receiver-endpoint"));
    let conversation_id = create_conversation(
        &runtime,
        &sender,
        &receiver,
        &format!("{stem}-conversation"),
    );
    McpContinuationFixture {
        _temp: temp,
        db,
        runtime,
        sender,
        receiver,
        sender_endpoint,
        sender_generation,
        receiver_endpoint,
        receiver_generation,
        conversation_id,
    }
}

fn post_fixture_message(fixture: &McpContinuationFixture, body: &str, key: &str) {
    post_as_agent(
        &fixture.runtime,
        &fixture.conversation_id,
        body,
        &fixture.sender,
        &fixture.sender_endpoint,
        fixture.sender_generation,
        &fixture.receiver,
        Some(key),
        None,
        None,
    );
}

#[test]
fn natural_agent_message_dispatches_once_and_burst_remains_bounded_and_private() {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&temp.path().join("natural.db")).unwrap());
    let runtime = runtime_with_db(db.clone());
    let agent_a = create_agent(
        &runtime,
        "architect",
        "Architect",
        "private architect description",
        "private-architect-label",
        "natural-agent-a",
    );
    let agent_b = create_agent(
        &runtime,
        "reviewer",
        "Reviewer",
        "private reviewer description",
        "private-reviewer-label",
        "natural-agent-b",
    );
    let (endpoint_a, generation_a) = attach(&runtime, &agent_a, "natural-endpoint-a");
    let (endpoint_b, generation_b) = attach(&runtime, &agent_b, "natural-endpoint-b");
    let conversation_id = create_conversation(&runtime, &agent_a, &agent_b, "natural-conversation");
    let adapter = Arc::new(FakeHostAdapter::delivered());
    let registration = runtime.register_agent_continuation_adapter(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        adapter.clone(),
    );
    assert!(registration.success, "{:?}", registration.output);
    assert_eq!(registration.output["endpoint"]["wake_capable"], true);

    let private_body = "private body must stay out of the continuation envelope";
    post_as_agent(
        &runtime,
        &conversation_id,
        private_body,
        &agent_a,
        &endpoint_a,
        generation_a,
        &agent_b,
        Some("natural-message-0"),
        None,
        None,
    );
    wait_until("first continuation dispatch", || {
        adapter.dispatch_count() == 1
    });
    let first_wake_id = wake_id_for(&db, &agent_b);
    let envelope = adapter.latest_envelope();
    assert_eq!(envelope.wake_id, first_wake_id);
    assert_eq!(envelope.agent_id, agent_b);
    assert_eq!(envelope.endpoint_id, endpoint_b);
    assert_eq!(envelope.controller_generation, generation_b);
    let envelope_text = serde_json::to_string(&envelope).unwrap();
    for private in [
        private_body,
        "private reviewer description",
        "private-reviewer-label",
        "natural-message-0",
        "wc_commprincipal_",
    ] {
        assert!(
            !envelope_text.contains(private),
            "envelope leaked {private}"
        );
    }

    for index in 1..50 {
        post_as_agent(
            &runtime,
            &conversation_id,
            &format!("bounded burst message {index}"),
            &agent_a,
            &endpoint_a,
            generation_a,
            &agent_b,
            Some(&format!("natural-message-{index}")),
            None,
            None,
        );
    }
    assert_eq!(count(&db, "wc_conversation_messages"), 50);
    assert_eq!(count(&db, "wc_agent_deliveries"), 50);
    assert!(
        count(&db, "wc_agent_wakes") <= 2,
        "one delivered Wake plus at most one coalesced successor is bounded"
    );
    assert_eq!(
        adapter.dispatch_count(),
        1,
        "an unresolved delivered Wake blocks 49 duplicate model-turn dispatches"
    );

    let bootstrap = runtime.bootstrap_agent_conversation(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        Some(conversation_id),
        Some(first_wake_id.clone()),
        None,
    );
    assert!(bootstrap.success, "{:?}", bootstrap.output);
    assert_eq!(bootstrap.output["host_binding"]["adapter_registered"], true);
    assert_eq!(
        bootstrap.output["host_binding"]["production_auto_resume_available"],
        false
    );
    assert_eq!(
        bootstrap.output["host_binding"]["runtime_wake_capable"],
        true
    );
    assert!(bootstrap.output["selected_conversation"]["conversation_id"].is_string());
    assert!(
        bootstrap.output["inbox"]["queued_delivery_count"]
            .as_i64()
            .unwrap()
            >= 50
    );
    assert!(bootstrap.output.get("messages").is_none());
    assert!(!bootstrap.output.to_string().contains(private_body));

    let unregistered = runtime.unregister_agent_continuation_adapter(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
    );
    assert!(unregistered.success, "{:?}", unregistered.output);
    assert_eq!(unregistered.output["endpoint"]["wake_capable"], false);
    let bootstrap = runtime.bootstrap_agent_conversation(
        None,
        agent_b,
        endpoint_b,
        generation_b,
        None,
        Some(first_wake_id),
        None,
    );
    assert!(bootstrap.success, "{:?}", bootstrap.output);
    assert_eq!(
        bootstrap.output["host_binding"]["adapter_registered"],
        false
    );
    assert_eq!(
        bootstrap.output["host_binding"]["runtime_wake_capable"],
        false
    );
}

#[test]
fn offline_restart_and_replacement_dispatch_the_same_logical_wake() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("restart.db");
    let db = Arc::new(Database::open(&path).unwrap());
    let runtime = runtime_with_db(db.clone());
    let agent_a = create_agent(
        &runtime,
        "sender",
        "Sender",
        "sender description",
        "sender-label",
        "restart-agent-a",
    );
    let agent_b = create_agent(
        &runtime,
        "offline",
        "Offline Agent",
        "offline description",
        "offline-label",
        "restart-agent-b",
    );
    let (endpoint_a, generation_a) = attach(&runtime, &agent_a, "restart-endpoint-a");
    let (endpoint_b, generation_b) = attach(&runtime, &agent_b, "restart-endpoint-b");
    let conversation_id = create_conversation(&runtime, &agent_a, &agent_b, "restart-conversation");
    post_as_agent(
        &runtime,
        &conversation_id,
        "queued while no usable Host adapter exists",
        &agent_a,
        &endpoint_a,
        generation_a,
        &agent_b,
        Some("restart-message"),
        None,
        None,
    );
    let logical_wake_id = wake_id_for(&db, &agent_b);
    assert_eq!(
        db.agent_wake(&logical_wake_id).unwrap().unwrap().state,
        AgentWakeState::Pending
    );

    let recovered_binding = bind_mcp_app(&runtime, &agent_b, &endpoint_b, generation_b);
    let persisted_fingerprint = endpoint_recovery_fingerprint(&db, &endpoint_b)
        .expect("a live MCP App must persist only its restart recovery fingerprint");
    assert_eq!(persisted_fingerprint.len(), 64);
    assert_ne!(persisted_fingerprint, recovered_binding);
    assert_eq!(
        db.agent_wake(&logical_wake_id).unwrap().unwrap().state,
        AgentWakeState::Pending
    );

    drop(runtime);
    drop(db);
    let reopened = Arc::new(Database::open(&path).unwrap());
    let ownership = crate::server_instance::ServerInstanceGuard::acquire(&reopened).unwrap();
    reopened
        .recover_agent_wakes_for_server_takeover(&ownership, chrono::Utc::now().timestamp_millis())
        .unwrap();
    let runtime = runtime_with_db(reopened.clone());
    let bootstrap = runtime.bootstrap_agent_conversation(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        Some(conversation_id.clone()),
        Some(logical_wake_id.clone()),
        None,
    );
    assert!(bootstrap.success, "{:?}", bootstrap.output);
    assert_eq!(bootstrap.output["endpoint"]["wake_capable"], false);
    assert_eq!(
        bootstrap.output["host_binding"]["adapter_registered"],
        false
    );
    assert_eq!(
        bootstrap.output["host_binding"]["production_auto_resume_available"],
        false
    );
    assert_eq!(bootstrap.output["wake"]["wake_id"], logical_wake_id);

    let old_process_registration = runtime.register_agent_continuation_adapter(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        Arc::new(FakeHostAdapter::delivered()),
    );
    assert!(!old_process_registration.success);
    assert_eq!(
        old_process_registration.output["error_kind"], "endpoint_not_attached_in_process",
        "a successor process cannot assume a pre-restart Host callback survived"
    );
    let wrong_binding = format!("wc_host_binding_{}", "b".repeat(32));
    let wrong_state = runtime.agent_continuation_state(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        wrong_binding,
    );
    assert!(!wrong_state.success);
    assert_eq!(wrong_state.output["error_kind"], "host_binding_stale");

    let restart_state = runtime.agent_continuation_state(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        recovered_binding.clone(),
    );
    assert!(restart_state.success, "{:?}", restart_state.output);
    assert_eq!(
        restart_state.output["agent_continuation"]["host_binding"]["bound"],
        false
    );
    assert_eq!(
        restart_state.output["agent_continuation"]["recovery"]["kind"],
        "host_binding_missing_in_process"
    );
    assert_eq!(
        restart_state.output["agent_continuation"]["wake"]["state"],
        "pending"
    );
    assert!(
        reopened
            .agent_wake_attempts(&logical_wake_id)
            .unwrap()
            .is_empty(),
        "a restart recovery observation must not claim or dispatch the pending Wake"
    );

    let stale_generation_state = runtime.agent_continuation_state(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b + 1,
        recovered_binding.clone(),
    );
    assert!(!stale_generation_state.success);
    assert_eq!(
        stale_generation_state.output["error_kind"], "endpoint_generation_stale",
        "restart recovery must not bypass exact controller-generation fencing"
    );
    let old_app_registration = runtime.agent_continuation_bind(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        recovered_binding.clone(),
    );
    assert!(
        old_app_registration.success,
        "{:?}",
        old_app_registration.output
    );
    assert_eq!(
        old_app_registration.output["agent_continuation"]["host_binding"]["bound"],
        true,
        "the exact fingerprint-proven View may recreate only its MCP App process-local binding after takeover"
    );
    let recovered_state = runtime.agent_continuation_state(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        recovered_binding,
    );
    assert!(recovered_state.success, "{:?}", recovered_state.output);
    assert!(recovered_state.output["agent_continuation"]["recovery"].is_null());

    let (replacement_endpoint, replacement_generation) =
        attach(&runtime, &agent_b, "restart-endpoint-b2");
    assert_eq!(replacement_generation, generation_b + 1);
    let replacement_binding = bind_mcp_app(
        &runtime,
        &agent_b,
        &replacement_endpoint,
        replacement_generation,
    );
    let unbound_replacement = runtime.agent_continuation_unbind(
        None,
        agent_b.clone(),
        replacement_endpoint.clone(),
        replacement_generation,
        replacement_binding,
    );
    assert!(
        unbound_replacement.success,
        "{:?}",
        unbound_replacement.output
    );
    assert_eq!(
        reopened.agent_wake(&logical_wake_id).unwrap().unwrap().state,
        AgentWakeState::Pending,
        "fresh replacement App bind/unbind must preserve a pre-fence logical Wake for another eligible carrier"
    );
    let replacement_adapter = Arc::new(FakeHostAdapter::delivered());
    let registration = runtime.register_agent_continuation_adapter(
        None,
        agent_b.clone(),
        replacement_endpoint.clone(),
        replacement_generation,
        replacement_adapter.clone(),
    );
    assert!(registration.success);
    wait_until("replacement continuation dispatch", || {
        replacement_adapter.dispatch_count() == 1
    });
    assert_eq!(
        replacement_adapter.latest_envelope().wake_id,
        logical_wake_id
    );
    let replayed_old = runtime.attach_agent_endpoint(
        None,
        agent_b.clone(),
        "Deterministic Host".to_string(),
        Some("attachment-restart-endpoint-b".to_string()),
        "restart-endpoint-b".to_string(),
    );
    assert!(replayed_old.success, "{:?}", replayed_old.output);
    assert_eq!(replayed_old.output["replayed"], true);
    assert_eq!(replayed_old.output["endpoint"]["endpoint_id"], endpoint_b);
    assert_eq!(
        replayed_old.output["endpoint"]["controller_generation"],
        generation_b
    );
    let stale_registration = runtime.register_agent_continuation_adapter(
        None,
        agent_b.clone(),
        endpoint_b,
        generation_b,
        Arc::new(FakeHostAdapter::delivered()),
    );
    assert!(!stale_registration.success);
    assert_eq!(
        stale_registration.output["error_kind"], "endpoint_expired",
        "the stale generation cannot register itself again"
    );
    let bootstrap = runtime.bootstrap_agent_conversation(
        None,
        agent_b,
        replacement_endpoint,
        replacement_generation,
        Some(conversation_id),
        Some(logical_wake_id),
        None,
    );
    assert!(bootstrap.success, "{:?}", bootstrap.output);
    assert_eq!(
        bootstrap.output["host_binding"]["adapter_registered"], true,
        "a rejected stale registration must not dislodge the current binding"
    );
}

#[test]
fn mcp_app_restart_recovery_fingerprint_fences_replaced_and_unbound_views() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("restart-view-fingerprint.db");
    let db = Arc::new(Database::open(&path).unwrap());
    let runtime = runtime_with_db(db.clone());
    let agent = create_agent(
        &runtime,
        "restart-view-agent",
        "Restart View Agent",
        "restart view description",
        "restart-view-label",
        "restart-view-agent-create",
    );
    let (endpoint, generation) = attach(&runtime, &agent, "restart-view-endpoint");
    let view_a = bind_mcp_app(&runtime, &agent, &endpoint, generation);
    let fingerprint_a = endpoint_recovery_fingerprint(&db, &endpoint).unwrap();
    assert_eq!(fingerprint_a.len(), 64);
    assert_ne!(fingerprint_a, view_a);

    let view_b = bind_mcp_app(&runtime, &agent, &endpoint, generation);
    let fingerprint_b = endpoint_recovery_fingerprint(&db, &endpoint).unwrap();
    assert_ne!(view_a, view_b);
    assert_ne!(
        fingerprint_a, fingerprint_b,
        "replacement must replace durable recovery provenance"
    );
    let stale_before_restart = runtime.agent_continuation_state(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        view_a.clone(),
    );
    assert!(!stale_before_restart.success);
    assert_eq!(
        stale_before_restart.output["error_kind"],
        "host_binding_stale"
    );

    drop(runtime);
    drop(db);
    let reopened = Arc::new(Database::open(&path).unwrap());
    let ownership = crate::server_instance::ServerInstanceGuard::acquire(&reopened).unwrap();
    reopened
        .recover_agent_wakes_for_server_takeover(&ownership, chrono::Utc::now().timestamp_millis())
        .unwrap();
    assert_eq!(
        endpoint_recovery_fingerprint(&reopened, &endpoint),
        Some(fingerprint_b)
    );
    let runtime = runtime_with_db(reopened.clone());

    let stale_a = runtime.agent_continuation_state(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        view_a.clone(),
    );
    assert!(!stale_a.success);
    assert_eq!(stale_a.output["error_kind"], "host_binding_stale");
    let wrong_view = format!("wc_host_binding_{}", "f".repeat(32));
    let wrong = runtime.agent_continuation_state(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        wrong_view,
    );
    assert!(!wrong.success);
    assert_eq!(wrong.output["error_kind"], "host_binding_stale");
    let stale_generation = runtime.agent_continuation_state(
        None,
        agent.clone(),
        endpoint.clone(),
        generation + 1,
        view_b.clone(),
    );
    assert!(!stale_generation.success);
    assert_eq!(
        stale_generation.output["error_kind"],
        "endpoint_generation_stale"
    );

    let current_b = runtime.agent_continuation_state(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        view_b.clone(),
    );
    assert!(current_b.success, "{:?}", current_b.output);
    assert_eq!(
        current_b.output["agent_continuation"]["recovery"]["kind"],
        "host_binding_missing_in_process"
    );
    let rebound = runtime.agent_continuation_bind(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        view_b.clone(),
    );
    assert!(rebound.success, "{:?}", rebound.output);

    let unbound = runtime.agent_continuation_unbind(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        view_b.clone(),
    );
    assert!(unbound.success, "{:?}", unbound.output);
    assert!(endpoint_recovery_fingerprint(&reopened, &endpoint).is_none());
    drop(runtime);
    drop(ownership);
    drop(reopened);

    let reopened = Arc::new(Database::open(&path).unwrap());
    let ownership = crate::server_instance::ServerInstanceGuard::acquire(&reopened).unwrap();
    reopened
        .recover_agent_wakes_for_server_takeover(&ownership, chrono::Utc::now().timestamp_millis())
        .unwrap();
    let runtime = runtime_with_db(reopened);
    for stale_binding in [view_a, view_b] {
        let state = runtime.agent_continuation_state(
            None,
            agent.clone(),
            endpoint.clone(),
            generation,
            stale_binding,
        );
        assert!(!state.success);
        assert_eq!(state.output["error_kind"], "host_binding_stale");
    }
}

#[test]
fn mcp_app_restart_refresh_recovers_only_same_client_window_without_attachment_shortcut() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("restart-refresh-window.db");
    let db = Arc::new(Database::open(&path).unwrap());
    let runtime = runtime_with_db(db.clone());
    let agent = create_agent(
        &runtime,
        "restart-refresh-agent",
        "Restart Refresh Agent",
        "restart refresh description",
        "restart-refresh-label",
        "restart-refresh-agent-create",
    );
    let (endpoint, generation) = attach(&runtime, &agent, "restart-refresh-endpoint");
    let window_a = crate::client_window::ClientWindow::for_test("refresh-window-a");
    let window_b = crate::client_window::ClientWindow::for_test("refresh-window-b");
    let mut old_binding = format!("wc_host_binding_{}", "a".repeat(32));
    let initial = runtime.agent_continuation_bind_for_window(
        None,
        Some(&window_a),
        agent.clone(),
        endpoint.clone(),
        generation,
        old_binding.clone(),
    );
    assert!(initial.success, "{:?}", initial.output);
    assert_eq!(
        endpoint_client_window_key(&db, &endpoint).as_deref(),
        Some(window_a.key())
    );
    assert_ne!(window_a.key(), "refresh-window-a");

    // Once the fresh attachment has established Window provenance, an iframe
    // refresh in the same process may rotate its binding fence, but another
    // Window cannot exploit the still-present attached_endpoints entry.
    let pre_restart_unbind = runtime.agent_continuation_unbind_for_window(
        None,
        Some(&window_a),
        agent.clone(),
        endpoint.clone(),
        generation,
        old_binding.clone(),
    );
    assert!(
        pre_restart_unbind.success,
        "{:?}",
        pre_restart_unbind.output
    );
    assert!(endpoint_recovery_fingerprint(&db, &endpoint).is_none());
    assert_eq!(
        endpoint_client_window_key(&db, &endpoint).as_deref(),
        Some(window_a.key())
    );
    let same_process_stale = runtime.agent_continuation_bind_for_window(
        None,
        Some(&window_b),
        agent.clone(),
        endpoint.clone(),
        generation,
        format!("wc_host_binding_{}", "9".repeat(32)),
    );
    assert!(!same_process_stale.success);
    assert_eq!(
        same_process_stale.output["error_kind"],
        "host_binding_stale"
    );
    old_binding = format!("wc_host_binding_{}", "1".repeat(32));
    let same_process_refresh = runtime.agent_continuation_bind_for_window(
        None,
        Some(&window_a),
        agent.clone(),
        endpoint.clone(),
        generation,
        old_binding.clone(),
    );
    assert!(
        same_process_refresh.success,
        "{:?}",
        same_process_refresh.output
    );
    assert!(endpoint_recovery_fingerprint(&db, &endpoint).is_some());

    drop(runtime);
    drop(db);
    let reopened = Arc::new(Database::open(&path).unwrap());
    let ownership = crate::server_instance::ServerInstanceGuard::acquire(&reopened).unwrap();
    reopened
        .recover_agent_wakes_for_server_takeover(&ownership, chrono::Utc::now().timestamp_millis())
        .unwrap();
    let runtime = runtime_with_db(reopened.clone());

    // Even possession of the exact old iframe fence cannot cross an explicit
    // durable ClientWindow mismatch after takeover.
    let stale_window_state = runtime.agent_continuation_state_for_window(
        None,
        Some(&window_b),
        agent.clone(),
        endpoint.clone(),
        generation,
        old_binding.clone(),
    );
    assert!(!stale_window_state.success);
    assert_eq!(
        stale_window_state.output["error_kind"],
        "host_binding_stale"
    );
    let stale_window_bind = runtime.agent_continuation_bind_for_window(
        None,
        Some(&window_b),
        agent.clone(),
        endpoint.clone(),
        generation,
        old_binding.clone(),
    );
    assert!(!stale_window_bind.success);
    assert_eq!(stale_window_bind.output["error_kind"], "host_binding_stale");

    let restart_state = runtime.agent_continuation_state_for_window(
        None,
        Some(&window_a),
        agent.clone(),
        endpoint.clone(),
        generation,
        old_binding.clone(),
    );
    assert!(restart_state.success, "{:?}", restart_state.output);
    assert_eq!(
        restart_state.output["agent_continuation"]["recovery"]["kind"],
        "host_binding_missing_in_process"
    );
    let rebound = runtime.agent_continuation_bind_for_window(
        None,
        Some(&window_a),
        agent.clone(),
        endpoint.clone(),
        generation,
        old_binding.clone(),
    );
    assert!(rebound.success, "{:?}", rebound.output);

    // A refresh tears down the old iframe. Its exact unbind withdraws only the
    // iframe fence/fingerprint; the canonical Host-window continuity survives.
    let unbound = runtime.agent_continuation_unbind_for_window(
        None,
        Some(&window_a),
        agent.clone(),
        endpoint.clone(),
        generation,
        old_binding,
    );
    assert!(unbound.success, "{:?}", unbound.output);
    assert_eq!(unbound.output["wake_capable"], false);
    assert!(endpoint_recovery_fingerprint(&reopened, &endpoint).is_none());
    assert_eq!(
        endpoint_client_window_key(&reopened, &endpoint).as_deref(),
        Some(window_a.key())
    );

    // Window continuity must not secretly repopulate the process-local fresh
    // attachment registry. Push carriers remain restart-strict.
    let stale_push = runtime.register_agent_continuation_adapter(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        Arc::new(FakeHostAdapter::delivered()),
    );
    assert!(!stale_push.success);
    assert_eq!(
        stale_push.output["error_kind"],
        "endpoint_not_attached_in_process"
    );

    let refreshed_binding = format!("wc_host_binding_{}", "b".repeat(32));
    let refreshed = runtime.agent_continuation_bind_for_window(
        None,
        Some(&window_a),
        agent.clone(),
        endpoint.clone(),
        generation,
        refreshed_binding.clone(),
    );
    assert!(refreshed.success, "{:?}", refreshed.output);

    let foreign_window_state = runtime.agent_continuation_state_for_window(
        None,
        Some(&window_b),
        agent.clone(),
        endpoint.clone(),
        generation,
        refreshed_binding.clone(),
    );
    assert!(!foreign_window_state.success);
    assert_eq!(
        foreign_window_state.output["error_kind"],
        "host_binding_stale"
    );
    let foreign_window_binding = format!("wc_host_binding_{}", "c".repeat(32));
    let foreign_window_bind = runtime.agent_continuation_bind_for_window(
        None,
        Some(&window_b),
        agent.clone(),
        endpoint.clone(),
        generation,
        foreign_window_binding,
    );
    assert!(!foreign_window_bind.success);
    assert_eq!(
        foreign_window_bind.output["error_kind"],
        "host_binding_stale"
    );

    let refreshed_unbind = runtime.agent_continuation_unbind_for_window(
        None,
        Some(&window_a),
        agent.clone(),
        endpoint.clone(),
        generation,
        refreshed_binding,
    );
    assert!(refreshed_unbind.success, "{:?}", refreshed_unbind.output);
    assert!(endpoint_recovery_fingerprint(&reopened, &endpoint).is_none());
    assert_eq!(
        endpoint_client_window_key(&reopened, &endpoint).as_deref(),
        Some(window_a.key())
    );

    // Missing Window metadata does not get the Window-continuity path. With the
    // fingerprint cleared by unbind, a new iframe fence remains stale.
    let no_window_binding = format!("wc_host_binding_{}", "d".repeat(32));
    let no_window = runtime.agent_continuation_bind(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        no_window_binding,
    );
    assert!(!no_window.success);
    assert_eq!(no_window.output["error_kind"], "host_binding_stale");

    // Ordinary Endpoint lifecycle remains authoritative over Window continuity.
    let (_replacement, replacement_generation) =
        attach(&runtime, &agent, "restart-refresh-replacement");
    assert_eq!(replacement_generation, generation + 1);
    assert!(endpoint_client_window_key(&reopened, &endpoint).is_none());
    let expired = runtime.agent_continuation_state_for_window(
        None,
        Some(&window_a),
        agent,
        endpoint,
        generation,
        format!("wc_host_binding_{}", "e".repeat(32)),
    );
    assert!(!expired.success);
    assert_eq!(expired.output["error_kind"], "endpoint_expired");
}

#[test]
fn push_replacement_clears_mcp_app_restart_recovery_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("restart-push-replacement.db");
    let db = Arc::new(Database::open(&path).unwrap());
    let runtime = runtime_with_db(db.clone());
    let agent = create_agent(
        &runtime,
        "restart-push-agent",
        "Restart Push Agent",
        "restart push description",
        "restart-push-label",
        "restart-push-agent-create",
    );
    let (endpoint, generation) = attach(&runtime, &agent, "restart-push-endpoint");
    let view = bind_mcp_app(&runtime, &agent, &endpoint, generation);
    assert!(endpoint_recovery_fingerprint(&db, &endpoint).is_some());

    let push = runtime.register_agent_continuation_adapter(
        None,
        agent.clone(),
        endpoint.clone(),
        generation,
        Arc::new(FakeHostAdapter::delivered()),
    );
    assert!(push.success, "{:?}", push.output);
    assert!(endpoint_recovery_fingerprint(&db, &endpoint).is_none());

    drop(runtime);
    drop(db);
    let reopened = Arc::new(Database::open(&path).unwrap());
    let ownership = crate::server_instance::ServerInstanceGuard::acquire(&reopened).unwrap();
    reopened
        .recover_agent_wakes_for_server_takeover(&ownership, chrono::Utc::now().timestamp_millis())
        .unwrap();
    let runtime = runtime_with_db(reopened);
    let stale_view =
        runtime.agent_continuation_state(None, agent.clone(), endpoint.clone(), generation, view);
    assert!(!stale_view.success);
    assert_eq!(stale_view.output["error_kind"], "host_binding_stale");
    let stale_push = runtime.register_agent_continuation_adapter(
        None,
        agent,
        endpoint,
        generation,
        Arc::new(FakeHostAdapter::delivered()),
    );
    assert!(!stale_push.success);
    assert_eq!(
        stale_push.output["error_kind"], "endpoint_not_attached_in_process",
        "push adapters never gain the MCP App restart recovery path"
    );
}

#[test]
fn mcp_app_restart_recovery_never_bypasses_endpoint_lifecycle() {
    let detached = mcp_continuation_fixture("mcp-recovery-detached");
    let detached_binding = bind_mcp_app(
        &detached.runtime,
        &detached.receiver,
        &detached.receiver_endpoint,
        detached.receiver_generation,
    );
    let detached_result = detached
        .runtime
        .detach_agent_endpoint(None, detached.receiver_endpoint.clone());
    assert!(detached_result.success, "{:?}", detached_result.output);
    assert!(endpoint_recovery_fingerprint(&detached.db, &detached.receiver_endpoint).is_none());
    let detached_state = detached.runtime.agent_continuation_state(
        None,
        detached.receiver.clone(),
        detached.receiver_endpoint.clone(),
        detached.receiver_generation,
        detached_binding,
    );
    assert!(!detached_state.success);
    assert_eq!(detached_state.output["error_kind"], "endpoint_detached");

    let expired = mcp_continuation_fixture("mcp-recovery-expired");
    let expired_binding = bind_mcp_app(
        &expired.runtime,
        &expired.receiver,
        &expired.receiver_endpoint,
        expired.receiver_generation,
    );
    let (_replacement, replacement_generation) = attach(
        &expired.runtime,
        &expired.receiver,
        "mcp-recovery-expired-replacement",
    );
    assert_eq!(replacement_generation, expired.receiver_generation + 1);
    assert!(endpoint_recovery_fingerprint(&expired.db, &expired.receiver_endpoint).is_none());
    let expired_state = expired.runtime.agent_continuation_state(
        None,
        expired.receiver.clone(),
        expired.receiver_endpoint.clone(),
        expired.receiver_generation,
        expired_binding,
    );
    assert!(!expired_state.success);
    assert_eq!(expired_state.output["error_kind"], "endpoint_expired");
}

#[test]
fn mcp_app_restart_recovery_preserves_prepared_delivery_unknown_without_redispatch() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("restart-prepared.db");
    let db = Arc::new(Database::open(&path).unwrap());
    let runtime = runtime_with_db(db.clone());
    let sender = create_agent(
        &runtime,
        "prepared-sender",
        "Prepared Sender",
        "prepared sender description",
        "prepared-sender-label",
        "prepared-sender-create",
    );
    let receiver = create_agent(
        &runtime,
        "prepared-receiver",
        "Prepared Receiver",
        "prepared receiver description",
        "prepared-receiver-label",
        "prepared-receiver-create",
    );
    let (sender_endpoint, sender_generation) =
        attach(&runtime, &sender, "prepared-sender-endpoint");
    let (receiver_endpoint, receiver_generation) =
        attach(&runtime, &receiver, "prepared-receiver-endpoint");
    let conversation = create_conversation(&runtime, &sender, &receiver, "prepared-conversation");
    let binding = bind_mcp_app(&runtime, &receiver, &receiver_endpoint, receiver_generation);
    post_as_agent(
        &runtime,
        &conversation,
        "prepared work",
        &sender,
        &sender_endpoint,
        sender_generation,
        &receiver,
        Some("prepared-message"),
        None,
        None,
    );
    let wake_id = wake_id_for(&db, &receiver);
    let acquired = acquire_mcp_app(
        &runtime,
        &receiver,
        &receiver_endpoint,
        receiver_generation,
        &binding,
    );
    let attempt_id = acquired["wake"]["attempt_id"].as_str().unwrap().to_string();
    let _ = prepare_mcp_app(
        &runtime,
        &receiver,
        &receiver_endpoint,
        receiver_generation,
        &binding,
        &wake_id,
        &attempt_id,
    );
    assert_eq!(
        db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::Prepared
    );

    drop(runtime);
    drop(db);
    let reopened = Arc::new(Database::open(&path).unwrap());
    let ownership = crate::server_instance::ServerInstanceGuard::acquire(&reopened).unwrap();
    reopened
        .recover_agent_wakes_for_server_takeover(&ownership, chrono::Utc::now().timestamp_millis())
        .unwrap();
    assert_eq!(
        reopened.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::DeliveryUnknown
    );
    let runtime = runtime_with_db(reopened.clone());
    let state = runtime.agent_continuation_state(
        None,
        receiver.clone(),
        receiver_endpoint.clone(),
        receiver_generation,
        binding.clone(),
    );
    assert!(state.success, "{:?}", state.output);
    assert_eq!(
        state.output["agent_continuation"]["recovery"]["kind"],
        "host_binding_missing_in_process"
    );
    assert_eq!(
        state.output["agent_continuation"]["wake"]["state"],
        "delivery_unknown"
    );
    let rebound = runtime.agent_continuation_bind(
        None,
        receiver.clone(),
        receiver_endpoint.clone(),
        receiver_generation,
        binding.clone(),
    );
    assert!(rebound.success, "{:?}", rebound.output);
    let acquire_after_restart = runtime.agent_continuation_wake_acquire(
        None,
        receiver,
        receiver_endpoint,
        receiver_generation,
        binding,
    );
    assert!(
        acquire_after_restart.success,
        "{:?}",
        acquire_after_restart.output
    );
    assert!(
        acquire_after_restart.output["wake"].is_null(),
        "delivery_unknown must not create a second Attempt or blindly resend after restart recovery"
    );
    assert_eq!(reopened.agent_wake_attempts(&wake_id).unwrap().len(), 1);
}

#[test]
fn wake_derived_reply_identity_closes_response_loss_without_merging_consumption() {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&temp.path().join("reply-replay.db")).unwrap());
    let runtime = runtime_with_db(db.clone());
    let agent_a = create_agent(
        &runtime,
        "requester",
        "Requester",
        "requester description",
        "requester-label",
        "reply-agent-a",
    );
    let agent_b = create_agent(
        &runtime,
        "responder",
        "Responder",
        "responder description",
        "responder-label",
        "reply-agent-b",
    );
    let (endpoint_a, generation_a) = attach(&runtime, &agent_a, "reply-endpoint-a");
    let (endpoint_b, generation_b) = attach(&runtime, &agent_b, "reply-endpoint-b");
    let conversation_id = create_conversation(&runtime, &agent_a, &agent_b, "reply-conversation");
    post_as_agent(
        &runtime,
        &conversation_id,
        "please review",
        &agent_a,
        &endpoint_a,
        generation_a,
        &agent_b,
        Some("reply-request"),
        None,
        None,
    );
    let wake_id = wake_id_for(&db, &agent_b);

    let pending_bootstrap = runtime.bootstrap_agent_conversation(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        Some(conversation_id.clone()),
        Some(wake_id.clone()),
        None,
    );
    assert!(pending_bootstrap.success, "{:?}", pending_bootstrap.output);
    assert!(pending_bootstrap.output["reply_replay"].is_null());
    let pending_reply = runtime.post_conversation_message(
        None,
        conversation_id.clone(),
        "must activate before using Wake reply identity".to_string(),
        Some(agent_b.clone()),
        Some(endpoint_b.clone()),
        Some(generation_b),
        Some(vec![agent_a.clone()]),
        None,
        None,
        Some(wake_id.clone()),
        Some(0),
    );
    assert!(!pending_reply.success);
    assert_eq!(pending_reply.output["error_kind"], "wake_not_dispatched");

    let activation = runtime.bootstrap_agent_conversation(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        Some(conversation_id.clone()),
        Some(wake_id.clone()),
        Some("reply-activation".to_string()),
    );
    assert!(activation.success, "{:?}", activation.output);
    assert_eq!(activation.output["wake"]["state"], "delivered");
    assert!(activation.output["reply_replay"].is_object());

    let first = post_as_agent(
        &runtime,
        &conversation_id,
        "review completed",
        &agent_b,
        &endpoint_b,
        generation_b,
        &agent_a,
        None,
        Some(&wake_id),
        Some(0),
    );
    assert_eq!(first["replayed"], false);
    post_as_agent(
        &runtime,
        &conversation_id,
        "a second intentional message",
        &agent_b,
        &endpoint_b,
        generation_b,
        &agent_a,
        None,
        Some(&wake_id),
        Some(1),
    );
    let (replacement_endpoint_b, replacement_generation_b) =
        attach(&runtime, &agent_b, "reply-endpoint-b2");
    let retry = post_as_agent(
        &runtime,
        &conversation_id,
        "review completed",
        &agent_b,
        &replacement_endpoint_b,
        replacement_generation_b,
        &agent_a,
        None,
        Some(&wake_id),
        Some(0),
    );
    assert_eq!(retry["replayed"], true);
    assert_eq!(
        retry["message"]["message_id"],
        first["message"]["message_id"]
    );
    assert_eq!(count(&db, "wc_conversation_messages"), 3);

    let changed = runtime.post_conversation_message(
        None,
        conversation_id.clone(),
        "changed replay must conflict".to_string(),
        Some(agent_b.clone()),
        Some(replacement_endpoint_b.clone()),
        Some(replacement_generation_b),
        Some(vec![agent_a.clone()]),
        None,
        None,
        Some(wake_id.clone()),
        Some(0),
    );
    assert!(!changed.success);
    assert_eq!(
        changed.output["error_kind"],
        "communication_idempotency_conflict"
    );
    assert_eq!(count(&db, "wc_conversation_messages"), 3);

    let delivery_id: String = db
        .conn_for_tests()
        .query_row(
            "SELECT delivery_id FROM wc_agent_deliveries
             WHERE recipient_agent_id = ?1 AND state = 'queued'
             ORDER BY delivery_order LIMIT 1",
            [&agent_b],
            |row| row.get(0),
        )
        .unwrap();
    let consumed = runtime.consume_agent_deliveries(
        None,
        agent_b.clone(),
        replacement_endpoint_b,
        replacement_generation_b,
        vec![delivery_id],
    );
    assert!(consumed.success);
    assert_eq!(
        db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::DeliveryUnknown,
        "Delivery consume remains independent from the logical Wake; replacement conservatively fences the delivered activation"
    );
}

#[test]
fn replacing_push_with_mcp_app_orders_same_generation_host_carriers() {
    let fixture = mcp_continuation_fixture("push-app-transition-fence");
    let adapter = Arc::new(BlockingHostAdapter::default());
    let registration = fixture.runtime.register_agent_continuation_adapter(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        adapter.clone(),
    );
    assert!(registration.success, "{:?}", registration.output);

    post_fixture_message(
        &fixture,
        "same-generation carrier replacement work",
        "push-app-transition-message",
    );
    let logical_wake_id = wake_id_for(&fixture.db, &fixture.receiver);
    adapter.wait_until_preflight();

    let runtime = fixture.runtime.clone();
    let receiver = fixture.receiver.clone();
    let endpoint = fixture.receiver_endpoint.clone();
    let generation = fixture.receiver_generation;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = runtime.agent_continuation_bind(
            None,
            receiver,
            endpoint,
            generation,
            format!("wc_host_binding_{}", "b".repeat(32)),
        );
        tx.send(result).unwrap();
    });

    assert!(
        matches!(
            rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "same-generation MCP App replacement must not return while the old push carrier is inside dispatch"
    );

    adapter.release_preflight();
    let bind = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("MCP App replacement should complete after the old push dispatch leaves its transition fence");
    assert!(bind.success, "{:?}", bind.output);
    assert_eq!(adapter.dispatch_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .db
            .agent_wake(&logical_wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::DeliveryUnknown,
        "replacing a carrier after its dispatch accepted path must preserve conservative post-fence uncertainty"
    );

    let binding_id = format!("wc_host_binding_{}", "b".repeat(32));
    let acquired = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding_id,
    );
    assert!(
        acquired["wake"].is_null(),
        "the replacement App must not acquire a second Attempt for the unresolved post-fence Wake"
    );
}

#[test]
fn mcp_app_view_replacement_fences_pre_and_post_dispatch_without_second_lifecycle() {
    let fixture = mcp_continuation_fixture("mcp-view-fence");
    let first_binding = bind_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
    );
    let private_body = "PRIVATE MCP App business message body";
    post_fixture_message(&fixture, private_body, "mcp-view-fence-message");
    let logical_wake_id = wake_id_for(&fixture.db, &fixture.receiver);

    let first = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &first_binding,
    );
    assert_eq!(first["wake"]["wake_id"], logical_wake_id);
    assert_eq!(first["wake"]["state"], "claimed");
    let first_attempt = first["wake"]["attempt_id"].as_str().unwrap().to_string();

    let second_binding = bind_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
    );
    assert_ne!(first_binding, second_binding);
    assert_eq!(
        fixture
            .db
            .agent_wake(&logical_wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::Pending,
        "replacing a pre-fence View must revoke its Attempt and safely recover the Wake"
    );
    let stale_state = fixture.runtime.agent_continuation_state(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        first_binding.clone(),
    );
    assert!(!stale_state.success);
    assert_eq!(stale_state.output["error_kind"], "host_binding_stale");
    let stale_acquire = fixture.runtime.agent_continuation_wake_acquire(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        first_binding.clone(),
    );
    let stale_prepare = fixture.runtime.agent_continuation_wake_prepare(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        first_binding.clone(),
        logical_wake_id.clone(),
        first_attempt.clone(),
    );
    let stale_unbind = fixture.runtime.agent_continuation_unbind(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        first_binding,
    );
    for stale in [stale_acquire, stale_prepare, stale_unbind] {
        assert!(!stale.success);
        assert_eq!(stale.output["error_kind"], "host_binding_stale");
    }
    let current = fixture.runtime.agent_continuation_state(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        second_binding.clone(),
    );
    assert!(
        current.success,
        "old View unbind must not withdraw new View"
    );

    let second = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &second_binding,
    );
    assert_eq!(second["wake"]["wake_id"], logical_wake_id);
    let second_attempt = second["wake"]["attempt_id"].as_str().unwrap().to_string();
    assert_ne!(first_attempt, second_attempt);
    let (_prepared, automatic_message) = prepare_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &second_binding,
        &logical_wake_id,
        &second_attempt,
    );
    assert_eq!(
        resume_field(&automatic_message, "agent_id"),
        fixture.receiver
    );
    assert_eq!(
        resume_field(&automatic_message, "endpoint_id"),
        fixture.receiver_endpoint
    );
    assert_eq!(
        resume_field(&automatic_message, "controller_generation"),
        fixture.receiver_generation.to_string()
    );
    assert_eq!(resume_field(&automatic_message, "wake_id"), logical_wake_id);
    assert!(resume_field(&automatic_message, "consume_token").starts_with("wc_wake_consume_"));
    for private in [
        private_body,
        "PRIVATE receiver description",
        "PRIVATE-receiver-label",
        "claim_fence=",
        "wc_commprincipal_",
    ] {
        assert!(
            !automatic_message.contains(private),
            "automatic continuation message leaked {private}"
        );
    }

    let third_binding = bind_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
    );
    assert_ne!(second_binding, third_binding);
    assert_eq!(
        fixture
            .db
            .agent_wake(&logical_wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::DeliveryUnknown,
        "replacing a post-fence View must preserve conservative dispatch uncertainty"
    );
    let stale_finish = fixture.runtime.agent_continuation_wake_finish(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        second_binding,
        logical_wake_id.clone(),
        second_attempt,
        "dispatch_accepted".to_string(),
    );
    assert!(!stale_finish.success);
    assert_eq!(stale_finish.output["error_kind"], "host_binding_stale");
    let blocked = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &third_binding,
    );
    assert!(
        blocked["wake"].is_null(),
        "an unresolved post-fence Wake must not manufacture a second model-turn Attempt"
    );
}

#[test]
fn mcp_app_consume_ack_race_and_teardown_preserve_exact_wake_semantics() {
    let fixture = mcp_continuation_fixture("mcp-consume-race");
    let binding = bind_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
    );
    post_fixture_message(
        &fixture,
        "work for exact continuation",
        "mcp-consume-race-message",
    );
    let wake_id = wake_id_for(&fixture.db, &fixture.receiver);
    let acquired = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
    );
    let attempt_id = acquired["wake"]["attempt_id"].as_str().unwrap().to_string();
    let (_prepared, automatic_message) = prepare_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
        &wake_id,
        &attempt_id,
    );
    let consume_token = resume_field(&automatic_message, "consume_token");

    let wrong_token = fixture.runtime.consume_agent_wake(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        wake_id.clone(),
        "wc_wake_consume_00000000000000000000000000000000".to_string(),
    );
    assert!(!wrong_token.success);
    let wrong_generation = fixture.runtime.consume_agent_wake(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation + 1,
        wake_id.clone(),
        consume_token.clone(),
    );
    assert!(!wrong_generation.success);
    let wrong_endpoint = fixture.runtime.consume_agent_wake(
        None,
        fixture.receiver.clone(),
        fixture.sender_endpoint.clone(),
        fixture.sender_generation,
        wake_id.clone(),
        consume_token.clone(),
    );
    assert!(!wrong_endpoint.success);

    let consumed = fixture.runtime.consume_agent_wake(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        wake_id.clone(),
        consume_token.clone(),
    );
    assert!(consumed.success, "{:?}", consumed.output);
    assert_eq!(
        fixture.db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::Consumed
    );

    let late_ack = fixture.runtime.agent_continuation_wake_finish(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        binding.clone(),
        wake_id.clone(),
        attempt_id.clone(),
        "dispatch_accepted".to_string(),
    );
    assert!(late_ack.success, "{:?}", late_ack.output);
    assert_eq!(late_ack.output["continuation_consumed"], true);
    assert_eq!(late_ack.output["wake_state"], "consumed");
    assert_eq!(
        late_ack.output["state_changed"], false,
        "a late Host ACK after exact consume is idempotent telemetry, not a new transition"
    );
    let late_ack_retry = fixture.runtime.agent_continuation_wake_finish(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        binding,
        wake_id.clone(),
        attempt_id,
        "dispatch_accepted".to_string(),
    );
    assert!(late_ack_retry.success, "{:?}", late_ack_retry.output);
    assert_eq!(
        late_ack_retry.output["state_changed"], false,
        "repeating the same exact late ACK remains idempotent"
    );
    assert_eq!(
        fixture.db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::Consumed,
        "late/retried Host ACK must never regress a consumed Wake"
    );

    let replay = fixture.runtime.consume_agent_wake(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        wake_id,
        consume_token,
    );
    assert!(replay.success, "{:?}", replay.output);
}

#[test]
fn mcp_app_state_exposes_successive_wakes_after_exact_consume() {
    for message_before_consume in [false, true] {
        let fixture = mcp_continuation_fixture("mcp-successive-wakes");
        let binding = bind_mcp_app(
            &fixture.runtime,
            &fixture.receiver,
            &fixture.receiver_endpoint,
            fixture.receiver_generation,
        );
        post_fixture_message(&fixture, "first work", "first-message");
        let first_wake = wake_id_for(&fixture.db, &fixture.receiver);
        let acquired = acquire_mcp_app(
            &fixture.runtime,
            &fixture.receiver,
            &fixture.receiver_endpoint,
            fixture.receiver_generation,
            &binding,
        );
        let attempt = acquired["wake"]["attempt_id"].as_str().unwrap();
        let (_, message) = prepare_mcp_app(
            &fixture.runtime,
            &fixture.receiver,
            &fixture.receiver_endpoint,
            fixture.receiver_generation,
            &binding,
            &first_wake,
            attempt,
        );
        let state = || {
            let result = fixture.runtime.agent_continuation_state(
                None,
                fixture.receiver.clone(),
                fixture.receiver_endpoint.clone(),
                fixture.receiver_generation,
                binding.clone(),
            );
            assert!(result.success, "{:?}", result.output);
            result.output["agent_continuation"].clone()
        };
        if message_before_consume {
            post_fixture_message(&fixture, "second work", "second-message");
            assert_eq!(state()["wake"]["wake_id"], first_wake);
        }
        let consumed = fixture.runtime.consume_agent_wake(
            None,
            fixture.receiver.clone(),
            fixture.receiver_endpoint.clone(),
            fixture.receiver_generation,
            first_wake.clone(),
            resume_field(&message, "consume_token"),
        );
        assert!(consumed.success, "{:?}", consumed.output);
        if !message_before_consume {
            assert_eq!(state()["dispatch_observation"], "continuation_consumed");
            post_fixture_message(&fixture, "second work", "second-message");
        }

        let next_wake = wake_id_for(&fixture.db, &fixture.receiver);
        assert_ne!(first_wake, next_wake);
        let next = state();
        assert_eq!(next["wake"]["wake_id"], next_wake);
        assert_eq!(next["wake"]["state"], "pending");
        assert!(next["dispatch_observation"].is_null());
        let presented = fixture.runtime.present_agent_continuation(
            None,
            fixture.receiver.clone(),
            fixture.receiver_endpoint.clone(),
            fixture.receiver_generation,
        );
        assert!(presented.success);
        assert_eq!(presented.output["agent_continuation"], next);

        // Observation must preserve the old claim for a consume-before-ACK race.
        let ack = fixture.runtime.agent_continuation_wake_finish(
            None,
            fixture.receiver.clone(),
            fixture.receiver_endpoint.clone(),
            fixture.receiver_generation,
            binding.clone(),
            first_wake,
            attempt.to_string(),
            "dispatch_accepted".to_string(),
        );
        assert!(ack.success, "{:?}", ack.output);
        assert_eq!(ack.output["wake_state"], "consumed");
        let acquired = acquire_mcp_app(
            &fixture.runtime,
            &fixture.receiver,
            &fixture.receiver_endpoint,
            fixture.receiver_generation,
            &binding,
        );
        assert_eq!(acquired["wake"]["wake_id"], next_wake);
        assert_eq!(acquired["wake"]["replayed"], false);
        assert_ne!(acquired["wake"]["attempt_id"], attempt);
        let (_, message) = prepare_mcp_app(
            &fixture.runtime,
            &fixture.receiver,
            &fixture.receiver_endpoint,
            fixture.receiver_generation,
            &binding,
            &next_wake,
            acquired["wake"]["attempt_id"].as_str().unwrap(),
        );
        assert_eq!(resume_field(&message, "wake_id"), next_wake);
    }
}

#[test]
fn mcp_app_post_fence_unbind_is_unknown_but_exact_turn_can_still_consume() {
    let fixture = mcp_continuation_fixture("mcp-unbind-race");
    let binding = bind_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
    );
    post_fixture_message(&fixture, "teardown race work", "mcp-unbind-race-message");
    let wake_id = wake_id_for(&fixture.db, &fixture.receiver);
    let acquired = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
    );
    let attempt_id = acquired["wake"]["attempt_id"].as_str().unwrap().to_string();
    let (_prepared, automatic_message) = prepare_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
        &wake_id,
        &attempt_id,
    );
    let consume_token = resume_field(&automatic_message, "consume_token");
    let unbound = fixture.runtime.agent_continuation_unbind(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        binding,
    );
    assert!(unbound.success, "{:?}", unbound.output);
    assert_eq!(unbound.output["wake_capable"], false);
    assert_eq!(
        fixture.db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::DeliveryUnknown
    );

    let consumed = fixture.runtime.consume_agent_wake(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        wake_id.clone(),
        consume_token,
    );
    assert!(
        consumed.success,
        "a turn already dispatched through mcp_app must remain exactly consumable after View teardown: {:?}",
        consumed.output
    );
    assert_eq!(
        fixture.db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::Consumed
    );
}

#[test]
fn mcp_app_fifty_message_burst_coalesces_to_one_current_attempt() {
    let fixture = mcp_continuation_fixture("mcp-burst");
    let binding = bind_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
    );
    for index in 0..50 {
        post_fixture_message(
            &fixture,
            &format!("durable burst message {index}"),
            &format!("mcp-burst-message-{index}"),
        );
    }
    assert_eq!(count(&fixture.db, "wc_conversation_messages"), 50);
    assert_eq!(count(&fixture.db, "wc_agent_deliveries"), 50);
    assert_eq!(
        count(&fixture.db, "wc_agent_wakes"),
        1,
        "all pending burst deliveries should coalesce into one logical Wake"
    );
    let first = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
    );
    let replay = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
    );
    assert_eq!(first["wake"]["wake_id"], replay["wake"]["wake_id"]);
    assert_eq!(first["wake"]["attempt_id"], replay["wake"]["attempt_id"]);
    assert_eq!(replay["wake"]["replayed"], true);
    assert_eq!(count(&fixture.db, "wc_agent_wakes"), 1);
}

#[test]
fn explicit_activation_bootstrap_is_replayable_and_consumes_wake_separately() {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&temp.path().join("explicit-activation.db")).unwrap());
    let runtime = runtime_with_db(db.clone());
    let agent_a = create_agent(
        &runtime,
        "manual-sender",
        "Manual Sender",
        "sender",
        "sender",
        "manual-agent-a",
    );
    let agent_b = create_agent(
        &runtime,
        "manual-receiver",
        "Manual Receiver",
        "receiver",
        "receiver",
        "manual-agent-b",
    );
    let (endpoint_a, generation_a) = attach(&runtime, &agent_a, "manual-endpoint-a");
    let (endpoint_b, generation_b) = attach(&runtime, &agent_b, "manual-endpoint-b");
    let conversation_id = create_conversation(&runtime, &agent_a, &agent_b, "manual-conversation");
    post_as_agent(
        &runtime,
        &conversation_id,
        "manual activation work",
        &agent_a,
        &endpoint_a,
        generation_a,
        &agent_b,
        Some("manual-message"),
        None,
        None,
    );
    let wake_id = wake_id_for(&db, &agent_b);
    let first = runtime.bootstrap_agent_conversation(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        Some(conversation_id.clone()),
        Some(wake_id.clone()),
        Some("manual-activation-key".to_string()),
    );
    assert!(first.success, "{:?}", first.output);
    assert_eq!(first.output["wake"]["state"], "delivered");
    assert_eq!(first.output["wake_activation"]["state_changed"], true);
    let consume_token = first.output["wake_activation"]["consume_token"]
        .as_str()
        .unwrap()
        .to_string();
    let attempt_id = first.output["wake_activation"]["attempt_id"].clone();

    let replay = runtime.bootstrap_agent_conversation(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        Some(conversation_id),
        Some(wake_id.clone()),
        Some("manual-activation-key".to_string()),
    );
    assert!(replay.success, "{:?}", replay.output);
    assert_eq!(replay.output["wake_activation"]["replayed"], true);
    assert_eq!(replay.output["wake_activation"]["state_changed"], false);
    assert_eq!(replay.output["wake_activation"]["attempt_id"], attempt_id);
    assert_eq!(
        replay.output["wake_activation"]["consume_token"],
        consume_token
    );

    let consumed = runtime.consume_agent_wake(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        wake_id.clone(),
        consume_token,
    );
    assert!(consumed.success, "{:?}", consumed.output);
    let inbox = runtime.list_agent_inbox(
        None,
        agent_b.clone(),
        endpoint_b,
        generation_b,
        Some(0),
        Some(10),
    );
    assert!(inbox.success, "{:?}", inbox.output);
    assert_eq!(
        inbox.output["total_queued_count"], 1,
        "Wake consume must not consume the Delivery"
    );
    assert_eq!(
        db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::Consumed
    );
}

#[test]
fn mcp_app_binding_input_requires_canonical_view_fence() {
    let fixture = mcp_continuation_fixture("mcp-binding-input");
    let valid = format!("wc_host_binding_{}", "a0".repeat(16));
    for invalid in [
        String::new(),
        format!("wc_binding_{}", "a".repeat(32)),
        format!("wc_host_binding_{}", "A".repeat(32)),
        format!("wc_host_binding_{}", "a".repeat(31)),
        format!("wc_host_binding_{}", "a".repeat(33)),
        format!("wc_host_binding_{}", "g".repeat(32)),
        format!("{valid}\n"),
    ] {
        let result = fixture.runtime.agent_continuation_bind(
            None,
            fixture.receiver.clone(),
            fixture.receiver_endpoint.clone(),
            fixture.receiver_generation,
            invalid,
        );
        assert!(!result.success);
        assert_eq!(result.output["error_kind"], "invalid_host_binding_id");
        assert!(
            !fixture
                .runtime
                .agent_continuations
                .as_ref()
                .unwrap()
                .binding_status(
                    &fixture.receiver,
                    &fixture.receiver_endpoint,
                    fixture.receiver_generation,
                )
                .adapter_registered
        );
    }
    let result = fixture.runtime.agent_continuation_bind(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        valid.clone(),
    );
    assert!(result.success);
    let state = fixture.runtime.agent_continuation_state(
        None,
        fixture.receiver,
        fixture.receiver_endpoint,
        fixture.receiver_generation,
        valid,
    );
    assert!(state.success);
    assert_eq!(
        state.output["agent_continuation"]["host_binding"]["bound"],
        true
    );
}

#[test]
fn mcp_app_same_view_bind_retry_preserves_claim_and_every_dispatch_phase() {
    let fixture = mcp_continuation_fixture("mcp-idempotent-bind");
    let binding = bind_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
    );
    let controller = fixture.runtime.agent_continuations.as_ref().unwrap();
    let retry = || {
        let before = controller
            .mcp_app_binding_observation(
                &fixture.receiver,
                &fixture.receiver_endpoint,
                fixture.receiver_generation,
            )
            .unwrap();
        let durable_before = before
            .active_wake_id
            .as_ref()
            .map(|wake| fixture.db.agent_wake(wake).unwrap().unwrap());
        let result = fixture.runtime.agent_continuation_bind(
            None,
            fixture.receiver.clone(),
            fixture.receiver_endpoint.clone(),
            fixture.receiver_generation,
            binding.clone(),
        );
        assert!(result.success);
        assert_eq!(
            result.output["agent_continuation"]["host_binding"]["bound"],
            true
        );
        assert_eq!(
            controller
                .mcp_app_binding_observation(
                    &fixture.receiver,
                    &fixture.receiver_endpoint,
                    fixture.receiver_generation,
                )
                .unwrap(),
            before,
            "same-id renew must preserve the exact active claim and phase"
        );
        if let Some(durable_before) = durable_before {
            assert_eq!(
                fixture
                    .db
                    .agent_wake(&durable_before.wake_id)
                    .unwrap()
                    .unwrap(),
                durable_before
            );
        }
        let capable: bool = fixture
            .db
            .conn_for_tests()
            .query_row(
                "SELECT wake_capable FROM wc_agent_endpoints WHERE endpoint_id = ?1",
                [&fixture.receiver_endpoint],
                |row| row.get(0),
            )
            .unwrap();
        assert!(capable);
    };
    retry();
    post_fixture_message(&fixture, "first work", "idempotent-bind-first");
    let acquired = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
    );
    let wake = acquired["wake"]["wake_id"].as_str().unwrap();
    let attempt = acquired["wake"]["attempt_id"].as_str().unwrap();
    retry(); // Claimed but pre-fence: replacement would revoke this Attempt.
    let replay = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
    );
    assert_eq!(replay["wake"]["attempt_id"], attempt);
    assert_eq!(replay["wake"]["replayed"], true);
    let (_, message) = prepare_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
        wake,
        attempt,
    );
    retry(); // Prepared: replacement would force delivery_unknown.
    let consumed = fixture.runtime.consume_agent_wake(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        wake.to_string(),
        resume_field(&message, "consume_token"),
    );
    assert!(consumed.success);
    retry(); // Consume before ACK must keep the old claim for late finish.
    let ack = fixture.runtime.agent_continuation_wake_finish(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        binding.clone(),
        wake.to_string(),
        attempt.to_string(),
        "dispatch_accepted".to_string(),
    );
    assert!(ack.success);
    retry();
    post_fixture_message(&fixture, "successor work", "idempotent-bind-second");
    let successor = acquire_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
    );
    assert_ne!(successor["wake"]["wake_id"], wake);
    assert_ne!(successor["wake"]["attempt_id"], attempt);
    assert!(successor["wake"]["dispatch_observation"].is_null());
    retry();
    let wake = successor["wake"]["wake_id"].as_str().unwrap();
    let attempt = successor["wake"]["attempt_id"].as_str().unwrap();
    prepare_mcp_app(
        &fixture.runtime,
        &fixture.receiver,
        &fixture.receiver_endpoint,
        fixture.receiver_generation,
        &binding,
        wake,
        attempt,
    );
    let unknown = fixture.runtime.agent_continuation_wake_finish(
        None,
        fixture.receiver.clone(),
        fixture.receiver_endpoint.clone(),
        fixture.receiver_generation,
        binding.clone(),
        wake.to_string(),
        attempt.to_string(),
        "delivery_unknown".to_string(),
    );
    assert!(unknown.success);
    retry();
}
