use serde_json::{json, Value};

use super::common::{object_schema, with_optional_session_id};

pub fn git_review_summary_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "base_commit",
            "string",
            "Exact 40-hex Git commit object id used to compute the merge-base.",
            true,
        ),
        (
            "head_commit",
            "string",
            "Exact 40-hex Git commit object id reviewed from merge-base to head.",
            true,
        ),
    ]));
    for field in ["base_commit", "head_commit"] {
        schema["properties"][field]["minLength"] = Value::from(40);
        schema["properties"][field]["maxLength"] = Value::from(40);
        schema["properties"][field]["pattern"] = Value::from("^[0-9A-Fa-f]{40}$");
    }
    schema
}

pub fn show_changes_input_schema() -> Value {
    object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "session_id",
            "string",
            "Optional wc_sess_* id to summarize with the git changes.",
            false,
        ),
        (
            "include_diff",
            "boolean",
            "Include bounded diff hunks (default false).",
            false,
        ),
        (
            "max_hunks",
            "integer",
            "Maximum hunks to return when include_diff=true (clamped).",
            false,
        ),
        (
            "max_hunk_lines",
            "integer",
            "Maximum lines per hunk when include_diff=true (clamped).",
            false,
        ),
        (
            "session_event_limit",
            "integer",
            "Maximum recent session events to include (clamped).",
            false,
        ),
    ]))
}

pub fn git_status_input_schema() -> Value {
    object_schema(with_optional_session_id(vec![(
        "project",
        "string",
        "Configured project id.",
        true,
    )]))
}

pub fn git_commit_paths_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "expected_head",
            "string",
            "Exact current 40-hex HEAD fence; normally copy show_changes.head.commit immediately before committing.",
            true,
        ),
        (
            "paths",
            "array",
            "Exact project-relative file paths to commit. Directories, project root, sensitive paths, and unchanged paths are rejected.",
            true,
        ),
        (
            "message",
            "string",
            "Commit message. Bounded and never persisted in model-facing audit previews.",
            true,
        ),
    ]));
    schema["properties"]["expected_head"]["minLength"] = Value::from(40);
    schema["properties"]["expected_head"]["maxLength"] = Value::from(40);
    schema["properties"]["expected_head"]["pattern"] = Value::from("^[0-9A-Fa-f]{40}$");
    schema["properties"]["paths"]["minItems"] = Value::from(1);
    schema["properties"]["paths"]["maxItems"] = Value::from(32);
    schema["properties"]["paths"]["items"] = json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 512
    });
    schema["properties"]["message"]["minLength"] = Value::from(1);
    schema["properties"]["message"]["maxLength"] = Value::from(1000);
    schema
}

pub fn git_diff_hunks_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "paths",
            "array",
            "Optional project-relative paths to scope diff.",
            false,
        ),
        (
            "max_hunks",
            "integer",
            "Maximum hunks to return (clamped).",
            false,
        ),
        (
            "max_hunk_lines",
            "integer",
            "Maximum lines per hunk (clamped).",
            false,
        ),
        (
            "max_page_bytes",
            "integer",
            "Raw producer page budget in bytes, independent of the final serialized model result. Defaults to 64 KiB and is bounded to 16..192 KiB so ordinary Runner result retention retains framing headroom.",
            false,
        ),
        (
            "cached",
            "boolean",
            "Use staged diff via git diff --cached.",
            false,
        ),
        (
            "base_commit",
            "string",
            "Optional exact 40-hex Git commit object id; requires head_commit and committed-range mode.",
            false,
        ),
        (
            "head_commit",
            "string",
            "Optional exact 40-hex Git commit object id reviewed from the single merge-base; requires base_commit.",
            false,
        ),
        (
            "continuation",
            "string",
            "Opaque continuation returned by a previous git_diff_hunks page. When continuing, repeat the exact original diff scope and paging inputs unchanged (base_commit/head_commit for committed mode, cached/worktree mode, paths, max_hunks, max_hunk_lines, and max_page_bytes); the token is scope-bound and does not reconstruct omitted request fields. Continuation recovers later records, never lines omitted from the current hunk.",
            false,
        ),
    ]));
    schema["properties"]["max_page_bytes"]["minimum"] =
        Value::from(webcodex_core::runtime_contract::MIN_GIT_DIFF_HUNKS_PAGE_BYTES);
    schema["properties"]["max_page_bytes"]["maximum"] =
        Value::from(webcodex_core::runtime_contract::MAX_GIT_DIFF_HUNKS_PAGE_BYTES);
    schema["properties"]["max_page_bytes"]["default"] =
        Value::from(webcodex_core::runtime_contract::DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES);
    for field in ["base_commit", "head_commit"] {
        schema["properties"][field]["minLength"] = Value::from(40);
        schema["properties"][field]["maxLength"] = Value::from(40);
        schema["properties"][field]["pattern"] = Value::from("^[0-9A-Fa-f]{40}$");
    }
    schema["properties"]["continuation"]["maxLength"] =
        Value::from(webcodex_core::runtime_contract::GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES);
    schema["allOf"] = json!([
        {
            "if": { "required": ["base_commit"] },
            "then": {
                "required": ["head_commit"],
                "not": { "required": ["cached"] }
            }
        },
        {
            "if": { "required": ["head_commit"] },
            "then": {
                "required": ["base_commit"],
                "not": { "required": ["cached"] }
            }
        },
        {
            "if": { "required": ["cached"] },
            "then": {
                "not": {
                    "anyOf": [
                        { "required": ["base_commit"] },
                        { "required": ["head_commit"] }
                    ]
                }
            }
        }
    ]);
    schema
}

pub fn git_log_input_schema() -> Value {
    object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "limit",
            "integer",
            "Maximum commits to return (default 20, clamped to 1..100).",
            false,
        ),
        (
            "skip",
            "integer",
            "Number of recent commits to skip (default 0, clamped to 0..10000).",
            false,
        ),
    ]))
}
