use serde_json::{json, Value};

const AGENT_ID_PATTERN: &str = "^wc_dagent_[0-9a-f]{32}$";
const ENDPOINT_ID_PATTERN: &str = "^wc_endpoint_[0-9a-f]{32}$";
const CONVERSATION_ID_PATTERN: &str = "^wc_conv_[0-9a-f]{32}$";
const MESSAGE_ID_PATTERN: &str = "^wc_cmsg_[0-9a-f]{32}$";
const DELIVERY_ID_PATTERN: &str = "^wc_delivery_[0-9a-f]{32}$";
const WAKE_ID_PATTERN: &str = "^wc_wake_[0-9a-f]{32}$";
const WAKE_CONSUME_TOKEN_PATTERN: &str = "^wc_wake_consume_[0-9a-f]{32}$";
const WAKE_ATTEMPT_ID_PATTERN: &str = "^wc_wake_attempt_[0-9a-f]{32}$";
const HOST_BINDING_ID_PATTERN: &str = "^wc_host_binding_[0-9a-f]{32}$";

fn bounded_string(description: &str, max_length: usize) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": max_length,
        "description": description,
    })
}

fn canonical_id(pattern: &str, description: &str) -> Value {
    json!({
        "type": "string",
        "pattern": pattern,
        "description": description,
    })
}

fn nullable_id(pattern: &str, description: &str) -> Value {
    json!({
        "anyOf": [
            canonical_id(pattern, description),
            {"type": "null"}
        ],
        "description": description,
    })
}

fn optional_offset() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "default": 0,
        "description": "Zero-based bounded page offset."
    })
}

fn optional_limit() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "default": 50,
        "description": "Maximum records to return. Values above 100 are accepted and clamped to 100."
    })
}

fn idempotency_key() -> Value {
    bounded_string(
        "Caller-generated operation key. Exact replay under the same communication principal returns the original durable resource; reuse with changed input is rejected.",
        128,
    )
}

fn expected_controller_generation() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "description": "Exact Server-assigned current Endpoint generation. Stale generations fail closed."
    })
}

fn agent_id_array(min_items: usize, description: &str) -> Value {
    json!({
        "type": "array",
        "items": canonical_id(AGENT_ID_PATTERN, "Canonical durable Agent id."),
        "minItems": min_items,
        "maxItems": 16,
        "uniqueItems": true,
        "description": description,
    })
}

fn specialty_labels() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "string",
            "minLength": 1,
            "maxLength": 64
        },
        "maxItems": 16,
        "uniqueItems": true,
        "description": "Bounded self-description labels. Labels never grant authority."
    })
}

pub fn create_agent_identity_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "handle": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "pattern": "^[A-Za-z0-9._-]+$",
                "description": "Mutable non-unique Agent handle. It is self-description metadata, not canonical identity or authority."
            },
            "display_name": bounded_string("Mutable non-unique Agent display name.", 128),
            "description": {
                "type": "string",
                "maxLength": 2048,
                "default": "",
                "description": "Optional Agent Card description, bounded by 2048 UTF-8 bytes server-side."
            },
            "specialty_labels": specialty_labels(),
            "idempotency_key": idempotency_key()
        },
        "required": ["handle", "display_name", "idempotency_key"],
        "additionalProperties": false
    })
}

pub fn list_agent_identities_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": nullable_id(AGENT_ID_PATTERN, "Optional exact canonical Agent id owned by the current communication principal."),
            "offset": optional_offset(),
            "limit": optional_limit()
        },
        "required": [],
        "additionalProperties": false
    })
}

pub fn update_agent_identity_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": canonical_id(AGENT_ID_PATTERN, "Canonical durable Agent id to update."),
            "expected_profile_revision": {
                "type": "integer",
                "minimum": 1,
                "description": "Exact profile revision fence. A stale value is rejected without mutation."
            },
            "handle": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "pattern": "^[A-Za-z0-9._-]+$"
            },
            "display_name": bounded_string("Replacement display name.", 128),
            "description": {
                "type": "string",
                "maxLength": 2048,
                "description": "Replacement description, bounded by 2048 UTF-8 bytes server-side."
            },
            "specialty_labels": specialty_labels()
        },
        "required": ["agent_id", "expected_profile_revision"],
        "minProperties": 3,
        "additionalProperties": false
    })
}

pub fn attach_agent_endpoint_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": canonical_id(AGENT_ID_PATTERN, "Canonical durable Agent id owned by the current communication principal."),
            "host": bounded_string("Endpoint host adapter name, for example ChatGPT. This is attachment metadata, not authority.", 64),
            "client_attachment_id": {
                "type": "string",
                "maxLength": 128,
                "description": "Optional host-local attachment identifier. It is not durable Agent identity."
            },
            "idempotency_key": idempotency_key()
        },
        "required": ["agent_id", "host", "idempotency_key"],
        "additionalProperties": false
    })
}

fn exact_agent_endpoint_properties() -> Value {
    json!({
        "agent_id": canonical_id(AGENT_ID_PATTERN, "Exact durable Agent id; no current/recent Agent fallback is permitted."),
        "endpoint_id": canonical_id(ENDPOINT_ID_PATTERN, "Exact current Agent Endpoint id."),
        "expected_controller_generation": expected_controller_generation()
    })
}

fn host_binding_id() -> Value {
    canonical_id(
        HOST_BINDING_ID_PATTERN,
        "Opaque process-local MCP App View binding fence. Possession never grants communication authority.",
    )
}

pub fn present_agent_continuation_input_schema() -> Value {
    let exact = exact_agent_endpoint_properties();
    json!({
        "type": "object",
        "properties": exact,
        "required": ["agent_id", "endpoint_id", "expected_controller_generation"],
        "additionalProperties": false
    })
}

pub fn agent_continuation_bind_input_schema() -> Value {
    present_agent_continuation_input_schema()
}

pub fn agent_continuation_state_input_schema() -> Value {
    let mut properties = exact_agent_endpoint_properties();
    properties["binding_id"] = host_binding_id();
    json!({
        "type": "object",
        "properties": properties,
        "required": ["agent_id", "endpoint_id", "expected_controller_generation", "binding_id"],
        "additionalProperties": false
    })
}

pub fn agent_continuation_wake_acquire_input_schema() -> Value {
    agent_continuation_state_input_schema()
}

pub fn agent_continuation_wake_prepare_input_schema() -> Value {
    let mut properties = exact_agent_endpoint_properties();
    properties["binding_id"] = host_binding_id();
    properties["wake_id"] = canonical_id(
        WAKE_ID_PATTERN,
        "Exact durable Wake acquired by this current Host binding.",
    );
    properties["attempt_id"] = canonical_id(
        WAKE_ATTEMPT_ID_PATTERN,
        "Exact durable Wake Delivery Attempt acquired by this current Host binding.",
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": [
            "agent_id", "endpoint_id", "expected_controller_generation",
            "binding_id", "wake_id", "attempt_id"
        ],
        "additionalProperties": false
    })
}

pub fn agent_continuation_wake_finish_input_schema() -> Value {
    let mut schema = agent_continuation_wake_prepare_input_schema();
    schema["properties"]["outcome"] = json!({
        "type": "string",
        "enum": ["dispatch_accepted", "delivery_unknown"],
        "description": "Host dispatch result after the durable fence. dispatch_accepted means only that ui/message returned success; delivery_unknown is required when post-fence delivery cannot be disproved."
    });
    schema["required"] = json!([
        "agent_id",
        "endpoint_id",
        "expected_controller_generation",
        "binding_id",
        "wake_id",
        "attempt_id",
        "outcome"
    ]);
    schema
}

pub fn agent_continuation_unbind_input_schema() -> Value {
    agent_continuation_state_input_schema()
}

pub fn detach_agent_endpoint_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "endpoint_id": canonical_id(ENDPOINT_ID_PATTERN, "Canonical Agent Endpoint id attached by the current communication principal.")
        },
        "required": ["endpoint_id"],
        "additionalProperties": false
    })
}

pub fn create_conversation_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "maxLength": 200,
                "description": "Optional mutable room title."
            },
            "agent_ids": agent_id_array(1, "Owned Agent participants to add with the current Human principal."),
            "idempotency_key": idempotency_key()
        },
        "required": ["agent_ids", "idempotency_key"],
        "additionalProperties": false
    })
}

fn conversation_access_properties() -> Value {
    json!({
        "agent_id": nullable_id(AGENT_ID_PATTERN, "Optional Agent view. Must be paired with exact Endpoint fencing; omit all three fields for the current Human principal."),
        "endpoint_id": nullable_id(ENDPOINT_ID_PATTERN, "Active Endpoint proving the optional Agent view."),
        "expected_controller_generation": expected_controller_generation()
    })
}

pub fn list_conversations_input_schema() -> Value {
    let access = conversation_access_properties();
    json!({
        "type": "object",
        "properties": {
            "agent_id": access["agent_id"].clone(),
            "endpoint_id": access["endpoint_id"].clone(),
            "expected_controller_generation": access["expected_controller_generation"].clone(),
            "offset": optional_offset(),
            "limit": optional_limit()
        },
        "required": [],
        "dependentRequired": {
            "agent_id": ["endpoint_id", "expected_controller_generation"],
            "endpoint_id": ["agent_id", "expected_controller_generation"],
            "expected_controller_generation": ["agent_id", "endpoint_id"]
        },
        "additionalProperties": false
    })
}

pub fn read_conversation_input_schema() -> Value {
    let access = conversation_access_properties();
    json!({
        "type": "object",
        "properties": {
            "conversation_id": canonical_id(CONVERSATION_ID_PATTERN, "Canonical Conversation id."),
            "agent_id": access["agent_id"].clone(),
            "endpoint_id": access["endpoint_id"].clone(),
            "expected_controller_generation": access["expected_controller_generation"].clone(),
            "after_seq": {
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "Return append-only transcript messages with seq greater than this cursor."
            },
            "limit": optional_limit()
        },
        "required": ["conversation_id"],
        "dependentRequired": {
            "agent_id": ["endpoint_id", "expected_controller_generation"],
            "endpoint_id": ["agent_id", "expected_controller_generation"],
            "expected_controller_generation": ["agent_id", "endpoint_id"]
        },
        "additionalProperties": false
    })
}

pub fn post_conversation_message_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "conversation_id": canonical_id(CONVERSATION_ID_PATTERN, "Canonical Conversation id."),
            "body": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4096,
                "description": "Append-only message body, bounded by 4096 UTF-8 bytes server-side."
            },
            "author_agent_id": nullable_id(AGENT_ID_PATTERN, "Agent author provenance. Omit for the current Human principal; Agent authors require endpoint_id."),
            "endpoint_id": nullable_id(ENDPOINT_ID_PATTERN, "Active Endpoint proving an Agent-authored message. Omit for Human authors."),
            "expected_controller_generation": expected_controller_generation(),
            "recipient_agent_ids": agent_id_array(0, "Optional explicit Agent Inbox recipients. Omit to deliver to every Agent participant except the author; an explicit empty array posts to the transcript/room without Agent deliveries."),
            "reply_to": nullable_id(MESSAGE_ID_PATTERN, "Optional parent Message in the same Conversation."),
            "idempotency_key": idempotency_key(),
            "wake_reply_id": canonical_id(WAKE_ID_PATTERN, "Exact durable Wake that already crossed a Host dispatch or explicit-activation fence and provides stable resumed-turn reply replay identity. Use with reply_operation_index instead of idempotency_key."),
            "reply_operation_index": {
                "type": "integer",
                "minimum": 0,
                "maximum": 31,
                "description": "Stable per-send index within one Wake. Reuse the same index only for an exact uncertain retry; use a different index for each intentional additional Message."
            }
        },
        "required": ["conversation_id", "body"],
        "oneOf": [
            {
                "required": ["idempotency_key"],
                "not": {"anyOf": [{"required": ["wake_reply_id"]}, {"required": ["reply_operation_index"]}]}
            },
            {
                "required": ["wake_reply_id", "reply_operation_index"],
                "not": {"required": ["idempotency_key"]}
            }
        ],
        "dependentRequired": {
            "author_agent_id": ["endpoint_id", "expected_controller_generation"],
            "endpoint_id": ["author_agent_id", "expected_controller_generation"],
            "expected_controller_generation": ["author_agent_id", "endpoint_id"],
            "wake_reply_id": ["author_agent_id", "endpoint_id", "expected_controller_generation", "reply_operation_index"],
            "reply_operation_index": ["wake_reply_id"]
        },
        "additionalProperties": false
    })
}

pub fn list_agent_inbox_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": canonical_id(AGENT_ID_PATTERN, "Canonical recipient Agent id."),
            "endpoint_id": canonical_id(ENDPOINT_ID_PATTERN, "Active Endpoint proving access to this Agent Inbox."),
            "expected_controller_generation": expected_controller_generation(),
            "after_delivery_order": {
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "Return queued deliveries with durable delivery_order greater than this cursor."
            },
            "limit": optional_limit()
        },
        "required": ["agent_id", "endpoint_id", "expected_controller_generation"],
        "additionalProperties": false
    })
}

pub fn consume_agent_wake_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": canonical_id(AGENT_ID_PATTERN, "Exact target Agent named by the durable Wake Intent."),
            "endpoint_id": canonical_id(ENDPOINT_ID_PATTERN, "Exact current Endpoint bound to this continuation. Host-adapter continuations require it to remain wake-capable; explicit_activation continuations do not."),
            "expected_controller_generation": {
                "type": "integer",
                "minimum": 1,
                "description": "Server-assigned Endpoint generation carried by this exact continuation. Stale generations fail closed."
            },
            "wake_id": canonical_id(WAKE_ID_PATTERN, "Exact durable Wake Intent to consume. This is not a caller-generated retry key."),
            "consume_token": canonical_id(WAKE_CONSUME_TOKEN_PATTERN, "Opaque exact-continuation token delivered by the Host adapter. It is bound to wake_id, target Agent, Endpoint, and generation; never substitute a new token or retry key.")
        },
        "required": [
            "agent_id", "endpoint_id", "expected_controller_generation",
            "wake_id", "consume_token"
        ],
        "additionalProperties": false
    })
}

pub fn consume_agent_deliveries_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": canonical_id(AGENT_ID_PATTERN, "Canonical recipient Agent id."),
            "endpoint_id": canonical_id(ENDPOINT_ID_PATTERN, "Active Endpoint proving access to this Agent Inbox."),
            "expected_controller_generation": expected_controller_generation(),
            "delivery_ids": {
                "type": "array",
                "items": canonical_id(DELIVERY_ID_PATTERN, "Canonical Agent Delivery id."),
                "minItems": 1,
                "maxItems": 100,
                "uniqueItems": true,
                "description": "Deliveries to mark consumed. Repeating already-consumed ids is a safe desired-state retry."
            }
        },
        "required": ["agent_id", "endpoint_id", "expected_controller_generation", "delivery_ids"],
        "additionalProperties": false
    })
}

pub fn bootstrap_agent_conversation_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": canonical_id(AGENT_ID_PATTERN, "Exact durable Agent this active turn acts for."),
            "endpoint_id": canonical_id(ENDPOINT_ID_PATTERN, "Exact current Host Endpoint carrying this activation."),
            "expected_controller_generation": expected_controller_generation(),
            "conversation_id": canonical_id(CONVERSATION_ID_PATTERN, "Optional explicit current Conversation. When omitted, an exact Wake may select its latest Conversation; no hidden Host selection is inferred."),
            "wake_id": canonical_id(WAKE_ID_PATTERN, "Optional exact Wake identity from a continuation envelope or explicit pending-work activation."),
            "activation_idempotency_key": idempotency_key()
        },
        "required": ["agent_id", "endpoint_id", "expected_controller_generation"],
        "additionalProperties": false
    })
}
