use super::common::{array_schema, schema_type, wrapped_output_schema};
use serde_json::{json, Value};

fn nullable_integer(description: &str) -> Value {
    json!({
        "anyOf": [{"type": "integer"}, {"type": "null"}],
        "description": description
    })
}

fn nullable_string(description: &str) -> Value {
    json!({
        "anyOf": [{"type": "string"}, {"type": "null"}],
        "description": description
    })
}

fn agent_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "agent_id": schema_type("string", "Canonical durable Agent identity."),
            "handle": schema_type("string", "Mutable non-unique Agent handle."),
            "display_name": schema_type("string", "Mutable non-unique display name."),
            "description": schema_type("string", "Agent Card description."),
            "specialty_labels": array_schema(schema_type("string", "Self-description label."), "Bounded self-description labels; never authority."),
            "profile_revision": schema_type("integer", "Monotonic Agent Card revision."),
            "created_at_unix_ms": schema_type("integer", "Creation time in Unix milliseconds."),
            "updated_at_unix_ms": schema_type("integer", "Latest profile update time in Unix milliseconds."),
            "current_controller_generation": schema_type("integer", "Server-authoritative monotonic Endpoint generation. Zero means no A2 attachment has been issued yet."),
            "active_endpoint_count": schema_type("integer", "Current unexpired generation-matching Endpoint count."),
            "queued_delivery_count": schema_type("integer", "Queued Inbox deliveries for this Agent."),
            "unresolved_wake_count": schema_type("integer", "Durable Wake Intents not yet consumed."),
            "latest_wake_id": nullable_string("Most recently created durable Wake Intent, if any."),
            "latest_wake_state": nullable_string("Latest Wake state, independent from Inbox Delivery state.")
        },
        "required": [
            "agent_id", "handle", "display_name", "description", "specialty_labels",
            "profile_revision", "created_at_unix_ms", "updated_at_unix_ms",
            "current_controller_generation", "active_endpoint_count", "queued_delivery_count",
            "unresolved_wake_count", "latest_wake_id", "latest_wake_state"
        ]
    })
}

fn endpoint_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "endpoint_id": schema_type("string", "Canonical current attachment id."),
            "agent_id": schema_type("string", "Durable Agent carried by this Endpoint."),
            "host": schema_type("string", "Host adapter name."),
            "client_attachment_id": nullable_string("Optional host-local attachment id."),
            "wake_capable": schema_type("boolean", "True only when this exact Endpoint generation also has a current callable process-local continuation adapter; never execution authority."),
            "controller_generation": schema_type("integer", "Server-assigned monotonic continuation ownership generation."),
            "lifecycle": {"type": "string", "enum": ["attached", "detached", "expired"]},
            "attached_at_unix_ms": schema_type("integer", "Attachment time in Unix milliseconds."),
            "last_seen_at_unix_ms": schema_type("integer", "Latest infrastructure liveness or communication activity time."),
            "lease_expires_at_unix_ms": schema_type("integer", "Bounded Endpoint lease expiry in Unix milliseconds."),
            "expired_at_unix_ms": nullable_integer("Server-observed expiry time, or null when not expired."),
            "detached_at_unix_ms": nullable_integer("Explicit detach time, or null when not detached.")
        },
        "required": [
            "endpoint_id", "agent_id", "host", "client_attachment_id", "wake_capable",
            "controller_generation", "lifecycle", "attached_at_unix_ms", "last_seen_at_unix_ms",
            "lease_expires_at_unix_ms", "expired_at_unix_ms", "detached_at_unix_ms"
        ]
    })
}

fn agent_continuation_projection_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "version": {"type": "integer", "const": 1},
            "agent_id": schema_type("string", "Exact durable Agent identity."),
            "display_name": schema_type("string", "Safe Agent display name only; private description and specialty labels are omitted."),
            "endpoint_id": schema_type("string", "Exact current Endpoint identity."),
            "controller_generation": schema_type("integer", "Exact Endpoint controller generation."),
            "endpoint_lease_expires_at_unix_ms": schema_type("integer", "Current bounded Endpoint lease expiry."),
            "host_binding": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "bound": schema_type("boolean", "Whether this exact Endpoint generation has a current process-local Host carrier."),
                    "adapter_kind": nullable_string("Safe Host carrier kind, if present."),
                    "production_auto_resume_available": schema_type("boolean", "Whether the current Host carrier has a demonstrated production new-turn primitive.")
                },
                "required": ["bound", "adapter_kind", "production_auto_resume_available"]
            },
            "wake": {
                "anyOf": [
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "wake_id": schema_type("string", "Exact unresolved durable Wake identity."),
                            "state": {"type": "string", "enum": ["pending", "claimed", "prepared", "delivered", "delivery_unknown"]},
                            "revision": schema_type("integer", "Current durable Wake revision.")
                        },
                        "required": ["wake_id", "state", "revision"]
                    },
                    {"type": "null"}
                ]
            },
            "queued_delivery_count": schema_type("integer", "Current authoritative queued Inbox count; no Message bodies are included."),
            "dispatch_observation": {
                "anyOf": [
                    {"type": "string", "enum": ["dispatch_prepared", "dispatch_accepted", "dispatch_unknown", "continuation_consumed"]},
                    {"type": "null"}
                ],
                "description": "Process-local Host observation only; only continuation_consumed proves a later turn exact-consumed the durable Wake."
            }
        },
        "required": [
            "version", "agent_id", "display_name", "endpoint_id", "controller_generation",
            "endpoint_lease_expires_at_unix_ms", "host_binding", "wake",
            "queued_delivery_count", "dispatch_observation"
        ]
    })
}

fn participant_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "participant_id": schema_type("string", "Canonical Conversation participant record id."),
            "participant_kind": {"type": "string", "enum": ["human", "agent"]},
            "agent_id": nullable_string("Canonical Agent id for Agent participants."),
            "handle": nullable_string("Current Agent handle projection."),
            "display_name": nullable_string("Current Agent display-name projection."),
            "principal_kind": nullable_string("Bounded credential class for Human provenance; no secret identity is returned."),
            "joined_at_unix_ms": schema_type("integer", "Join time in Unix milliseconds.")
        },
        "required": [
            "participant_id", "participant_kind", "agent_id", "handle",
            "display_name", "principal_kind", "joined_at_unix_ms"
        ]
    })
}

fn author_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "participant_kind": {"type": "string", "enum": ["human", "agent"]},
            "agent_id": nullable_string("Canonical Agent author id when participant_kind is agent."),
            "handle": nullable_string("Current Agent handle projection."),
            "display_name": nullable_string("Current Agent display-name projection."),
            "principal_kind": nullable_string("Bounded Human credential class; no credential digest is returned.")
        },
        "required": ["participant_kind", "agent_id", "handle", "display_name", "principal_kind"]
    })
}

fn delivery_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "delivery_order": schema_type("integer", "Durable monotonic Inbox ordering cursor."),
            "delivery_id": schema_type("string", "Canonical recipient-specific Delivery id."),
            "recipient_agent_id": schema_type("string", "Recipient Agent identity."),
            "state": {"type": "string", "enum": ["queued", "consumed"]},
            "created_at_unix_ms": schema_type("integer", "Delivery creation time in Unix milliseconds."),
            "consumed_at_unix_ms": nullable_integer("Consumption time, or null while queued.")
        },
        "required": [
            "delivery_order", "delivery_id", "recipient_agent_id", "state",
            "created_at_unix_ms", "consumed_at_unix_ms"
        ]
    })
}

fn message_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "message_id": schema_type("string", "Canonical append-only Conversation Message id."),
            "conversation_id": schema_type("string", "Owning Conversation id."),
            "seq": schema_type("integer", "Stable monotonic sequence within the Conversation."),
            "author": author_schema(),
            "body": schema_type("string", "Message body."),
            "reply_to": nullable_string("Optional parent Message in the same Conversation."),
            "created_at_unix_ms": schema_type("integer", "Creation time in Unix milliseconds."),
            "deliveries": array_schema(delivery_schema(), "Recipient-specific delivery state, distinct from the Message transcript record.")
        },
        "required": [
            "message_id", "conversation_id", "seq", "author", "body",
            "reply_to", "created_at_unix_ms", "deliveries"
        ]
    })
}

fn conversation_summary_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "conversation_id": schema_type("string", "Canonical durable Conversation id."),
            "title": nullable_string("Optional Conversation title."),
            "lifecycle": {"type": "string", "enum": ["open", "closed"]},
            "created_at_unix_ms": schema_type("integer", "Creation time in Unix milliseconds."),
            "updated_at_unix_ms": schema_type("integer", "Latest transcript update time in Unix milliseconds."),
            "participant_count": schema_type("integer", "Current participant count."),
            "message_count": schema_type("integer", "Append-only transcript message count."),
            "last_seq": schema_type("integer", "Latest stable transcript sequence."),
            "queued_delivery_count": nullable_integer("Queued deliveries for an Agent view; null for a Human view.")
        },
        "required": [
            "conversation_id", "title", "lifecycle", "created_at_unix_ms",
            "updated_at_unix_ms", "participant_count", "message_count", "last_seq",
            "queued_delivery_count"
        ]
    })
}

fn conversation_detail_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "conversation": conversation_summary_schema(),
            "participants": array_schema(participant_schema(), "Human and Agent participant provenance."),
            "messages": array_schema(message_schema(), "Ordered append-only transcript page."),
            "after_seq": schema_type("integer", "Requested exclusive transcript cursor."),
            "next_after_seq": nullable_integer("Continuation cursor when truncated."),
            "truncated": schema_type("boolean", "True when more transcript messages remain.")
        },
        "required": [
            "conversation", "participants", "messages", "after_seq",
            "next_after_seq", "truncated"
        ]
    })
}

pub fn output_schema_for_tool(name: &str) -> Option<Value> {
    let schema = match name {
        "create_agent_identity" | "update_agent_identity" => wrapped_output_schema(vec![
            ("agent", agent_schema()),
            (
                "created",
                schema_type(
                    "boolean",
                    "True only for first creation; false for profile updates.",
                ),
            ),
            (
                "replayed",
                schema_type("boolean", "True for exact idempotent creation replay."),
            ),
            (
                "state_changed",
                schema_type("boolean", "Whether durable state changed."),
            ),
        ]),
        "list_agent_identities" => wrapped_output_schema(vec![
            (
                "total_count",
                schema_type(
                    "integer",
                    "Total Agents visible to the current communication principal.",
                ),
            ),
            ("offset", schema_type("integer", "Returned page offset.")),
            (
                "next_offset",
                nullable_integer("Next page offset when truncated."),
            ),
            (
                "truncated",
                schema_type("boolean", "True when more Agents remain."),
            ),
            (
                "agents",
                array_schema(agent_schema(), "Bounded Agent Card page."),
            ),
        ]),
        "present_agent_continuation" => wrapped_output_schema(vec![(
            "agent_continuation",
            agent_continuation_projection_schema(),
        )]),
        "attach_agent_endpoint" | "detach_agent_endpoint" => wrapped_output_schema(vec![
            ("endpoint", endpoint_schema()),
            (
                "created",
                schema_type(
                    "boolean",
                    "True only for first attachment; false for detach.",
                ),
            ),
            (
                "replayed",
                schema_type("boolean", "True for exact idempotent attach replay."),
            ),
            (
                "state_changed",
                schema_type("boolean", "Whether attachment state changed."),
            ),
        ]),
        "bootstrap_agent_conversation" => wrapped_output_schema(vec![
            ("acting_agent", agent_schema()),
            ("endpoint", endpoint_schema()),
            (
                "selected_conversation",
                json!({
                    "anyOf": [conversation_summary_schema(), {"type": "null"}],
                    "description": "Explicitly selected Conversation, or the exact Wake's latest Conversation. Null means no hidden Host selection was inferred."
                }),
            ),
            (
                "inbox",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "queued_delivery_count": schema_type("integer", "Current authoritative queued Inbox count."),
                        "inbox_high_watermark": schema_type("integer", "Highest currently queued durable delivery_order, or zero.")
                    },
                    "required": ["queued_delivery_count", "inbox_high_watermark"]
                }),
            ),
            (
                "wake",
                json!({
                    "anyOf": [
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "wake_id": schema_type("string", "Exact unresolved durable Wake identity."),
                                "state": {"type": "string", "enum": ["pending", "claimed", "prepared", "delivered", "delivery_unknown"]},
                                "revision": schema_type("integer", "Current Wake revision."),
                                "conversation_id": schema_type("string", "Latest Conversation represented by the Wake."),
                                "latest_message_id": schema_type("string", "Latest Message id represented by the Wake; no Message body is included."),
                                "queued_delivery_count": schema_type("integer", "Bounded queued count snapshot represented by the Wake."),
                                "inbox_high_watermark": schema_type("integer", "Durable delivery high-watermark represented by the Wake.")
                            },
                            "required": [
                                "wake_id", "state", "revision", "conversation_id",
                                "latest_message_id", "queued_delivery_count", "inbox_high_watermark"
                            ]
                        },
                        {"type": "null"}
                    ]
                }),
            ),
            (
                "host_binding",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "adapter_registered": schema_type("boolean", "Whether this exact Endpoint generation has a current process-local adapter handle."),
                        "adapter_kind": nullable_string("Bounded adapter kind, if registered."),
                        "runtime_wake_capable": schema_type("boolean", "Conjunction of current durable Endpoint state and exact process-local adapter registration."),
                        "production_auto_resume_available": schema_type("boolean", "True only for a demonstrated production Host new-model-turn primitive."),
                        "manual_fallback": schema_type("boolean", "True when explicit Host/model activation is required.")
                    },
                    "required": [
                        "adapter_registered", "adapter_kind", "runtime_wake_capable",
                        "production_auto_resume_available", "manual_fallback"
                    ]
                }),
            ),
            (
                "reply_replay",
                json!({
                    "anyOf": [
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "wake_id": schema_type("string", "Stable Wake component of resumed-turn reply identity."),
                                "reply_operation_index_min": schema_type("integer", "Minimum per-send reply index."),
                                "reply_operation_index_max": schema_type("integer", "Maximum per-send reply index."),
                                "contract": schema_type("string", "How to replay exact uncertain sends without duplicating Messages.")
                            },
                            "required": ["wake_id", "reply_operation_index_min", "reply_operation_index_max", "contract"]
                        },
                        {"type": "null"}
                    ]
                }),
            ),
            (
                "wake_activation",
                json!({
                    "anyOf": [
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "wake_id": schema_type("string", "Exact Wake accepted by this already-active explicit turn."),
                                "attempt_id": schema_type("string", "Durable explicit-activation Wake Attempt."),
                                "consume_token": schema_type("string", "Exact token for consume_agent_wake; audit/session projections omit it."),
                                "adapter_kind": {"type": "string", "const": "explicit_activation"},
                                "replayed": schema_type("boolean", "Whether this activation exactly replayed."),
                                "state_changed": schema_type("boolean", "Whether this call first accepted the pending Wake.")
                            },
                            "required": [
                                "wake_id", "attempt_id", "consume_token", "adapter_kind",
                                "replayed", "state_changed"
                            ]
                        },
                        {"type": "null"}
                    ],
                    "description": "Present only when activation_idempotency_key accepts or replays a pending Wake into the current explicit model turn."
                }),
            ),
            (
                "bootstrap_note",
                schema_type("string", "Bounded reminder that durable reads remain authoritative."),
            ),
        ]),
        "create_conversation" => wrapped_output_schema(vec![
            ("conversation", conversation_detail_schema()),
            (
                "created",
                schema_type("boolean", "True only for first committed creation."),
            ),
            (
                "replayed",
                schema_type("boolean", "True for exact idempotent replay."),
            ),
            (
                "state_changed",
                schema_type("boolean", "Whether durable state changed."),
            ),
        ]),
        "list_conversations" => wrapped_output_schema(vec![
            (
                "total_count",
                schema_type(
                    "integer",
                    "Total Conversations visible to this Human or Agent view.",
                ),
            ),
            ("offset", schema_type("integer", "Returned page offset.")),
            (
                "next_offset",
                nullable_integer("Next page offset when truncated."),
            ),
            (
                "truncated",
                schema_type("boolean", "True when more Conversations remain."),
            ),
            (
                "conversations",
                array_schema(conversation_summary_schema(), "Bounded Conversation page."),
            ),
        ]),
        "read_conversation" => wrapped_output_schema(vec![
            ("conversation", conversation_summary_schema()),
            (
                "participants",
                array_schema(participant_schema(), "Conversation participants."),
            ),
            (
                "messages",
                array_schema(message_schema(), "Ordered transcript page."),
            ),
            (
                "after_seq",
                schema_type("integer", "Requested exclusive sequence cursor."),
            ),
            (
                "next_after_seq",
                nullable_integer("Continuation cursor when truncated."),
            ),
            (
                "truncated",
                schema_type("boolean", "True when more messages remain."),
            ),
        ]),
        "post_conversation_message" => wrapped_output_schema(vec![
            ("message", message_schema()),
            (
                "replayed",
                schema_type("boolean", "True for exact idempotent replay."),
            ),
            (
                "state_changed",
                schema_type("boolean", "True only for first append."),
            ),
        ]),
        "list_agent_inbox" => wrapped_output_schema(vec![
            ("agent_id", schema_type("string", "Recipient Agent id.")),
            (
                "total_queued_count",
                schema_type("integer", "Total queued deliveries for this Agent."),
            ),
            (
                "after_delivery_order",
                schema_type("integer", "Requested exclusive delivery cursor."),
            ),
            (
                "next_after_delivery_order",
                nullable_integer("Continuation cursor when truncated."),
            ),
            (
                "truncated",
                schema_type("boolean", "True when more queued deliveries remain."),
            ),
            (
                "deliveries",
                array_schema(
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "delivery_id": schema_type("string", "Canonical Delivery id."),
                            "state": {"type": "string", "enum": ["queued"]},
                            "conversation_id": schema_type("string", "Owning Conversation id."),
                            "conversation_title": nullable_string("Optional Conversation title."),
                            "message": message_schema()
                        },
                        "required": ["delivery_id", "state", "conversation_id", "conversation_title", "message"]
                    }),
                    "Bounded queued Inbox page.",
                ),
            ),
        ]),
        "consume_agent_deliveries" => wrapped_output_schema(vec![
            ("agent_id", schema_type("string", "Recipient Agent id.")),
            (
                "consumed_delivery_ids",
                array_schema(
                    schema_type("string", "Delivery changed from queued to consumed."),
                    "Deliveries changed by this call.",
                ),
            ),
            (
                "already_consumed_delivery_ids",
                array_schema(
                    schema_type("string", "Delivery already in the desired consumed state."),
                    "Desired-state replay results.",
                ),
            ),
            (
                "state_changed",
                schema_type("boolean", "True when at least one delivery changed; Wake state is unaffected."),
            ),
        ]),
        "consume_agent_wake" => wrapped_output_schema(vec![
            ("wake_id", schema_type("string", "Exact durable Wake Intent consumed.")),
            ("target_agent_id", schema_type("string", "Exact target Agent bound to the Wake.")),
            ("state", json!({"type": "string", "enum": ["consumed"]})),
            ("already_consumed", schema_type("boolean", "True when this exact continuation had already been consumed by the same Endpoint generation.")),
            ("consumed_at_unix_ms", schema_type("integer", "Stable consume time in Unix milliseconds.")),
            ("state_changed", schema_type("boolean", "True only for the first exact consume; Inbox Delivery state is unaffected.")),
        ]),
        _ => return None,
    };
    Some(schema)
}
