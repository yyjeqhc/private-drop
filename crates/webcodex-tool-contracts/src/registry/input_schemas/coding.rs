use serde_json::{json, Value};

pub fn work_on_project_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {
                "type": "string",
                "minLength": 1,
                "description": "Existing runtime project id. Use project + instruction for an existing project; do not combine project with client_id or path."
            },
            "client_id": {
                "type": "string",
                "minLength": 1,
                "description": "Runner client_id for the path form. Use client_id + path + instruction together; do not combine with project."
            },
            "path": {
                "type": "string",
                "minLength": 1,
                "description": "Runner-owned absolute directory path for the path form. In checkout mode it is the working checkout to resolve/register; in worktree mode it is the source Git checkout. Use with client_id + instruction; do not combine with project. Runner filesystem authority remains authoritative."
            },
            "mode": {
                "type": "string",
                "enum": ["checkout", "worktree"],
                "default": "checkout",
                "description": "Optional bootstrap mode. Omitted or checkout preserves existing behavior exactly. worktree is supported only with client_id + path and asks the Runner to create/recover an isolated managed detached worktree, register it as an ordinary Project, then start the Workflow Session."
            },
            "base_ref": {
                "type": "string",
                "minLength": 1,
                "maxLength": 1024,
                "description": "Optional Git ref only for mode=worktree. The Runner resolves it inside the source repository to an exact commit SHA before creating the detached worktree. On a fresh bootstrap omission means the source checkout's current HEAD; on exact session resume omission keeps the registered managed worktree's stored exact base. The Server never interprets this ref."
            },
            "instruction": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4000,
                "description": "Required current user instruction. On a new task it becomes the root task title; when session_id is provided it is appended to the existing Workflow Session ledger and never overwrites the root title."
            },
            "include_project_instructions": {
                "type": "boolean",
                "default": true,
                "description": "Whether this bootstrap response should include bounded project-instruction bodies such as AGENTS.md. Defaults to true. Set false only when the caller's current model context already retains the applicable repository instructions. WebCodex does not infer that from session_id or transport identity. Instruction files are still re-observed and Workflow Session metadata is updated; this flag controls only model-facing instruction-content projection."
            },
            "include_workflow_guidance": {
                "type": "boolean",
                "default": true,
                "description": "Whether this bootstrap response should include the static built-in WebCodex coding-workflow guidance. Defaults to true. Set false only when the caller's current model context already retains that workflow guidance. WebCodex does not infer that from session_id or transport identity. This flag controls only model-facing workflow projection; it does not change Workflow Session state, authority, role selection, or execution semantics."
            },
            "include_extension_catalog": {
                "type": "boolean",
                "default": true,
                "description": "Whether startup should include a small bounded Skills/Plugins selection catalog. Defaults to true. Set false only when the caller's current model context already retains the relevant extension metadata. False skips the startup Skill/Plugin discovery observations. The catalog grants no authority, never loads Skill bodies, never creates Plugin bindings, and never substitutes for plugin_tool describe before invocation."
            },
            "session_id": {
                "type": "string",
                "pattern": "^wc_sess_[A-Za-z0-9_]+$",
                "description": "Optional explicit Workflow Session to continue exactly. It must be active and accessible and remains bound to its exact final Project; in worktree mode the Runner re-observes that registered managed Project and its source provenance instead of creating a second worktree. Failure never guesses or creates a replacement Session. This business input is distinct from wrapper recording_session_id."
            }
        },
        "required": ["instruction"],
        "additionalProperties": false,
    })
}

pub fn finish_coding_task_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {
                "type": "string",
                "description": "Required runtime project id. Use the same project used to start the task."
            },
            "session_id": {
                "type": "string",
                "description": "Required explicit wc_sess_* business Session id for the current coding task, obtained from its compatible Session bootstrap."
            },
            "include_diff": {
                "type": "boolean",
                "description": "Include bounded diff hunks in show_changes. Defaults to true."
            },
            "include_workspace": {
                "type": "boolean",
                "description": "Defaults to true. When include_handoff=true, controls whether the nested handoff summary includes its workspace block; the top-level finish workspace/show_changes check remains unchanged."
            },
            "include_hygiene": {
                "type": "boolean",
                "description": "Include workspace_hygiene_check output. Defaults to true."
            },
            "include_handoff": {
                "type": "boolean",
                "description": "Include session_handoff_summary output. Defaults to true."
            },
            "include_validation_summary": {
                "type": "boolean",
                "description": "Include deterministic validation-like session ledger event summary when available. Defaults to true; minimal diagnostics require bounded tails or safe result metadata."
            },
            "summary_only": {
                "type": "boolean",
                "description": "When true, return the minimal decision-complete closeout only: workspace cleanliness/conflicts, hygiene state, bounded Job counts, final validation state/counts, tool-failure actionability counts, canonical task_outcome, evidence_integrity, warnings, and suggested_next_actions. Omits project/session identity, permissions, review/work/change/handoff provenance, facts/evidence history/informational notes, command text, stdout/stderr, event history, tails, excerpts, and detailed validation history."
            }
        },
        "required": ["project", "session_id"],
        "additionalProperties": false,
    })
}
