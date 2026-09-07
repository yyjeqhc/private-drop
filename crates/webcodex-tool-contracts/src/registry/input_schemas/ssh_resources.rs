use serde_json::{json, Value};
use webcodex_core::ssh_resource::{
    SSH_RESOURCE_DEFAULT_CWD_MAX_BYTES, SSH_RESOURCE_NAME_MAX_BYTES, SSH_RESOURCE_TARGET_MAX_BYTES,
};

pub fn ssh_resource_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list", "register", "remove"],
                "description": "Managed SSH resource operation. List first to obtain the exact Runner/revision binding required by register/remove."
            },
            "runner": {
                "type": "string",
                "minLength": 1,
                "maxLength": 128,
                "description": "Exact caller-visible Runner client_id. Required only for list."
            },
            "binding": {
                "type": "string",
                "pattern": "^wc_sbind_[0-9a-f]{32}$",
                "description": "Opaque exact Runner + registry revision observation returned by list. Required for register/remove; never grants authority by itself."
            },
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": SSH_RESOURCE_NAME_MAX_BYTES,
                "pattern": "^[A-Za-z0-9_.-]+$",
                "description": "Logical Runner-local SSH resource name. Required for register/remove."
            },
            "target": {
                "type": "string",
                "minLength": 1,
                "maxLength": SSH_RESOURCE_TARGET_MAX_BYTES,
                "description": "Single OpenSSH destination argv. Required only for register. It is persisted on the Runner and never echoed. Options or credential material are not accepted."
            },
            "default_cwd": {
                "type": "string",
                "minLength": 1,
                "maxLength": SSH_RESOURCE_DEFAULT_CWD_MAX_BYTES,
                "description": "Optional remote default cwd for register."
            }
        },
        "required": ["action"],
        "additionalProperties": false,
        "allOf": [
            {
                "if": {"properties": {"action": {"const": "list"}}, "required": ["action"]},
                "then": {"required": ["runner"]}
            },
            {
                "if": {"properties": {"action": {"const": "register"}}, "required": ["action"]},
                "then": {"required": ["binding", "name", "target"]}
            },
            {
                "if": {"properties": {"action": {"const": "remove"}}, "required": ["action"]},
                "then": {"required": ["binding", "name"]}
            }
        ]
    })
}
