//! Human-facing, read-only Workflow Session projection for the host Console.
//!
//! This is deliberately a projection of the existing bounded Session ledger.
//! It does not create observation truth, infer execution outcomes, or expose raw
//! arguments/output. Correlation is presentation-only via `call_id`.

use serde::Serialize;
use std::collections::{HashMap, HashSet};

use super::events::normalize_observed_project_path;
use super::model::{SessionEvent, SessionMessageKind, SessionRecord};
use super::query::build_messages_summary;
use super::util::{bound_chars, looks_like_secret_string};
use webcodex_core::workflow_session_contract::is_safe_job_id;

#[derive(Clone, Copy)]
pub struct ConsoleValidationHooks {
    pub event_observes_validation_activity: fn(&SessionEvent) -> bool,
    pub validation_summary_from_events: fn(&[SessionEvent], usize) -> serde_json::Value,
}

pub const DEFAULT_CONSOLE_SESSION_LIST_LIMIT: usize = 20;
pub const MAX_CONSOLE_SESSION_LIST_LIMIT: usize = 50;
pub const DEFAULT_CONSOLE_ACTIVITY_LIMIT: usize = 100;
pub const MAX_CONSOLE_ACTIVITY_LIMIT: usize = 200;
const MAX_CONSOLE_TEXT_CHARS: usize = 240;
const MAX_CONSOLE_PATHS_PER_ITEM: usize = 8;
const MAX_CONSOLE_LIST_TEXT_CHARS: usize = 120;
const MAX_CONSOLE_LIST_PATHS_PER_ITEM: usize = 3;
const MAX_CONSOLE_GROUP_TOOLS: usize = 8;

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleList {
    pub sessions: Vec<WorkflowSessionConsoleListItem>,
    pub total: usize,
    pub returned: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleListItem {
    pub session_id: String,
    pub title: String,
    pub lifecycle: String,
    pub mode: String,
    pub updated_at: i64,
    pub running_call: bool,
    pub running_jobs: usize,
    pub running_jobs_complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_activity: Option<WorkflowSessionConsoleActivityPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<WorkflowSessionConsoleActivityPreview>,
    pub overview: WorkflowSessionConsoleOverview,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleOverview {
    pub work: WorkflowSessionConsoleWorkOverview,
    pub validation: WorkflowSessionConsoleValidationOverview,
    pub attention: WorkflowSessionConsoleAttentionOverview,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_progress: Option<WorkflowSessionConsoleReportedProgress>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleWorkOverview {
    pub exploration: usize,
    pub edits: usize,
    pub reviews: usize,
    pub validations: usize,
    pub runs: usize,
    pub history_complete: bool,
    pub history_truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleValidationOverview {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_at: Option<i64>,
    pub unresolved_failure_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests_run_count: Option<u64>,
    pub history_complete: bool,
    pub history_truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleAttentionOverview {
    pub open_guidance: usize,
    pub open_questions: usize,
    pub open_risks: usize,
    pub open_todos: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleAggregate {
    pub retained_sessions: usize,
    pub returned_sessions: usize,
    pub sessions_truncated: bool,
    pub active_sessions: usize,
    pub running_sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_updated_at: Option<i64>,
    pub attention: WorkflowSessionConsoleAttentionOverview,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleReportedProgress {
    pub reported_at: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleActivityPreview {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_state: Option<String>,
    pub job_handoff: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleDetail {
    pub session_id: String,
    pub title: String,
    pub lifecycle: String,
    pub mode: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub running_call: bool,
    pub running_jobs: usize,
    pub running_jobs_complete: bool,
    pub overview: WorkflowSessionConsoleOverview,
    pub activity: Vec<WorkflowSessionConsoleActivity>,
    pub activity_total: usize,
    pub activity_returned: usize,
    pub activity_truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSessionConsoleActivity {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_state: Option<String>,
    pub job_handoff: bool,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_count: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub group_kinds: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub group_tools: Vec<String>,
}

#[derive(Clone, Copy)]
struct Interaction<'a> {
    start: Option<&'a SessionEvent>,
    finish: Option<&'a SessionEvent>,
    sequence: usize,
}

struct OrderedActivity {
    activity: WorkflowSessionConsoleActivity,
    ledger_sequence: Option<usize>,
    fallback_sequence: usize,
}

pub(super) fn normalize_console_session_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_CONSOLE_SESSION_LIST_LIMIT)
        .clamp(1, MAX_CONSOLE_SESSION_LIST_LIMIT)
}

pub(super) fn normalize_console_activity_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_CONSOLE_ACTIVITY_LIMIT)
        .clamp(1, MAX_CONSOLE_ACTIVITY_LIMIT)
}

pub(super) fn build_list_item(
    record: &SessionRecord,
    project: &str,
    validation: ConsoleValidationHooks,
) -> WorkflowSessionConsoleListItem {
    let interactions = build_interactions(record);
    let running_call = interactions
        .iter()
        .any(|interaction| interaction.finish.is_none() && interaction.start.is_some());
    // `queued` / `running` / `started` on a finished Job handoff is only the
    // bounded execution snapshot observed when the tool returned. Job terminal
    // lifecycle does not write back into the Session event, so only an actually
    // unfinished correlated tool call is truthful current work here.
    let current = interactions
        .iter()
        .rev()
        .copied()
        .find(|interaction| interaction.finish.is_none() && interaction.start.is_some());
    let current_sequence = current.map(|interaction| interaction.sequence);
    let last = interactions.iter().rev().copied().find(|interaction| {
        interaction.finish.is_some()
            && Some(interaction.sequence) != current_sequence
            && interaction
                .finish
                .or(interaction.start)
                .is_some_and(|event| !interaction_is_progress_metadata(event))
    });
    let overview = build_overview(record, &interactions, false, validation);
    WorkflowSessionConsoleListItem {
        session_id: record.session_id.clone(),
        title: console_title(record.title.as_deref()),
        lifecycle: record.lifecycle.as_str().to_string(),
        mode: record.mode.as_str().to_string(),
        updated_at: record.updated_at,
        running_call,
        running_jobs: 0,
        running_jobs_complete: false,
        current_activity: current.map(|interaction| activity_preview(interaction, project)),
        last_activity: last.map(|interaction| activity_preview(interaction, project)),
        overview,
    }
}

pub(super) fn build_detail(
    record: &SessionRecord,
    project: &str,
    limit: usize,
    validation: ConsoleValidationHooks,
) -> WorkflowSessionConsoleDetail {
    let interactions = build_interactions(record);
    let running_call = interactions
        .iter()
        .any(|interaction| interaction.finish.is_none() && interaction.start.is_some());
    let overview = build_overview(record, &interactions, true, validation);
    let mut ordered_activity = interactions
        .into_iter()
        .map(|interaction| OrderedActivity {
            activity: activity_from_interaction(interaction, project),
            ledger_sequence: Some(interaction.sequence),
            fallback_sequence: interaction.sequence,
        })
        .collect::<Vec<_>>();
    for (fallback_sequence, message) in record
        .messages
        .iter()
        .filter(|message| message.kind == SessionMessageKind::Progress)
        .enumerate()
    {
        ordered_activity.push(OrderedActivity {
            activity: WorkflowSessionConsoleActivity {
                kind: "Progress".to_string(),
                tool: None,
                state: "info".to_string(),
                execution_state: None,
                job_handoff: false,
                started_at: message.created_at,
                finished_at: None,
                duration_ms: None,
                exit_code: None,
                job_id: None,
                summary: Some(console_safe_text(&message.message, MAX_CONSOLE_TEXT_CHARS)),
                paths: Vec::new(),
                group_count: None,
                group_kinds: Vec::new(),
                group_tools: Vec::new(),
            },
            ledger_sequence: None,
            fallback_sequence,
        });
    }
    sort_ordered_activity(&mut ordered_activity);
    let mut ordered_activity = group_exploration_activity(ordered_activity);
    let activity_total = ordered_activity.len();
    if ordered_activity.len() > limit {
        ordered_activity.drain(0..ordered_activity.len() - limit);
    }
    let activity = ordered_activity
        .into_iter()
        .map(|entry| entry.activity)
        .collect::<Vec<_>>();
    let activity_returned = activity.len();
    WorkflowSessionConsoleDetail {
        session_id: record.session_id.clone(),
        title: console_title(record.title.as_deref()),
        lifecycle: record.lifecycle.as_str().to_string(),
        mode: record.mode.as_str().to_string(),
        created_at: record.created_at,
        updated_at: record.updated_at,
        running_call,
        running_jobs: 0,
        running_jobs_complete: false,
        overview,
        activity,
        activity_total,
        activity_returned,
        activity_truncated: activity_total > activity_returned,
    }
}

fn build_overview(
    record: &SessionRecord,
    interactions: &[Interaction<'_>],
    include_reported_progress: bool,
    validation: ConsoleValidationHooks,
) -> WorkflowSessionConsoleOverview {
    let history_truncated = event_history_truncated(record);
    let mut work = WorkflowSessionConsoleWorkOverview {
        exploration: 0,
        edits: 0,
        reviews: 0,
        validations: 0,
        runs: 0,
        history_complete: !history_truncated,
        history_truncated,
    };
    for interaction in interactions
        .iter()
        .copied()
        .filter(|interaction| interaction.finish.is_some())
    {
        let evidence = interaction
            .finish
            .or(interaction.start)
            .expect("finished interaction evidence");
        match semantic_kind(evidence) {
            "Read" | "Searched" | "Navigated" => {
                work.exploration = work.exploration.saturating_add(1)
            }
            "Edited" => work.edits = work.edits.saturating_add(1),
            "Reviewed" => work.reviews = work.reviews.saturating_add(1),
            "Tested" => work.validations = work.validations.saturating_add(1),
            "Ran" => work.runs = work.runs.saturating_add(1),
            _ => {}
        }
    }

    let validation_activity_observed = interactions.iter().any(|interaction| {
        interaction
            .start
            .is_some_and(validation.event_observes_validation_activity)
            || interaction
                .finish
                .is_some_and(validation.event_observes_validation_activity)
    });

    let retained_events = record
        .events
        .iter()
        .map(|event| event.as_ref().clone())
        .collect::<Vec<_>>();
    // The validation module owns validation identity, parser, terminality, and
    // historical failure resolution. Console deliberately narrows that existing
    // aggregate instead of interpreting validation output itself.
    let validation_summary = (validation.validation_summary_from_events)(&retained_events, 1);
    let validation_available = validation_summary
        .get("available")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let latest_status = validation_summary
        .get("latest_status")
        .and_then(serde_json::Value::as_str);
    let unresolved_failure_count = validation_summary
        .get("unresolved_failures")
        .and_then(|value| value.get("count"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0);
    let latest = validation_summary.get("latest");
    let latest_kind = latest
        .and_then(|value| value.get("validation_kind"))
        .and_then(serde_json::Value::as_str)
        .and_then(safe_atom);
    let latest_at = latest.and_then(|value| {
        value
            .get("completed_at")
            .and_then(serde_json::Value::as_i64)
            .or_else(|| value.get("started_at").and_then(serde_json::Value::as_i64))
    });
    let tests_run_count = latest
        .and_then(|value| value.get("tests_run_count"))
        .and_then(serde_json::Value::as_u64);
    let validation_state = if !validation_available {
        if !history_truncated && latest_status == Some("not_run") && !validation_activity_observed {
            "not_run"
        } else {
            "unavailable"
        }
    } else if unresolved_failure_count > 0 || latest_status == Some("failed") {
        "failed"
    } else if latest_status == Some("passed") {
        "passed"
    } else if latest_status == Some("inconclusive") {
        "inconclusive"
    } else {
        "unavailable"
    };
    let validation = WorkflowSessionConsoleValidationOverview {
        state: validation_state.to_string(),
        latest_kind,
        latest_at,
        unresolved_failure_count,
        tests_run_count,
        history_complete: !history_truncated,
        history_truncated,
    };

    let messages = build_messages_summary(record);
    let attention = WorkflowSessionConsoleAttentionOverview {
        open_guidance: messages.pending_guidance,
        open_questions: messages.open_questions,
        open_risks: messages.open_risks,
        open_todos: messages.open_todos,
    };
    let reported_progress = include_reported_progress
        .then(|| {
            messages
                .recent_progress
                .first()
                .map(|message| WorkflowSessionConsoleReportedProgress {
                    reported_at: message.created_at,
                    text: console_safe_text(&message.message, MAX_CONSOLE_TEXT_CHARS),
                })
        })
        .flatten();

    WorkflowSessionConsoleOverview {
        work,
        validation,
        attention,
        reported_progress,
    }
}

pub fn aggregate_console_list(
    list: &WorkflowSessionConsoleList,
) -> WorkflowSessionConsoleAggregate {
    let mut attention = WorkflowSessionConsoleAttentionOverview {
        open_guidance: 0,
        open_questions: 0,
        open_risks: 0,
        open_todos: 0,
    };
    let mut active_sessions = 0usize;
    let mut running_sessions = 0usize;
    let mut latest_updated_at = None;
    for session in &list.sessions {
        if session.lifecycle == "active" {
            active_sessions = active_sessions.saturating_add(1);
        }
        if session.running_call || session.running_jobs > 0 {
            running_sessions = running_sessions.saturating_add(1);
        }
        latest_updated_at = Some(
            latest_updated_at.map_or(session.updated_at, |current: i64| {
                current.max(session.updated_at)
            }),
        );
        attention.open_guidance = attention
            .open_guidance
            .saturating_add(session.overview.attention.open_guidance);
        attention.open_questions = attention
            .open_questions
            .saturating_add(session.overview.attention.open_questions);
        attention.open_risks = attention
            .open_risks
            .saturating_add(session.overview.attention.open_risks);
        attention.open_todos = attention
            .open_todos
            .saturating_add(session.overview.attention.open_todos);
    }
    WorkflowSessionConsoleAggregate {
        retained_sessions: list.total,
        returned_sessions: list.returned,
        sessions_truncated: list.truncated,
        active_sessions,
        running_sessions,
        latest_updated_at,
        attention,
    }
}

fn event_history_truncated(record: &SessionRecord) -> bool {
    record.events_observed.max(record.events.len() as u64) > record.events.len() as u64
}

fn sort_ordered_activity(activity: &mut [OrderedActivity]) {
    activity.sort_by(|left, right| {
        left.activity
            .started_at
            .cmp(&right.activity.started_at)
            .then_with(|| match (left.ledger_sequence, right.ledger_sequence) {
                (Some(left), Some(right)) => left.cmp(&right),
                // Progress messages have no sequence comparable to Session events.
                // Keep same-second tool activity in authoritative ledger order and
                // place progress after it without claiming a finer causal order.
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => left.fallback_sequence.cmp(&right.fallback_sequence),
            })
    });
}

fn group_exploration_activity(activity: Vec<OrderedActivity>) -> Vec<OrderedActivity> {
    let ambiguous_progress_timestamps = activity
        .iter()
        .filter(|entry| entry.ledger_sequence.is_none() && entry.activity.kind == "Progress")
        .map(|entry| entry.activity.started_at)
        .collect::<HashSet<_>>();
    let mut grouped: Vec<OrderedActivity> = Vec::with_capacity(activity.len());
    for entry in activity {
        if !is_groupable_exploration(&entry.activity)
            || ambiguous_progress_timestamps.contains(&entry.activity.started_at)
        {
            // Progress has no sequence comparable to Session events. When a
            // timestamp contains Progress, any same-second tool placement is a
            // display fallback only, so do not merge exploration across that
            // ambiguity boundary.
            grouped.push(entry);
            continue;
        }
        let Some(previous) = grouped.last_mut() else {
            grouped.push(entry);
            continue;
        };
        if !is_groupable_exploration(&previous.activity) {
            grouped.push(entry);
            continue;
        }
        if previous.activity.group_count.is_none() {
            let first_kind = previous.activity.kind.clone();
            let first_tool = previous.activity.tool.clone();
            previous.activity.kind = "Explored".to_string();
            previous.activity.tool = None;
            previous.activity.duration_ms = None;
            previous.activity.exit_code = None;
            previous.activity.summary = None;
            previous.activity.group_count = Some(1);
            previous.activity.group_kinds = vec![first_kind];
            if let Some(tool) = first_tool {
                previous.activity.group_tools.push(tool);
            }
        }
        let next_kind = entry.activity.kind.clone();
        if !previous.activity.group_kinds.contains(&next_kind) {
            previous.activity.group_kinds.push(next_kind);
        }
        if let Some(tool) = entry.activity.tool.as_ref() {
            if previous.activity.group_tools.len() < MAX_CONSOLE_GROUP_TOOLS
                && !previous.activity.group_tools.contains(tool)
            {
                previous.activity.group_tools.push(tool.clone());
            }
        }
        for path in &entry.activity.paths {
            if previous.activity.paths.len() >= MAX_CONSOLE_PATHS_PER_ITEM {
                break;
            }
            if !previous.activity.paths.contains(path) {
                previous.activity.paths.push(path.clone());
            }
        }
        let count = previous.activity.group_count.unwrap_or(1).saturating_add(1);
        previous.activity.group_count = Some(count);
        previous.activity.finished_at =
            entry.activity.finished_at.or(previous.activity.finished_at);
        previous.activity.summary = Some(format!("{count} exploration activities"));
    }
    grouped
}

fn is_groupable_exploration(activity: &WorkflowSessionConsoleActivity) -> bool {
    activity.state == "succeeded"
        && !activity.job_handoff
        && activity.job_id.is_none()
        && matches!(
            activity.kind.as_str(),
            "Read" | "Searched" | "Navigated" | "Explored"
        )
}

fn build_interactions(record: &SessionRecord) -> Vec<Interaction<'_>> {
    let mut correlated: HashMap<&str, Interaction<'_>> = HashMap::new();
    let mut legacy_finished = Vec::new();

    for (sequence, event) in record.events.iter().enumerate() {
        let event = event.as_ref();
        match event.kind.as_str() {
            "tool_call_started" => {
                let Some(call_id) = event.call_id.as_deref() else {
                    // A legacy start cannot safely be labelled "running": its
                    // matching finish predates call correlation. Do not guess.
                    continue;
                };
                correlated.entry(call_id).or_insert(Interaction {
                    start: Some(event),
                    finish: None,
                    sequence,
                });
            }
            "tool_call_finished" => {
                let Some(call_id) = event.call_id.as_deref() else {
                    legacy_finished.push(Interaction {
                        start: None,
                        finish: Some(event),
                        sequence,
                    });
                    continue;
                };
                correlated
                    .entry(call_id)
                    .and_modify(|interaction| interaction.finish = Some(event))
                    .or_insert(Interaction {
                        start: None,
                        finish: Some(event),
                        sequence,
                    });
            }
            _ => {}
        }
    }

    let mut interactions: Vec<_> = correlated.into_values().collect();
    interactions.extend(legacy_finished);
    interactions.sort_by_key(|interaction| interaction.sequence);
    interactions
}

fn activity_preview(
    interaction: Interaction<'_>,
    project: &str,
) -> WorkflowSessionConsoleActivityPreview {
    let activity = activity_from_interaction(interaction, project);
    WorkflowSessionConsoleActivityPreview {
        kind: activity.kind,
        tool: activity.tool,
        state: activity.state,
        execution_state: activity.execution_state,
        job_handoff: activity.job_handoff,
        job_id: activity.job_id,
        summary: activity
            .summary
            .map(|summary| bound_chars(&summary, MAX_CONSOLE_LIST_TEXT_CHARS)),
        paths: activity
            .paths
            .into_iter()
            .take(MAX_CONSOLE_LIST_PATHS_PER_ITEM)
            .collect(),
    }
}

fn activity_from_interaction(
    interaction: Interaction<'_>,
    project: &str,
) -> WorkflowSessionConsoleActivity {
    let evidence = interaction
        .finish
        .or(interaction.start)
        .expect("interaction evidence");
    let started_at = interaction
        .start
        .and_then(|event| event.started_at)
        .or_else(|| interaction.finish.and_then(|event| event.started_at))
        .unwrap_or(evidence.timestamp);
    let finish = interaction.finish;
    let same_project = interaction
        .start
        .into_iter()
        .chain(finish)
        .all(|event| event_is_project_safe(event, project));
    let state = console_activity_state(interaction.start, finish);
    let execution_state = finish.and_then(console_execution_state).map(str::to_string);
    let job_id = same_project
        .then(|| {
            finish
                .and_then(|event| event.job_id.as_deref())
                .and_then(safe_job_id)
        })
        .flatten();
    let job_handoff = job_id.is_some()
        && finish.is_some_and(|event| is_job_handoff_tool(&event.tool_name))
        && execution_state.is_some();
    let paths = if same_project {
        safe_paths(interaction.start, finish)
    } else {
        Vec::new()
    };
    let summary = if same_project {
        console_interaction_summary(interaction.start, finish, &paths)
    } else {
        Some("cross-project details omitted".to_string())
    };
    WorkflowSessionConsoleActivity {
        kind: semantic_kind(evidence).to_string(),
        tool: Some(bound_chars(&evidence.tool_name, 80)),
        state,
        execution_state,
        job_handoff,
        started_at,
        finished_at: finish.and_then(|event| event.finished_at),
        duration_ms: finish.and_then(|event| event.duration_ms),
        exit_code: finish.and_then(|event| event.exit_code),
        job_id,
        summary,
        paths,
        group_count: None,
        group_kinds: Vec::new(),
        group_tools: Vec::new(),
    }
}

fn console_activity_state(start: Option<&SessionEvent>, finish: Option<&SessionEvent>) -> String {
    let Some(finish) = finish else {
        return if start.is_some() {
            "running"
        } else {
            "completed"
        }
        .to_string();
    };
    if let Some(execution_state) = console_execution_state(finish) {
        match execution_state {
            "queued" => return "queued".to_string(),
            "running" | "started" => return "running".to_string(),
            "outcome_unknown" => return "outcome_unknown".to_string(),
            "timed_out" => return "timed_out".to_string(),
            "cancelled" => return "cancelled".to_string(),
            "not_started" => return "not_started".to_string(),
            // Completion is execution truth, but success/failure remains the
            // ToolResult outcome already retained on the Session event.
            "completed" => {}
            _ => unreachable!("console_execution_state returns a closed allowlist"),
        }
    }
    match finish.status.as_deref() {
        Some("succeeded") => "succeeded",
        Some("failed") => "failed",
        Some(_) => "completed",
        None => "completed",
    }
    .to_string()
}

fn console_execution_state(event: &SessionEvent) -> Option<&str> {
    event
        .effect_evidence
        .as_ref()
        .and_then(|evidence| evidence.execution_state.as_deref())
        .or_else(|| {
            event
                .validation_output_summary
                .as_ref()
                .and_then(|summary| summary.get("execution_state"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::trim)
        .filter(|state| {
            matches!(
                *state,
                "queued"
                    | "running"
                    | "started"
                    | "outcome_unknown"
                    | "completed"
                    | "cancelled"
                    | "timed_out"
                    | "not_started"
            )
        })
}

fn is_job_handoff_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "cargo_test"
            | "cargo_check"
            | "cargo_fmt"
            | "go_test"
            | "run_process"
            | "run_script"
            | "run_shell"
            | "run_job"
    )
}

fn event_is_project_safe(event: &SessionEvent, project: &str) -> bool {
    event
        .resolved_project
        .as_deref()
        .or(event.project.as_deref())
        .is_none_or(|event_project| event_project == project)
}

fn safe_paths(start: Option<&SessionEvent>, finish: Option<&SessionEvent>) -> Vec<String> {
    let mut paths = Vec::new();
    for raw in start.into_iter().chain(finish).flat_map(|event| {
        event
            .observed_paths
            .iter()
            .chain(event.changed_paths.iter())
    }) {
        let Some(path) = normalize_observed_project_path(raw) else {
            continue;
        };
        if !paths.contains(&path) {
            paths.push(path);
        }
        if paths.len() >= MAX_CONSOLE_PATHS_PER_ITEM {
            break;
        }
    }
    paths
}

fn console_interaction_summary(
    start: Option<&SessionEvent>,
    finish: Option<&SessionEvent>,
    paths: &[String],
) -> Option<String> {
    let evidence = finish.or(start)?;
    let mut parts = Vec::new();
    if let Some(input) = start.and_then(|event| event.input_summary.as_ref()) {
        if let Some(count) = input_array_count(input, "items") {
            parts.push(format!("{count} items"));
        }
        if let Some(count) = input_array_count(input, "queries") {
            parts.push(format!("{count} searches"));
        }
        if let Some(count) = input_array_count(input, "changes") {
            parts.push(format!("{count} changes"));
        }
        if let Some(count) = input_array_count(input, "packages") {
            parts.push(format!("{count} package scopes"));
        }
    }
    if !paths.is_empty() {
        parts.push(format!(
            "{} path{}",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        ));
    }
    if let Some(validation) = finish.and_then(|event| event.validation_output_summary.as_ref()) {
        if let Some(count) = validation
            .get("tests_run_count")
            .and_then(|value| value.as_u64())
        {
            parts.push(format!("{count} tests"));
        } else if validation
            .get("zero_tests_run")
            .and_then(|value| value.as_bool())
            == Some(true)
        {
            parts.push("0 tests".to_string());
        }
        let handoff = finish.is_some_and(|event| {
            event.job_id.as_deref().and_then(safe_job_id).is_some()
                && is_job_handoff_tool(&event.tool_name)
        });
        if !handoff {
            if let Some(state) = validation
                .get("execution_state")
                .and_then(|value| value.as_str())
                .and_then(safe_atom)
            {
                parts.push(format!("execution {state}"));
            }
        }
        let output_lines = validation
            .get("stdout_lines")
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
            .saturating_add(
                validation
                    .get("stderr_lines")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0),
            );
        if output_lines > 0 {
            parts.push(format!("{output_lines} output lines (content omitted)"));
        }
    }
    if let Some(finish) = finish {
        let safe_job = finish.job_id.as_deref().and_then(safe_job_id);
        if safe_job.is_some() && is_job_handoff_tool(&finish.tool_name) {
            parts.push("handed off to Job".to_string());
            if let Some(state) = console_execution_state(finish).and_then(safe_atom) {
                parts.push(format!("execution {state}"));
            }
        } else if safe_job.is_some() {
            parts.push("job-backed".to_string());
        }
    }
    if finish.and_then(|event| event.exit_code).is_some() {
        parts.push(format!(
            "exit {}",
            finish.and_then(|event| event.exit_code).unwrap()
        ));
    }
    if parts.is_empty() && interaction_is_progress_metadata(evidence) {
        parts.push("session metadata".to_string());
    }
    (!parts.is_empty()).then(|| bound_chars(&parts.join(" · "), MAX_CONSOLE_TEXT_CHARS))
}

fn input_array_count(value: &serde_json::Value, key: &str) -> Option<usize> {
    value
        .get(key)
        .and_then(|value| value.as_array())
        .map(Vec::len)
        .filter(|count| *count > 0)
}

fn safe_atom(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then(|| value.to_string())
}

fn safe_job_id(value: &str) -> Option<String> {
    let value = value.trim();
    is_safe_job_id(value).then(|| value.to_string())
}

fn console_title(title: Option<&str>) -> String {
    title
        .map(|title| console_safe_text(title, MAX_CONSOLE_TEXT_CHARS))
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Untitled workflow session".to_string())
}

fn console_safe_text(value: &str, max_chars: usize) -> String {
    if looks_like_secret_string(value) || contains_secret_assignment(value) {
        return "[redacted]".to_string();
    }
    let sanitized = value
        .split_whitespace()
        .map(|part| {
            if looks_like_private_path(part) {
                "[private path]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    bound_chars(&sanitized, max_chars)
}

fn contains_secret_assignment(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "token=",
        "secret=",
        "password=",
        "authorization=",
        "authorization:",
        "api_key=",
        "apikey=",
        "access_key=",
        "private_key=",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn looks_like_private_path(value: &str) -> bool {
    let token = trim_path_wrappers(value);
    if looks_like_absolute_path(token) {
        return true;
    }
    if token
        .rsplit_once('=')
        .map(|(_, candidate)| trim_path_wrappers(candidate))
        .is_some_and(looks_like_absolute_path)
    {
        return true;
    }
    contains_delimited_absolute_path(token)
}

fn contains_delimited_absolute_path(value: &str) -> bool {
    let mut previous = None;
    for (index, ch) in value.char_indices() {
        let delimited = index == 0
            || previous.is_some_and(|previous| {
                matches!(
                    previous,
                    '"' | '\'' | '(' | '[' | '{' | '<' | ',' | ';' | '='
                )
            });
        if delimited && looks_like_absolute_path(trim_path_wrappers(&value[index..])) {
            return true;
        }
        previous = Some(ch);
    }
    false
}

fn trim_path_wrappers(value: &str) -> &str {
    value.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
        )
    })
}

fn looks_like_absolute_path(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("file://") || value.starts_with('/') || value.starts_with("\\\\") {
        return true;
    }
    let bytes = value.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
        && bytes
            .get(2)
            .is_some_and(|byte| matches!(*byte, b'\\' | b'/'))
}

fn semantic_kind(event: &SessionEvent) -> &'static str {
    match event.tool_name.as_str() {
        "read_files"
        | "list_project_files"
        | "list_project_tracked_files"
        | "project_overview"
        | "read_project_artifact"
        | "read_project_artifact_metadata" => "Read",
        "search_project_texts" => "Searched",
        "lsp_status"
        | "document_symbols"
        | "document_diagnostics"
        | "hover"
        | "workspace_symbols"
        | "goto_definition"
        | "find_references"
        | "call_hierarchy" => "Navigated",
        "apply_text_edits"
        | "apply_unified_diff"
        | "write_project_file"
        | "delete_project_files"
        | "git_restore_paths"
        | "discard_untracked"
        | "workspace_checkpoint_restore" => "Edited",
        "git_status"
        | "git_diff_hunks"
        | "git_review_summary"
        | "git_log"
        | "show_changes"
        | "workspace_hygiene_check"
        | "finish_coding_task" => "Reviewed",
        "cargo_test" | "cargo_check" | "cargo_fmt" | "go_test" => "Tested",
        "run_process"
        | "run_script"
        | "run_shell"
        | "run_job"
        | "open_session_shell"
        | "session_shell_exec"
        | "session_shell_status"
        | "close_session_shell"
        | "observe_jobs"
        | "stop_job" => "Ran",
        _ if event.write_like => "Edited",
        _ if event.git_like || event.change_summary_like => "Reviewed",
        _ if event.shell_like => "Ran",
        _ if event.read_like => "Read",
        _ => "Used",
    }
}

fn interaction_is_progress_metadata(event: &SessionEvent) -> bool {
    matches!(
        event.tool_name.as_str(),
        "post_session_message" | "resolve_session_message" | "update_session_context"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordered(
        kind: &str,
        ledger_sequence: Option<usize>,
        fallback_sequence: usize,
    ) -> OrderedActivity {
        OrderedActivity {
            activity: WorkflowSessionConsoleActivity {
                kind: kind.to_string(),
                tool: None,
                state: "succeeded".to_string(),
                execution_state: None,
                job_handoff: false,
                started_at: 1_234,
                finished_at: Some(1_234),
                duration_ms: Some(0),
                exit_code: None,
                job_id: None,
                summary: None,
                paths: Vec::new(),
                group_count: None,
                group_kinds: Vec::new(),
                group_tools: Vec::new(),
            },
            ledger_sequence,
            fallback_sequence,
        }
    }

    #[test]
    fn same_second_progress_timestamp_blocks_exploration_grouping_across_ambiguity() {
        // The source facts are Read -> Progress -> Read in one second, but
        // Progress has no ledger sequence comparable to either tool event.
        let mut activity = vec![
            ordered("Read", Some(1), 1),
            ordered("Progress", None, 1),
            ordered("Read", Some(2), 2),
        ];
        activity[1].activity.state = "info".to_string();
        sort_ordered_activity(&mut activity);
        let grouped = group_exploration_activity(activity);
        let kinds = grouped
            .iter()
            .map(|entry| entry.activity.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(kinds, vec!["Read", "Read", "Progress"]);
        assert!(grouped
            .iter()
            .all(|entry| entry.activity.group_count.is_none()));
    }

    #[test]
    fn same_second_tool_activity_uses_ledger_sequence_and_progress_is_conservative() {
        let mut activity = vec![
            ordered("Read", Some(4), 4),
            ordered("Progress", None, 1),
            ordered("Searched", Some(1), 1),
            ordered("Progress later", None, 2),
        ];
        sort_ordered_activity(&mut activity);
        let kinds = activity
            .iter()
            .map(|entry| entry.activity.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec!["Searched", "Read", "Progress", "Progress later"]
        );
    }

    #[test]
    fn aggregate_console_list_preserves_bounds_and_attention_counts() {
        let overview = WorkflowSessionConsoleOverview {
            work: WorkflowSessionConsoleWorkOverview {
                exploration: 0,
                edits: 0,
                reviews: 0,
                validations: 0,
                runs: 0,
                history_complete: true,
                history_truncated: false,
            },
            validation: WorkflowSessionConsoleValidationOverview {
                state: "none".to_string(),
                latest_kind: None,
                latest_at: None,
                unresolved_failure_count: 0,
                tests_run_count: None,
                history_complete: true,
                history_truncated: false,
            },
            attention: WorkflowSessionConsoleAttentionOverview {
                open_guidance: 1,
                open_questions: 2,
                open_risks: 3,
                open_todos: 4,
            },
            reported_progress: None,
        };
        let list = WorkflowSessionConsoleList {
            sessions: vec![WorkflowSessionConsoleListItem {
                session_id: "wc_sess_test".to_string(),
                title: "test".to_string(),
                lifecycle: "active".to_string(),
                mode: "normal".to_string(),
                updated_at: 42,
                running_call: true,
                running_jobs: 0,
                running_jobs_complete: true,
                current_activity: None,
                last_activity: None,
                overview,
            }],
            total: 9,
            returned: 1,
            truncated: true,
        };
        let aggregate = aggregate_console_list(&list);
        assert_eq!(aggregate.retained_sessions, 9);
        assert_eq!(aggregate.returned_sessions, 1);
        assert!(aggregate.sessions_truncated);
        assert_eq!(aggregate.active_sessions, 1);
        assert_eq!(aggregate.running_sessions, 1);
        assert_eq!(aggregate.latest_updated_at, Some(42));
        assert_eq!(aggregate.attention.open_todos, 4);
        assert_eq!(aggregate.attention.open_questions, 2);
    }
}
