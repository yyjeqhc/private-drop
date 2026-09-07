use serde_json::{json, Value};
use webcodex_core::ssh_resource::{MANAGED_SSH_RESOURCE_MAX_COUNT, SSH_RESOURCE_NAME_MAX_BYTES};

use super::common::{schema_type, wrapped_output_schema};

fn resource_name_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": SSH_RESOURCE_NAME_MAX_BYTES,
        "description": description
    })
}

fn inventory_entry_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": resource_name_schema("Runner-local logical SSH resource name."),
            "source": {
                "type": "string",
                "enum": ["static", "managed"],
                "description": "Whether the resource comes from static Runner config or the managed registry."
            },
            "active": schema_type("boolean", "Whether this resource is active in the current Runner process."),
            "pending_restart": schema_type("boolean", "Whether persisted state differs from the active Runner process and requires restart.")
        },
        "required": ["name", "source", "active", "pending_restart"]
    })
}

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    if name != "ssh_resource" {
        return None;
    }
    Some(wrapped_output_schema(vec![
        (
            "runner",
            schema_type("string", "Exact caller-visible Runner client_id used by a successful list observation."),
        ),
        (
            "binding",
            json!({
                "type": "string",
                "pattern": "^wc_sbind_[0-9a-f]{32}$",
                "description": "Opaque caller + exact Runner instance + registry revision observation used to fence register/remove."
            }),
        ),
        (
            "resources",
            json!({
                "type": "array",
                "maxItems": MANAGED_SSH_RESOURCE_MAX_COUNT,
                "items": inventory_entry_schema(),
                "description": "Bounded logical SSH resource inventory. Targets and authentication material are never returned."
            }),
        ),
        (
            "resource",
            resource_name_schema("Logical resource name affected by a successful register/remove operation."),
        ),
        (
            "persisted",
            schema_type("boolean", "Whether the requested managed registry state is durably persisted."),
        ),
        (
            "active",
            schema_type("boolean", "Whether the resulting resource state is active in the current Runner process."),
        ),
        (
            "restart_required",
            schema_type("boolean", "Whether Runner restart is required before persisted state becomes active."),
        ),
        (
            "error_kind",
            schema_type("string", "Bounded managed-SSH failure classification for canonical Tool Runtime calls."),
        ),
        (
            "dispatch_state",
            json!({
                "type": "string",
                "enum": ["not_started", "completed", "outcome_unknown"],
                "description": "Whether the managed SSH request definitely did not start, completed with a known result, or may have reached the Runner."
            }),
        ),
        (
            "recovery",
            schema_type("string", "Bounded recovery guidance when re-observation is required before another mutation."),
        ),
    ]))
}
