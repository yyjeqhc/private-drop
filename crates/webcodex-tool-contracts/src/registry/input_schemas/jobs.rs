use serde_json::{json, Value};

use super::common::{object_schema, with_optional_session_id};
use webcodex_core::runner_protocol::{
    DETACHED_IDEMPOTENCY_KEY_MAX_BYTES, JOB_TERMINAL_RETENTION_SECS, PROCESS_ARG_MAX_BYTES,
    PROCESS_ARG_MAX_COUNT, PROCESS_CWD_MAX_BYTES, PROCESS_EXECUTABLE_MAX_BYTES,
    PROCESS_STDIN_MAX_BYTES, PROCESS_TIMEOUT_MAX_SECS, RAW_SHELL_COMMAND_MAX_BYTES,
    SCRIPT_ARGV_MAX_BYTES, SCRIPT_ARG_MAX_BYTES, SCRIPT_ARG_MAX_COUNT, SCRIPT_CWD_MAX_BYTES,
    SCRIPT_MAX_BYTES, SCRIPT_STDIN_MAX_BYTES, SCRIPT_TIMEOUT_MAX_SECS,
};
use webcodex_core::runtime_contract::STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS;
use webcodex_core::workflow_session_contract::{
    MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS, TOOL_ACCEPTED_EXIT_CODES_FIELD,
    TOOL_ASSERTION_NAME_FIELD, TOOL_RESULT_EXPECTATION_FIELD,
};

fn with_optional_validation_assertion(mut schema: Value) -> Value {
    schema["properties"][TOOL_ASSERTION_NAME_FIELD] = json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS,
        "description": "Optional stable human-readable validation assertion label. Reuse the same value after a fix to correlate later validation evidence even when the command changes. It never grants execution authority, changes success, or bypasses structured validation proof. It is inert unless the existing purpose/evidence rules classify the execution as validation-like."
    });
    schema
}

fn with_optional_result_expectation(mut schema: Value, accepted_exit_codes: bool) -> Value {
    schema["properties"][TOOL_RESULT_EXPECTATION_FIELD] = json!({
        "type": "string",
        "enum": ["success", "failure", "observe"],
        "description": "Optional pre-execution result expectation. Omit (or use success) for the normal success-required path; failure means a completed known business failure is the expected negative-test result; observe means either a completed known success or completed known business failure is acceptable. It changes only Session ledger expectation classification, never authorization, the real ToolResult, exit code, timeout, guard, transport, or outcome-unknown semantics."
    });
    if accepted_exit_codes {
        schema["properties"][TOOL_ACCEPTED_EXIT_CODES_FIELD] = json!({
            "type": "array",
            "minItems": 1,
            "maxItems": 32,
            "items": { "type": "integer" },
            "description": "Optional exact set of completed process exit codes that are valid observations (for example [0,1] for boolean probe commands). A completed exit code outside the set is an expectation mismatch. This is more precise than observe and may be combined only with result_expectation=observe or with result_expectation omitted. Timeout, cancellation, transport failure, guard/permission rejection, malformed results, and outcome-unknown remain failures."
        });
    }
    schema
}

pub fn run_process_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Configured project id.", true),
        (
            "executable",
            "string",
            "Executable name or path, resolved through the Runner execution environment. Native executables use literal argv. Windows .cmd/.bat shims use Runner-owned cmd.exe conversion with AutoRun and delayed expansion disabled; no model shell string. Batch paths/arguments reject quotes, %, !, ^, control characters and trailing backslashes before spawn; use a native runtime for these values. Batch command lines are limited to 8000 UTF-16 units and require a local drive cwd; UNC cwd is rejected before spawn.",
            true,
        ),
        (
            "args",
            "array",
            "Ordered argv values passed literally to the child process. Defaults to an empty array. Runtime validation allows at most 16,000 UTF-8 bytes across executable and argv boundaries.",
            false,
        ),
        (
            "cwd",
            "string",
            "Project-relative working directory. Omit, empty string, or '.' for the project root. Named Session SSH resources are unsupported for run_process.",
            false,
        ),
        (
            "stdin",
            "string",
            "Optional bounded UTF-8 stdin payload passed through a pipe.",
            false,
        ),
        (
            "timeout_secs",
            "integer",
            "Total process runtime budget in seconds (1..=3600, default 60). Short work returns synchronously; longer work keeps the same execution and returns job_id when durable structured execution is available.",
            false,
        ),
        (
            "sync_wait_secs",
            "integer",
            "Optional synchronous grace before durable Job handoff. Omit to use 10 seconds bounded by the total timeout; explicit values are 1..=60 and must not exceed timeout_secs. It only controls how long the Server waits for the already-started execution before exposing that same execution as a Job; it does not extend the total runtime timeout or rerun work.",
            false,
        ),
        (
            "purpose",
            "string",
            "Declared execution intent: validation, test, build, format, release, diagnostic, operation, or other. This records evidence and never changes authorization.",
            false,
        ),
    ]));
    schema["properties"]["executable"]["minLength"] = json!(1);
    schema["properties"]["executable"]["maxLength"] = json!(PROCESS_EXECUTABLE_MAX_BYTES);
    schema["properties"]["args"]["maxItems"] = json!(PROCESS_ARG_MAX_COUNT);
    schema["properties"]["args"]["items"]["maxLength"] = json!(PROCESS_ARG_MAX_BYTES);
    schema["properties"]["args"]["default"] = json!([]);
    schema["properties"]["stdin"] = json!({
        "anyOf": [
            {"type": "string", "maxLength": PROCESS_STDIN_MAX_BYTES},
            {"type": "null"}
        ],
        "description": "Optional bounded UTF-8 stdin payload passed through a pipe; null and omission both mean no stdin payload."
    });
    schema["properties"]["cwd"]["maxLength"] = json!(PROCESS_CWD_MAX_BYTES);
    schema["properties"]["timeout_secs"]["minimum"] = json!(1);
    schema["properties"]["timeout_secs"]["maximum"] = json!(PROCESS_TIMEOUT_MAX_SECS);
    schema["properties"]["timeout_secs"]["default"] = json!(60);
    schema["properties"]["sync_wait_secs"]["minimum"] = json!(1);
    schema["properties"]["sync_wait_secs"]["maximum"] =
        json!(STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS);
    schema["properties"]["purpose"]["enum"] = json!([
        "validation",
        "test",
        "build",
        "format",
        "release",
        "diagnostic",
        "operation",
        "other"
    ]);
    with_optional_result_expectation(with_optional_validation_assertion(schema), true)
}

pub fn run_detached_process_input_schema() -> Value {
    let mut schema = run_process_input_schema();
    schema["properties"]
        .as_object_mut()
        .expect("run_process schema properties")
        .remove(TOOL_ASSERTION_NAME_FIELD);
    schema["properties"]
        .as_object_mut()
        .expect("run_process schema properties")
        .remove(TOOL_RESULT_EXPECTATION_FIELD);
    schema["properties"]
        .as_object_mut()
        .expect("run_process schema properties")
        .remove(TOOL_ACCEPTED_EXIT_CODES_FIELD);
    schema["properties"]
        .as_object_mut()
        .expect("run_process schema properties")
        .remove("sync_wait_secs");
    schema["properties"]["idempotency_key"] = json!({
        "type": "string",
        "minLength": 1,
        "maxLength": DETACHED_IDEMPOTENCY_KEY_MAX_BYTES,
        "description": format!("Required bounded caller-chosen key for this detached initiation. While the logical Job is active or retained for {JOB_TERMINAL_RETENTION_SECS} seconds after terminal completion, reusing the same key resolves to that Job and cannot redispatch its payload. After retained history expires the key may identify a new execution, so never reuse an expired key as a retry token. After Server restart an existing retained Job is returned for recovery rather than guessing that a resent body matches.")
    });
    schema["required"]
        .as_array_mut()
        .expect("run_process schema required array")
        .push(json!("idempotency_key"));
    schema["properties"]["cwd"]["description"] = json!(
        "Project-relative working directory. Omit, empty string, or '.' for the project root. Named Session SSH resources are unsupported for run_detached_process."
    );
    schema["properties"]["timeout_secs"]["description"] = json!(
        "Total detached process runtime budget in seconds (1..=3600, default 60). Admission returns the stable Job identity without waiting for terminal completion."
    );
    schema
}

pub fn run_script_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Configured project id.", true),
        (
            "language",
            "string",
            "Required semantic script language. JavaScript uses Runner-resolved Node.js with fixed .mjs ESM semantics. TypeScript uses Runner-resolved Node.js native erasable type stripping from a fixed .mts ESM file and requires Node.js 22.6.0 or newer. The Runner owns any runtime compatibility flags; callers cannot provide a runtime path or runtime flags. Session default_shell never overrides this field.",
            true,
        ),
        (
            "script",
            "string",
            "Bounded UTF-8 script content transported as typed data. It is written to a Runner-owned temporary file and never placed in a shell command string.",
            true,
        ),
        (
            "args",
            "array",
            "Ordered script arguments passed as independent argv values. Defaults to an empty array; values are never interpolated into the script body.",
            false,
        ),
        (
            "stdin",
            "string",
            "Optional bounded UTF-8 stdin payload piped independently to the script process. Null and omission both mean no stdin payload.",
            false,
        ),
        (
            "cwd",
            "string",
            "Project-relative working directory. Omit, empty string, or '.' for the project root. Named Session SSH resources are unsupported for run_script.",
            false,
        ),
        (
            "timeout_secs",
            "integer",
            "Total script runtime budget in seconds (1..=3600, default 60). Short work returns synchronously; longer work keeps the same execution and returns job_id when durable structured execution is available.",
            false,
        ),
        (
            "sync_wait_secs",
            "integer",
            "Optional synchronous grace before durable Job handoff. Omit to use 10 seconds bounded by the total timeout; explicit values are 1..=60 and must not exceed timeout_secs. It only controls how long the Server waits for the already-started execution before exposing that same execution as a Job; it does not extend the total runtime timeout or rerun work.",
            false,
        ),
        (
            "purpose",
            "string",
            "Declared execution intent: validation, test, build, format, release, diagnostic, operation, or other. This records evidence and never changes authorization.",
            false,
        ),
    ]));
    schema["properties"]["language"]["enum"] =
        json!(["sh", "bash", "powershell", "javascript", "typescript"]);
    schema["properties"]["script"]["minLength"] = json!(1);
    schema["properties"]["script"]["maxLength"] = json!(SCRIPT_MAX_BYTES);
    schema["properties"]["args"]["maxItems"] = json!(SCRIPT_ARG_MAX_COUNT);
    schema["properties"]["args"]["items"]["maxLength"] = json!(SCRIPT_ARG_MAX_BYTES);
    schema["properties"]["args"]["default"] = json!([]);
    schema["properties"]["args"]["description"] = json!(format!(
        "Ordered script arguments passed as independent argv values. Defaults to an empty array. At most {SCRIPT_ARG_MAX_COUNT} entries, each at most {SCRIPT_ARG_MAX_BYTES} UTF-8 bytes, with at most {SCRIPT_ARGV_MAX_BYTES} bytes total including one boundary byte per value; values are never interpolated into the script body."
    ));
    schema["properties"]["stdin"] = json!({
        "anyOf": [
            {"type": "string", "maxLength": SCRIPT_STDIN_MAX_BYTES},
            {"type": "null"}
        ],
        "description": "Optional bounded UTF-8 stdin payload piped independently to the script process; null and omission both mean no stdin payload."
    });
    schema["properties"]["cwd"]["maxLength"] = json!(SCRIPT_CWD_MAX_BYTES);
    schema["properties"]["timeout_secs"]["minimum"] = json!(1);
    schema["properties"]["timeout_secs"]["maximum"] = json!(SCRIPT_TIMEOUT_MAX_SECS);
    schema["properties"]["timeout_secs"]["default"] = json!(60);
    schema["properties"]["sync_wait_secs"]["minimum"] = json!(1);
    schema["properties"]["sync_wait_secs"]["maximum"] =
        json!(STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS);
    schema["properties"]["purpose"]["enum"] = json!([
        "validation",
        "test",
        "build",
        "format",
        "release",
        "diagnostic",
        "operation",
        "other"
    ]);
    with_optional_result_expectation(with_optional_validation_assertion(schema), false)
}

pub fn run_shell_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Configured project id.", true),
        ("command", "string", "Shell command to run.", true),
        (
            "timeout_secs",
            "integer",
            "Total command timeout in seconds (1..=120, default 60). Explicit values above 60 may hand off the same original execution as a durable Job when the Runner supports async shell Jobs; default 60-second calls remain synchronous.",
            false,
        ),
        (
            "cwd",
            "string",
            "Working directory contract: without a Session SSH resource, omit, empty string, or '.' selects the project root and any other value is project-relative. With a named Session SSH resource, cwd is a remote path checked by the remote shell instead of the Runner project-root policy.",
            false,
        ),
        (
            "purpose",
            "string",
            "Declared execution intent: validation, test, build, format, release, diagnostic, operation, or other. This records evidence and never changes authorization.",
            false,
        ),
        (
            "shell",
            "string",
            "Optional explicit command language: sh or bash. When omitted, local run_shell uses sh, a Runner-backed run_shell uses that Runner's configured shell, and a named Session SSH resource uses the remote login shell. The response always records the actual selection.",
            false,
        ),
    ]));
    schema["properties"]["purpose"]["enum"] = json!([
        "validation",
        "test",
        "build",
        "format",
        "release",
        "diagnostic",
        "operation",
        "other"
    ]);
    schema["properties"]["shell"]["enum"] = json!(["sh", "bash"]);
    schema["properties"]["command"]["maxLength"] = json!(RAW_SHELL_COMMAND_MAX_BYTES);
    schema["properties"]["command"]["description"] = json!(format!(
        "Shell command to run. At most {RAW_SHELL_COMMAND_MAX_BYTES} UTF-8 bytes; use run_script for larger program text and stdin/files/artifacts for large data."
    ));
    schema["properties"]["timeout_secs"]["minimum"] = json!(1);
    schema["properties"]["timeout_secs"]["maximum"] = json!(120);
    schema["properties"]["timeout_secs"]["default"] = json!(60);
    with_optional_result_expectation(with_optional_validation_assertion(schema), false)
}

pub fn run_job_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Configured project id.", true),
        (
            "command",
            "string",
            "Shell command to run asynchronously.",
            true,
        ),
        (
            "timeout_secs",
            "integer",
            "Maximum runtime in seconds.",
            false,
        ),
        (
            "cwd",
            "string",
            "Working directory contract: without a Session SSH resource, omit, empty string, or '.' selects the project root and any other value is project-relative. With a named Session SSH resource, cwd is a remote path checked by the remote shell instead of the Runner project-root policy.",
            false,
        ),
        (
            "purpose",
            "string",
            "Declared execution intent: validation, test, build, format, release, diagnostic, operation, or other. This records evidence and never changes authorization.",
            false,
        ),
        (
            "shell",
            "string",
            "Optional explicit command language: sh or bash. When omitted, local run_job preserves its existing bash contract, a Runner-backed run_job uses that Runner's configured shell, and a named Session SSH resource uses the remote login shell. The response always records the actual selection.",
            false,
        ),
    ]));
    schema["properties"]["purpose"]["enum"] = json!([
        "validation",
        "test",
        "build",
        "format",
        "release",
        "diagnostic",
        "operation",
        "other"
    ]);
    schema["properties"]["shell"]["enum"] = json!(["sh", "bash"]);
    schema["properties"]["command"]["maxLength"] = json!(RAW_SHELL_COMMAND_MAX_BYTES);
    schema["properties"]["command"]["description"] = json!(format!(
        "Shell command to run asynchronously. At most {RAW_SHELL_COMMAND_MAX_BYTES} UTF-8 bytes; use run_script for larger program text and stdin/files/artifacts for large data."
    ));
    with_optional_validation_assertion(schema)
}

pub fn open_session_shell_input_schema() -> Value {
    let mut schema = object_schema(vec![
        ("project", "string", "Exact Workflow Session project id.", true),
        (
            "session_id",
            "string",
            "Explicit active Workflow Session id. Current-session fallback is not used.",
            true,
        ),
        (
            "cwd",
            "string",
            "Optional initial cwd. Without a named Session SSH resource it is project-relative; with one it is a remote path. Omission uses the Session default, then the project or SSH-resource default.",
            false,
        ),
        (
            "shell",
            "string",
            "Optional Unix local-shell override: sh or bash. Omit on Windows so the Runner uses its configured PowerShell program/profile.",
            false,
        ),
    ]);
    schema["properties"]["shell"]["enum"] = json!(["sh", "bash"]);
    schema
}

pub fn session_shell_exec_input_schema() -> Value {
    let mut schema = object_schema(vec![
        ("project", "string", "Exact Workflow Session project id.", true),
        ("session_id", "string", "Explicit active Workflow Session id.", true),
        (
            "shell_id",
            "string",
            "Opaque id returned by open_session_shell.",
            true,
        ),
        (
            "command",
            "string",
            "One command evaluated by the existing long-lived shell.",
            true,
        ),
        (
            "timeout_secs",
            "integer",
            "Command timeout in seconds (1..=3600, default 60). Timeout recovery requires verified framing resynchronization; otherwise the shell is poisoned and terminated before reuse.",
            false,
        ),
        (
            "purpose",
            "string",
            "Declared execution intent recorded as evidence.",
            false,
        ),
    ]);
    schema["properties"]["timeout_secs"]["minimum"] = json!(1);
    schema["properties"]["timeout_secs"]["maximum"] = json!(3600);
    schema["properties"]["timeout_secs"]["default"] = json!(60);
    schema["properties"]["command"]["maxLength"] = json!(RAW_SHELL_COMMAND_MAX_BYTES);
    schema["properties"]["command"]["description"] = json!(format!(
        "One command evaluated by the existing long-lived shell. At most {RAW_SHELL_COMMAND_MAX_BYTES} UTF-8 bytes."
    ));
    schema["properties"]["purpose"]["enum"] = json!([
        "validation",
        "test",
        "build",
        "format",
        "release",
        "diagnostic",
        "operation",
        "other"
    ]);
    with_optional_result_expectation(schema, false)
}

pub fn session_shell_identity_input_schema() -> Value {
    object_schema(vec![
        (
            "project",
            "string",
            "Exact Workflow Session project id.",
            true,
        ),
        (
            "session_id",
            "string",
            "Explicit active Workflow Session id.",
            true,
        ),
        (
            "shell_id",
            "string",
            "Opaque id returned by open_session_shell.",
            true,
        ),
    ])
}

pub fn stop_job_input_schema() -> Value {
    object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Configured project id that must match the job project.",
            true,
        ),
        ("job_id", "string", "Existing runtime Job id to stop.", true),
        (
            "confirm",
            "boolean",
            "Must be true to stop or no-op an already-finished job; false returns confirmation_required.",
            false,
        ),
    ]))
}

pub fn job_status_input_schema() -> Value {
    object_schema(vec![
        ("job_id", "string", "Job id.", true),
        (
            "include_command_preview",
            "boolean",
            "Optional debug flag. Defaults to false; when true, includes bounded command_preview metadata. stdout/stderr bodies are never included.",
            false,
        ),
    ])
}

pub fn job_log_input_schema() -> Value {
    let mut schema = object_schema(vec![
        ("job_id", "string", "Job id.", true),
        (
            "offset",
            "integer",
            "Optional 1-based cursor returned by a previous call. Reads the next bounded segment.",
            false,
        ),
        (
            "tail_lines",
            "integer",
            "Optional number of trailing lines per stream. Defaults to 200 and is capped at 500.",
            false,
        ),
        (
            "after_observation_token",
            "string",
            "Opaque token from the latest Job observation. Return it unchanged; it carries bounded lifecycle and automatic log-delta state. It is bound to one job_id, not execution identity or retry authority. A Server epoch change causes an immediate conservative log reset when that Job still exists.",
            false,
        ),
        (
            "wait_secs",
            "integer",
            "Optional bounded wait in seconds (1..=60). When both after_observation_token and wait_secs are supplied, this is a single bounded wait, not a subscription or streaming connection.",
            false,
        ),
    ]);
    schema["properties"]["after_observation_token"]["maxLength"] =
        json!(webcodex_core::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN);
    schema["properties"]["wait_secs"]["minimum"] = json!(1);
    schema["properties"]["wait_secs"]["maximum"] = json!(60);
    schema
}

pub fn observe_jobs_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "maxItems": 8,
                "description": "Existing Jobs to observe in input order. Duplicate job_id values are rejected.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Existing opaque runtime Job id."
                        },
                        "after_observation_token": {
                            "type": "string",
                            "maxLength": webcodex_core::job_observation::MAX_JOB_OBSERVATION_TOKEN_LEN,
                            "description": "Optional opaque Job-bound lifecycle/log-delta token from the latest observation. Return it unchanged without interpreting its cursor state. It is not execution identity or retry authority; a stale Server epoch is immediately actionable and conservatively resets the bounded log projection."
                        }
                    },
                    "required": ["job_id"]
                }
            },
            "tail_lines": {
                "type": "integer",
                "minimum": 1,
                "maximum": 200,
                "default": 40,
                "description": "Global per-stream bound. First observations return a current tail; cursor-aware follow-ups return at most this many new or reset-recovery lines."
            },
            "wait_secs": {
                "type": "integer",
                "minimum": 1,
                "maximum": 60,
                "description": "Optional one shared bounded wait. It returns when any relevant Job changes and is never multiplied by item count."
            }
        },
        "required": ["items"]
    })
}

pub fn list_jobs_input_schema() -> Value {
    let mut schema = object_schema(vec![
        (
            "limit",
            "integer",
            "Maximum number of job summaries to return after all filters.",
            false,
        ),
        (
            "status",
            "string",
            "Optional exact status filter (e.g. running, completed, failed).",
            false,
        ),
        (
            "project",
            "string",
            "Optional exact full runtime Project id; filters only already caller-visible Jobs.",
            false,
        ),
        (
            "session_id",
            "string",
            "Optional exact Workflow Session id; filters only already caller-visible Jobs.",
            false,
        ),
    ]);
    schema["properties"]["limit"]["minimum"] = json!(1);
    schema["properties"]["limit"]["maximum"] = json!(100);
    schema["properties"]["project"]["minLength"] = json!(1);
    schema["properties"]["project"]["maxLength"] = json!(512);
    schema["properties"]["session_id"]["minLength"] = json!(1);
    schema["properties"]["session_id"]["maxLength"] = json!(128);
    schema
}
