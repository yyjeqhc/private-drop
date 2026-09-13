use super::common::{array_schema, schema_type, wrapped_output_schema};
use serde_json::{json, Value};

fn nullable_string(description: &str) -> Value {
    json!({
        "anyOf": [{"type": "string"}, {"type": "null"}],
        "description": description
    })
}

fn nullable_integer(description: &str) -> Value {
    json!({
        "anyOf": [{"type": "integer"}, {"type": "null"}],
        "description": description
    })
}

fn attempt_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "attempt_id": schema_type("string", "Canonical durable AgentTaskAttempt id."),
            "task_id": schema_type("string", "Owning durable AgentTask id."),
            "attempt_number": schema_type("integer", "Monotonic Attempt number within one AgentTask."),
            "assignee_agent_id": schema_type("string", "Agent explicitly assigned when this Attempt started."),
            "state": {"type": "string", "enum": ["active", "expired", "succeeded", "failed"]},
            "lease_expires_at_unix_ms": schema_type("integer", "Server-authoritative lease expiry in Unix milliseconds."),
            "lease_active": schema_type("boolean", "True only while this exact latest Attempt lease remains valid at observation time."),
            "attempt_controller_generation": schema_type("integer", "Attempt-local current controller generation; not a credential or Agent Endpoint generation."),
            "created_at_unix_ms": schema_type("integer", "Attempt creation time."),
            "started_at_unix_ms": schema_type("integer", "Attempt ownership start time."),
            "terminal_at_unix_ms": nullable_integer("Terminal or materialized-expiry time, if any."),
            "terminal_result": nullable_string("Bounded terminal result metadata, if completed."),
            "terminal_reason": nullable_string("Bounded terminal reason metadata, if completed.")
        },
        "required": [
            "attempt_id", "task_id", "attempt_number", "assignee_agent_id", "state",
            "lease_expires_at_unix_ms", "lease_active", "attempt_controller_generation",
            "created_at_unix_ms", "started_at_unix_ms", "terminal_at_unix_ms",
            "terminal_result", "terminal_reason"
        ]
    })
}

fn nullable_attempt_schema() -> Value {
    json!({
        "anyOf": [attempt_schema(), {"type": "null"}],
        "description": "Latest durable Attempt, if one has ever started. Generic read/list never returns attempt_fence."
    })
}

fn task_summary_properties() -> serde_json::Map<String, Value> {
    json!({
        "properties": {
            "task_id": schema_type("string", "Canonical durable AgentTask id."),
            "assignee_agent_id": nullable_string("Explicit current assignee, or null while unassigned."),
            "title": schema_type("string", "Bounded AgentTask title."),
            "source_conversation_id": nullable_string("Optional Conversation correlation only."),
            "source_message_id": nullable_string("Optional exact Message correlation only."),
            "referenced_project_id": nullable_string("Optional Project correlation only; never authority."),
            "state": {"type": "string", "enum": ["ready", "active", "succeeded", "failed"]},
            "created_at_unix_ms": schema_type("integer", "Task creation time."),
            "updated_at_unix_ms": schema_type("integer", "Latest durable Task/Attempt ownership update time."),
            "terminal_at_unix_ms": nullable_integer("Task terminal time, or null while nonterminal."),
            "latest_attempt": nullable_attempt_schema()
            ,"execution_bound": schema_type("boolean", "True when the latest AgentTaskAttempt has a durable concrete execution backend binding (CodingAgentRun or Agent Endpoint continuation). This high-level projection grants no execution authority."),
            "execution_kind": {
                "anyOf": [
                    {"type": "string", "enum": ["coding_agent_run", "agent_endpoint"]},
                    {"type": "null"}
                ],
                "description": "Concrete durable execution backend selected for the latest AgentTaskAttempt, or null before backend selection. This is observation only and grants no authority."
            },
            "execution_status": {
                "anyOf": [
                    {"type": "string", "enum": ["not_started", "active", "waiting_permission", "outcome_unknown", "terminal"]},
                    {"type": "null"}
                ],
                "description": "High-level bound execution status only; generic Task reads expose neither CodingAgentRun backend identity nor Agent Endpoint carrier identity and grant no execution authority."
            },
            "recovery_kind": {
                "type": "string",
                "enum": ["none", "observe", "reconcile"],
                "description": "High-level recovery action for the latest bound execution. Agent Endpoint continuation recovery remains owned by the Wake/continuation substrate and therefore reports none here."
            }
        }
    })["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

fn task_summary_schema() -> Value {
    let properties = task_summary_properties();
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": [
            "task_id", "assignee_agent_id", "title", "source_conversation_id",
            "source_message_id", "referenced_project_id", "state", "created_at_unix_ms",
            "updated_at_unix_ms", "terminal_at_unix_ms", "latest_attempt",
            "execution_bound", "execution_kind", "execution_status", "recovery_kind"
        ]
    })
}

fn task_detail_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summary": task_summary_schema(),
            "instruction": schema_type("string", "Bounded durable Task instruction. It is independent from any source Conversation Message body.")
        },
        "required": ["summary", "instruction"]
    })
}

fn task_mutation_schema() -> Value {
    wrapped_output_schema(vec![
        ("task", task_detail_schema()),
        (
            "created",
            schema_type("boolean", "True only for first AgentTask creation."),
        ),
        (
            "replayed",
            schema_type("boolean", "True only for exact keyed replay."),
        ),
        (
            "state_changed",
            schema_type("boolean", "Whether durable AgentTask state changed."),
        ),
    ])
}

fn endpoint_execution_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "task_id": schema_type("string", "Owning durable AgentTask id."),
            "attempt_id": schema_type("string", "Exact durable AgentTaskAttempt id."),
            "wake_id": schema_type("string", "Durable Task-origin Wake used by the Agent Endpoint continuation backend."),
            "wake_state": {"type": "string", "enum": ["pending", "claimed", "prepared", "delivered", "delivery_unknown", "consumed", "retired"]},
            "endpoint_id": nullable_string("Current claimed continuation Endpoint carrier, or null while no Host carrier has claimed this Task execution."),
            "endpoint_controller_generation": nullable_integer("Exact Endpoint generation of the current claimed carrier, or null before claim / after a pre-dispatch release."),
            "created_at_unix_ms": schema_type("integer", "Endpoint backend selection time."),
            "updated_at_unix_ms": schema_type("integer", "Latest durable backend carrier update time.")
        },
        "required": [
            "task_id", "attempt_id", "wake_id", "wake_state", "endpoint_id",
            "endpoint_controller_generation", "created_at_unix_ms", "updated_at_unix_ms"
        ]
    })
}

fn coding_run_terminal_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "stop_reason": nullable_string("Bounded authoritative CodingAgent terminal stop reason when available."),
            "error_code": nullable_string("Bounded authoritative CodingAgent terminal error code when available."),
            "message": nullable_string("Bounded CodingAgent terminal diagnostic; never a transcript or full event history."),
            "completed_at": nullable_integer("Unix terminal timestamp when authoritative terminal evidence exists.")
        },
        "required": ["stop_reason", "error_code", "message", "completed_at"]
    })
}

fn coding_run_binding_output_schema() -> Value {
    wrapped_output_schema(vec![
        ("task_id", schema_type("string", "Owning durable AgentTask id.")),
        ("attempt_id", schema_type("string", "Exact durable AgentTaskAttempt id.")),
        ("run_id", schema_type("string", "Exact durable CodingAgentRun id for execution-authorized follow-up observation/cancellation.")),
        ("project", schema_type("string", "Exact execution-authorized runtime Project id bound to this Attempt.")),
        ("provider_id", schema_type("string", "Logical CodingAgent provider id. Provider instance identity remains private.")),
        ("dispatch_state", {json!({"type":"string","enum":["prepared","not_started","outcome_unknown","bound","terminal"]})}),
        ("run_state", json!({"anyOf":[{"type":"string","enum":["starting","running","waiting_permission","completed","failed","cancelled","lost"]},{"type":"null"}]})),
        ("execution_state", json!({"anyOf":[{"type":"string","enum":["not_started","started","outcome_unknown","completed"]},{"type":"null"}]})),
        ("execution_status", {json!({"type":"string","enum":["not_started","active","waiting_permission","outcome_unknown","terminal"]})}),
        ("execution_recovery", {json!({"type":"string","enum":["none","observe","reconcile"]})}),
        ("terminal", coding_run_terminal_schema()),
        ("task_state", {json!({"type":"string","enum":["ready","active","succeeded","failed"]})}),
        ("attempt_state", {json!({"type":"string","enum":["active","expired","succeeded","failed"]})}),
        ("replayed", schema_type("boolean", "True when the durable Attempt binding was an exact intent replay.")),
        ("state_changed", schema_type("boolean", "Whether this call changed durable Task terminal truth.")),
        ("error_kind", schema_type("string", "Bounded failure classification when the operation is unsuccessful.")),
    ])
}

pub fn output_schema_for_tool(name: &str) -> Option<Value> {
    let schema = match name {
        "create_agent_task" | "assign_agent_task" => task_mutation_schema(),
        "list_agent_tasks" => wrapped_output_schema(vec![
            ("total_count", schema_type("integer", "Total AgentTasks visible to the current owner principal.")),
            ("offset", schema_type("integer", "Returned page offset.")),
            ("next_offset", nullable_integer("Next page offset when truncated.")),
            ("truncated", schema_type("boolean", "True when more AgentTasks remain.")),
            ("tasks", array_schema(task_summary_schema(), "Bounded AgentTask summaries; no instruction bodies or attempt fences.")),
        ]),
        "read_agent_task" => wrapped_output_schema(vec![("task", task_detail_schema())]),
        "start_agent_task_attempt" => wrapped_output_schema(vec![
            ("task", task_summary_schema()),
            ("attempt", attempt_schema()),
            ("attempt_fence", schema_type("string", "Opaque exact-Attempt freshness fence required for heartbeat/completion. It is returned only by exact start/replay, not generic list/read.")),
            ("replayed", schema_type("boolean", "True for exact keyed Attempt-start replay.")),
            ("state_changed", schema_type("boolean", "Whether this call first created the Attempt.")),
        ]),
        "start_agent_task_endpoint_continuation" => wrapped_output_schema(vec![
            ("execution", endpoint_execution_schema()),
            ("replayed", schema_type("boolean", "True when the exact Attempt already selected the Agent Endpoint continuation backend.")),
            ("state_changed", schema_type("boolean", "True only when this call first creates the Task-origin Wake and backend selection.")),
        ]),
        "start_agent_task_coding_run" | "reconcile_agent_task_coding_run" => {
            coding_run_binding_output_schema()
        }
        "heartbeat_agent_task_attempt" => wrapped_output_schema(vec![
            ("task", task_summary_schema()),
            ("attempt", attempt_schema()),
            ("state_changed", schema_type("boolean", "True when the exact current Attempt lease was renewed.")),
        ]),
        "complete_agent_task_attempt" => wrapped_output_schema(vec![
            ("task", task_summary_schema()),
            ("attempt", attempt_schema()),
            ("replayed", schema_type("boolean", "True for exact keyed terminal replay.")),
            ("state_changed", schema_type("boolean", "True only for the first terminal transition.")),
        ]),
        _ => return None,
    };
    Some(schema)
}
