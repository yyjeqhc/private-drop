//! Deterministic, bounded continuation feedback for long Workflow Sessions.
//!
//! This module is a *read-only projection* over the existing durable Workflow
//! Session ledger, validation evidence, job lifecycle state, and message
//! board. It gives a model continuing a long task a compact, deterministic
//! answer to:
//!
//!   - what the last accepted instruction was and what it covered,
//!   - which meaningful tool calls and changes happened since then,
//!   - which tool/validation failures are still open versus already resolved,
//!   - which validation evidence changed relative to a comparable prior run,
//!   - which jobs are active/recovering/terminal-pending,
//!   - which human guidance is still open,
//!   - and the most reasonable deterministic next action.
//!
//! It is **not** an LLM summary, **not** an Agent loop, **not** a new
//! pass/fail verdict, and **not** a second persisted attempt state. Every
//! field is derived from existing state under the store's own bounds. The
//! helpers never execute shell, read project files, enqueue Runner requests,
//! modify the ledger, refresh activity, consume guidance, or change any
//! validation verdict. Where evidence is insufficient a stable `reason_code`
//! is returned instead of a guess.
//!
//! Scope identity and failure identity reuse the existing validation parser
//! and evidence model; `validation_delta` only compares two validation
//! attempts that are proven comparable.

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};

use crate::{
    canonical_tool_call_finished_events, closeout_work_projection, current_attempt_event_view,
    exploration_tool_kind, normalize_observed_project_path, ExplorationToolKind,
    SessionDiscussionSummary, SessionEvent, SessionMessage, SessionSummary,
};

/// Maximum number of characters returned from the previous instruction.
const MAX_INSTRUCTION_EXCERPT_CHARS: usize = 500;
/// Maximum number of changed paths surfaced in the attempt summary.
const MAX_CHANGED_PATHS: usize = 100;
/// Maximum unique paths returned by the full attempt exploration projection.
const MAX_EXPLORATION_PATHS: usize = 100;
/// Maximum number of unresolved failure identities surfaced in the attempt
/// summary and per failure list in the validation delta.
const MAX_FAILURE_IDENTITIES: usize = 20;
/// Maximum number of suggested next actions.
const MAX_SUGGESTED_ACTIONS: usize = 8;
pub const EXPLORATION_CONTINUITY_ACTION: &str =
    "continue from the recent exploration workset before repeating broad discovery";

/// Root continuation feedback projection returned to the model.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContinuationFeedback {
    /// `not_applicable` for a freshly created empty session, `available`
    /// when an attempt summary could be derived.
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<&'static str>,
    pub(crate) attempt: AttemptSummary,
    pub(crate) validation_delta: ValidationDelta,
    pub(crate) deterministic: bool,
    pub(crate) llm_summary: bool,
}

/// Deterministic summary of the current attempt.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptSummary {
    /// How the attempt boundary was determined.
    pub(crate) boundary: AttemptBoundary,
    pub(crate) instruction: AttemptInstruction,
    pub(crate) event_range: AttemptEventRange,
    pub(crate) activity: AttemptActivity,
    pub(crate) changes: AttemptChanges,
    pub(crate) exploration: AttemptExploration,
    pub(crate) validation: AttemptValidation,
    pub(crate) jobs: AttemptJobs,
    pub(crate) guidance: AttemptGuidance,
    pub(crate) outcome: AttemptOutcome,
    pub(crate) suggested_next_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptBoundary {
    /// `task_instruction` when the last accepted instruction event was found in
    /// the retained window, `session_start` when no instruction event exists and
    /// nothing was evicted, `unavailable` when the window was truncated and the
    /// most recent `task_instruction` is no longer retained, `no_events` when the
    /// session is empty.
    pub(crate) source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) event_id: Option<String>,
    /// Position (0-based) of the boundary event within the summarized events.
    pub(crate) event_index: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptInstruction {
    /// `available` when a `task_instruction` event carried the instruction,
    /// `not_observed` otherwise.
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) excerpt: Option<String>,
    /// True when the persisted instruction exceeded the excerpt bound.
    pub(crate) truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recorded_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effective_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) capability_changed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resumed: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptEventRange {
    pub(crate) start_event_id: Option<String>,
    pub(crate) end_event_id: Option<String>,
    pub(crate) start_sequence: usize,
    pub(crate) end_sequence: usize,
    pub(crate) event_count: usize,
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptActivity {
    pub(crate) meaningful_tool_calls: usize,
    pub(crate) successful_tool_calls: usize,
    /// Immutable raw failed ToolCall count in the attempt.
    pub(crate) failed_tool_calls: usize,
    /// Failed ToolCalls that the canonical runtime closeout projection still
    /// considers actionable. Expected, resolved/stale validation, and proven
    /// non-effect failures remain visible in failed_tool_calls but not here.
    pub(crate) actionable_failed_tool_calls: usize,
    pub(crate) expected_failures: usize,
    pub(crate) resolved_failures: usize,
    pub(crate) unresolved_failures: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptChanges {
    pub(crate) changed_paths: Vec<String>,
    pub(crate) total_changed_paths: usize,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptExploration {
    /// Unique validated project-relative paths, newest successful observation
    /// first.
    pub(crate) observed_paths: Vec<String>,
    /// Real unique count before the projection list cap.
    pub(crate) total_observed_paths: usize,
    pub(crate) truncated: bool,
    pub(crate) read_count: usize,
    pub(crate) search_count: usize,
    pub(crate) navigation_count: usize,
    pub(crate) latest_tool: Option<String>,
    /// False when the attempt boundary was evicted from the retained ledger.
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptValidation {
    /// Current workspace evidence status: `passed`, `failed`, `inconclusive`,
    /// `stale`, `not_run`, or `unknown`.
    pub(crate) status: String,
    /// Latest validation event status inside the current evidence window.
    pub(crate) latest_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_at: Option<i64>,
    pub(crate) unresolved_failure_count: usize,
    /// Inconclusive/request-scoped evidence events retained inside the current
    /// post-mutation window. This is process history, not a durable requirement.
    pub(crate) evidence_gap_event_count: usize,
    pub(crate) validation_events: usize,
    pub(crate) stale_failure_count: usize,
    pub(crate) open_failures: Vec<FailureIdentity>,
    pub(crate) total_open_failures: usize,
    pub(crate) failures_truncated: bool,
    pub(crate) delta_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) delta_reason_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptJobs {
    /// Total active jobs (blocking + nonblocking) from the bounded job aggregate.
    pub(crate) active_count: u64,
    /// Blocking-active jobs (queued/running/started/agent_queued/recovering).
    pub(crate) running_count: u64,
    /// Recovering jobs that the runner must reconcile before output is trusted.
    /// Counted over the full active aggregate, not the truncated `recent` list.
    pub(crate) recovering_count: u64,
    /// Jobs awaiting a terminal status after a stop request.
    pub(crate) terminal_pending_count: u64,
    /// True when the active aggregate truncated its `recent` list; counts above
    /// remain reliable because they are computed over the full active vector.
    pub(crate) recent_truncated: bool,
    /// `not_observed` when no job metadata is present, otherwise the latest
    /// active/recovering job status string.
    pub(crate) latest_job_status: String,
    /// `none`, `recovering`, `terminal_pending`, `active`, or `unknown` — derived
    /// only from proven aggregate lifecycle fields, never from wall-clock and
    /// never from the truncated `recent` list. `active` is reported instead of
    /// `healthy` because the aggregate cannot prove every active job is healthy
    /// running rather than recovering/queued.
    pub(crate) recovery_state: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptGuidance {
    pub(crate) open_count: usize,
    pub(crate) open_risk_count: usize,
    pub(crate) open_todo_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_open_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_open_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_open_message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttemptOutcome {
    /// `in_progress`, `blocked`, `clean`, or `unknown`.
    pub(crate) status: String,
    pub(crate) reason_codes: Vec<String>,
}

/// Deterministic diff between the latest validation evidence and the most
/// recent prior comparable validation evidence.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ValidationDelta {
    pub(crate) comparison: ValidationComparison,
    pub(crate) counts: ValidationDeltaCounts,
    pub(crate) failures: ValidationDeltaFailures,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ValidationComparison {
    /// `available` or `unavailable`.
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scope_identity: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ValidationDeltaCounts {
    pub(crate) passed_delta: i64,
    pub(crate) failed_delta: i64,
    pub(crate) ignored_delta: i64,
    pub(crate) total_delta: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ValidationDeltaFailures {
    /// `available` or `unavailable` (with a reason) when stable identities are
    /// not present and only counts could be compared.
    pub(crate) identity_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) identity_reason_code: Option<&'static str>,
    pub(crate) newly_failed: Vec<FailureIdentity>,
    pub(crate) resolved: Vec<FailureIdentity>,
    pub(crate) still_failing: Vec<FailureIdentity>,
    pub(crate) total_newly_failed: usize,
    pub(crate) total_resolved: usize,
    pub(crate) total_still_failing: usize,
    pub(crate) list_truncated: bool,
}

/// A bounded, stable failure identity derived from the existing parser.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FailureIdentity {
    /// `test`, `diagnostic`, or `unknown`.
    pub(crate) kind: &'static str,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<u64>,
}

/// Narrow deterministic policy hook supplied by the root adapter. The Session
/// observation domain receives only the classification fact it needs; it does
/// not own or duplicate the runtime tool taxonomy.
#[derive(Clone, Copy)]
pub struct ContinuationProjectionHooks {
    tool_is_meaningful: fn(&str) -> bool,
}

impl ContinuationProjectionHooks {
    pub fn new(tool_is_meaningful: fn(&str) -> bool) -> Self {
        Self { tool_is_meaningful }
    }
}

/// Bounded current-validation view computed by the canonical Validation owner.
/// The full validation summary remains a separate input because it may carry
/// job-enriched history while this snapshot is derived from durable Session
/// events using the exact current-evidence fence semantics.
#[derive(Clone, Copy)]
pub struct ContinuationValidationSnapshot<'a> {
    current_evidence: &'a Value,
    current_validation: &'a Value,
}

impl<'a> ContinuationValidationSnapshot<'a> {
    pub fn new(current_evidence: &'a Value, current_validation: &'a Value) -> Self {
        Self {
            current_evidence,
            current_validation,
        }
    }
}

/// Canonical runtime-owned ToolFailure actionability for the same Session
/// snapshot. Workflow Session uses only exact event-id membership and never
/// reimplements validation/result-expectation/effect-safety classification.
#[derive(Clone, Copy)]
pub struct ContinuationToolFailureSnapshot<'a> {
    actionable_event_ids: &'a HashSet<String>,
}

impl<'a> ContinuationToolFailureSnapshot<'a> {
    pub fn new(actionable_event_ids: &'a HashSet<String>) -> Self {
        Self {
            actionable_event_ids,
        }
    }

    fn is_actionable(&self, event: &SessionEvent) -> bool {
        self.actionable_event_ids.contains(&event.event_id)
    }
}

/// Inputs gathered up-front so the projection helpers stay pure and never
/// touch locks, the network, or the filesystem. Callers copy bounded
/// snapshots before constructing this.
pub struct ContinuationFeedbackInput<'a> {
    pub session_summary: &'a SessionSummary,
    /// Ledger-derived validation summary value (`validation_summary_from_events`
    /// output) for the *current* session events, already job-enriched.
    pub validation: &'a Value,
    /// `active_jobs_summary` value, already bounded by the caller.
    pub jobs: &'a Value,
    /// Discussion/message-board summary with open guidance/risk/todo lists.
    pub discussion: &'a SessionDiscussionSummary,
    /// Continuation kind reported by the session start path:
    /// `created`, `continued`, `resumed_explicitly`, or `restored`.
    pub continuation: &'static str,
    /// True only for a coding-start continuation/resume. Closeout callers do
    /// not add startup-oriented exploration guidance.
    pub suggest_exploration_continuity: bool,
    /// Current startup workspace conflict fact, gathered outside this pure
    /// projection. It gates the optional exploration continuity suggestion.
    pub workspace_conflicts: bool,
    pub hooks: ContinuationProjectionHooks,
    pub current_validation: ContinuationValidationSnapshot<'a>,
    /// Exact raw failed ToolCall event ids that the canonical runtime closeout
    /// projection still considers actionable.
    pub tool_failures: ContinuationToolFailureSnapshot<'a>,
}

impl ContinuationFeedback {
    /// Build the full continuation feedback projection from bounded
    /// snapshots. Pure read-only: no shell, no files, no ledger mutation,
    /// no Runner requests, no activity refresh, no guidance consumption.
    pub(crate) fn from_snapshots(input: ContinuationFeedbackInput<'_>) -> Value {
        let events = &input.session_summary.events;
        if events.is_empty() {
            // `created` is a freshly started empty session; a resumed/restored
            // session that happens to have no events yet is `empty_session`.
            let reason_code = if input.continuation == "created" {
                "fresh_session"
            } else {
                "empty_session"
            };
            return not_applicable_continuation_feedback_value(reason_code);
        }

        let attempt_view = current_attempt_event_view(input.session_summary);
        // Attempt events start *after* the boundary task_instruction event. The
        // shared view canonicalizes finished recorder/business evidence against
        // the whole retained Session before slicing.
        let attempt_start = attempt_view.attempt_start;
        let attempt_events = &events[attempt_start..];
        let semantic_attempt_events = attempt_view.semantic_events;
        let boundary_event = attempt_view
            .boundary_event_index
            .map(|index| &events[index]);

        let attempt = build_attempt_summary(
            attempt_events,
            &semantic_attempt_events,
            boundary_event,
            attempt_view.boundary_event_index,
            attempt_view.boundary_source,
            attempt_view.boundary_reason_code,
            attempt_start,
            input.session_summary,
            input.validation,
            input.hooks,
            input.current_validation,
            input.tool_failures,
            input.jobs,
            input.discussion,
            input.continuation,
            input.suggest_exploration_continuity,
            input.workspace_conflicts,
        );

        let validation_delta = validation_delta(input.validation);
        let status = "available";

        to_value(ContinuationFeedback {
            status,
            reason_code: None,
            attempt,
            validation_delta,
            deterministic: true,
            llm_summary: false,
        })
    }
}

fn empty_attempt() -> AttemptSummary {
    AttemptSummary {
        boundary: AttemptBoundary {
            source: "no_events",
            reason_code: None,
            event_id: None,
            event_index: 0,
        },
        instruction: AttemptInstruction {
            status: "not_observed",
            excerpt: None,
            truncated: false,
            recorded_at: None,
            requested_mode: None,
            effective_mode: None,
            capability_changed: None,
            resumed: None,
        },
        event_range: AttemptEventRange {
            start_event_id: None,
            end_event_id: None,
            start_sequence: 0,
            end_sequence: 0,
            event_count: 0,
            complete: true,
        },
        activity: AttemptActivity {
            meaningful_tool_calls: 0,
            successful_tool_calls: 0,
            failed_tool_calls: 0,
            actionable_failed_tool_calls: 0,
            expected_failures: 0,
            resolved_failures: 0,
            unresolved_failures: 0,
        },
        changes: AttemptChanges {
            changed_paths: Vec::new(),
            total_changed_paths: 0,
            truncated: false,
        },
        exploration: AttemptExploration {
            observed_paths: Vec::new(),
            total_observed_paths: 0,
            truncated: false,
            read_count: 0,
            search_count: 0,
            navigation_count: 0,
            latest_tool: None,
            complete: true,
        },
        validation: AttemptValidation {
            status: "not_run".to_string(),
            latest_status: "not_run".to_string(),
            latest_kind: None,
            latest_at: None,
            unresolved_failure_count: 0,
            evidence_gap_event_count: 0,
            validation_events: 0,
            stale_failure_count: 0,
            open_failures: Vec::new(),
            total_open_failures: 0,
            failures_truncated: false,
            delta_available: false,
            delta_reason_code: None,
        },
        jobs: AttemptJobs {
            active_count: 0,
            running_count: 0,
            recovering_count: 0,
            terminal_pending_count: 0,
            recent_truncated: false,
            latest_job_status: "not_observed".to_string(),
            recovery_state: "none".to_string(),
        },
        guidance: AttemptGuidance {
            open_count: 0,
            open_risk_count: 0,
            open_todo_count: 0,
            latest_open_kind: None,
            latest_open_at: None,
            latest_open_message_id: None,
        },
        outcome: AttemptOutcome {
            status: "in_progress".to_string(),
            reason_codes: Vec::new(),
        },
        suggested_next_actions: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_attempt_summary(
    attempt_events: &[SessionEvent],
    semantic_attempt_events: &[SessionEvent],
    boundary_event: Option<&SessionEvent>,
    boundary_event_index: Option<usize>,
    boundary_source: &'static str,
    boundary_reason_code: Option<&'static str>,
    attempt_start: usize,
    session_summary: &SessionSummary,
    validation: &Value,
    hooks: ContinuationProjectionHooks,
    current_validation: ContinuationValidationSnapshot<'_>,
    tool_failures: ContinuationToolFailureSnapshot<'_>,
    jobs: &Value,
    discussion: &SessionDiscussionSummary,
    continuation: &'static str,
    suggest_exploration_continuity: bool,
    workspace_conflicts: bool,
) -> AttemptSummary {
    let total_events = attempt_events.len();

    // --- instruction (from the boundary task_instruction event) ---
    let instruction = instruction_from_boundary(boundary_event, continuation);

    // --- event range ---
    // `complete` is false when the retained window is truncated *and* the most
    // recent task_instruction was evicted (boundary unavailable): the attempt
    // before that boundary is not fully represented by the retained tail.
    // Otherwise the boundary event and everything after it is retained, so the
    // attempt tail is complete relative to the durable ledger.
    let complete = boundary_reason_code != Some("attempt_boundary_evicted");
    // Sequences are absolute within the retained ledger: the returned window
    // starts at `first_retained_sequence`, so the attempt's first event has
    // sequence `first_retained_sequence + attempt_start`.
    let base_sequence = session_summary.first_retained_sequence;
    let start_event_id = attempt_events.first().map(|event| event.event_id.clone());
    let end_event_id = attempt_events.last().map(|event| event.event_id.clone());
    let event_range = AttemptEventRange {
        start_event_id,
        end_event_id,
        start_sequence: base_sequence + attempt_start,
        end_sequence: base_sequence + attempt_start + total_events,
        event_count: total_events,
        complete,
    };

    // --- activity (meaningful tool calls only) ---
    let canonical_finished = canonical_tool_call_finished_events(semantic_attempt_events);
    let meaningful: Vec<&SessionEvent> = canonical_finished
        .iter()
        .copied()
        .filter(|event| (hooks.tool_is_meaningful)(&event.tool_name))
        .collect();
    let successful_tool_calls = meaningful
        .iter()
        .filter(|event| event.status.as_deref() == Some("succeeded"))
        .count();
    let failed_tool_calls = meaningful
        .iter()
        .filter(|event| event.status.as_deref() == Some("failed"))
        .count();
    let actionable_failed_tool_calls = meaningful
        .iter()
        .filter(|event| {
            event.status.as_deref() == Some("failed") && tool_failures.is_actionable(event)
        })
        .count();
    // expected failures = finished tool calls flagged as expected-failure that
    // matched (the ledger's `failure_expectation_result == matched_expected_failure`).
    let expected_failures = canonical_finished
        .iter()
        .filter(|event| {
            event
                .failure_expectation_result
                .as_deref()
                .unwrap_or("none")
                == "matched_expected_failure"
        })
        .count();
    // Current attempt validation is additionally reset by the latest trusted
    // material workspace-content change. Historical validation remains intact
    // in the session-wide summary, while activity/open-failure counts use only
    // evidence that still describes the current workspace state.
    let current_evidence = validation
        .get("current_evidence")
        .filter(|value| value.is_object())
        .unwrap_or(current_validation.current_evidence);
    let resolved_failures = current_evidence
        .get("resolved_failure_count")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let unresolved_failures = current_evidence
        .get("unresolved_failure_count")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;

    // --- changes (deduped, deterministic order via closeout_work_projection) ---
    let (_, changed_paths_value) = closeout_work_projection(semantic_attempt_events);
    let mut changed_paths: Vec<String> = changed_paths_value
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let total_changed_paths = changed_paths.len();
    let truncated = total_changed_paths > MAX_CHANGED_PATHS;
    changed_paths.truncate(MAX_CHANGED_PATHS);
    let exploration = build_attempt_exploration(semantic_attempt_events, complete);

    // --- validation (current attempt verdict, history must not pollute) ---
    let delta = validation_delta(validation);
    let validation_block = build_attempt_validation(validation, current_validation, &delta);

    // --- jobs (reuse existing lifecycle fields) ---
    let jobs_block = build_attempt_jobs(jobs);

    // --- guidance (read-only counts from the message board) ---
    let guidance_block = build_attempt_guidance(discussion);

    // --- outcome ---
    let (outcome_status, reason_codes) = build_attempt_outcome(
        &meaningful,
        actionable_failed_tool_calls,
        unresolved_failures,
        &jobs_block,
        &guidance_block,
        &validation_block,
    );

    let suggested_next_actions = build_suggested_next_actions(
        &outcome_status,
        unresolved_failures,
        &jobs_block,
        &guidance_block,
        &validation_block,
        &exploration,
        suggest_exploration_continuity,
        workspace_conflicts,
    );

    AttemptSummary {
        boundary: AttemptBoundary {
            source: boundary_source,
            reason_code: boundary_reason_code,
            event_id: boundary_event.map(|event| event.event_id.clone()),
            event_index: boundary_event_index.unwrap_or(0),
        },
        instruction,
        event_range,
        activity: AttemptActivity {
            meaningful_tool_calls: meaningful.len(),
            successful_tool_calls,
            failed_tool_calls,
            actionable_failed_tool_calls,
            expected_failures,
            resolved_failures,
            unresolved_failures,
        },
        changes: AttemptChanges {
            changed_paths,
            total_changed_paths,
            truncated,
        },
        exploration,
        validation: validation_block,
        jobs: jobs_block,
        guidance: guidance_block,
        outcome: AttemptOutcome {
            status: outcome_status,
            reason_codes,
        },
        suggested_next_actions,
    }
}

fn build_attempt_exploration(
    attempt_events: &[SessionEvent],
    complete: bool,
) -> AttemptExploration {
    let mut read_count = 0usize;
    let mut search_count = 0usize;
    let mut navigation_count = 0usize;
    let mut latest_tool = None;
    let mut seen = BTreeSet::new();
    let mut observed_paths = Vec::new();

    for event in canonical_tool_call_finished_events(attempt_events)
        .into_iter()
        .rev()
        .filter(|event| event.status.as_deref() == Some("succeeded"))
    {
        let Some(kind) = exploration_tool_kind(&event.tool_name) else {
            continue;
        };
        match kind {
            ExplorationToolKind::Read => read_count += 1,
            ExplorationToolKind::Search => search_count += 1,
            ExplorationToolKind::Navigation => navigation_count += 1,
        }

        let mut event_has_evidence = false;
        for raw_path in &event.observed_paths {
            let Some(path) = normalize_observed_project_path(raw_path) else {
                continue;
            };
            event_has_evidence = true;
            if seen.insert(path.clone()) && observed_paths.len() < MAX_EXPLORATION_PATHS {
                observed_paths.push(path);
            }
        }
        if event_has_evidence && latest_tool.is_none() {
            latest_tool = Some(event.tool_name.clone());
        }
    }

    let total_observed_paths = seen.len();
    AttemptExploration {
        observed_paths,
        total_observed_paths,
        truncated: total_observed_paths > MAX_EXPLORATION_PATHS,
        read_count,
        search_count,
        navigation_count,
        latest_tool,
        complete,
    }
}

fn instruction_from_boundary(
    boundary_event: Option<&SessionEvent>,
    continuation: &'static str,
) -> AttemptInstruction {
    let Some(event) = boundary_event else {
        // No `task_instruction` event was found; report a clear fallback rather
        // than guessing an instruction.
        return AttemptInstruction {
            status: "not_observed",
            excerpt: None,
            truncated: false,
            recorded_at: None,
            requested_mode: None,
            effective_mode: None,
            capability_changed: None,
            resumed: None,
        };
    };
    // `explicit_resume` and `session_reused` are recorded in the boundary
    // event's redacted `input_summary`; the resumed flag is best derived from
    // the continuation kind reported by the session start path when present,
    // falling back to the recorded metadata otherwise.
    let resumed = match continuation {
        "resumed_explicitly" => Some(true),
        "created" => Some(false),
        _ => event
            .input_summary
            .as_ref()
            .and_then(|summary| summary.get("explicit_resume"))
            .and_then(Value::as_bool),
    };
    let (excerpt, truncated) = event
        .instruction
        .as_deref()
        .map(|instruction| bounded_excerpt(instruction, MAX_INSTRUCTION_EXCERPT_CHARS))
        .map_or((None, false), |(excerpt, truncated)| {
            (Some(excerpt), truncated)
        });
    AttemptInstruction {
        status: "available",
        excerpt,
        truncated,
        recorded_at: Some(event.timestamp),
        requested_mode: event.requested_mode.as_deref().map(str::to_string),
        effective_mode: event.requested_mode.as_deref().map(str::to_string),
        capability_changed: event.capability_changed,
        resumed,
    }
}

fn build_attempt_validation(
    validation: &Value,
    current_validation: ContinuationValidationSnapshot<'_>,
    delta: &ValidationDelta,
) -> AttemptValidation {
    // When the caller explicitly did not request validation (handoff with
    // `include_validation=false`), report it as unavailable with a stable
    // reason rather than masquerading the absence as `not_run`.
    let not_requested = validation
        .get("not_requested")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let evidence = validation
        .get("current_evidence")
        .filter(|value| value.is_object())
        .unwrap_or(current_validation.current_evidence);
    let current_status = if not_requested {
        "unknown"
    } else {
        evidence
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    };
    let current_unresolved = if not_requested {
        0
    } else {
        evidence
            .get("unresolved_failure_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize
    };
    let evidence_gap_event_count = if not_requested {
        0
    } else {
        evidence
            .get("evidence_gap_event_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize
    };
    let (open_failures, observed_open_failures, observed_truncated) = if not_requested {
        (Vec::new(), 0, false)
    } else {
        attempt_open_failures(current_validation.current_validation)
    };
    let total_open_failures = observed_open_failures.max(current_unresolved);
    let failures_truncated = observed_truncated || total_open_failures > open_failures.len();
    let latest_status = if not_requested {
        "unavailable".to_string()
    } else {
        evidence
            .get("latest_status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string()
    };
    let delta_available = !not_requested && delta.comparison.status == "available";
    let delta_reason_code = if not_requested {
        Some("validation_not_requested".to_string())
    } else if delta_available {
        None
    } else {
        delta
            .comparison
            .reason_code
            .map(|code| code.to_string())
            .or_else(|| Some("no_previous_validation".to_string()))
    };
    let latest = current_validation.current_validation.get("latest");
    let latest_kind = latest
        .and_then(|event| event.get("validation_kind"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let latest_at = latest
        .and_then(|event| {
            event
                .get("completed_at")
                .or_else(|| event.get("started_at"))
        })
        .and_then(Value::as_i64);
    let validation_events = if not_requested {
        0
    } else {
        evidence
            .get("events_total")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize
    };
    let stale_failure_count = if not_requested {
        0
    } else {
        evidence
            .get("stale_failure_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize
    };
    AttemptValidation {
        status: current_status.to_string(),
        latest_status,
        latest_kind,
        latest_at,
        unresolved_failure_count: current_unresolved,
        evidence_gap_event_count,
        validation_events,
        stale_failure_count,
        open_failures,
        total_open_failures,
        failures_truncated,
        delta_available,
        delta_reason_code,
    }
}

/// Derive the attempt jobs block purely from the bounded `active_jobs_summary`
/// aggregate. Counts come from the aggregate (computed over the full active
/// vector), never from the truncated `recent` list, so a long active list cannot
/// hide a recovering job and report it as healthy. `recovering` is never equated
/// with healthy `running`, runner-offline is never equated with process-lost,
/// and `failed`/`lost`/`orphaned` counts are not reported because the active
/// aggregate never includes terminal jobs. No Runner query or network call.
fn build_attempt_jobs(jobs: &Value) -> AttemptJobs {
    let active_count = jobs
        .get("active_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let running_count = jobs
        .get("running_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let recovering_count = jobs
        .get("recovering_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let terminal_pending_count = jobs
        .get("terminal_pending_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let recent_truncated = jobs
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let latest_job_status = jobs
        .get("recent")
        .and_then(Value::as_array)
        .and_then(|recent| recent.first())
        .and_then(|job| job.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("not_observed")
        .to_string();
    let recovery_state = if active_count == 0 && terminal_pending_count == 0 {
        "none".to_string()
    } else if recovering_count > 0 {
        // Recovering jobs must be reconciled before their output is trusted; this
        // is never reported as healthy/active.
        "recovering".to_string()
    } else if terminal_pending_count > 0 {
        "terminal_pending".to_string()
    } else if running_count > 0 {
        // Active but no proven recovering subset; report the neutral `active`
        // rather than a stronger `healthy` claim.
        "active".to_string()
    } else {
        "unknown".to_string()
    };
    AttemptJobs {
        active_count,
        running_count,
        recovering_count,
        terminal_pending_count,
        recent_truncated,
        latest_job_status,
        recovery_state,
    }
}

/// Read-only guidance projection from the message board. Counts open
/// guidance/risk/todo messages and surfaces a bounded pointer to the latest
/// open message (kind, time, id) — never the full text, and never marks
/// anything read or resolved.
fn build_attempt_guidance(discussion: &SessionDiscussionSummary) -> AttemptGuidance {
    let open_guidance = &discussion.open_guidance;
    let open_risks = &discussion.open_risks;
    let open_todos = &discussion.open_todos;
    let open_count = open_guidance.len();
    let open_risk_count = open_risks.len();
    let open_todo_count = open_todos.len();

    // Latest open message across guidance/risk/todo by created_at, descending.
    let latest: Option<&SessionMessage> = [open_guidance, open_risks, open_todos]
        .into_iter()
        .flatten()
        .max_by_key(|message| message.created_at);
    AttemptGuidance {
        open_count,
        open_risk_count,
        open_todo_count,
        latest_open_kind: latest.map(|message| message.kind.as_str().to_string()),
        latest_open_at: latest.map(|message| message.created_at),
        latest_open_message_id: latest.map(|message| message.message_id.clone()),
    }
}

fn build_attempt_outcome(
    meaningful: &[&SessionEvent],
    actionable_failed_tool_calls: usize,
    unresolved_failures: usize,
    jobs: &AttemptJobs,
    guidance: &AttemptGuidance,
    validation: &AttemptValidation,
) -> (String, Vec<String>) {
    let mut reasons: Vec<String> = Vec::new();
    if unresolved_failures > 0 {
        push_unique(&mut reasons, "unresolved_validation_failures");
    }
    if actionable_failed_tool_calls > 0 && unresolved_failures == 0 {
        push_unique(&mut reasons, "actionable_failed_tool_calls");
    }
    if jobs.recovering_count > 0 {
        push_unique(&mut reasons, "jobs_recovering");
    }
    if jobs.terminal_pending_count > 0 {
        push_unique(&mut reasons, "jobs_terminal_pending");
    }
    if guidance.open_count > 0 {
        push_unique(&mut reasons, "open_guidance");
    }
    if validation.status == "not_run" && !meaningful.is_empty() {
        push_unique(&mut reasons, "validation_not_run");
    }
    if validation.status == "stale" {
        push_unique(&mut reasons, "validation_stale_after_changes");
    }
    if validation.status == "inconclusive" {
        push_unique(&mut reasons, "validation_inconclusive");
    }
    let status = if reasons.is_empty() {
        "in_progress".to_string()
    } else if unresolved_failures > 0 || jobs.recovering_count > 0 {
        "blocked".to_string()
    } else {
        "in_progress".to_string()
    };
    (status, reasons)
}

fn build_suggested_next_actions(
    _outcome_status: &str,
    unresolved_failures: usize,
    jobs: &AttemptJobs,
    guidance: &AttemptGuidance,
    validation: &AttemptValidation,
    exploration: &AttemptExploration,
    suggest_exploration_continuity: bool,
    workspace_conflicts: bool,
) -> Vec<String> {
    let mut actions = Vec::new();
    if unresolved_failures > 0 {
        if let Some(failure) = validation.open_failures.first() {
            let target = match failure.kind {
                "test" => format!("fix failing test {}", failure.name),
                "diagnostic" => match (&failure.file, failure.line) {
                    (Some(file), Some(line)) => {
                        format!("fix diagnostic {} at {file}:{line}", failure.name)
                    }
                    _ => format!("fix diagnostic {}", failure.name),
                },
                _ => format!("fix validation failure {}", failure.name),
            };
            actions.push(target);
        } else {
            actions.push("review unresolved validation failures before continuing".to_string());
        }
    }
    if jobs.recovering_count > 0 {
        push_unique(
            &mut actions,
            "await recovering jobs before relying on their output",
        );
    }
    if jobs.terminal_pending_count > 0 {
        push_unique(
            &mut actions,
            "confirm terminal-pending jobs reached a final state",
        );
    }
    if guidance.open_count > 0 {
        push_unique(
            &mut actions,
            "address open guidance on the session message board",
        );
    }
    if matches!(
        validation.status.as_str(),
        "not_run" | "stale" | "inconclusive"
    ) {
        push_unique(
            &mut actions,
            "run validation before proceeding when the task warrants it",
        );
    }
    if suggest_exploration_continuity
        && !exploration.observed_paths.is_empty()
        && unresolved_failures == 0
        && jobs.running_count == 0
        && guidance.open_risk_count == 0
        && !workspace_conflicts
    {
        push_unique(&mut actions, EXPLORATION_CONTINUITY_ACTION);
    }
    if actions.is_empty() {
        actions.push("continue with the next task step".to_string());
    }
    actions.into_iter().take(MAX_SUGGESTED_ACTIONS).collect()
}

// ---------------------------------------------------------------------------
// Validation delta
// ---------------------------------------------------------------------------

/// Public, pure read-only entry: build only the validation delta from a
/// ledger-derived validation summary value. Used by `validation_summary` so
/// the delta stays a read-only projection that never re-runs validation,
/// mutates the ledger, or changes the verdict.
pub fn validation_delta_value(validation: &Value) -> Value {
    to_value(validation_delta(validation))
}

/// Public, pure read-only entry: build the full continuation feedback from
/// bounded snapshots gathered by the caller. The caller is responsible for
/// copying bounded snapshots under the appropriate locks and *before*
/// constructing the input; this helper never touches a lock, the network, or
/// the filesystem.
pub fn continuation_feedback_value(input: ContinuationFeedbackInput<'_>) -> Value {
    ContinuationFeedback::from_snapshots(input)
}

/// Stable full empty shape for fresh/empty Workflow Sessions. Keeping the
/// attempt object present means every caller sees the same empty exploration
/// contract instead of transport-specific omissions.
pub fn not_applicable_continuation_feedback_value(reason_code: &'static str) -> Value {
    to_value(ContinuationFeedback {
        status: "not_applicable",
        reason_code: Some(reason_code),
        attempt: empty_attempt(),
        validation_delta: unavailable_delta("no_previous_validation"),
        deterministic: true,
        llm_summary: false,
    })
}

/// Build the deterministic validation delta from the current validation
/// summary value. The delta compares the *latest* validation event to the
/// most recent *prior comparable* validation event. Comparison is gated by a
/// proven scope identity; when scopes cannot be proven comparable the delta
/// is `unavailable` with a stable reason code rather than a guess.
fn validation_delta(validation: &Value) -> ValidationDelta {
    let events_value = match validation.get("events").and_then(Value::as_array) {
        Some(events) if !events.is_empty() => events,
        _ => {
            return unavailable_delta("no_previous_validation");
        }
    };

    let current = match events_value.last() {
        Some(event) => event,
        None => return unavailable_delta("no_previous_validation"),
    };
    let scope = scope_identity_for(current);
    // Find the most recent prior validation event of the same comparable type
    // (validation kind + tool). The finer-grained scope (cwd, command summary,
    // purpose) is then proven by `compare_scopes`, which returns
    // `validation_scope_changed` / `insufficient_scope_identity` instead of a
    // guess. This distinguishes "no prior run of this validation" from "a prior
    // run existed but under a different scope".
    let current_type = comparable_type_for(current);
    let previous = events_value
        .iter()
        .rev()
        .skip(1)
        .find(|event| comparable_type_for(event) == current_type);

    let Some(previous) = previous else {
        return unavailable_delta("no_previous_validation");
    };

    let comparability = compare_scopes(current, previous, &scope);
    if !comparability.comparable {
        return ValidationDelta {
            comparison: ValidationComparison {
                status: "unavailable",
                reason_code: Some(comparability.reason_code),
                current_event_id: current
                    .get("identity")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                previous_event_id: previous
                    .get("identity")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                scope_identity: Some(scope),
            },
            counts: ValidationDeltaCounts {
                passed_delta: 0,
                failed_delta: 0,
                ignored_delta: 0,
                total_delta: 0,
            },
            failures: ValidationDeltaFailures {
                identity_status: "unavailable",
                identity_reason_code: Some(comparability.reason_code),
                newly_failed: Vec::new(),
                resolved: Vec::new(),
                still_failing: Vec::new(),
                total_newly_failed: 0,
                total_resolved: 0,
                total_still_failing: 0,
                list_truncated: false,
            },
        };
    }

    let counts = delta_counts(current, previous);
    let (failures, failure_reason) = delta_failures(current, previous);

    ValidationDelta {
        comparison: ValidationComparison {
            status: "available",
            reason_code: None,
            current_event_id: current
                .get("identity")
                .and_then(Value::as_str)
                .map(str::to_string),
            previous_event_id: previous
                .get("identity")
                .and_then(Value::as_str)
                .map(str::to_string),
            scope_identity: Some(scope),
        },
        counts,
        failures: ValidationDeltaFailures {
            identity_status: if failure_reason.is_some() {
                "unavailable"
            } else {
                "available"
            },
            identity_reason_code: failure_reason,
            newly_failed: failures.newly_failed,
            resolved: failures.resolved,
            still_failing: failures.still_failing,
            total_newly_failed: failures.total_newly_failed,
            total_resolved: failures.total_resolved,
            total_still_failing: failures.total_still_failing,
            list_truncated: failures.list_truncated,
        },
    }
}

fn unavailable_delta(reason: &'static str) -> ValidationDelta {
    ValidationDelta {
        comparison: ValidationComparison {
            status: "unavailable",
            reason_code: Some(reason),
            current_event_id: None,
            previous_event_id: None,
            scope_identity: None,
        },
        counts: ValidationDeltaCounts {
            passed_delta: 0,
            failed_delta: 0,
            ignored_delta: 0,
            total_delta: 0,
        },
        failures: ValidationDeltaFailures {
            identity_status: "unavailable",
            identity_reason_code: Some(reason),
            newly_failed: Vec::new(),
            resolved: Vec::new(),
            still_failing: Vec::new(),
            total_newly_failed: 0,
            total_resolved: 0,
            total_still_failing: 0,
            list_truncated: false,
        },
    }
}

/// The comparable type for a validation event: validation kind + tool name. Two
/// events of different types (e.g. a `cargo check` versus a `cargo test`) are
/// never comparable; the finer-grained scope fields are proven by
/// `compare_scopes` after this coarse type match.
fn comparable_type_for(event: &Value) -> (Option<&str>, Option<&str>) {
    let kind = event.get("validation_kind").and_then(Value::as_str);
    let tool = event.get("tool_name").and_then(Value::as_str);
    (kind, tool)
}

/// A stable, domain-separated, opaque scope identity for a validation event.
///
/// This is a SHA-256 digest over a normalized structured scope (validation kind,
/// tool, purpose, project-relative cwd, command summary). It is returned for
/// explainability only: it lets a caller see "same scope" vs "different scope"
/// without ever exposing the raw command text, absolute paths, test filters, or
/// arguments. The raw scope components are never returned over the wire.
fn scope_identity_for(event: &Value) -> String {
    let kind = event
        .get("validation_kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool = event.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let purpose = event.get("purpose").and_then(Value::as_str).unwrap_or("");
    let cwd = normalize_scope_cwd(event.get("cwd").and_then(Value::as_str).unwrap_or(""));
    let command = normalize_scope_command(
        event
            .get("command_summary")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    // Domain-separated, version-tagged, length-prefixed join so two different
    // scope tuples cannot collide by field-boundary ambiguity.
    let input = format!(
        "validation_scope:v1\u{1f}{kind}\u{1f}{tool}\u{1f}{purpose}\u{1f}{cwd}\u{1f}{command}"
    );
    let digest = Sha256::digest(input.as_bytes());
    format!("validation_scope:v1:{:x}", digest)
}

/// Normalize a cwd for the scope identity to a compact, relative form that does
/// not expose absolute paths or environment-specific prefixes. We keep only the
/// last path segment(s) that distinguish package/project identity, collapsing
/// leading `./`, `crates/`, and absolute roots. This is best-effort stable: the
/// same logical package produces the same normalized cwd across runs.
fn normalize_scope_cwd(cwd: &str) -> String {
    let mut value = cwd.trim().trim_start_matches("./").to_string();
    // Strip a leading absolute root or Windows drive root down to a relative tail.
    while value.starts_with('/') {
        value.remove(0);
    }
    // Collapse repeated slashes.
    while value.contains("//") {
        value = value.replace("//", "/");
    }
    value.trim_end_matches('/').to_string()
}

/// Normalize a command summary into a stable, opaque token. We intentionally do
/// not preserve the raw command in the public identity; instead we hash the
/// normalized whitespace-joined command so the identity is stable for the same
/// recipe but the raw text never leaves this function. The returned token is a
/// short digest already folded into `scope_identity_for`; this helper only
/// canonicalizes whitespace/case-insensitivity of the recipe surface.
fn normalize_scope_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct ScopeComparability {
    comparable: bool,
    reason_code: &'static str,
}

/// Prove two validation events are comparable using only the scope fields the
/// ledger actually records. Missing fields that are required to prove
/// comparability yield `insufficient_scope_identity` rather than a guess.
fn compare_scopes(current: &Value, previous: &Value, _scope: &str) -> ScopeComparability {
    // validation kind + tool must match.
    let current_kind = current.get("validation_kind").and_then(Value::as_str);
    let previous_kind = previous.get("validation_kind").and_then(Value::as_str);
    let current_tool = current.get("tool_name").and_then(Value::as_str);
    let previous_tool = previous.get("tool_name").and_then(Value::as_str);
    if current_kind.is_none()
        || previous_kind.is_none()
        || current_tool.is_none()
        || previous_tool.is_none()
    {
        return ScopeComparability {
            comparable: false,
            reason_code: "insufficient_scope_identity",
        };
    }
    if current_kind != previous_kind || current_tool != previous_tool {
        return ScopeComparability {
            comparable: false,
            reason_code: "validation_scope_changed",
        };
    }

    // cwd (package/project-relative working directory) must match when present.
    let current_cwd = current.get("cwd").and_then(Value::as_str);
    let previous_cwd = previous.get("cwd").and_then(Value::as_str);
    if current_cwd.is_none() || previous_cwd.is_none() {
        return ScopeComparability {
            comparable: false,
            reason_code: "insufficient_scope_identity",
        };
    }
    if current_cwd != previous_cwd {
        return ScopeComparability {
            comparable: false,
            reason_code: "validation_scope_changed",
        };
    }

    // command_summary encodes the test filter / features / package; a different
    // command means a different test selection and the runs are not comparable.
    let current_command = current.get("command_summary").and_then(Value::as_str);
    let previous_command = previous.get("command_summary").and_then(Value::as_str);
    if current_command.is_none() || previous_command.is_none() {
        return ScopeComparability {
            comparable: false,
            reason_code: "insufficient_scope_identity",
        };
    }
    if current_command != previous_command {
        return ScopeComparability {
            comparable: false,
            reason_code: "validation_scope_changed",
        };
    }

    // purpose must match.
    let current_purpose = current.get("purpose").and_then(Value::as_str);
    let previous_purpose = previous.get("purpose").and_then(Value::as_str);
    if current_purpose.is_none() || previous_purpose.is_none() {
        return ScopeComparability {
            comparable: false,
            reason_code: "insufficient_scope_identity",
        };
    }
    if current_purpose != previous_purpose {
        return ScopeComparability {
            comparable: false,
            reason_code: "validation_scope_changed",
        };
    }

    // Per-event parser diagnostics. Both events must have parsed evidence
    // (`diagnostics.available`) and the same parser identity; a different parser
    // kind means the failure/identity model changed and the runs are not
    // comparable. Missing per-event diagnostics means that event's evidence was
    // not parsed and its failure identities are unknown.
    let current_diagnostics = current.get("diagnostics");
    let previous_diagnostics = previous.get("diagnostics");
    let current_parsed = current_diagnostics
        .and_then(|d| d.get("available"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let previous_parsed = previous_diagnostics
        .and_then(|d| d.get("available"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Current evidence completeness: the latest run must have actually executed
    // (carry a success verdict) to be diffed.
    let current_complete = current.get("success").is_some();
    if !current_complete {
        return ScopeComparability {
            comparable: false,
            reason_code: "current_evidence_incomplete",
        };
    }

    // Previous evidence completeness: the prior run must also have executed and
    // parsed; otherwise we cannot prove its failure identities to diff against.
    let previous_complete = previous.get("success").is_some();
    if !previous_complete {
        return ScopeComparability {
            comparable: false,
            reason_code: "previous_evidence_incomplete",
        };
    }

    if !current_parsed {
        return ScopeComparability {
            comparable: false,
            reason_code: "current_evidence_incomplete",
        };
    }
    if !previous_parsed {
        return ScopeComparability {
            comparable: false,
            reason_code: "previous_evidence_incomplete",
        };
    }

    // Parser identity (kind) must match so the failure/identity model is the same
    // on both sides. A changed parser means identities are not directly
    // comparable even when the scope otherwise matches.
    let current_parser = current_diagnostics
        .and_then(|d| d.get("parser"))
        .and_then(Value::as_str);
    let previous_parser = previous_diagnostics
        .and_then(|d| d.get("parser"))
        .and_then(Value::as_str);
    if current_parser.is_none() || previous_parser.is_none() {
        return ScopeComparability {
            comparable: false,
            reason_code: "parser_identity_unavailable",
        };
    }
    if current_parser != previous_parser {
        return ScopeComparability {
            comparable: false,
            reason_code: "parser_changed",
        };
    }

    // Output truncation can make counts or failure identities incomplete. When
    // either side's bounded evidence was truncated, downgrade: counts may under-
    // report and identities may be partial, so a clean delta is not provable.
    let current_truncated = current_diagnostics
        .and_then(|d| {
            d.get("diagnostics_truncated")
                .or_else(|| d.get("truncated"))
        })
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || current_diagnostics
            .and_then(|d| d.get("failed_test_details_truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let previous_truncated = previous_diagnostics
        .and_then(|d| {
            d.get("diagnostics_truncated")
                .or_else(|| d.get("truncated"))
        })
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || previous_diagnostics
            .and_then(|d| d.get("failed_test_details_truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if current_truncated || previous_truncated {
        return ScopeComparability {
            comparable: false,
            reason_code: "evidence_truncated",
        };
    }

    ScopeComparability {
        comparable: true,
        reason_code: "",
    }
}

fn delta_counts(current: &Value, previous: &Value) -> ValidationDeltaCounts {
    let (cur_passed, cur_failed, cur_ignored, cur_total) = test_counts(current);
    let (prev_passed, prev_failed, prev_ignored, prev_total) = test_counts(previous);
    ValidationDeltaCounts {
        passed_delta: delta_signed(cur_passed, prev_passed),
        failed_delta: delta_signed(cur_failed, prev_failed),
        ignored_delta: delta_signed(cur_ignored, prev_ignored),
        total_delta: delta_signed(cur_total, prev_total),
    }
}

fn delta_signed(current: u64, previous: u64) -> i64 {
    if current >= previous {
        (current - previous) as i64
    } else {
        -((previous - current) as i64)
    }
}

/// Extract (passed, failed, ignored, total) from a validation event's
/// diagnostics test_summary, falling back to 0 when absent.
fn test_counts(event: &Value) -> (u64, u64, u64, u64) {
    let summary = event.get("diagnostics").and_then(|d| d.get("test_summary"));
    let passed = summary
        .and_then(|s| s.get("passed"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let failed = summary
        .and_then(|s| s.get("failed"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let ignored = summary
        .and_then(|s| s.get("ignored"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = passed + failed + ignored;
    (passed, failed, ignored, total)
}

/// True when a test validation event did not actually execute any tests
/// (`zero_tests_run` flag, or a parsed test summary with zero run and zero
/// failures). Such a run is evidence-poor: it cannot prove prior failures were
/// resolved, only that nothing ran.
fn current_run_did_not_execute_tests(event: &Value) -> bool {
    if event
        .get("zero_tests_run")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    // Missing count metadata is not evidence that zero tests ran. Only fall
    // back to a parsed test summary when that summary actually exists.
    let Some(summary) = event
        .get("diagnostics")
        .and_then(|diagnostics| diagnostics.get("test_summary"))
        .filter(|summary| summary.is_object())
    else {
        return false;
    };
    let passed = summary.get("passed").and_then(Value::as_u64).unwrap_or(0);
    let failed = summary.get("failed").and_then(Value::as_u64).unwrap_or(0);
    let ignored = summary.get("ignored").and_then(Value::as_u64).unwrap_or(0);
    let total = passed + failed + ignored;
    total == 0 && failed == 0 && passed == 0 && ignored == 0
}

struct DeltaFailures {
    newly_failed: Vec<FailureIdentity>,
    resolved: Vec<FailureIdentity>,
    still_failing: Vec<FailureIdentity>,
    total_newly_failed: usize,
    total_resolved: usize,
    total_still_failing: usize,
    list_truncated: bool,
}

/// Compare the stable failure identities of two comparable validation events.
/// Identities come from the existing parser's `failed_test_details` (test
/// names) and `diagnostics` (rustc error codes / file:line). We never diff raw
/// stdout/stderr lines.
fn delta_failures(current: &Value, previous: &Value) -> (DeltaFailures, Option<&'static str>) {
    let current_failures = failure_identities(current);
    let previous_failures = failure_identities(previous);

    // Only test-kind validation events carry stable per-failure identities.
    let current_kind = current.get("validation_kind").and_then(Value::as_str);
    let previous_kind = previous.get("validation_kind").and_then(Value::as_str);
    let is_test_like =
        matches!(current_kind, Some("test")) && matches!(previous_kind, Some("test"));

    if !is_test_like {
        // Non-test validation (check/format) carries diagnostics rather than
        // test names; comparing diagnostic sets is meaningful but coarse. We
        // still surface it when both sides parsed diagnostics.
        let current_parsed = current
            .get("diagnostics")
            .and_then(|d| d.get("available"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let previous_parsed = previous
            .get("diagnostics")
            .and_then(|d| d.get("available"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !current_parsed || !previous_parsed {
            return (
                DeltaFailures {
                    newly_failed: Vec::new(),
                    resolved: Vec::new(),
                    still_failing: Vec::new(),
                    total_newly_failed: 0,
                    total_resolved: 0,
                    total_still_failing: 0,
                    list_truncated: false,
                },
                Some("test_identity_unavailable"),
            );
        }
    }

    let current_set: BTreeSet<&FailureIdentity> = current_failures.iter().collect();
    let previous_set: BTreeSet<&FailureIdentity> = previous_failures.iter().collect();

    // A zero-test success (no tests actually executed) must NOT be treated as
    // resolving prior test failures: the absence of failures is not evidence
    // they were fixed. Downgrade the whole identity diff to unavailable with a
    // stable reason so callers do not see a false "resolved" set.
    if is_test_like && !previous_set.is_empty() && current_run_did_not_execute_tests(current) {
        return (
            DeltaFailures {
                newly_failed: Vec::new(),
                resolved: Vec::new(),
                still_failing: Vec::new(),
                total_newly_failed: 0,
                total_resolved: 0,
                total_still_failing: 0,
                list_truncated: false,
            },
            Some("test_identity_unavailable"),
        );
    }

    let mut newly_failed: Vec<FailureIdentity> = current_set
        .iter()
        .filter(|identity| !previous_set.contains(*identity))
        .map(|identity| (*identity).clone())
        .collect();
    let mut resolved: Vec<FailureIdentity> = previous_set
        .iter()
        .filter(|identity| !current_set.contains(*identity))
        .map(|identity| (*identity).clone())
        .collect();
    let mut still_failing: Vec<FailureIdentity> = current_set
        .iter()
        .filter(|identity| previous_set.contains(*identity))
        .map(|identity| (*identity).clone())
        .collect();

    // Deterministic ordering.
    newly_failed.sort_by(compare_failure_identity);
    resolved.sort_by(compare_failure_identity);
    still_failing.sort_by(compare_failure_identity);

    let total_newly_failed = newly_failed.len();
    let total_resolved = resolved.len();
    let total_still_failing = still_failing.len();
    let list_truncated = newly_failed.len() > MAX_FAILURE_IDENTITIES
        || resolved.len() > MAX_FAILURE_IDENTITIES
        || still_failing.len() > MAX_FAILURE_IDENTITIES;
    newly_failed.truncate(MAX_FAILURE_IDENTITIES);
    resolved.truncate(MAX_FAILURE_IDENTITIES);
    still_failing.truncate(MAX_FAILURE_IDENTITIES);

    (
        DeltaFailures {
            newly_failed,
            resolved,
            still_failing,
            total_newly_failed,
            total_resolved,
            total_still_failing,
            list_truncated,
        },
        None,
    )
}

/// Extract stable failure identities from a validation event's parser output.
/// Prefers test names (`failed_test_details`), then rustc diagnostics (error
/// code + file:line). Never uses raw stdout/stderr text.
fn failure_identities(event: &Value) -> Vec<FailureIdentity> {
    let mut identities = Vec::new();
    let diagnostics = event.get("diagnostics");

    if let Some(details) = diagnostics
        .and_then(|d| d.get("failed_test_details"))
        .and_then(Value::as_array)
    {
        for detail in details {
            let name = detail.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            identities.push(FailureIdentity {
                kind: "test",
                name: name.to_string(),
                file: detail
                    .get("file")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                line: detail.get("line").and_then(Value::as_u64),
            });
        }
    }

    if let Some(diags) = diagnostics
        .and_then(|d| d.get("diagnostics"))
        .and_then(Value::as_array)
    {
        for diag in diags {
            if diag.get("severity").and_then(Value::as_str) != Some("error") {
                continue;
            }
            let code = diag.get("code").and_then(Value::as_str);
            let file = diag.get("file").and_then(Value::as_str);
            let line = diag.get("line").and_then(Value::as_u64);
            let name = match (code, file, line) {
                (Some(code), _, _) => format!("error:{code}"),
                (None, Some(file), Some(line)) => format!("{file}:{line}"),
                _ => continue,
            };
            identities.push(FailureIdentity {
                kind: "diagnostic",
                name,
                file: file.map(str::to_string),
                line,
            });
        }
    }

    identities
}

fn compare_failure_identity(left: &FailureIdentity, right: &FailureIdentity) -> std::cmp::Ordering {
    left.kind
        .cmp(right.kind)
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| {
            optional_string_sort_key(left.file.as_deref())
                .cmp(&optional_string_sort_key(right.file.as_deref()))
        })
        .then_with(|| {
            left.line
                .unwrap_or(u64::MAX)
                .cmp(&right.line.unwrap_or(u64::MAX))
        })
}

fn optional_string_sort_key(value: Option<&str>) -> (bool, &str) {
    (value.is_none(), value.unwrap_or_default())
}

fn attempt_open_failures(validation: &Value) -> (Vec<FailureIdentity>, usize, bool) {
    let mut failures = validation
        .pointer("/unresolved_failures/events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(failure_identities)
        .collect::<Vec<_>>();
    failures.sort_by(compare_failure_identity);
    failures.dedup();
    let total = failures.len();
    let truncated = total > MAX_FAILURE_IDENTITIES;
    failures.truncate(MAX_FAILURE_IDENTITIES);
    (failures, total, truncated)
}

fn bounded_excerpt(value: &str, max_chars: usize) -> (String, bool) {
    let mut chars = value.chars();
    let excerpt = chars.by_ref().take(max_chars).collect::<String>();
    let truncated = chars.next().is_some();
    (excerpt, truncated)
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

fn to_value<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|_| {
        json!({
            "status": "unknown",
            "reason_code": "continuation_feedback_unavailable",
            "deterministic": true,
            "llm_summary": false,
        })
    })
}
