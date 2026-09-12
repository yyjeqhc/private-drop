use serde_json::{json, Value};

const GOAL_ID_PATTERN: &str = "^wc_goal_[0-9a-f]{32}$";
const TASK_ID_PATTERN: &str = "^wc_agent_task_[0-9a-f]{32}$";
const SESSION_ID_PATTERN: &str = "^wc_sess_[0-9a-f]{32}$";

fn canonical_id(pattern: &str, description: &str) -> Value {
    json!({"type": "string", "pattern": pattern, "description": description})
}

fn goal_id() -> Value {
    canonical_id(
        GOAL_ID_PATTERN,
        "Canonical durable Goal id. It is exact identity only and is never a bearer credential or execution selector.",
    )
}

fn idempotency_key(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 128,
        "description": description
    })
}

fn lifecycle() -> Value {
    json!({
        "type": "string",
        "enum": ["active", "completed", "cancelled"],
        "description": "Closed authoritative Goal lifecycle. Execution/presentation states such as implementing, blocked, or waiting_validation are not Goal lifecycle values."
    })
}

pub fn create_goal_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "minLength": 1,
                "maxLength": 200,
                "description": "Bounded human-readable Goal title."
            },
            "objective": {
                "type": "string",
                "minLength": 1,
                "maxLength": 8192,
                "description": "Bounded authoritative high-level objective/instruction. The Server additionally enforces an 8192-byte UTF-8 bound."
            },
            "idempotency_key": idempotency_key("Caller-generated Goal creation key. Exact retry returns the same Goal; changed reuse fails closed.")
        },
        "required": ["title", "objective", "idempotency_key"],
        "additionalProperties": false
    })
}

pub fn get_goal_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"goal_id": goal_id()},
        "required": ["goal_id"],
        "additionalProperties": false
    })
}

pub fn list_goals_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "lifecycle": lifecycle(),
            "offset": {
                "type": "integer",
                "minimum": 0,
                "maximum": 9223372036854775807i64,
                "default": 0,
                "description": "Bounded SQLite-compatible page offset."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "default": 50,
                "description": "Maximum number of caller-visible Goal summaries returned."
            }
        },
        "required": [],
        "additionalProperties": false
    })
}

pub fn update_goal_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal_id": goal_id(),
            "expected_revision": {
                "type": "integer",
                "minimum": 1,
                "description": "Exact observed Goal revision. Stale mutation fails closed and returns the current revision only."
            },
            "title": {
                "type": "string",
                "minLength": 1,
                "maxLength": 200,
                "description": "Optional replacement Goal title."
            },
            "objective": {
                "type": "string",
                "minLength": 1,
                "maxLength": 8192,
                "description": "Optional replacement objective. The Server additionally enforces an 8192-byte UTF-8 bound."
            },
            "lifecycle": lifecycle(),
            "terminal_reason": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4096,
                "description": "Optional bounded terminal reason; valid only with an explicit completed or cancelled transition."
            },
            "idempotency_key": idempotency_key("Caller-generated Goal update key. Exact retry replays; changed reuse fails closed.")
        },
        "required": ["goal_id", "expected_revision", "idempotency_key"],
        "dependentRequired": {"terminal_reason": ["lifecycle"]},
        "anyOf": [
            {"required": ["title"]},
            {"required": ["objective"]},
            {"required": ["lifecycle"]},
            {"required": ["terminal_reason"]}
        ],
        "additionalProperties": false
    })
}

pub fn associate_goal_agent_task_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal_id": goal_id(),
            "task_id": canonical_id(TASK_ID_PATTERN, "Exact durable AgentTask id. Association is correlation only and never grants TaskAttempt, Project, Runner, or execution authority."),
            "idempotency_key": idempotency_key("Caller-generated Goal-to-AgentTask association key. Exact retry replays; changed reuse fails closed.")
        },
        "required": ["goal_id", "task_id", "idempotency_key"],
        "additionalProperties": false
    })
}

pub fn associate_goal_workflow_session_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal_id": goal_id(),
            "session_id": canonical_id(SESSION_ID_PATTERN, "Exact Workflow Session id. The target Session is independently re-authorized before association; the correlation never grants Session or Project authority."),
            "idempotency_key": idempotency_key("Caller-generated Goal-to-Workflow-Session association key. Exact retry replays; changed reuse fails closed.")
        },
        "required": ["goal_id", "session_id", "idempotency_key"],
        "additionalProperties": false
    })
}
