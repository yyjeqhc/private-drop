//! Tests for the deterministic continuation feedback projection.
//!
//! These tests exercise the pure read-only projection over the session
//! ledger, validation evidence, job metadata, and message board: attempt
//! segmentation by `task_instruction`, resume/restored determinism,
//! validation delta comparability and stable failure identity, bounded
//! output, message-board counts that do not change state, job recovery
//! projection, and the pure-read contract.

use super::support::*;
use crate::tool_runtime::continuation_feedback::{
    continuation_feedback_value, continuation_projection_hooks, continuation_validation_snapshot,
    root_tool_is_meaningful, ContinuationFeedbackInput, ContinuationToolFailureSnapshot,
};
use crate::tool_runtime::handoff::reconcile_closeout_evidence;
use crate::tool_runtime::sessions::{
    self, tool_failure_summary_from_events, SessionDiscussionCounts, SessionGuards,
    SessionTransport,
};
use crate::tool_runtime::tool_definition::{
    runtime_tool_captures_validation_output, runtime_tool_is_git_like, runtime_tool_is_shell_like,
    runtime_tool_is_write_like,
};
use crate::tool_runtime::validation_events::{
    current_validation_evidence_for_session, validation_summary_from_events,
};
use crate::tool_runtime::{
    known_tool_names, registered_tool_specs, SessionMode, StartupDetail, ToolCall, ToolResult,
    ToolRuntime, ToolSpec,
};
use serde_json::{json, Value};

fn continuation_feedback_subschema(specs: &[ToolSpec], tool: &str) -> Value {
    let spec = spec_named(specs, tool);
    spec.output_schema["properties"]["output"]["properties"]["continuation_feedback"].clone()
}

// =========================================================================
// Schema / registration
// =========================================================================

#[test]
fn continuation_feedback_output_conforms_to_strict_schema_shape() {
    // A real continued-session output carries exactly the schema'd fields and
    // no unknown core fields. Because serde derives the output from the Rust
    // structs and the schema now sets additionalProperties:false, a drift
    // (unknown field) would be rejected by the schema; assert the keys line up.
    let runtime = test_runtime();
    let session = create_session(&runtime, "schema shape");
    add_instruction(&runtime, &session, "do something", SessionMode::Normal);
    record_validation_event(
        &runtime,
        &session,
        "cargo_test",
        false,
        test_output(0, 1, 0, &["tests::a"]),
    );
    let summary = runtime.sessions.summary(&session, Some(200)).unwrap();
    let feedback = feedback_for(&runtime, &summary, "continued");
    let schema = continuation_feedback_subschema(&registered_tool_specs(), "finish_coding_task");

    // Every field present in the serialized output must be declared in the
    // strict schema's properties for its parent object (no drift fields).
    fn assert_no_unknown_fields(output: &Value, schema: &Value, path: &str) {
        if let (Some(out_obj), Some(schema_obj)) = (output.as_object(), schema.as_object()) {
            if schema_obj.get("type").and_then(Value::as_str) == Some("object") {
                let declared = schema_obj["properties"].as_object().unwrap();
                for key in out_obj.keys() {
                    assert!(
                        declared.contains_key(key),
                        "{path}: output field '{key}' is not declared in the strict schema"
                    );
                }
            }
        }
        if let (Some(out_obj), Some(schema_obj)) = (output.as_object(), schema.as_object()) {
            for (key, child) in out_obj {
                if let Some(child_schema) = schema_obj["properties"].get(key) {
                    assert_no_unknown_fields(child, child_schema, &format!("{path}.{key}"));
                }
            }
        }
    }
    assert_no_unknown_fields(&feedback, &schema, "continuation_feedback");

    // Negative passed_delta: 2 passed -> 0 passed yields -2, proving the signed
    // integer round-trips through the serializer.
    let session_neg = create_session(&runtime, "neg delta");
    add_instruction(&runtime, &session_neg, "neg", SessionMode::Normal);
    record_validation_event(
        &runtime,
        &session_neg,
        "cargo_test",
        true,
        test_output(2, 0, 0, &[]),
    );
    add_instruction(&runtime, &session_neg, "neg2", SessionMode::Normal);
    record_validation_event(
        &runtime,
        &session_neg,
        "cargo_test",
        false,
        test_output(0, 1, 0, &["tests::a"]),
    );
    let summary = runtime.sessions.summary(&session_neg, Some(200)).unwrap();
    let feedback = feedback_for(&runtime, &summary, "continued");
    assert_eq!(
        feedback["validation_delta"]["counts"]["passed_delta"].as_i64(),
        Some(-2),
        "passed_delta must serialize as a negative integer"
    );
    assert_eq!(
        feedback["validation_delta"]["counts"]["failed_delta"].as_i64(),
        Some(1)
    );
    assert_no_unknown_fields(&feedback, &schema, "continuation_feedback");
}

#[test]
fn exploration_continuity_suggestion_is_lower_priority_than_blocking_evidence() {
    let has_exploration_action = |feedback: &Value| {
        feedback["attempt"]["suggested_next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| {
                action
                    == "continue from the recent exploration workset before repeating broad discovery"
            })
    };
    let seed_read = |runtime: &ToolRuntime, title: &str| {
        let session = create_session(runtime, title);
        add_instruction(runtime, &session, "inspect", SessionMode::Normal);
        record_exploration_event(
            runtime,
            &session,
            "read_file",
            json!({"project": "test-project", "path": "src/seed.rs"}),
            true,
            json!({}),
        );
        session
    };

    let runtime = test_runtime();
    let validation_session = seed_read(&runtime, "validation blocks exploration");
    record_validation_event(
        &runtime,
        &validation_session,
        "cargo_test",
        false,
        test_output(0, 1, 0, &["tests::failing"]),
    );
    let validation_summary = runtime
        .sessions
        .summary(&validation_session, Some(200))
        .unwrap();
    let validation_feedback = feedback_for(&runtime, &validation_summary, "continued");
    assert_eq!(
        validation_feedback["attempt"]["suggested_next_actions"][0],
        "fix failing test tests::failing"
    );
    assert!(!has_exploration_action(&validation_feedback));

    let job_session = seed_read(&runtime, "job blocks exploration");
    let job_summary = runtime.sessions.summary(&job_session, Some(200)).unwrap();
    let job_feedback = feedback_for_with_jobs(
        &runtime,
        &job_summary,
        &json!({
            "active_count": 1,
            "running_count": 1,
            "recovering_count": 0,
            "terminal_pending_count": 0,
            "recent": [{"status": "running"}]
        }),
        "continued",
    );
    assert!(!has_exploration_action(&job_feedback));

    let risk_session = seed_read(&runtime, "risk blocks exploration");
    post_session_message(&runtime, &risk_session, "risk", "review this risk");
    let risk_summary = runtime.sessions.summary(&risk_session, Some(200)).unwrap();
    let risk_discussion = runtime
        .sessions
        .discussion_summary(&risk_session, Some(20))
        .unwrap();
    let risk_feedback =
        feedback_for_with_discussion(&runtime, &risk_summary, &risk_discussion, "continued");
    assert!(!has_exploration_action(&risk_feedback));

    let conflict_session = seed_read(&runtime, "conflict blocks exploration");
    let conflict_summary = runtime
        .sessions
        .summary(&conflict_session, Some(200))
        .unwrap();
    let conflict_validation = validation_summary_from_events(&conflict_summary.events, 20);
    let conflict_current_validation =
        current_validation_evidence_for_session(&conflict_summary, 20);
    let conflict_tool_failures = tool_failure_summary_from_events(&conflict_summary.events, 20);
    let conflict_reconciliation = reconcile_closeout_evidence(
        &conflict_tool_failures,
        &conflict_summary,
        &conflict_validation,
    );
    let conflict_discussion = empty_discussion();
    let conflict_feedback = continuation_feedback_value(ContinuationFeedbackInput {
        session_summary: &conflict_summary,
        validation: &conflict_validation,
        jobs: &json!({"active_count": 0, "running_count": 0, "recent": []}),
        discussion: &conflict_discussion,
        continuation: "continued",
        suggest_exploration_continuity: true,
        workspace_conflicts: true,
        hooks: continuation_projection_hooks(),
        current_validation: continuation_validation_snapshot(&conflict_current_validation),
        tool_failures: ContinuationToolFailureSnapshot::new(
            &conflict_reconciliation.actionable_unexpected_event_ids,
        ),
    });
    assert!(!has_exploration_action(&conflict_feedback));
}

// =========================================================================
// Resume / restart determinism
// =========================================================================

async fn diagnostic_coding_workflow(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    instruction: &str,
    resume: Option<&str>,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.to_string();
        let instruction = instruction.to_string();
        let resume = resume.map(str::to_string);
        let auth = auth_context(None, true);
        async move {
            runtime
                .start_coding_workflow_for_test(
                    project,
                    None,
                    None,
                    Some(instruction),
                    SessionMode::Normal,
                    false,
                    false,
                    // These integration assertions exercise the complete
                    // continuation_feedback block retained by full diagnostics.
                    StartupDetail::Full,
                    resume,
                    None,
                    Some(&auth),
                    None,
                    None,
                    crate::tool_runtime::sessions::SessionTransport::Mcp,
                )
                .await
        }
    });
    while !task.is_finished() {
        if let Some(req) = runtime
            .runner_registry
            .poll(crate::runner_protocol::RunnerPollRequest {
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
            })
            .await
            .unwrap()
        {
            let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&req);
            complete_patch_agent_request(
                runtime,
                client_id,
                &req.request_id,
                exit_code,
                &stdout,
                &stderr,
            )
            .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    task.await.unwrap()
}

#[tokio::test]
async fn coding_workflow_continuation_describes_previous_attempt_not_empty_new_one() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "continuation-agent", "demo", dir.path()).await;

    // First coding workflow creates the session with instruction A.
    let first = diagnostic_coding_workflow(
        &runtime,
        "continuation-agent",
        &project,
        "instruction A",
        None,
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let session_id = first.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Record real work under attempt A: a write tool call and a validation run.
    record_write(&runtime, &session_id, &["src/lib.rs"]);
    record_validation_event(
        &runtime,
        &session_id,
        "cargo_test",
        false,
        test_output(0, 1, 0, &["tests::a"]),
    );

    // Capture event count *after* A's work, before the continuation call.
    let events_before_continuation = runtime
        .sessions
        .summary(&session_id, Some(200))
        .unwrap()
        .events
        .len();

    // Second coding workflow continues with instruction B on the same session.
    let second = diagnostic_coding_workflow(
        &runtime,
        "continuation-agent",
        &project,
        "instruction B",
        Some(&session_id),
    )
    .await;
    assert!(second.success, "{:?}", second.error);
    assert_eq!(
        second.output["session"]["continuation"],
        "resumed_explicitly"
    );

    let feedback = &second.output["continuation_feedback"];
    // The feedback must describe A's attempt (real work), not the empty new B attempt.
    assert_eq!(feedback["status"], "available");
    assert_eq!(
        feedback["attempt"]["boundary"]["source"],
        "task_instruction"
    );
    assert_eq!(feedback["attempt"]["instruction"]["status"], "available");
    assert_eq!(
        feedback["attempt"]["instruction"]["excerpt"],
        "instruction A"
    );
    // Instruction B must be appended exactly once.
    let summary = runtime.sessions.summary(&session_id, Some(200)).unwrap();
    let instructions: Vec<&str> = summary
        .events
        .iter()
        .filter(|event| event.kind == "task_instruction")
        .map(|event| event.instruction.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        instructions
            .iter()
            .filter(|i| **i == "instruction B")
            .count(),
        1,
        "instruction B appended exactly once"
    );
    assert_eq!(
        instructions
            .iter()
            .filter(|i| **i == "instruction A")
            .count(),
        1,
        "instruction A still present exactly once"
    );

    // Reading the continuation feedback must not append any further events
    // beyond the one legitimate instruction append for B itself.
    let events_after = summary.events.len();
    // B's continuation appended exactly one task_instruction event.
    assert_eq!(
        events_after,
        events_before_continuation + 1,
        "continuation read appended only the B instruction"
    );
}

#[tokio::test]
async fn coding_workflow_fresh_session_continuation_is_not_applicable() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "fresh-agent", "demo", dir.path()).await;

    let first =
        diagnostic_coding_workflow(&runtime, "fresh-agent", &project, "fresh start", None).await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.output["session"]["continuation"], "created");
    let feedback = &first.output["continuation_feedback"];
    assert_eq!(feedback["status"], "not_applicable");
    assert_eq!(feedback["reason_code"], "fresh_session");
}

// =========================================================================
// Real entry integration: session_handoff_summary
// =========================================================================

fn handoff_call(session_id: &str) -> ToolCall {
    ToolCall::SessionHandoffSummary {
        session_id: session_id.to_string(),
        project: None,
        include_workspace: None,
        include_checkpoints: None,
        include_validation: None,
        summary_only: false,
        limit: None,
    }
}

#[tokio::test]
async fn handoff_default_limit_does_not_shrink_continuation_attempt_boundary() {
    let runtime = test_runtime();
    let session = create_session(&runtime, "handoff display limit");
    add_instruction(&runtime, &session, "instruction A", SessionMode::Normal);
    // Produce 30+ events so the default display limit (20) cannot see instruction A.
    for i in 0..35 {
        record_write(&runtime, &session, &[&format!("src/file_{i}.rs")]);
    }

    // Call the real session_handoff_summary with the default display limit.
    let result = runtime.dispatch(handoff_call(&session)).await;
    assert!(result.success, "{:?}", result.error);
    // The display `events` count is bounded by the caller limit.
    let display_events = result.output["counts"]["events"].as_u64().unwrap_or(0);
    assert!(
        display_events <= 20,
        "display events bounded by default limit, got {display_events}"
    );
    // The continuation feedback still locates instruction A as the attempt
    // boundary (it uses an independent 200-event evidence snapshot).
    let feedback = &result.output["continuation_feedback"];
    assert_eq!(feedback["status"], "available");
    assert_eq!(
        feedback["attempt"]["boundary"]["source"],
        "task_instruction"
    );
    assert_eq!(feedback["attempt"]["activity"]["meaningful_tool_calls"], 35);
}

#[tokio::test]
async fn handoff_summary_only_and_include_validation_false_shape() {
    let runtime = test_runtime();
    let session = create_session(&runtime, "handoff summary only");
    add_instruction(&runtime, &session, "instruction A", SessionMode::Normal);
    record_write(&runtime, &session, &["src/lib.rs"]);
    record_validation_event(
        &runtime,
        &session,
        "cargo_test",
        false,
        test_output(0, 1, 0, &["tests::a"]),
    );

    // summary_only=true, include_validation=false.
    let result = runtime
        .dispatch(ToolCall::SessionHandoffSummary {
            session_id: session.clone(),
            project: None,
            include_workspace: None,
            include_checkpoints: None,
            include_validation: Some(false),
            summary_only: true,
            limit: None,
        })
        .await;
    assert!(result.success, "{:?}", result.error);
    // summary_only must not expose raw command/output fields.
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(
        !serialized.contains("command_summary") && !serialized.contains("stdout_tail"),
        "summary_only leaked raw validation/command fields"
    );
    let feedback = &result.output["continuation_feedback"];
    assert_eq!(feedback["status"], "available");
    // Validation in the feedback is explicitly unavailable, not `not_run`.
    assert_eq!(
        feedback["attempt"]["validation"]["latest_status"],
        "unavailable"
    );
    assert_eq!(
        feedback["attempt"]["validation"]["delta_reason_code"],
        "validation_not_requested"
    );
}

#[tokio::test]
async fn handoff_guidance_read_does_not_resolve_messages() {
    let runtime = test_runtime();
    let session = create_session(&runtime, "handoff guidance read");
    add_instruction(&runtime, &session, "with guidance", SessionMode::Normal);
    post_session_message(&runtime, &session, "guidance", "review the boundary");
    post_session_message(&runtime, &session, "todo", "finish tests");

    let result = runtime.dispatch(handoff_call(&session)).await;
    assert!(result.success, "{:?}", result.error);
    let feedback = &result.output["continuation_feedback"];
    assert_eq!(feedback["attempt"]["guidance"]["open_count"], 1);
    assert_eq!(feedback["attempt"]["guidance"]["open_todo_count"], 1);

    // Reading guidance via handoff must not resolve or change counts.
    let after = runtime
        .sessions
        .discussion_summary(&session, Some(20))
        .unwrap();
    assert_eq!(after.counts.guidance, 1);
    assert_eq!(after.counts.todo, 1);
}

#[tokio::test]
async fn validation_summary_surfaces_validation_delta_without_shell_or_new_events() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "vsummary-agent", "demo", dir.path()).await;
    let auth = auth_context(None, true);

    // Create a real session scoped to the registered project, then record
    // prior + current validation evidence directly in the ledger (no shell,
    // no agent request).
    let session = runtime
        .sessions
        .start_session_with_options(sessions::SessionCreateOptions::new(
            Some(project.clone()),
            Some("validation delta".to_string()),
            SessionMode::Normal,
            SessionGuards::default(),
        ))
        .unwrap()
        .session_id;
    // Use a Normal-mode instruction so the attempt boundary is task_instruction.
    add_instruction_for(
        &runtime,
        &session,
        "fix tests",
        SessionMode::Normal,
        &project,
    );
    // Prior comparable run: 2 passed, 0 failed.
    record_validation_event_for(
        &runtime,
        &session,
        "cargo_test",
        true,
        test_output(2, 0, 0, &[]),
        &project,
    );
    // Current run: 0 passed, 1 failed. passed_delta must be negative (-2).
    record_validation_event_for(
        &runtime,
        &session,
        "cargo_test",
        false,
        test_output(0, 1, 0, &["tests::a"]),
        &project,
    );

    let events_before = runtime
        .sessions
        .summary(&session, Some(200))
        .unwrap()
        .events
        .len();

    let result = runtime
        .dispatch_with_auth(
            ToolCall::ValidationSummary {
                project: project.clone(),
                session_id: session.clone(),
                limit: None,
            },
            Some(&auth),
        )
        .await;
    assert!(result.success, "{:?}", result.error);

    // The real tool output exposes validation_delta at the top level.
    let delta = &result.output["validation_delta"];
    assert_eq!(delta["comparison"]["status"], "available");
    assert_eq!(
        delta["counts"]["passed_delta"].as_i64(),
        Some(-2),
        "negative passed_delta"
    );
    assert_eq!(delta["counts"]["failed_delta"].as_i64(), Some(1));

    // The delta itself never fabricates a new verdict and never leaks raw
    // command text or raw output. (The validation *summary* block may carry a
    // derived command_summary; the delta projection must not.)
    let delta_serialized = serde_json::to_string(delta).unwrap();
    assert!(
        !delta_serialized.contains("cargo test --lib"),
        "validation_delta leaked command text"
    );
    assert!(
        !delta_serialized.contains("stdout_tail") && !delta_serialized.contains("stderr_tail"),
        "validation_delta leaked raw output"
    );

    // Reading validation_summary must not append ledger events and must not
    // enqueue an agent/runner request.
    let events_after = runtime
        .sessions
        .summary(&session, Some(200))
        .unwrap()
        .events
        .len();
    assert_eq!(
        events_after, events_before,
        "validation_summary appended events"
    );
    assert!(
        probe_patch_agent_request(&runtime, "vsummary-agent")
            .await
            .is_none(),
        "validation_summary enqueued an agent request"
    );
}

// =========================================================================
// Real entry integration: finish_coding_task
// =========================================================================

#[tokio::test]
async fn finish_coding_task_continuation_matches_handoff_attempt_without_rerunning_validation() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "finish-agent", "demo", dir.path()).await;
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("finish continuation".to_string()),
    );
    let session_id = session.session_id.clone();
    add_instruction_for(
        &runtime,
        &session_id,
        "instruction A",
        SessionMode::Normal,
        &project,
    );
    record_write_for(&runtime, &session_id, &project, &["src/lib.rs"]);
    record_validation_event_for(
        &runtime,
        &session_id,
        "cargo_test",
        false,
        test_output(0, 1, 0, &["tests::a"]),
        &project,
    );

    let handoff = runtime
        .session_handoff_summary(
            session_id.clone(),
            Some(project.clone()),
            Some(false),
            Some(false),
            Some(false),
            false,
            Some(20),
            Some(&auth),
        )
        .await;
    assert!(handoff.success, "{:?}", handoff.error);
    let handoff_feedback = handoff.output["continuation_feedback"].clone();
    assert_eq!(handoff_feedback["status"], "available");

    let events_before_finish = runtime
        .sessions
        .summary(&session_id, Some(200))
        .unwrap()
        .events
        .len();
    let finish = runtime
        .dispatch_with_auth(
            ToolCall::FinishCodingTask {
                project: project.clone(),
                session_id: session_id.clone(),
                summary_only: false,
                include_diff: Some(false),
                include_workspace: Some(false),
                include_hygiene: Some(false),
                include_handoff: Some(false),
                include_validation_summary: Some(false),
            },
            Some(&auth),
        )
        .await;
    assert!(finish.success, "{:?}", finish.error);
    let finish_feedback = &finish.output["continuation_feedback"];

    for pointer in [
        "/status",
        "/attempt/boundary",
        "/attempt/instruction",
        "/attempt/validation/latest_status",
        "/attempt/changes/total_changed_paths",
    ] {
        assert_eq!(
            finish_feedback.pointer(pointer),
            handoff_feedback.pointer(pointer),
            "finish and handoff must project the same established attempt at {pointer}"
        );
    }

    let summary = runtime.sessions.summary(&session_id, Some(200)).unwrap();
    assert!(summary.events.len() >= events_before_finish);
    assert_eq!(
        summary
            .events
            .iter()
            .filter(|event| event.kind == "tool_call_finished" && event.tool_name == "cargo_test")
            .count(),
        1,
        "finish_coding_task must not re-run validation"
    );
    assert!(
        probe_patch_agent_request(&runtime, "finish-agent")
            .await
            .is_none(),
        "finish_coding_task enqueued an agent request"
    );
}

// =========================================================================
// Meaningful-tool-call classification is consistent with tool policy
// =========================================================================

#[test]
fn meaningful_tool_classification_excludes_status_and_manifest_queries() {
    for name in known_tool_names() {
        assert_eq!(
            root_tool_is_meaningful(name),
            is_meaningful(name),
            "continuation hook drifted from canonical runtime policy for {name}"
        );
    }

    // Pure status/manifest/summary queries must never count as work progress.
    for name in [
        "runtime_status",
        "tool_manifest",
        "list_tools",
        "session_summary",
    ] {
        assert!(
            !is_meaningful(name),
            "{name} must not count as a meaningful tool call"
        );
    }
    // Write / shell / git / validation tools do count.
    for name in ["apply_text_edits", "run_shell", "git_diff", "cargo_test"] {
        assert!(is_meaningful(name), "{name} should count as meaningful");
    }
}

fn is_meaningful(name: &str) -> bool {
    runtime_tool_is_write_like(name)
        || runtime_tool_is_shell_like(name)
        || runtime_tool_is_git_like(name)
        || runtime_tool_captures_validation_output(name)
}

#[test]
fn current_validation_window_excludes_pre_mutation_failure_from_attempt_activity() {
    let runtime = test_runtime();
    let session = create_session(&runtime, "current validation window");
    add_instruction(
        &runtime,
        &session,
        "fix validation and continue",
        SessionMode::Normal,
    );
    record_validation_event(
        &runtime,
        &session,
        "cargo_test",
        false,
        test_output(0, 1, 0, &["tests::parser"]),
    );
    record_write(&runtime, &session, &["src/parser.rs"]);
    record_validation_event(&runtime, &session, "cargo_check", true, check_output(0, 1));

    let summary = runtime.sessions.summary(&session, Some(200)).unwrap();
    let historical = validation_summary_from_events(&summary.events, 20);
    assert_eq!(historical["status"], "mixed");
    assert_eq!(historical["unresolved_failures"]["count"], 1);

    let feedback = feedback_for(&runtime, &summary, "continued");
    assert_eq!(feedback["attempt"]["activity"]["unresolved_failures"], 0);
    assert_eq!(feedback["attempt"]["validation"]["status"], "passed");
    assert_eq!(
        feedback["attempt"]["validation"]["unresolved_failure_count"],
        0
    );
    assert_eq!(feedback["attempt"]["validation"]["validation_events"], 1);
    assert_eq!(feedback["attempt"]["validation"]["stale_failure_count"], 1);
}

// =========================================================================
// Helpers
// =========================================================================

fn create_session(runtime: &ToolRuntime, title: &str) -> String {
    runtime
        .sessions
        .start_session(Some("test-project".to_string()), Some(title.to_string()))
        .session_id
}

fn add_instruction(runtime: &ToolRuntime, session_id: &str, instruction: &str, mode: SessionMode) {
    runtime
        .sessions
        .ensure_coding_session(sessions::CodingSessionRequest {
            project: "test-project".to_string(),
            authority_fingerprint: sessions::TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT
                .to_string(),
            resume_session_id: Some(session_id.to_string()),
            instruction: Some(instruction.to_string()),
            mode,
            guards: SessionGuards::default(),
            execution_context: None,
            project_instructions: None,
            transport: SessionTransport::Api,
            context_refreshed: true,
            write_scope_verified: true,
        })
        .unwrap();
}

fn record_validation_event(
    runtime: &ToolRuntime,
    session_id: &str,
    tool_name: &str,
    success: bool,
    output: Value,
) {
    let start = runtime.sessions.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        tool_name,
        &json!({"project": "test-project"}),
        crate::tool_runtime::sessions::session_tool_contract(tool_name),
    );
    runtime.sessions.record_tool_call_finished(
        start,
        success,
        &output,
        (!success).then_some("validation failed"),
        None,
    );
}

/// Record a finished write tool call (`apply_text_edits`) whose `changes` carry
/// the given changed paths. The ledger records the deduped changed paths from
/// the start event and surfaces them on the finished event, the same extraction
/// the production `closeout_work_projection` reads.
fn record_write(runtime: &ToolRuntime, session_id: &str, paths: &[&str]) {
    record_write_for(runtime, session_id, "test-project", paths);
}

fn record_write_for(runtime: &ToolRuntime, session_id: &str, project: &str, paths: &[&str]) {
    let changes: Vec<Value> = paths
        .iter()
        .map(|path| json!({"kind": "edit", "path": path}))
        .collect();
    let start = runtime.sessions.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        "apply_text_edits",
        &json!({
            "project": project,
            "changes": changes,
        }),
        crate::tool_runtime::sessions::session_tool_contract("apply_text_edits"),
    );
    runtime.sessions.record_tool_call_finished(
        start,
        true,
        &json!({"applied": true, "state_changed": true}),
        None,
        None,
    );
}

fn record_exploration_event(
    runtime: &ToolRuntime,
    session_id: &str,
    tool_name: &str,
    arguments: Value,
    success: bool,
    output: Value,
) {
    let start = runtime.sessions.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        tool_name,
        &arguments,
        crate::tool_runtime::sessions::session_tool_contract(tool_name),
    );
    runtime.sessions.record_tool_call_finished(
        start,
        success,
        &output,
        (!success).then_some("exploration failed"),
        None,
    );
}

/// Project-scoped variants of the helpers above for real entry tests that
/// dispatch tools against a registered agent project (e.g. validation_summary).
fn add_instruction_for(
    runtime: &ToolRuntime,
    session_id: &str,
    instruction: &str,
    mode: SessionMode,
    project: &str,
) {
    runtime
        .sessions
        .ensure_coding_session(sessions::CodingSessionRequest {
            project: project.to_string(),
            authority_fingerprint: sessions::TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT
                .to_string(),
            resume_session_id: Some(session_id.to_string()),
            instruction: Some(instruction.to_string()),
            mode,
            guards: SessionGuards::default(),
            execution_context: None,
            project_instructions: None,
            transport: SessionTransport::Api,
            context_refreshed: true,
            write_scope_verified: true,
        })
        .unwrap();
}

fn record_validation_event_for(
    runtime: &ToolRuntime,
    session_id: &str,
    tool_name: &str,
    success: bool,
    output: Value,
    project: &str,
) {
    let start = runtime.sessions.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        tool_name,
        &json!({"project": project}),
        crate::tool_runtime::sessions::session_tool_contract(tool_name),
    );
    runtime.sessions.record_tool_call_finished(
        start,
        success,
        &output,
        (!success).then_some("validation failed"),
        None,
    );
}

fn post_session_message(runtime: &ToolRuntime, session_id: &str, kind: &str, message: &str) {
    use crate::tool_runtime::sessions::{
        PostSessionMessageInput, SessionMessageKind, SessionMessagePriority,
    };
    let kind = match kind {
        "note" => SessionMessageKind::Note,
        "proposal" => SessionMessageKind::Proposal,
        "question" => SessionMessageKind::Question,
        "answer" => SessionMessageKind::Answer,
        "decision" => SessionMessageKind::Decision,
        "risk" => SessionMessageKind::Risk,
        "progress" => SessionMessageKind::Progress,
        "guidance" => SessionMessageKind::Guidance,
        "todo" => SessionMessageKind::Todo,
        _ => panic!("unknown message kind: {kind}"),
    };
    runtime
        .sessions
        .post_message(PostSessionMessageInput {
            session_id: session_id.to_string(),
            kind,
            message: message.to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
}

fn check_output(errors: usize, passed: usize) -> Value {
    let mut stderr = String::new();
    for i in 0..errors {
        stderr.push_str(&format!("error[E0308]: message {i}\n --> src/f{i}:1:1\n"));
    }
    let stdout = if passed > 0 {
        format!("test result: ok. {passed} passed; 0 failed")
    } else {
        String::new()
    };
    json!({
        "exit_code": if errors > 0 { 101 } else { 0 },
        "stdout_tail": stdout,
        "stderr_tail": stderr,
        "stdout_truncated": false,
        "stderr_truncated": false,
        "failure_kind": if errors > 0 { json!("validation_failed") } else { Value::Null },
        "cwd": "crates/webcodex",
        "command_summary": "cargo check",
    })
}

/// Build a cargo `test` validation output. The parser combines stdout and
/// stderr line-by-line, so the single `test result:` summary line and the
/// `test <name> ... FAILED` lines are placed in stdout only to avoid
/// double-counting. `failed_names` are the bare test module paths (e.g.
/// `tests::a`); they are rendered as `test <name> ... FAILED` so the existing
/// parser captures stable failure identities.
fn test_output(passed: u64, failed: u64, ignored: u64, failed_names: &[&str]) -> Value {
    let mut stdout = String::new();
    for name in failed_names {
        stdout.push_str(&format!("test {name} ... FAILED\n"));
    }
    if failed == 0 {
        stdout.push_str(&format!(
            "test result: ok. {passed} passed; 0 failed; {ignored} ignored"
        ));
    } else {
        stdout.push_str(&format!(
            "test result: FAILED. {passed} passed; {failed} failed; {ignored} ignored"
        ));
    }
    json!({
        "exit_code": if failed > 0 { 101 } else { 0 },
        "stdout_tail": stdout,
        "stderr_tail": "",
        "stdout_truncated": false,
        "stderr_truncated": false,
        "failure_kind": if failed > 0 { json!("test_failure") } else { Value::Null },
        "cwd": "crates/webcodex",
        "command_summary": "cargo test --lib",
    })
}

fn empty_discussion() -> sessions::SessionDiscussionSummary {
    sessions::SessionDiscussionSummary {
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

fn feedback_for(
    runtime: &ToolRuntime,
    summary: &sessions::SessionSummary,
    continuation: &'static str,
) -> Value {
    let discussion = runtime
        .sessions
        .discussion_summary(&summary.session_id, Some(20))
        .unwrap_or_else(|_| empty_discussion());
    feedback_for_with_discussion(runtime, summary, &discussion, continuation)
}

fn feedback_for_with_discussion(
    _runtime: &ToolRuntime,
    summary: &sessions::SessionSummary,
    discussion: &sessions::SessionDiscussionSummary,
    continuation: &'static str,
) -> Value {
    let validation = validation_summary_from_events(&summary.events, 20);
    let current_validation = current_validation_evidence_for_session(summary, 20);
    let tool_failures = tool_failure_summary_from_events(&summary.events, 20);
    let reconciliation = reconcile_closeout_evidence(&tool_failures, summary, &validation);
    let jobs = json!({"active_count": 0, "terminal_pending_count": 0, "recent": []});
    continuation_feedback_value(ContinuationFeedbackInput {
        session_summary: summary,
        validation: &validation,
        jobs: &jobs,
        discussion,
        continuation,
        suggest_exploration_continuity: true,
        workspace_conflicts: false,
        hooks: continuation_projection_hooks(),
        current_validation: continuation_validation_snapshot(&current_validation),
        tool_failures: ContinuationToolFailureSnapshot::new(
            &reconciliation.actionable_unexpected_event_ids,
        ),
    })
}

fn feedback_for_with_jobs(
    _runtime: &ToolRuntime,
    summary: &sessions::SessionSummary,
    jobs: &Value,
    continuation: &'static str,
) -> Value {
    let validation = validation_summary_from_events(&summary.events, 20);
    let current_validation = current_validation_evidence_for_session(summary, 20);
    let tool_failures = tool_failure_summary_from_events(&summary.events, 20);
    let reconciliation = reconcile_closeout_evidence(&tool_failures, summary, &validation);
    let discussion = empty_discussion();
    continuation_feedback_value(ContinuationFeedbackInput {
        session_summary: summary,
        validation: &validation,
        jobs,
        discussion: &discussion,
        continuation,
        suggest_exploration_continuity: true,
        workspace_conflicts: false,
        hooks: continuation_projection_hooks(),
        current_validation: continuation_validation_snapshot(&current_validation),
        tool_failures: ContinuationToolFailureSnapshot::new(
            &reconciliation.actionable_unexpected_event_ids,
        ),
    })
}
