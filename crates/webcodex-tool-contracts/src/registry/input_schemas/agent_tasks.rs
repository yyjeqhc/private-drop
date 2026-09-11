use serde_json::{json, Value};
use webcodex_core::coding_agent::{
    CODING_AGENT_MAX_CONFIG_OPTIONS, CODING_AGENT_TIMEOUT_MAX_SECS, CODING_AGENT_TIMEOUT_MIN_SECS,
};

const AGENT_ID_PATTERN: &str = "^wc_dagent_[0-9a-f]{32}$";
const TASK_ID_PATTERN: &str = "^wc_agent_task_[0-9a-f]{32}$";
const ATTEMPT_ID_PATTERN: &str = "^wc_agent_task_attempt_[0-9a-f]{32}$";
const ATTEMPT_FENCE_PATTERN: &str = "^wc_agent_task_fence_[0-9a-f]{32}$";
const CONVERSATION_ID_PATTERN: &str = "^wc_conv_[0-9a-f]{32}$";
const MESSAGE_ID_PATTERN: &str = "^wc_cmsg_[0-9a-f]{32}$";

fn canonical_id(pattern: &str, description: &str) -> Value {
    json!({"type": "string", "pattern": pattern, "description": description})
}

fn nullable_id(pattern: &str, description: &str) -> Value {
    json!({
        "anyOf": [canonical_id(pattern, description), {"type": "null"}],
        "description": description
    })
}

fn idempotency_key(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 128,
        "description": description
    })
}

fn task_id() -> Value {
    canonical_id(
        TASK_ID_PATTERN,
        "Canonical durable AgentTask id. It is not a credential or Connector Task id.",
    )
}

fn assignee_agent_id() -> Value {
    canonical_id(AGENT_ID_PATTERN, "Explicit current durable Agent assignee. Agent identity does not grant Project or executor authority.")
}

fn attempt_identity_properties() -> serde_json::Map<String, Value> {
    json!({
        "properties": {
            "task_id": task_id(),
            "attempt_id": canonical_id(ATTEMPT_ID_PATTERN, "Exact durable AgentTaskAttempt id."),
            "assignee_agent_id": assignee_agent_id(),
            "attempt_fence": canonical_id(ATTEMPT_FENCE_PATTERN, "Opaque exact-Attempt freshness fence returned by start_agent_task_attempt. It is not a bearer credential or idempotency key."),
            "attempt_controller_generation": {
                "type": "integer",
                "minimum": 1,
                "description": "Exact current Attempt-local controller generation. Carrier replacement increments it without creating a new Attempt."
            }
        }
    })["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

pub fn create_agent_task_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "minLength": 1,
                "maxLength": 200,
                "description": "Bounded AgentTask title."
            },
            "instruction": {
                "type": "string",
                "minLength": 1,
                "maxLength": 8192,
                "description": "Bounded execution instruction owned by the AgentTask. Conversation Message bodies are not copied implicitly; the Server enforces an 8192-byte UTF-8 bound."
            },
            "assignee_agent_id": nullable_id(AGENT_ID_PATTERN, "Optional explicit current assignee. Omit to create an unassigned Task; an unassigned Task cannot start an Attempt."),
            "source_conversation_id": nullable_id(CONVERSATION_ID_PATTERN, "Optional authorized Conversation correlation only. Conversation participation does not grant AgentTask execution authority."),
            "source_message_id": nullable_id(MESSAGE_ID_PATTERN, "Optional exact Message correlation inside source_conversation_id. The Message does not become or control the Task."),
            "referenced_project_id": {
                "type": "string",
                "minLength": 1,
                "maxLength": 256,
                "description": "Optional intended Project correlation only. AgentTask authorization never grants Project, Runner, filesystem, Job, or CodingAgent authority."
            },
            "idempotency_key": idempotency_key("Caller-generated AgentTask creation key. Exact retry returns the same Task; changed reuse conflicts.")
        },
        "required": ["title", "instruction", "idempotency_key"],
        "dependentRequired": {"source_message_id": ["source_conversation_id"]},
        "additionalProperties": false
    })
}

pub fn list_agent_tasks_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "assignee_agent_id": nullable_id(AGENT_ID_PATTERN, "Optional assignee filter within Tasks visible to the current owner principal."),
            "offset": {"type": "integer", "minimum": 0, "default": 0},
            "limit": {"type": "integer", "minimum": 1, "default": 50, "description": "Maximum AgentTasks returned. Values above 100 are accepted and clamped to 100."}
        },
        "required": [],
        "additionalProperties": false
    })
}

pub fn read_agent_task_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"task_id": task_id()},
        "required": ["task_id"],
        "additionalProperties": false
    })
}

pub fn assign_agent_task_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": task_id(),
            "assignee_agent_id": assignee_agent_id()
        },
        "required": ["task_id", "assignee_agent_id"],
        "additionalProperties": false
    })
}

pub fn start_agent_task_attempt_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": task_id(),
            "assignee_agent_id": assignee_agent_id(),
            "idempotency_key": idempotency_key("Caller-generated Attempt-start key. Exact retry returns the same attempt_id and attempt_fence, even if that Attempt later becomes stale.")
        },
        "required": ["task_id", "assignee_agent_id", "idempotency_key"],
        "additionalProperties": false
    })
}

pub fn start_agent_task_coding_run_input_schema() -> Value {
    let mut properties = attempt_identity_properties();
    properties.insert(
        "project".to_string(),
        json!({
            "type": "string",
            "minLength": 1,
            "description": "Exact registered Project id. It must equal AgentTask.referenced_project_id, which remains correlation only; this execution call independently re-authorizes Project write and CodingAgentRun authority."
        }),
    );
    properties.insert(
        "provider_id".to_string(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "description": "Logical Runner-advertised CodingAgent provider. The Server binds its exact provider instance; callers cannot supply provider_instance_id."
        }),
    );
    properties.insert(
        "config".to_string(),
        json!({
            "type": "object",
            "maxProperties": CODING_AGENT_MAX_CONFIG_OPTIONS,
            "additionalProperties": {
                "anyOf": [
                    {"type": "string", "maxLength": 4096},
                    {"type": "boolean"},
                    {"type": "integer"}
                ]
            },
            "description": "Optional run-level CodingAgent config. It participates in the immutable Attempt binding intent; changing it after binding conflicts rather than creating a second Run."
        }),
    );
    properties.insert(
        "timeout_secs".to_string(),
        json!({
            "type": "integer",
            "minimum": CODING_AGENT_TIMEOUT_MIN_SECS,
            "maximum": CODING_AGENT_TIMEOUT_MAX_SECS,
            "default": 300,
            "description": "Total CodingAgentRun budget. It participates in immutable binding intent."
        }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": [
            "project", "task_id", "attempt_id", "assignee_agent_id", "attempt_fence",
            "attempt_controller_generation", "provider_id"
        ],
        "additionalProperties": false
    })
}

pub fn reconcile_agent_task_coding_run_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": task_id(),
            "attempt_id": canonical_id(ATTEMPT_ID_PATTERN, "Exact durable AgentTaskAttempt whose already-bound CodingAgentRun must be reconciled. No old attempt fence is required because this operation can only consume authoritative backend truth.")
        },
        "required": ["task_id", "attempt_id"],
        "additionalProperties": false
    })
}

pub fn heartbeat_agent_task_attempt_input_schema() -> Value {
    let properties = attempt_identity_properties();
    json!({
        "type": "object",
        "properties": properties,
        "required": [
            "task_id", "attempt_id", "assignee_agent_id", "attempt_fence",
            "attempt_controller_generation"
        ],
        "additionalProperties": false
    })
}

pub fn complete_agent_task_attempt_input_schema() -> Value {
    let mut properties = attempt_identity_properties();
    properties.insert(
        "outcome".to_string(),
        json!({
            "type": "string",
            "enum": ["succeeded", "failed"],
            "description": "Terminal AgentTask outcome. In A3, failed completion is terminal; only lease expiry before completion permits a later Attempt."
        }),
    );
    properties.insert(
        "terminal_result".to_string(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 4096,
            "description": "Optional bounded terminal result metadata; full Conversation bodies or execution logs do not belong here."
        }),
    );
    properties.insert(
        "terminal_reason".to_string(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 4096,
            "description": "Optional bounded terminal reason metadata."
        }),
    );
    properties.insert(
        "completion_key".to_string(),
        idempotency_key("Caller-generated terminal completion key. Same key and same intent replay exactly; changed reuse conflicts. attempt_fence is never used as this key."),
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": [
            "task_id", "attempt_id", "assignee_agent_id", "attempt_fence",
            "attempt_controller_generation", "outcome", "completion_key"
        ],
        "additionalProperties": false
    })
}
