use crate::{
    continuation_feedback_value, validation_delta_value, CodingSessionRequest,
    ContinuationFeedbackInput, ContinuationProjectionHooks, ContinuationToolFailureSnapshot,
    ContinuationValidationSnapshot, ListSessionMessagesFilter, PostSessionMessageInput,
    SessionDiscussionCounts, SessionDiscussionSummary, SessionGuards, SessionMessageKind,
    SessionMessagePriority, SessionPathHint, SessionStore, SessionToolContract, SessionTransport,
    ToolCallRecorderMetadata, TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use webcodex_core::workflow_session_contract::SessionMode;

const PROJECT: &str = "test-project";

fn store(max_events: usize) -> SessionStore {
    SessionStore::new(16, max_events)
}

fn create_session(store: &SessionStore, title: &str) -> String {
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

fn synthetic_contract(
    read_like: bool,
    write_like: bool,
    path_hint: SessionPathHint,
) -> SessionToolContract {
    SessionToolContract {
        risk_class: if write_like { "write" } else { "read" },
        read_like,
        write_like,
        shell_like: false,
        git_like: false,
        change_summary_like: false,
        project_write: write_like,
        path_hint,
        accepts_context_ack: false,
        advances_context_checkpoint: false,
    }
}

fn meaningful_hooks() -> ContinuationProjectionHooks {
    ContinuationProjectionHooks::new(|tool| {
        matches!(tool, "synthetic_write" | "synthetic_validation")
    })
}

fn record_write(store: &SessionStore, session_id: &str, paths: &[&str]) {
    let start = store.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        "synthetic_write",
        &json!({"project": PROJECT, "paths": paths}),
        synthetic_contract(false, true, SessionPathHint::PathList),
    );
    store.record_tool_call_finished(start, true, &json!({"applied": true}), None, None);
}

fn record_read(store: &SessionStore, session_id: &str, path: &str, output: Value) {
    let start = store.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        "read_files",
        &json!({"project": PROJECT, "items": [{"path": path}]}),
        synthetic_contract(true, false, SessionPathHint::None),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({"items": [{"index": 0, "success": true, "output": output}]}),
        None,
        None,
    );
}

fn record_search(store: &SessionStore, session_id: &str, paths: &[&str]) {
    let start = store.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        "search_project_texts",
        &json!({"project": PROJECT, "queries": [{"pattern": "secret-pattern"}]}),
        synthetic_contract(true, false, SessionPathHint::None),
    );
    store.record_tool_call_finished(start, true, &json!({
        "items": [{
            "index": 0,
            "success": true,
            "output": {
                "matches": paths.iter().enumerate().map(|(i, path)| json!({"path": path, "line": i + 1, "preview": "raw preview"})).collect::<Vec<_>>()
            }
        }]
    }), None, None);
}

fn not_run_validation() -> Value {
    json!({
        "available": false, "status": "not_run", "latest_status": "not_run",
        "current_evidence": {"status": "not_run", "latest_status": "not_run", "unresolved_failure_count": 0, "events_total": 0, "stale_failure_count": 0},
        "events": [],
    })
}

fn empty_current_validation() -> Value {
    json!({"latest": null, "unresolved_failures": {"events": []}})
}

fn empty_discussion() -> SessionDiscussionSummary {
    SessionDiscussionSummary {
        counts: SessionDiscussionCounts {
            total: 0,
            open: 0,
            resolved: 0,
            guidance: 0,
            progress: 0,
            risk: 0,
            todo: 0,
            question: 0,
            answer: 0,
            decision: 0,
            open_guidance: 0,
            open_questions: 0,
            open_risks: 0,
            open_todos: 0,
        },
        open_guidance: Vec::new(),
        open_questions: Vec::new(),
        open_risks: Vec::new(),
        open_todos: Vec::new(),
        high_priority_open_todos: Vec::new(),
        recent_answers: Vec::new(),
        recent_completions: Vec::new(),
        recent_progress: Vec::new(),
        recent_decisions: Vec::new(),
    }
}

fn raw_failed_event_ids(summary: &crate::SessionSummary) -> HashSet<String> {
    crate::canonical_tool_call_finished_events(&summary.events)
        .into_iter()
        .filter(|event| event.status.as_deref() == Some("failed"))
        .map(|event| event.event_id.clone())
        .collect()
}

fn feedback_for(store: &SessionStore, session_id: &str) -> Value {
    let summary = store.summary(session_id, Some(200)).unwrap();
    let validation = not_run_validation();
    let evidence = validation["current_evidence"].clone();
    let current = empty_current_validation();
    let jobs = json!({"active_count": 0, "running_count": 0, "recovering_count": 0, "terminal_pending_count": 0, "recent": [], "truncated": false});
    let discussion = store
        .discussion_summary(session_id, Some(20))
        .unwrap_or_else(|_| empty_discussion());
    let actionable = raw_failed_event_ids(&summary);
    continuation_feedback_value(ContinuationFeedbackInput {
        session_summary: &summary,
        validation: &validation,
        jobs: &jobs,
        discussion: &discussion,
        continuation: "continued",
        suggest_exploration_continuity: true,
        workspace_conflicts: false,
        hooks: meaningful_hooks(),
        current_validation: ContinuationValidationSnapshot::new(&evidence, &current),
        tool_failures: ContinuationToolFailureSnapshot::new(&actionable),
    })
}

fn feedback_with_jobs(store: &SessionStore, session_id: &str, jobs: &Value) -> Value {
    let summary = store.summary(session_id, Some(200)).unwrap();
    let validation = not_run_validation();
    let evidence = validation["current_evidence"].clone();
    let current = empty_current_validation();
    let discussion = empty_discussion();
    let actionable = raw_failed_event_ids(&summary);
    continuation_feedback_value(ContinuationFeedbackInput {
        session_summary: &summary,
        validation: &validation,
        jobs,
        discussion: &discussion,
        continuation: "continued",
        suggest_exploration_continuity: true,
        workspace_conflicts: false,
        hooks: meaningful_hooks(),
        current_validation: ContinuationValidationSnapshot::new(&evidence, &current),
        tool_failures: ContinuationToolFailureSnapshot::new(&actionable),
    })
}

fn validation_event(
    id: &str,
    command: Option<&str>,
    cwd: Option<&str>,
    success: Option<bool>,
    passed: u64,
    failed_names: &[&str],
    parsed: bool,
) -> Value {
    json!({
        "identity": id, "validation_kind": "test", "tool_name": "cargo_test", "purpose": "test",
        "cwd": cwd, "command_summary": command, "success": success, "completed_at": 1,
        "diagnostics": {
            "available": parsed, "parser": "structured_validation_parser", "diagnostics": [],
            "failed_test_details": failed_names.iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
            "test_summary": {"passed": passed, "failed": failed_names.len(), "ignored": 0},
            "diagnostics_truncated": false, "failed_test_details_truncated": false,
        },
    })
}

fn validation_summary(events: Vec<Value>) -> Value {
    json!({"events": events})
}

#[test]
fn attempt_only_counts_events_after_the_last_task_instruction() {
    let store = store(200);
    let session = create_session(&store, "attempt segmentation");
    add_instruction(&store, &session, "A");
    record_write(&store, &session, &["src/a.rs"]);
    add_instruction(&store, &session, "B");
    record_write(&store, &session, &["src/b.rs"]);
    let feedback = feedback_for(&store, &session);
    assert_eq!(
        feedback["attempt"]["boundary"]["source"],
        "task_instruction"
    );
    assert_eq!(feedback["attempt"]["activity"]["meaningful_tool_calls"], 1);
    assert_eq!(
        feedback["attempt"]["changes"]["changed_paths"],
        json!(["src/b.rs"])
    );
}

#[test]
fn attempt_does_not_recount_recorder_finish_when_business_finish_precedes_boundary() {
    let store = store(200);
    let session = create_session(&store, "logical boundary straddle");
    let args = json!({"project": PROJECT, "paths": ["src/prior.rs"]});
    let mut recorder = ToolCallRecorderMetadata::default();
    recorder.assign_logical_invocation();
    let mut business = recorder.clone();
    business.mark_business_execution();
    let recorder_start = store.record_tool_call_started_with_metadata(
        Some(&session),
        SessionTransport::Api,
        "synthetic_write",
        &args,
        Some(PROJECT.to_string()),
        recorder,
        synthetic_contract(false, true, SessionPathHint::PathList),
    );
    let business_start = store.record_tool_call_started_with_metadata(
        Some(&session),
        SessionTransport::Api,
        "synthetic_write",
        &args,
        Some(PROJECT.to_string()),
        business,
        synthetic_contract(false, true, SessionPathHint::PathList),
    );
    store.record_tool_call_finished(business_start, true, &json!({"applied": true}), None, None);
    add_instruction(&store, &session, "next attempt");
    store.record_tool_call_finished(recorder_start, true, &json!({"applied": true}), None, None);
    let feedback = feedback_for(&store, &session);
    assert_eq!(feedback["attempt"]["activity"]["meaningful_tool_calls"], 0);
    assert_eq!(feedback["attempt"]["changes"]["total_changed_paths"], 0);
}

#[test]
fn attempt_without_task_instruction_falls_back_to_session_start() {
    let store = store(200);
    let session = create_session(&store, "no instruction");
    record_write(&store, &session, &["src/a.rs"]);
    let feedback = feedback_for(&store, &session);
    assert_eq!(feedback["attempt"]["boundary"]["source"], "session_start");
    assert_eq!(feedback["attempt"]["instruction"]["status"], "not_observed");
}

#[test]
fn evicted_task_instruction_reports_unavailable_boundary_not_session_start() {
    let store = store(20);
    let session = create_session(&store, "evicted instruction");
    add_instruction(&store, &session, "do the work");
    for i in 0..30 {
        record_write(&store, &session, &[&format!("src/file_{i}.rs")]);
    }
    record_read(&store, &session, "src/retained-tail.rs", json!({}));
    assert!(store.summary(&session, Some(200)).unwrap().events_truncated);
    let feedback = feedback_for(&store, &session);
    assert_eq!(feedback["attempt"]["boundary"]["source"], "unavailable");
    assert_eq!(
        feedback["attempt"]["boundary"]["reason_code"],
        "attempt_boundary_evicted"
    );
    assert_eq!(feedback["attempt"]["event_range"]["complete"], false);
    assert_eq!(
        feedback["attempt"]["exploration"]["observed_paths"],
        json!(["src/retained-tail.rs"])
    );
}

#[test]
fn retained_task_instruction_still_segments_and_reports_complete() {
    let store = store(200);
    let session = create_session(&store, "retained instruction");
    add_instruction(&store, &session, "do work");
    record_write(&store, &session, &["src/lib.rs"]);
    let feedback = feedback_for(&store, &session);
    assert_eq!(
        feedback["attempt"]["boundary"]["source"],
        "task_instruction"
    );
    assert_eq!(feedback["attempt"]["event_range"]["complete"], true);
    assert_eq!(feedback["attempt"]["activity"]["meaningful_tool_calls"], 1);
}

#[test]
fn exploration_workset_is_attempt_scoped_deduped_and_newest_first() {
    let store = store(200);
    let session = create_session(&store, "exploration ordering");
    add_instruction(&store, &session, "inspect");
    record_read(
        &store,
        &session,
        "src/read.rs",
        json!({"content":"RAW_BODY"}),
    );
    record_search(
        &store,
        &session,
        &["src/search.rs", "src/read.rs", "/absolute.rs"],
    );
    record_read(&store, &session, "src/latest.rs", json!({}));
    let feedback = feedback_for(&store, &session);
    assert_eq!(
        feedback["attempt"]["exploration"]["observed_paths"],
        json!(["src/latest.rs", "src/search.rs", "src/read.rs"])
    );
    let serialized = feedback.to_string();
    assert!(!serialized.contains("RAW_BODY"));
    assert!(!serialized.contains("secret-pattern"));
    assert!(!serialized.contains("raw preview"));
}

#[test]
fn exploration_workset_reports_real_total_at_under_equal_and_over_limit() {
    for (count, returned, truncated) in [
        (99usize, 99usize, false),
        (100, 100, false),
        (101, 100, true),
    ] {
        let store = store(200);
        let session = create_session(&store, "exploration limit");
        add_instruction(&store, &session, "search");
        let owned = (0..count)
            .map(|i| format!("src/observed-{i:03}.rs"))
            .collect::<Vec<_>>();
        let refs = owned.iter().map(String::as_str).collect::<Vec<_>>();
        record_search(&store, &session, &refs);
        let feedback = feedback_for(&store, &session);
        assert_eq!(
            feedback["attempt"]["exploration"]["total_observed_paths"],
            count
        );
        assert_eq!(
            feedback["attempt"]["exploration"]["observed_paths"]
                .as_array()
                .unwrap()
                .len(),
            returned
        );
        assert_eq!(feedback["attempt"]["exploration"]["truncated"], truncated);
    }
}

#[test]
fn exploration_is_segmented_by_the_latest_attempt_instruction() {
    let store = store(200);
    let session = create_session(&store, "exploration attempts");
    add_instruction(&store, &session, "attempt A");
    record_read(&store, &session, "src/attempt-a.rs", json!({}));
    add_instruction(&store, &session, "attempt B");
    record_read(&store, &session, "src/attempt-b.rs", json!({}));
    assert_eq!(
        feedback_for(&store, &session)["attempt"]["exploration"]["observed_paths"],
        json!(["src/attempt-b.rs"])
    );
}

#[test]
fn resume_and_restart_produce_the_same_attempt_summary_without_appending_events() {
    let store = store(200);
    let session = create_session(&store, "resume determinism");
    add_instruction(&store, &session, "do work");
    record_read(&store, &session, "src/deterministic.rs", json!({}));
    let before = store.summary(&session, Some(200)).unwrap();
    let first = feedback_for(&store, &session);
    let second = feedback_for(&store, &session);
    let after = store.summary(&session, Some(200)).unwrap();
    assert_eq!(first, second);
    assert_eq!(before.events_total, after.events_total);
    assert_eq!(before.updated_at, after.updated_at);
}

#[test]
fn validation_delta_fail_to_pass_reports_resolved_failure() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test --lib"),
            Some("."),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
        validation_event(
            "b",
            Some("cargo test --lib"),
            Some("."),
            Some(true),
            2,
            &[],
            true,
        ),
    ]);
    let d = validation_delta_value(&v);
    assert_eq!(d["comparison"]["status"], "available");
    assert_eq!(d["counts"]["failed_delta"], -1);
    assert_eq!(d["counts"]["passed_delta"], 2);
    assert_eq!(d["failures"]["resolved"][0]["name"], "tests::a");
}

#[test]
fn validation_delta_pass_to_fail_reports_newly_failed() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test --lib"),
            Some("."),
            Some(true),
            2,
            &[],
            true,
        ),
        validation_event(
            "b",
            Some("cargo test --lib"),
            Some("."),
            Some(false),
            0,
            &["tests::b"],
            true,
        ),
    ]);
    let d = validation_delta_value(&v);
    assert_eq!(d["counts"]["passed_delta"], -2);
    assert_eq!(d["counts"]["failed_delta"], 1);
    assert_eq!(d["failures"]["newly_failed"][0]["name"], "tests::b");
}

#[test]
fn validation_delta_passed_count_decrease_is_negative_not_zero() {
    let v = validation_summary(vec![
        validation_event("a", Some("cargo test"), Some("."), Some(true), 2, &[], true),
        validation_event(
            "b",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &["tests::b"],
            true,
        ),
    ]);
    let d = validation_delta_value(&v);
    assert_eq!(d["counts"]["passed_delta"], -2);
    assert_eq!(d["counts"]["total_delta"], -1);
}

#[test]
fn scope_identity_is_opaque_hash_and_does_not_leak_command() {
    let marker = "unique_marker_filter_xyz_123";
    let command = format!("cargo test {marker}");
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some(&command),
            Some("."),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
        validation_event(
            "b",
            Some(&command),
            Some("."),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
    ]);
    let d = validation_delta_value(&v);
    let s = d.to_string();
    assert!(!s.contains(marker));
    assert!(d["comparison"]["scope_identity"]
        .as_str()
        .unwrap()
        .starts_with("validation_scope:v1:"));
}

#[test]
fn scope_identity_is_stable_for_same_scope_and_differs_for_different_command() {
    fn scope(command: &str, cwd: &str) -> String {
        let v = validation_summary(vec![
            validation_event(
                "a",
                Some(command),
                Some(cwd),
                Some(false),
                0,
                &["tests::a"],
                true,
            ),
            validation_event(
                "b",
                Some(command),
                Some(cwd),
                Some(false),
                0,
                &["tests::a"],
                true,
            ),
        ]);
        validation_delta_value(&v)["comparison"]["scope_identity"]
            .as_str()
            .unwrap()
            .to_string()
    }
    assert_eq!(
        scope("cargo test --lib", "."),
        scope("cargo   test   --lib", ".")
    );
    assert_ne!(
        scope("cargo test tests::a", "."),
        scope("cargo test tests::b", ".")
    );
    assert_ne!(
        scope("cargo test", "crate-a"),
        scope("cargo test", "crate-b")
    );
}

#[test]
fn validation_delta_fail_a_to_fail_a_reports_still_failing() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
        validation_event(
            "b",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
    ]);
    let d = validation_delta_value(&v);
    assert_eq!(d["failures"]["still_failing"][0]["name"], "tests::a");
    assert_eq!(d["failures"]["total_still_failing"], 1);
}

#[test]
fn validation_delta_fail_a_to_fail_b_reports_newly_failed_and_resolved() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
        validation_event(
            "b",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &["tests::b"],
            true,
        ),
    ]);
    let d = validation_delta_value(&v);
    assert_eq!(d["failures"]["newly_failed"][0]["name"], "tests::b");
    assert_eq!(d["failures"]["resolved"][0]["name"], "tests::a");
}

#[test]
fn validation_delta_multi_failure_partial_resolution() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &["tests::a", "tests::b"],
            true,
        ),
        validation_event(
            "b",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &["tests::b"],
            true,
        ),
    ]);
    let d = validation_delta_value(&v);
    assert_eq!(d["failures"]["resolved"][0]["name"], "tests::a");
    assert_eq!(d["failures"]["still_failing"][0]["name"], "tests::b");
}

#[test]
fn validation_delta_unavailable_when_command_filter_changes() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test tests::a"),
            Some("."),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
        validation_event(
            "b",
            Some("cargo test tests::b"),
            Some("."),
            Some(false),
            0,
            &["tests::b"],
            true,
        ),
    ]);
    let d = validation_delta_value(&v);
    assert_eq!(d["comparison"]["status"], "unavailable");
    assert_eq!(d["comparison"]["reason_code"], "validation_scope_changed");
}

#[test]
fn validation_delta_unavailable_when_cwd_changes() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test"),
            Some("crate-a"),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
        validation_event(
            "b",
            Some("cargo test"),
            Some("crate-b"),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
    ]);
    assert_eq!(
        validation_delta_value(&v)["comparison"]["reason_code"],
        "validation_scope_changed"
    );
}

#[test]
fn validation_delta_unavailable_with_no_previous_validation() {
    let v = validation_summary(vec![validation_event(
        "a",
        Some("cargo test"),
        Some("."),
        Some(true),
        1,
        &[],
        true,
    )]);
    assert_eq!(
        validation_delta_value(&v)["comparison"]["reason_code"],
        "no_previous_validation"
    );
}

#[test]
fn validation_delta_unavailable_when_scope_identity_is_missing() {
    let v = validation_summary(vec![
        validation_event("a", None, None, Some(false), 0, &["tests::a"], true),
        validation_event("b", None, None, Some(false), 0, &["tests::a"], true),
    ]);
    assert_eq!(
        validation_delta_value(&v)["comparison"]["reason_code"],
        "insufficient_scope_identity"
    );
}

#[test]
fn validation_delta_previous_evidence_incomplete_when_prior_run_unparsed() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &[],
            false,
        ),
        validation_event("b", Some("cargo test"), Some("."), Some(true), 1, &[], true),
    ]);
    assert_eq!(
        validation_delta_value(&v)["comparison"]["reason_code"],
        "previous_evidence_incomplete"
    );
}

#[test]
fn validation_delta_zero_test_success_does_not_resolve_prior_test_failures() {
    let v = validation_summary(vec![
        validation_event(
            "a",
            Some("cargo test"),
            Some("."),
            Some(false),
            0,
            &["tests::a"],
            true,
        ),
        validation_event("b", Some("cargo test"), Some("."), Some(true), 0, &[], true),
    ]);
    let d = validation_delta_value(&v);
    assert_eq!(d["comparison"]["status"], "available");
    assert_eq!(d["failures"]["identity_status"], "unavailable");
    assert_eq!(
        d["failures"]["identity_reason_code"],
        "test_identity_unavailable"
    );
    assert!(d["failures"]["resolved"].as_array().unwrap().is_empty());
}

#[test]
fn changed_paths_are_deduped_bounded_and_report_total_and_truncated() {
    let store = store(200);
    let session = create_session(&store, "many paths");
    add_instruction(&store, &session, "edit many");
    for i in 0..75 {
        let a = format!("src/file_{:03}.rs", i * 2);
        let b = format!("src/file_{:03}.rs", i * 2 + 1);
        record_write(&store, &session, &[&a, &b]);
    }
    let f = feedback_for(&store, &session);
    assert_eq!(f["attempt"]["changes"]["total_changed_paths"], 150);
    assert_eq!(
        f["attempt"]["changes"]["changed_paths"]
            .as_array()
            .unwrap()
            .len(),
        100
    );
    assert_eq!(f["attempt"]["changes"]["truncated"], true);
}

#[test]
fn continuation_feedback_output_is_bounded_in_size() {
    let store = store(200);
    let session = create_session(&store, "bounded size");
    add_instruction(&store, &session, "bounded");
    for i in 0..90 {
        record_write(&store, &session, &[&format!("src/{i:03}.rs")]);
    }
    let f = feedback_for(&store, &session);
    assert!(serde_json::to_vec(&f).unwrap().len() < 32_000);
    assert!(
        f["attempt"]["suggested_next_actions"]
            .as_array()
            .unwrap()
            .len()
            <= 8
    );
}

#[test]
fn guidance_counts_open_messages_without_changing_status() {
    let store = store(200);
    let session = create_session(&store, "guidance board");
    add_instruction(&store, &session, "with guidance");
    for (kind, message) in [
        (SessionMessageKind::Guidance, "guide"),
        (SessionMessageKind::Risk, "risk"),
        (SessionMessageKind::Todo, "todo"),
    ] {
        store
            .post_message(PostSessionMessageInput {
                session_id: session.clone(),
                kind,
                message: message.to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::Normal,
            })
            .unwrap();
    }
    let f = feedback_for(&store, &session);
    assert_eq!(f["attempt"]["guidance"]["open_count"], 1);
    assert_eq!(f["attempt"]["guidance"]["open_risk_count"], 1);
    assert_eq!(f["attempt"]["guidance"]["open_todo_count"], 1);
    let after = store.discussion_summary(&session, Some(20)).unwrap();
    assert_eq!(after.counts.guidance, 1);
    assert_eq!(after.counts.risk, 1);
    assert_eq!(after.counts.todo, 1);
}

#[test]
fn resolved_guidance_is_not_counted_as_open() {
    let store = store(200);
    let session = create_session(&store, "resolved guidance");
    store
        .post_message(PostSessionMessageInput {
            session_id: session.clone(),
            kind: SessionMessageKind::Guidance,
            message: "resolved guidance".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
    let listed = store
        .list_messages(
            &session,
            ListSessionMessagesFilter {
                kind: Some(SessionMessageKind::Guidance),
                status: None,
                message_id: None,
                reply_to: None,
                limit: None,
            },
        )
        .unwrap();
    store
        .resolve_message(&session, &listed[0].message_id, Some("done".to_string()))
        .unwrap();
    assert_eq!(
        feedback_for(&store, &session)["attempt"]["guidance"]["open_count"],
        0
    );
}

#[test]
fn jobs_block_reports_recovering_not_healthy_running() {
    let store = store(200);
    let session = create_session(&store, "jobs recovering");
    add_instruction(&store, &session, "observe jobs");
    let f = feedback_with_jobs(
        &store,
        &session,
        &json!({"active_count":1,"running_count":1,"recovering_count":1,"terminal_pending_count":0,"truncated":false,"recent":[{"status":"recovering"}]}),
    );
    assert_eq!(f["attempt"]["jobs"]["recovery_state"], "recovering");
}

#[test]
fn jobs_block_terminal_pending_does_not_become_healthy_running() {
    let store = store(200);
    let session = create_session(&store, "terminal pending");
    add_instruction(&store, &session, "observe jobs");
    let f = feedback_with_jobs(
        &store,
        &session,
        &json!({"active_count":1,"running_count":0,"recovering_count":0,"terminal_pending_count":1,"truncated":false,"recent":[{"status":"stop_requested"}]}),
    );
    assert_eq!(f["attempt"]["jobs"]["recovery_state"], "terminal_pending");
}

#[test]
fn jobs_block_none_when_no_active_jobs() {
    let store = store(200);
    let session = create_session(&store, "no jobs");
    let f = feedback_with_jobs(
        &store,
        &session,
        &json!({"active_count":0,"running_count":0,"recovering_count":0,"terminal_pending_count":0,"truncated":false,"recent":[]}),
    );
    assert_eq!(f["attempt"]["jobs"]["recovery_state"], "none");
    assert_eq!(f["attempt"]["jobs"]["latest_job_status"], "not_observed");
}

#[test]
fn jobs_block_recovering_hidden_beyond_truncated_recent_still_reported() {
    let store = store(200);
    let session = create_session(&store, "truncated jobs");
    add_instruction(&store, &session, "observe jobs");
    let f = feedback_with_jobs(
        &store,
        &session,
        &json!({"active_count":5,"running_count":5,"recovering_count":1,"terminal_pending_count":0,"truncated":true,"recent":[{"status":"running"}]}),
    );
    assert_eq!(f["attempt"]["jobs"]["recovering_count"], 1);
    assert_eq!(f["attempt"]["jobs"]["recent_truncated"], true);
    assert_eq!(f["attempt"]["jobs"]["recovery_state"], "recovering");
}

#[test]
fn continuation_feedback_does_not_leak_raw_output_or_commands() {
    let store = store(200);
    let session = create_session(&store, "no leak");
    add_instruction(&store, &session, "inspect");
    record_read(
        &store,
        &session,
        "src/lib.rs",
        json!({"content":"SUPER_SECRET_OUTPUT"}),
    );
    record_search(&store, &session, &["src/search.rs"]);
    let s = feedback_for(&store, &session).to_string();
    assert!(!s.contains("SUPER_SECRET_OUTPUT"));
    assert!(!s.contains("secret-pattern"));
    assert!(!s.contains("raw preview"));
}

#[test]
fn fresh_session_reports_not_applicable() {
    let store = store(200);
    let session = create_session(&store, "fresh");
    let f = feedback_for(&store, &session);
    assert_eq!(f["status"], "not_applicable");
    assert_eq!(f["reason_code"], "empty_session");
}
