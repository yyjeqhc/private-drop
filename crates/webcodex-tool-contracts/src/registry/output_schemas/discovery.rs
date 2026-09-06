use serde_json::{json, Value};

use super::common::{
    array_schema, authority_profile_schema, nullable_schema, open_object_schema, schema_type,
    wrapped_output_schema,
};

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    match name {
        "runtime_status" => Some(wrapped_output_schema(vec![
            ("service", schema_type("string", "Runtime service name.")),
            (
                "runtime_exposure",
                schema_type(
                    "string",
                    "Configured runtime exposure: local_coding, adaptive_runtime, full_operator_runtime, or project_connector.",
                ),
            ),
            (
                "mcp_compact_schemas",
                schema_type(
                    "boolean",
                    "Whether MCP tools/list omits outputSchema while retaining tool names, descriptions, inputSchema, and annotations. This is MCP discovery schema compaction, not runtime_status compact=true response shaping or GPT Action response compaction.",
                ),
            ),
            (
                "effective_config",
                json!({
                    "type": "object",
                    "description": "Safe allowlisted effective configuration of the running Server. This is distinct from runtime_status compact=true response shaping and from health or transport state.",
                    "additionalProperties": false,
                    "properties": {
                        "auth": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "shared_key_enabled": {"type": "boolean", "description": "Whether direct shared-key quick-start authentication is effective for the running Server; false under the project-bound ProjectConnector exposure."},
                                "anonymous_enabled": {"type": "boolean", "description": "Whether explicit open-anonymous access is effective for the running Server; false under the project-bound ProjectConnector exposure."},
                                "oauth2_enabled": {"type": "boolean", "description": "Whether OAuth2 support was enabled in the running Server configuration."},
                                "oauth2_shared_key_bridge_enabled": {"type": "boolean", "description": "Whether the OAuth2 shared-key bridge is enabled in the running OAuth2 configuration; false whenever OAuth2 itself is disabled. This public OAuth flow is distinct from direct Bearer shared-key authentication."}
                            },
                            "required": ["shared_key_enabled", "anonymous_enabled", "oauth2_enabled", "oauth2_shared_key_bridge_enabled"]
                        },
                        "tool_request_trace_mode": {
                            "type": "string",
                            "enum": ["off", "metadata", "full"],
                            "description": "Effective bounded tool-request trace mode; no trace paths, request bodies, headers, or environment values are exposed."
                        }
                    },
                    "required": ["auth", "tool_request_trace_mode"]
                }),
            ),
            ("version", schema_type("string", "Runtime version.")),
            (
                "focus",
                open_object_schema("Exact focused Runner status when client_id is supplied."),
            ),
            (
                "server",
                open_object_schema("Server build/version identity in focused mode."),
            ),
            (
                "fleet_summary",
                open_object_schema("Secondary aggregate fleet mismatch counts in focused mode; never overrides focus truth."),
            ),
            (
                "build",
                open_object_schema("Build revision metadata for the running binary."),
            ),
            ("server_time", schema_type("integer", "Server timestamp.")),
            ("pid", schema_type("integer", "Server process id.")),
            (
                "auth_enabled",
                schema_type("boolean", "Whether bearer auth is enabled."),
            ),
            (
                "configured_public_url",
                nullable_schema("string", "Configured public URL, when set."),
            ),
            (
                "projects",
                open_object_schema("Project counts from the Runner registry. Prefer projects.effective for model-facing status."),
            ),
            (
                "agents",
                open_object_schema("Runner counts and client summaries. Per-client host_context is bounded Runner-configured advisory data, not observed truth or authority. job_concurrency contains the static Runner limit plus caller-visible running and queued counts. Canonical top-level counts are count, online_count, and stale_count in full, compact, and summary_only output."),
            ),
            (
                "jobs",
                open_object_schema("Bounded runtime job counts, including active_count, running_count, queued_count, recovering_count, reconciled_count, and lost_after_reconcile_count."),
            ),
            ("tools", open_object_schema("Runtime tool counts and names.")),
            (
                "authority",
                authority_profile_schema("Canonical authority profile. trusted_agent is the self-hosted single-operator default and does not bypass hard safety checks (scopes, project boundary, read-only sessions, path policy)."),
            ),
            (
                "quic",
                open_object_schema("QUIC transport status, when enabled."),
            ),
        ])),
        "read_tool_trace" => Some(wrapped_output_schema(vec![
            ("trace_mode", schema_type("string", "Trace mode; read_tool_trace requires full.")),
            ("payload_count", schema_type("integer", "Total indexed payloads in this trace.")),
            ("returned_count", schema_type("integer", "Payload metadata entries returned in listing mode.")),
            ("offset", schema_type("integer", "Listing offset.")),
            ("next_offset", nullable_schema("integer", "Next listing offset or null.")),
            ("payloads", array_schema(open_object_schema("Safe payload metadata: payload_index, phase, payload_bytes, compressed_bytes, payload_sha256, and payload_available. Native paths are never returned."), "Bounded payload metadata.")),
            ("payload_index", schema_type("integer", "Selected payload index in payload-read mode.")),
            ("phase", schema_type("string", "Selected payload lifecycle phase.")),
            ("payload_bytes", schema_type("integer", "Uncompressed payload size.")),
            ("payload_sha256", schema_type("string", "SHA-256 of the selected uncompressed payload.")),
            ("payload_available", schema_type("boolean", "Whether the selected payload is within the model read ceiling.")),
            ("max_payload_bytes", schema_type("integer", "Maximum uncompressed payload bytes readable through this tool.")),
            ("reason", schema_type("string", "Bounded reason when a payload is not model-readable.")),
            ("payload", json!({"description": "Selected raw JSON trace payload of any JSON type. May contain sensitive tool data; operator-only."})),
        ])),
        "list_projects" => Some(wrapped_output_schema(vec![
            (
                "projects",
                array_schema(open_object_schema("Project summary including capabilities.git_available, supports_cleanup_verification, and recommended_for_smoke."), "Runtime projects."),
            ),
            ("count", schema_type("integer", "Project count.")),
            (
                "matched_count",
                schema_type("integer", "Caller-visible Project count matching filters before limit."),
            ),
            (
                "truncated",
                schema_type("boolean", "Whether limit truncated matching Projects."),
            ),
            (
                "recommended_for_smoke",
                array_schema(
                    schema_type("string", "Runtime project id recommended for smoke tests."),
                    "Runtime project ids whose capabilities.recommended_for_smoke is true.",
                ),
            ),
        ])),
        "list_runners" => Some(wrapped_output_schema(vec![
            (
                "agents",
                array_schema(open_object_schema("Runner summary including bounded Runner-configured host_context advisory data, never authority or proof of current state, plus job_concurrency limit/running/queued facts."), "Legacy compatibility key containing Runner summaries."),
            ),
            (
                "clients",
                array_schema(open_object_schema("Runner client summary including job_concurrency limit/running/queued facts."), "Runner client summaries."),
            ),
            ("count", schema_type("integer", "Runner/client count.")),
        ])),
        "list_tools" => Some(wrapped_output_schema(vec![
            (
                "tools",
                array_schema(
                    open_object_schema("Tool metadata or compact summary."),
                    "Runtime tool specs, or compact summaries when summary_only is true.",
                ),
            ),
            (
                "names",
                array_schema(schema_type("string", "Tool name."), "Returned tool names."),
            ),
            ("count", schema_type("integer", "Tool count.")),
            (
                "returned_count",
                schema_type("integer", "Returned tool count after filters and limits."),
            ),
            (
                "total_count",
                schema_type("integer", "Total number of visible runtime tools."),
            ),
            (
                "filtered_count",
                schema_type("integer", "Number of tools matching category/features before limit."),
            ),
            (
                "truncated",
                schema_type("boolean", "Whether limit truncated the matching tools."),
            ),
            (
                "truncation_reason",
                nullable_schema("string", "Reason for truncation, such as limit, or null."),
            ),
            (
                "limit_applied",
                schema_type("boolean", "Whether a caller-supplied limit was applied."),
            ),
            (
                "requested_limit",
                nullable_schema("integer", "Caller-supplied limit before effective cap, or null."),
            ),
            (
                "category",
                nullable_schema("string", "Requested category filter, or null."),
            ),
            (
                "features",
                nullable_schema("string", "Requested loose feature filter, or null."),
            ),
            (
                "limit",
                nullable_schema("integer", "Effective focused discovery limit, or null."),
            ),
            (
                "categories",
                open_object_schema("Map of discovery category name to visible tool names."),
            ),
            (
                "recommended_flows",
                array_schema(
                    schema_type("string", "Short recommended tool flow summary."),
                    "Short GPT-facing recommended flow summaries.",
                ),
            ),
            ("hint", schema_type("string", "Focused discovery guidance.")),
            (
                "recommended_next",
                schema_type("string", "Recommended next discovery action."),
            ),
        ])),
        "tool_manifest" => Some(wrapped_output_schema(vec![
            (
                "name",
                schema_type(
                    "string",
                    "Exact requested tool identity in the sparse model-facing exact projection.",
                ),
            ),
            (
                "description",
                schema_type(
                    "string",
                    "Canonical ToolSpec description for exact lookup, or a bounded canonical-derived selection summary in filtered tool entries.",
                ),
            ),
            (
                "route",
                json!({
                    "type": "object",
                    "description": "Current ModelSurface invocation route only. This never grants scope, project authority, feature availability, or permission.",
                    "additionalProperties": false,
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["direct", "gateway", "unavailable"]
                        },
                        "via": {
                            "type": "string",
                            "const": "call_runtime_tool",
                            "description": "Gateway entry point, present only when mode=gateway."
                        }
                    },
                    "required": ["mode"]
                }),
            ),
            (
                "input_schema",
                open_object_schema(
                    "Exact tool input schema in the sparse model-facing exact projection; output schema remains omitted.",
                ),
            ),
            (
                "effect",
                schema_type(
                    "string",
                    "Canonical business effect: observe, mutate, or execute.",
                ),
            ),
            (
                "risk",
                schema_type(
                    "string",
                    "Canonical risk class when relevant to exact lookup or filtered selection.",
                ),
            ),
            (
                "approval",
                schema_type(
                    "string",
                    "Canonical interactive approval policy in exact lookup.",
                ),
            ),
            (
                "idempotency",
                schema_type(
                    "string",
                    "Canonical retry/idempotency contract in exact lookup.",
                ),
            ),
            (
                "authority",
                open_object_schema(
                    "Canonical required-scope policy in exact lookup. Route discovery never grants these scopes.",
                ),
            ),
            (
                "annotations",
                open_object_schema("Canonical ToolSpec annotations in exact lookup."),
            ),
            (
                "schema_version",
                schema_type("integer", "Manifest schema version."),
            ),
            (
                "tool_count",
                schema_type("integer", "Total number of tools in the runtime."),
            ),
            (
                "count",
                schema_type("integer", "Returned compact tool count after filtering."),
            ),
            (
                "returned_count",
                schema_type("integer", "Returned compact tool count after filtering and limit."),
            ),
            (
                "total_count",
                schema_type("integer", "Total number of runtime tools before filtering."),
            ),
            (
                "filtered_count",
                schema_type(
                    "integer",
                    "Number of tools after applying the optional category filter.",
                ),
            ),
            (
                "tool_name",
                nullable_schema("string", "Compatibility/full canonical exact requested tool name. The default sparse model projection uses name instead."),
            ),
            (
                "contract",
                json!({
                    "anyOf": [
                        {
                            "type": "object",
                            "description": "Exact one-tool contract containing name, description, canonical effect/risk/approval/idempotency semantics, input_schema, annotations, and current MCP model-surface invocation routing. output_schema is intentionally omitted.",
                            "additionalProperties": false,
                            "properties": {
                                "name": {"type": "string"},
                                "description": {"type": "string"},
                                "effect": {
                                    "type": "string",
                                    "enum": ["observe", "mutate", "execute"],
                                    "description": "Canonical business effect."
                                },
                                "risk": {
                                    "type": "string",
                                    "enum": ["read_only", "project_write", "skill_manage", "memory_manage", "communication_manage", "session_collaborate", "workflow_manage", "checkpoint_manage", "run_control", "computer_control", "job_run", "account_manage"],
                                    "description": "Canonical risk class; read_only is the retained external label for read/observation risk."
                                },
                                "approval": {
                                    "type": "string",
                                    "enum": ["none", "standard", "inherit_from_start"],
                                    "description": "Canonical interactive approval policy, independent from effect and authority."
                                },
                                "idempotency": {
                                    "type": "string",
                                    "enum": ["pure_read", "desired_state", "keyed", "fenced_replay", "non_idempotent"],
                                    "description": "Canonical retry/idempotency contract."
                                },
                                "input_schema": {"type": "object", "additionalProperties": true},
                                "annotations": {"type": "object", "additionalProperties": true},
                                "availability": {
                                    "type": "string",
                                    "enum": ["direct", "gateway", "unavailable"],
                                    "description": "Invocation route on the current MCP ModelSurface only; authorization, feature gates, and project authority are checked separately."
                                },
                                "gateway_tool": {
                                    "anyOf": [
                                        {"type": "string", "const": "call_runtime_tool"},
                                        {"type": "null"}
                                    ],
                                    "description": "call_runtime_tool only when availability=gateway; otherwise null."
                                }
                            },
                            "required": ["name", "description", "effect", "risk", "approval", "idempotency", "input_schema", "annotations", "availability", "gateway_tool"]
                        },
                        {"type": "null"}
                    ]
                }),
            ),
            (
                "category",
                nullable_schema(
                    "string",
                    "Requested category filter, or null when no filter was applied.",
                ),
            ),
            (
                "intent",
                nullable_schema(
                    "string",
                    "Resolved task-intent view such as coding, audit, exploration, release, or discovery; null when no intent was requested. Intent views only filter and rank discovery output; they do not change tool behavior, policy, permissions, execution, or finish verdict semantics.",
                ),
            ),
            (
                "available_intents",
                array_schema(
                    schema_type("string", "Supported tool_manifest intent name."),
                    "Stable list of supported tool_manifest intent names used for discovery filtering only.",
                ),
            ),
            (
                "filtered",
                schema_type(
                    "boolean",
                    "True when intent, category filtering, or limit was applied.",
                ),
            ),
            (
                "categories_requested",
                nullable_string_array_schema(
                    "Normalized requested category filters, or null when unfiltered.",
                ),
            ),
            (
                "limit",
                nullable_schema("integer", "Effective manifest limit, or null."),
            ),
            (
                "truncated",
                schema_type("boolean", "Whether the limit truncated matching tools."),
            ),
            (
                "truncation_reason",
                nullable_schema("string", "Reason for truncation, such as limit, or null."),
            ),
            (
                "limit_applied",
                schema_type("boolean", "Whether a caller-supplied limit was applied."),
            ),
            (
                "requested_limit",
                nullable_schema("integer", "Caller-supplied limit after runtime clamping, or null."),
            ),
            (
                "categories",
                open_object_schema(
                    "Global category inventory in unfiltered discovery. Compatibility/full canonical results may also carry it for filtered/exact calls, but the default sparse model projection omits unrelated inventory.",
                ),
            ),
            (
                "tools",
                array_schema(
                    open_object_schema(
                        "Default filtered model projection: name, bounded canonical-derived description, route, requires_project, effect, and risk only when non-observe. Compatibility/full canonical results may retain richer metadata."
                    ),
                    "Filtered selection entries without input/output schemas; unfiltered sparse discovery uses the categories inventory instead of duplicating all tool names here.",
                ),
            ),
            (
                "risk_summary",
                open_object_schema(
                    "Counts of tools grouped by risk class (read_only, project_write, job_run, etc.).",
                ),
            ),
            (
                "recommended_flows",
                array_schema(
                    open_object_schema("Recommended tool flow with name, purpose, and tools."),
                    "Short list of recommended tool flows for common tasks.",
                ),
            ),
        ])),
        _ => None,
    }
}

fn nullable_string_array_schema(description: &str) -> Value {
    json!({
        "anyOf": [
            {
                "type": "array",
                "items": {
                    "type": "string"
                }
            },
            {
                "type": "null"
            }
        ],
        "description": description,
    })
}
