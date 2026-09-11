use serde_json::{json, Value};

use super::common::{
    array_schema, nullable_schema, permission_decision_schema, schema_type, search_match_schema,
    session_hint_schema, wrapped_output_schema,
};

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    match name {
        "project_overview" => Some(wrapped_output_schema(vec![
            ("schema_version", schema_type("integer", "Overview schema version.")),
            ("project", schema_type("string", "Resolved runtime project id.")),
            ("path", schema_type("string", "Project-relative overview scope; empty means project root.")),
            ("deterministic", schema_type("boolean", "Always true; the overview uses deterministic path evidence only.")),
            ("project_types", array_schema(project_type_schema(), "Detected project types with project-relative evidence paths.")),
            ("manifests", array_schema(path_kind_schema("Detected build or package manifest."), "Detected manifests.")),
            ("key_files", array_schema(key_file_schema(), "Prioritized project entrypoints; metadata only.")),
            ("roots", roots_schema()),
            ("top_level", array_schema(top_level_entry_schema(), "Direct safe children of the requested path.")),
            ("suggested_next_reads", array_schema(suggested_read_schema(), "Bounded key-file subset recommended for later read_file calls.")),
            ("scan", scan_schema()),
            ("warnings", array_schema(schema_type("string", "Stable warning code."), "Bounded scan warning codes.")),
        ])),
        "list_project_files" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved project id.")),
            (
                "path",
                schema_type("string", "Project-relative listed directory path."),
            ),
            (
                "entries",
                array_schema(
                    file_list_entry_schema(),
                    "Bounded project-relative file and directory entries.",
                ),
            ),
            (
                "returned",
                schema_type("integer", "Number of entries returned in this page."),
            ),
            (
                "total_entries",
                schema_type(
                    "integer",
                    "Exact entry count in the fully acquired, deterministically sorted directory source.",
                ),
            ),
            (
                "offset",
                schema_type("integer", "Zero-based offset used for this page."),
            ),
            (
                "next_offset",
                nullable_schema(
                    "integer",
                    "Exact offset for the next page, or null when this page reaches the end of the complete source.",
                ),
            ),
            (
                "truncated",
                schema_type(
                    "boolean",
                    "Whether another deterministic page remains after this page.",
                ),
            ),
        ])),
        "list_project_tracked_files" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved project id.")),
            (
                "path",
                schema_type("string", "Project-relative scope; empty means project root."),
            ),
            (
                "entries",
                array_schema(
                    tracked_list_entry_schema(),
                    "Tracked files, plus rolled-up directories carrying file_count.",
                ),
            ),
            ("returned", schema_type("integer", "Entries in this page.")),
            (
                "total_files",
                schema_type("integer", "Tracked files matching scope and globs, before rollup."),
            ),
            (
                "total_entries",
                schema_type("integer", "Entries after rollup, before paging."),
            ),
            (
                "depth",
                nullable_schema(
                    "integer",
                    "Effective rollup depth; null means every matching file is listed individually.",
                ),
            ),
            (
                "depth_auto",
                schema_type(
                    "boolean",
                    "True when depth was chosen automatically because the flat list exceeded limit.",
                ),
            ),
            (
                "truncated",
                schema_type("boolean", "Whether more entries remain on a later page."),
            ),
            (
                "next_offset",
                nullable_schema("integer", "Offset that continues the listing; null when complete."),
            ),
            (
                "list_truncated",
                schema_type(
                    "boolean",
                    "True when the raw index listing hit the transport cap, so total_files undercounts. Distinct from truncated, which is paging.",
                ),
            ),
            (
                "source",
                schema_type("string", "Listing source; git_index."),
            ),
            (
                "code",
                schema_type("string", "Stable structured error code on failure."),
            ),
            ("message", schema_type("string", "Structured failure message.")),
        ])),
        "read_file" => Some(read_file_output_schema()),
        "read_files" => Some(read_files_output_schema()),
        "search_project_texts" => Some(search_project_texts_output_schema()),
        "search_project_text" => {
            Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved project id; omitted as request echo in ordinary complete default rg/matches success.")),
            ("pattern", schema_type("string", "Search pattern; omitted as request echo in ordinary complete default rg/matches success.")),
            (
                "path",
                schema_type("string", "Project-relative search root; omitted when it is the default project root in ordinary complete default rg/matches success."),
            ),
            (
                "backend",
                nullable_schema(
                    "string",
                    "Search backend used: rg, grep, native, or claude_code. Omitted for ordinary complete default rg/matches success, or null/omitted when unknown (for example outer wait timeout before backend selection).",
                ),
            ),
            (
                "result_mode",
                json!({
                    "type": "string",
                    "enum": ["matches", "files_with_matches", "count"],
                    "description": "Effective result mode; omitted when it is the default matches mode in ordinary complete rg success."
                }),
            ),
            (
                "pattern_mode",
                json!({
                    "type": "string",
                    "enum": ["regex", "literal"],
                    "description": "Effective pattern interpretation; omitted when it is the default regex mode in ordinary complete rg/matches success."
                }),
            ),
            (
                "effective_timeout_secs",
                schema_type("integer", "Effective clamped timeout in seconds; omitted when it is the default budget in ordinary complete rg/matches success."),
            ),
            (
                "matches",
                array_schema(
                    search_match_schema(),
                    "Bounded search matches; present in matches mode.",
                ),
            ),
            ("count", schema_type("integer", "Returned match count; omitted in sparse default matches success because it equals matches.length.")),
            (
                "files",
                array_schema(
                    search_file_result_schema(),
                    "Bounded file records for files_with_matches or count mode.",
                ),
            ),
            (
                "returned_file_count",
                schema_type("integer", "Number of returned file records."),
            ),
            (
                "returned_match_count",
                schema_type(
                    "integer",
                    "Sum of match_count values in returned count-mode file records.",
                ),
            ),
            (
                "count_complete",
                schema_type(
                    "boolean",
                    "True only when count mode completed without limit or transport truncation.",
                ),
            ),
            (
                "total_matches",
                nullable_schema(
                    "integer",
                    "Global matching-line total only when count_complete=true; otherwise null.",
                ),
            ),
            (
                "truncated",
                schema_type("boolean", "Whether more mode-specific records were available; false is omitted in sparse default matches success."),
            ),
            (
                "truncation_reason",
                json!({
                    "anyOf": [
                        {
                            "type": "string",
                            "enum": ["limit", "output_bytes", "timeout", "transport"],
                        },
                        { "type": "null" }
                    ],
                    "description": "Truncation reason: limit, output_bytes (the search byte budget cut the stream, complete records only), timeout (the effective timeout fired; records collected before it are complete), or transport; null when complete."
                }),
            ),
            (
                "continuation",
                search_refinement_continuation_schema(),
            ),
            (
                "exit_code",
                nullable_schema("integer", "Search command exit code, when available."),
            ),
            (
                "context_before",
                schema_type("integer", "Effective context lines before each match."),
            ),
            (
                "context_after",
                schema_type("integer", "Effective context lines after each match."),
            ),
            (
                "code",
                schema_type(
                    "string",
                    "Stable compatibility error code on validation, backend capability, execution, timeout, provider, or request-drop failure.",
                ),
            ),
            (
                "failure_stage",
                json!({
                    "type": "string",
                    "enum": [
                        "request_validation", "backend_selection", "backend_protocol",
                        "backend_execution", "agent_request", "agent_execution",
                        "agent_transport", "provider", "local_execution", "batch_deadline"
                    ],
                    "description": "Stable stage at which search evidence failed."
                }),
            ),
            (
                "reason_code",
                json!({
                    "type": "string",
                    "enum": [
                        "invalid_pattern", "invalid_path", "invalid_glob",
                        "invalid_search_request", "backend_feature_unavailable",
                        "backend_identity_missing", "backend_identity_invalid",
                        "backend_status_unavailable", "backend_output_inconsistent",
                        "backend_process_failed", "agent_request_failed",
                        "agent_execution_failed", "search_request_dropped", "timeout",
                        "provider_execution_failed", "provider_protocol_invalid",
                        "local_execution_failed"
                    ],
                    "description": "Specific stable failure reason within failure_stage."
                }),
            ),
            (
                "field",
                schema_type(
                    "string",
                    "Input field name for invalid_search_request failures.",
                ),
            ),
            (
                "index",
                schema_type(
                    "integer",
                    "Optional zero-based index for invalid glob list entries.",
                ),
            ),
            (
                "reason",
                schema_type(
                    "string",
                    "Optional stable validation reason (empty, too_long, control_char, negated, protected_path, too_many, nul_byte, invalid_path).",
                ),
            ),
            (
                "requested_features",
                array_schema(
                    schema_type("string", "Requested advanced feature."),
                    "Advanced features that require ripgrep.",
                ),
            ),
            (
                "format",
                schema_type("string", "Bounded external-provider envelope format on provider failure."),
            ),
            (
                "provider",
                schema_type("string", "Known external search provider identity."),
            ),
            (
                "provider_code",
                json!({
                    "type": "string",
                    "maxLength": 64,
                    "pattern": "^[a-z0-9_]+$",
                    "description": "Sanitized provider error code; arbitrary provider prose is not returned."
                }),
            ),
            (
                "capability",
                schema_type("string", "External provider capability that failed."),
            ),
            (
                "write_state",
                schema_type("string", "Provider write state; search failures are not_submitted."),
            ),
            (
                "changed",
                schema_type("boolean", "False for read-only search failures."),
            ),
            (
                "error",
                schema_type("string", "Bounded stable provider error classification."),
            ),
            ("message", schema_type("string", "Structured failure message.")),
        ]))
        }
        _ => None,
    }
}

fn search_refinement_continuation_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Model-facing recovery hint for a single truncated query. Search match order is not a stable cursor, so this requires query refinement rather than offset paging.",
        "properties": {
            "kind": {"type": "string", "const": "refine_query"},
            "safe_cursor": {"type": "boolean", "const": false},
            "refine_with": {
                "type": "array",
                "minItems": 1,
                "maxItems": 5,
                "uniqueItems": true,
                "items": {
                    "type": "string",
                    "enum": ["path", "include_globs", "pattern", "result_mode", "limit"]
                }
            }
        },
        "required": ["kind", "safe_cursor", "refine_with"]
    })
}

fn search_project_texts_output_schema() -> Value {
    let search_success_properties = json!({
        "path": schema_type("string", "Effective project-relative search root; omitted for the default project root in sparse complete matches success."),
        "backend": nullable_schema("string", "Search backend: rg, grep, native, or null when unknown; omitted for ordinary complete default rg/matches success."),
        "result_mode": {"type": "string", "enum": ["matches", "files_with_matches", "count"]},
        "pattern_mode": {"type": "string", "enum": ["regex", "literal"]},
        "effective_timeout_secs": {"type": "integer", "minimum": 1, "maximum": 120},
        "exit_code": nullable_schema("integer", "Search command exit code, when available."),
        "context_before": {"type": "integer", "minimum": 0, "maximum": 20},
        "context_after": {"type": "integer", "minimum": 0, "maximum": 20},
        "matches": {"type": "array", "items": search_match_schema()},
        "count": {"type": "integer", "minimum": 0},
        "files": {"type": "array", "items": search_file_result_schema()},
        "returned_file_count": {"type": "integer", "minimum": 0},
        "returned_match_count": {"type": "integer", "minimum": 0},
        "count_complete": {"type": "boolean"},
        "total_matches": nullable_schema("integer", "Complete total in count mode; null when incomplete."),
        "truncated": {"type": "boolean"},
        "truncation_reason": {
            "anyOf": [
                {"type": "string", "enum": ["limit", "output_bytes", "timeout", "transport"]},
                {"type": "null"}
            ]
        },
        "continuation": search_refinement_continuation_schema()
    });
    let search_success_full = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": search_success_properties.clone(),
        "required": [
            "path", "backend", "result_mode", "pattern_mode", "effective_timeout_secs", "exit_code",
            "context_before", "context_after", "truncated", "truncation_reason"
        ]
    });
    let search_success_sparse_matches = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": search_success_properties["path"].clone(),
            "pattern_mode": {"type": "string", "const": "literal"},
            "effective_timeout_secs": search_success_properties["effective_timeout_secs"].clone(),
            "context_before": search_success_properties["context_before"].clone(),
            "context_after": search_success_properties["context_after"].clone(),
            "matches": search_success_properties["matches"].clone()
        },
        "required": ["matches"],
        "description": "Sparse model-facing form for complete rg matches-mode success. Literal mode, custom timeout, non-root path, and requested context counts remain explicit; boring defaults are omitted."
    });
    let search_success_sparse_files = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": search_success_properties["path"].clone(),
            "result_mode": {"type": "string", "const": "files_with_matches"},
            "pattern_mode": {"type": "string", "const": "literal"},
            "effective_timeout_secs": search_success_properties["effective_timeout_secs"].clone(),
            "context_before": search_success_properties["context_before"].clone(),
            "context_after": search_success_properties["context_after"].clone(),
            "files": search_success_properties["files"].clone()
        },
        "required": ["result_mode", "files"],
        "description": "Sparse model-facing form for complete rg files_with_matches success; files are the primary result and redundant returned-file counts are omitted."
    });
    let search_success_sparse_count = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": search_success_properties["path"].clone(),
            "result_mode": {"type": "string", "const": "count"},
            "pattern_mode": {"type": "string", "const": "literal"},
            "effective_timeout_secs": search_success_properties["effective_timeout_secs"].clone(),
            "context_before": search_success_properties["context_before"].clone(),
            "context_after": search_success_properties["context_after"].clone(),
            "files": search_success_properties["files"].clone(),
            "total_matches": {"type": "integer", "minimum": 0}
        },
        "required": ["result_mode", "total_matches"],
        "description": "Sparse model-facing form for complete rg count success. total_matches is authoritative; optional files retain bounded per-path grouping and redundant count bookkeeping is omitted."
    });
    let search_success = json!({
        "anyOf": [
            search_success_full,
            search_success_sparse_matches,
            search_success_sparse_files,
            search_success_sparse_count
        ]
    });
    let search_failure = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "error_kind": {"type": "string", "const": "search_project_text_failed"},
            "reason_code": {
                "type": "string",
                "enum": [
                    "invalid_pattern", "invalid_path", "invalid_glob", "invalid_search_request",
                    "search_backend_feature_unavailable", "search_execution_failed", "timeout",
                    "search_request_dropped", "external_provider_error", "agent_unavailable"
                ]
            },
            "failure_stage": {
                "type": "string",
                "enum": [
                    "request_validation", "backend_selection", "backend_protocol",
                    "backend_execution", "agent_request", "agent_execution",
                    "agent_transport", "provider", "local_execution", "batch_deadline"
                ]
            },
            "detail_code": {
                "type": "string",
                "enum": [
                    "invalid_pattern", "invalid_path", "invalid_glob",
                    "invalid_search_request", "backend_feature_unavailable",
                    "backend_identity_missing", "backend_identity_invalid",
                    "backend_status_unavailable", "backend_output_inconsistent",
                    "backend_process_failed", "agent_request_failed",
                    "agent_execution_failed", "search_request_dropped", "timeout",
                    "provider_execution_failed", "provider_protocol_invalid",
                    "local_execution_failed",
                    "search_backend_feature_unavailable", "search_execution_failed",
                    "external_provider_error", "agent_unavailable"
                ]
            },
            "backend": {"type": "string", "enum": ["rg", "grep", "native", "claude_code"]},
            "exit_code": {"type": "integer"},
            "result_mode": {"type": "string", "enum": ["matches", "files_with_matches", "count"]},
            "pattern_mode": {"type": "string", "enum": ["regex", "literal"]},
            "effective_timeout_secs": {"type": "integer", "minimum": 1, "maximum": 120},
            "provider_code": {
                "type": "string",
                "maxLength": 64,
                "pattern": "^[a-z0-9_]+$"
            },
            "state_changed": {"type": "boolean", "const": false}
        },
        "required": [
            "error_kind", "reason_code", "failure_stage", "detail_code", "state_changed"
        ]
    });
    let item_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "index": {"type": "integer", "minimum": 0, "maximum": 7},
            "success": {"type": "boolean"},
            "output": {"anyOf": [search_success.clone(), search_failure.clone()]},
            "error": {"anyOf": [{"type": "string"}, {"type": "null"}]}
        },
        "required": ["index", "success", "output", "error"],
        "allOf": [{
            "if": {"properties": {"success": {"const": true}}, "required": ["success"]},
            "then": {"properties": {"output": search_success, "error": {"type": "null"}}},
            "else": {"properties": {"output": search_failure, "error": {"type": "string"}}}
        }]
    });
    let batch_output_full = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "project": schema_type("string", "Resolved runtime project id."),
            "requested_count": {"type": "integer", "minimum": 1, "maximum": 8},
            "returned_count": {"type": "integer", "minimum": 0, "maximum": 8},
            "succeeded_count": {"type": "integer", "minimum": 0, "maximum": 8},
            "failed_count": {"type": "integer", "minimum": 0, "maximum": 8},
            "items": {"type": "array", "maxItems": 8, "items": item_schema.clone()},
            "output_truncated": {"type": "boolean"},
            "next_index": {"anyOf": [{"type": "integer", "minimum": 0, "maximum": 7}, {"type": "null"}]},
            "truncation_reason": {"type": "string", "enum": ["batch_response_budget", "hard_result_cap"]},
            "session_hint": session_hint_schema(),
            "permission": permission_decision_schema()
        },
        "required": [
            "project", "requested_count", "returned_count", "succeeded_count",
            "failed_count", "items", "output_truncated", "next_index"
        ]
    });
    let sparse_success_item = json!({
        "allOf": [
            item_schema,
            {
                "properties": {
                    "success": {"const": true},
                    "error": {"type": "null"}
                },
                "required": ["success", "error"]
            }
        ]
    });
    let batch_output_sparse = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {"type": "array", "minItems": 1, "maxItems": 8, "items": sparse_success_item},
            "session_hint": session_hint_schema(),
            "permission": permission_decision_schema()
        },
        "required": ["items"],
        "description": "Sparse model-facing batch form used only when every returned query succeeded and the outer batch was complete. Omitted counts and continuation fields therefore mean all items succeeded and no outer truncation occurred."
    });
    let batch_output = json!({
        "anyOf": [batch_output_full, batch_output_sparse]
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "success": {"type": "boolean"},
            "output": {"anyOf": [batch_output.clone(), {"type": "object", "additionalProperties": true}, {"type": "null"}]},
            "error": {"anyOf": [{"type": "string"}, {"type": "null"}]}
        },
        "required": ["success", "output"],
        "allOf": [{
            "if": {"properties": {"success": {"const": true}}, "required": ["success"]},
            "then": {"properties": {"output": batch_output, "error": {"type": "null"}}},
            "else": {"required": ["error"], "properties": {"error": {"type": "string"}}}
        }]
    })
}

fn read_suggested_call_schema(tool: &'static str, arguments: Value) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "tool": {"type": "string", "const": tool},
            "arguments": arguments
        },
        "required": ["tool", "arguments"]
    })
}

fn suggested_read_file_arguments_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "project": schema_type("string", "Exact resolved Project id selected by the current read; shorthand is never replayed by recovery."),
            "path": schema_type("string", "Same project-relative file path."),
            "start_line": {"type": "integer", "minimum": 1},
            "limit": {"type": "integer", "minimum": 1, "maximum": 2000},
            "session_id": {
                "type": "string",
                "pattern": "^wc_sess_[A-Za-z0-9_]+$",
                "description": "Original explicit business Workflow Session id, present only when the triggering read_file call supplied one."
            },
            "with_line_numbers": {"type": "boolean"}
        },
        "required": ["project", "path", "start_line", "limit"]
    })
}

fn read_range_continuation_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Actionable positional continuation for unread lines in one file. The cursor is deterministic for the observed file position, but it is not a frozen snapshot: compare the next call's full-file sha256 with source_sha256 before concatenating ranges as one unchanged source.",
        "properties": {
            "kind": {"type": "string", "const": "read_range"},
            "safe_cursor": {"type": "boolean", "const": true},
            "source_sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
            "snapshot_stable": {"type": "boolean", "const": false},
            "suggested_call": read_suggested_call_schema(
                "read_file",
                suggested_read_file_arguments_schema(),
            )
        },
        "required": [
            "kind", "safe_cursor", "source_sha256", "snapshot_stable", "suggested_call"
        ]
    })
}

fn suggested_read_files_arguments_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "project": schema_type("string", "Exact resolved Project id selected by the current batch; shorthand is never replayed by recovery."),
            "items": {
                "type": "array",
                "minItems": 1,
                "maxItems": 8,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": schema_type("string", "Original project-relative path."),
                        "start_line": schema_type("integer", "Original optional line offset, preserved exactly as accepted by read_files input normalization."),
                        "limit": schema_type("integer", "Original optional line limit, preserved exactly as accepted by read_files input normalization.")
                    },
                    "required": ["path"]
                }
            },
            "session_id": {
                "type": "string",
                "pattern": "^wc_sess_[A-Za-z0-9_]+$",
                "description": "Original explicit business Workflow Session id, present only when the triggering read_files call supplied one."
            },
            "with_line_numbers": {"type": "boolean"},
            "max_result_bytes": {
                "type": "integer",
                "minimum": webcodex_core::runtime_contract::MIN_READ_FILES_RESULT_BYTES,
                "maximum": webcodex_core::runtime_contract::MODEL_INSPECTION_MAX_RESULT_BYTES
            }
        },
        "required": ["project", "items"]
    })
}

fn read_batch_continuation_schema() -> Value {
    let suggested_call =
        read_suggested_call_schema("read_files", suggested_read_files_arguments_schema());
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "description": "Safe continuation for original batch items that were not returned. next_index is their original request index; suggested_call already slices the original items, so next_index is not passed as an input cursor.",
                "properties": {
                    "kind": {"type": "string", "const": "batch_items"},
                    "safe_cursor": {"type": "boolean", "const": true},
                    "next_index": {"type": "integer", "minimum": 0, "maximum": 7},
                    "recommended_order": {
                        "type": "string",
                        "enum": ["next", "after_partial_item"]
                    },
                    "suggested_call": suggested_call.clone()
                },
                "required": [
                    "kind", "safe_cursor", "next_index", "recommended_order", "suggested_call"
                ]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "description": "Budget refinement used only when the current primary result budget could not return any part of the first remaining item. This is not a cursor; the suggested budget is bounded by the explicit 512 KiB inspection hard cap.",
                "properties": {
                    "kind": {"type": "string", "const": "increase_result_budget"},
                    "safe_cursor": {"type": "boolean", "const": false},
                    "next_index": {"type": "integer", "const": 0},
                    "suggested_max_result_bytes": {
                        "type": "integer",
                        "const": webcodex_core::runtime_contract::MODEL_INSPECTION_MAX_RESULT_BYTES
                    },
                    "suggested_call": suggested_call
                },
                "required": [
                    "kind", "safe_cursor", "next_index", "suggested_max_result_bytes", "suggested_call"
                ]
            }
        ]
    })
}

fn read_file_output_schema() -> Value {
    let default_limit = webcodex_core::runtime_contract::FILE_READ_DEFAULT_LIMIT;
    let success_properties = json!({
        "text": schema_type("string", "The single primary text representation: plain content or numbered text according to format."),
        "format": {
            "type": "string",
            "enum": ["plain", "numbered"],
            "description": "Primary text format: plain or numbered. Complete default full-file model-facing success may omit plain; numbered remains explicit."
        },
        "path": schema_type("string", "Project-relative path."),
        "sha256": {
            "type": "string",
            "pattern": "^[0-9a-f]{64}$",
            "description": "sha256 of the complete file, independent of the returned line range."
        },
        "start_line": {"type": "integer", "minimum": 1},
        "limit": {"type": "integer", "minimum": 1, "maximum": 2000},
        "total_lines": {"type": "integer", "minimum": 0},
        "returned_lines": {
            "type": "integer",
            "minimum": 0,
            "maximum": 2000,
            "description": "Returned source-line count. Runtime cursor construction guarantees this does not exceed limit."
        },
        "end_line": {
            "anyOf": [
                {"type": "integer", "minimum": 1},
                {"type": "null"}
            ]
        },
        "has_more": {"type": "boolean"},
        "next_start_line": {
            "anyOf": [
                {"type": "integer", "minimum": 1},
                {"type": "null"}
            ]
        },
        "continuation": read_range_continuation_schema(),
        "session_hint": session_hint_schema(),
        "permission": permission_decision_schema()
    });
    let failure_properties = json!({
        "error_kind": {"type": "string", "const": "read_file_failed"},
        "reason_code": {
            "type": "string",
            "enum": [
                "invalid_path", "sensitive_path", "not_found", "not_file",
                "permission_denied", "invalid_utf8", "range_too_large",
                "agent_unavailable", "timeout", "malformed_agent_response", "io_error"
            ]
        },
        "path": schema_type("string", "Project-relative input path."),
        "state_changed": {"type": "boolean", "const": false},
        "session_hint": session_hint_schema(),
        "permission": permission_decision_schema()
    });
    let full_success_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": success_properties.clone(),
        "required": [
            "text", "format", "path", "sha256", "start_line", "limit",
            "total_lines", "returned_lines", "end_line", "has_more", "next_start_line"
        ]
    });
    let mut sparse_success_properties = success_properties
        .as_object()
        .expect("read_file success properties")
        .clone();
    for key in [
        "start_line",
        "limit",
        "returned_lines",
        "end_line",
        "has_more",
        "next_start_line",
        "continuation",
    ] {
        sparse_success_properties.remove(key);
    }
    sparse_success_properties.insert(
        "total_lines".to_string(),
        json!({
            "type": "integer",
            "minimum": 0,
            "maximum": default_limit,
            "description": "Complete-file line count; sparse default reads cannot exceed the canonical default line limit."
        }),
    );
    sparse_success_properties.insert(
        "format".to_string(),
        json!({
            "type": "string",
            "const": "numbered",
            "description": "Present only when the complete full-file sparse projection contains numbered text; omission means plain."
        }),
    );
    let sparse_success_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": sparse_success_properties,
        "required": ["text", "path", "sha256", "total_lines"],
        "description": "Sparse model-facing form for a provably complete default full-file read. Omitted range fields mean start_line=1, the canonical default limit, returned_lines=total_lines, and no continuation. sha256 and total_lines remain explicit."
    });
    let success_output = json!({
        "anyOf": [full_success_output, sparse_success_output]
    });
    let read_failure_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": failure_properties.clone(),
        "required": ["error_kind", "reason_code", "path", "state_changed"]
    });
    let failure_output = json!({
        "anyOf": [
            {"type": "null"},
            {
                "type": "object",
                "additionalProperties": true,
                "allOf": [
                    {
                        "if": {
                            "properties": {"error_kind": {"const": "read_file_failed"}},
                            "required": ["error_kind"]
                        },
                        "then": read_failure_output
                    }
                ]
            }
        ]
    });
    let mut discovery_properties = success_properties
        .as_object()
        .expect("read_file success properties")
        .clone();
    discovery_properties.extend(
        failure_properties
            .as_object()
            .expect("read_file failure properties")
            .clone(),
    );
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "success": {"type": "boolean"},
            "output": {
                "properties": discovery_properties,
                "anyOf": [
                    {"type": "object"},
                    {"type": "null"}
                ]
            },
            "error": {
                "anyOf": [
                    {"type": "string"},
                    {"type": "null"}
                ]
            }
        },
        "required": ["success", "output"],
        "allOf": [
            {
                "if": {
                    "properties": {"success": {"const": true}},
                    "required": ["success"]
                },
                "then": {
                    "properties": {
                        "output": success_output,
                        "error": {"type": "null"}
                    }
                },
                "else": {
                    "required": ["error"],
                    "properties": {
                        "output": failure_output,
                        "error": {"type": "string"}
                    }
                }
            }
        ]
    })
}

fn read_files_output_schema() -> Value {
    let default_limit = webcodex_core::runtime_contract::FILE_READ_DEFAULT_LIMIT;
    let read_success_properties = json!({
        "text": schema_type("string", "The single primary text representation."),
        "format": {"type": "string", "enum": ["plain", "numbered"]},
        "path": schema_type("string", "Project-relative path; omitted from a sparse complete item when identical to the outer item path."),
        "sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
        "start_line": {"type": "integer", "minimum": 1},
        "limit": {"type": "integer", "minimum": 1, "maximum": 2000},
        "total_lines": {"type": "integer", "minimum": 0},
        "returned_lines": {"type": "integer", "minimum": 0, "maximum": 2000},
        "end_line": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}]},
        "has_more": {"type": "boolean"},
        "next_start_line": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}]},
        "budget_truncated": {"type": "boolean", "const": true},
        "budget_next_limit": {"type": "integer", "minimum": 1, "maximum": 1999}
    });
    let read_success_full = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": read_success_properties.clone(),
        "required": [
            "text", "format", "path", "sha256", "start_line", "limit",
            "total_lines", "returned_lines", "end_line", "has_more", "next_start_line"
        ]
    });
    let mut read_success_sparse_properties = read_success_properties
        .as_object()
        .expect("read_files success properties")
        .clone();
    for key in [
        "path",
        "start_line",
        "limit",
        "returned_lines",
        "end_line",
        "has_more",
        "next_start_line",
        "budget_truncated",
        "budget_next_limit",
    ] {
        read_success_sparse_properties.remove(key);
    }
    read_success_sparse_properties.insert(
        "total_lines".to_string(),
        json!({
            "type": "integer",
            "minimum": 0,
            "maximum": default_limit,
            "description": "Complete-file line count; sparse default reads cannot exceed the canonical default line limit."
        }),
    );
    read_success_sparse_properties.insert(
        "format".to_string(),
        json!({
            "type": "string",
            "const": "numbered",
            "description": "Present only for numbered complete default full-file sparse output; omission means plain."
        }),
    );
    let read_success_sparse = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": read_success_sparse_properties,
        "required": ["text", "sha256", "total_lines"],
        "description": "Sparse model-facing item form for a provably complete default full-file read. The outer item path is the only navigation identity; inner path and range fields are omitted, and no continuation exists."
    });
    let read_success = json!({
        "anyOf": [read_success_full, read_success_sparse.clone()]
    });
    let read_failure = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "error_kind": {"type": "string", "const": "read_file_failed"},
            "reason_code": {
                "type": "string",
                "enum": [
                    "invalid_path", "sensitive_path", "not_found", "not_file",
                    "permission_denied", "invalid_utf8", "range_too_large",
                    "agent_unavailable", "timeout", "malformed_agent_response", "io_error"
                ]
            },
            "path": schema_type("string", "Project-relative input path."),
            "state_changed": {"type": "boolean", "const": false}
        },
        "required": ["error_kind", "reason_code", "path", "state_changed"]
    });
    let item_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "index": {"type": "integer", "minimum": 0, "maximum": 7},
            "path": schema_type("string", "Project-relative input path."),
            "success": {"type": "boolean"},
            "output": {"anyOf": [read_success.clone(), read_failure.clone()]},
            "error": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            "continuation": read_range_continuation_schema()
        },
        "required": ["index", "path", "success", "output", "error"],
        "allOf": [{
            "if": {"properties": {"success": {"const": true}}, "required": ["success"]},
            "then": {"properties": {"output": read_success, "error": {"type": "null"}}},
            "else": {"properties": {"output": read_failure, "error": {"type": "string"}}}
        }]
    });
    let batch_output_full = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "project": schema_type("string", "Resolved runtime project id."),
            "requested_count": {"type": "integer", "minimum": 1, "maximum": 8},
            "returned_count": {"type": "integer", "minimum": 0, "maximum": 8},
            "succeeded_count": {"type": "integer", "minimum": 0, "maximum": 8},
            "failed_count": {"type": "integer", "minimum": 0, "maximum": 8},
            "items": {"type": "array", "maxItems": 8, "items": item_schema},
            "output_truncated": {"type": "boolean"},
            "next_index": {"anyOf": [{"type": "integer", "minimum": 0, "maximum": 7}, {"type": "null"}]},
            "truncation_reason": {"type": "string", "enum": ["batch_response_budget", "hard_result_cap"]},
            "continuation": read_batch_continuation_schema(),
            "session_hint": session_hint_schema(),
            "permission": permission_decision_schema()
        },
        "required": [
            "project", "requested_count", "returned_count", "succeeded_count",
            "failed_count", "items", "output_truncated", "next_index"
        ]
    });
    let sparse_complete_item = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "index": {"type": "integer", "minimum": 0, "maximum": 7},
            "path": schema_type("string", "Project-relative input path."),
            "success": {"type": "boolean", "const": true},
            "output": read_success_sparse,
            "error": {"type": "null"}
        },
        "required": ["index", "path", "success", "output", "error"]
    });
    let batch_output_sparse = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {"type": "array", "minItems": 1, "maxItems": 8, "items": sparse_complete_item},
            "session_hint": session_hint_schema(),
            "permission": permission_decision_schema()
        },
        "required": ["items"],
        "description": "Sparse model-facing batch form used only when every requested item succeeded as a complete default full-file read and the batch itself was not truncated. Omitted outer counts/defaults are therefore implied."
    });
    let batch_output = json!({
        "anyOf": [batch_output_full, batch_output_sparse]
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "success": {"type": "boolean"},
            "output": {"anyOf": [batch_output.clone(), {"type": "object", "additionalProperties": true}, {"type": "null"}]},
            "error": {"anyOf": [{"type": "string"}, {"type": "null"}]}
        },
        "required": ["success", "output"],
        "allOf": [{
            "if": {"properties": {"success": {"const": true}}, "required": ["success"]},
            "then": {"properties": {"output": batch_output, "error": {"type": "null"}}},
            "else": {"required": ["error"], "properties": {"error": {"type": "string"}}}
        }]
    })
}

pub(super) fn project_type_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": schema_type("string", "Stable project type identifier."),
            "evidence": array_schema(schema_type("string", "Project-relative evidence path."), "Sorted evidence paths."),
            "evidence_total": schema_type("integer", "Real evidence path count before bounding."),
            "evidence_truncated": schema_type("boolean", "True when evidence was capped."),
        },
        "required": ["kind", "evidence"],
        "additionalProperties": false,
    })
}

pub(super) fn path_kind_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "path": schema_type("string", "Project-relative path."),
            "kind": schema_type("string", "Stable classification."),
        },
        "required": ["path", "kind"],
        "additionalProperties": false,
    })
}

pub(super) fn key_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": schema_type("string", "Project-relative key-file path."),
            "kind": schema_type("string", "Stable key-file classification."),
            "reason": schema_type("string", "Deterministic classification reason."),
        },
        "required": ["path", "kind", "reason"],
        "additionalProperties": false,
    })
}

pub(super) fn roots_schema() -> Value {
    let paths = || {
        array_schema(
            schema_type("string", "Project-relative conventional root."),
            "Sorted conventional roots.",
        )
    };
    json!({
        "type": "object",
        "properties": {
            "source": paths(),
            "tests": paths(),
            "docs": paths(),
            "examples": paths(),
            "scripts": paths(),
            "ci": paths(),
            "classification_basis": schema_type("string", "Classification basis; conventional_directory_name."),
        },
        "required": ["source", "tests", "docs", "examples", "scripts", "ci", "classification_basis"],
        "additionalProperties": false,
    })
}

pub(super) fn top_level_entry_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": schema_type("string", "Project-relative direct-child path."),
            "kind": {"type": "string", "enum": ["file", "directory"]},
        },
        "required": ["path", "kind"],
        "additionalProperties": false,
    })
}

pub(super) fn suggested_read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": schema_type("string", "Project-relative path for a later read_file call."),
            "reason": schema_type("string", "Deterministic recommendation reason."),
        },
        "required": ["path", "reason"],
        "additionalProperties": false,
    })
}

pub(super) fn scan_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "max_depth": schema_type("integer", "Effective clamped maximum depth."),
            "limit": schema_type("integer", "Effective clamped entry limit."),
            "returned_entry_count": schema_type("integer", "Number of safe scanned entries used to construct the overview."),
            "truncated": schema_type("boolean", "Whether limit or depth bounded the scan."),
            "truncation_reason": nullable_schema("string", "limit, max_depth, limit_and_max_depth, or null."),
        },
        "required": ["max_depth", "limit", "returned_entry_count", "truncated", "truncation_reason"],
        "additionalProperties": false,
    })
}

fn search_file_result_schema() -> Value {
    json!({
        "type": "object",
        "description": "Unique project-relative matching file, with match_count in count mode.",
        "properties": {
            "path": schema_type("string", "Project-relative file path."),
            "match_count": schema_type("integer", "Matching-line count for this file in count mode."),
        },
        "required": ["path"],
        "additionalProperties": false,
    })
}

fn tracked_list_entry_schema() -> Value {
    json!({
        "type": "object",
        "description": "A tracked file, or a directory standing in for the files rolled up beneath it.",
        "properties": {
            "path": schema_type("string", "Project-relative path; rolled-up directories keep a trailing slash."),
            "kind": {
                "type": "string",
                "enum": ["file", "dir"],
                "description": "Entry kind."
            },
            "file_count": schema_type(
                "integer",
                "Tracked files beneath a rolled-up directory; absent for files.",
            ),
        },
        "required": ["path", "kind"],
        "additionalProperties": false
    })
}

fn file_list_entry_schema() -> Value {
    json!({
        "type": "object",
        "description": "One bounded file-list entry.",
        "properties": {
            "path": schema_type("string", "Project-relative file or directory path."),
            "kind": {
                "type": "string",
                "enum": ["file", "dir"],
                "description": "Entry kind."
            }
        },
        "required": ["path", "kind"],
        "additionalProperties": true
    })
}
