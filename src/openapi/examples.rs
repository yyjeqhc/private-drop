use serde_json::{json, Value};

use crate::route_metadata::OpenApiExampleSet;
use crate::tool_runtime::sessions::TOOL_CALL_RECORDING_SESSION_ID_FIELD;

pub(super) fn request_examples(example_set: OpenApiExampleSet) -> Option<Value> {
    match example_set {
        OpenApiExampleSet::None => None,
        OpenApiExampleSet::RegisterProject => Some(json!({
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
        })),
        OpenApiExampleSet::CreateProject => Some(json!({
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
        })),
        OpenApiExampleSet::ListJobs => Some(json!({
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
        })),
        OpenApiExampleSet::JobTail => Some(json!({
            "byJobId": {
                "summary": "Read a bounded tail",
                "value": {
                    "job_id": "11111111-2222-3333-4444-555555555555",
                    "tail_lines": 50
                }
            }
        })),
        OpenApiExampleSet::GitStatus => Some(json!({
            "byProject": {
                "summary": "Check git status of a project",
                "value": {
                    "project": "webcodex"
                }
            }
        })),
        OpenApiExampleSet::ListProjectFiles => Some(json!({
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
        })),
        OpenApiExampleSet::ApplyUnifiedDiff => Some(json!({
            "example": {
                "summary": "Apply a small unified diff",
                "value": {
                    "project": "webcodex",
                    "diff": "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n# WebCodex\n+edited\n"
                }
            }
        })),
        OpenApiExampleSet::RunShell => Some(json!({
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
        })),
        OpenApiExampleSet::GitRestorePaths => Some(json!({
            "byProject": {
                "summary": "Restore selected tracked paths",
                "value": {
                    "project": "webcodex",
                    "paths": ["tmp_probe.txt"]
                }
            }
        })),
        OpenApiExampleSet::DiscardUntracked => Some(json!({
            "byProject": {
                "summary": "Discard selected untracked files",
                "value": {
                    "project": "webcodex",
                    "paths": ["tmp_probe.txt"]
                }
            }
        })),
        OpenApiExampleSet::ImportConversationFiles => Some(json!({
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
        })),
        OpenApiExampleSet::StartProjectShellJob => Some(json!({
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
        })),
        OpenApiExampleSet::CallRuntimeTool => Some(json!({
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
                    "tool": "show_changes",
                    "params": {"project": "webcodex"}
                }
            },
            "noParams": {
                "summary": "Argument-less tool; omit params",
                "value": {
                    "tool": "list_tools"
                }
            }
        })),
    }
}
