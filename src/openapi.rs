use salvo::prelude::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD;
use crate::tool_runtime::{
    generic_tool_call_flattened_args_for_spec, registered_tool_specs, MAX_UNIFIED_DIFF_BYTES,
    TOOL_CALL_PARAMS_FIELD, TOOL_CALL_TOOL_FIELD,
};

const UNIFIED_DIFF_FIELD_DESCRIPTION: &str = "Raw standard unified diff only. Do not include shell heredocs or Codex apply_patch wrapper syntax such as *** Begin Patch / *** Update File / *** End Patch. The first non-empty line should be diff --git ..., --- ..., or another git-apply-compatible unified diff header.";
const SESSION_ID_FIELD_DESCRIPTION: &str = "Optional explicit existing wc_sess_* id. When provided, records this dedicated action in that exact Workflow Session; omission does not infer a Session.";
const FLATTENED_TOOL_ARG_DESCRIPTION: &str =
    "Flattened tool-specific argument. Used only when `params` is absent or null.";

fn flattened_tool_arg_schema(schema_type: &str) -> Value {
    json!({
        "type": schema_type,
        "description": FLATTENED_TOOL_ARG_DESCRIPTION
    })
}

fn flattened_tool_arg_schema_from_input(input_schema: &Value) -> Option<Value> {
    let direct_type_supported = matches!(
        input_schema.get("type").and_then(Value::as_str),
        Some("array" | "object" | "string" | "boolean" | "integer" | "number")
    );
    let nullable_scalar_supported = input_schema
        .get("anyOf")
        .and_then(Value::as_array)
        .is_some_and(|variants| {
            !variants.is_empty()
                && variants.iter().all(|variant| {
                    matches!(
                        variant.get("type").and_then(Value::as_str),
                        Some("string" | "boolean" | "integer" | "number" | "null")
                    )
                })
        });
    if !direct_type_supported && !nullable_scalar_supported {
        return None;
    }
    let mut schema = input_schema.clone();
    schema["description"] = Value::String(FLATTENED_TOOL_ARG_DESCRIPTION.to_string());
    Some(schema)
}

fn flattened_tool_arg_semantic_key(schema: &Value) -> String {
    fn without_descriptions(value: &Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "description")
                    .map(|(key, value)| (key.clone(), without_descriptions(value)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(without_descriptions).collect()),
            _ => value.clone(),
        }
    }

    serde_json::to_string(&without_descriptions(schema))
        .expect("flattened OpenAPI argument schemas must serialize")
}

fn flattened_tool_arg_schema_union(schemas: BTreeMap<String, (String, Value)>) -> Option<Value> {
    let mut schemas = schemas
        .into_values()
        .map(|(_, schema)| schema)
        .collect::<Vec<_>>();
    if schemas.len() == 1 {
        return schemas.pop();
    }
    if schemas.is_empty() {
        return None;
    }

    for schema in &mut schemas {
        if let Some(object) = schema.as_object_mut() {
            object.remove("description");
        }
    }
    Some(json!({
        "description": FLATTENED_TOOL_ARG_DESCRIPTION,
        "anyOf": schemas
    }))
}

pub(crate) fn public_url() -> String {
    std::env::var("WEBCODEX_PUBLIC_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:8080".to_string())
}

/// The exact, ordered set of GPT Actions operation ids exposed by
/// `/openapi.json`. Tests assert this set matches the generated schema.
///
/// Order is grouped by recommended GPT call flow:
/// 1. discovery (`listRuntimeTools`, `listProjects`, `getRuntimeStatus`)
/// 2. job inspection (`getRuntimeJobStatus`, `getRuntimeJobLog`)
/// 3. project inspection (`readProjectFile`, `getProjectGitStatus`,
///    `getProjectGitDiff`, `getProjectGitDiffSummary`, `listProjectFiles`,
///    `searchProjectText`)
/// 4. project mutation (`applyUnifiedDiff`, `runProjectShellCommand`,
///    `gitRestorePaths`, `discardUntrackedFiles`, `startProjectShellJob`)
/// 5. job inspection (`listRuntimeJobs`, `getRuntimeJobTail`)
/// 6. generic runtime entry point (`callRuntimeTool`), including the formal
///    GPT Actions route for model-generated `apply_patch` edits
///
/// Edit tools reachable through `callRuntimeTool` are `apply_text_edits`
/// (small guarded transactional changes), `apply_patch` (model-generated Codex
/// Patch DSL with Runner-side transactional preflight), `apply_unified_diff`
/// (external raw unified diff with internal preflight), and `write_project_file`
/// (intentional full rewrite). The legacy line/pattern and patch-triplet edit
/// tools were removed entirely; the current `apply_patch` name is the Codex-DSL
/// transactional tool, not the removed legacy patch facade.
#[cfg(test)]
const GPT_ACTION_OPS: &[&str] = &[
    "listRuntimeTools",
    "listProjects",
    "registerProject",
    "createProject",
    "getRuntimeStatus",
    "getRuntimeJobStatus",
    "getRuntimeJobLog",
    "readProjectFile",
    "getProjectGitStatus",
    "getProjectGitDiff",
    "getProjectGitDiffSummary",
    "listProjectFiles",
    "searchProjectText",
    "applyUnifiedDiff",
    "runProjectShellCommand",
    "gitRestorePaths",
    "discardUntrackedFiles",
    "importConversationFilesToProject",
    "startProjectShellJob",
    "listRuntimeJobs",
    "getRuntimeJobTail",
    "callRuntimeTool",
];

/// Removed historical endpoints that must never appear in `/openapi.json`.
/// Current HTTP-route visibility, including public browser/document delivery,
/// is owned by `route_metadata`.
#[cfg(test)]
const LEGACY_FORBIDDEN_PATHS: &[&str] = &[
    "/api/messages",
    "/api/files",
    "/api/desktop/task_op",
    "/api/desktop/task",
    "/api/codex/command_request_op",
    "/api/codex/command_request",
    "/api/codex/context",
    "/api/codex/context_batch",
    "/api/codex/apply_patch",
    "/api/codex/edit",
    "/api/codex/artifact",
    "/api/codex/git",
    "/api/codex/job",
    "/api/codex/report",
    "/api/codex/projects",
    "/api/codex/run",
    "/api/projects/write_file",
    "/api/projects/apply_patch",
    "/api/projects/validate_patch",
    "/api/projects/apply_patch_checked",
];

#[handler]
pub async fn openapi_json(depot: &mut Depot, res: &mut Response) {
    let spec = match crate::connector_runtime::http::runtime(depot) {
        Some(_) => crate::connector_runtime::surface::build_openapi_spec(public_url()),
        None => build_openapi_spec(),
    };
    res.render(Json(spec));
}

pub(crate) fn build_openapi_spec() -> Value {
    let mut spec = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "WebCodex Runtime API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Self-hosted tool runtime for ChatGPT. Flow: call listProjects (or listRuntimeTools), inspect with readProjectFile/getProjectGitStatus/git diff tools, edit with structured file/patch actions, and validate with cargo/job tools. Projects are registered by Runners and use the stable runtime id form agent:<client_id>:<project_id>. All endpoints require Bearer auth; static bearer/API-key hosts may use a shared key for quick start or wc_pat_* for managed mode. MCP and GPT Actions share the same ToolRuntime."
        },
        "servers": [
            {
                "url": public_url(),
                "description": "WebCodex server"
            }
        ],
        "paths": {
            "/api/tools/list": {
                "post": operation(
                    "listRuntimeTools",
                    "List runtime tools",
                    "Read-only. Full detail returns MCP-compatible tool specs and can be too large for GPT Actions. Prefer callRuntimeTool with tool=tool_manifest for daily discovery; when using listRuntimeTools, pass summary_only=true plus category, features, or limit for bounded discovery.",
                    "ToolsListRequest",
                    "ToolsListResponse"
                )
            },
            "/api/projects/list": {
                "post": operation(
                    "listProjects",
                    "List Runner-registered Projects",
                    "Read-only. When a Runner or Project is already known, pass exact client_id/project instead of reading the full registry; query is bounded text filtering over already-visible metadata and summary_only returns a compact workspace-selection projection.",
                    "ListProjectsRequest",
                    "ToolResult"
                )
            },
            "/api/projects/register": {
                "post": operation_with_examples(
                    "registerProject",
                    "Register an existing project",
                    "Mutation with side effects. Registers an existing directory as a WebCodex project on the selected Runner. Executes on the Runner and is constrained by Runner policy. Requires Bearer auth.",
                    "RegisterProjectRequest",
                    "ToolResult",
                    json!({
                        "basic": {
                            "summary": "Register an existing directory",
                            "value": {
                                "client_id": "oe",
                                "id": "my-project",
                                "name": "My Project",
                                "path": "/root/git/my-project",
                                "description": "Optional description",
                                "allow_patch": true,
                                "overwrite": false
                            }
                        }
                    })
                )
            },
            "/api/projects/create": {
                "post": operation_with_examples(
                    "createProject",
                    "Create and register a new project",
                    "Mutation with side effects. Creates a new directory, or explicitly adopts an already-existing empty directory, on the selected Runner and registers it as a WebCodex Project. Executes on the Runner and is constrained by Runner policy. Requires Bearer auth.",
                    "CreateProjectRequest",
                    "ToolResult",
                    json!({
                        "basicTemplate": {
                            "summary": "Create a project with the basic template",
                            "value": {
                                "client_id": "oe",
                                "id": "hello",
                                "name": "Hello",
                                "path": "/root/git/hello",
                                "description": "A new project",
                                "allow_patch": true,
                                "template": "basic",
                                "git_init": true,
                                "adopt_existing_empty": false,
                                "overwrite": false
                            }
                        },
                        "emptyTemplate": {
                            "summary": "Create an empty project",
                            "value": {
                                "client_id": "oe",
                                "id": "scratch",
                                "name": "Scratch",
                                "path": "/root/git/scratch"
                            }
                        }
                    })
                )
            },
            "/api/runtime/status": {
                "post": operation(
                    "getRuntimeStatus",
                    "Get runtime status",
                    "Read-only runtime health/observability with Runner count/online_count/stale_count, project/Job counts, and safe allowlisted effective_config. compact=true compacts this response, not MCP schema discovery. Pass exact client_id for one Runner; omit it for fleet-wide status.",
                    "RuntimeStatusRequest",
                    "ToolResult"
                )
            },
            "/api/jobs/status": {
                "post": operation_with_examples(
                    "getRuntimeJobStatus",
                    "Get job status",
                    "Read-only. Returns status, timing, and exit metadata for a runtime job. Use this to poll the job_id returned by run_job until status is completed, failed, stopped, or lost.",
                    "JobStatusRequest",
                    "ToolResult",
                    json!({
                        "byJobId": {
                            "summary": "Poll a job by id",
                            "value": {
                                "job_id": "11111111-2222-3333-4444-555555555555"
                            }
                        }
                    })
                )
            },
            "/api/jobs/log": {
                "post": operation_with_examples(
                    "getRuntimeJobLog",
                    "Get job log",
                    "Read-only. Returns bounded tails, line totals, truncation, cursor, exit status, and detected summary for a job_id. Use cursor.stdout as offset to continue.",
                    "JobLogRequest",
                    "ToolResult",
                    json!({
                        "byJobId": {
                            "summary": "Read the tail of a job log",
                            "value": {
                                "job_id": "11111111-2222-3333-4444-555555555555"
                            }
                        },
                        "withTailLines": {
                            "summary": "Read the last N stdout lines",
                            "value": {
                                "job_id": "11111111-2222-3333-4444-555555555555",
                                "tail_lines": 200
                            }
                        }
                    })
                )
            },
            "/api/jobs/list": {
                "post": operation_with_examples(
                    "listRuntimeJobs",
                    "List runtime jobs",
                    "Read-only bounded runtime job summaries. Inside a coding Session, prefer exact project/session_id filters; status combines with them using AND semantics. Filters only reduce caller-visible Jobs and are applied before limit. Never returns stdout/stderr bodies.",
                    "ListJobsRequest",
                    "ToolResult",
                    json!({
                        "all": {
                            "summary": "List recent jobs",
                            "value": {}
                        },
                        "running": {
                            "summary": "List running jobs",
                            "value": {
                                "status": "running",
                                "limit": 20
                            }
                        },
                        "session": {
                            "summary": "List Jobs for one coding Session",
                            "value": {
                                "project": "agent:special:webcodex",
                                "session_id": "wc_sess_example"
                            }
                        }
                    })
                )
            },
            "/api/jobs/tail": {
                "post": operation_with_examples(
                    "getRuntimeJobTail",
                    "Get job tail",
                    "Read-only bounded stdout/stderr tails for a runtime job. Defaults to a bounded tail so the caller never reads full logs by default. Use the job_id returned by run_job.",
                    "JobTailRequest",
                    "ToolResult",
                    json!({
                        "byJobId": {
                            "summary": "Read a bounded tail",
                            "value": {
                                "job_id": "11111111-2222-3333-4444-555555555555",
                                "tail_lines": 50
                            }
                        }
                    })
                )
            },
            "/api/projects/read_file": {
                "post": operation_with_examples(
                    "readProjectFile",
                    "Read a project file",
                    "Read-only. Reads a UTF-8 project file through its owning Runner. Output is bounded; use start_line and limit for pagination. The response carries one text representation only: plain by default or 1-based numbered text when with_line_numbers=true.",
                    "ReadProjectFileRequest",
                    "ToolResult",
                    json!({
                        "readme": {
                            "summary": "Read a project README",
                            "value": {
                                "project": "webcodex",
                                "path": "README.md"
                            }
                        },
                        "paginated": {
                            "summary": "Read a slice of a source file",
                            "value": {
                                "project": "webcodex",
                                "path": "src/main.rs",
                                "start_line": 1,
                                "limit": 100,
                                "with_line_numbers": true
                            }
                        }
                    })
                )
            },
            "/api/projects/git_status": {
                "post": operation_with_examples(
                    "getProjectGitStatus",
                    "Get project git status",
                    "Runs `git status --porcelain` in a Runner-registered Project and returns stdout, stderr, and exit_code. Safe read-only project inspection; use before proposing changes or invoking mutation tools.",
                    "ProjectIdRequest",
                    "ToolResult",
                    json!({
                        "byProject": {
                            "summary": "Check git status of a project",
                            "value": {
                                "project": "webcodex"
                            }
                        }
                    })
                )
            },
            "/api/projects/git_diff": {
                "post": operation_with_examples(
                    "getProjectGitDiff",
                    "Get project git diff",
                    "Runs `git diff` in a Runner-registered Project and returns stdout, stderr, and exit_code. Optional `args` scopes paths or adds flags (e.g. [\"--stat\"]). Read-only inspection; routes to the owning Runner.",
                    "ProjectGitDiffRequest",
                    "ToolResult",
                    json!({
                        "byProject": {
                            "summary": "Full diff of a project",
                            "value": {
                                "project": "webcodex"
                            }
                        },
                        "withStat": {
                            "summary": "Diffstat of a project",
                            "value": {
                                "project": "webcodex",
                                "args": ["--stat"]
                            }
                        }
                    })
                )
            },
            "/api/projects/git_diff_summary": {
                "post": operation_with_examples(
                    "getProjectGitDiffSummary",
                    "Get project git diff summary",
                    "Read-only git diff summary for a Runner-registered Project: `git status --porcelain`, `git diff --stat`, and a parsed changed-file list. Does not modify the worktree. Routes to the owning Runner.",
                    "ProjectIdRequest",
                    "ToolResult",
                    json!({
                        "byProject": {
                            "summary": "Diff summary of a project",
                            "value": {
                                "project": "webcodex"
                            }
                        }
                    })
                )
            },
            "/api/projects/list_files": {
                "post": operation_with_examples(
                    "listProjectFiles",
                    "List project files",
                    "Read-only bounded file listing of a Runner-registered Project directory. Returns project-relative paths plus a file/dir kind. Optional `path` scopes a subdirectory; `limit` bounds the entry count. Routes to the owning Runner.",
                    "ListProjectFilesRequest",
                    "ToolResult",
                    json!({
                        "root": {
                            "summary": "List project root",
                            "value": {
                                "project": "webcodex"
                            }
                        },
                        "subdir": {
                            "summary": "List a subdirectory",
                            "value": {
                                "project": "webcodex",
                                "path": "src",
                                "limit": 100
                            }
                        }
                    })
                )
            },
            "/api/projects/search_text": {
                "post": operation_with_examples(
                    "searchProjectText",
                    "Search project text",
                    "Read-only bounded project-text search. Regex is the default; prefer pattern_mode=literal for identifiers, snippets, paths, and other exact text. Results use project-relative paths and 1-based line numbers; optional context is bounded and sensitive/build directories are excluded.",
                    "SearchProjectTextRequest",
                    "ToolResult",
                    json!({
                        "byPattern": {
                            "summary": "Search for a pattern",
                            "value": {
                                "project": "webcodex",
                                "pattern": "fn main",
                                "limit": 20,
                                "context_before": 2,
                                "context_after": 4
                            }
                        }
                    })
                )
            },
            "/api/projects/apply_unified_diff": {
                "post": operation_with_examples(
                    "applyUnifiedDiff",
                    "Apply a unified diff to a project",
                    "External/raw unified-diff mutation only, with side effects; requires Bearer auth and Runner shell capability. Use only when input is already a standard unified diff; ordinary model-generated edits should use callRuntimeTool with tool=apply_text_edits after reading the current file SHA, while contextual or large patch-shaped changes can use tool=apply_patch. Performs bounded preflight; failed preflight is zero-write and post-dispatch uncertainty requires workspace inspection.",
                    "ApplyUnifiedDiffRequest",
                    "ApplyUnifiedDiffToolResult",
                    json!({
                        "example": {
                            "summary": "Apply a small unified diff",
                            "value": {
                                "project": "webcodex",
                                "diff": "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n# WebCodex\n+edited\n"
                            }
                        }
                    })
                )
            },
            "/api/projects/run_shell": {
                "post": operation_with_examples(
                    "runProjectShellCommand",
                    "Run a shell command in a project",
                    "Runs a shell command in a Runner-registered Project and returns stdout, stderr, exit_code plus command_started/command_ok/failure_kind/tool_failure. Executable with side effects; requires Bearer auth and Runner shell capability.",
                    "RunShellRequest",
                    "ToolResult",
                    json!({
                        "tests": {
                            "summary": "Run the test suite",
                            "value": {
                                "project": "webcodex",
                                "command": "cargo test"
                            }
                        },
                        "withCwd": {
                            "summary": "Run a command in a subdirectory",
                            "value": {
                                "project": "webcodex",
                                "command": "ls",
                                "cwd": "src"
                            }
                        }
                    })
                )
            },
            "/api/projects/git_restore_paths": {
                "post": operation_with_examples(
                    "gitRestorePaths",
                    "Restore tracked project paths",
                    "Mutation with side effects. Runs `git restore -- <paths>` on selected tracked project-relative paths. Does not remove untracked files. Requires Bearer auth and the Runner `structured_process_argv` capability.",
                    "GitRestorePathsRequest",
                    "ToolResult",
                    json!({
                        "byProject": {
                            "summary": "Restore selected tracked paths",
                            "value": {
                                "project": "webcodex",
                                "paths": ["tmp_probe.txt"]
                            }
                        }
                    })
                )
            },
            "/api/projects/discard_untracked": {
                "post": operation_with_examples(
                    "discardUntrackedFiles",
                    "Discard untracked project files",
                    "Mutation with side effects. Runs `git clean -f -- <paths>` only for selected project-relative untracked paths. Requires Bearer auth and the Runner `structured_process_argv` capability.",
                    "DiscardUntrackedRequest",
                    "ToolResult",
                    json!({
                        "byProject": {
                            "summary": "Discard selected untracked files",
                            "value": {
                                "project": "webcodex",
                                "paths": ["tmp_probe.txt"]
                            }
                        }
                    })
                )
            },
            "/api/artifacts/import": {
                "post": operation_with_examples(
                    "importConversationFilesToProject",
                    "Import ChatGPT conversation files to a project",
                    "Mutation with side effects. Downloads GPT Actions openaiFileIdRefs immediately and saves bounded binary files into a Runner-registered Project. Populate openaiFileIdRefs from current conversation files generated by image generation, user upload, or Code Interpreter; never call with an empty array.",
                    "ImportConversationFilesRequest",
                    "ImportConversationFilesResponse",
                    json!({
                        "generatedImage": {
                            "summary": "Save a generated image into docs/assets",
                            "value": {
                                "project": "agent:oe:webcodex",
                                "output_dir": "docs/assets",
                                "overwrite": false,
                                "openaiFileIdRefs": [{
                                    "name": "generated.png",
                                    "id": "file_abc123",
                                    "mime_type": "image/png",
                                    "download_link": "https://files.oaiusercontent.com/example"
                                }]
                            }
                        }
                    })
                )
            },
            "/api/projects/run_job": {
                "post": operation_with_examples(
                    "startProjectShellJob",
                    "Start an async project shell job",
                    "Starts an async background shell job in a Runner-registered Project and returns a job_id. Execution with side effects; requires Bearer auth and the Runner async shell job capability. Poll with getRuntimeJobStatus; read output with getRuntimeJobTail or getRuntimeJobLog.",
                    "StartProjectShellJobRequest",
                    "ToolResult",
                    json!({
                        "testCommand": {
                            "summary": "Run a lightweight test command asynchronously",
                            "value": {
                                "project": "webcodex",
                                "command": "cargo test --no-run"
                            }
                        },
                        "withTimeout": {
                            "summary": "Run a check command with a timeout",
                            "value": {
                                "project": "webcodex",
                                "command": "cargo clippy",
                                "timeout_secs": 300,
                                "cwd": "src"
                            }
                        }
                    })
                )
            },
            "/api/tools/call": {
                "post": operation_with_examples(
                    "callRuntimeTool",
                    "Call runtime tool",
                    "Generic/advanced route for model-visible runtime tools. Prefer dedicated actions when they match. For ordinary model-generated file edits after read_file/read_files, use tool=apply_text_edits with the current expected_sha256; use tool=apply_patch when a contextual or large patch-shaped change is clearer. Flatten tool args at top level; params is the canonical non-Action envelope; recording_session_id records wrapper calls.",
                    "ToolCallRequest",
                    "ToolResult",
                    json!({
                        "applyTextEdits": {
                            "summary": "Apply a SHA-guarded edit after reading the current file",
                            "value": {
                                "tool": "apply_text_edits",
                                "project": "webcodex",
                                "changes": [{
                                    "kind": "edit",
                                    "path": "README.md",
                                    "expected_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                                    "edits": [{
                                        "kind": "replace_exact",
                                        "old_text": "# WebCodex",
                                        "new_text": "# WebCodex Runtime"
                                    }]
                                }]
                            }
                        },
                        "applyPatch": {
                            "summary": "Apply a model-generated Codex patch",
                            "value": {
                                "tool": "apply_patch",
                                "project": "webcodex",
                                "patch": "*** Begin Patch\n*** Update File: README.md\n@@\n-# WebCodex\n+# WebCodex Runtime\n*** End Patch"
                            }
                        },
                        "workOnAbsolutePath": {
                            "summary": "Resolve or register a Runner path, then start coding",
                            "value": {
                                "tool": "work_on_project",
                                "client_id": "special",
                                "path": "/root/git/example-worktree",
                                "instruction": "Complete the development task"
                            }
                        },
                        "workOnManagedWorktree": {
                            "summary": "Bootstrap an isolated Runner-managed worktree, register it, then start coding",
                            "value": {
                                "tool": "work_on_project",
                                "client_id": "special",
                                "path": "/root/git/source-checkout",
                                "mode": "worktree",
                                "base_ref": "origin/main",
                                "instruction": "Complete the development task in an isolated worktree"
                            }
                        },
                        "recordedGitStatus": {
                            "summary": "Record this wrapper call while passing flattened tool args",
                            "value": {
                                "tool": "git_status",
                                "project": "webcodex",
                                TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_example"
                            }
                        },
                        "sessionSummary": {
                            "summary": "Read a session summary with top-level business session_id",
                            "value": {
                                "tool": "session_summary",
                                "session_id": "wc_sess_example",
                                "limit": 20
                            }
                        },
                        "postSessionMessage": {
                            "summary": "Post session-local guidance while recording the wrapper call separately",
                            "value": {
                                "tool": "post_session_message",
                                "session_id": "wc_sess_business",
                                TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_recorder",
                                "kind": "guidance",
                                "message": "Keep new capabilities behind callRuntimeTool; do not add dedicated OpenAPI operations.",
                                "tags": ["openapi", "constraint"],
                                "priority": "normal"
                            }
                        },
                        "showChanges": {
                            "summary": "Summarize current worktree changes with optional session activity",
                            "value": {
                                "tool": "show_changes",
                                "project": "webcodex",
                                "session_id": "wc_sess_example",
                                "include_diff": false,
                                "session_event_limit": 30
                            }
                        },
                        "readFile": {
                            "summary": "Call read_file via flattened GPT Action fields",
                            "value": {
                                "tool": "read_file",
                                "project": "webcodex",
                                "path": "README.md",
                                "with_line_numbers": true
                            }
                        },
                        "readFiles": {
                            "summary": "Read several files with one bounded call",
                            "value": {
                                "tool": "read_files",
                                "project": "webcodex",
                                "items": [
                                    {"path": "src/lib.rs", "start_line": 1, "limit": 120},
                                    {"path": "src/main.rs", "limit": 80}
                                ],
                                "with_line_numbers": true
                            }
                        },
                        "searchProjectTexts": {
                            "summary": "Run several independent bounded text searches",
                            "value": {
                                "tool": "search_project_texts",
                                "project": "webcodex",
                                "queries": [
                                    {
                                        "pattern": "ResolvedProject",
                                        "path": "src",
                                        "result_mode": "matches",
                                        "limit": 20,
                                        "context_before": 2,
                                        "context_after": 4
                                    },
                                    {
                                        "pattern": "read_files",
                                        "path": "src/tool_runtime/tests",
                                        "result_mode": "files_with_matches",
                                        "limit": 20
                                    }
                                ]
                            }
                        },
                        "checkpointRestore": {
                            "summary": "Restore a checkpoint via flattened GPT Action fields",
                            "value": {
                                "tool": "workspace_checkpoint_restore",
                                "project": "webcodex",
                                "checkpoint_id": "wc_ckpt_abc",
                                "confirm": true,
                                TOOL_CALL_RECORDING_SESSION_ID_FIELD: "wc_sess_record"
                            }
                        },
                        "applyTextEdits": {
                            "summary": "Transactional file edit via flattened GPT Action fields",
                            "value": {
                                "tool": "apply_text_edits",
                                "project": "webcodex",
                                "dry_run": true,
                                "changes": [{
                                    "kind": "edit",
                                    "path": "src/lib.rs",
                                    "expected_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                                    "edits": [
                                        {"kind": "replace_exact", "old_text": "alpha", "new_text": "beta"}
                                    ]
                                }]
                            }
                        },
                        "paramsEnvelope": {
                            "summary": "Canonical direct/non-Action params envelope",
                            "value": {
                                "tool": "git_diff_summary",
                                "params": {"project": "webcodex"}
                            }
                        },
                        "noParams": {
                            "summary": "Argument-less tool; omit params",
                            "value": {
                                "tool": "list_tools"
                            }
                        }
                    })
                )
            }
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Bearer token. Static bearer/API-key hosts may send a shared key for quick start or wc_pat_* for managed mode; WEBCODEX_TOKEN is the server bootstrap/admin credential."
                }
            },
            "schemas": schemas()
        },
        "security": [
            {
                "bearerAuth": []
            }
        ]
    });
    spec["paths"]
        .as_object_mut()
        .expect("OpenAPI paths object")
        .retain(|path, _| {
            crate::route_metadata::lookup("POST", path).is_some_and(|route| {
                route.openapi_visibility == crate::route_metadata::OpenApiVisibility::PublicActions
            })
        });
    spec
}

fn operation(
    operation_id: &str,
    summary: &str,
    description: &str,
    request_schema: &str,
    response_schema: &str,
) -> Value {
    operation_with_examples(
        operation_id,
        summary,
        description,
        request_schema,
        response_schema,
        Value::Null,
    )
}

fn operation_with_examples(
    operation_id: &str,
    summary: &str,
    description: &str,
    request_schema: &str,
    response_schema: &str,
    examples: Value,
) -> Value {
    let mut media_type = json!({
        "schema": {
            "$ref": format!("#/components/schemas/{}", request_schema)
        }
    });
    if let Value::Object(examples_obj) = examples {
        if !examples_obj.is_empty() {
            media_type["examples"] = Value::Object(examples_obj);
        }
    }
    json!({
        "operationId": operation_id,
        "x-openai-isConsequential": is_consequential_operation(operation_id),
        "summary": summary,
        "description": description,
        "requestBody": {
            "required": true,
            "content": {
                "application/json": media_type
            }
        },
        "responses": {
            "200": {
                "description": "Success",
                "content": {
                    "application/json": {
                        "schema": {
                            "$ref": format!("#/components/schemas/{}", response_schema)
                        }
                    }
                }
            },
            "400": {
                "description": "Bad request",
                "content": {
                    "application/json": {
                        "schema": {
                            "$ref": "#/components/schemas/ErrorResponse"
                        }
                    }
                }
            },
            "401": {
                "description": "Unauthorized"
            }
        }
    })
}

fn is_consequential_operation(operation_id: &str) -> bool {
    match operation_id {
        "listRuntimeTools"
        | "listProjects"
        | "getRuntimeStatus"
        | "readProjectFile"
        | "listProjectFiles"
        | "searchProjectText"
        | "getProjectGitStatus"
        | "getProjectGitDiff"
        | "getProjectGitDiffSummary"
        | "getProjectGitDiffHunks"
        | "getRuntimeJobStatus"
        | "getRuntimeJobLog"
        | "getRuntimeJobTail"
        | "listRuntimeJobs"
        | "registerProject"
        | "createProject" => false,

        "applyUnifiedDiff"
        | "importConversationFilesToProject"
        | "runProjectShellCommand"
        | "startProjectShellJob"
        | "stopRuntimeJob"
        | "gitRestorePaths"
        | "discardUntracked"
        | "discardUntrackedFiles"
        | "callRuntimeTool" => true,

        other => panic!("missing consequential classification for operationId {other}"),
    }
}

fn schemas() -> Value {
    let mut schemas = json!({
        "EmptyRequest": {
            "type": "object",
            "additionalProperties": false,
            "properties": {},
            "description": "Empty request body. Send {} for actions that take no arguments."
        },
        "ListProjectsRequest": {
            "type": "object",
            "additionalProperties": false,
            "description": "Optional targeted Project inventory filters. Omit all fields for legacy full-registry behavior.",
            "properties": {
                "client_id": {"type": "string", "maxLength": 128, "description": "Exact caller-visible Runner client_id."},
                "project": {"type": "string", "maxLength": 512, "description": "Exact full runtime Project id."},
                "query": {"type": "string", "maxLength": 200, "description": "Bounded deterministic text filter over already-visible Project metadata; blank queries are rejected."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum Projects after filtering; targeted calls default to 100."},
                "summary_only": {"type": "boolean", "description": "Return compact workspace-selection metadata instead of full Project detail."}
            }
        },
        "RuntimeStatusRequest": {
            "type": "object",
            "additionalProperties": false,
            "description": "Optional focused runtime observation. Omit client_id for legacy fleet-wide semantics.",
            "properties": {
                "client_id": {"type": "string", "maxLength": 128, "description": "Exact caller-visible Runner client_id to evaluate independently of unrelated fleet mismatches."},
                "compact": {"type": "boolean", "description": "Return compact runtime observability."},
                "summary_only": {"type": "boolean", "description": "Alias for compact=true."}
            }
        },
        "ToolsListRequest": {
            "type": "object",
            "additionalProperties": false,
            "description": "Optional bounded runtime tool discovery request. Omit fields for the legacy full detail list; GPT Actions should prefer summary_only=true with category, features, or limit.",
            "properties": {
                "category": {
                    "type": "string",
                    "description": "Optional tool_manifest category filter such as artifact, edit, session, git, validation, job, project, or runtime."
                },
                "features": {
                    "type": "string",
                    "description": "Optional loose feature filter such as artifact, artifact_upload, upload, read, edit, session, git, or validation."
                },
                "summary_only": {
                    "type": "boolean",
                    "description": "When true, return compact summaries without full input/output schemas."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Maximum returned tools for focused discovery. Runtime caps this at 100."
                }
            }
        },
        "OpenAiFileIdRef": {
            "type": "object",
            "additionalProperties": false,
            "required": ["download_link"],
            "description": "GPT Actions file reference. Field name openaiFileIdRefs must be used by the Action request so ChatGPT can pass conversation files.",
            "properties": {
                "name": {"type": "string"},
                "id": {"type": "string"},
                "mime_type": {"type": "string"},
                "download_link": {"type": "string", "description": "Temporary download URL; WebCodex downloads it immediately."}
            }
        },
        "ImportConversationFilesRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["openaiFileIdRefs", "project"],
            "description": "Import up to 10 GPT Actions conversation files into a project. Supports image/png, image/jpeg, image/webp, application/pdf, application/zip, DOCX/PPTX/XLSX OOXML MIME types, text/plain, text/csv, application/json, and restricted application/octet-stream.",
            "properties": {
                "openaiFileIdRefs": {"type": "array", "maxItems": 10, "items": {"$ref": "#/components/schemas/OpenAiFileIdRef"}},
                "project": {"type": "string", "description": "Runner-registered runtime Project id from listProjects."},
                "output_dir": {"type": "string", "description": "Optional project-relative output directory, for example docs/assets or artifacts/imports."},
                "targets": {"type": "array", "items": {"type": "string"}, "description": "Optional per-file output filenames."},
                "overwrite": {"type": "boolean", "description": "Allow overwriting existing files. Defaults to false."}
            }
        },
        "ImportConversationFilesResponse": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "success": {"type": "boolean"},
                "output": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {
                        "count": {"type": "integer"},
                        "imported": {"type": "array", "items": {"type": "object", "additionalProperties": true}}
                    }
                },
                "error": {"type": "string", "nullable": true}
            }
        },
        "ToolCallRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": [TOOL_CALL_TOOL_FIELD],
            "description": "Generic GPT Actions runtime tool call. The model-facing `tool` selector and flattened top-level fields cover only model-visible runtime tools and match registered_tool_specs, MCP discovery, and tool_manifest. GPT Actions should pass tool-specific arguments as flattened top-level fields because some Action runtimes reject free-form params objects. `params` is the canonical direct/non-Action argument envelope; a non-null value takes precedence and a null wrapper does not suppress flattened arguments. The retired `arguments` wrapper is rejected. Top-level `session_id` is ordinary explicit tool business input when declared by the selected visible tool; use `recording_session_id` only to record this wrapper call in an explicitly selected existing Workflow Session. Omitted Session identifiers never infer a Workflow Session from window, credential, project, or prior calls. For daily discovery prefer tool_manifest; it exposes accepted_flattened_args for model-facing top-level calls. Use list_tools with summary_only/category/features/limit only for focused discovery.",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Flattened tool-specific argument. For session_summary and message-board tools this is the required business session id to read or update in the session ledger; for project tools it explicitly selects the Workflow Session whose policy/evidence applies. Omission leaves ordinary project calls unrecorded. Use recording_session_id to record the wrapper call itself in an explicitly authorized Session."
                },
                "kind": {
                    "type": "string",
                    "description": "Flattened tool-specific argument. For message-board tools, one of note, proposal, question, answer, decision, risk, progress, guidance, todo. For workspace_checkpoint_create, one of snapshot, baseline, before_refactor, after_refactor, last_known_good, rollback_candidate. Used only when `params` is absent or null."
                },
                "labels": {
                    "type": "array",
                    "items": {"type": "string", "maxLength": 64, "pattern": "^[A-Za-z0-9._-]+$"},
                    "maxItems": 20,
                    "description": "Flattened workspace_checkpoint_create labels. Used only when `params` is absent or null."
                },
                "validation": {
                    "type": "object",
                    "additionalProperties": false,
                    "description": "Flattened workspace_checkpoint_create validation metadata. The runtime records this metadata only and does not run commands.",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["unknown", "not_run", "passed", "failed"]
                        },
                        "commands": {
                            "type": "array",
                            "items": {"type": "string", "maxLength": 200},
                            "maxItems": 20
                        },
                        "summary": {
                            "anyOf": [
                                {"type": "string"},
                                {"type": "null"}
                            ],
                            "maxLength": 500
                        }
                    }
                },
                "note": {
                    "type": "string",
                    "description": "Flattened workspace_checkpoint_create optional note (not used by restore). Used only when `params` is absent or null."
                },
                "include_untracked": {
                    "type": "boolean",
                    "description": "Flattened workspace_checkpoint_create flag to capture small non-secret UTF-8 untracked files (default false). Used only when `params` is absent or null."
                },
                "checkpoint_id": {
                    "type": "string",
                    "description": "Flattened workspace_checkpoint_show/restore/delete wc_ckpt_* id. Used only when `params` is absent or null."
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Flattened confirmation flag for workspace_checkpoint_restore/delete and stop_job; must be true to proceed. Used only when `params` is absent or null."
                },
                "include_command_preview": {
                    "type": "boolean",
                    "description": "Flattened job_status debug flag. Defaults to false; when true, job_status includes bounded command_preview metadata. stdout/stderr bodies are never included. Used only when `params` is absent or null."
                },
                "include_diff_stat": {
                    "type": "boolean",
                    "description": "Flattened workspace_checkpoint_show flag to include tracked/staged diff stat strings (default false). Used only when `params` is absent or null."
                },
                // Keep the flattened GPT Action shape composition-free. The canonical MCP/local-coding
                // ToolSpec carries the strict per-kind oneOf contract; this import-facing projection uses
                // explicit bounded properties plus exact field guidance because nested composed schemas are
                // less reliable on the flattened Actions surface. Runtime preflight remains authoritative.
                "changes": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 16,
                    "description": "Flattened apply_text_edits transactional file changes. kind=edit requires path, expected_sha256, and edits and forbids to_path/content; create requires path/content and forbids to_path/expected_sha256/edits; delete requires path/expected_sha256 and forbids to_path/content/edits; rename requires path/to_path/expected_sha256 and forbids content/edits. Used only when `params` is absent or null.",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind", "path"],
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["edit", "create", "delete", "rename"],
                                "description": "File change kind; use only the fields allowed for that kind as documented on changes."
                            },
                            "path": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Project-relative source or target path."
                            },
                            "to_path": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Required only for rename; project-relative destination must differ from path. Forbidden for edit/create/delete."
                            },
                            "content": {
                                "type": "string",
                                "description": "Required only for create; complete UTF-8 content, which may be empty. Forbidden for edit/delete/rename."
                            },
                            "expected_sha256": {
                                "type": "string",
                                "pattern": "^[a-f0-9]{64}$",
                                "description": "Required current-file hash for edit, delete, and rename; forbidden for create."
                            },
                            "edits": {
                                "type": "array",
                                "minItems": 1,
                                "maxItems": 20,
                                "description": "Required only for kind=edit. replace_exact requires non-empty old_text, optional new_text (omitted means empty replacement), and forbids anchor_text; delete_exact requires non-empty old_text and forbids new_text/anchor_text; insert_before/insert_after require non-empty anchor_text and new_text and forbid old_text.",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["kind"],
                                    "properties": {
                                        "kind": {"type": "string", "enum": ["replace_exact", "insert_after", "insert_before", "delete_exact"], "description": "Exact edit kind; use only the fields documented on edits for that kind."},
                                        "old_text": {"type": "string", "minLength": 1, "description": "Required for replace_exact/delete_exact; forbidden for insert_before/insert_after."},
                                        "new_text": {"type": "string", "description": "Replacement for replace_exact (may be empty or omitted); required non-empty for insert_before/insert_after; forbidden for delete_exact."},
                                        "anchor_text": {"type": "string", "minLength": 1, "description": "Required for insert_before/insert_after; forbidden for replace_exact/delete_exact."},
                                        "occurrence": {"type": "integer", "minimum": 1, "description": "Optional 1-based exact occurrence selector; use only when structured conflict recovery advertises selector support. expected_sha256 remains required."}
                                    }
                                }
                            }
                        }
                    }
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "Flattened apply_text_edits flag to compute the plan without writing. Used only when `params` is absent or null."
                },
                "message": {
                    "type": "string",
                    "description": "Flattened post_session_message body. Used only when `params` is absent or null."
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Flattened post_session_message tags. Used only when `params` is absent or null."
                },
                "reply_to": {
                    "type": "string",
                    "description": "Flattened post_session_message reply target wc_msg_* id. Used only when `params` is absent or null."
                },
                "priority": {
                    "type": "string",
                    "description": "Flattened post_session_message priority: low, normal, or high. Used only when `params` is absent or null."
                },
                "status": {
                    "type": "string",
                    "description": "Flattened list_session_messages status filter: open or resolved. Used only when `params` is absent or null."
                },
                "after_observation_token": {
                    "type": "string",
                    "maxLength": 192,
                    "description": "Flattened opaque observation token for observe_session_messages and compatible bounded observation tools. Used only when `params` is absent or null."
                },
                "wait_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 60,
                    "description": "Flattened one-shot bounded wait for observe_session_messages and compatible observation tools. Used only when `params` is absent or null."
                },
                "message_id": {
                    "type": "string",
                    "description": "Flattened resolve_session_message wc_msg_* id. Used only when `params` is absent or null."
                },
                "resolution": {
                    "type": "string",
                    "description": "Flattened resolve_session_message resolution note. Used only when `params` is absent or null."
                },
                "compact": {
                    "type": "boolean",
                    "description": "Flattened runtime_status flag. Defaults to false. When true, returns compact runtime observability for sanity checks instead of the full status payload. Used only when `params` is absent or null."
                },
                "path": {
                    "type": "string",
                    "description": "Flattened tool-specific argument. For artifact_upload_chunk/finish/abort this is required and must exactly match the path used by artifact_upload_begin to bind upload_id to the target path. Used only when `params` is absent or null."
                },
                "skip": {
                    "type": "integer",
                    "description": "Flattened git_log commit offset. Used only when `params` is absent or null."
                },
                "category": {
                    "type": "string",
                    "description": "Flattened list_tools/tool_manifest category filter. Used only when `params` is absent or null."
                },
                "intent": {
                    "type": "string",
                    "description": "Flattened tool_manifest task-intent view such as coding, audit, exploration, release, or discovery. Distinct from category. Intent views only filter and rank discovery output; they do not change tool behavior, policy, permissions, execution, or finish verdict semantics. Used only when `params` is absent or null."
                },
                "include_recommended_flows": {
                    "type": "boolean",
                    "description": "Flattened tool_manifest flag. Defaults to true and controls recommended_flows in compact discovery output. Used only when `params` is absent or null."
                },
                "include_risk_summary": {
                    "type": "boolean",
                    "description": "Flattened tool_manifest flag. Defaults to true and controls risk_summary in compact discovery output. Used only when `params` is absent or null."
                },
                "include_hygiene": {
                    "type": "boolean",
                    "description": "Flattened finish_coding_task flag. Defaults to true. Used only when `params` is absent or null."
                },
                "max_findings": {
                    "type": "integer",
                    "description": "Flattened workspace_hygiene_check maximum findings to return; clamped by the runtime to 1..200. Used only when `params` is absent or null."
                },
                "include_tracked": {
                    "type": "boolean",
                    "description": "Flattened workspace_hygiene_check flag. When true, also report tracked suspicious path names by path/name only; file contents are never read. Used only when `params` is absent or null."
                },
                "include_handoff": {
                    "type": "boolean",
                    "description": "Flattened finish_coding_task flag. Defaults to true. Used only when `params` is absent or null."
                },
                "include_validation_summary": {
                    "type": "boolean",
                    "description": "Flattened finish_coding_task flag. Defaults to true; minimal diagnostics may be derived from safe bounded validation metadata, but raw stdout/stderr is never exposed. Used only when `params` is absent or null."
                },
                "include_validation": {
                    "type": "boolean",
                    "description": "Flattened session_handoff_summary flag. Defaults to true; validation is ledger-derived and parser.available is true only when safe bounded metadata is present. Used only when `params` is absent or null."
                },
                "include_workspace": {
                    "type": "boolean",
                    "description": "Flattened session_handoff_summary/finish_coding_task flag. For handoff, include a bounded workspace/git status summary when project is provided. For finish, control the nested handoff workspace block; the top-level finish workspace/show_changes check still runs. Used only when params and arguments are absent."
                },
                "include_checkpoints": {
                    "type": "boolean",
                    "description": "Flattened session_handoff_summary flag. Include bounded checkpoint candidates when project is provided. Used only when params and arguments are absent."
                },
                "features": {
                    "type": "string",
                    "description": "Flattened list_tools feature filter, or cargo feature selection for cargo tools. Used only when `params` is absent or null."
                },
                "summary_only": {
                    "type": "boolean",
                    "description": "Flattened list_tools/runtime_status/session_handoff_summary/finish_coding_task flag. For list_tools, returns compact tool summaries without full schemas. For runtime_status, aliases compact=true. For handoff/finish, returns compact closeout outcome fields and omits recent_events, long ledger details, command text, stdout/stderr, tails, and excerpts. Used only when `params` is absent or null."
                },
                "upload_id": {
                    "type": "string",
                    "description": "Flattened artifact_upload_chunk/finish/abort wc_upload_* id. The same path from artifact_upload_begin is also required so the runtime can bind upload_id to the requested target path. Used only when `params` is absent or null."
                },
                "expected_bytes": {
                    "type": "integer",
                    "description": "Flattened artifact_upload_begin final byte count guard. Used only when `params` is absent or null."
                },
                "allow_missing": {
                    "type": "boolean",
                    "description": "Flattened read_project_artifact_metadata flag. When true, a missing artifact returns exists=false instead of a failed tool call. Used only when `params` is absent or null."
                },
            }
        },
        "JobStatusRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["job_id"],
            "description": "Poll a runtime job by id.",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "Runtime job id returned by run_job."
                }
            }
        },
        "JobLogRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["job_id"],
            "description": "Read bounded stdout/stderr for a runtime job.",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "Runtime job id returned by run_job."
                },
                "offset": {
                    "type": "integer",
                    "description": "Optional 1-based continuation offset. Use cursor.stdout from the previous response."
                },
                "tail_lines": {
                    "type": "integer",
                    "description": "Optional number of trailing stdout/stderr lines. Logs are always bounded; large values are capped server-side."
                }
            }
        },
        "ReadProjectFileRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project", "path"],
            "description": "Read a UTF-8 file from a Runner-registered Project.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "path": {
                    "type": "string",
                    "description": "Project-relative file path. Absolute paths and traversal (..) are rejected."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                },
                "start_line": {
                    "type": "integer",
                    "description": "Optional 1-based line offset for pagination."
                },
                "limit": {
                    "type": "integer",
                    "description": "Optional maximum line count (bounded server-side)."
                },
                "with_line_numbers": {
                    "type": "boolean",
                    "description": "Optional. When true, the single text field uses numbered format with 1-based line numbers; plain and numbered content are never duplicated."
                }
            }
        },
        "ProjectIdRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project"],
            "description": "Identify a project by id.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                }
            }
        },
        "ProjectGitDiffRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project"],
            "description": "Run `git diff` in a Runner-registered Project. Optional `args` scopes paths or adds git diff flags.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional git diff arguments / path specs (e.g. [\"--stat\"] or [\"src/main.rs\"])."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                }
            }
        },
        "ApplyUnifiedDiffRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project", "diff"],
            "description": "Apply one bounded raw standard unified diff to a Runner-registered Project after the tool's own safety/applicability preflight.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "diff": {
                    "type": "string",
                    "maxLength": MAX_UNIFIED_DIFF_BYTES,
                    "description": UNIFIED_DIFF_FIELD_DESCRIPTION
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                },
                "deny_sensitive_paths": {
                    "type": "boolean",
                    "default": true,
                    "description": "Optional fail-safe sensitive-path policy. Defaults to true; when true, any sensitive-path warning blocks mutation before git apply --check is dispatched."
                }
            }
        },
        "GitRestorePathsRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project", "paths"],
            "description": "Restore selected tracked project-relative paths with git restore. Mutation with side effects.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Project-relative tracked paths to restore."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                }
            }
        },
        "DiscardUntrackedRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project", "paths"],
            "description": "Discard selected untracked project-relative files with git clean -f. Mutation with side effects.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Project-relative untracked paths to remove."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                }
            }
        },
        "StartProjectShellJobRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project", "command"],
            "description": "Start an async background shell job in a Runner-registered Project. Execution with side effects; returns a job_id to poll with getRuntimeJobStatus.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "command": {
                    "type": "string",
                    "description": "Shell command to run asynchronously in the project directory."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional maximum runtime in seconds."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional project-relative working directory. The owning Runner enforces its cwd policy."
                }
            }
        },
        "ListProjectFilesRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project"],
            "description": "List files in a Runner-registered Project directory. Read-only bounded listing.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                },
                "path": {
                    "type": "string",
                    "description": "Optional project-relative directory to list (default: project root)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Optional maximum number of entries to return."
                }
            }
        },
        "SearchProjectTextRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project", "pattern"],
            "description": "Search text inside a Runner-registered Project. Read-only bounded matches.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "pattern": {
                    "type": "string",
                    "description": "Search pattern. Interpreted as a regular expression by default. For identifiers, source snippets, paths, and other exact text, prefer pattern_mode=literal; use regex when regex syntax is intentional."
                },
                "pattern_mode": {
                    "type": "string",
                    "enum": ["regex", "literal"],
                    "default": "regex",
                    "description": "Pattern interpretation: regex (default, backward compatible) or literal. Prefer literal unless regex syntax is intentional."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                },
                "path": {
                    "type": "string",
                    "description": "Optional project-relative directory to scope the search (default: project root)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Optional maximum number of matches to return."
                },
                "context_before": {
                    "type": "integer",
                    "description": "Optional context lines before each match; clamped server-side to 20."
                },
                "context_after": {
                    "type": "integer",
                    "description": "Optional context lines after each match; clamped server-side to 20."
                },
                "include_globs": {
                    "type": "array",
                    "maxItems": 32,
                    "items": {"type": "string", "minLength": 1, "maxLength": 256},
                    "description": "Optional ripgrep include globs. Negated and protected-path globs are rejected."
                },
                "exclude_globs": {
                    "type": "array",
                    "maxItems": 32,
                    "items": {"type": "string", "minLength": 1, "maxLength": 256},
                    "description": "Optional additive ripgrep exclude globs; built-in secret/build exclusions remain active."
                },
                "result_mode": {
                    "type": "string",
                    "enum": ["matches", "files_with_matches", "count"],
                    "default": "matches",
                    "description": "Result shape. limit applies to matches in matches mode and files in other modes."
                },
                "timeout_secs": {
                    "type": "integer",
                    "default": 30,
                    "description": "Optional search timeout in seconds. Server clamps the value to 1..120; out-of-range integers are accepted and clamped rather than schema-rejected."
                }
            }
        },
        "ListJobsRequest": {
            "type": "object",
            "additionalProperties": false,
            "description": "List bounded caller-visible runtime Job summaries. Exact project/session_id/status filters use AND semantics before limit; never returns stdout/stderr bodies.",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Optional maximum number of matching Job summaries to return."
                },
                "status": {
                    "type": "string",
                    "description": "Optional exact status filter (e.g. running, completed, failed)."
                },
                "project": {
                    "type": "string",
                    "maxLength": 512,
                    "description": "Optional exact full runtime Project id."
                },
                "session_id": {
                    "type": "string",
                    "maxLength": 128,
                    "description": "Optional exact workflow Session id."
                }
            }
        },
        "JobTailRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["job_id"],
            "description": "Read bounded stdout/stderr tails for a runtime job. Read-only.",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "Runtime job id returned by run_job."
                },
                "tail_lines": {
                    "type": "integer",
                    "description": "Optional number of trailing lines to return per stream."
                }
            }
        },
        "RunShellRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["project", "command"],
            "description": "Run a shell command in a Runner-registered Project. Executable with side effects; result output includes command_started, command_ok, failure_kind, and tool_failure semantics.",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Runner-registered runtime Project id from listProjects, such as `agent:<client_id>:<project_id>`."
                },
                "command": {
                    "type": "string",
                    "description": "Shell command to run in the project directory."
                },
                "session_id": {
                    "type": "string",
                    "description": SESSION_ID_FIELD_DESCRIPTION
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional maximum runtime in seconds."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional project-relative working directory. The owning Runner enforces its cwd policy."
                }
            }
        },
        "ToolSpec": {
            "type": "object",
            "required": ["name", "description", "inputSchema", "outputSchema", "annotations"],
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "inputSchema": { "type": "object", "additionalProperties": true },
                "outputSchema": { "type": "object", "additionalProperties": true },
                "annotations": {
                    "type": "object",
                    "description": "Tool annotations / client hints.",
                    "additionalProperties": true
                }
            }
        },
        "ToolSummary": {
            "type": "object",
            "required": ["name", "category", "risk", "read_only", "requires_project"],
            "description": "Compact tool summary returned by listRuntimeTools when summary_only=true.",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "category": { "type": "string" },
                "risk": { "type": "string" },
                "read_only": { "type": "boolean" },
                "requires_project": { "type": "boolean" },
                "annotations": {
                    "type": "object",
                    "description": "Tool annotations / client hints.",
                    "additionalProperties": true
                }
            }
        },
        "ToolsListResponse": {
            "type": "object",
            "required": ["success", "tools", "names", "count"],
            "description": "Runtime tool list. No-arg calls return the full MCP-compatible ToolSpec list for schema debugging. Bounded calls can return compact ToolSummary entries without schemas. GPT Actions should prefer tool_manifest for daily discovery.",
            "properties": {
                "success": { "type": "boolean" },
                "tools": {
                    "type": "array",
                    "items": {
                        "oneOf": [
                            { "$ref": "#/components/schemas/ToolSpec" },
                            { "$ref": "#/components/schemas/ToolSummary" }
                        ]
                    }
                },
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Accepted runtime tool names, in spec order."
                },
                "count": {
                    "type": "integer",
                    "description": "Number of tools in `tools`/`names`."
                },
                "total_count": {
                    "type": "integer",
                    "description": "Total number of model-visible runtime tools before filters."
                },
                "filtered_count": {
                    "type": "integer",
                    "description": "Number of tools matching category/features before limit."
                },
                "truncated": {
                    "type": "boolean",
                    "description": "Whether the response was truncated by limit."
                },
                "category": {
                    "type": ["string", "null"],
                    "description": "Requested category filter, when provided."
                },
                "features": {
                    "type": ["string", "null"],
                    "description": "Requested feature filter, when provided."
                },
                "limit": {
                    "type": ["integer", "null"],
                    "description": "Effective bounded discovery limit, when a bounded request was used."
                },
                "categories": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "description": "Optional grouping by family: inspect, git, review, validation, patch, edit, shell, jobs, runtime, cleanup. A tool may appear in more than one category."
                },
                "recommended_flows": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional short GPT flow hints for common tool sequences."
                },
                "hint": {
                    "type": "string",
                    "description": "Short guidance for using bounded discovery."
                },
                "recommended_next": {
                    "type": "string",
                    "description": "Recommended next discovery action."
                }
            }
        },
        "ToolResult": {
            "type": "object",
            "required": ["success", "output"],
            "properties": {
                "success": { "type": "boolean" },
                "output": {
                    "description": "Tool-specific JSON output.",
                    "oneOf": [
                        {
                            "type": "object",
                            "additionalProperties": true,
                            "properties": {
                                "handoff_brief": {
                                    "$ref": "#/components/schemas/HandoffBrief"
                                }
                            }
                        },
                        {
                            "type": ["array", "string", "number", "boolean", "null"]
                        }
                    ]
                },
                "error": {
                    "type": "string",
                    "description": "Human-readable error when success is false."
                }
            }
        },
        "ErrorResponse": {
            "type": "object",
            "properties": {
                "status": { "type": "integer" },
                "error": { "type": "string" }
            }
        },
        "RegisterProjectRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["client_id", "id", "name", "path"],
            "description": "Register an existing directory as a WebCodex Project on the selected Runner. Mutation with side effects; executes on the Runner and is constrained by Runner policy.",
            "properties": {
                "client_id": {"type": "string", "description": "Registered Runner client_id from the `list_runners` runtime tool."},
                "id": {"type": "string", "description": "Project id (ASCII letters, digits, '-', '_'; no slash)."},
                "name": {"type": "string", "description": "Human-readable project name, bounded to 120 UTF-8 bytes server-side."},
                "path": {"type": "string", "description": "Absolute directory path on the Runner host."},
                "description": {"type": "string", "description": "Optional project description, bounded to 500 UTF-8 bytes server-side."},
                "allow_patch": {"type": "boolean", "description": "Allow patch operations on this project (default true)."},
                "overwrite": {"type": "boolean", "description": "Overwrite an existing project config file (default false)."}
            }
        },
        "CreateProjectRequest": {
            "type": "object",
            "additionalProperties": false,
            "required": ["client_id", "id", "name", "path"],
            "description": "Create a new directory, or explicitly adopt an already-existing empty directory, on the selected Runner and register it as a WebCodex Project. Mutation with side effects; executes on the Runner and is constrained by Runner policy.",
            "properties": {
                "client_id": {"type": "string", "description": "Registered Runner client_id from the `list_runners` runtime tool."},
                "id": {"type": "string", "description": "Project id (ASCII letters, digits, '-', '_'; no slash)."},
                "name": {"type": "string", "description": "Human-readable project name, bounded to 120 UTF-8 bytes server-side."},
                "path": {"type": "string", "description": "Absolute directory path on the Runner host. For createProject, an already-existing path must be empty and adopt_existing_empty must be true."},
                "description": {"type": "string", "description": "Optional project registration description, bounded to 500 UTF-8 bytes server-side. The 'empty' template never creates project files from this metadata; the 'basic' template also includes it in generated README.md content."},
                "allow_patch": {"type": "boolean", "description": "Allow patch operations on this project (default true)."},
                "template": {"type": "string", "description": "Template: 'empty' (default; generates no project files) or 'basic' (generates README.md and .gitignore). git_init is a separate explicit side effect."},
                "git_init": {"type": "boolean", "description": "Initialize git in the new directory (default false)."},
                "adopt_existing_empty": {"type": "boolean", "description": "Adopt an already-existing empty target directory instead of requiring create_project to create it (default false). Non-empty directories are always rejected."},
                "overwrite": {"type": "boolean", "description": "Overwrite an existing project config file (default false)."}
            }
        }
    });
    insert_tool_call_request_flattened_arg_properties(&mut schemas);
    insert_tool_call_request_reserved_properties(&mut schemas);
    insert_apply_unified_diff_result_schema(&mut schemas);
    insert_handoff_brief_schema(&mut schemas);
    schemas
}

fn insert_apply_unified_diff_result_schema(schemas: &mut Value) {
    let schema = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "apply_unified_diff")
        .map(|spec| spec.output_schema)
        .expect("apply_unified_diff must publish an output schema");
    schemas
        .as_object_mut()
        .expect("OpenAPI schemas must be an object")
        .insert("ApplyUnifiedDiffToolResult".to_string(), schema);
}

fn insert_handoff_brief_schema(schemas: &mut Value) {
    let schema = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "session_handoff_summary")
        .and_then(|spec| {
            spec.output_schema
                .pointer("/properties/output/properties/handoff_brief")
                .cloned()
        })
        .expect("session_handoff_summary must publish handoff_brief");
    schemas
        .as_object_mut()
        .expect("OpenAPI schemas must be an object")
        .insert("HandoffBrief".to_string(), schema);
}

fn tool_call_request_properties_mut(
    schemas: &mut Value,
) -> Option<&mut serde_json::Map<String, Value>> {
    schemas
        .pointer_mut("/ToolCallRequest/properties")
        .and_then(Value::as_object_mut)
}

fn insert_tool_call_request_flattened_arg_properties(schemas: &mut Value) {
    // The GPT Actions schema is a model-facing contract. Hidden runtime
    // compatibility specs remain parser/dispatch contracts and must not add
    // selector names or flattened fields here.
    insert_tool_call_request_flattened_arg_properties_for_specs(schemas, registered_tool_specs());
}

fn insert_tool_call_request_flattened_arg_properties_for_specs(
    schemas: &mut Value,
    specs: impl IntoIterator<Item = crate::tool_runtime::ToolSpec>,
) {
    let Some(properties) = tool_call_request_properties_mut(schemas) else {
        return;
    };

    let mut schemas_by_field = BTreeMap::<String, BTreeMap<String, (String, Value)>>::new();
    for spec in specs {
        let input_properties = spec.input_schema["properties"].as_object();
        for field in generic_tool_call_flattened_args_for_spec(&spec) {
            if properties.contains_key(&field) {
                continue;
            }
            let schema =
                if let Some(input_schema) = input_properties.and_then(|props| props.get(&field)) {
                    let schema = if field == "execution_context" {
                        let mut schema = input_schema.clone();
                        schema["description"] =
                            Value::String(FLATTENED_TOOL_ARG_DESCRIPTION.to_string());
                        Some(schema)
                    } else {
                        flattened_tool_arg_schema_from_input(input_schema)
                    };
                    let Some(schema) = schema else {
                        continue;
                    };
                    schema
                } else {
                    flattened_tool_arg_schema("string")
                };

            let semantic_key = flattened_tool_arg_semantic_key(&schema);
            let rendered =
                serde_json::to_string(&schema).expect("flattened OpenAPI schema must serialize");
            let alternatives = schemas_by_field.entry(field).or_default();
            match alternatives.entry(semantic_key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert((rendered, schema));
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    if rendered < entry.get().0 {
                        entry.insert((rendered, schema));
                    }
                }
            }
        }
    }

    for (field, schemas) in schemas_by_field {
        if let Some(schema) = flattened_tool_arg_schema_union(schemas) {
            properties.insert(field, schema);
        }
    }
}

fn insert_tool_call_request_reserved_properties(schemas: &mut Value) {
    let Some(properties) = tool_call_request_properties_mut(schemas) else {
        return;
    };

    properties.insert(
        TOOL_CALL_TOOL_FIELD.to_string(),
        json!({
            "type": "string",
            "description": format!(
                "Model-visible runtime tool name. Accepted model-facing values: {}. Prefer tool_manifest for daily discovery; use listRuntimeTools for schema debugging.",
                crate::tool_runtime::tool_definition::model_visible_tool_names_csv()
            )
        }),
    );
    properties.insert(
        TOOL_CALL_PARAMS_FIELD.to_string(),
        json!({
            "type": "object",
            "description": "Canonical tool-specific arguments object for non-Action clients. A non-null value takes precedence; a null wrapper does not suppress flattened top-level fields. GPT Actions should prefer flattened top-level fields.",
            "nullable": true,
            "additionalProperties": true
        }),
    );
    properties.insert(
        TOOL_CALL_RECORDING_SESSION_ID_FIELD.to_string(),
        json!({
            "type": "string",
            "description": "Optional recorder metadata for the generic wrapper call. Pass an existing explicit wc_sess_* id to record this call in that exact Session ledger and preserve trusted provenance/project-boundary checks. This field is stripped before concrete tool parsing and never supplies business Session guards or execution_context. Use top-level session_id only when the selected model-visible tool declares it as business input."
        }),
    );
}

#[cfg(test)]
#[path = "openapi_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "openapi_patch_description_tests.rs"]
mod patch_description_tests;
