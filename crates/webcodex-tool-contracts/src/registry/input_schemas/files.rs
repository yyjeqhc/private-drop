use serde_json::{json, Value};

use super::common::{object_schema, with_optional_session_id};

pub fn list_project_files_input_schema() -> Value {
    object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "path",
            "string",
            "Optional project-relative directory to list (default: project root).",
            false,
        ),
        (
            "limit",
            "integer",
            "Maximum number of entries to return.",
            false,
        ),
    ]))
}

pub fn list_project_tracked_files_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "path",
            "string",
            "Optional project-relative directory scope (default: project root). Rollup depth is counted inside this scope.",
            false,
        ),
        (
            "globs",
            "array",
            "Optional path globs; an entry matches if it matches any of them. Supports * (not crossing /), ** (crossing /), and ?. A pattern without / also matches the basename, so *.py works at any depth.",
            false,
        ),
        (
            "depth",
            "integer",
            "Optional directory rollup depth, clamped to 1..16. Omit to list every file when the result fits limit, and otherwise roll up automatically to the deepest depth that does fit.",
            false,
        ),
        (
            "limit",
            "integer",
            "Maximum entries to return; clamped to 1..1000 (default 200).",
            false,
        ),
        (
            "offset",
            "integer",
            "Entry offset for paging; use the next_offset value from the previous page.",
            false,
        ),
    ]));
    schema["properties"]["globs"] = json!({
        "type": "array",
        "maxItems": 20,
        "items": { "type": "string", "minLength": 1, "maxLength": 256 },
        "description": schema["properties"]["globs"]["description"].clone(),
    });
    schema["properties"]["limit"]["default"] = json!(200);
    schema
}

pub fn project_overview_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Full Runner runtime project id (legacy agent:<client_id>:<project_id> identity).", true),
        (
            "path",
            "string",
            "Optional project-relative directory scope (default: project root).",
            false,
        ),
        (
            "max_depth",
            "integer",
            "Bounded scan depth; defaults to 2 and is clamped by the runtime to 1..4.",
            false,
        ),
        (
            "limit",
            "integer",
            "Bounded scanned-entry limit; defaults to 200 and is clamped by the runtime to 20..500.",
            false,
        ),
    ]));
    schema["properties"]["max_depth"]["default"] = json!(2);
    schema["properties"]["limit"]["default"] = json!(200);
    schema
}

pub fn search_project_text_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "pattern",
            "string",
            "Search pattern. Interpreted as a regular expression by default. For identifiers, source snippets, paths, and other exact text, prefer pattern_mode=literal; use regex when regex syntax is intentional.",
            true,
        ),
        (
            "pattern_mode",
            "string",
            "Pattern interpretation: regex (default, backward compatible) or literal for exact text. Prefer literal unless regex syntax is intentional.",
            false,
        ),
        (
            "path",
            "string",
            "Optional project-relative directory to scope the search (default: project root).",
            false,
        ),
        (
            "limit",
            "integer",
            "Maximum records to return: matches in matches mode, files in files_with_matches/count modes.",
            false,
        ),
        (
            "context_before",
            "integer",
            "Optional number of context lines before each match (clamped to 20).",
            false,
        ),
        (
            "context_after",
            "integer",
            "Optional number of context lines after each match (clamped to 20).",
            false,
        ),
        (
            "include_globs",
            "array",
            "Optional ripgrep include globs. At most 32 entries of 1..256 bytes; negated and protected-path globs are rejected.",
            false,
        ),
        (
            "exclude_globs",
            "array",
            "Optional additive ripgrep exclude globs. Built-in secret/build excludes always remain active.",
            false,
        ),
        (
            "result_mode",
            "string",
            "Result shape: matches (default), files_with_matches, or count.",
            false,
        ),
        (
            "timeout_secs",
            "integer",
            "Optional search timeout in seconds. Server clamps the value to 1..120 (default 30). Out-of-range values are accepted and clamped rather than rejected by schema.",
            false,
        ),
    ]));
    for field in ["include_globs", "exclude_globs"] {
        let description = schema["properties"][field]["description"].clone();
        schema["properties"][field] = json!({
            "type": "array",
            "maxItems": 32,
            "items": {
                "type": "string",
                "minLength": 1,
                "maxLength": 256,
            },
            "description": description,
        });
    }
    schema["properties"]["result_mode"]["enum"] = json!(["matches", "files_with_matches", "count"]);
    schema["properties"]["result_mode"]["default"] = json!("matches");
    schema["properties"]["pattern_mode"]["enum"] = json!(["regex", "literal"]);
    schema["properties"]["pattern_mode"]["default"] = json!("regex");
    // Intentionally no minimum/maximum: strict clients would reject 0/999
    // before send, but runtime clamps any integer to 1..120.
    schema["properties"]["timeout_secs"]["default"] = json!(30);
    schema
}

pub fn search_project_texts_input_schema() -> Value {
    let single = search_project_text_input_schema();
    let mut query_properties = single["properties"]
        .as_object()
        .expect("search_project_text properties")
        .clone();
    for outer_field in ["project", "session_id"] {
        query_properties.remove(outer_field);
    }
    query_properties
        .get_mut("pattern")
        .expect("search pattern schema")["minLength"] = json!(1);
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "queries",
            "array",
            "One to eight independent bounded text-search queries, returned in request order.",
            true,
        ),
        (
            "max_result_bytes",
            "integer",
            "Optional primary model-facing batch projection budget in bytes. Defaults to 64 KiB and caps at 256 KiB. Continuation is whole-query via next_index; if the first remaining query cannot fit, raise this budget or narrow that query's limit/context/path. Independently bounded Session/continuity overlays remain outside this budget.",
            false,
        ),
    ]));
    schema["properties"]["queries"] = json!({
        "type": "array",
        "minItems": 1,
        "maxItems": 8,
        "description": "One to eight independent bounded text-search queries, returned in request order.",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["pattern"],
            "properties": query_properties,
        }
    });
    schema["properties"]["max_result_bytes"]["minimum"] =
        json!(webcodex_core::runtime_contract::MIN_SEARCH_PROJECT_TEXTS_RESULT_BYTES);
    schema["properties"]["max_result_bytes"]["maximum"] =
        json!(webcodex_core::runtime_contract::FILE_READ_MAX_SERIALIZED_OUTPUT_BYTES);
    schema["properties"]["max_result_bytes"]["default"] =
        json!(webcodex_core::runtime_contract::DEFAULT_SEARCH_PROJECT_TEXTS_RESULT_BYTES);
    schema
}

pub fn read_file_input_schema() -> Value {
    object_schema(with_optional_session_id(vec![
        ("project", "string", "Configured project id.", true),
        ("path", "string", "Project-relative file path.", true),
        ("start_line", "integer", "1-based line offset.", false),
        ("limit", "integer", "Maximum line count.", false),
        (
            "with_line_numbers",
            "boolean",
            "When true, return the single text field in numbered format instead of plain format.",
            false,
        ),
    ]))
}

pub fn read_files_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Configured project id.", true),
        (
            "items",
            "array",
            "One to eight project-relative UTF-8 file ranges, returned in request order.",
            true,
        ),
        (
            "with_line_numbers",
            "boolean",
            "When true, every successful item returns numbered text instead of plain text.",
            false,
        ),
        (
            "max_result_bytes",
            "integer",
            "Optional primary model-facing batch projection budget in bytes. Defaults to 64 KiB; raise only for explicit broad/deep reads, up to 256 KiB. If the current budget cannot return any part of the first remaining item, the result supplies a bounded increase_result_budget suggested call; otherwise batch continuation reuses the current budget. Independently bounded Session/continuity protocol overlays are preserved outside this budget.",
            false,
        ),
    ]));
    schema["properties"]["items"] = json!({
        "type": "array",
        "minItems": 1,
        "maxItems": 8,
        "description": "One to eight project-relative UTF-8 file ranges, returned in request order.",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Non-empty project-relative file path."
                },
                "start_line": {
                    "type": "integer",
                    "description": "Optional 1-based line offset; normalized exactly like read_file."
                },
                "limit": {
                    "type": "integer",
                    "description": "Optional maximum line count; normalized exactly like read_file."
                }
            }
        }
    });
    schema["properties"]["max_result_bytes"]["minimum"] =
        json!(webcodex_core::runtime_contract::MIN_READ_FILES_RESULT_BYTES);
    schema["properties"]["max_result_bytes"]["maximum"] =
        json!(webcodex_core::runtime_contract::FILE_READ_MAX_SERIALIZED_OUTPUT_BYTES);
    schema["properties"]["max_result_bytes"]["default"] =
        json!(webcodex_core::runtime_contract::DEFAULT_READ_FILES_RESULT_BYTES);
    schema
}
