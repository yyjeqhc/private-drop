use serde_json::{json, Value};

use super::common::{object_schema, with_optional_session_id};
use webcodex_core::workflow_session_contract::TOOL_RESULT_EXPECTATION_FIELD;

/// `timeout_secs` for read-only structured validation tools is the total
/// runtime budget. `sync_wait_secs` optionally selects the bounded synchronous
/// grace before same-execution Job handoff; omission preserves the existing
/// grace capped by the effective timeout.
const VALIDATION_TIMEOUT_SECS_DESCRIPTION: &str =
    "Total validation runtime budget in seconds (1..=3600). Short validation returns immediately; longer validation keeps the same execution and returns job_id for observation. Defaults vary per tool; invalid values are rejected before start.";
const VALIDATION_TIMEOUT_MIN: u64 = 1;
const VALIDATION_TIMEOUT_MAX: u64 = 3600;
const VALIDATION_SYNC_WAIT_SECS_DESCRIPTION: &str =
    "Optional synchronous grace in seconds (1..=60). It only controls how long this tool call waits after validation starts; if it is still running and total timeout remains, the same execution is returned as a Job. It never extends timeout_secs or starts a second validation.";
const VALIDATION_SYNC_WAIT_MIN: u64 = 1;
const VALIDATION_SYNC_WAIT_MAX: u64 =
    webcodex_core::runtime_contract::STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS;

fn with_validation_timeout_bounds(mut schema: Value, default: u64) -> Value {
    schema["properties"]["timeout_secs"]["minimum"] = json!(VALIDATION_TIMEOUT_MIN);
    schema["properties"]["timeout_secs"]["maximum"] = json!(VALIDATION_TIMEOUT_MAX);
    schema["properties"]["timeout_secs"]["default"] = json!(default);
    schema["properties"]["timeout_secs"]["description"] =
        json!(VALIDATION_TIMEOUT_SECS_DESCRIPTION);
    schema
}

fn with_validation_sync_wait(mut schema: Value) -> Value {
    schema["properties"]["sync_wait_secs"] = json!({
        "type": "integer",
        "minimum": VALIDATION_SYNC_WAIT_MIN,
        "maximum": VALIDATION_SYNC_WAIT_MAX,
        "description": VALIDATION_SYNC_WAIT_SECS_DESCRIPTION,
    });
    schema
}

fn with_optional_result_expectation(mut schema: Value) -> Value {
    schema["properties"][TOOL_RESULT_EXPECTATION_FIELD] = json!({
        "type": "string",
        "enum": ["success", "failure", "observe"],
        "description": "Optional pre-execution validation-result expectation. Omit (or use success) for the normal success-required path; failure is for negative tests that must reach a completed known failing validation result; observe accepts either completed known validation result. The real validation ToolResult and process outcome remain unchanged, and pre-execution rejection, timeout, cancellation, transport failure, malformed output, or outcome-unknown never satisfy the expectation."
    });
    schema
}

pub fn cargo_fmt_input_schema() -> Value {
    // `cargo_fmt(check=false)` mutates source and keeps the existing explicit
    // synchronous semantics: it never auto-promotes to a Job, so its
    // `timeout_secs` stays a synchronous command timeout. Only `check=true`
    // accepts the long read-only budget.
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "cwd",
            "string",
            "Optional project-relative working directory.",
            false,
        ),
        (
            "check",
            "boolean",
            "Run cargo fmt -- --check instead of formatting.",
            false,
        ),
        (
            "timeout_secs",
            "integer",
            "For mutating format, synchronous timeout in seconds (1..=120, default 120). With check=true, total validation budget is 1..=3600 and a long check keeps the same execution and returns job_id.",
            false,
        ),
        (
            "sync_wait_secs",
            "integer",
            VALIDATION_SYNC_WAIT_SECS_DESCRIPTION,
            false,
        ),
    ]));
    schema["properties"]["timeout_secs"]["minimum"] = json!(1);
    schema["properties"]["timeout_secs"]["maximum"] = json!(3600);
    schema["properties"]["timeout_secs"]["default"] = json!(120);
    schema["properties"]["sync_wait_secs"]["minimum"] = json!(VALIDATION_SYNC_WAIT_MIN);
    schema["properties"]["sync_wait_secs"]["maximum"] = json!(VALIDATION_SYNC_WAIT_MAX);
    schema["allOf"] = json!([{
        "if": {
            "required": ["check"],
            "properties": { "check": { "const": true } }
        },
        "then": {
            "properties": {
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600 }
            }
        },
        "else": {
            "properties": {
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 120 },
                "sync_wait_secs": { "type": "null" }
            }
        }
    }, {
        "if": {
            "required": ["check"],
            "properties": { "check": { "const": true } }
        },
        "else": {
            "properties": {
                "result_expectation": { "type": "null" }
            }
        }
    }]);
    let mut schema = with_optional_result_expectation(schema);
    schema["properties"][TOOL_RESULT_EXPECTATION_FIELD]["description"] = json!(
        "Optional pre-execution validation-result expectation, available only with check=true. Mutating cargo fmt never accepts result_expectation. The real ToolResult and process outcome remain unchanged."
    );
    schema
}

pub fn cargo_check_input_schema() -> Value {
    with_optional_result_expectation(with_validation_sync_wait(with_validation_timeout_bounds(
        object_schema(with_optional_session_id(vec![
            ("project", "string", "Runner-registered project id.", true),
            (
                "cwd",
                "string",
                "Optional project-relative working directory.",
                false,
            ),
            (
                "all_targets",
                "boolean",
                "Include --all-targets (default true).",
                false,
            ),
            ("all_features", "boolean", "Include --all-features.", false),
            (
                "no_default_features",
                "boolean",
                "Include --no-default-features.",
                false,
            ),
            (
                "features",
                "string",
                "Feature list passed to --features.",
                false,
            ),
            ("package", "string", "Package passed to -p.", false),
            (
                "timeout_secs",
                "integer",
                VALIDATION_TIMEOUT_SECS_DESCRIPTION,
                false,
            ),
        ])),
        600,
    )))
}

pub fn cargo_test_input_schema() -> Value {
    let mut schema = with_validation_sync_wait(with_validation_timeout_bounds(
        object_schema(with_optional_session_id(vec![
            ("project", "string", "Runner-registered project id.", true),
            (
                "cwd",
                "string",
                "Optional project-relative working directory.",
                false,
            ),
            ("filter", "string", "Optional cargo test filter.", false),
            ("all_targets", "boolean", "Include --all-targets.", false),
            ("all_features", "boolean", "Include --all-features.", false),
            (
                "no_default_features",
                "boolean",
                "Include --no-default-features.",
                false,
            ),
            (
                "features",
                "string",
                "Feature list passed to --features.",
                false,
            ),
            ("package", "string", "Package passed to -p.", false),
            (
                "no_run",
                "boolean",
                "When true, compile tests with --no-run. A successful compile-only validation does not require executed-test-count proof.",
                false,
            ),
            (
                "require_tests",
                "boolean",
                "Controls executed-test proof explicitly. Omission keeps the normal requirement for non-zero executed-test evidence; false is an explicit opt-out that allows zero tests to count as proof when no min_tests minimum is requested; true requires proof that at least one test executed.",
                false,
            ),
            (
                "min_tests",
                "integer",
                "Require proof that at least this many tests executed. Combined with require_tests using the stricter minimum.",
                false,
            ),
            (
                "timeout_secs",
                "integer",
                VALIDATION_TIMEOUT_SECS_DESCRIPTION,
                false,
            ),
        ])),
        1800,
    ));
    schema["properties"]["min_tests"]["minimum"] = json!(1);
    schema["properties"]["min_tests"]["maximum"] =
        json!(webcodex_core::runner_protocol::CARGO_TEST_MIN_TESTS_MAX);
    schema["allOf"] = json!([{
        "if": {
            "properties": { "no_run": { "const": true } },
            "required": ["no_run"]
        },
        "then": {
            "properties": {
                "require_tests": { "enum": [false] },
                "min_tests": { "enum": [] }
            }
        }
    }]);
    with_optional_result_expectation(schema)
}

pub fn go_test_input_schema() -> Value {
    let mut schema = with_validation_sync_wait(with_validation_timeout_bounds(
        object_schema(with_optional_session_id(vec![
            ("project", "string", "Runner-registered project id.", true),
            (
                "cwd",
                "string",
                "Optional project-relative working directory.",
                false,
            ),
            (
                "packages",
                "array",
                "Optional 1..8 project-relative Go package patterns: '.', './path', './...', or './path/...'.",
                false,
            ),
            (
                "timeout_secs",
                "integer",
                VALIDATION_TIMEOUT_SECS_DESCRIPTION,
                false,
            ),
        ])),
        1800,
    ));
    schema["properties"]["packages"]["minItems"] = json!(1);
    schema["properties"]["packages"]["maxItems"] =
        json!(webcodex_core::runner_protocol::GO_TEST_PACKAGE_MAX_ITEMS);
    schema["properties"]["packages"]["items"]["minLength"] = json!(1);
    schema["properties"]["packages"]["items"]["maxLength"] =
        json!(webcodex_core::runner_protocol::GO_TEST_PACKAGE_MAX_BYTES);
    with_optional_result_expectation(schema)
}
