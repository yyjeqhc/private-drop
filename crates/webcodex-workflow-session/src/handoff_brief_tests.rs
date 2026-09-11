use crate::{
    build_handoff_brief, continuation_feedback_value, handoff_brief_size, CodingSessionRequest,
    ContinuationFeedbackInput, ContinuationProjectionHooks, ContinuationToolFailureSnapshot,
    ContinuationValidationSnapshot, HandoffBriefInput, PostSessionMessageInput,
    SessionDiscussionSummary, SessionGuards, SessionMessageKind, SessionMessagePriority,
    SessionPathHint, SessionStore, SessionToolContract, SessionTransport,
    HANDOFF_BRIEF_HARD_MAX_BYTES, HANDOFF_CHANGED_PATHS_MAX_ITEMS, HANDOFF_INSTRUCTION_MAX_CHARS,
    HANDOFF_NEXT_ACTIONS_MAX_ITEMS, HANDOFF_OPEN_FAILURES_MAX_ITEMS,
    HANDOFF_RECENT_FILES_MAX_ITEMS, TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use webcodex_core::workflow_session_contract::SessionMode;

const PROJECT: &str = "test-project";

fn store_with_limit(max_events: usize) -> SessionStore {
    SessionStore::new(16, max_events)
}

fn start_session(store: &SessionStore, title: &str) -> String {
    store
        .start_session(Some(PROJECT.to_string()), Some(title.to_string()))
        .session_id
}

fn add_instruction(store: &SessionStore, session_id: &str, instruction: &str) {
    store
        .ensure_coding_session(CodingSessionRequest {
            project: PROJECT.to_string(),
            authority_fingerprint: TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT.to_string(),
            resume_session_id: Some(session_id.to_string()),
            instruction: Some(instruction.to_string()),
            mode: SessionMode::Normal,
            guards: SessionGuards::default(),
            execution_context: None,
            project_instructions: None,
            transport: SessionTransport::Api,
            context_refreshed: true,
            write_scope_verified: true,
        })
        .unwrap();
}

fn synthetic_read_contract() -> SessionToolContract {
    SessionToolContract {
        risk_class: "read",
        read_like: true,
        write_like: false,
        shell_like: false,
        git_like: false,
        change_summary_like: false,
        project_write: false,
        path_hint: SessionPathHint::SinglePath,
        accepts_context_ack: false,
        advances_context_checkpoint: false,
    }
}

fn synthetic_write_contract() -> SessionToolContract {
    SessionToolContract {
        risk_class: "write",
        read_like: false,
        write_like: true,
        shell_like: false,
        git_like: false,
        change_summary_like: false,
        project_write: true,
        path_hint: SessionPathHint::PathList,
        accepts_context_ack: false,
        advances_context_checkpoint: false,
    }
}

fn projection_hooks() -> ContinuationProjectionHooks {
    ContinuationProjectionHooks::new(|tool| tool == "synthetic_write")
}

fn jobs_summary(running: u64, recovering: u64, terminal_pending: u64) -> Value {
    json!({
        "active_count": running + terminal_pending,
        "running_count": running,
        "recovering_count": recovering,
        "terminal_pending_count": terminal_pending,
        "blocking_active_count": running,
        "nonblocking_active_count": terminal_pending,
        "recent": [],
        "truncated": false,
    })
}

fn empty_jobs() -> Value {
    jobs_summary(0, 0, 0)
}

fn clean_workspace() -> Value {
    json!({
        "git_available": true,
        "clean": true,
        "branch": "main",
        "head": {"commit": "0123456789abcdef0123456789abcdef01234567"},
        "counts": {"modified": 0, "added": 0, "deleted": 0, "renamed": 0, "copied": 0, "untracked": 0, "conflicted": 0},
    })
}

fn dirty_workspace(conflicted: u64) -> Value {
    json!({
        "git_available": true,
        "clean": false,
        "branch": "main",
        "head": {"commit": "0123456789abcdef0123456789abcdef01234567"},
        "counts": {"modified": 1, "added": 0, "deleted": 0, "renamed": 0, "copied": 0, "untracked": 0, "conflicted": conflicted},
    })
}

fn not_run_validation() -> Value {
    json!({
        "available": false,
        "status": "not_run",
        "latest_status": "not_run",
        "current_evidence": {
            "status": "not_run",
            "latest_status": "not_run",
            "unresolved_failure_count": 0,
            "events_total": 0,
            "stale_failure_count": 0,
        },
        "unresolved_failures": {"count": 0, "events": []},
        "events": [],
    })
}

fn passed_validation() -> Value {
    json!({
        "available": true,
        "status": "passed",
        "latest_status": "passed",
        "current_evidence": {
            "status": "passed",
            "latest_status": "passed",
            "unresolved_failure_count": 0,
            "events_total": 1,
            "stale_failure_count": 0,
        },
        "unresolved_failures": {"count": 0, "events": []},
        "events": [],
    })
}

fn failed_validation(names: &[String]) -> Value {
    let event = json!({
        "identity": "validation-event-1",
        "validation_kind": "test",
        "tool_name": "cargo_test",
        "purpose": "test",
        "cwd": ".",
        "command_summary": "cargo test --lib",
        "success": false,
        "completed_at": 1,
        "diagnostics": {
            "available": true,
            "parser": "structured_validation_parser",
            "diagnostics": [],
            "failed_test_details": names.iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
            "test_summary": {"passed": 0, "failed": names.len(), "ignored": 0},
            "diagnostics_truncated": false,
            "failed_test_details_truncated": false,
        },
    });
    json!({
        "available": true,
        "status": "failed",
        "latest_status": "failed",
        "current_evidence": {
            "status": "failed",
            "latest_status": "failed",
            "unresolved_failure_count": names.len(),
            "events_total": 1,
            "stale_failure_count": 0,
        },
        "unresolved_failures": {"count": names.len(), "events": [event.clone()]},
        "current_validation": {"latest": event.clone(), "unresolved_failures": {"events": [event.clone()]}},
        "events": [event],
    })
}

fn discussion(store: &SessionStore, session_id: &str) -> SessionDiscussionSummary {
    store.discussion_summary(session_id, Some(20)).unwrap()
}

fn raw_failed_event_ids(summary: &crate::SessionSummary) -> HashSet<String> {
    crate::canonical_tool_call_finished_events(&summary.events)
        .into_iter()
        .filter(|event| event.status.as_deref() == Some("failed"))
        .map(|event| event.event_id.clone())
        .collect()
}

fn current_snapshot_values(validation: &Value) -> (Value, Value) {
    let evidence = validation
        .get("current_evidence")
        .cloned()
        .unwrap_or_else(|| json!({"status": "unknown"}));
    let current = validation
        .get("current_validation")
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "latest": null,
                "unresolved_failures": {"events": []},
            })
        });
    (evidence, current)
}

#[allow(clippy::too_many_arguments)]
fn brief_for(
    store: &SessionStore,
    session_id: &str,
    workspace_requested: bool,
    workspace: Option<&Value>,
    validation_requested: bool,
    validation_override: Option<&Value>,
    jobs: Option<&Value>,
    guidance_available: bool,
) -> Value {
    let summary = store.summary(session_id, Some(200)).unwrap();
    let validation = validation_override
        .cloned()
        .unwrap_or_else(not_run_validation);
    let validation_not_requested = json!({"available": false, "not_requested": true});
    let feedback_validation = if validation_requested {
        &validation
    } else {
        &validation_not_requested
    };
    let (current_evidence, current_validation) = current_snapshot_values(&validation);
    let discussion = discussion(store, session_id);
    let null_jobs = Value::Null;
    let feedback_jobs = jobs.unwrap_or(&null_jobs);
    let actionable = raw_failed_event_ids(&summary);
    let continuation = continuation_feedback_value(ContinuationFeedbackInput {
        session_summary: &summary,
        validation: feedback_validation,
        jobs: feedback_jobs,
        discussion: &discussion,
        continuation: "continued",
        suggest_exploration_continuity: false,
        workspace_conflicts: workspace
            .and_then(|value| value.pointer("/counts/conflicted"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0,
        hooks: projection_hooks(),
        current_validation: ContinuationValidationSnapshot::new(
            &current_evidence,
            &current_validation,
        ),
        tool_failures: ContinuationToolFailureSnapshot::new(&actionable),
    });
    build_handoff_brief(HandoffBriefInput {
        session_summary: &summary,
        continuation_feedback: &continuation,
        workspace_requested,
        workspace,
        validation_requested,
        validation: Some(&validation),
        jobs,
        guidance_available,
        existing_suggested_actions: None,
    })
}

fn record_write(store: &SessionStore, session_id: &str, path: &str) {
    let start = store.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        "synthetic_write",
        &json!({"project": PROJECT, "paths": [path]}),
        synthetic_write_contract(),
    );
    store.record_tool_call_finished(start, true, &json!({"changed": true}), None, None);
}

fn record_read(store: &SessionStore, session_id: &str, path: &str) {
    let start = store.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        "read_files",
        &json!({"project": PROJECT, "items": [{"path": path}]}),
        synthetic_read_contract(),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({"items": [{"index": 0, "success": true, "output": {"path": path}}]}),
        None,
        None,
    );
}

#[test]
fn handoff_brief_fresh_active_session_is_stable_and_ready() {
    let store = store_with_limit(200);
    let session_id = start_session(&store, "implement deterministic handoff");
    let workspace = clean_workspace();
    let jobs = empty_jobs();
    let first = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        None,
        Some(&jobs),
        true,
    );
    let second = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        None,
        Some(&jobs),
        true,
    );
    assert_eq!(first, second);
    assert_eq!(first["version"], 1);
    assert_eq!(first["session"]["lifecycle"], "active");
    assert_eq!(
        first["task"]["root_instruction"]["excerpt"],
        "implement deterministic handoff"
    );
    assert!(first["task"]["latest_instruction"]["excerpt"].is_null());
    assert_eq!(first["workspace"]["status"], "available");
    assert_eq!(first["validation"]["status"], "not_run");
    assert_eq!(first["progress"]["state"], "ready_to_continue");
    assert_eq!(first["basis"]["complete"], true);
    assert_eq!(first["deterministic"], true);
    assert_eq!(first["llm_summary"], false);
}

#[test]
fn handoff_brief_root_and_latest_instructions_are_distinct_and_bounded() {
    let store = store_with_limit(200);
    let long_root = format!("root \"quoted\" \\\\ {}", "界".repeat(800));
    let session_id = start_session(&store, &long_root);
    let long_latest = format!("latest \"quoted\" \\\\ {}", "新".repeat(800));
    add_instruction(&store, &session_id, &long_latest);
    let workspace = clean_workspace();
    let jobs = empty_jobs();
    let brief = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        None,
        Some(&jobs),
        true,
    );
    let root = brief["task"]["root_instruction"]["excerpt"]
        .as_str()
        .unwrap();
    let latest = brief["task"]["latest_instruction"]["excerpt"]
        .as_str()
        .unwrap();
    assert_eq!(
        root,
        long_root
            .chars()
            .take(HANDOFF_INSTRUCTION_MAX_CHARS)
            .collect::<String>()
    );
    assert_eq!(
        latest,
        long_latest
            .chars()
            .take(HANDOFF_INSTRUCTION_MAX_CHARS)
            .collect::<String>()
    );
    assert_eq!(brief["task"]["root_instruction"]["truncated"], true);
    assert_eq!(brief["task"]["latest_instruction"]["truncated"], true);
}

#[test]
fn handoff_brief_progress_state_uses_only_proven_blockers() {
    let store = store_with_limit(200);
    let session_id = start_session(&store, "state matrix");
    let clean = clean_workspace();
    let dirty = dirty_workspace(0);
    let conflict = dirty_workspace(1);
    let empty = empty_jobs();
    let passed = passed_validation();
    assert_eq!(
        brief_for(
            &store,
            &session_id,
            true,
            Some(&clean),
            true,
            None,
            Some(&empty),
            true
        )["progress"]["state"],
        "ready_to_continue"
    );
    assert_eq!(
        brief_for(
            &store,
            &session_id,
            true,
            Some(&dirty),
            true,
            Some(&passed),
            Some(&empty),
            true
        )["progress"]["state"],
        "ready_to_continue"
    );
    assert_eq!(
        brief_for(
            &store,
            &session_id,
            true,
            Some(&dirty),
            true,
            None,
            Some(&empty),
            true
        )["progress"]["state"],
        "needs_validation"
    );
    assert_eq!(
        brief_for(
            &store,
            &session_id,
            true,
            Some(&conflict),
            true,
            Some(&passed),
            Some(&empty),
            true
        )["progress"]["state"],
        "blocked"
    );
    let blocking = jobs_summary(1, 0, 0);
    assert_eq!(
        brief_for(
            &store,
            &session_id,
            true,
            Some(&clean),
            true,
            Some(&passed),
            Some(&blocking),
            true
        )["progress"]["state"],
        "blocked"
    );
    let recovering = jobs_summary(1, 1, 0);
    assert_eq!(
        brief_for(
            &store,
            &session_id,
            true,
            Some(&clean),
            true,
            Some(&passed),
            Some(&recovering),
            true
        )["progress"]["state"],
        "blocked"
    );
    let terminal_pending = jobs_summary(0, 0, 1);
    let terminal = brief_for(
        &store,
        &session_id,
        true,
        Some(&clean),
        true,
        Some(&passed),
        Some(&terminal_pending),
        true,
    );
    assert_eq!(terminal["progress"]["state"], "ready_to_continue");
    assert_eq!(terminal["attention"]["terminal_pending_jobs"], 1);
}

#[test]
fn handoff_brief_unresolved_validation_failure_is_blocking_and_bounded() {
    let store = store_with_limit(200);
    let session_id = start_session(&store, "fix failing tests");
    add_instruction(&store, &session_id, "run focused validation");
    let failed_names = (0..8)
        .map(|i| format!("tests::handoff_failure_{i}"))
        .collect::<Vec<_>>();
    let validation = failed_validation(&failed_names);
    let workspace = clean_workspace();
    let jobs = empty_jobs();
    let brief = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        Some(&validation),
        Some(&jobs),
        true,
    );
    assert_eq!(brief["validation"]["status"], "failed");
    assert_eq!(brief["progress"]["state"], "blocked");
    assert_eq!(
        brief["validation"]["open_failures"]["items"]
            .as_array()
            .unwrap()
            .len(),
        HANDOFF_OPEN_FAILURES_MAX_ITEMS
    );
    assert_eq!(brief["validation"]["open_failures"]["total"], 8);
    assert_eq!(brief["validation"]["open_failures"]["truncated"], true);
}

#[test]
fn handoff_brief_caps_changed_paths_and_recent_files_without_mutating_exploration() {
    let store = store_with_limit(200);
    let session_id = start_session(&store, "bounded workset");
    add_instruction(&store, &session_id, "edit and inspect");
    for index in 0..15 {
        record_write(&store, &session_id, &format!("src/change_{index:02}.rs"));
    }
    for index in 0..10 {
        record_read(&store, &session_id, &format!("src/recent_{index:02}.rs"));
    }
    let before = store.summary(&session_id, Some(200)).unwrap();
    let workspace = dirty_workspace(0);
    let jobs = empty_jobs();
    let brief = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        None,
        Some(&jobs),
        true,
    );
    let after = store.summary(&session_id, Some(200)).unwrap();
    assert_eq!(before.events_total, after.events_total);
    assert_eq!(before.updated_at, after.updated_at);
    assert_eq!(
        brief["progress"]["changes"]["items"]
            .as_array()
            .unwrap()
            .len(),
        HANDOFF_CHANGED_PATHS_MAX_ITEMS
    );
    assert_eq!(brief["progress"]["changes"]["total"], 15);
    assert_eq!(brief["progress"]["changes"]["truncated"], true);
    let recent = brief["progress"]["recent_files"]["items"]
        .as_array()
        .unwrap();
    assert_eq!(recent.len(), HANDOFF_RECENT_FILES_MAX_ITEMS);
    assert_eq!(recent[0], "src/recent_09.rs");
    assert_eq!(brief["progress"]["recent_files"]["total"], 10);
}

#[test]
fn handoff_brief_closed_and_insufficient_evidence_states_are_explicit() {
    let store = store_with_limit(200);
    let session_id = start_session(&store, "close me");
    store.close_session(&session_id).unwrap();
    let workspace = clean_workspace();
    let jobs = empty_jobs();
    let closed = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        None,
        Some(&jobs),
        true,
    );
    assert_eq!(closed["progress"]["state"], "closed");
    let active = start_session(&store, "missing evidence");
    let unavailable = json!({"available": false, "status": "unknown"});
    let insufficient = brief_for(
        &store,
        &active,
        true,
        None,
        true,
        Some(&unavailable),
        None,
        false,
    );
    assert_eq!(insufficient["progress"]["state"], "insufficient_evidence");
    assert_eq!(insufficient["basis"]["complete"], false);
}

#[test]
fn handoff_brief_not_requested_and_unavailable_statuses_use_fixed_reasons() {
    let store = store_with_limit(200);
    let session_id = start_session(&store, "projection controls");
    let jobs = empty_jobs();
    let omitted = brief_for(
        &store,
        &session_id,
        false,
        None,
        false,
        None,
        Some(&jobs),
        true,
    );
    assert_eq!(
        omitted["workspace"]["reason_code"],
        "workspace_not_requested"
    );
    assert_eq!(
        omitted["validation"]["reason_code"],
        "validation_not_requested"
    );
    let unavailable_workspace = json!({"git_available": false, "clean": true, "git_error": "sensitive internal error must not escape"});
    let unavailable_validation = json!({"available": false, "status": "unknown"});
    let unavailable = brief_for(
        &store,
        &session_id,
        true,
        Some(&unavailable_workspace),
        true,
        Some(&unavailable_validation),
        Some(&jobs),
        true,
    );
    assert_eq!(
        unavailable["workspace"]["reason_code"],
        "workspace_unavailable"
    );
    assert_eq!(unavailable["validation"]["status"], "unavailable");
    assert!(!unavailable.to_string().contains("sensitive internal error"));
}

#[test]
fn handoff_brief_attempt_boundary_eviction_marks_basis_incomplete() {
    let store = store_with_limit(6);
    let session_id = start_session(&store, "evicted attempt");
    add_instruction(&store, &session_id, "boundary that will be evicted");
    for index in 0..4 {
        record_write(&store, &session_id, &format!("src/evicted_{index}.rs"));
    }
    let summary = store.summary(&session_id, Some(200)).unwrap();
    assert!(summary.events_truncated);
    let workspace = dirty_workspace(0);
    let jobs = empty_jobs();
    let brief = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        None,
        Some(&jobs),
        true,
    );
    assert_eq!(brief["basis"]["complete"], false);
    assert!(brief["basis"]["reason_codes"]
        .as_array()
        .unwrap()
        .contains(&json!("attempt_boundary_evicted")));
}

#[test]
fn handoff_brief_next_action_priority_is_stable() {
    let store = store_with_limit(200);
    let session_id = start_session(&store, "priority");
    add_instruction(&store, &session_id, "resolve blockers");
    store
        .post_message(PostSessionMessageInput {
            session_id: session_id.clone(),
            kind: SessionMessageKind::Risk,
            message: "review the risky migration".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::High,
        })
        .unwrap();
    let validation = failed_validation(&["tests::priority_failure".to_string()]);
    let workspace = dirty_workspace(1);
    let jobs = jobs_summary(1, 1, 0);
    let brief = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        Some(&validation),
        Some(&jobs),
        true,
    );
    assert_eq!(brief["progress"]["state"], "blocked");
    assert_eq!(
        brief["next_actions"],
        json!([
            "resolve workspace conflicts before continuing",
            "await recovering jobs before relying on their output",
            "resolve the latest validation failure",
            "review open risk guidance before continuing",
        ])
    );
    assert!(brief["next_actions"].as_array().unwrap().len() <= HANDOFF_NEXT_ACTIONS_MAX_ITEMS);
}

fn mutate_feedback_to_worst_case(feedback: &mut Value) {
    let long_path =
        |prefix: &str, index: usize| format!("{prefix}/{index:03}_{}.rs", "x".repeat(470));
    feedback["status"] = json!("available");
    feedback["attempt"]["changes"] = json!({"changed_paths": (0..100).map(|i| json!(long_path("src/quoted_\\segment", i))).collect::<Vec<_>>(), "total_changed_paths": 100, "truncated": false});
    feedback["attempt"]["exploration"] = json!({"observed_paths": (0..100).map(|i| json!(long_path("src/recent_\\segment", i))).collect::<Vec<_>>(), "total_observed_paths": 100, "truncated": false, "read_count": 100, "search_count": 0, "navigation_count": 0, "latest_tool": "read_files", "complete": true});
    feedback["attempt"]["validation"] = json!({"latest_status": "failed", "unresolved_failure_count": 20, "open_failures": (0..20).map(|i| json!({"kind": "test", "name": format!("tests::{}", format!("failure_{i}_").repeat(18))})).collect::<Vec<_>>(), "total_open_failures": 20, "failures_truncated": false, "delta_available": false});
}

#[test]
fn handoff_brief_hard_limit_uses_actual_escaped_json_bytes() {
    let store = store_with_limit(200);
    let root = format!("root {} {}", "\"\\\\\n\t".repeat(180), "界".repeat(600));
    let session_id = start_session(&store, &root);
    let latest = format!("latest {} {}", "\"\\\\\n\t".repeat(180), "新".repeat(600));
    add_instruction(&store, &session_id, &latest);
    let summary = store.summary(&session_id, Some(200)).unwrap();
    let validation = passed_validation();
    let (current_evidence, current_validation) = current_snapshot_values(&validation);
    let discussion = discussion(&store, &session_id);
    let jobs = empty_jobs();
    let actionable = raw_failed_event_ids(&summary);
    let mut feedback = continuation_feedback_value(ContinuationFeedbackInput {
        session_summary: &summary,
        validation: &validation,
        jobs: &jobs,
        discussion: &discussion,
        continuation: "continued",
        suggest_exploration_continuity: false,
        workspace_conflicts: false,
        hooks: projection_hooks(),
        current_validation: ContinuationValidationSnapshot::new(
            &current_evidence,
            &current_validation,
        ),
        tool_failures: ContinuationToolFailureSnapshot::new(&actionable),
    });
    mutate_feedback_to_worst_case(&mut feedback);
    let mut workspace = dirty_workspace(0);
    workspace["branch"] = json!(format!("feature/{}", "b".repeat(240)));
    let brief = build_handoff_brief(HandoffBriefInput {
        session_summary: &summary,
        continuation_feedback: &feedback,
        workspace_requested: true,
        workspace: Some(&workspace),
        validation_requested: true,
        validation: Some(&validation),
        jobs: Some(&jobs),
        guidance_available: true,
        existing_suggested_actions: None,
    });
    let bytes = handoff_brief_size(&brief);
    assert!(bytes < HANDOFF_BRIEF_HARD_MAX_BYTES, "{bytes}");
    assert_eq!(brief["progress"]["recent_files"]["truncated"], true);
    assert_eq!(brief["progress"]["changes"]["truncated"], true);
    assert_eq!(brief["validation"]["open_failures"]["truncated"], true);
}

#[test]
fn handoff_brief_redacts_instruction_credentials() {
    let store = store_with_limit(200);
    let workspace = clean_workspace();
    let jobs = empty_jobs();
    for credential in [
        "wc_pat_test_secret_value",
        "Bearer test_bearer_value",
        "client_secret=test_client_secret_value",
    ] {
        let session_id = start_session(&store, &format!("do not expose {credential}"));
        let brief = brief_for(
            &store,
            &session_id,
            true,
            Some(&workspace),
            true,
            None,
            Some(&jobs),
            true,
        );
        assert!(!brief.to_string().contains(credential));
        assert_eq!(brief["task"]["root_instruction"]["excerpt"], "[redacted]");
    }
}

#[test]
fn handoff_brief_preserves_instruction_paths_commands_and_code_deterministically() {
    let store = store_with_limit(200);
    let root_instruction = "Continue the handoff task. Inspect /root/git/private-drop/src/tool_runtime/handoff_brief.rs and run `cargo test -p webcodex --lib handoff_brief`.";
    let latest_instruction = "Continue from C:/repo/private-drop and keep `git diff --check`.";
    let session_id = start_session(&store, root_instruction);
    add_instruction(&store, &session_id, latest_instruction);
    let workspace = clean_workspace();
    let jobs = empty_jobs();
    let first = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        None,
        Some(&jobs),
        true,
    );
    let second = brief_for(
        &store,
        &session_id,
        true,
        Some(&workspace),
        true,
        None,
        Some(&jobs),
        true,
    );
    assert_eq!(first, second);
    assert_eq!(
        first["task"]["root_instruction"]["excerpt"],
        root_instruction
    );
    assert_eq!(
        first["task"]["latest_instruction"]["excerpt"],
        latest_instruction
    );
}
