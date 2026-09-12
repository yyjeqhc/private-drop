use super::common::{schema_type, wrapped_output_schema};
use serde_json::{json, Value};

fn nullable_integer(description: &str) -> Value {
    json!({
        "anyOf": [{"type": "integer"}, {"type": "null"}],
        "description": description
    })
}

fn lifecycle_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["active", "completed", "cancelled"],
        "description": "Authoritative durable Goal lifecycle."
    })
}

fn goal_summary_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "goal_id": {"type": "string", "pattern": "^wc_goal_[0-9a-f]{32}$", "description": "Canonical durable Goal id; identity only, never authority."},
            "title": {"type": "string", "minLength": 1, "maxLength": 200, "description": "Bounded Goal title."},
            "lifecycle": lifecycle_schema(),
            "revision": {"type": "integer", "minimum": 1, "description": "Monotonic authoritative Goal revision."},
            "created_at_unix_ms": schema_type("integer", "Goal creation time."),
            "updated_at_unix_ms": schema_type("integer", "Latest durable Goal mutation time."),
            "terminal_at_unix_ms": nullable_integer("Terminal transition time, or null while active."),
            "agent_task_count": {"type": "integer", "minimum": 0, "maximum": 64, "description": "Count of explicit AgentTask correlations; correlation grants no Task or execution authority."},
            "workflow_session_count": {"type": "integer", "minimum": 0, "maximum": 64, "description": "Count of explicit Workflow Session correlations; correlation grants no Session or Project authority."}
        },
        "required": [
            "goal_id", "title", "lifecycle", "revision", "created_at_unix_ms",
            "updated_at_unix_ms", "terminal_at_unix_ms", "agent_task_count",
            "workflow_session_count"
        ]
    })
}

fn correlation_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "kind": {"type": "string", "enum": ["agent_task", "workflow_session"]},
            "reference_id": {"type": "string", "pattern": "^(wc_agent_task_[0-9a-f]{32}|wc_sess_[0-9a-f]{32})$", "maxLength": 46, "description": "Exact correlated durable identity. It is not a credential and cannot be dereferenced without that domain's normal authorization."},
            "created_at_unix_ms": schema_type("integer", "Correlation creation time.")
        },
        "required": ["kind", "reference_id", "created_at_unix_ms"]
    })
}

fn goal_detail_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summary": goal_summary_schema(),
            "objective": {"type": "string", "minLength": 1, "maxLength": 8192, "description": "Bounded authoritative high-level objective/instruction; the Store enforces the same 8192-byte UTF-8 ceiling."},
            "terminal_reason": {
                "anyOf": [
                    {"type": "string", "minLength": 1, "maxLength": 4096},
                    {"type": "null"}
                ],
                "description": "Bounded terminal reason, if one was explicitly supplied."
            },
            "correlations": {
                "type": "array",
                "maxItems": 64,
                "items": correlation_schema(),
                "description": "Bounded explicit AgentTask and Workflow Session correlations only. No target-domain authority or private target state is projected."
            }
        },
        "required": ["summary", "objective", "terminal_reason", "correlations"]
    })
}

fn goal_mutation_schema() -> Value {
    wrapped_output_schema(vec![
        ("goal", goal_detail_schema()),
        (
            "created",
            schema_type("boolean", "True only for first Goal creation."),
        ),
        (
            "replayed",
            schema_type("boolean", "True only for exact keyed replay."),
        ),
        (
            "state_changed",
            schema_type("boolean", "Whether authoritative Goal state changed."),
        ),
    ])
}

pub fn output_schema_for_tool(name: &str) -> Option<Value> {
    let schema = match name {
        "create_goal"
        | "update_goal"
        | "associate_goal_agent_task"
        | "associate_goal_workflow_session" => goal_mutation_schema(),
        "get_goal" => wrapped_output_schema(vec![("goal", goal_detail_schema())]),
        "list_goals" => wrapped_output_schema(vec![
            (
                "total_count",
                schema_type(
                    "integer",
                    "Total Goals visible to the current owner principal.",
                ),
            ),
            ("offset", schema_type("integer", "Returned page offset.")),
            (
                "next_offset",
                nullable_integer("Next page offset when truncated."),
            ),
            (
                "truncated",
                schema_type("boolean", "True when more caller-visible Goals remain."),
            ),
            (
                "goals",
                json!({
                    "type": "array",
                    "maxItems": 100,
                    "items": goal_summary_schema(),
                    "description": "Bounded Goal summaries. Objective, terminal reason, and correlation identities are omitted from list projection."
                }),
            ),
        ]),
        _ => return None,
    };
    Some(schema)
}
