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

    fn unavailable() -> Self {
        Self {
            preflight_error: Some("host_bridge_unavailable"),
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
    let result = runtime.agent_continuation_bind(
        None,
        agent_id.to_string(),
        endpoint_id.to_string(),
        generation,
    );
    assert!(result.success, "{:?}", result.output);
    result.output["_app_private"]["binding_id"]
        .as_str()
        .unwrap()
        .to_string()
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
    let message = result.output["_app_private"]["automatic_message"]
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

    let unavailable = Arc::new(FakeHostAdapter::unavailable());
    let registration = runtime.register_agent_continuation_adapter(
        None,
        agent_b.clone(),
        endpoint_b.clone(),
        generation_b,
        unavailable.clone(),
    );
    assert!(registration.success);
    wait_until("failed preflight", || {
        unavailable.preflight_count.load(Ordering::SeqCst) >= 1
    });
    assert_eq!(unavailable.dispatch_count(), 0);
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
    let old_app_registration =
        runtime.agent_continuation_bind(None, agent_b.clone(), endpoint_b.clone(), generation_b);
    assert!(!old_app_registration.success);
    assert_eq!(
        old_app_registration.output["error_kind"], "endpoint_not_attached_in_process",
        "a successor process cannot resurrect a pre-restart MCP App View from endpoint_id alone"
    );

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
    let (replayed_old_endpoint, replayed_old_generation) =
        attach(&runtime, &agent_b, "restart-endpoint-b");
    assert_eq!(replayed_old_endpoint, endpoint_b);
    assert_eq!(replayed_old_generation, generation_b);
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
        let result = runtime.agent_continuation_bind(None, receiver, endpoint, generation);
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

    let binding_id = bind.output["_app_private"]["binding_id"]
        .as_str()
        .expect("replacement App binding id")
        .to_string();
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
        first_binding,
    );
    assert!(!stale_state.success);
    assert_eq!(stale_state.output["error_kind"], "host_binding_stale");

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
