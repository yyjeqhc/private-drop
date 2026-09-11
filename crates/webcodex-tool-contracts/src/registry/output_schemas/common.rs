use serde_json::{json, Value};

use webcodex_core::runtime_contract::{RECOVERY_KIND_VALUES, RECOVERY_TOOL_VALUES};
use webcodex_core::workflow_session_contract::{
    SESSION_INBOX_HIGH_GUIDANCE_ATTENTION_INSTRUCTION, SESSION_INBOX_HIGH_GUIDANCE_ATTENTION_REASON,
};

use crate::tool_definition::exploration_tool_names;

pub fn schema_type(kind: &str, description: &str) -> Value {
    json!({
        "type": kind,
        "description": description,
    })
}

pub fn nullable_schema(kind: &str, description: &str) -> Value {
    json!({
        "anyOf": [
            { "type": kind },
            { "type": "null" }
        ],
        "description": description,
    })
}

pub fn job_activity_schema() -> Value {
    json!({
        "anyOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "state": {"type": "string", "enum": ["working", "waiting"]},
                    "phase": {
                        "type": "string",
                        "enum": [
                            "process_running",
                            "validation_format",
                            "validation_check",
                            "validation_test",
                            "cargo_waiting_for_build_lock",
                            "cargo_compiling",
                            "cargo_checking"
                        ]
                    },
                    "source": {
                        "type": "string",
                        "enum": ["runner_execution", "validation_plan", "cargo_output"]
                    }
                },
                "required": ["state", "phase", "source"]
            },
            {"type": "null"}
        ],
        "description": "Runner-owned bounded current activity for an active Job. Observation only: it never replaces canonical status, proves completion, or grants retry/continuation authority. null means unavailable, terminal, or temporarily untrusted during recovery."
    })
}

pub fn exploration_tool_name_schema() -> Value {
    let names = exploration_tool_names().collect::<Vec<_>>();
    json!({
        "anyOf": [
            {
                "type": "string",
                "enum": names
            },
            {"type": "null"}
        ]
    })
}

pub fn array_schema(items: Value, description: &str) -> Value {
    json!({
        "type": "array",
        "items": items,
        "description": description,
    })
}

pub fn open_object_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": true,
    })
}

pub fn task_outcome_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["pass", "warn", "fail"]
            },
            "blocking": schema_type("boolean", "True only when the final task outcome is fail."),
            "blocking_reasons": array_schema(schema_type("string", "Task blocker reason identifier."), "Bounded task blocker reasons."),
            "warning_reasons": array_schema(schema_type("string", "Task warning reason identifier."), "Bounded task-only warning reasons.")
        },
        "required": ["status", "blocking", "blocking_reasons", "warning_reasons"]
    })
}

pub fn evidence_history_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["clean", "mixed_resolved", "mixed_unresolved", "failed"]
            }
        },
        "required": ["status"]
    })
}

pub fn evidence_integrity_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["clean", "warning", "error"]
            },
            "error_reasons": array_schema(schema_type("string", "Evidence integrity error reason identifier."), "Bounded integrity error reasons."),
            "warning_reasons": array_schema(schema_type("string", "Evidence integrity warning reason identifier."), "Bounded integrity warning reasons.")
        },
        "required": ["status", "error_reasons", "warning_reasons"]
    })
}

pub fn authority_profile_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": false,
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["trusted_agent", "restricted", "invalid"],
                "description": "Canonical authority mode. trusted_agent auto-authorizes consequential tools after hard safety checks; restricted requires human authorization; invalid means the configuration failed to resolve (fail closed)."
            },
            "source": {
                "type": "string",
                "description": "Where the resolved mode came from (default, env:WEBCODEX_AUTHORITY_MODE, migrated_env:WEBCODEX_PERMISSION_MODE, rejected_legacy_env:WEBCODEX_PERMISSION_MODE)."
            },
            "project_write": {
                "type": "boolean",
                "description": "Project file writes execute without human approval."
            },
            "shell": {
                "type": "boolean",
                "description": "Shell and async jobs execute without human approval."
            },
            "git": {
                "type": "boolean",
                "description": "Project git operations execute without human approval."
            },
            "network": {
                "type": "boolean",
                "description": "Network-using project commands execute without human approval."
            },
            "package_install": {
                "type": "boolean",
                "description": "Dependency installation executes without human approval."
            },
            "service_control": {
                "type": "boolean",
                "description": "Local service control executes without human approval."
            },
            "release": {
                "type": "string",
                "enum": ["user_task_scoped", "human_approval"],
                "description": "External release actions: user_task_scoped executes when the user task explicitly includes the action and target; human_approval requires an operator decision."
            },
            "human_approval_required": {
                "type": "boolean",
                "description": "True when consequential tools require a human authorization step."
            }
        },
        "required": [
            "mode",
            "source",
            "project_write",
            "shell",
            "git",
            "network",
            "package_install",
            "service_control",
            "release",
            "human_approval_required"
        ]
    })
}

pub fn permission_summary_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": true,
        "properties": {
            "policy": schema_type("string", "Effective permission policy."),
            "events_total": schema_type("integer", "Permission-bearing ledger events counted."),
            "required_count": schema_type("integer", "Permission decisions that required approval handling."),
            "manual_approved_count": schema_type("integer", "Manually approved decisions."),
            "auto_approved_count": schema_type("integer", "Automatically approved decisions."),
            "total_approved_count": schema_type("integer", "manual_approved_count plus auto_approved_count."),
            "denied_count": schema_type("integer", "Denied or expired decisions."),
            "pending_count": schema_type("integer", "Pending approval decisions."),
            "hard_denied_count": schema_type("integer", "Hard-denied decisions after safety guards."),
            "human_approval_required": schema_type("boolean", "Whether the active profile requires human approval."),
            "recent": array_schema(open_object_schema("Bounded recent permission decision."), "Newest-first bounded permission decisions.")
        }
    })
}

pub fn job_lifecycle_summary_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": true,
        "properties": {
            "active_count": schema_type("integer", "Compatibility broad active count: blocking active plus nonblocking terminal-pending jobs."),
            "running_count": schema_type("integer", "Blocking running-like jobs: queued, running, started, or agent_queued."),
            "stop_requested_count": schema_type("integer", "Jobs with status stop_requested."),
            "terminal_pending_count": schema_type("integer", "Nonblocking active jobs waiting for terminal status."),
            "blocking_active_count": schema_type("integer", "Jobs that should block finish/handoff closeout."),
            "nonblocking_active_count": schema_type("integer", "Active but nonblocking jobs, currently stop_requested."),
            "recent": array_schema(open_object_schema("Bounded recent job metadata; never stdout/stderr or command text."), "Bounded recent active job metadata."),
            "recent_limit": schema_type("integer", "Maximum recent jobs returned."),
            "truncated": schema_type("boolean", "True when more active jobs existed than recent_limit."),
            "warnings": array_schema(open_object_schema("Job lifecycle warning; active_jobs_present has blocking=true and jobs_terminal_pending has blocking=false."), "Bounded lifecycle warnings.")
        }
    })
}

pub(super) fn permission_decision_schema() -> Value {
    open_object_schema("Permission decision metadata for high-risk tools after hard safety checks pass. Never includes stdout, stderr, env, tokens, secrets, or raw input content.")
}

pub fn search_context_line_schema() -> Value {
    json!({
        "type": "object",
        "description": "A context line adjacent to a search match.",
        "properties": {
            "line": {
                "type": "integer",
                "description": "1-based line number."
            },
            "text": {
                "type": "string",
                "description": "Line text."
            }
        },
        "required": ["line", "text"],
        "additionalProperties": true
    })
}

pub fn search_match_schema() -> Value {
    let context_lines = array_schema(search_context_line_schema(), "Context lines.");
    let read_hint = json!({
        "type": "object",
        "description": "Ready-to-use bounded expansion range around this match. Reuse the outer match path and the same project; canonical results also repeat path here, while sparse model-facing results omit that duplicate path. This hint performs no read and contains no additional file content.",
        "properties": {
            "path": {
                "type": "string",
                "description": "Same trusted project-relative file path as the match; omitted from sparse model-facing results when identical to the outer match path."
            },
            "start_line": {
                "type": "integer",
                "minimum": 1,
                "description": "Deterministic 1-based expansion start, up to 20 lines before the match."
            },
            "limit": {
                "type": "integer",
                "const": 80,
                "description": "Deterministic bounded line count for read_files expansion."
            }
        },
        "required": ["start_line", "limit"],
        "additionalProperties": false
    });
    json!({
        "type": "object",
        "description": "Search match with path, 1-based line, preview, optional requested context lines, and deterministic search-to-read continuation metadata. Canonical results include both context arrays even when empty; sparse model-facing results omit empty arrays only.",
        "properties": {
            "path": {
                "type": "string",
                "description": "Project-relative file path."
            },
            "line": {
                "type": "integer",
                "description": "1-based match line number."
            },
            "preview": {
                "type": "string",
                "description": "Matched line preview."
            },
            "context_before": context_lines.clone(),
            "context_after": context_lines,
            "read_hint": read_hint,
        },
        "required": ["path", "line", "preview", "read_hint"],
        "additionalProperties": true
    })
}

pub(super) fn session_hint_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional lightweight hint that the recorder session has open guidance, question, todo, or risk messages. Counts only; never includes message text.",
        "properties": {
            "has_open_messages": {
                "type": "boolean",
                "description": "True when any counted open session-local message exists."
            },
            "open_counts": {
                "type": "object",
                "description": "Open message counts by counted kind.",
                "properties": {
                    "guidance": { "type": "integer", "minimum": 0 },
                    "question": { "type": "integer", "minimum": 0 },
                    "todo": { "type": "integer", "minimum": 0 },
                    "risk": { "type": "integer", "minimum": 0 }
                },
                "required": ["guidance", "question", "todo", "risk"],
                "additionalProperties": false
            },
            "highest_priority": {
                "type": "string",
                "enum": ["low", "normal", "high"],
                "description": "Highest priority among counted open messages."
            },
            "attention_required": {
                "type": "boolean",
                "const": true,
                "description": "Counts-only fallback marker for open high-priority guidance requiring model-context acknowledgement; may be omitted when the same response already fully projects or ACK-suppresses the urgent guidance set."
            },
            "attention_reason": {
                "type": "string",
                "enum": [SESSION_INBOX_HIGH_GUIDANCE_ATTENTION_REASON],
                "description": "Stable reason for the strong counts-only attention fallback; omitted for ordinary hints and when the same response already fully covers the urgent guidance set."
            },
            "attention_instruction": {
                "type": "string",
                "enum": [SESSION_INBOX_HIGH_GUIDANCE_ATTENTION_INSTRUCTION],
                "description": "Short fixed counts-only fallback instruction; never contains Session message body text and may be omitted when session_attention already fully covers the urgent guidance set."
            },
            "suggested_next_tool": {
                "type": "string",
                "enum": ["session_discussion_summary"],
                "description": "Tool to call when the model needs the bounded message details."
            }
        },
        "required": [
            "has_open_messages",
            "open_counts",
            "highest_priority",
            "suggested_next_tool"
        ],
        "additionalProperties": false
    })
}

pub fn recovery_kind_schema() -> Value {
    json!({
        "type": "string",
        "enum": RECOVERY_KIND_VALUES,
        "description": "Closed model-facing class of the next safe recovery action. retry_same means exact idempotent replay only; outcome_unknown is never ordinary retry authority."
    })
}

pub fn recovery_tool_schema() -> Value {
    json!({
        "type": "string",
        "enum": RECOVERY_TOOL_VALUES,
        "description": "Optional bounded public WebCodex tool to use for the declared reobserve or reconcile action. This field never grants authority or triggers execution."
    })
}

pub fn wrapped_output_schema(output_properties: Vec<(&str, Value)>) -> Value {
    let mut output_properties = output_properties;
    output_properties.extend([
        (
            "trace_ref",
            schema_type(
                "string",
                "Opaque operator diagnostic reference emitted only on eligible failed calls while full tracing is enabled. Read with read_tool_trace; on Adaptive Runtime invoke that target through call_runtime_tool. Never a native path.",
            ),
        ),
        ("session_hint", session_hint_schema()),
        ("permission", permission_decision_schema()),
        ("recovery_kind", recovery_kind_schema()),
        ("recovery_tool", recovery_tool_schema()),
    ]);
    let properties = output_properties
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "properties": {
            "success": { "type": "boolean" },
            "output": {
                "type": "object",
                "properties": properties,
                "additionalProperties": true
            },
            "error": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "null" }
                ]
            }
        },
        "required": ["success"],
        "additionalProperties": true,
        "allOf": [
            {
                "if": {
                    "properties": {"success": {"const": true}},
                    "required": ["success"]
                },
                "then": {
                    "properties": {
                        "output": {
                            "properties": {
                                "recovery_kind": {
                                    "type": "null",
                                    "const": "__forbidden_on_success__"
                                },
                                "recovery_tool": {
                                    "type": "null",
                                    "const": "__forbidden_on_success__"
                                }
                            }
                        }
                    }
                }
            },
            {
                "if": {
                    "properties": {
                        "output": {"required": ["recovery_tool"]}
                    },
                    "required": ["output"]
                },
                "then": {
                    "properties": {
                        "output": {
                            "required": ["recovery_kind"],
                            "properties": {
                                "recovery_kind": {"enum": ["reobserve", "reconcile"]}
                            }
                        }
                    }
                }
            }
        ]
    })
}

pub fn default_output_schema() -> Value {
    wrapped_output_schema(vec![])
}

pub fn cargo_test_count_assertion_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "minimum_tests": {
                "type": "integer",
                "minimum": 1,
                "maximum": webcodex_core::runner_protocol::CARGO_TEST_MIN_TESTS_MAX,
                "description": "Effective caller-requested minimum after combining require_tests and min_tests."
            },
            "actual_tests_run": {
                "anyOf": [
                    {"type": "integer", "minimum": 0},
                    {"type": "null"}
                ],
                "description": "Proven executed test count, or null when complete count evidence was unavailable."
            },
            "status": {
                "type": "string",
                "enum": ["passed", "failed", "unproven"]
            },
            "reason_code": {
                "type": "string",
                "enum": ["minimum_satisfied", "minimum_not_met", "test_count_unproven"]
            },
            "evidence_reason_code": {
                "type": "string",
                "enum": ["complete_summary", "output_truncated", "partial_harness_summary", "no_complete_summary"],
                "description": "Why executed-test count evidence was proven or remained unavailable; this refines evidence diagnostics without changing the assertion verdict."
            }
        },
        "required": ["minimum_tests", "actual_tests_run", "status", "reason_code", "evidence_reason_code"]
    })
}

/// Deterministic continuation-feedback projection: an attempt summary plus a
/// validation delta over comparable prior evidence. Always bounded and
/// read-only; never an LLM summary, never a new verdict. Core sub-objects use
/// strict `additionalProperties: false` schemas so field drift fails loudly.
pub fn continuation_feedback_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["available", "not_applicable", "unknown"],
                "description": "available when an attempt summary was derived; not_applicable for a fresh empty session."
            },
            "reason_code": nullable_schema("string", "Reason code when status is not available/not_applicable; null otherwise."),
            "deterministic": schema_type("boolean", "True: the projection is derived only from existing persistent state."),
            "llm_summary": schema_type("boolean", "Always false; never an LLM-generated summary."),
            "attempt": attempt_summary_schema(),
            "validation_delta": validation_delta_schema("Deterministic diff between the latest validation evidence and the most recent prior comparable evidence. unavailable with a stable reason code when the two runs are not proven comparable; never a new pass/fail verdict.")
        },
        "required": ["status", "deterministic", "llm_summary"],
        "allOf": [
            {
                "if": {
                    "properties": {
                        "status": {"const": "available"}
                    },
                    "required": ["status"]
                },
                "then": {
                    "required": ["attempt", "validation_delta"]
                }
            }
        ]
    })
}

/// Strict compact handoff projection shared by `session_handoff_summary` and
/// `finish_coding_task`.
pub fn handoff_brief_schema(description: &str) -> Value {
    fn nullable_with(schema: Value) -> Value {
        json!({
            "anyOf": [
                schema,
                {"type": "null"}
            ]
        })
    }

    fn instruction_schema(description: &str) -> Value {
        json!({
            "type": "object",
            "description": description,
            "additionalProperties": false,
            "properties": {
                "excerpt": nullable_with(json!({
                    "type": "string",
                    "maxLength": 600,
                    "description": "Redacted bounded task instruction excerpt."
                })),
                "truncated": schema_type("boolean", "True when credential redaction, the 600-character limit, or the final serialized byte budget changed the returned excerpt.")
            },
            "required": ["excerpt", "truncated"]
        })
    }

    fn bounded_string_list_schema(max_items: usize, max_length: usize, description: &str) -> Value {
        json!({
            "type": "object",
            "description": description,
            "additionalProperties": false,
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": max_items,
                    "uniqueItems": true,
                    "items": {
                        "type": "string",
                        "maxLength": max_length
                    }
                },
                "total": {"type": "integer", "minimum": 0},
                "returned": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": max_items
                },
                "truncated": schema_type("boolean", "True when count, safety, or byte-budget bounds omitted items.")
            },
            "required": ["items", "total", "returned", "truncated"]
        })
    }

    let workspace_reason = nullable_with(json!({
        "type": "string",
        "enum": ["workspace_not_requested", "workspace_unavailable"]
    }));
    let validation_reason = nullable_with(json!({
        "type": "string",
        "enum": ["validation_not_requested", "validation_unavailable"]
    }));
    let nullable_bool = || nullable_with(json!({"type": "boolean"}));
    let nullable_count = || {
        nullable_with(json!({
            "type": "integer",
            "minimum": 0
        }))
    };

    json!({
        "type": "object",
        "description": description,
        "additionalProperties": false,
        "properties": {
            "version": {
                "type": "integer",
                "const": 1
            },
            "session": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "session_id": {
                        "type": "string",
                        "maxLength": 128
                    },
                    "lifecycle": {
                        "type": "string",
                        "enum": ["active", "closed"]
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["normal", "read_only"]
                    }
                },
                "required": ["session_id", "lifecycle", "mode"]
            },
            "task": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "root_instruction": instruction_schema("The Workflow Session root instruction. It remains separate from later accepted task instructions."),
                    "latest_instruction": instruction_schema("The latest retained task_instruction event. The same excerpt is returned when it equals the root instruction.")
                },
                "required": ["root_instruction", "latest_instruction"]
            },
            "workspace": {
                "type": "object",
                "description": "Workspace facts from the caller's already-obtained projection only. No implicit Git query is performed.",
                "additionalProperties": false,
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["available", "not_requested", "unavailable"]
                    },
                    "reason_code": workspace_reason,
                    "branch": nullable_with(json!({
                        "type": "string",
                        "maxLength": 256
                    })),
                    "head": nullable_with(json!({
                        "type": "string",
                        "pattern": "^[0-9a-f]{7,64}$"
                    })),
                    "dirty": nullable_bool(),
                    "conflicted": nullable_bool(),
                    "ahead": nullable_count(),
                    "behind": nullable_count()
                },
                "required": [
                    "status", "reason_code", "branch", "head", "dirty",
                    "conflicted", "ahead", "behind"
                ]
            },
            "progress": {
                "type": "object",
                "description": "Proven progress facts only; never a percentage, completion estimate, or merge verdict.",
                "additionalProperties": false,
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": [
                            "blocked",
                            "needs_validation",
                            "ready_to_continue",
                            "closed",
                            "insufficient_evidence"
                        ]
                    },
                    "meaningful_tool_calls": {
                        "type": "integer",
                        "minimum": 0
                    },
                    "changes": bounded_string_list_schema(
                        12,
                        512,
                        "Attempt changed paths from continuation feedback."
                    ),
                    "recent_files": bounded_string_list_schema(
                        8,
                        512,
                        "Recent relevant exploration paths, newest observation first. This is a continuity hint, not complete history."
                    )
                },
                "required": [
                    "state", "meaningful_tool_calls", "changes", "recent_files"
                ]
            },
            "validation": {
                "type": "object",
                "description": "Latest provable validation state projected from existing ledger evidence; never raw commands, output, or diagnostics bodies.",
                "additionalProperties": false,
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": [
                            "passed",
                            "failed",
                            "inconclusive",
                            "stale",
                            "not_run",
                            "not_requested",
                            "unavailable"
                        ]
                    },
                    "open_failures": bounded_string_list_schema(
                        5,
                        240,
                        "Bounded stable open failure identities only."
                    ),
                    "reason_code": validation_reason
                },
                "required": ["status", "open_failures", "reason_code"]
            },
            "attention": {
                "type": "object",
                "description": "Proven workspace, Job, and open guidance counts. Null means the corresponding evidence was unavailable.",
                "additionalProperties": false,
                "properties": {
                    "workspace_conflict": nullable_bool(),
                    "blocking_jobs": nullable_count(),
                    "terminal_pending_jobs": nullable_count(),
                    "recovering_jobs": nullable_count(),
                    "open_guidance": nullable_count(),
                    "open_risks": nullable_count(),
                    "open_questions": nullable_count(),
                    "open_todos": nullable_count()
                },
                "required": [
                    "workspace_conflict", "blocking_jobs",
                    "terminal_pending_jobs", "recovering_jobs", "open_guidance",
                    "open_risks", "open_questions", "open_todos"
                ]
            },
            "next_actions": {
                "type": "array",
                "maxItems": 5,
                "uniqueItems": true,
                "items": {
                    "type": "string",
                    "maxLength": 160
                },
                "description": "Fixed deterministic action templates in priority order."
            },
            "basis": {
                "type": "object",
                "description": "Whether all requested evidence needed to describe the retained attempt was available.",
                "additionalProperties": false,
                "properties": {
                    "complete": schema_type("boolean", "True only when no fixed evidence-gap reason applies."),
                    "reason_codes": {
                        "type": "array",
                        "maxItems": 8,
                        "uniqueItems": true,
                        "items": {
                            "type": "string",
                            "enum": [
                                "attempt_boundary_evicted",
                                "continuation_unavailable",
                                "guidance_unavailable",
                                "job_summary_unavailable",
                                "validation_not_requested",
                                "validation_unavailable",
                                "workspace_not_requested",
                                "workspace_unavailable"
                            ]
                        }
                    }
                },
                "required": ["complete", "reason_codes"]
            },
            "deterministic": {
                "type": "boolean",
                "const": true
            },
            "llm_summary": {
                "type": "boolean",
                "const": false
            }
        },
        "required": [
            "version", "session", "task", "workspace", "progress",
            "validation", "attention", "next_actions", "basis",
            "deterministic", "llm_summary"
        ]
    })
}

/// Strict schema for the deterministic attempt summary.
fn attempt_summary_schema() -> Value {
    json!({
        "type": "object",
        "description": "Bounded deterministic summary of the current attempt: boundary, instruction, event range, activity, changes, exploration, validation, jobs, guidance, outcome, and suggested next actions. Pointer fields only; never raw search text, file/LSP bodies, commands, stdout/stderr, full guidance text, absolute roots, or secrets.",
        "additionalProperties": false,
        "properties": {
            "boundary": attempt_boundary_schema(),
            "instruction": attempt_instruction_schema(),
            "event_range": attempt_event_range_schema(),
            "activity": attempt_activity_schema(),
            "changes": attempt_changes_schema(),
            "exploration": attempt_exploration_schema(),
            "validation": attempt_validation_schema(),
            "jobs": attempt_jobs_schema(),
            "guidance": attempt_guidance_schema(),
            "outcome": attempt_outcome_schema(),
            "suggested_next_actions": array_schema(schema_type("string", "A bounded, deterministic suggested next action."), "Bounded suggested next actions (<=8).")
        },
        "required": ["boundary", "instruction", "event_range", "activity", "changes", "exploration", "validation", "jobs", "guidance", "outcome", "suggested_next_actions"]
    })
}

fn attempt_boundary_schema() -> Value {
    json!({
        "type": "object",
        "description": "How the attempt boundary was determined.",
        "additionalProperties": false,
        "properties": {
            "source": {
                "type": "string",
                "enum": ["task_instruction", "session_start", "unavailable", "no_events"],
                "description": "task_instruction when the last accepted instruction event was retained; session_start when no instruction exists and nothing was evicted; unavailable when the window truncated and the instruction is gone; no_events for an empty session."
            },
            "reason_code": nullable_schema("string", "attempt_boundary_evicted when source is unavailable; null otherwise."),
            "event_id": nullable_schema("string", "Event id of the boundary event, when present."),
            "event_index": schema_type("integer", "0-based position of the boundary event within the summarized events.")
        },
        "required": ["source", "event_index"]
    })
}

fn attempt_instruction_schema() -> Value {
    json!({
        "type": "object",
        "description": "The instruction that defines the current attempt, if observed.",
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["available", "not_observed"]
            },
            "excerpt": nullable_schema("string", "Bounded, redacted excerpt of the previous attempt instruction."),
            "truncated": schema_type("boolean", "True when the persisted instruction exceeded the excerpt bound."),
            "recorded_at": nullable_schema("integer", "Unix timestamp when the instruction was recorded."),
            "requested_mode": nullable_schema("string", "Mode requested with the instruction, if any."),
            "effective_mode": nullable_schema("string", "Effective mode after applying the instruction, if any."),
            "capability_changed": nullable_schema("boolean", "Whether capability changed with the instruction."),
            "resumed": nullable_schema("boolean", "True when the instruction explicitly resumed an existing session.")
        },
        "required": ["status", "truncated"]
    })
}

fn attempt_event_range_schema() -> Value {
    json!({
        "type": "object",
        "description": "Bounded event range covered by the attempt.",
        "additionalProperties": false,
        "properties": {
            "start_event_id": nullable_schema("string", "Event id at the start of the attempt, when retained."),
            "end_event_id": nullable_schema("string", "Event id at the end of the attempt, when retained."),
            "start_sequence": schema_type("integer", "0-based start sequence within the summarized events."),
            "end_sequence": schema_type("integer", "0-based end sequence within the summarized events."),
            "event_count": schema_type("integer", "Number of events in the attempt window."),
            "complete": schema_type("boolean", "False when the retained window was truncated and the boundary is unavailable.")
        },
        "required": ["start_sequence", "end_sequence", "event_count", "complete"]
    })
}

fn attempt_activity_schema() -> Value {
    json!({
        "type": "object",
        "description": "Meaningful tool-call activity within the attempt window.",
        "additionalProperties": false,
        "properties": {
            "meaningful_tool_calls": schema_type("integer", "Count of meaningful (status/manifest-excluding) tool calls."),
            "successful_tool_calls": schema_type("integer", "Succeeded meaningful tool calls."),
            "failed_tool_calls": schema_type("integer", "Immutable raw failed meaningful ToolCall count, including expected/resolved/non-actionable history."),
            "actionable_failed_tool_calls": schema_type("integer", "Failed meaningful ToolCalls that the canonical closeout projection still considers actionable for this attempt."),
            "expected_failures": schema_type("integer", "Expected failure tool calls."),
            "resolved_failures": schema_type("integer", "Validation failures resolved by the attempt."),
            "unresolved_failures": schema_type("integer", "Validation failures still unresolved.")
        },
        "required": ["meaningful_tool_calls", "successful_tool_calls", "failed_tool_calls", "actionable_failed_tool_calls", "expected_failures", "resolved_failures", "unresolved_failures"]
    })
}

fn attempt_changes_schema() -> Value {
    json!({
        "type": "object",
        "description": "Deduped, bounded changed paths within the attempt window.",
        "additionalProperties": false,
        "properties": {
            "changed_paths": array_schema(schema_type("string", "A project-relative changed path."), "Bounded deduped changed paths (<=100), deterministic sorted order."),
            "total_changed_paths": schema_type("integer", "Real deduped changed-path count over the attempt window."),
            "truncated": schema_type("boolean", "True when changed_paths was capped at the bound.")
        },
        "required": ["changed_paths", "total_changed_paths", "truncated"]
    })
}

fn attempt_exploration_schema() -> Value {
    json!({
        "type": "object",
        "description": "Attempt-scoped exploration workset projected only from successful structured ledger evidence. Paths are validated project-relative values in newest-observation-first order; no search text, file/LSP content, commands, output, absolute roots, or secrets.",
        "additionalProperties": false,
        "properties": {
            "observed_paths": {
                "type": "array",
                "maxItems": 100,
                "uniqueItems": true,
                "items": {"type": "string", "maxLength": 512},
                "description": "Unique project-relative paths, newest successful observation first."
            },
            "total_observed_paths": schema_type("integer", "Real unique path count before the 100-path projection cap."),
            "truncated": schema_type("boolean", "True when observed_paths was capped."),
            "read_count": schema_type("integer", "Successful read_files calls in the attempt."),
            "search_count": schema_type("integer", "Successful search_project_texts calls in the attempt."),
            "navigation_count": schema_type("integer", "Successful LSP navigation calls in the attempt."),
            "latest_tool": exploration_tool_name_schema(),
            "complete": schema_type("boolean", "False when the attempt boundary was evicted and only a retained tail is available.")
        },
        "required": [
            "observed_paths",
            "total_observed_paths",
            "truncated",
            "read_count",
            "search_count",
            "navigation_count",
            "latest_tool",
            "complete"
        ]
    })
}

fn attempt_validation_schema() -> Value {
    json!({
        "type": "object",
        "description": "Current-attempt validation evidence after the latest trusted material workspace-content change. Historical validation remains separate.",
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["passed", "failed", "inconclusive", "stale", "not_run", "unknown"]
            },
            "latest_status": {
                "type": "string",
                "enum": ["passed", "failed", "inconclusive", "not_run", "unknown", "unavailable"]
            },
            "latest_kind": nullable_schema("string", "Validation kind of the latest run, when present."),
            "latest_at": nullable_schema("integer", "Unix timestamp of the latest run, when present."),
            "unresolved_failure_count": schema_type("integer", "Unresolved failure event count from this attempt."),
            "evidence_gap_event_count": schema_type("integer", "Inconclusive/request-scoped validation evidence events retained in the current post-mutation window. These are process evidence, not persistent task requirements."),
            "validation_events": schema_type("integer", "Validation event count in the current evidence window."),
            "stale_failure_count": schema_type("integer", "Failure events from this attempt that predate the latest trusted material workspace-content change."),
            "open_failures": array_schema(failure_identity_schema(), "Bounded stable identities for currently unresolved failures in this attempt."),
            "total_open_failures": schema_type("integer", "Total unresolved failure identities before bounding."),
            "failures_truncated": schema_type("boolean", "True when open_failures was capped at the bound."),
            "delta_available": schema_type("boolean", "Whether the validation delta is comparable."),
            "delta_reason_code": nullable_schema("string", "Reason code when the delta is not available; null otherwise.")
        },
        "required": ["status", "latest_status", "unresolved_failure_count", "evidence_gap_event_count", "validation_events", "stale_failure_count", "open_failures", "total_open_failures", "failures_truncated", "delta_available"]
    })
}

fn attempt_jobs_schema() -> Value {
    json!({
        "type": "object",
        "description": "Proven job lifecycle fields from the bounded active job aggregate; counts never depend on the truncated recent list.",
        "additionalProperties": false,
        "properties": {
            "active_count": schema_type("integer", "Total active jobs (blocking + nonblocking) from the bounded aggregate."),
            "running_count": schema_type("integer", "Blocking-active jobs (queued/running/started/agent_queued/recovering)."),
            "recovering_count": schema_type("integer", "Recovering jobs counted over the full active aggregate."),
            "terminal_pending_count": schema_type("integer", "Jobs awaiting terminal status after a stop request."),
            "recent_truncated": schema_type("boolean", "True when the aggregate truncated its recent list; counts remain reliable."),
            "latest_job_status": schema_type("string", "Latest active/recovering job status, or not_observed when none."),
            "recovery_state": {
                "type": "string",
                "enum": ["none", "recovering", "terminal_pending", "active", "unknown"],
                "description": "Derived only from proven aggregate fields, never wall-clock or the truncated recent list. active is reported instead of healthy because the aggregate cannot prove every active job is healthy running."
            }
        },
        "required": ["active_count", "running_count", "recovering_count", "terminal_pending_count", "recent_truncated", "latest_job_status", "recovery_state"]
    })
}

fn attempt_guidance_schema() -> Value {
    json!({
        "type": "object",
        "description": "Read-only open-guidance counts from the message board; never changes message status.",
        "additionalProperties": false,
        "properties": {
            "open_count": schema_type("integer", "Total open guidance messages."),
            "open_risk_count": schema_type("integer", "Open risk messages."),
            "open_todo_count": schema_type("integer", "Open todo messages."),
            "latest_open_kind": nullable_schema("string", "Kind of the most recent open guidance, when any."),
            "latest_open_at": nullable_schema("integer", "Unix timestamp of the most recent open guidance, when any."),
            "latest_open_message_id": nullable_schema("string", "Id of the most recent open guidance, when any.")
        },
        "required": ["open_count", "open_risk_count", "open_todo_count"]
    })
}

fn attempt_outcome_schema() -> Value {
    json!({
        "type": "object",
        "description": "Deterministic attempt outcome derived from proven activity, validation, jobs, and guidance.",
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["in_progress", "blocked", "clean", "unknown"]
            },
            "reason_codes": array_schema(schema_type("string", "A deterministic outcome reason code."), "Bounded outcome reason codes.")
        },
        "required": ["status", "reason_codes"]
    })
}

/// Validation delta projection surfaced by `validation_summary` and inside
/// `continuation_feedback`. Strict `additionalProperties: false` on all
/// sub-objects so field drift fails loudly.
pub fn validation_delta_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": false,
        "properties": {
            "comparison": validation_comparison_schema(),
            "counts": validation_delta_counts_schema(),
            "failures": validation_delta_failures_schema()
        },
        "required": ["comparison", "counts", "failures"]
    })
}

fn validation_comparison_schema() -> Value {
    json!({
        "type": "object",
        "description": "Comparison status, reason code (when unavailable), current/previous event identities, and a proven scope identity string.",
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["available", "unavailable"]
            },
            "reason_code": {
                "type": "string",
                "enum": [
                    "no_previous_validation",
                    "validation_scope_changed",
                    "previous_evidence_incomplete",
                    "current_evidence_incomplete",
                    "parser_changed",
                    "parser_identity_unavailable",
                    "test_identity_unavailable",
                    "insufficient_scope_identity",
                    "validation_not_requested"
                ],
                "description": "Stable reason code when status is unavailable."
            },
            "current_event_id": nullable_schema("string", "Event id of the current validation run, when retained."),
            "previous_event_id": nullable_schema("string", "Event id of the previous comparable run, when retained."),
            "scope_identity": nullable_schema("string", "Opaque, domain-separated scope identity (validation_scope:v1:<sha256>); never a raw command or absolute path.")
        },
        "required": ["status"]
    })
}

fn validation_delta_counts_schema() -> Value {
    json!({
        "type": "object",
        "description": "Signed count deltas (passed, failed, ignored, total) between the two comparable validation runs.",
        "additionalProperties": false,
        "properties": {
            "passed_delta": schema_type("integer", "Signed delta of passed tests; may be negative."),
            "failed_delta": schema_type("integer", "Signed delta of failed tests; may be negative."),
            "ignored_delta": schema_type("integer", "Signed delta of ignored tests; may be negative."),
            "total_delta": schema_type("integer", "Signed delta of total tests; may be negative.")
        },
        "required": ["passed_delta", "failed_delta", "ignored_delta", "total_delta"]
    })
}

fn validation_delta_failures_schema() -> Value {
    json!({
        "type": "object",
        "description": "Newly failed, resolved, and still-failing stable failure identities with totals and a truncation flag.",
        "additionalProperties": false,
        "properties": {
            "identity_status": {
                "type": "string",
                "enum": ["available", "unavailable"],
                "description": "available when stable failure identities are present; unavailable (with identity_reason_code) when only counts could be compared."
            },
            "identity_reason_code": nullable_schema("string", "Reason code when identity_status is unavailable; null otherwise."),
            "newly_failed": array_schema(failure_identity_schema(), "Bounded newly-failed stable failure identities."),
            "resolved": array_schema(failure_identity_schema(), "Bounded resolved stable failure identities."),
            "still_failing": array_schema(failure_identity_schema(), "Bounded still-failing stable failure identities."),
            "total_newly_failed": schema_type("integer", "Real count of newly-failed identities before bounding."),
            "total_resolved": schema_type("integer", "Real count of resolved identities before bounding."),
            "total_still_failing": schema_type("integer", "Real count of still-failing identities (may exceed the bounded list)."),
            "list_truncated": schema_type("boolean", "True when a failure list was capped at the bound.")
        },
        "required": ["identity_status", "newly_failed", "resolved", "still_failing", "total_newly_failed", "total_resolved", "total_still_failing", "list_truncated"]
    })
}

fn failure_identity_schema() -> Value {
    json!({
        "type": "object",
        "description": "A bounded, stable failure identity derived from the existing parser.",
        "additionalProperties": false,
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["test", "diagnostic", "unknown"]
            },
            "name": schema_type("string", "Stable failure name."),
            "file": nullable_schema("string", "Source file, when available."),
            "line": nullable_schema("integer", "Source line, when available.")
        },
        "required": ["kind", "name"]
    })
}
