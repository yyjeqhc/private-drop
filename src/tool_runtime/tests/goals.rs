use super::support::*;
use crate::auth::scopes::{
    COMMUNICATION_MANAGE_SCOPES, COMMUNICATION_READ_SCOPES, SCOPE_COMMUNICATION_MANAGE,
    SCOPE_COMMUNICATION_READ, SCOPE_SESSION_COLLABORATE,
};
use crate::tool_runtime::metadata::{
    ToolApprovalPolicy, ToolAuthorityPolicy, ToolEffect, ToolIdempotency, ToolRisk,
};
use crate::tool_runtime::tool_definition::{lookup_tool_definition, RunnerCapabilityRequirement};
use crate::tool_runtime::{registered_tool_specs, ToolCall, ToolRuntime};
use serde_json::json;
use std::sync::Arc;

fn runtime_with_goal_db() -> (tempfile::TempDir, Arc<crate::db::Database>, ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(crate::db::Database::open(&temp.path().join("goals.db")).unwrap());
    let runtime = ToolRuntime::new_for_tests().with_communication_database(db.clone());
    (temp, db, runtime)
}

fn create_goal(
    runtime: &ToolRuntime,
    auth: Option<&crate::auth::AuthContext>,
    key: &str,
) -> String {
    let created = runtime.create_goal(
        auth,
        "Durable Goal Phase 1".to_string(),
        "Keep high-level durable intent authoritative and independent from execution domains."
            .to_string(),
        key.to_string(),
    );
    assert!(created.success, "{:?}", created.output);
    created.output["goal"]["summary"]["goal_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn goal_tools_are_control_only_and_never_declare_execution_authority() {
    for name in [
        "create_goal",
        "get_goal",
        "present_goal_plan",
        "list_goals",
        "update_goal",
        "associate_goal_agent_task",
        "associate_goal_workflow_session",
    ] {
        let definition = lookup_tool_definition(name).unwrap_or_else(|| panic!("missing {name}"));
        assert!(
            definition.model_spec.is_some(),
            "{name} must be model-visible"
        );
        assert_eq!(definition.category, "goal");
        assert_eq!(definition.metadata.provider_id, "control");
        assert!(!definition.metadata.requires_project, "{name}");
        assert_eq!(
            definition.runner_capability, None::<RunnerCapabilityRequirement>,
            "{name}"
        );
        assert!(!definition.metadata.shell_like, "{name}");
        assert!(!matches!(
            definition.metadata.risk,
            ToolRisk::ProjectWrite | ToolRisk::JobRun
        ));
    }

    for name in ["get_goal", "list_goals", "present_goal_plan"] {
        let definition = lookup_tool_definition(name).unwrap();
        assert_eq!(definition.metadata.effect, ToolEffect::Observe);
        assert_eq!(definition.metadata.risk, ToolRisk::Read);
        assert_eq!(definition.metadata.approval, ToolApprovalPolicy::None);
        assert_eq!(definition.metadata.idempotency, ToolIdempotency::PureRead);
        assert_eq!(
            definition.metadata.authority,
            ToolAuthorityPolicy::RequireAll(COMMUNICATION_READ_SCOPES)
        );
    }

    for name in ["create_goal", "update_goal", "associate_goal_agent_task"] {
        let definition = lookup_tool_definition(name).unwrap();
        assert_eq!(definition.metadata.effect, ToolEffect::Mutate);
        assert_eq!(definition.metadata.risk, ToolRisk::WorkflowManage);
        assert_eq!(definition.metadata.approval, ToolApprovalPolicy::Standard);
        assert_eq!(definition.metadata.idempotency, ToolIdempotency::Keyed);
        assert_eq!(
            definition.metadata.authority,
            ToolAuthorityPolicy::RequireAll(COMMUNICATION_MANAGE_SCOPES)
        );
    }

    let app_state = lookup_tool_definition("goal_plan_state").unwrap();
    assert!(app_state.model_spec.is_none());
    assert_eq!(app_state.category, "goal");
    assert_eq!(app_state.metadata.provider_id, "control");
    assert!(!app_state.metadata.requires_project);
    assert_eq!(
        app_state.runner_capability,
        None::<RunnerCapabilityRequirement>
    );
    assert!(!app_state.metadata.shell_like);
    assert_eq!(app_state.metadata.effect, ToolEffect::Observe);
    assert_eq!(app_state.metadata.risk, ToolRisk::Read);
    assert_eq!(app_state.metadata.approval, ToolApprovalPolicy::None);
    assert_eq!(app_state.metadata.idempotency, ToolIdempotency::PureRead);
    assert_eq!(
        app_state.metadata.authority,
        ToolAuthorityPolicy::RequireAll(COMMUNICATION_READ_SCOPES)
    );

    let session_link = lookup_tool_definition("associate_goal_workflow_session").unwrap();
    assert_eq!(session_link.metadata.idempotency, ToolIdempotency::Keyed);
    assert_eq!(
        session_link.metadata.authority,
        ToolAuthorityPolicy::RequireAll(&[
            SCOPE_COMMUNICATION_READ,
            SCOPE_COMMUNICATION_MANAGE,
            SCOPE_SESSION_COLLABORATE,
        ])
    );
}

#[test]
fn goal_schemas_are_bounded_private_and_existing_coding_tools_do_not_accept_goal_id() {
    let specs = registered_tool_specs();
    let spec = |name: &str| spec_named(&specs, name);

    let create = spec("create_goal");
    assert_eq!(create.input_schema["properties"]["title"]["maxLength"], 200);
    assert_eq!(
        create.input_schema["properties"]["objective"]["maxLength"],
        8192
    );
    assert_eq!(
        create.input_schema["properties"]["idempotency_key"]["maxLength"],
        128
    );

    let update = spec("update_goal");
    assert_eq!(
        update.input_schema["properties"]["lifecycle"]["enum"],
        json!(["active", "completed", "cancelled"])
    );
    assert_eq!(
        update.input_schema["properties"]["terminal_reason"]["maxLength"],
        4096
    );

    let present = spec("present_goal_plan");
    assert_eq!(present.input_schema["required"], json!(["goal_id"]));
    let plan = &present.output_schema["properties"]["output"]["properties"]["goal_plan"];
    assert_eq!(plan["properties"]["version"]["const"], 1);
    assert_eq!(plan["properties"]["title"]["maxLength"], 200);
    assert_eq!(plan["properties"]["objective"]["maxLength"], 8192);
    assert_eq!(
        plan["properties"]["lifecycle"]["enum"],
        json!(["active", "completed", "cancelled"])
    );
    assert_eq!(plan["properties"]["agent_task_count"]["maximum"], 64);
    assert_eq!(plan["properties"]["workflow_session_count"]["maximum"], 64);
    let plan_properties = plan["properties"].as_object().unwrap();
    for forbidden_property in [
        "correlations",
        "terminal_reason",
        "attempt_fence",
        "authority_fingerprint",
        "consume_token",
        "wake_token",
        "session_ledger",
        "stdout",
        "stderr",
        "job_log",
    ] {
        assert!(
            !plan_properties.contains_key(forbidden_property),
            "Goal Plan schema exposed private property {forbidden_property}"
        );
    }
    let app_specs = crate::tool_runtime::goal_plan_app_tool_specs();
    assert_eq!(app_specs.len(), 1);
    assert_eq!(app_specs[0].name, "goal_plan_state");
    assert_eq!(app_specs[0].input_schema["required"], json!(["goal_id"]));
    assert!(!registered_tool_names()
        .iter()
        .any(|name| name == "goal_plan_state"));

    let list_summary =
        &spec("list_goals").output_schema["properties"]["output"]["properties"]["goals"]["items"];
    for field in [
        "goal_id",
        "title",
        "lifecycle",
        "revision",
        "created_at_unix_ms",
        "updated_at_unix_ms",
        "terminal_at_unix_ms",
        "agent_task_count",
        "workflow_session_count",
    ] {
        assert!(list_summary["properties"].get(field).is_some(), "{field}");
    }
    for private in ["objective", "terminal_reason", "correlations"] {
        assert!(
            list_summary["properties"].get(private).is_none(),
            "{private}"
        );
    }

    let detail = &spec("get_goal").output_schema["properties"]["output"]["properties"]["goal"];
    let correlation = &detail["properties"]["correlations"]["items"];
    assert_eq!(
        correlation["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "created_at_unix_ms".to_string(),
            "kind".to_string(),
            "reference_id".to_string(),
        ]
    );
    let serialized_detail = serde_json::to_string(detail).unwrap();
    for forbidden in [
        "attempt_fence",
        "authority_fingerprint",
        "consume_token",
        "wake_token",
        "session_ledger",
        "stdout",
        "stderr",
    ] {
        assert!(
            !serialized_detail.contains(forbidden),
            "Goal detail schema leaked execution/private field {forbidden}"
        );
    }

    for existing in [
        "work_on_project",
        "read_files",
        "apply_text_edits",
        "run_process",
        "run_shell",
        "finish_coding_task",
    ] {
        let properties = spec(existing).input_schema["properties"]
            .as_object()
            .unwrap();
        assert!(
            !properties.contains_key("goal_id"),
            "{existing} must not gain Goal as an implicit selector or required execution context"
        );
    }
}

#[test]
fn goal_tool_calls_and_audit_keep_goal_identity_distinct_and_private_text_out_of_logs() {
    let goal_id = format!("wc_goal_{}", "0".repeat(32));
    let call = ToolCall::from_tool_name(
        "update_goal",
        json!({
            "goal_id": goal_id,
            "expected_revision": 4,
            "objective": "PRIVATE_GOAL_OBJECTIVE_DO_NOT_LOG",
            "lifecycle": "completed",
            "terminal_reason": "PRIVATE_GOAL_REASON_DO_NOT_LOG",
            "idempotency_key": "PRIVATE_GOAL_KEY_DO_NOT_LOG"
        }),
    )
    .unwrap();
    assert_eq!(call.tool_name(), "update_goal");

    let audit = crate::tool_runtime::tool_audit::session_log_arguments_for_tool_request(
        "update_goal",
        &json!({
            "goal_id": goal_id,
            "expected_revision": 4,
            "objective": "PRIVATE_GOAL_OBJECTIVE_DO_NOT_LOG",
            "lifecycle": "completed",
            "terminal_reason": "PRIVATE_GOAL_REASON_DO_NOT_LOG",
            "idempotency_key": "PRIVATE_GOAL_KEY_DO_NOT_LOG"
        }),
    );
    assert_eq!(audit["goal_id"], goal_id);
    assert_eq!(audit["expected_revision"], 4);
    assert_eq!(
        audit["objective_bytes"],
        "PRIVATE_GOAL_OBJECTIVE_DO_NOT_LOG".len()
    );
    assert_eq!(
        audit["terminal_reason_bytes"],
        "PRIVATE_GOAL_REASON_DO_NOT_LOG".len()
    );
    assert_eq!(audit["idempotency_key_present"], true);
    let serialized = serde_json::to_string(&audit).unwrap();
    for private in [
        "PRIVATE_GOAL_OBJECTIVE_DO_NOT_LOG",
        "PRIVATE_GOAL_REASON_DO_NOT_LOG",
        "PRIVATE_GOAL_KEY_DO_NOT_LOG",
    ] {
        assert!(!serialized.contains(private), "Goal audit leaked {private}");
    }

    let plan_audit = crate::tool_runtime::tool_audit::session_log_arguments_for_tool_request(
        "present_goal_plan",
        &json!({"goal_id": goal_id}),
    );
    assert_eq!(plan_audit, json!({"goal_id": goal_id}));
    let private_objective = "PRIVATE_GOAL_PLAN_OBJECTIVE_DO_NOT_AUDIT";
    let result_audit = crate::tool_runtime::tool_audit::session_log_result_for_tool(
        "present_goal_plan",
        &json!({
            "goal_plan": {
                "goal_id": goal_id,
                "title": "Private title",
                "objective": private_objective,
                "lifecycle": "active",
                "revision": 7,
                "updated_at_unix_ms": 1,
                "terminal_at_unix_ms": null,
                "agent_task_count": 2,
                "workflow_session_count": 3
            }
        }),
    );
    assert_eq!(result_audit["goal_id"], goal_id);
    assert_eq!(result_audit["revision"], 7);
    assert_eq!(result_audit["agent_task_count"], 2);
    assert_eq!(result_audit["workflow_session_count"], 3);
    assert!(!serde_json::to_string(&result_audit)
        .unwrap()
        .contains(private_objective));

    assert!(ToolCall::from_tool_name(
        "associate_goal_agent_task",
        json!({
            "goal_id": format!("wc_goal_{}", "1".repeat(32)),
            "task_id": format!("wc_agent_task_{}", "2".repeat(32)),
            "idempotency_key": "link"
        })
    )
    .is_ok());
}

#[test]
fn goal_runtime_crud_replay_and_exact_read_hide_foreign_existence() {
    let (_temp, _db, runtime) = runtime_with_goal_db();
    let bob = auth_context(Some("bob-goal"), false);
    let alice = auth_context(Some("alice-goal"), false);

    let first = runtime.create_goal(
        Some(&bob),
        "Private Bob Goal".to_string(),
        "Private objective".to_string(),
        "bob-create-goal".to_string(),
    );
    assert!(first.success, "{:?}", first.output);
    let goal_id = first.output["goal"]["summary"]["goal_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(first.output["goal"]["summary"]["revision"], 1);
    assert_eq!(first.output["goal"]["summary"]["lifecycle"], "active");

    let replay = runtime.create_goal(
        Some(&bob),
        "Private Bob Goal".to_string(),
        "Private objective".to_string(),
        "bob-create-goal".to_string(),
    );
    assert!(replay.success);
    assert_eq!(replay.output["replayed"], true);
    assert_eq!(replay.output["goal"]["summary"]["goal_id"], goal_id);

    let changed = runtime.create_goal(
        Some(&bob),
        "Private Bob Goal".to_string(),
        "Changed objective".to_string(),
        "bob-create-goal".to_string(),
    );
    assert!(!changed.success);
    assert_eq!(changed.output["error_kind"], "goal_idempotency_conflict");

    let foreign = runtime.get_goal(Some(&alice), goal_id.clone());
    let missing = runtime.get_goal(Some(&alice), format!("wc_goal_{}", "f".repeat(32)));
    assert!(!foreign.success);
    assert!(!missing.success);
    assert_eq!(foreign.output["error_kind"], "goal_not_found");
    assert_eq!(foreign.output["error_kind"], missing.output["error_kind"]);
    assert_eq!(foreign.error, missing.error);

    let updated = runtime.update_goal(
        Some(&bob),
        goal_id.clone(),
        1,
        Some("Updated Bob Goal".to_string()),
        None,
        None,
        None,
        "bob-update-goal".to_string(),
    );
    assert!(updated.success, "{:?}", updated.output);
    assert_eq!(updated.output["goal"]["summary"]["revision"], 2);

    let update_replay = runtime.update_goal(
        Some(&bob),
        goal_id,
        1,
        Some("Updated Bob Goal".to_string()),
        None,
        None,
        None,
        "bob-update-goal".to_string(),
    );
    assert!(update_replay.success);
    assert_eq!(update_replay.output["replayed"], true);
    assert_eq!(update_replay.output["goal"]["summary"]["revision"], 2);
}

#[test]
fn goal_plan_projection_is_exact_pure_revisioned_terminal_and_existence_hidden() {
    let (_temp, _db, runtime) = runtime_with_goal_db();
    let bob = auth_context(Some("bob-plan"), false);
    let alice = auth_context(Some("alice-plan"), false);
    let goal_id = create_goal(&runtime, Some(&bob), "bob-plan-create");

    let initial = runtime.present_goal_plan(Some(&bob), goal_id.clone());
    assert!(initial.success, "{:?}", initial.output);
    let plan = &initial.output["goal_plan"];
    assert_eq!(plan["version"], 1);
    assert_eq!(plan["goal_id"], goal_id);
    assert_eq!(plan["lifecycle"], "active");
    assert_eq!(plan["revision"], 1);
    assert_eq!(plan["agent_task_count"], 0);
    assert_eq!(plan["workflow_session_count"], 0);
    assert!(plan["terminal_at_unix_ms"].is_null());
    assert_eq!(
        plan.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "agent_task_count",
            "goal_id",
            "lifecycle",
            "objective",
            "revision",
            "terminal_at_unix_ms",
            "title",
            "updated_at_unix_ms",
            "version",
            "workflow_session_count"
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );

    let polled = runtime.goal_plan_state(Some(&bob), goal_id.clone());
    assert!(polled.success);
    assert_eq!(polled.output["goal_plan"]["revision"], 1);
    assert_eq!(
        runtime.get_goal(Some(&bob), goal_id.clone()).output["goal"]["summary"]["revision"],
        1
    );

    let foreign = runtime.goal_plan_state(Some(&alice), goal_id.clone());
    let missing = runtime.goal_plan_state(Some(&alice), format!("wc_goal_{}", "f".repeat(32)));
    assert!(!foreign.success);
    assert!(!missing.success);
    assert_eq!(foreign.output["error_kind"], "goal_not_found");
    assert_eq!(foreign.error, missing.error);

    let updated = runtime.update_goal(
        Some(&bob),
        goal_id.clone(),
        1,
        None,
        Some("Updated durable plan objective".to_string()),
        None,
        None,
        "bob-plan-update".to_string(),
    );
    assert!(updated.success, "{:?}", updated.output);
    let after_update = runtime.goal_plan_state(Some(&bob), goal_id.clone());
    assert!(after_update.success);
    assert_eq!(after_update.output["goal_plan"]["revision"], 2);
    assert_eq!(
        after_update.output["goal_plan"]["objective"],
        "Updated durable plan objective"
    );

    let completed = runtime.update_goal(
        Some(&bob),
        goal_id.clone(),
        2,
        None,
        None,
        Some("completed".to_string()),
        Some("done".to_string()),
        "bob-plan-complete".to_string(),
    );
    assert!(completed.success, "{:?}", completed.output);
    let terminal = runtime.goal_plan_state(Some(&bob), goal_id.clone());
    assert!(terminal.success);
    assert_eq!(terminal.output["goal_plan"]["lifecycle"], "completed");
    assert_eq!(terminal.output["goal_plan"]["revision"], 3);
    assert!(terminal.output["goal_plan"]["terminal_at_unix_ms"].is_i64());
    assert_eq!(
        runtime.get_goal(Some(&bob), goal_id).output["goal"]["summary"]["revision"],
        3
    );
}

#[test]
fn goal_plan_projection_fails_closed_on_malformed_persisted_goal() {
    let (_temp, db, runtime) = runtime_with_goal_db();
    let bob = auth_context(Some("bob-plan-corrupt"), false);
    let goal_id = create_goal(&runtime, Some(&bob), "bob-plan-corrupt-create");
    {
        let conn = db.conn_for_tests();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        conn.execute(
            "UPDATE wc_goals SET lifecycle = 'waiting_validation' WHERE goal_id = ?1",
            [goal_id.as_str()],
        )
        .unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .unwrap();
    }
    let presented = runtime.present_goal_plan(Some(&bob), goal_id.clone());
    let polled = runtime.goal_plan_state(Some(&bob), goal_id);
    assert!(!presented.success);
    assert!(!polled.success);
    assert_eq!(presented.output["error_kind"], "goal_store_unavailable");
    assert_eq!(polled.output["error_kind"], "goal_store_unavailable");
}

#[test]
fn goal_agent_task_link_reauthorizes_task_and_task_completion_never_completes_goal() {
    let (_temp, db, runtime) = runtime_with_goal_db();
    let bob = auth_context(Some("bob-task-goal"), false);
    let alice = auth_context(Some("alice-task-goal"), false);
    let goal_id = create_goal(&runtime, Some(&bob), "bob-task-goal-create");

    let bob_agent = runtime.create_agent_identity(
        Some(&bob),
        "bob-goal-worker".to_string(),
        "Bob Goal Worker".to_string(),
        None,
        Vec::new(),
        "bob-goal-worker-create".to_string(),
    );
    assert!(bob_agent.success);
    let bob_agent_id = bob_agent.output["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    let bob_task = runtime.create_agent_task(
        Some(&bob),
        "Goal-correlated task".to_string(),
        "Complete one explicit durable work chunk.".to_string(),
        Some(bob_agent_id.clone()),
        None,
        None,
        Some("agent:special:reference-is-not-authority".to_string()),
        "bob-goal-task-create".to_string(),
    );
    assert!(bob_task.success, "{:?}", bob_task.output);
    let bob_task_id = bob_task.output["task"]["summary"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();

    let linked = runtime.associate_goal_agent_task(
        Some(&bob),
        goal_id.clone(),
        bob_task_id.clone(),
        "bob-goal-task-link".to_string(),
    );
    assert!(linked.success, "{:?}", linked.output);
    assert_eq!(linked.output["goal"]["summary"]["revision"], 2);
    assert_eq!(linked.output["goal"]["summary"]["agent_task_count"], 1);
    assert_eq!(
        db.conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_task_coding_runs",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0,
        "Goal correlation must not dispatch an AgentTask CodingAgentRun"
    );

    let alice_agent = runtime.create_agent_identity(
        Some(&alice),
        "alice-goal-worker".to_string(),
        "Alice Goal Worker".to_string(),
        None,
        Vec::new(),
        "alice-goal-worker-create".to_string(),
    );
    assert!(alice_agent.success);
    let alice_agent_id = alice_agent.output["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    let alice_task = runtime.create_agent_task(
        Some(&alice),
        "Alice private task".to_string(),
        "Private Alice work".to_string(),
        Some(alice_agent_id),
        None,
        None,
        None,
        "alice-private-task-create".to_string(),
    );
    assert!(alice_task.success);
    let alice_task_id = alice_task.output["task"]["summary"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let foreign_link = runtime.associate_goal_agent_task(
        Some(&bob),
        goal_id.clone(),
        alice_task_id,
        "foreign-task-link".to_string(),
    );
    assert!(!foreign_link.success);
    assert_eq!(foreign_link.output["error_kind"], "agent_task_not_found");

    let attempt = runtime.start_agent_task_attempt(
        Some(&bob),
        bob_task_id.clone(),
        bob_agent_id.clone(),
        "bob-goal-task-attempt".to_string(),
    );
    assert!(attempt.success, "{:?}", attempt.output);
    let attempt_id = attempt.output["attempt"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_string();
    let fence = attempt.output["attempt_fence"]
        .as_str()
        .unwrap()
        .to_string();
    let completed = runtime.complete_agent_task_attempt(
        Some(&bob),
        bob_task_id.clone(),
        attempt_id.clone(),
        bob_agent_id.clone(),
        fence,
        1,
        "succeeded".to_string(),
        Some("Task work finished".to_string()),
        None,
        "bob-goal-task-complete".to_string(),
    );
    assert!(completed.success, "{:?}", completed.output);
    assert_eq!(completed.output["task"]["state"], "succeeded");

    let goal = runtime.get_goal(Some(&bob), goal_id.clone());
    assert!(goal.success, "{:?}", goal.output);
    assert_eq!(goal.output["goal"]["summary"]["lifecycle"], "active");
    assert_eq!(goal.output["goal"]["summary"]["revision"], 2);
    assert!(goal.output["goal"]["summary"]["terminal_at_unix_ms"].is_null());

    let (event_id, wake_id): (String, String) = db
        .conn_for_tests()
        .query_row(
            "SELECT e.event_id, w.wake_id
             FROM wc_agent_attention_events e
             JOIN wc_agent_wakes w ON w.source_event_id = e.event_id
             WHERE e.kind = 'agent_task_terminal'
               AND e.goal_id = ?1 AND e.task_id = ?2 AND e.task_attempt_id = ?3
               AND e.target_agent_id = ?4 AND e.terminal_task_state = 'succeeded'
               AND w.trigger_kind = 'attention_event' AND w.state = 'pending'",
            rusqlite::params![goal_id, bob_task_id, attempt_id, bob_agent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    let endpoint = runtime.attach_agent_endpoint(
        Some(&bob),
        bob_agent_id.clone(),
        "ChatGPT".to_string(),
        Some("goal-attention-runtime".to_string()),
        "goal-attention-runtime-endpoint".to_string(),
    );
    assert!(endpoint.success, "{:?}", endpoint.output);
    let endpoint_id = endpoint.output["endpoint"]["endpoint_id"]
        .as_str()
        .unwrap()
        .to_string();
    let generation = endpoint.output["endpoint"]["controller_generation"]
        .as_i64()
        .unwrap();
    let bootstrap = runtime.bootstrap_agent_conversation(
        Some(&bob),
        bob_agent_id,
        endpoint_id,
        generation,
        None,
        Some(wake_id),
        None,
    );
    assert!(bootstrap.success, "{:?}", bootstrap.output);
    assert_eq!(bootstrap.output["wake"]["trigger_kind"], "attention_event");
    assert_eq!(bootstrap.output["wake"]["event_id"], event_id);
    assert_eq!(bootstrap.output["wake"]["goal_id"], goal_id);
    assert_eq!(bootstrap.output["wake"]["task_id"], bob_task_id);
    assert_eq!(bootstrap.output["wake"]["task_attempt_id"], attempt_id);

    let explicitly_completed = runtime.update_goal(
        Some(&bob),
        goal_id.clone(),
        2,
        None,
        None,
        Some("completed".to_string()),
        Some("Explicit model decision after terminal task attention".to_string()),
        "bob-goal-after-attention-complete".to_string(),
    );
    assert!(
        explicitly_completed.success,
        "{:?}",
        explicitly_completed.output
    );
    assert_eq!(
        explicitly_completed.output["goal"]["summary"]["lifecycle"],
        "completed"
    );
    assert_eq!(
        explicitly_completed.output["goal"]["summary"]["revision"],
        3
    );
}

#[tokio::test]
async fn workflow_session_link_is_identity_only_and_finish_coding_task_does_not_complete_goal() {
    let temp = tempfile::tempdir().unwrap();
    init_git_repo(temp.path());
    commit_file(temp.path(), "README.md", "hello\n", "add readme");
    let db = Arc::new(crate::db::Database::open(&temp.path().join("goal-session.db")).unwrap());
    let runtime = ToolRuntime::new_for_tests().with_communication_database(db);
    let project =
        register_runner_project_at_path(&runtime, "goal-session-finish", "demo", temp.path()).await;
    let auth = auth_context(None, true);
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("Goal-correlated coding work".to_string()),
    );
    let mut limited = auth_context(Some("goal-session-limited"), false);
    limited.scopes = vec![
        SCOPE_COMMUNICATION_READ.to_string(),
        SCOPE_COMMUNICATION_MANAGE.to_string(),
        SCOPE_SESSION_COLLABORATE.to_string(),
    ];
    let limited_goal_id = create_goal(&runtime, Some(&limited), "limited-session-goal-create");
    let unauthorized_link = runtime
        .associate_goal_workflow_session(
            Some(&limited),
            limited_goal_id.clone(),
            session.session_id.clone(),
            "limited-session-goal-link".to_string(),
        )
        .await;
    assert!(
        !unauthorized_link.success,
        "Goal ownership and session:collaborate must not grant bound Project authority"
    );
    let limited_goal = runtime.get_goal(Some(&limited), limited_goal_id);
    assert!(limited_goal.success, "{:?}", limited_goal.output);
    assert_eq!(limited_goal.output["goal"]["summary"]["revision"], 1);
    assert_eq!(
        limited_goal.output["goal"]["summary"]["workflow_session_count"],
        0
    );

    let goal_id = create_goal(&runtime, Some(&auth), "session-goal-create");

    let linked = runtime
        .associate_goal_workflow_session(
            Some(&auth),
            goal_id.clone(),
            session.session_id.clone(),
            "session-goal-link".to_string(),
        )
        .await;
    assert!(linked.success, "{:?}", linked.output);
    assert_eq!(linked.output["goal"]["summary"]["revision"], 2);
    assert_eq!(
        linked.output["goal"]["summary"]["workflow_session_count"],
        1
    );

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::FinishCodingTask {
                        project,
                        session_id,
                        summary_only: true,
                        include_diff: Some(false),
                        include_workspace: Some(true),
                        include_hygiene: Some(false),
                        include_handoff: Some(false),
                        include_validation_summary: Some(false),
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "goal-session-finish").await;
    let show_changes_stdout =
        crate::tool_runtime::framed_clean_show_changes_test_stdout("add readme", false);
    complete_patch_agent_request(
        &runtime,
        "goal-session-finish",
        &request.request_id,
        0,
        &show_changes_stdout,
        "",
    )
    .await;
    let finish = task.await.unwrap();
    assert!(finish.success, "{:?}", finish.error);

    let goal = runtime.get_goal(Some(&auth), goal_id);
    assert!(goal.success, "{:?}", goal.output);
    assert_eq!(goal.output["goal"]["summary"]["lifecycle"], "active");
    assert_eq!(goal.output["goal"]["summary"]["revision"], 2);
    assert!(goal.output["goal"]["summary"]["terminal_at_unix_ms"].is_null());
}
