use super::support::*;
use crate::auth::scopes::{
    COMMUNICATION_MANAGE_SCOPES, COMMUNICATION_READ_SCOPES, SCOPE_CODING_AGENT_RUN,
    SCOPE_COMMUNICATION_MANAGE, SCOPE_COMMUNICATION_READ, SCOPE_PROJECT_WRITE,
};
use crate::runner_http::RunnerRegistry;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerRegisterRequest, RunnerResultPayload, RunnerResultRequest,
};
use crate::tool_runtime::metadata::{
    ToolApprovalPolicy, ToolAuthorityPolicy, ToolEffect, ToolIdempotency, ToolRisk,
};
use crate::tool_runtime::tool_definition::{lookup_tool_definition, RunnerCapabilityRequirement};
use crate::tool_runtime::{RuntimeInfo, ToolCall, ToolRuntime};
use serde_json::json;
use std::sync::Arc;
use webcodex_core::coding_agent::{
    CodingAgentExecutionState, CodingAgentProvider, CodingAgentRequest, CodingAgentResponse,
    CodingAgentResponsePayload, CodingAgentRunInventory, CodingAgentRunSnapshot,
    CodingAgentRunState, CodingAgentTerminal,
};

fn runtime_with_db() -> (tempfile::TempDir, Arc<crate::db::Database>, ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(crate::db::Database::open(&temp.path().join("agent-tasks.db")).unwrap());
    let runtime = ToolRuntime::new_for_tests().with_communication_database(db.clone());
    (temp, db, runtime)
}

fn create_agent(runtime: &ToolRuntime, handle: &str) -> String {
    let result = runtime.create_agent_identity(
        None,
        handle.to_string(),
        format!("{handle} display"),
        None,
        Vec::new(),
        format!("create-{handle}"),
    );
    assert!(result.success, "{:?}", result.output);
    result.output["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn register_coding_agent_task_runner(
    runtime: &ToolRuntime,
    client_id: &str,
    instance_id: &str,
    owner: &str,
    project_id: &str,
    root: &std::path::Path,
    inventory: CodingAgentRunInventory,
) -> String {
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: Some(1_700_000_000),
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: Some(vec![CodingAgentProvider {
                provider_id: "codex".to_string(),
                provider_instance_id: "codex-instance-a4a".to_string(),
                name: "Codex A4a test provider".to_string(),
            }]),
            coding_agent_inventory: Some(inventory),
            client_id: client_id.to_string(),
            runner_instance_id: instance_id.to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: Some("A4a test runner".to_string()),
            owner: Some(owner.to_string()),
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(RunnerCapabilities {
                coding_agent_runs: true,
                ..Default::default()
            }),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        instance_id,
        vec![registered_project(project_id, &root.to_string_lossy())],
    )
    .await;
    crate::tool_runtime::runner_project_runtime_id(client_id, project_id)
}

fn runtime_with_agent_task_db(db: Arc<crate::db::Database>) -> ToolRuntime {
    ToolRuntime::new(
        Arc::new(RunnerRegistry::default()),
        Arc::new(RuntimeInfo::default()),
    )
    .with_communication_database(db)
}

#[test]
fn agent_task_tools_are_definition_owned_and_a4a_adds_execution_authority_explicitly() {
    for name in [
        "create_agent_task",
        "list_agent_tasks",
        "read_agent_task",
        "assign_agent_task",
        "start_agent_task_attempt",
        "heartbeat_agent_task_attempt",
        "complete_agent_task_attempt",
    ] {
        let definition = lookup_tool_definition(name).unwrap_or_else(|| panic!("missing {name}"));
        assert!(
            definition.model_spec.is_some(),
            "{name} must own model_spec"
        );
        assert_eq!(definition.category, "agent_task");
        assert!(!definition.metadata.requires_project);
        assert_eq!(
            definition.runner_capability,
            None::<RunnerCapabilityRequirement>
        );
        assert_eq!(definition.metadata.provider_id, "control");
    }

    let start_coding = lookup_tool_definition("start_agent_task_coding_run").unwrap();
    assert!(start_coding.model_spec.is_some());
    assert_eq!(start_coding.category, "agent_task");
    assert!(start_coding.metadata.requires_project);
    assert_eq!(
        start_coding.runner_capability,
        Some(RunnerCapabilityRequirement::CodingAgentRuns)
    );
    assert_eq!(start_coding.metadata.provider_id, "agent");
    assert_eq!(start_coding.metadata.effect, ToolEffect::Execute);
    assert_eq!(start_coding.metadata.risk, ToolRisk::JobRun);
    assert_eq!(start_coding.metadata.approval, ToolApprovalPolicy::Standard);
    assert_eq!(
        start_coding.metadata.idempotency,
        ToolIdempotency::FencedReplay
    );
    assert_eq!(
        start_coding.metadata.authority,
        ToolAuthorityPolicy::RequireAll(&[
            SCOPE_COMMUNICATION_READ,
            SCOPE_COMMUNICATION_MANAGE,
            SCOPE_CODING_AGENT_RUN,
            SCOPE_PROJECT_WRITE,
        ])
    );

    let reconcile = lookup_tool_definition("reconcile_agent_task_coding_run").unwrap();
    assert!(reconcile.model_spec.is_some());
    assert_eq!(reconcile.category, "agent_task");
    assert!(!reconcile.metadata.requires_project);
    assert_eq!(
        reconcile.runner_capability,
        None::<RunnerCapabilityRequirement>
    );
    assert_eq!(reconcile.metadata.provider_id, "agent");
    assert_eq!(reconcile.metadata.effect, ToolEffect::Mutate);
    assert_eq!(reconcile.metadata.risk, ToolRisk::WorkflowManage);
    assert_eq!(reconcile.metadata.approval, ToolApprovalPolicy::None);
    assert_eq!(
        reconcile.metadata.idempotency,
        ToolIdempotency::DesiredState
    );
    assert_eq!(
        reconcile.metadata.authority,
        ToolAuthorityPolicy::RequireAll(&[
            SCOPE_COMMUNICATION_READ,
            SCOPE_COMMUNICATION_MANAGE,
            SCOPE_CODING_AGENT_RUN,
        ])
    );

    for name in ["list_agent_tasks", "read_agent_task"] {
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

    for name in [
        "create_agent_task",
        "assign_agent_task",
        "start_agent_task_attempt",
        "heartbeat_agent_task_attempt",
        "complete_agent_task_attempt",
    ] {
        let definition = lookup_tool_definition(name).unwrap();
        assert_eq!(definition.metadata.effect, ToolEffect::Mutate);
        assert_eq!(definition.metadata.risk, ToolRisk::WorkflowManage);
        assert_eq!(definition.metadata.approval, ToolApprovalPolicy::Standard);
        assert_eq!(
            definition.metadata.authority,
            ToolAuthorityPolicy::RequireAll(COMMUNICATION_MANAGE_SCOPES)
        );
    }
    assert_eq!(
        lookup_tool_definition("create_agent_task")
            .unwrap()
            .metadata
            .idempotency,
        ToolIdempotency::Keyed
    );
    assert_eq!(
        lookup_tool_definition("assign_agent_task")
            .unwrap()
            .metadata
            .idempotency,
        ToolIdempotency::DesiredState
    );
    assert_eq!(
        lookup_tool_definition("start_agent_task_attempt")
            .unwrap()
            .metadata
            .idempotency,
        ToolIdempotency::Keyed
    );
    assert_eq!(
        lookup_tool_definition("heartbeat_agent_task_attempt")
            .unwrap()
            .metadata
            .idempotency,
        ToolIdempotency::NonIdempotent
    );
    assert_eq!(
        lookup_tool_definition("complete_agent_task_attempt")
            .unwrap()
            .metadata
            .idempotency,
        ToolIdempotency::Keyed
    );
}

#[test]
fn agent_task_output_schemas_publish_bounded_task_and_attempt_contracts() {
    let specs = crate::tool_runtime::registered_tool_specs();
    let spec = |name: &str| {
        specs
            .iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("missing {name}"))
    };

    let list = spec("list_agent_tasks");
    let task_summary = &list.output_schema["properties"]["output"]["properties"]["tasks"]["items"];
    for field in [
        "task_id",
        "assignee_agent_id",
        "title",
        "source_conversation_id",
        "source_message_id",
        "referenced_project_id",
        "state",
        "latest_attempt",
        "execution_bound",
        "execution_status",
        "recovery_kind",
    ] {
        assert!(
            task_summary["properties"].get(field).is_some(),
            "list_agent_tasks must publish task summary field {field}"
        );
    }
    assert!(task_summary["properties"].get("instruction").is_none());
    for forbidden in [
        "run_id",
        "provider_id",
        "provider_instance_id",
        "authority_fingerprint",
        "binding_intent_fingerprint",
        "attempt_fence",
    ] {
        assert!(
            task_summary["properties"].get(forbidden).is_none(),
            "generic communication read must not publish private CodingAgent binding field {forbidden}"
        );
    }
    assert!(
        task_summary["properties"]["latest_attempt"]["anyOf"][0]["properties"]
            .get("attempt_fence")
            .is_none(),
        "generic Task summaries must never publish attempt_fence"
    );

    let read = spec("read_agent_task");
    let detail = &read.output_schema["properties"]["output"]["properties"]["task"];
    assert!(detail["properties"].get("instruction").is_some());
    assert!(detail["properties"]["summary"]["properties"]
        .get("task_id")
        .is_some());

    let start = spec("start_agent_task_attempt");
    let start_output = &start.output_schema["properties"]["output"]["properties"];
    assert!(start_output.get("attempt_fence").is_some());
    assert!(start_output["attempt"]["properties"]
        .get("attempt_id")
        .is_some());
    assert!(start_output["attempt"]["properties"]
        .get("attempt_fence")
        .is_none());

    for name in [
        "heartbeat_agent_task_attempt",
        "complete_agent_task_attempt",
    ] {
        let output = &spec(name).output_schema["properties"]["output"]["properties"];
        assert!(output["task"]["properties"].get("task_id").is_some());
        assert!(output["attempt"]["properties"].get("attempt_id").is_some());
        assert!(output.get("attempt_fence").is_none());
    }

    let coding_start = spec("start_agent_task_coding_run");
    let coding_start_input = coding_start.input_schema["properties"].as_object().unwrap();
    for required in [
        "project",
        "task_id",
        "attempt_id",
        "assignee_agent_id",
        "attempt_fence",
        "attempt_controller_generation",
        "provider_id",
    ] {
        assert!(coding_start_input.contains_key(required));
    }
    for forbidden in [
        "instruction",
        "idempotency_key",
        "run_id",
        "client_id",
        "provider_instance_id",
        "execution_kind",
    ] {
        assert!(!coding_start_input.contains_key(forbidden));
    }
    let coding_start_output = &coding_start.output_schema["properties"]["output"]["properties"];
    assert!(coding_start_output.get("run_id").is_some());
    assert!(coding_start_output.get("provider_instance_id").is_none());
    assert!(coding_start_output.get("authority_fingerprint").is_none());
    assert!(coding_start_output
        .get("binding_intent_fingerprint")
        .is_none());
    assert!(coding_start_output.get("attempt_fence").is_none());

    let reconcile = spec("reconcile_agent_task_coding_run");
    let reconcile_input = reconcile.input_schema["properties"].as_object().unwrap();
    assert_eq!(reconcile_input.len(), 2);
    assert!(reconcile_input.contains_key("task_id"));
    assert!(reconcile_input.contains_key("attempt_id"));
    assert!(!reconcile_input.contains_key("attempt_fence"));
    assert!(!reconcile_input.contains_key("project"));

    for name in ["start_agent_task_attempt", "heartbeat_agent_task_attempt"] {
        let properties = spec(name).input_schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} input properties"));
        for forbidden in [
            "lease_expires_at",
            "lease_expires_at_unix_ms",
            "lease_duration_ms",
            "lease_ms",
        ] {
            assert!(
                !properties.contains_key(forbidden),
                "{name} must keep Attempt lease timing Server-authoritative; found {forbidden}"
            );
        }
    }
}

#[test]
fn tool_call_parser_keeps_agent_task_and_connector_task_identities_distinct() {
    let call = ToolCall::from_tool_name(
        "create_agent_task",
        json!({
            "title": "Explicit durable work",
            "instruction": "No Connector Task or Workflow Session identity is inferred.",
            "referenced_project_id": "agent:special:correlation-only",
            "idempotency_key": "parser-task"
        }),
    )
    .unwrap();
    assert_eq!(call.tool_name(), "create_agent_task");

    let task_id = format!("wc_agent_task_{}", "1".repeat(32));
    let attempt_id = format!("wc_agent_task_attempt_{}", "2".repeat(32));
    let fence = format!("wc_agent_task_fence_{}", "3".repeat(32));
    let assignee = format!("wc_dagent_{}", "4".repeat(32));
    let heartbeat = ToolCall::from_tool_name(
        "heartbeat_agent_task_attempt",
        json!({
            "task_id": task_id,
            "attempt_id": attempt_id,
            "assignee_agent_id": assignee,
            "attempt_fence": fence,
            "attempt_controller_generation": 1
        }),
    )
    .unwrap();
    assert_eq!(heartbeat.tool_name(), "heartbeat_agent_task_attempt");

    let coding_start = ToolCall::from_tool_name(
        "start_agent_task_coding_run",
        json!({
            "project": "agent:special:task-project",
            "task_id": task_id,
            "attempt_id": attempt_id,
            "assignee_agent_id": assignee,
            "attempt_fence": fence,
            "attempt_controller_generation": 1,
            "provider_id": "codex"
        }),
    )
    .unwrap();
    assert_eq!(coding_start.tool_name(), "start_agent_task_coding_run");
    let reconcile = ToolCall::from_tool_name(
        "reconcile_agent_task_coding_run",
        json!({"task_id": task_id, "attempt_id": attempt_id}),
    )
    .unwrap();
    assert_eq!(reconcile.tool_name(), "reconcile_agent_task_coding_run");

    assert!(ToolCall::from_tool_name(
        "start_agent_task_attempt",
        json!({
            "task_id": "wc_task_connector_identity",
            "assignee_agent_id": format!("wc_dagent_{}", "5".repeat(32)),
            "idempotency_key": "wrong-domain"
        })
    )
    .is_ok(), "ToolCall serde validates shape while canonical id format is enforced by the AgentTask domain at runtime, never remapped to Connector Task");
}

#[test]
fn runtime_surface_exposes_fence_only_for_exact_start_and_never_requires_endpoint() {
    let (_temp, _db, runtime) = runtime_with_db();
    let assignee = create_agent(&runtime, "task-owner");

    let created = runtime.create_agent_task(
        None,
        "Durable task".to_string(),
        "This work survives window and Endpoint replacement.".to_string(),
        Some(assignee.clone()),
        None,
        None,
        Some("agent:special:reference-only".to_string()),
        "runtime-task-create".to_string(),
    );
    assert!(created.success, "{:?}", created.output);
    let task_id = created.output["task"]["summary"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(created.output["task"]["summary"]["state"], "ready");
    assert_eq!(
        created.output["task"]["summary"]["referenced_project_id"],
        "agent:special:reference-only"
    );

    let listed = runtime.list_agent_tasks(None, Some(assignee.clone()), None, Some(10));
    assert!(listed.success);
    assert_eq!(listed.output["tasks"].as_array().unwrap().len(), 1);
    assert!(listed.output["tasks"][0].get("attempt_fence").is_none());
    assert!(listed.output["tasks"][0]["latest_attempt"].is_null());

    let read = runtime.read_agent_task(None, task_id.clone());
    assert!(read.success);
    assert_eq!(read.output["task"]["summary"]["task_id"], task_id);
    assert!(read.output["task"].get("attempt_fence").is_none());

    let started = runtime.start_agent_task_attempt(
        None,
        task_id.clone(),
        assignee.clone(),
        "runtime-attempt-start".to_string(),
    );
    assert!(started.success, "{:?}", started.output);
    let attempt_id = started.output["attempt"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_string();
    let fence = started.output["attempt_fence"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(fence.starts_with("wc_agent_task_fence_"));
    assert_eq!(
        started.output["attempt"]["attempt_controller_generation"],
        1
    );
    assert!(started.output["attempt"].get("attempt_fence").is_none());

    let endpoint_execution = runtime.start_agent_task_endpoint_continuation(
        None,
        task_id.clone(),
        attempt_id.clone(),
        assignee.clone(),
        fence.clone(),
        1,
    );
    assert!(
        endpoint_execution.success,
        "{:?}",
        endpoint_execution.output
    );
    assert_eq!(
        endpoint_execution.output["execution"]["wake_state"],
        "pending"
    );
    assert!(endpoint_execution.output["execution"]["endpoint_id"].is_null());
    assert!(endpoint_execution.output["execution"]["endpoint_controller_generation"].is_null());
    assert_eq!(endpoint_execution.output["replayed"], false);
    assert_eq!(endpoint_execution.output["state_changed"], true);
    let wake_id = endpoint_execution.output["execution"]["wake_id"]
        .as_str()
        .unwrap()
        .to_string();

    let endpoint_replay = runtime.start_agent_task_endpoint_continuation(
        None,
        task_id.clone(),
        attempt_id.clone(),
        assignee.clone(),
        fence.clone(),
        1,
    );
    assert!(endpoint_replay.success, "{:?}", endpoint_replay.output);
    assert_eq!(endpoint_replay.output["execution"]["wake_id"], wake_id);
    assert_eq!(endpoint_replay.output["replayed"], true);
    assert_eq!(endpoint_replay.output["state_changed"], false);

    let endpoint_bound_read = runtime.read_agent_task(None, task_id.clone());
    assert!(endpoint_bound_read.success);
    assert_eq!(
        endpoint_bound_read.output["task"]["summary"]["execution_bound"],
        true
    );
    assert_eq!(
        endpoint_bound_read.output["task"]["summary"]["execution_status"],
        "not_started"
    );
    assert_eq!(
        endpoint_bound_read.output["task"]["summary"]["recovery_kind"],
        "none"
    );
    assert_eq!(
        _db.conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_task_coding_runs",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        0,
        "A3 start_agent_task_attempt must remain ownership-only and dispatch no backend"
    );

    let heartbeat = runtime.heartbeat_agent_task_attempt(
        None,
        task_id.clone(),
        attempt_id.clone(),
        assignee.clone(),
        fence.clone(),
        1,
    );
    assert!(heartbeat.success, "{:?}", heartbeat.output);
    assert_eq!(heartbeat.output["attempt"]["attempt_id"], attempt_id);

    let completed = runtime.complete_agent_task_attempt(
        None,
        task_id.clone(),
        attempt_id,
        assignee,
        fence,
        1,
        "succeeded".to_string(),
        Some("bounded terminal result".to_string()),
        None,
        "runtime-completion".to_string(),
    );
    assert!(completed.success, "{:?}", completed.output);
    assert_eq!(completed.output["task"]["state"], "succeeded");
    assert_eq!(completed.output["attempt"]["state"], "succeeded");

    let terminal = runtime.read_agent_task(None, task_id);
    assert!(terminal.success);
    assert_eq!(terminal.output["task"]["summary"]["state"], "succeeded");
    assert_eq!(
        terminal.output["task"]["summary"]["execution_status"],
        "terminal"
    );
    assert!(terminal.output["task"].get("attempt_fence").is_none());
}

#[tokio::test]
async fn coding_run_executes_then_reconciles_from_reopened_db_and_fresh_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("agent-task-a4a-runtime.db");
    let db = Arc::new(crate::db::Database::open(&db_path).unwrap());
    let runtime = runtime_with_agent_task_db(db.clone());
    let client_id = "a4a-task-runner";
    let instance_id = "a4a-task-runner-instance";
    let project_id = "a4a-task-project";
    let auth = auth_context(Some("a4a-owner"), false);
    let project = register_coding_agent_task_runner(
        &runtime,
        client_id,
        instance_id,
        "a4a-owner",
        project_id,
        temp.path(),
        CodingAgentRunInventory::default(),
    )
    .await;
    let agent_result = runtime.create_agent_identity(
        Some(&auth),
        "a4a-runtime-agent".to_string(),
        "A4a Runtime Agent".to_string(),
        None,
        Vec::new(),
        "a4a-runtime-agent-create".to_string(),
    );
    assert!(agent_result.success, "{:?}", agent_result.output);
    let assignee = agent_result.output["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    let instruction = "Implement the exact durable A4a runtime test instruction.";
    let created = runtime.create_agent_task(
        Some(&auth),
        "A4a runtime task".to_string(),
        instruction.to_string(),
        Some(assignee.clone()),
        None,
        None,
        Some(project.clone()),
        "a4a-runtime-task-create".to_string(),
    );
    assert!(created.success, "{:?}", created.output);
    let task_id = created.output["task"]["summary"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let started = runtime.start_agent_task_attempt(
        Some(&auth),
        task_id.clone(),
        assignee.clone(),
        "a4a-runtime-attempt".to_string(),
    );
    assert!(started.success, "{:?}", started.output);
    let attempt_id = started.output["attempt"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_string();
    let fence = started.output["attempt_fence"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        probe_agent_request_for_instance(&runtime, client_id, instance_id)
            .await
            .is_none(),
        "start_agent_task_attempt must not enqueue CodingAgent work"
    );

    let mut start_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let task_id = task_id.clone();
        let attempt_id = attempt_id.clone();
        let assignee = assignee.clone();
        let fence = fence.clone();
        let auth = auth.clone();
        async move {
            runtime
                .start_agent_task_coding_run(
                    Some(&auth),
                    project,
                    task_id,
                    attempt_id,
                    assignee,
                    fence,
                    1,
                    "codex".to_string(),
                    None,
                    Some(300),
                )
                .await
        }
    });
    let request = tokio::select! {
        result = &mut start_task => {
            let result = result.unwrap();
            panic!("A4a start returned before Runner dispatch: success={} output={:?} error={:?}", result.success, result.output, result.error);
        }
        request = wait_for_runner_request_for_instance(&runtime, client_id, instance_id) => request,
    };
    let start = match request
        .coding_agent
        .as_ref()
        .expect("typed CodingAgent request")
    {
        CodingAgentRequest::Start(start) => start.clone(),
        other => panic!("expected CodingAgent Start, got {other:?}"),
    };
    assert_eq!(start.runtime_project_id, project);
    assert_eq!(start.provider_id, "codex");
    assert_eq!(start.provider_instance_id, "codex-instance-a4a");
    assert_eq!(start.instruction, instruction);
    assert!(start.run_id.starts_with("wc_agent_run_"));
    let run_now = chrono::Utc::now().timestamp();
    let running = CodingAgentRunSnapshot {
        run_id: start.run_id.clone(),
        intent_fingerprint: start.intent_fingerprint.clone(),
        authority_fingerprint: start.authority_fingerprint.clone(),
        runtime_project_id: start.runtime_project_id.clone(),
        provider_id: start.provider_id.clone(),
        provider_instance_id: start.provider_instance_id.clone(),
        state: CodingAgentRunState::Running,
        execution_state: CodingAgentExecutionState::Started,
        observation_revision: 1,
        created_at: run_now,
        updated_at: run_now,
        terminal: None,
    };
    runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: client_id.to_string(),
                runner_instance_id: instance_id.to_string(),
                request_id: request.request_id,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: None,
            },
            command_execution_state: None,
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: Some(CodingAgentResponse::success(
                CodingAgentResponsePayload::Start {
                    run: running.clone(),
                },
            )),
        })
        .await
        .unwrap();
    let started_run = start_task.await.unwrap();
    assert!(started_run.success, "{:?}", started_run.output);
    assert_eq!(started_run.output["run_id"], running.run_id);
    assert_eq!(started_run.output["execution_status"], "active");
    assert_eq!(
        db.conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_task_coding_runs",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        1
    );

    let completed = CodingAgentRunSnapshot {
        state: CodingAgentRunState::Completed,
        execution_state: CodingAgentExecutionState::Completed,
        observation_revision: 2,
        updated_at: run_now + 1,
        terminal: Some(CodingAgentTerminal {
            stop_reason: Some("end_turn".to_string()),
            error_code: None,
            message: Some("A4a durable run completed".to_string()),
            completed_at: run_now + 1,
        }),
        ..running
    };

    drop(runtime);
    drop(db);
    let reopened_db = Arc::new(crate::db::Database::open(&db_path).unwrap());
    let fresh_runtime = runtime_with_agent_task_db(reopened_db.clone());
    let fresh_project = register_coding_agent_task_runner(
        &fresh_runtime,
        client_id,
        instance_id,
        "a4a-owner",
        project_id,
        temp.path(),
        CodingAgentRunInventory {
            runs: vec![completed.clone()],
        },
    )
    .await;
    assert_eq!(fresh_project, project);
    let (_, inventory_run) = fresh_runtime
        .runner_registry
        .coding_agent_run_for_auth(
            Some(&crate::test_support::runner_access(&auth)),
            &completed.run_id,
        )
        .await
        .expect("fresh runtime must see the exact durable CodingAgentRun inventory entry");
    assert_eq!(inventory_run, completed);

    let reconciled = fresh_runtime
        .reconcile_agent_task_coding_run(Some(&auth), task_id.clone(), attempt_id.clone())
        .await;
    assert!(reconciled.success, "{:?}", reconciled.output);
    assert_eq!(reconciled.output["run_id"], completed.run_id);
    assert_eq!(
        reconciled.output["task_state"], "succeeded",
        "reconcile output: {:?}",
        reconciled.output
    );
    assert_eq!(reconciled.output["attempt_state"], "succeeded");
    assert_eq!(reconciled.output["state_changed"], true);

    let task = fresh_runtime.read_agent_task(Some(&auth), task_id);
    assert!(task.success, "{:?}", task.output);
    assert_eq!(task.output["task"]["summary"]["state"], "succeeded");
    assert_eq!(task.output["task"]["summary"]["execution_bound"], true);
    assert_eq!(
        task.output["task"]["summary"]["execution_status"],
        "terminal"
    );
    assert_eq!(task.output["task"]["summary"]["recovery_kind"], "none");
    assert!(
        probe_agent_request_for_instance(&fresh_runtime, client_id, instance_id)
            .await
            .is_none(),
        "restart reconciliation from authoritative inventory must not mint another Start"
    );
}

#[test]
fn agent_task_audit_projection_never_records_instruction_fence_keys_or_terminal_text() {
    const INSTRUCTION: &str = "PRIVATE_AGENT_TASK_INSTRUCTION_DO_NOT_LOG";
    const FENCE: &str = "wc_agent_task_fence_11111111111111111111111111111111";
    const START_KEY: &str = "PRIVATE_START_REPLAY_KEY_DO_NOT_LOG";
    const COMPLETION_KEY: &str = "PRIVATE_COMPLETION_KEY_DO_NOT_LOG";
    const RESULT: &str = "PRIVATE_TERMINAL_RESULT_DO_NOT_LOG";
    const REASON: &str = "PRIVATE_TERMINAL_REASON_DO_NOT_LOG";

    let create_summary = crate::tool_runtime::tool_audit::session_log_arguments_for_tool_request(
        "create_agent_task",
        &json!({
            "title": "private title",
            "instruction": INSTRUCTION,
            "assignee_agent_id": format!("wc_dagent_{}", "a".repeat(32)),
            "referenced_project_id": "agent:special:reference-only",
            "idempotency_key": START_KEY,
        }),
    );
    assert_eq!(create_summary["instruction_bytes"], INSTRUCTION.len());
    assert_eq!(
        create_summary["title_chars"],
        "private title".chars().count()
    );
    assert_eq!(create_summary["idempotency_key_present"], true);

    let complete_summary = crate::tool_runtime::tool_audit::session_log_arguments_for_tool_request(
        "complete_agent_task_attempt",
        &json!({
            "task_id": format!("wc_agent_task_{}", "1".repeat(32)),
            "attempt_id": format!("wc_agent_task_attempt_{}", "2".repeat(32)),
            "assignee_agent_id": format!("wc_dagent_{}", "a".repeat(32)),
            "attempt_fence": FENCE,
            "attempt_controller_generation": 3,
            "outcome": "succeeded",
            "terminal_result": RESULT,
            "terminal_reason": REASON,
            "completion_key": COMPLETION_KEY,
        }),
    );
    assert_eq!(complete_summary["attempt_fence_present"], true);
    assert_eq!(complete_summary["completion_key_present"], true);
    assert_eq!(complete_summary["terminal_result_bytes"], RESULT.len());
    assert_eq!(complete_summary["terminal_reason_bytes"], REASON.len());

    let result_summary = crate::tool_runtime::tool_audit::session_log_result_for_tool(
        "start_agent_task_attempt",
        &json!({
            "task": {
                "task_id": format!("wc_agent_task_{}", "1".repeat(32)),
                "state": "active"
            },
            "attempt": {
                "attempt_id": format!("wc_agent_task_attempt_{}", "2".repeat(32)),
                "attempt_number": 1,
                "state": "active",
                "attempt_controller_generation": 1,
                "terminal_result": RESULT,
                "terminal_reason": REASON
            },
            "attempt_fence": FENCE,
            "replayed": false,
            "state_changed": true
        }),
    );
    let serialized = serde_json::to_string(&json!({
        "create": create_summary,
        "complete": complete_summary,
        "result": result_summary,
    }))
    .unwrap();
    for private in [
        INSTRUCTION,
        FENCE,
        START_KEY,
        COMPLETION_KEY,
        RESULT,
        REASON,
        "private title",
    ] {
        assert!(!serialized.contains(private), "audit leaked {private}");
    }
}

#[test]
fn foreign_runtime_task_ids_are_existence_hidden_and_project_reference_grants_nothing() {
    let (_temp, _db, runtime) = runtime_with_db();
    let bob = auth_context(Some("bob"), false);
    let alice = auth_context(Some("alice"), false);
    let bob_agent = runtime.create_agent_identity(
        Some(&bob),
        "bob-task-agent".to_string(),
        "Bob Task Agent".to_string(),
        None,
        Vec::new(),
        "bob-task-agent-create".to_string(),
    );
    assert!(bob_agent.success);
    let bob_agent_id = bob_agent.output["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    let created = runtime.create_agent_task(
        Some(&bob),
        "Private durable task".to_string(),
        "Private task instruction".to_string(),
        Some(bob_agent_id),
        None,
        None,
        Some("agent:special:project-not-authority".to_string()),
        "bob-private-task".to_string(),
    );
    assert!(created.success);
    let task_id = created.output["task"]["summary"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let missing_id = format!("wc_agent_task_{}", "f".repeat(32));

    let foreign = runtime.read_agent_task(Some(&alice), task_id);
    let missing = runtime.read_agent_task(Some(&alice), missing_id);
    assert!(!foreign.success);
    assert!(!missing.success);
    assert_eq!(foreign.output["error_kind"], "agent_task_not_found");
    assert_eq!(foreign.output["error_kind"], missing.output["error_kind"]);
    assert_eq!(foreign.error, missing.error);

    let definition = lookup_tool_definition("create_agent_task").unwrap();
    assert!(!definition.metadata.requires_project);
    assert_eq!(definition.runner_capability, None);
    assert!(!matches!(
        definition.metadata.risk,
        ToolRisk::ProjectWrite | ToolRisk::JobRun
    ));
}
