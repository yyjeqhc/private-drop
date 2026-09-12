use serde_json::{json, Value};

use webcodex_core::coding_agent::{
    CODING_AGENT_MAX_CONFIG_OPTIONS, CODING_AGENT_MAX_INSTRUCTION_BYTES,
    CODING_AGENT_TIMEOUT_MAX_SECS, CODING_AGENT_TIMEOUT_MIN_SECS,
};

pub fn coding_agent_start_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {
                "type": "string",
                "minLength": 1,
                "description": "Exact registered Project id. It resolves the Runner and fixes ACP session cwd to that Project root; cwd is not a filesystem sandbox."
            },
            "provider_id": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "description": "Logical Runner-advertised ACP provider id, for example codex. Executable, argv, environment, credentials, and provider instance ids are Runner-owned and cannot be supplied here."
            },
            "idempotency_key": {
                "type": "string",
                "minLength": 1,
                "maxLength": 256,
                "description": "Required caller-chosen replay key for this autonomous Run initiation. Reuse the same key for the same intent after an uncertain start; do not mint a replacement key to retry an uncertain prompt."
            },
            "instruction": {
                "type": "string",
                "minLength": 1,
                "maxLength": CODING_AGENT_MAX_INSTRUCTION_BYTES,
                "description": "Bounded coding-agent instruction/prompt. It is sent to the delegated ACP agent but excluded from durable Run recovery records, Workflow lifecycle evidence, audit summaries, and generic telemetry bodies."
            },
            "config": {
                "type": "object",
                "maxProperties": CODING_AGENT_MAX_CONFIG_OPTIONS,
                "additionalProperties": {
                    "anyOf": [
                        {"type": "string", "maxLength": 4096},
                        {"type": "boolean"},
                        {"type": "integer"}
                    ]
                },
                "description": "Optional explicit run-level ACP config overrides. Omission or {} sends zero set_config_option calls. Every key/value must be live-advertised and operator-allowed before prompt dispatch."
            },
            "timeout_secs": {
                "type": "integer",
                "minimum": CODING_AGENT_TIMEOUT_MIN_SECS,
                "maximum": CODING_AGENT_TIMEOUT_MAX_SECS,
                "default": 300,
                "description": "Total Run budget. Timeout requests cancellation; it is not a retry signal and may become lost/outcome_unknown if terminal correlation is unavailable."
            }
        },
        "required": ["project", "provider_id", "idempotency_key", "instruction"],
        "additionalProperties": false
    })
}

pub fn coding_agent_observe_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {
                "type": "string",
                "pattern": "^wc_agent_run_[A-Za-z0-9_.-]+$",
                "description": "Opaque CodingAgentRun id returned by coding_agent_start. Knowing the id alone grants no authority."
            },
            "after_observation_token": {
                "type": "string",
                "maxLength": 192,
                "description": "Opaque exact-Run-bound observation token returned by the previous observation. A Server restart may reset it while preserving the Run."
            },
            "wait_secs": {
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "One bounded wait for retained Run changes; values above the supported wait ceiling are accepted and clamped. Not a subscription or stream."
            }
        },
        "required": ["run_id"],
        "additionalProperties": false
    })
}

pub fn coding_agent_cancel_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {
                "type": "string",
                "pattern": "^wc_agent_run_[A-Za-z0-9_.-]+$",
                "description": "Opaque CodingAgentRun id to cancel. Cancellation never starts or retries work and must be followed by observation for authoritative terminal state."
            }
        },
        "required": ["run_id"],
        "additionalProperties": false
    })
}
