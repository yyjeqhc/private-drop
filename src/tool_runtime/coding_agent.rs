use super::{RecoveryKind, ToolResult, ToolRuntime};
use crate::auth::{AuthContext, AuthKind};
use crate::runner_http::RunnerFeature;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;
use webcodex_core::coding_agent::{
    merge_coding_agent_run_snapshot, validate_coding_agent_run_snapshot, CodingAgentCancelRequest,
    CodingAgentConfigValue, CodingAgentDispatchState, CodingAgentEvent, CodingAgentExecutionState,
    CodingAgentObservationMerge, CodingAgentObserveRequest, CodingAgentObserveResult,
    CodingAgentRequest, CodingAgentResponse, CodingAgentResponsePayload, CodingAgentRunSnapshot,
    CodingAgentRunState, CodingAgentStartRequest, CodingAgentTerminal,
    CODING_AGENT_MAX_CONFIG_OPTIONS, CODING_AGENT_MAX_EVENTS_PER_RESPONSE,
    CODING_AGENT_MAX_INVENTORY_RUNS, CODING_AGENT_OBSERVE_WAIT_MAX_SECS,
    CODING_AGENT_TIMEOUT_MAX_SECS, CODING_AGENT_TIMEOUT_MIN_SECS,
};

const IDEMPOTENCY_KEY_MAX_BYTES: usize = 256;
const START_RESPONSE_WAIT_SECS: u64 = 32;
const CONTROL_RESPONSE_WAIT_SECS: u64 = 65;
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 300;
const PUBLIC_TOKEN_PREFIX: &str = "wcar2_";
const PUBLIC_TOKEN_MAX_BYTES: usize = 192;
const PUBLIC_TOKEN_EPOCH_BYTES: usize = 32;
const PUBLIC_TOKEN_SEQUENCE_BYTES: usize = 8;
const PUBLIC_TOKEN_TAG_BYTES: usize = 16;
const PUBLIC_TOKEN_PAYLOAD_BYTES: usize =
    PUBLIC_TOKEN_EPOCH_BYTES + PUBLIC_TOKEN_SEQUENCE_BYTES + PUBLIC_TOKEN_TAG_BYTES;
const SERVER_TERMINAL_RETENTION_SECS: i64 = 15 * 60;
const SERVER_MAX_TERMINAL_RUNS: usize = CODING_AGENT_MAX_INVENTORY_RUNS;
const OBSERVATION_MAC_KEY_DIR: &str = "private";
const OBSERVATION_MAC_KEY_FILE: &str = "coding-agent-observation-mac-key-v2";
const OBSERVATION_MAC_KEY_BYTES: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct ServerRunBinding {
    authority_fingerprint: String,
    client_id: String,
    runner_instance_id: String,
    runtime_project_id: String,
    provider_id: String,
    provider_instance_id: String,
    recording_session_id: Option<String>,
    recorded_lifecycle_mask: u8,
    snapshot: CodingAgentRunSnapshot,
}

pub(crate) struct CodingAgentServerState {
    epoch: String,
    observation_mac_key: [u8; OBSERVATION_MAC_KEY_BYTES],
    runs: Mutex<HashMap<String, ServerRunBinding>>,
}

#[derive(Debug, Clone)]
pub(crate) struct CodingAgentPreparedStart {
    pub(crate) run_id: String,
    pub(crate) authority_fingerprint: String,
    pub(crate) runtime_project_id: String,
    pub(crate) provider_id: String,
    pub(crate) provider_instance_id: String,
    pub(crate) intent_fingerprint: String,
    client: crate::runner_protocol::RunnerView,
    project_root: String,
    instruction: String,
    config: BTreeMap<String, CodingAgentConfigValue>,
    timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodingAgentStartCertainty {
    NotStarted,
    OutcomeUnknown,
}

#[derive(Debug, Clone)]
pub(crate) struct CodingAgentStartFailure {
    pub(crate) kind: String,
    pub(crate) message: String,
    pub(crate) certainty: CodingAgentStartCertainty,
    pub(crate) recovery: RecoveryKind,
    pub(crate) run_id: String,
}

impl CodingAgentStartFailure {
    fn into_tool_result(self) -> ToolResult {
        coding_agent_error(
            &self.kind,
            self.message,
            match self.certainty {
                CodingAgentStartCertainty::NotStarted => "not_started",
                CodingAgentStartCertainty::OutcomeUnknown => "outcome_unknown",
            },
            self.recovery,
            Some(&self.run_id),
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CodingAgentTypedStartOutcome {
    Run(CodingAgentRunSnapshot),
    Failure(CodingAgentStartFailure),
}

fn prune_server_runs_locked(runs: &mut HashMap<String, ServerRunBinding>, now: i64) {
    let cutoff = now.saturating_sub(SERVER_TERMINAL_RETENTION_SECS);
    runs.retain(|_, binding| {
        !binding.snapshot.state.terminal() || binding.snapshot.updated_at >= cutoff
    });
    let mut terminals = runs
        .iter()
        .filter(|(_, binding)| binding.snapshot.state.terminal())
        .map(|(run_id, binding)| (run_id.clone(), binding.snapshot.updated_at))
        .collect::<Vec<_>>();
    if terminals.len() <= SERVER_MAX_TERMINAL_RUNS {
        return;
    }
    terminals.sort_by_key(|(_, updated_at)| *updated_at);
    let remove_count = terminals.len().saturating_sub(SERVER_MAX_TERMINAL_RUNS);
    for (run_id, _) in terminals.into_iter().take(remove_count) {
        runs.remove(&run_id);
    }
}

fn run_matches_binding_identity(binding: &ServerRunBinding, run: &CodingAgentRunSnapshot) -> bool {
    run.run_id == binding.snapshot.run_id
        && run.authority_fingerprint == binding.authority_fingerprint
        && run.intent_fingerprint == binding.snapshot.intent_fingerprint
        && run.runtime_project_id == binding.runtime_project_id
        && run.provider_id == binding.provider_id
        && run.provider_instance_id == binding.provider_instance_id
}

fn merge_server_run_binding_locked(
    runs: &mut HashMap<String, ServerRunBinding>,
    mut incoming: ServerRunBinding,
) -> Result<ServerRunBinding, String> {
    let run_id = incoming.snapshot.run_id.clone();
    let Some(existing) = runs.get_mut(&run_id) else {
        validate_coding_agent_run_snapshot(&incoming.snapshot)?;
        runs.insert(run_id, incoming.clone());
        return Ok(incoming);
    };

    let disposition = merge_coding_agent_run_snapshot(&existing.snapshot, &incoming.snapshot)?;
    match disposition {
        CodingAgentObservationMerge::Stale | CodingAgentObservationMerge::ExactReplay => {
            if existing.recording_session_id.is_none() {
                existing.recording_session_id = incoming.recording_session_id.take();
            }
            Ok(existing.clone())
        }
        CodingAgentObservationMerge::Advance => {
            incoming.recording_session_id = existing
                .recording_session_id
                .clone()
                .or(incoming.recording_session_id);
            incoming.recorded_lifecycle_mask = existing.recorded_lifecycle_mask;
            *existing = incoming;
            Ok(existing.clone())
        }
    }
}

impl Default for CodingAgentServerState {
    fn default() -> Self {
        Self::with_observation_mac_key(new_observation_mac_key())
    }
}

impl CodingAgentServerState {
    fn with_observation_mac_key(observation_mac_key: [u8; OBSERVATION_MAC_KEY_BYTES]) -> Self {
        Self {
            epoch: Uuid::new_v4().simple().to_string(),
            observation_mac_key,
            runs: Mutex::new(HashMap::new()),
        }
    }

    fn with_persistent_observation_mac_key(state_dir: &Path) -> Result<Self, String> {
        Ok(Self::with_observation_mac_key(
            load_or_create_observation_mac_key(state_dir)?,
        ))
    }

    fn observation_token(&self, run_id: &str, sequence: u64) -> String {
        observation_token(&self.observation_mac_key, &self.epoch, run_id, sequence)
    }

    fn parse_observation_token(&self, run_id: &str, token: &str) -> Result<u64, TokenError> {
        parse_observation_token(&self.observation_mac_key, &self.epoch, run_id, token)
    }
    async fn bind(
        &self,
        client: &crate::runner_protocol::RunnerView,
        run: CodingAgentRunSnapshot,
        recording_session_id: Option<String>,
    ) -> Result<ServerRunBinding, String> {
        let incoming = ServerRunBinding {
            authority_fingerprint: run.authority_fingerprint.clone(),
            client_id: client.client_id.clone(),
            runner_instance_id: client.runner_instance_id.clone(),
            runtime_project_id: run.runtime_project_id.clone(),
            provider_id: run.provider_id.clone(),
            provider_instance_id: run.provider_instance_id.clone(),
            recording_session_id,
            recorded_lifecycle_mask: 0,
            snapshot: run,
        };
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        merge_server_run_binding_locked(&mut runs, incoming)
    }

    async fn mark_lost(
        &self,
        run_id: &str,
        code: &str,
    ) -> Result<Option<ServerRunBinding>, String> {
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        let Some(current) = runs.get(run_id).cloned() else {
            return Ok(None);
        };
        if current.snapshot.state.terminal() {
            return Ok(Some(current));
        }
        let mut lost = current.clone();
        mark_server_binding_lost(&mut lost, code)?;
        merge_server_run_binding_locked(&mut runs, lost).map(Some)
    }

    async fn merge_bound_snapshot(
        &self,
        binding: &ServerRunBinding,
        run: CodingAgentRunSnapshot,
    ) -> Result<ServerRunBinding, String> {
        let mut incoming = binding.clone();
        incoming.snapshot = run;
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        merge_server_run_binding_locked(&mut runs, incoming)
    }

    async fn attach_recorder(&self, run_id: &str, recording_session_id: Option<String>) {
        let Some(recording_session_id) = recording_session_id else {
            return;
        };
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        if let Some(binding) = runs.get_mut(run_id) {
            if binding.recording_session_id.is_none() {
                binding.recording_session_id = Some(recording_session_id);
            }
        }
    }

    async fn take_lifecycle_evidence(
        &self,
        run_id: &str,
    ) -> Option<(String, CodingAgentRunSnapshot, &'static str)> {
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        let binding = runs.get_mut(run_id)?;
        let session_id = binding.recording_session_id.clone()?;
        let (bit, kind) = match binding.snapshot.state {
            CodingAgentRunState::Starting | CodingAgentRunState::Running => {
                (1, "coding_agent_started")
            }
            CodingAgentRunState::WaitingPermission => (2, "coding_agent_waiting_permission"),
            CodingAgentRunState::Completed
            | CodingAgentRunState::Failed
            | CodingAgentRunState::Cancelled
            | CodingAgentRunState::Lost => (4, "coding_agent_terminal"),
        };
        if binding.recorded_lifecycle_mask & bit != 0 {
            return None;
        }
        binding.recorded_lifecycle_mask |= bit;
        Some((session_id, binding.snapshot.clone(), kind))
    }

    async fn get(&self, run_id: &str) -> Option<ServerRunBinding> {
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        runs.get(run_id).cloned()
    }
}

impl ToolRuntime {
    pub(crate) fn with_persistent_coding_agent_observation_state(
        mut self,
        state_dir: impl AsRef<Path>,
    ) -> Result<Self, String> {
        self.coding_agent_runs = Arc::new(
            CodingAgentServerState::with_persistent_observation_mac_key(state_dir.as_ref())?,
        );
        Ok(self)
    }

    pub(crate) async fn coding_agent_start(
        &self,
        project: String,
        provider_id: String,
        idempotency_key: String,
        instruction: String,
        config: Option<BTreeMap<String, CodingAgentConfigValue>>,
        timeout_secs: Option<u64>,
        recording_session_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let prepared = match self
            .prepare_coding_agent_start(
                project,
                provider_id,
                idempotency_key,
                instruction,
                config,
                timeout_secs,
                auth,
            )
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => return error,
        };
        let run_id = prepared.run_id.clone();
        match self
            .dispatch_prepared_coding_agent_start(prepared, recording_session_id, auth)
            .await
        {
            CodingAgentTypedStartOutcome::Run(run) => ToolResult::ok(start_projection(
                &run,
                self.coding_agent_runs.observation_token(&run_id, 0),
            )),
            CodingAgentTypedStartOutcome::Failure(error) => error.into_tool_result(),
        }
    }

    pub(crate) async fn prepare_coding_agent_start(
        &self,
        project: String,
        provider_id: String,
        idempotency_key: String,
        instruction: String,
        config: Option<BTreeMap<String, CodingAgentConfigValue>>,
        timeout_secs: Option<u64>,
        auth: Option<&AuthContext>,
    ) -> Result<CodingAgentPreparedStart, ToolResult> {
        if let Err(error) = validate_start_input(
            &provider_id,
            &idempotency_key,
            &instruction,
            config.as_ref(),
            timeout_secs,
        ) {
            return Err(coding_agent_error(
                "invalid_coding_agent_start",
                error,
                "not_started",
                RecoveryKind::FixInput,
                None,
            ));
        }
        let principal = stable_principal(auth).map_err(|error| {
            coding_agent_error(
                "coding_agent_identity_unavailable",
                error,
                "not_started",
                RecoveryKind::FixInput,
                None,
            )
        })?;
        let authority_fingerprint = authority_fingerprint(&principal);
        let run_id = deterministic_run_id(&principal, &idempotency_key);
        let resolved = self
            .resolve_project_input_for_auth(&project, auth)
            .await
            .map_err(|error| error.into_tool_result())?;
        if !resolved.config.allow_patch {
            return Err(coding_agent_project_not_writable_result(&run_id));
        }
        let client_id = resolved.config.client_id.as_str();
        let client = match self
            .runner_registry
            .get_runner_semantic_view_for_auth(
                client_id,
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
            )
            .await
        {
            Some(client) if client.view.connected => client,
            _ => {
                return Err(coding_agent_error(
                    "coding_agent_runner_unavailable",
                    "exact Project Runner is offline or unauthorized",
                    "not_started",
                    RecoveryKind::Wait,
                    Some(&run_id),
                ))
            }
        };
        if !client.supports(RunnerFeature::CodingAgentRuns) {
            return Err(coding_agent_error(
                "coding_agent_unsupported",
                "exact Project Runner does not advertise CodingAgentRun",
                "not_started",
                RecoveryKind::Reobserve,
                Some(&run_id),
            ));
        }
        let client = client.view;
        let provider_instance_id = match client
            .coding_agent_providers
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|provider| provider.provider_id == provider_id)
        {
            Some(provider) => provider.provider_instance_id.clone(),
            None => {
                return Err(coding_agent_error(
                    "coding_agent_provider_unavailable",
                    "logical ACP provider is not advertised by the exact Project Runner",
                    "not_started",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                ))
            }
        };
        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_RUN_TIMEOUT_SECS);
        let config = config.unwrap_or_default();
        let intent_fingerprint = intent_fingerprint(
            &resolved.resolved_id,
            &provider_id,
            &instruction,
            &config,
            timeout_secs,
        );
        Ok(CodingAgentPreparedStart {
            run_id,
            authority_fingerprint,
            runtime_project_id: resolved.resolved_id,
            provider_id,
            provider_instance_id,
            intent_fingerprint,
            client,
            project_root: resolved.config.path,
            instruction,
            config,
            timeout_secs,
        })
    }

    pub(crate) async fn dispatch_prepared_coding_agent_start(
        &self,
        prepared: CodingAgentPreparedStart,
        recording_session_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> CodingAgentTypedStartOutcome {
        let existing = match self
            .reconcile_run(&prepared.run_id, &prepared.authority_fingerprint, auth)
            .await
        {
            Ok(existing) => existing,
            Err(message) => {
                return CodingAgentTypedStartOutcome::Failure(CodingAgentStartFailure {
                    kind: "coding_agent_observation_conflict".to_string(),
                    message,
                    certainty: CodingAgentStartCertainty::OutcomeUnknown,
                    recovery: RecoveryKind::Reconcile,
                    run_id: prepared.run_id,
                })
            }
        };
        if let Some(existing) = existing {
            if existing.snapshot.intent_fingerprint != prepared.intent_fingerprint
                || existing.runtime_project_id != prepared.runtime_project_id
            {
                return CodingAgentTypedStartOutcome::Failure(CodingAgentStartFailure {
                    kind: "idempotency_conflict".to_string(),
                    message:
                        "idempotency_key is already bound to a different CodingAgentRun intent"
                            .to_string(),
                    certainty: CodingAgentStartCertainty::NotStarted,
                    recovery: RecoveryKind::FixInput,
                    run_id: prepared.run_id,
                });
            }
            self.coding_agent_runs
                .attach_recorder(&prepared.run_id, recording_session_id)
                .await;
            self.record_coding_agent_lifecycle_if_needed(&prepared.run_id)
                .await;
            return CodingAgentTypedStartOutcome::Run(existing.snapshot);
        }

        let operation = CodingAgentRequest::Start(CodingAgentStartRequest {
            run_id: prepared.run_id.clone(),
            intent_fingerprint: prepared.intent_fingerprint.clone(),
            authority_fingerprint: prepared.authority_fingerprint.clone(),
            runtime_project_id: prepared.runtime_project_id.clone(),
            project_root: prepared.project_root.clone(),
            provider_id: prepared.provider_id.clone(),
            provider_instance_id: prepared.provider_instance_id.clone(),
            instruction: prepared.instruction.clone(),
            config: prepared.config.clone(),
            timeout_secs: prepared.timeout_secs,
        });
        let (request_id, receiver) = match self
            .runner_registry
            .enqueue_coding_agent(
                &prepared.client.client_id,
                &prepared.client.runner_instance_id,
                &prepared.provider_id,
                &prepared.provider_instance_id,
                operation,
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                prepared.authority_fingerprint.clone(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return CodingAgentTypedStartOutcome::Failure(CodingAgentStartFailure {
                    kind: "coding_agent_dispatch_rejected".to_string(),
                    message: error,
                    certainty: CodingAgentStartCertainty::NotStarted,
                    recovery: RecoveryKind::Reobserve,
                    run_id: prepared.run_id,
                })
            }
        };
        let response =
            match tokio::time::timeout(Duration::from_secs(START_RESPONSE_WAIT_SECS), receiver)
                .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(_)) | Err(_) => {
                    return self
                        .start_waiter_lost_typed(
                            &request_id,
                            &prepared.run_id,
                            &prepared.authority_fingerprint,
                            recording_session_id,
                            auth,
                        )
                        .await;
                }
            };
        match response.payload {
            Some(CodingAgentResponsePayload::Start { run }) => {
                if run.authority_fingerprint != prepared.authority_fingerprint
                    || run.intent_fingerprint != prepared.intent_fingerprint
                    || run.runtime_project_id != prepared.runtime_project_id
                    || run.provider_id != prepared.provider_id
                    || run.provider_instance_id != prepared.provider_instance_id
                {
                    return CodingAgentTypedStartOutcome::Failure(CodingAgentStartFailure {
                        kind: "invalid_runner_response".to_string(),
                        message: "Runner returned mismatched CodingAgentRun identity".to_string(),
                        certainty: CodingAgentStartCertainty::OutcomeUnknown,
                        recovery: RecoveryKind::Reconcile,
                        run_id: prepared.run_id,
                    });
                }
                let binding = match self
                    .coding_agent_runs
                    .bind(&prepared.client, run, recording_session_id)
                    .await
                {
                    Ok(binding) => binding,
                    Err(message) => {
                        return CodingAgentTypedStartOutcome::Failure(CodingAgentStartFailure {
                            kind: "coding_agent_observation_conflict".to_string(),
                            message,
                            certainty: CodingAgentStartCertainty::OutcomeUnknown,
                            recovery: RecoveryKind::Reconcile,
                            run_id: prepared.run_id,
                        })
                    }
                };
                self.record_coding_agent_lifecycle_if_needed(&prepared.run_id)
                    .await;
                CodingAgentTypedStartOutcome::Run(binding.snapshot)
            }
            _ => CodingAgentTypedStartOutcome::Failure(coding_agent_start_failure_from_response(
                response,
                &prepared.run_id,
            )),
        }
    }

    pub(crate) async fn reconcile_coding_agent_run_snapshot(
        &self,
        run_id: &str,
        auth: Option<&AuthContext>,
    ) -> Result<Option<CodingAgentRunSnapshot>, ToolResult> {
        let principal = stable_principal(auth).map_err(|error| {
            coding_agent_error(
                "coding_agent_identity_unavailable",
                error,
                "not_started",
                RecoveryKind::FixInput,
                Some(run_id),
            )
        })?;
        let authority = authority_fingerprint(&principal);
        let binding = self
            .reconcile_run(run_id, &authority, auth)
            .await
            .map_err(|error| {
                coding_agent_error(
                    "coding_agent_observation_conflict",
                    error,
                    "outcome_unknown",
                    RecoveryKind::Reconcile,
                    Some(run_id),
                )
            })?;
        Ok(binding
            .filter(|binding| binding.authority_fingerprint == authority)
            .map(|binding| binding.snapshot))
    }

    pub(crate) async fn coding_agent_observe(
        &self,
        run_id: String,
        after_observation_token: Option<String>,
        wait_secs: Option<u64>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let wait_secs = wait_secs
            .unwrap_or(0)
            .min(CODING_AGENT_OBSERVE_WAIT_MAX_SECS);
        let authority = match stable_principal(auth) {
            Ok(principal) => authority_fingerprint(&principal),
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_identity_unavailable",
                    error,
                    "not_started",
                    RecoveryKind::FixInput,
                    Some(&run_id),
                )
            }
        };
        let binding = match self.reconcile_run(&run_id, &authority, auth).await {
            Ok(Some(binding)) => binding,
            Ok(None) => {
                return coding_agent_error(
                    "unknown_coding_agent_run",
                    "CodingAgentRun is not visible to this caller",
                    "not_started",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                )
            }
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_observation_conflict",
                    error,
                    "outcome_unknown",
                    RecoveryKind::Reconcile,
                    Some(&run_id),
                )
            }
        };
        if binding.authority_fingerprint != authority {
            return coding_agent_error(
                "unknown_coding_agent_run",
                "CodingAgentRun is not visible to this caller",
                "not_started",
                RecoveryKind::Reobserve,
                Some(&run_id),
            );
        }
        self.record_coding_agent_lifecycle_if_needed(&run_id).await;
        let (after_sequence, token_reset) = match after_observation_token.as_deref() {
            None => (None, false),
            Some(token) => {
                match self
                    .coding_agent_runs
                    .parse_observation_token(&run_id, token)
                {
                    Ok(sequence) => (Some(sequence), false),
                    Err(TokenError::StaleEpoch) => (None, true),
                    Err(TokenError::Invalid) => {
                        return coding_agent_error(
                            "invalid_observation_token",
                            "observation token is invalid or belongs to another Run",
                            "not_started",
                            RecoveryKind::FixInput,
                            Some(&run_id),
                        )
                    }
                }
            }
        };
        if binding.snapshot.state.terminal()
            && binding.runner_instance_id
                != self
                    .current_agent_instance(&binding.client_id, auth)
                    .await
                    .unwrap_or_default()
        {
            return ToolResult::ok(observe_projection(
                CodingAgentObserveResult {
                    run: binding.snapshot.clone(),
                    events: Vec::new(),
                    first_retained_sequence: 1,
                    next_sequence: after_sequence.unwrap_or(0),
                    has_more: false,
                    history_lost: true,
                },
                self.coding_agent_runs.as_ref(),
                token_reset,
            ));
        }
        let operation = CodingAgentRequest::Observe(CodingAgentObserveRequest {
            run_id: run_id.clone(),
            after_sequence,
            limit: CODING_AGENT_MAX_EVENTS_PER_RESPONSE,
            wait_secs,
        });
        let (request_id, receiver) = match self
            .runner_registry
            .enqueue_coding_agent(
                &binding.client_id,
                &binding.runner_instance_id,
                &binding.provider_id,
                &binding.provider_instance_id,
                operation,
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                authority.clone(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if binding.snapshot.state.terminal() {
                    return ToolResult::ok(observe_projection(
                        CodingAgentObserveResult {
                            run: binding.snapshot.clone(),
                            events: Vec::new(),
                            first_retained_sequence: 1,
                            next_sequence: after_sequence.unwrap_or(0),
                            has_more: false,
                            history_lost: true,
                        },
                        self.coding_agent_runs.as_ref(),
                        true,
                    ));
                }
                return coding_agent_error(
                    "coding_agent_runner_unavailable",
                    error,
                    "outcome_unknown",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                );
            }
        };
        let response =
            match tokio::time::timeout(Duration::from_secs(CONTROL_RESPONSE_WAIT_SECS), receiver)
                .await
            {
                Ok(Ok(response)) => response,
                _ => {
                    let _ = self
                        .runner_registry
                        .cancel_request_dispatch_state(&request_id)
                        .await;
                    return coding_agent_error(
                        "coding_agent_observe_timeout",
                        "timed out waiting for bounded CodingAgentRun observation",
                        "outcome_unknown",
                        RecoveryKind::Reobserve,
                        Some(&run_id),
                    );
                }
            };
        match response.payload {
            Some(CodingAgentResponsePayload::Observe { mut observation }) => {
                if !run_matches_binding_identity(&binding, &observation.run) {
                    return coding_agent_error(
                        "invalid_runner_response",
                        "Runner returned mismatched CodingAgentRun identity",
                        "outcome_unknown",
                        RecoveryKind::Reconcile,
                        Some(&run_id),
                    );
                }
                if token_reset {
                    observation.history_lost = true;
                }
                let client = match self
                    .runner_registry
                    .get_runner_view_for_auth(
                        &binding.client_id,
                        crate::runner_http::runner_access_from_auth(auth).as_ref(),
                    )
                    .await
                {
                    Some(client) if client.runner_instance_id == binding.runner_instance_id => {
                        client
                    }
                    Some(_) => {
                        return coding_agent_error(
                            "invalid_runner_response",
                            "owning Runner instance changed while observation was in flight",
                            "outcome_unknown",
                            RecoveryKind::Reconcile,
                            Some(&run_id),
                        )
                    }
                    None => {
                        return coding_agent_error(
                            "coding_agent_runner_unavailable",
                            "exact Runner became unavailable",
                            "outcome_unknown",
                            RecoveryKind::Reobserve,
                            Some(&run_id),
                        )
                    }
                };
                let merged = match self
                    .coding_agent_runs
                    .bind(&client, observation.run.clone(), None)
                    .await
                {
                    Ok(binding) => binding,
                    Err(error) => {
                        return coding_agent_error(
                            "coding_agent_observation_conflict",
                            error,
                            "outcome_unknown",
                            RecoveryKind::Reconcile,
                            Some(&run_id),
                        )
                    }
                };
                observation.run = merged.snapshot;
                self.record_coding_agent_lifecycle_if_needed(&run_id).await;
                ToolResult::ok(observe_projection(
                    observation,
                    self.coding_agent_runs.as_ref(),
                    token_reset,
                ))
            }
            _ => response_to_tool_error(response, Some(&run_id)),
        }
    }

    pub(crate) async fn coding_agent_cancel(
        &self,
        run_id: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let authority = match stable_principal(auth) {
            Ok(principal) => authority_fingerprint(&principal),
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_identity_unavailable",
                    error,
                    "not_started",
                    RecoveryKind::FixInput,
                    Some(&run_id),
                )
            }
        };
        let binding = match self.reconcile_run(&run_id, &authority, auth).await {
            Ok(Some(binding)) => binding,
            Ok(None) => {
                return coding_agent_error(
                    "unknown_coding_agent_run",
                    "CodingAgentRun is not visible to this caller",
                    "not_started",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                )
            }
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_observation_conflict",
                    error,
                    "outcome_unknown",
                    RecoveryKind::Reconcile,
                    Some(&run_id),
                )
            }
        };
        self.record_coding_agent_lifecycle_if_needed(&run_id).await;
        if binding.snapshot.state.terminal() {
            return ToolResult::ok(cancel_projection(&binding.snapshot));
        }
        let operation = CodingAgentRequest::Cancel(CodingAgentCancelRequest {
            run_id: run_id.clone(),
        });
        let (request_id, receiver) = match self
            .runner_registry
            .enqueue_coding_agent(
                &binding.client_id,
                &binding.runner_instance_id,
                &binding.provider_id,
                &binding.provider_instance_id,
                operation,
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                authority.clone(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_cancel_unavailable",
                    error,
                    "outcome_unknown",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                )
            }
        };
        let response =
            match tokio::time::timeout(Duration::from_secs(START_RESPONSE_WAIT_SECS), receiver)
                .await
            {
                Ok(Ok(response)) => response,
                _ => {
                    let _ = self
                        .runner_registry
                        .cancel_request_dispatch_state(&request_id)
                        .await;
                    return coding_agent_error(
                        "coding_agent_cancel_timeout",
                        "cancel outcome is not yet authoritative; observe the same Run",
                        "outcome_unknown",
                        RecoveryKind::Reobserve,
                        Some(&run_id),
                    );
                }
            };
        match response.payload {
            Some(CodingAgentResponsePayload::Cancel { run }) => {
                if !run_matches_binding_identity(&binding, &run) {
                    return coding_agent_error(
                        "invalid_runner_response",
                        "Runner returned mismatched CodingAgentRun identity",
                        "outcome_unknown",
                        RecoveryKind::Reconcile,
                        Some(&run_id),
                    );
                }
                if let Some(client) = self
                    .runner_registry
                    .get_runner_view_for_auth(
                        &binding.client_id,
                        crate::runner_http::runner_access_from_auth(auth).as_ref(),
                    )
                    .await
                {
                    if client.runner_instance_id != binding.runner_instance_id {
                        return coding_agent_error(
                            "invalid_runner_response",
                            "owning Runner instance changed while cancellation was in flight",
                            "outcome_unknown",
                            RecoveryKind::Reconcile,
                            Some(&run_id),
                        );
                    }
                }
                let merged = match self
                    .coding_agent_runs
                    .merge_bound_snapshot(&binding, run)
                    .await
                {
                    Ok(binding) => binding,
                    Err(error) => {
                        return coding_agent_error(
                            "coding_agent_observation_conflict",
                            error,
                            "outcome_unknown",
                            RecoveryKind::Reconcile,
                            Some(&run_id),
                        )
                    }
                };
                self.record_coding_agent_lifecycle_if_needed(&run_id).await;
                ToolResult::ok(cancel_projection(&merged.snapshot))
            }
            _ => response_to_tool_error(response, Some(&run_id)),
        }
    }

    async fn start_waiter_lost_typed(
        &self,
        request_id: &str,
        run_id: &str,
        authority: &str,
        recording_session_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> CodingAgentTypedStartOutcome {
        let dispatched = self
            .runner_registry
            .cancel_request_dispatch_state(request_id)
            .await;
        match self.reconcile_run(run_id, authority, auth).await {
            Ok(Some(binding)) => {
                self.coding_agent_runs
                    .attach_recorder(run_id, recording_session_id)
                    .await;
                self.record_coding_agent_lifecycle_if_needed(run_id).await;
                return CodingAgentTypedStartOutcome::Run(binding.snapshot);
            }
            Ok(None) => {}
            Err(message) => {
                return CodingAgentTypedStartOutcome::Failure(CodingAgentStartFailure {
                    kind: "coding_agent_observation_conflict".to_string(),
                    message,
                    certainty: CodingAgentStartCertainty::OutcomeUnknown,
                    recovery: RecoveryKind::Reconcile,
                    run_id: run_id.to_string(),
                })
            }
        }
        match dispatched {
            Some(false) => CodingAgentTypedStartOutcome::Failure(CodingAgentStartFailure {
                kind: "coding_agent_start_timeout".to_string(),
                message: "Run admission timed out before Runner dispatch".to_string(),
                certainty: CodingAgentStartCertainty::NotStarted,
                recovery: RecoveryKind::RetrySame,
                run_id: run_id.to_string(),
            }),
            Some(true) | None => CodingAgentTypedStartOutcome::Failure(CodingAgentStartFailure {
                kind: "coding_agent_start_outcome_unknown".to_string(),
                message: "Run dispatch may have reached the Runner; do not use a new idempotency key, reobserve/retry the same initiation".to_string(),
                certainty: CodingAgentStartCertainty::OutcomeUnknown,
                recovery: RecoveryKind::Reconcile,
                run_id: run_id.to_string(),
            }),
        }
    }

    async fn record_coding_agent_lifecycle_if_needed(&self, run_id: &str) {
        let Some((session_id, snapshot, kind)) =
            self.coding_agent_runs.take_lifecycle_evidence(run_id).await
        else {
            return;
        };
        self.sessions.record_coding_agent_lifecycle_evidence(
            &session_id,
            &snapshot.runtime_project_id,
            &snapshot.run_id,
            &snapshot.provider_id,
            kind,
            state_name(&snapshot.state),
            execution_name(snapshot.execution_state),
            snapshot
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.stop_reason.as_deref()),
            snapshot
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.error_code.as_deref()),
        );
    }

    async fn reconcile_run(
        &self,
        run_id: &str,
        authority: &str,
        auth: Option<&AuthContext>,
    ) -> Result<Option<ServerRunBinding>, String> {
        let existing = self.coding_agent_runs.get(run_id).await;
        if let Some(binding) = existing {
            if binding.authority_fingerprint != authority {
                return Ok(None);
            }
            // Once the Server has a binding, only the exact bound client may
            // refresh it. Another visible Runner advertising the same run_id is
            // not evidence about this Run and must not force a false retarget/lost.
            if let Some((client, run)) = self
                .runner_registry
                .coding_agent_run_for_runner_for_auth(
                    crate::runner_http::runner_access_from_auth(auth).as_ref(),
                    &binding.client_id,
                    run_id,
                )
                .await
            {
                if run.authority_fingerprint != authority {
                    return Ok(None);
                }
                let identity_changed = !run_matches_binding_identity(&binding, &run);
                let runner_replaced_while_active = client.runner_instance_id
                    != binding.runner_instance_id
                    && !run.state.terminal();
                if identity_changed || runner_replaced_while_active {
                    let code = if identity_changed {
                        "coding_agent_identity_changed_uncertain"
                    } else {
                        "runner_replaced_uncertain"
                    };
                    return self.coding_agent_runs.mark_lost(run_id, code).await;
                }
                return self
                    .coding_agent_runs
                    .bind(&client, run, None)
                    .await
                    .map(Some);
            }

            if binding.snapshot.state.terminal() {
                return Ok(Some(binding));
            }

            // A temporary disconnect of the same Runner is not proof of loss: keep
            // the active projection so callers get wait/reobserve semantics. A live
            // replacement instance, however, is a positive fence crossing. If that
            // replacement does not advertise the durable Run, the old prompt may have
            // executed and P1 must close it `lost` rather than retrying blindly.
            if let Some(current) = self
                .runner_registry
                .get_runner_view_for_auth(
                    &binding.client_id,
                    crate::runner_http::runner_access_from_auth(auth).as_ref(),
                )
                .await
            {
                let instance_replaced = current.runner_instance_id != binding.runner_instance_id;
                let provider_replaced = !instance_replaced
                    && current
                        .coding_agent_providers
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .all(|provider| {
                            provider.provider_instance_id != binding.provider_instance_id
                        });
                if instance_replaced || provider_replaced {
                    let code = if instance_replaced {
                        "runner_replaced_uncertain"
                    } else {
                        "provider_replaced_uncertain"
                    };
                    return self.coding_agent_runs.mark_lost(run_id, code).await;
                }
            }
            return Ok(self.coding_agent_runs.get(run_id).await);
        }

        // After a Server restart there is no process-local binding. Recover only
        // from a unique visible Runner inventory match; the registry fails closed
        // on duplicate run ids instead of choosing by iteration order.
        let Some((client, run)) = self
            .runner_registry
            .coding_agent_run_for_auth(
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                run_id,
            )
            .await
        else {
            return Ok(None);
        };
        if run.authority_fingerprint != authority {
            return Ok(None);
        }
        self.coding_agent_runs
            .bind(&client, run, None)
            .await
            .map(Some)
    }

    async fn current_agent_instance(
        &self,
        client_id: &str,
        auth: Option<&AuthContext>,
    ) -> Option<String> {
        self.runner_registry
            .get_runner_view_for_auth(
                client_id,
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
            )
            .await
            .map(|client| client.runner_instance_id)
    }
}

fn mark_server_binding_lost(binding: &mut ServerRunBinding, code: &str) -> Result<(), String> {
    if binding.snapshot.state.terminal() {
        return Ok(());
    }
    let next_revision = binding
        .snapshot
        .observation_revision
        .checked_add(1)
        .ok_or_else(|| "CodingAgentRun observation revision is exhausted".to_string())?;
    let completed_at = chrono::Utc::now().timestamp();
    binding.snapshot.state = CodingAgentRunState::Lost;
    binding.snapshot.execution_state = CodingAgentExecutionState::OutcomeUnknown;
    binding.snapshot.updated_at = completed_at;
    binding.snapshot.observation_revision = next_revision;
    binding.snapshot.terminal = Some(CodingAgentTerminal {
        stop_reason: None,
        error_code: Some(code.to_string()),
        message: Some(
            "owning Runner/provider instance was replaced while prompt outcome was uncertain; do not redispatch"
                .to_string(),
        ),
        completed_at,
    });
    Ok(())
}

fn validate_start_input(
    provider_id: &str,
    idempotency_key: &str,
    instruction: &str,
    config: Option<&BTreeMap<String, CodingAgentConfigValue>>,
    timeout_secs: Option<u64>,
) -> Result<(), String> {
    webcodex_core::coding_agent::validate_provider_id(provider_id)?;
    if idempotency_key.is_empty()
        || idempotency_key.len() > IDEMPOTENCY_KEY_MAX_BYTES
        || idempotency_key.contains(['\0', '\r', '\n'])
    {
        return Err(format!(
            "idempotency_key must contain 1..={IDEMPOTENCY_KEY_MAX_BYTES} bytes and no NUL/CR/LF"
        ));
    }
    if instruction.is_empty()
        || instruction.len() > webcodex_core::coding_agent::CODING_AGENT_MAX_INSTRUCTION_BYTES
        || instruction.contains('\0')
    {
        return Err("instruction is empty, too large, or contains NUL".to_string());
    }
    if config.is_some_and(|config| config.len() > CODING_AGENT_MAX_CONFIG_OPTIONS) {
        return Err("too many CodingAgentRun config overrides".to_string());
    }
    if let Some(timeout) = timeout_secs {
        if !(CODING_AGENT_TIMEOUT_MIN_SECS..=CODING_AGENT_TIMEOUT_MAX_SECS).contains(&timeout) {
            return Err("timeout_secs is outside the supported range".to_string());
        }
    }
    Ok(())
}

fn stable_principal(auth: Option<&AuthContext>) -> Result<String, String> {
    let Some(auth) = auth else {
        return Ok("local-dev:local-dev".to_string());
    };
    if auth.kind == AuthKind::Bootstrap || auth.is_bootstrap {
        return Ok("bootstrap:server-bootstrap".to_string());
    }
    if auth.is_oauth_shared_key_subject() || auth.is_shared_key() {
        let shared_key_hash = auth
            .shared_key_hash
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "shared-key authority has no stable CodingAgentRun group identity".to_string()
            })?;
        return Ok(format!("shared-key-group:{shared_key_hash}"));
    }
    if auth.is_oauth_project_subject() || auth.is_project_credential() || auth.is_agent_token() {
        let project_grant_id = auth
            .project_grant_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "project-grant authority has no stable CodingAgentRun grant identity".to_string()
            })?;
        return Ok(format!("project-grant:{project_grant_id}"));
    }
    if auth.is_open_anonymous() {
        return Ok("open-anonymous:open-anonymous".to_string());
    }
    if matches!(
        auth.kind,
        AuthKind::ApiToken | AuthKind::AccountCredential | AuthKind::OAuth2Token
    ) {
        let user_id = auth
            .user_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "managed authority has no stable CodingAgentRun user identity".to_string()
            })?;
        return Ok(format!("managed-user:{user_id}"));
    }
    Err("authenticated credential has no canonical CodingAgentRun authority identity".to_string())
}

fn authority_fingerprint(principal: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-coding-agent-authority-v1\0");
    hasher.update(principal.as_bytes());
    format!("auth_{:x}", hasher.finalize())
}

fn deterministic_run_id(principal: &str, key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-coding-agent-run-v1\0");
    hasher.update(principal.as_bytes());
    hasher.update(b"\0");
    hasher.update(key.as_bytes());
    format!("wc_agent_run_{:x}", hasher.finalize())
}

fn intent_fingerprint(
    project: &str,
    provider: &str,
    instruction: &str,
    config: &BTreeMap<String, CodingAgentConfigValue>,
    timeout_secs: u64,
) -> String {
    let canonical = serde_json::to_vec(&json!({
        "project": project,
        "provider": provider,
        "instruction": instruction,
        "config": config,
        "timeout_secs": timeout_secs,
    }))
    .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-coding-agent-intent-v1\0");
    hasher.update(&canonical);
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenError {
    Invalid,
    StaleEpoch,
}

fn new_observation_mac_key() -> [u8; OBSERVATION_MAC_KEY_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.coding-agent.observation.mac-key.v2\0");
    hasher.update(Uuid::new_v4().as_bytes());
    hasher.update(Uuid::new_v4().as_bytes());
    hasher.finalize().into()
}

fn read_observation_mac_key(path: &Path) -> Result<[u8; OBSERVATION_MAC_KEY_BYTES], String> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| format!("cannot read CodingAgent observation MAC key: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect CodingAgent observation MAC key: {error}"))?;
    if !metadata.is_file() {
        return Err("CodingAgent observation MAC key is not a regular file".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("CodingAgent observation MAC key permissions are not private".to_string());
        }
    }
    let mut bytes = Vec::with_capacity(OBSERVATION_MAC_KEY_BYTES);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read CodingAgent observation MAC key: {error}"))?;
    bytes
        .try_into()
        .map_err(|_| "CodingAgent observation MAC key has an invalid persisted length".to_string())
}

fn load_or_create_observation_mac_key(
    state_dir: &Path,
) -> Result<[u8; OBSERVATION_MAC_KEY_BYTES], String> {
    let private_dir = state_dir.join(OBSERVATION_MAC_KEY_DIR);
    fs::create_dir_all(&private_dir)
        .map_err(|error| format!("cannot create CodingAgent private state directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&private_dir, fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!("cannot protect CodingAgent private state directory: {error}")
        })?;
    }
    let path = private_dir.join(OBSERVATION_MAC_KEY_FILE);
    match read_observation_mac_key(&path) {
        Ok(key) => return Ok(key),
        Err(_) if !path.exists() => {}
        Err(error) => return Err(error),
    }

    let key = new_observation_mac_key();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            if let Err(error) = file.write_all(&key).and_then(|_| file.sync_all()) {
                let _ = fs::remove_file(&path);
                return Err(format!(
                    "cannot persist CodingAgent observation MAC key: {error}"
                ));
            }
            Ok(key)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => read_observation_mac_key(&path),
        Err(error) => Err(format!(
            "cannot create CodingAgent observation MAC key: {error}"
        )),
    }
}

fn observation_token_hmac(
    key: &[u8; OBSERVATION_MAC_KEY_BYTES],
    domain: &[u8],
    epoch: &str,
    run_id: &str,
    extra: &[u8],
) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut ipad = [0x36u8; BLOCK_BYTES];
    let mut opad = [0x5cu8; BLOCK_BYTES];
    for (index, byte) in key.iter().enumerate() {
        ipad[index] ^= byte;
        opad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(domain);
    inner.update((epoch.len() as u64).to_be_bytes());
    inner.update(epoch.as_bytes());
    inner.update((run_id.len() as u64).to_be_bytes());
    inner.update(run_id.as_bytes());
    inner.update((extra.len() as u64).to_be_bytes());
    inner.update(extra);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn observation_token(
    key: &[u8; OBSERVATION_MAC_KEY_BYTES],
    epoch: &str,
    run_id: &str,
    sequence: u64,
) -> String {
    debug_assert_eq!(epoch.len(), PUBLIC_TOKEN_EPOCH_BYTES);
    let mask = observation_token_hmac(
        key,
        b"webcodex.coding-agent.observation.sequence-mask.v2\0",
        epoch,
        run_id,
        &[],
    );
    let sequence = sequence.to_be_bytes();
    let mut masked_sequence = [0_u8; PUBLIC_TOKEN_SEQUENCE_BYTES];
    for (index, byte) in sequence.iter().enumerate() {
        masked_sequence[index] = byte ^ mask[index];
    }
    let tag = observation_token_hmac(
        key,
        b"webcodex.coding-agent.observation.tag.v2\0",
        epoch,
        run_id,
        &masked_sequence,
    );
    let mut payload = Vec::with_capacity(PUBLIC_TOKEN_PAYLOAD_BYTES);
    payload.extend_from_slice(epoch.as_bytes());
    payload.extend_from_slice(&masked_sequence);
    payload.extend_from_slice(&tag[..PUBLIC_TOKEN_TAG_BYTES]);
    let token = format!("{PUBLIC_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(payload));
    debug_assert!(token.len() <= PUBLIC_TOKEN_MAX_BYTES);
    token
}

fn parse_observation_token(
    key: &[u8; OBSERVATION_MAC_KEY_BYTES],
    epoch: &str,
    run_id: &str,
    token: &str,
) -> Result<u64, TokenError> {
    if token.len() > PUBLIC_TOKEN_MAX_BYTES {
        return Err(TokenError::Invalid);
    }
    let encoded = token
        .strip_prefix(PUBLIC_TOKEN_PREFIX)
        .ok_or(TokenError::Invalid)?;
    if encoded.is_empty() || !encoded.is_ascii() {
        return Err(TokenError::Invalid);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(encoded.as_bytes())
        .map_err(|_| TokenError::Invalid)?;
    if payload.len() != PUBLIC_TOKEN_PAYLOAD_BYTES {
        return Err(TokenError::Invalid);
    }
    let token_epoch = std::str::from_utf8(&payload[..PUBLIC_TOKEN_EPOCH_BYTES])
        .map_err(|_| TokenError::Invalid)?;
    if token_epoch.len() != PUBLIC_TOKEN_EPOCH_BYTES
        || !token_epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(TokenError::Invalid);
    }
    let masked_start = PUBLIC_TOKEN_EPOCH_BYTES;
    let masked_end = masked_start + PUBLIC_TOKEN_SEQUENCE_BYTES;
    let masked_sequence: [u8; PUBLIC_TOKEN_SEQUENCE_BYTES] = payload[masked_start..masked_end]
        .try_into()
        .map_err(|_| TokenError::Invalid)?;
    let expected_tag = observation_token_hmac(
        key,
        b"webcodex.coding-agent.observation.tag.v2\0",
        token_epoch,
        run_id,
        &masked_sequence,
    );
    if !crate::config::constant_time_eq(
        &payload[masked_end..],
        &expected_tag[..PUBLIC_TOKEN_TAG_BYTES],
    ) {
        return Err(TokenError::Invalid);
    }
    if token_epoch != epoch {
        return Err(TokenError::StaleEpoch);
    }
    let mask = observation_token_hmac(
        key,
        b"webcodex.coding-agent.observation.sequence-mask.v2\0",
        token_epoch,
        run_id,
        &[],
    );
    let mut sequence = [0_u8; PUBLIC_TOKEN_SEQUENCE_BYTES];
    for (index, byte) in masked_sequence.iter().enumerate() {
        sequence[index] = byte ^ mask[index];
    }
    Ok(u64::from_be_bytes(sequence))
}

fn start_projection(run: &CodingAgentRunSnapshot, token: String) -> Value {
    json!({
        "run_id": run.run_id,
        "project": run.runtime_project_id,
        "provider_id": run.provider_id,
        "state": state_name(&run.state),
        "execution_state": execution_name(run.execution_state),
        "observation_token": token,
        "terminal": terminal_projection(run),
    })
}

fn cancel_projection(run: &CodingAgentRunSnapshot) -> Value {
    json!({
        "run_id": run.run_id,
        "project": run.runtime_project_id,
        "provider_id": run.provider_id,
        "state": state_name(&run.state),
        "execution_state": execution_name(run.execution_state),
        "cancel_requested": !run.state.terminal(),
        "terminal": terminal_projection(run),
    })
}

fn observe_projection(
    observation: CodingAgentObserveResult,
    token_state: &CodingAgentServerState,
    reset: bool,
) -> Value {
    let run = &observation.run;
    let token = token_state.observation_token(&run.run_id, observation.next_sequence);
    let events = observation
        .events
        .iter()
        .map(event_projection)
        .collect::<Vec<_>>();
    json!({
        "run_id": run.run_id,
        "project": run.runtime_project_id,
        "provider_id": run.provider_id,
        "state": state_name(&run.state),
        "execution_state": execution_name(run.execution_state),
        "events": events,
        "observation_token": token,
        "has_more": observation.has_more,
        "history_lost": observation.history_lost || reset,
        "first_retained_sequence": observation.first_retained_sequence,
        "terminal": terminal_projection(run),
        "recovery_kind": run_recovery_kind(run),
    })
}

fn event_projection(event: &CodingAgentEvent) -> Value {
    json!({
        "sequence": event.sequence,
        "kind": event.kind.as_str(),
        "text": event.text,
        "label": event.label,
        "status": event.status,
        "usage": event.usage,
    })
}

fn terminal_projection(run: &CodingAgentRunSnapshot) -> Value {
    run.terminal
        .as_ref()
        .map(|terminal| {
            json!({
                "stop_reason": terminal.stop_reason,
                "error_code": terminal.error_code,
                "message": terminal.message,
                "completed_at": terminal.completed_at,
            })
        })
        .unwrap_or(Value::Null)
}

fn state_name(state: &CodingAgentRunState) -> &'static str {
    match state {
        CodingAgentRunState::Starting => "starting",
        CodingAgentRunState::Running => "running",
        CodingAgentRunState::WaitingPermission => "waiting_permission",
        CodingAgentRunState::Completed => "completed",
        CodingAgentRunState::Failed => "failed",
        CodingAgentRunState::Cancelled => "cancelled",
        CodingAgentRunState::Lost => "lost",
    }
}

fn execution_name(state: CodingAgentExecutionState) -> &'static str {
    match state {
        CodingAgentExecutionState::NotStarted => "not_started",
        CodingAgentExecutionState::Started => "started",
        CodingAgentExecutionState::OutcomeUnknown => "outcome_unknown",
        CodingAgentExecutionState::Completed => "completed",
    }
}

fn run_recovery_kind(run: &CodingAgentRunSnapshot) -> &'static str {
    match run.state {
        CodingAgentRunState::Starting | CodingAgentRunState::Running => "reobserve",
        CodingAgentRunState::WaitingPermission => "wait",
        CodingAgentRunState::Lost => "reconcile",
        CodingAgentRunState::Completed
        | CodingAgentRunState::Failed
        | CodingAgentRunState::Cancelled => "none",
    }
}

fn coding_agent_project_not_writable_result(run_id: &str) -> ToolResult {
    ToolResult::err_with_output(
        "coding_agent_start requires a Project with allow_patch=true",
        json!({
            "error_kind": "coding_agent_project_not_writable",
            "failure_kind": "policy_rejected",
            "run_id": run_id,
            "state_changed": false,
            "execution_state": "not_started",
        }),
    )
    .with_recovery(RecoveryKind::UserAction, None)
}

fn coding_agent_error(
    kind: &str,
    message: impl Into<String>,
    execution_state: &str,
    recovery: RecoveryKind,
    run_id: Option<&str>,
) -> ToolResult {
    ToolResult::err_with_output(
        message.into(),
        json!({
            "error_kind": kind,
            "run_id": run_id,
            "execution_state": execution_state,
        }),
    )
    .with_recovery(recovery, None)
}

fn coding_agent_start_failure_from_response(
    response: CodingAgentResponse,
    run_id: &str,
) -> CodingAgentStartFailure {
    let dispatch = response.dispatch_state;
    let certainty = if dispatch == CodingAgentDispatchState::NotStarted {
        CodingAgentStartCertainty::NotStarted
    } else {
        CodingAgentStartCertainty::OutcomeUnknown
    };
    let Some(error) = response.error else {
        return CodingAgentStartFailure {
            kind: "invalid_runner_response".to_string(),
            message: "Runner CodingAgentRun response contained no result".to_string(),
            certainty,
            recovery: RecoveryKind::Reobserve,
            run_id: run_id.to_string(),
        };
    };
    let recovery = match error.recovery_kind.as_deref() {
        Some("fix_input") => RecoveryKind::FixInput,
        Some("retry_same") => RecoveryKind::RetrySame,
        Some("reconcile") => RecoveryKind::Reconcile,
        Some("wait") => RecoveryKind::Wait,
        Some("user_action") => RecoveryKind::UserAction,
        Some("none") => RecoveryKind::NoAction,
        _ => RecoveryKind::Reobserve,
    };
    CodingAgentStartFailure {
        kind: error.code,
        message: error.message,
        certainty,
        recovery,
        run_id: run_id.to_string(),
    }
}

fn response_to_tool_error(response: CodingAgentResponse, run_id: Option<&str>) -> ToolResult {
    let dispatch = response.dispatch_state;
    let Some(error) = response.error else {
        return coding_agent_error(
            "invalid_runner_response",
            "Runner CodingAgentRun response contained no result",
            if dispatch == CodingAgentDispatchState::NotStarted {
                "not_started"
            } else {
                "outcome_unknown"
            },
            RecoveryKind::Reobserve,
            run_id,
        );
    };
    let recovery = match error.recovery_kind.as_deref() {
        Some("fix_input") => RecoveryKind::FixInput,
        Some("retry_same") => RecoveryKind::RetrySame,
        Some("reconcile") => RecoveryKind::Reconcile,
        Some("wait") => RecoveryKind::Wait,
        Some("user_action") => RecoveryKind::UserAction,
        Some("none") => RecoveryKind::NoAction,
        _ => RecoveryKind::Reobserve,
    };
    coding_agent_error(
        &error.code,
        error.message,
        if dispatch == CodingAgentDispatchState::NotStarted {
            "not_started"
        } else {
            "outcome_unknown"
        },
        recovery,
        run_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server_binding(
        run_id: String,
        state: CodingAgentRunState,
        updated_at: i64,
    ) -> ServerRunBinding {
        let terminal = state.terminal().then(|| CodingAgentTerminal {
            stop_reason: Some("end_turn".to_string()),
            error_code: None,
            message: None,
            completed_at: updated_at,
        });
        ServerRunBinding {
            authority_fingerprint: "auth_test".to_string(),
            client_id: "client".to_string(),
            runner_instance_id: "instance".to_string(),
            runtime_project_id: "agent:test:demo".to_string(),
            provider_id: "codex".to_string(),
            provider_instance_id: "provider".to_string(),
            recording_session_id: None,
            recorded_lifecycle_mask: 0,
            snapshot: CodingAgentRunSnapshot {
                run_id,
                intent_fingerprint: "fingerprint".to_string(),
                authority_fingerprint: "auth_test".to_string(),
                runtime_project_id: "agent:test:demo".to_string(),
                provider_id: "codex".to_string(),
                provider_instance_id: "provider".to_string(),
                state,
                execution_state: if terminal.is_some() {
                    CodingAgentExecutionState::Completed
                } else {
                    CodingAgentExecutionState::Started
                },
                observation_revision: 0,
                created_at: updated_at,
                updated_at,
                terminal,
            },
        }
    }

    fn test_shell_client() -> crate::runner_protocol::RunnerView {
        crate::runner_protocol::RunnerView {
            client_id: "client".to_string(),
            runner_instance_id: "instance".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            status: "online".to_string(),
            connected: true,
            last_seen: 0,
            capabilities: Default::default(),
            coding_agent_providers: None,
            pending_requests: 0,
            projects: Vec::new(),
            project_inventory: None,
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            transport: "websocket".to_string(),
            policy: None,
            registered_at: 0,
            connected_at: 0,
            disconnected_at: None,
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
        }
    }

    #[tokio::test]
    async fn server_run_binding_rejects_out_of_order_and_conflicting_observations() {
        let state = CodingAgentServerState::default();
        let client = test_shell_client();
        let run_id = "wc_agent_run_monotonic";
        let now = chrono::Utc::now().timestamp();

        let mut running =
            test_server_binding(run_id.to_string(), CodingAgentRunState::Running, now).snapshot;
        running.observation_revision = 1;
        state.bind(&client, running.clone(), None).await.unwrap();

        let mut completed =
            test_server_binding(run_id.to_string(), CodingAgentRunState::Completed, now).snapshot;
        completed.observation_revision = 2;
        let accepted = state.bind(&client, completed.clone(), None).await.unwrap();
        assert_eq!(accepted.snapshot, completed);

        let stale = state.bind(&client, running.clone(), None).await.unwrap();
        assert_eq!(stale.snapshot, completed);
        let replay = state.bind(&client, completed.clone(), None).await.unwrap();
        assert_eq!(replay.snapshot, completed);

        let mut same_revision_conflict = running.clone();
        same_revision_conflict.observation_revision = 2;
        same_revision_conflict.updated_at = now;
        assert!(state
            .bind(&client, same_revision_conflict, None)
            .await
            .is_err());
        assert_eq!(state.get(run_id).await.unwrap().snapshot, completed);

        let mut terminal_regression = running;
        terminal_regression.observation_revision = 3;
        terminal_regression.updated_at = now;
        assert!(state
            .bind(&client, terminal_regression, None)
            .await
            .is_err());
        let lost_run_id = "wc_agent_run_lost_recovery";
        let mut lost =
            test_server_binding(lost_run_id.to_string(), CodingAgentRunState::Running, now)
                .snapshot;
        lost.state = CodingAgentRunState::Lost;
        lost.execution_state = CodingAgentExecutionState::OutcomeUnknown;
        lost.observation_revision = 2;
        lost.terminal = Some(CodingAgentTerminal {
            stop_reason: None,
            error_code: Some("coding_agent_transport_lost".to_string()),
            message: Some("outcome is uncertain".to_string()),
            completed_at: now,
        });
        state.bind(&client, lost, None).await.unwrap();

        let mut recovered =
            test_server_binding(lost_run_id.to_string(), CodingAgentRunState::Completed, now)
                .snapshot;
        recovered.observation_revision = 4;
        let accepted = state.bind(&client, recovered.clone(), None).await.unwrap();
        assert_eq!(accepted.snapshot, recovered);
        assert_eq!(
            state.get(lost_run_id).await.unwrap().snapshot.state,
            CodingAgentRunState::Completed
        );

        assert_eq!(state.get(run_id).await.unwrap().snapshot, completed);
    }

    #[test]
    fn server_run_registry_prunes_expired_and_bounds_recent_terminals() {
        let now = 10_000;
        let mut runs = HashMap::new();
        runs.insert(
            "wc_agent_run_active".to_string(),
            test_server_binding(
                "wc_agent_run_active".to_string(),
                CodingAgentRunState::Running,
                1,
            ),
        );
        runs.insert(
            "wc_agent_run_expired".to_string(),
            test_server_binding(
                "wc_agent_run_expired".to_string(),
                CodingAgentRunState::Completed,
                now - SERVER_TERMINAL_RETENTION_SECS - 1,
            ),
        );
        for index in 0..SERVER_MAX_TERMINAL_RUNS + 2 {
            let run_id = format!("wc_agent_run_recent_{index:03}");
            runs.insert(
                run_id.clone(),
                test_server_binding(run_id, CodingAgentRunState::Completed, now - index as i64),
            );
        }
        prune_server_runs_locked(&mut runs, now);
        assert!(runs.contains_key("wc_agent_run_active"));
        assert!(!runs.contains_key("wc_agent_run_expired"));
        assert_eq!(
            runs.values()
                .filter(|binding| binding.snapshot.state.terminal())
                .count(),
            SERVER_MAX_TERMINAL_RUNS
        );
    }

    #[test]
    fn non_writable_project_is_a_hard_prestart_denial() {
        let result = coding_agent_project_not_writable_result("wc_agent_run_readonly");
        assert!(!result.success);
        assert_eq!(result.output["failure_kind"], "policy_rejected");
        assert_eq!(result.output["execution_state"], "not_started");
        assert_eq!(result.output["state_changed"], false);
        assert_eq!(result.output["recovery_kind"], "user_action");
        assert!(crate::tool_runtime::permissions::is_hard_denied_output(
            &result.output,
            result.error.as_deref()
        ));
    }

    #[test]
    fn bound_run_identity_rejects_provider_project_and_intent_retarget() {
        let binding = test_server_binding(
            "wc_agent_run_identity_fence".to_string(),
            CodingAgentRunState::Running,
            1,
        );
        assert!(run_matches_binding_identity(&binding, &binding.snapshot));

        let mut retargeted = binding.snapshot.clone();
        retargeted.provider_id = "other-provider".to_string();
        assert!(!run_matches_binding_identity(&binding, &retargeted));

        let mut retargeted = binding.snapshot.clone();
        retargeted.provider_instance_id = "other-instance".to_string();
        assert!(!run_matches_binding_identity(&binding, &retargeted));

        let mut retargeted = binding.snapshot.clone();
        retargeted.runtime_project_id = "agent:test:other".to_string();
        assert!(!run_matches_binding_identity(&binding, &retargeted));

        let mut retargeted = binding.snapshot.clone();
        retargeted.intent_fingerprint = "other-intent".to_string();
        assert!(!run_matches_binding_identity(&binding, &retargeted));
    }

    #[test]
    fn terminal_not_started_run_does_not_advertise_retry_same() {
        let mut binding = test_server_binding(
            "wc_agent_run_failed_not_started".to_string(),
            CodingAgentRunState::Failed,
            1,
        );
        binding.snapshot.execution_state = CodingAgentExecutionState::NotStarted;
        assert_eq!(
            run_recovery_kind(&binding.snapshot),
            "none",
            "a retained terminal Run cannot be redispatched by replaying the same idempotency key"
        );
    }

    #[test]
    fn stable_principal_canonicalizes_equivalent_credential_transports() {
        let direct_shared = crate::auth::shared_key_context("coding-agent-shared-key");
        let shared_hash = direct_shared.shared_key_hash.clone().unwrap();
        let oauth_shared = AuthContext {
            token_kind: Some("oauth2_shared_key".to_string()),
            shared_key_hash: Some(shared_hash),
            ..AuthContext::new(AuthKind::OAuth2Token)
        };
        assert_eq!(
            stable_principal(Some(&direct_shared)).unwrap(),
            stable_principal(Some(&oauth_shared)).unwrap()
        );

        let pat = AuthContext {
            user_id: Some("user-1".to_string()),
            api_key_id: Some("pat-1".to_string()),
            ..AuthContext::new(AuthKind::ApiToken)
        };
        let oauth_user = AuthContext {
            user_id: Some("user-1".to_string()),
            api_key_id: Some("oauth-access-1".to_string()),
            token_kind: Some("oauth2".to_string()),
            ..AuthContext::new(AuthKind::OAuth2Token)
        };
        assert_eq!(
            stable_principal(Some(&pat)).unwrap(),
            stable_principal(Some(&oauth_user)).unwrap()
        );

        let project = AuthContext {
            project_grant_id: Some("grant-1".to_string()),
            ..AuthContext::new(AuthKind::ProjectCredential)
        };
        let oauth_project = AuthContext {
            token_kind: Some(crate::auth::PROJECT_SHARE_OAUTH_TOKEN_KIND.to_string()),
            project_grant_id: Some("grant-1".to_string()),
            ..AuthContext::new(AuthKind::OAuth2Token)
        };
        assert_eq!(
            stable_principal(Some(&project)).unwrap(),
            stable_principal(Some(&oauth_project)).unwrap()
        );
    }

    #[test]
    fn identities_are_domain_separated_and_tokens_are_run_bound_and_tamper_evident() {
        let principal = "oauth2:shared-key:abc";
        let run = deterministic_run_id(principal, "same-key");
        let epoch = "11111111111111111111111111111111";
        let stale_epoch = "22222222222222222222222222222222";
        assert!(run.starts_with("wc_agent_run_"));
        assert_ne!(authority_fingerprint(principal), run);

        let key = [0x5au8; OBSERVATION_MAC_KEY_BYTES];
        let token = observation_token(&key, epoch, &run, 7);
        assert!(token.starts_with(PUBLIC_TOKEN_PREFIX));
        assert!(token.len() <= PUBLIC_TOKEN_MAX_BYTES);
        assert!(!token.contains(&run));
        assert_eq!(parse_observation_token(&key, epoch, &run, &token), Ok(7));
        let continuation = observation_token(&key, epoch, &run, 9);
        assert_eq!(
            parse_observation_token(&key, epoch, &run, &continuation),
            Ok(9)
        );
        assert_eq!(
            parse_observation_token(&key, stale_epoch, &run, &token),
            Err(TokenError::StaleEpoch)
        );
        assert_eq!(
            parse_observation_token(&key, epoch, "wc_agent_run_other", &token),
            Err(TokenError::Invalid)
        );

        let encoded = token.strip_prefix(PUBLIC_TOKEN_PREFIX).unwrap();
        let mut payload = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        payload[PUBLIC_TOKEN_EPOCH_BYTES] ^= 1;
        let forged_sequence = format!("{PUBLIC_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(&payload));
        assert_eq!(
            parse_observation_token(&key, epoch, &run, &forged_sequence),
            Err(TokenError::Invalid)
        );

        let mut payload = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        payload[0] = b'2';
        let forged_epoch = format!("{PUBLIC_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(&payload));
        assert_eq!(
            parse_observation_token(&key, epoch, &run, &forged_epoch),
            Err(TokenError::Invalid)
        );

        let mut payload = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        *payload.last_mut().unwrap() ^= 1;
        let tampered = format!("{PUBLIC_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(&payload));
        assert_eq!(
            parse_observation_token(&key, epoch, &run, &tampered),
            Err(TokenError::Invalid)
        );
    }

    #[test]
    fn persistent_observation_mac_key_preserves_stale_epoch_across_server_restart() {
        let state_dir = tempfile::tempdir().unwrap();
        let run = "wc_agent_run_restart";
        let first =
            CodingAgentServerState::with_persistent_observation_mac_key(state_dir.path()).unwrap();
        let first_epoch = first.epoch.clone();
        let token = first.observation_token(run, 7);
        drop(first);

        let second =
            CodingAgentServerState::with_persistent_observation_mac_key(state_dir.path()).unwrap();
        assert_ne!(second.epoch, first_epoch);
        assert_eq!(
            second.parse_observation_token(run, &token),
            Err(TokenError::StaleEpoch)
        );

        let encoded = token.strip_prefix(PUBLIC_TOKEN_PREFIX).unwrap();
        let mut payload = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        *payload.last_mut().unwrap() ^= 1;
        let tampered = format!("{PUBLIC_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(payload));
        assert_eq!(
            second.parse_observation_token(run, &tampered),
            Err(TokenError::Invalid)
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let private_dir = state_dir.path().join(OBSERVATION_MAC_KEY_DIR);
            let key_path = private_dir.join(OBSERVATION_MAC_KEY_FILE);
            assert_eq!(
                fs::metadata(private_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(key_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn intent_fingerprint_is_stable_over_sorted_config_and_changes_with_execution_intent() {
        let config = BTreeMap::from([(
            "mode".to_string(),
            CodingAgentConfigValue::String("agent".to_string()),
        )]);
        let a = intent_fingerprint("agent:x:p", "codex", "inspect", &config, 30);
        let b = intent_fingerprint("agent:x:p", "codex", "inspect", &config, 30);
        let c = intent_fingerprint("agent:x:p", "codex", "different", &config, 30);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
