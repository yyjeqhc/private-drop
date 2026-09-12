use crate::db::{
    AgentEndpointRecord, AgentWakeClaim, AgentWakeEnvelope, AgentWakeRecord, AgentWakeState,
    CommunicationPrincipal, CommunicationStoreError, Database,
};
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContinuationPreflight {
    pub(crate) wake_id: String,
    pub(crate) agent_id: String,
    pub(crate) endpoint_id: String,
    pub(crate) controller_generation: i64,
}

impl From<&AgentWakeClaim> for ContinuationPreflight {
    fn from(claim: &AgentWakeClaim) -> Self {
        Self {
            wake_id: claim.wake.wake_id.clone(),
            agent_id: claim.wake.target_agent_id.clone(),
            endpoint_id: claim.attempt.endpoint_id.clone(),
            controller_generation: claim.attempt.controller_generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContinuationPreflightError {
    pub(crate) kind: &'static str,
}

impl ContinuationPreflightError {
    #[cfg(test)]
    pub(crate) const fn new(kind: &'static str) -> Self {
        Self { kind }
    }
}

// Production variants are returned by Host adapters. The current tree ships
// only deterministic test adapters; keeping this narrow contract is intentional.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContinuationDispatchOutcome {
    Delivered,
    OutcomeUnknown,
}

/// Narrow Host boundary for delivering one already-durable Agent continuation.
///
/// `preflight` runs before the durable dispatch fence and therefore must not
/// resume a model turn. `dispatch` runs only after the Wake Attempt is durably
/// prepared; any non-acknowledged result must be reported as `OutcomeUnknown`
/// rather than silently retried.
pub(crate) trait ContinuationAdapter: Send + Sync {
    fn adapter_kind(&self) -> &'static str;

    /// True only for a demonstrated Host primitive that can request a fresh
    /// production model turn. Deterministic/fake/manual test adapters leave
    /// this false.
    fn production_auto_resume_available(&self) -> bool {
        false
    }

    fn preflight(
        &self,
        continuation: &ContinuationPreflight,
    ) -> Result<(), ContinuationPreflightError>;

    fn dispatch(&self, envelope: &AgentWakeEnvelope) -> ContinuationDispatchOutcome;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentWakeDispatchReport {
    NoPendingWake,
    ReleasedBeforeDispatch {
        wake: AgentWakeRecord,
        adapter_error_kind: &'static str,
    },
    Delivered {
        wake: AgentWakeRecord,
    },
    DeliveryUnknown {
        wake: AgentWakeRecord,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentHostBindingStatus {
    pub(crate) adapter_registered: bool,
    pub(crate) adapter_kind: Option<String>,
    pub(crate) production_auto_resume_available: bool,
}

pub(crate) const MCP_APP_CONTINUATION_ADAPTER_KIND: &str = "mcp_app";
const MCP_APP_BINDING_ID_PREFIX: &str = "wc_host_binding_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpAppDispatchPhase {
    Prepared,
    DispatchAccepted,
    DispatchUnknown,
}

impl McpAppDispatchPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "dispatch_prepared",
            Self::DispatchAccepted => "dispatch_accepted",
            Self::DispatchUnknown => "dispatch_unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpAppHostBindingObservation {
    pub(crate) active_wake_id: Option<String>,
    pub(crate) active_attempt_id: Option<String>,
    pub(crate) dispatch_phase: Option<McpAppDispatchPhase>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpAppWakeAcquisition {
    pub(crate) wake_id: String,
    pub(crate) attempt_id: String,
    pub(crate) wake_state: AgentWakeState,
    pub(crate) wake_revision: i64,
    pub(crate) queued_delivery_count: i64,
    pub(crate) inbox_high_watermark: i64,
    pub(crate) dispatch_phase: Option<McpAppDispatchPhase>,
    pub(crate) replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpAppWakePreparation {
    pub(crate) wake_id: String,
    pub(crate) attempt_id: String,
    pub(crate) wake_revision: i64,
    pub(crate) automatic_message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpAppWakeFinish {
    pub(crate) wake: AgentWakeRecord,
    pub(crate) dispatch_phase: McpAppDispatchPhase,
    pub(crate) state_changed: bool,
}

#[derive(Clone)]
enum EndpointContinuationCarrier {
    Push(Arc<dyn ContinuationAdapter>),
    McpApp {
        binding_id: String,
        active_claim: Option<AgentWakeClaim>,
        dispatch_phase: Option<McpAppDispatchPhase>,
    },
}

impl EndpointContinuationCarrier {
    fn adapter_kind(&self) -> &str {
        match self {
            Self::Push(adapter) => adapter.adapter_kind(),
            Self::McpApp { .. } => MCP_APP_CONTINUATION_ADAPTER_KIND,
        }
    }

    fn production_auto_resume_available(&self) -> bool {
        match self {
            Self::Push(adapter) => adapter.production_auto_resume_available(),
            Self::McpApp { .. } => true,
        }
    }

    fn push_adapter(&self) -> Option<Arc<dyn ContinuationAdapter>> {
        match self {
            Self::Push(adapter) => Some(adapter.clone()),
            Self::McpApp { .. } => None,
        }
    }
}

#[derive(Clone)]
struct EndpointContinuationBinding {
    principal: CommunicationPrincipal,
    agent_id: String,
    endpoint_id: String,
    controller_generation: i64,
    carrier: EndpointContinuationCarrier,
}

struct AgentContinuationControllerState {
    db: Arc<Database>,
    bindings: Mutex<HashMap<String, EndpointContinuationBinding>>,
    /// Serialize process-local Host binding replacement and App coordination
    /// against exact durable Endpoint fencing without becoming durable truth.
    binding_transitions: Mutex<()>,
    /// Exact Endpoints newly attached through this controller's ToolRuntime.
    /// This set is process-local so a successor cannot re-register a callback
    /// against a pre-restart Endpoint without an explicit replacement attach.
    attached_endpoints: Mutex<HashMap<String, (String, i64)>>,
    /// false = queued/running with no later event; true = at least one event
    /// arrived while the current dispatch opportunity was queued/running.
    scheduled_agents: Mutex<HashMap<String, bool>>,
}

/// Process-local Host adapter registry plus bounded event-driven Wake dispatcher.
///
/// Durable truth remains in SQLite. This registry stores only callable adapter
/// handles and exact Endpoint/generation bindings; a new process starts empty.
/// A bounded deduplicating queue reacts to Message commit, adapter registration,
/// and exact Wake consume without permanent polling.
#[derive(Clone)]
pub(crate) struct AgentContinuationController {
    state: Arc<AgentContinuationControllerState>,
    dispatch_tx: mpsc::SyncSender<String>,
}

impl AgentContinuationController {
    const DISPATCH_QUEUE_CAPACITY: usize = crate::db::MAX_DURABLE_AGENTS as usize;

    pub(crate) fn new(db: Arc<Database>) -> Self {
        let state = Arc::new(AgentContinuationControllerState {
            db,
            bindings: Mutex::new(HashMap::new()),
            binding_transitions: Mutex::new(()),
            attached_endpoints: Mutex::new(HashMap::new()),
            scheduled_agents: Mutex::new(HashMap::new()),
        });
        let (dispatch_tx, dispatch_rx) =
            mpsc::sync_channel::<String>(Self::DISPATCH_QUEUE_CAPACITY);
        let worker_state = state.clone();
        std::thread::Builder::new()
            .name("webcodex-agent-continuations".to_string())
            .spawn(move || {
                while let Ok(agent_id) = dispatch_rx.recv() {
                    loop {
                        worker_state.dispatch_one(&agent_id);
                        let mut scheduled = worker_state
                            .scheduled_agents
                            .lock()
                            .expect("Agent continuation schedule mutex poisoned");
                        let run_again = scheduled.get_mut(&agent_id).is_some_and(|dirty| {
                            let run_again = *dirty;
                            *dirty = false;
                            run_again
                        });
                        if !run_again {
                            scheduled.remove(&agent_id);
                            break;
                        }
                    }
                }
            })
            .expect("Agent continuation controller thread must start");
        Self { state, dispatch_tx }
    }

    /// Register a push-style process-local adapter for one exact current Endpoint.
    /// Replacing any exact current Host carrier first withdraws durable wake
    /// capability so claimed work is safely reconciled before the new carrier wins.
    pub(crate) fn register_endpoint_adapter(
        &self,
        principal: CommunicationPrincipal,
        agent_id: String,
        endpoint_id: String,
        controller_generation: i64,
        adapter: Arc<dyn ContinuationAdapter>,
    ) -> Result<AgentEndpointRecord, CommunicationStoreError> {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.verify_registration_target(
            &principal,
            &agent_id,
            &endpoint_id,
            controller_generation,
        )?;
        self.disable_exact_binding_for_replacement(
            &principal,
            &agent_id,
            &endpoint_id,
            controller_generation,
        )?;
        self.state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned")
            .insert(
                agent_id.clone(),
                EndpointContinuationBinding {
                    principal: principal.clone(),
                    agent_id: agent_id.clone(),
                    endpoint_id: endpoint_id.clone(),
                    controller_generation,
                    carrier: EndpointContinuationCarrier::Push(adapter),
                },
            );
        let endpoint = match self.state.db.set_agent_endpoint_wake_capability(
            &principal,
            &agent_id,
            &endpoint_id,
            controller_generation,
            true,
        ) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.remove_exact_binding(&agent_id, &endpoint_id, controller_generation);
                return Err(error);
            }
        };
        self.schedule_agent(&agent_id);
        Ok(endpoint)
    }

    pub(crate) fn unregister_endpoint_adapter(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
    ) -> Result<AgentEndpointRecord, CommunicationStoreError> {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.state.db.verify_current_agent_endpoint(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
        )?;
        let push_is_current = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned")
            .get(agent_id)
            .is_some_and(|binding| {
                binding.endpoint_id == endpoint_id
                    && binding.controller_generation == controller_generation
                    && matches!(binding.carrier, EndpointContinuationCarrier::Push(_))
            });
        if !push_is_current {
            return Err(stale_host_binding());
        }
        let endpoint = self.state.db.set_agent_endpoint_wake_capability(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            false,
        )?;
        self.remove_exact_binding(agent_id, endpoint_id, controller_generation);
        Ok(endpoint)
    }

    /// Bind one live MCP App View as the pull-style Host carrier for an exact
    /// Endpoint generation. The View supplies a stable, process-local fence, not
    /// authority. Same-View response-loss retries renew without replacing claims.
    pub(crate) fn register_mcp_app_binding(
        &self,
        principal: CommunicationPrincipal,
        agent_id: String,
        endpoint_id: String,
        controller_generation: i64,
        binding_id: String,
    ) -> Result<AgentEndpointRecord, CommunicationStoreError> {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.verify_mcp_app_registration_target(
            &principal,
            &agent_id,
            &endpoint_id,
            controller_generation,
        )?;
        if !binding_id
            .strip_prefix(MCP_APP_BINDING_ID_PREFIX)
            .is_some_and(|suffix| {
                suffix.len() == 32
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            return Err(CommunicationStoreError::new(
                "invalid_host_binding_id",
                "binding_id must be wc_host_binding_ followed by 32 lowercase hex characters",
            ));
        }
        let same_view = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned")
            .get(&agent_id)
            .is_some_and(|binding| {
                binding.principal == principal
                    && binding.endpoint_id == endpoint_id
                    && binding.controller_generation == controller_generation
                    && matches!(&binding.carrier, EndpointContinuationCarrier::McpApp {
                        binding_id: current, ..
                    } if current == &binding_id)
            });
        if same_view {
            // Do not withdraw capability, revoke a claim, or reset dispatch phase.
            return Ok(self
                .state
                .db
                .renew_agent_endpoint(&principal, &endpoint_id, controller_generation)?
                .endpoint);
        }
        self.disable_exact_binding_for_replacement(
            &principal,
            &agent_id,
            &endpoint_id,
            controller_generation,
        )?;
        self.state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned")
            .insert(
                agent_id.clone(),
                EndpointContinuationBinding {
                    principal: principal.clone(),
                    agent_id: agent_id.clone(),
                    endpoint_id: endpoint_id.clone(),
                    controller_generation,
                    carrier: EndpointContinuationCarrier::McpApp {
                        binding_id,
                        active_claim: None,
                        dispatch_phase: None,
                    },
                },
            );
        let endpoint = match self.state.db.set_agent_endpoint_wake_capability(
            &principal,
            &agent_id,
            &endpoint_id,
            controller_generation,
            true,
        ) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.remove_exact_binding(&agent_id, &endpoint_id, controller_generation);
                return Err(error);
            }
        };
        Ok(endpoint)
    }

    /// App heartbeat/state path. Renewal is deliberately coupled to exact
    /// process-local binding validation so a stale iframe cannot keep a
    /// replacement Endpoint lease alive.
    pub(crate) fn mcp_app_binding_state(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
        binding_id: &str,
    ) -> Result<(AgentEndpointRecord, McpAppHostBindingObservation), CommunicationStoreError> {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        let observation = self.verify_mcp_app_binding(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            binding_id,
        )?;
        let endpoint = self
            .state
            .db
            .renew_agent_endpoint(principal, endpoint_id, controller_generation)?
            .endpoint;
        Ok((endpoint, observation))
    }

    pub(crate) fn acquire_mcp_app_wake(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
        binding_id: &str,
    ) -> Result<Option<McpAppWakeAcquisition>, CommunicationStoreError> {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.verify_mcp_app_binding(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            binding_id,
        )?;
        self.state
            .db
            .renew_agent_endpoint(principal, endpoint_id, controller_generation)?;

        let existing = {
            let bindings = self
                .state
                .bindings
                .lock()
                .expect("Agent continuation registry mutex poisoned");
            bindings
                .get(agent_id)
                .and_then(|binding| match &binding.carrier {
                    EndpointContinuationCarrier::McpApp {
                        active_claim,
                        dispatch_phase,
                        ..
                    } => active_claim.clone().map(|claim| (claim, *dispatch_phase)),
                    EndpointContinuationCarrier::Push(_) => None,
                })
        };
        if let Some((claim, dispatch_phase)) = existing {
            let pre_fence_expired = dispatch_phase.is_none()
                && claim
                    .wake
                    .claim_lease_expires_at_unix_ms
                    .is_some_and(|expires| expires <= process_now_unix_ms());
            let still_unresolved = if pre_fence_expired {
                false
            } else {
                self.state
                    .db
                    .bootstrap_agent_conversation(
                        principal,
                        agent_id,
                        endpoint_id,
                        controller_generation,
                        None,
                        Some(&claim.wake.wake_id),
                    )?
                    .wake
                    .is_some()
            };
            if still_unresolved {
                return Ok(Some(acquisition_from_claim(&claim, dispatch_phase, true)));
            }
            let mut bindings = self
                .state
                .bindings
                .lock()
                .expect("Agent continuation registry mutex poisoned");
            if let Some(EndpointContinuationBinding {
                carrier:
                    EndpointContinuationCarrier::McpApp {
                        binding_id: current_binding_id,
                        active_claim,
                        dispatch_phase,
                    },
                ..
            }) = bindings.get_mut(agent_id)
            {
                if current_binding_id == binding_id {
                    *active_claim = None;
                    *dispatch_phase = None;
                }
            }
        }

        let Some(claim) = self.state.db.claim_next_agent_wake(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            MCP_APP_CONTINUATION_ADAPTER_KIND,
        )?
        else {
            return Ok(None);
        };
        {
            let mut bindings = self
                .state
                .bindings
                .lock()
                .expect("Agent continuation registry mutex poisoned");
            let binding = bindings.get_mut(agent_id).ok_or_else(stale_host_binding)?;
            match &mut binding.carrier {
                EndpointContinuationCarrier::McpApp {
                    binding_id: current_binding_id,
                    active_claim,
                    dispatch_phase,
                } if current_binding_id == binding_id => {
                    *active_claim = Some(claim.clone());
                    *dispatch_phase = None;
                }
                _ => return Err(stale_host_binding()),
            }
        }
        Ok(Some(acquisition_from_claim(&claim, None, false)))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_mcp_app_wake(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
        binding_id: &str,
        wake_id: &str,
        attempt_id: &str,
    ) -> Result<McpAppWakePreparation, CommunicationStoreError> {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.verify_mcp_app_binding(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            binding_id,
        )?;
        let claim = self.exact_mcp_app_claim(agent_id, binding_id, wake_id, attempt_id)?;
        let phase = self.mcp_app_dispatch_phase(agent_id, binding_id)?;
        if phase.is_some() {
            return Err(CommunicationStoreError::new(
                "wake_dispatch_already_prepared",
                "This MCP App Wake Attempt already crossed the durable dispatch fence; do not request a second Host dispatch",
            ));
        }
        let prepared = self.state.db.prepare_agent_wake_dispatch(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            wake_id,
            attempt_id,
            &claim.claim_fence,
            &claim.consume_token,
        )?;
        self.state.db.verify_agent_wake_dispatch_binding(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            wake_id,
            attempt_id,
            &claim.claim_fence,
        )?;
        self.set_mcp_app_dispatch_phase(
            agent_id,
            binding_id,
            wake_id,
            attempt_id,
            McpAppDispatchPhase::Prepared,
        )?;
        Ok(McpAppWakePreparation {
            wake_id: wake_id.to_string(),
            attempt_id: attempt_id.to_string(),
            wake_revision: prepared.wake.revision,
            automatic_message: prepared.envelope.resume_hint,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_mcp_app_wake(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
        binding_id: &str,
        wake_id: &str,
        attempt_id: &str,
        dispatch_accepted: bool,
    ) -> Result<McpAppWakeFinish, CommunicationStoreError> {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.verify_mcp_app_binding(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            binding_id,
        )?;
        let claim = self.exact_mcp_app_claim(agent_id, binding_id, wake_id, attempt_id)?;
        let Some(previous_phase) = self.mcp_app_dispatch_phase(agent_id, binding_id)? else {
            return Err(CommunicationStoreError::new(
                "wake_not_prepared",
                "MCP App Host delivery outcome requires an Attempt that already crossed the durable dispatch fence",
            ));
        };
        let (wake, dispatch_phase) = if dispatch_accepted {
            (
                self.state.db.complete_agent_wake_delivery(
                    principal,
                    agent_id,
                    endpoint_id,
                    controller_generation,
                    wake_id,
                    attempt_id,
                    &claim.claim_fence,
                )?,
                McpAppDispatchPhase::DispatchAccepted,
            )
        } else {
            (
                self.state.db.mark_agent_wake_delivery_unknown(
                    principal,
                    agent_id,
                    endpoint_id,
                    controller_generation,
                    wake_id,
                    attempt_id,
                    &claim.claim_fence,
                )?,
                McpAppDispatchPhase::DispatchUnknown,
            )
        };
        self.set_mcp_app_dispatch_phase(agent_id, binding_id, wake_id, attempt_id, dispatch_phase)?;
        let state_changed =
            wake.state != AgentWakeState::Consumed && previous_phase != dispatch_phase;
        Ok(McpAppWakeFinish {
            wake,
            dispatch_phase,
            state_changed,
        })
    }

    pub(crate) fn unregister_mcp_app_binding(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
        binding_id: &str,
    ) -> Result<AgentEndpointRecord, CommunicationStoreError> {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.verify_mcp_app_binding(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            binding_id,
        )?;
        let endpoint = self.state.db.set_agent_endpoint_wake_capability(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
            false,
        )?;
        self.remove_exact_mcp_app_binding(agent_id, endpoint_id, controller_generation, binding_id);
        Ok(endpoint)
    }

    /// Fence process-local state after a durable replacement attach. Exact
    /// idempotent attach replay preserves the already-registered binding.
    pub(crate) fn reconcile_attached_endpoint(
        &self,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
    ) {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.state
            .attached_endpoints
            .lock()
            .expect("Agent continuation attachment mutex poisoned")
            .insert(
                agent_id.to_string(),
                (endpoint_id.to_string(), controller_generation),
            );
        let mut bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        if bindings.get(agent_id).is_some_and(|binding| {
            binding.endpoint_id != endpoint_id
                || binding.controller_generation != controller_generation
        }) {
            bindings.remove(agent_id);
        }
    }

    pub(crate) fn endpoint_detached(
        &self,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
    ) {
        let _transition = self
            .state
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        self.remove_exact_binding(agent_id, endpoint_id, controller_generation);
        let mut attached_endpoints = self
            .state
            .attached_endpoints
            .lock()
            .expect("Agent continuation attachment mutex poisoned");
        if attached_endpoints.get(agent_id).is_some_and(
            |(attached_endpoint_id, attached_generation)| {
                attached_endpoint_id == endpoint_id && *attached_generation == controller_generation
            },
        ) {
            attached_endpoints.remove(agent_id);
        }
    }

    pub(crate) fn binding_status(
        &self,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
    ) -> AgentHostBindingStatus {
        let bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        let binding = bindings.get(agent_id).filter(|binding| {
            binding.endpoint_id == endpoint_id
                && binding.controller_generation == controller_generation
        });
        AgentHostBindingStatus {
            adapter_registered: binding.is_some(),
            adapter_kind: binding.map(|binding| binding.carrier.adapter_kind().to_string()),
            production_auto_resume_available: binding
                .is_some_and(|binding| binding.carrier.production_auto_resume_available()),
        }
    }

    pub(crate) fn mcp_app_binding_observation(
        &self,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
    ) -> Option<McpAppHostBindingObservation> {
        let bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        let binding = bindings.get(agent_id)?;
        if binding.endpoint_id != endpoint_id
            || binding.controller_generation != controller_generation
        {
            return None;
        }
        match &binding.carrier {
            EndpointContinuationCarrier::McpApp {
                active_claim,
                dispatch_phase,
                ..
            } => Some(McpAppHostBindingObservation {
                active_wake_id: active_claim
                    .as_ref()
                    .map(|claim| claim.wake.wake_id.clone()),
                active_attempt_id: active_claim
                    .as_ref()
                    .map(|claim| claim.attempt.attempt_id.clone()),
                dispatch_phase: *dispatch_phase,
            }),
            EndpointContinuationCarrier::Push(_) => None,
        }
    }

    pub(crate) fn schedule_agent(&self, agent_id: &str) {
        let mut scheduled = self
            .state
            .scheduled_agents
            .lock()
            .expect("Agent continuation schedule mutex poisoned");
        if let Some(dirty) = scheduled.get_mut(agent_id) {
            *dirty = true;
            return;
        }
        scheduled.insert(agent_id.to_string(), false);
        match self.dispatch_tx.try_send(agent_id.to_string()) {
            Ok(()) => {}
            Err(error) => {
                scheduled.remove(agent_id);
                tracing::warn!(
                    agent_id,
                    error = %error,
                    "Agent continuation dispatch queue is unavailable; durable Wake remains authoritative"
                );
            }
        }
    }

    fn verify_registration_target(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
    ) -> Result<(), CommunicationStoreError> {
        self.state.db.verify_current_agent_endpoint(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
        )?;
        let attached_here = self
            .state
            .attached_endpoints
            .lock()
            .expect("Agent continuation attachment mutex poisoned")
            .get(agent_id)
            .is_some_and(|(attached_endpoint_id, attached_generation)| {
                attached_endpoint_id == endpoint_id && *attached_generation == controller_generation
            });
        if attached_here {
            Ok(())
        } else {
            Err(CommunicationStoreError::new(
                "endpoint_not_attached_in_process",
                "Registering a Host adapter requires a fresh Endpoint attach in this Server process",
            ))
        }
    }

    fn verify_mcp_app_registration_target(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
    ) -> Result<(), CommunicationStoreError> {
        let endpoint = self.state.db.verify_current_agent_endpoint(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
        )?;
        let attached_here = self
            .state
            .attached_endpoints
            .lock()
            .expect("Agent continuation attachment mutex poisoned")
            .get(agent_id)
            .is_some_and(|(attached_endpoint_id, attached_generation)| {
                attached_endpoint_id == endpoint_id && *attached_generation == controller_generation
            });
        if attached_here {
            return Ok(());
        }
        let bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        let exact_mcp_app_binding = bindings.get(agent_id).is_some_and(|binding| {
            binding.principal == *principal
                && binding.endpoint_id == endpoint_id
                && binding.controller_generation == controller_generation
                && matches!(binding.carrier, EndpointContinuationCarrier::McpApp { .. })
        });
        if exact_mcp_app_binding {
            // Preserve same-View response-loss retries and ordinary replacement
            // fencing after a restart-recovered binding has been recreated.
            return Ok(());
        }
        if !endpoint.wake_capable && !bindings.contains_key(agent_id) {
            // Server takeover clears durable wake_capable together with every
            // process-local carrier. An already-open MCP App may then prove the
            // same exact current Endpoint/generation and recreate only its local
            // binding. Push adapters still require a fresh attach in this process.
            return Ok(());
        }
        Err(CommunicationStoreError::new(
            "endpoint_not_attached_in_process",
            "Registering a Host adapter requires a fresh Endpoint attach in this Server process",
        ))
    }

    fn disable_exact_binding_for_replacement(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
    ) -> Result<(), CommunicationStoreError> {
        let exact_exists = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned")
            .get(agent_id)
            .is_some_and(|binding| {
                binding.endpoint_id == endpoint_id
                    && binding.controller_generation == controller_generation
            });
        if exact_exists {
            self.state.db.set_agent_endpoint_wake_capability(
                principal,
                agent_id,
                endpoint_id,
                controller_generation,
                false,
            )?;
            self.remove_exact_binding(agent_id, endpoint_id, controller_generation);
        }
        Ok(())
    }

    fn verify_mcp_app_binding(
        &self,
        principal: &CommunicationPrincipal,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
        binding_id: &str,
    ) -> Result<McpAppHostBindingObservation, CommunicationStoreError> {
        // Durable authorization deliberately runs before process-local probing.
        let endpoint = self.state.db.verify_current_agent_endpoint(
            principal,
            agent_id,
            endpoint_id,
            controller_generation,
        )?;
        let bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        let Some(binding) = bindings.get(agent_id) else {
            return Err(if endpoint.wake_capable {
                stale_host_binding()
            } else {
                missing_process_host_binding()
            });
        };
        if binding.principal != *principal
            || binding.endpoint_id != endpoint_id
            || binding.controller_generation != controller_generation
        {
            return Err(stale_host_binding());
        }
        match &binding.carrier {
            EndpointContinuationCarrier::McpApp {
                binding_id: current_binding_id,
                active_claim,
                dispatch_phase,
            } if current_binding_id == binding_id => Ok(McpAppHostBindingObservation {
                active_wake_id: active_claim
                    .as_ref()
                    .map(|claim| claim.wake.wake_id.clone()),
                active_attempt_id: active_claim
                    .as_ref()
                    .map(|claim| claim.attempt.attempt_id.clone()),
                dispatch_phase: *dispatch_phase,
            }),
            _ => Err(stale_host_binding()),
        }
    }

    fn exact_mcp_app_claim(
        &self,
        agent_id: &str,
        binding_id: &str,
        wake_id: &str,
        attempt_id: &str,
    ) -> Result<AgentWakeClaim, CommunicationStoreError> {
        let bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        let Some(binding) = bindings.get(agent_id) else {
            return Err(stale_host_binding());
        };
        match &binding.carrier {
            EndpointContinuationCarrier::McpApp {
                binding_id: current_binding_id,
                active_claim: Some(claim),
                ..
            } if current_binding_id == binding_id
                && claim.wake.wake_id == wake_id
                && claim.attempt.attempt_id == attempt_id =>
            {
                Ok(claim.clone())
            }
            _ => Err(CommunicationStoreError::new(
                "wake_claim_stale",
                "MCP App Wake claim is stale or belongs to another Host binding",
            )),
        }
    }

    fn mcp_app_dispatch_phase(
        &self,
        agent_id: &str,
        binding_id: &str,
    ) -> Result<Option<McpAppDispatchPhase>, CommunicationStoreError> {
        let bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        match bindings.get(agent_id).map(|binding| &binding.carrier) {
            Some(EndpointContinuationCarrier::McpApp {
                binding_id: current_binding_id,
                dispatch_phase,
                ..
            }) if current_binding_id == binding_id => Ok(*dispatch_phase),
            _ => Err(stale_host_binding()),
        }
    }

    fn set_mcp_app_dispatch_phase(
        &self,
        agent_id: &str,
        binding_id: &str,
        wake_id: &str,
        attempt_id: &str,
        phase: McpAppDispatchPhase,
    ) -> Result<(), CommunicationStoreError> {
        let mut bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        match bindings
            .get_mut(agent_id)
            .map(|binding| &mut binding.carrier)
        {
            Some(EndpointContinuationCarrier::McpApp {
                binding_id: current_binding_id,
                active_claim: Some(claim),
                dispatch_phase,
            }) if current_binding_id == binding_id
                && claim.wake.wake_id == wake_id
                && claim.attempt.attempt_id == attempt_id =>
            {
                *dispatch_phase = Some(phase);
                Ok(())
            }
            _ => Err(stale_host_binding()),
        }
    }

    fn remove_exact_binding(&self, agent_id: &str, endpoint_id: &str, controller_generation: i64) {
        let mut bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        if bindings.get(agent_id).is_some_and(|binding| {
            binding.endpoint_id == endpoint_id
                && binding.controller_generation == controller_generation
        }) {
            bindings.remove(agent_id);
        }
    }

    fn remove_exact_mcp_app_binding(
        &self,
        agent_id: &str,
        endpoint_id: &str,
        controller_generation: i64,
        binding_id: &str,
    ) {
        let mut bindings = self
            .state
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned");
        let remove = bindings.get(agent_id).is_some_and(|binding| {
            binding.endpoint_id == endpoint_id
                && binding.controller_generation == controller_generation
                && matches!(
                    &binding.carrier,
                    EndpointContinuationCarrier::McpApp {
                        binding_id: current_binding_id,
                        ..
                    } if current_binding_id == binding_id
                )
        });
        if remove {
            bindings.remove(agent_id);
        }
    }
}

fn missing_process_host_binding() -> CommunicationStoreError {
    CommunicationStoreError::new(
        "host_binding_missing_in_process",
        "MCP App Host binding is missing from this Server process; the exact current Endpoint generation may rebind",
    )
}

fn stale_host_binding() -> CommunicationStoreError {
    CommunicationStoreError::new(
        "host_binding_stale",
        "MCP App Host binding is stale, replaced, or not current for this exact Endpoint generation",
    )
}

fn process_now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

fn acquisition_from_claim(
    claim: &AgentWakeClaim,
    dispatch_phase: Option<McpAppDispatchPhase>,
    replayed: bool,
) -> McpAppWakeAcquisition {
    McpAppWakeAcquisition {
        wake_id: claim.wake.wake_id.clone(),
        attempt_id: claim.attempt.attempt_id.clone(),
        wake_state: claim.wake.state,
        wake_revision: claim.wake.revision,
        queued_delivery_count: claim.wake.queued_delivery_count_snapshot,
        inbox_high_watermark: claim.wake.inbox_high_watermark,
        dispatch_phase,
        replayed,
    }
}

impl AgentContinuationControllerState {
    fn dispatch_one(&self, agent_id: &str) {
        // Put push dispatch and Host-carrier replacement on one process-local
        // fence. Endpoint generation alone cannot distinguish two carriers for
        // the same exact generation, so an already-cloned old push callback
        // must not run after a replacement App binding re-enables wake_capable.
        let _transition = self
            .binding_transitions
            .lock()
            .expect("Agent continuation binding transition mutex poisoned");
        let binding = self
            .bindings
            .lock()
            .expect("Agent continuation registry mutex poisoned")
            .get(agent_id)
            .cloned();
        let Some(binding) = binding else {
            return;
        };
        let Some(adapter) = binding.carrier.push_adapter() else {
            // MCP App bindings are pull bridges: the View coordinates claim /
            // prepare / Host ui/message / finish. The Server worker must never
            // pretend it can synchronously invoke that Host primitive.
            return;
        };
        let result = dispatch_next_agent_wake(
            &self.db,
            &binding.principal,
            &binding.agent_id,
            &binding.endpoint_id,
            binding.controller_generation,
            adapter.as_ref(),
        );
        match result {
            Ok(AgentWakeDispatchReport::NoPendingWake)
            | Ok(AgentWakeDispatchReport::ReleasedBeforeDispatch { .. })
            | Ok(AgentWakeDispatchReport::Delivered { .. })
            | Ok(AgentWakeDispatchReport::DeliveryUnknown { .. }) => {}
            Err(error)
                if matches!(
                    error.code(),
                    "endpoint_not_found"
                        | "endpoint_expired"
                        | "endpoint_detached"
                        | "endpoint_not_active"
                        | "endpoint_not_wake_capable"
                        | "endpoint_generation_stale"
                        | "endpoint_agent_mismatch"
                ) =>
            {
                let mut bindings = self
                    .bindings
                    .lock()
                    .expect("Agent continuation registry mutex poisoned");
                if bindings.get(agent_id).is_some_and(|current| {
                    current.endpoint_id == binding.endpoint_id
                        && current.controller_generation == binding.controller_generation
                }) {
                    bindings.remove(agent_id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    agent_id,
                    endpoint_id = %binding.endpoint_id,
                    controller_generation = binding.controller_generation,
                    error_kind = error.code(),
                    "Agent continuation dispatch event failed; durable Wake state is preserved"
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_next_agent_wake<A: ContinuationAdapter + ?Sized>(
    db: &Database,
    principal: &CommunicationPrincipal,
    agent_id: &str,
    endpoint_id: &str,
    expected_controller_generation: i64,
    adapter: &A,
) -> Result<AgentWakeDispatchReport, CommunicationStoreError> {
    let Some(claim) = db.claim_next_agent_wake(
        principal,
        agent_id,
        endpoint_id,
        expected_controller_generation,
        adapter.adapter_kind(),
    )?
    else {
        return Ok(AgentWakeDispatchReport::NoPendingWake);
    };

    if let Err(error) = adapter.preflight(&ContinuationPreflight::from(&claim)) {
        let wake = db.release_agent_wake_claim(
            principal,
            agent_id,
            endpoint_id,
            expected_controller_generation,
            &claim.wake.wake_id,
            &claim.attempt.attempt_id,
            &claim.claim_fence,
        )?;
        return Ok(AgentWakeDispatchReport::ReleasedBeforeDispatch {
            wake,
            adapter_error_kind: error.kind,
        });
    }

    let prepared = db.prepare_agent_wake_dispatch(
        principal,
        agent_id,
        endpoint_id,
        expected_controller_generation,
        &claim.wake.wake_id,
        &claim.attempt.attempt_id,
        &claim.claim_fence,
        &claim.consume_token,
    )?;

    db.verify_agent_wake_dispatch_binding(
        principal,
        agent_id,
        endpoint_id,
        expected_controller_generation,
        &claim.wake.wake_id,
        &claim.attempt.attempt_id,
        &claim.claim_fence,
    )?;

    match adapter.dispatch(&prepared.envelope) {
        ContinuationDispatchOutcome::Delivered => {
            let wake = db.complete_agent_wake_delivery(
                principal,
                agent_id,
                endpoint_id,
                expected_controller_generation,
                &claim.wake.wake_id,
                &claim.attempt.attempt_id,
                &claim.claim_fence,
            )?;
            Ok(AgentWakeDispatchReport::Delivered { wake })
        }
        ContinuationDispatchOutcome::OutcomeUnknown => {
            let wake = db.mark_agent_wake_delivery_unknown(
                principal,
                agent_id,
                endpoint_id,
                expected_controller_generation,
                &claim.wake.wake_id,
                &claim.attempt.attempt_id,
                &claim.claim_fence,
            )?;
            Ok(AgentWakeDispatchReport::DeliveryUnknown { wake })
        }
    }
}
