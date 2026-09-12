use serde_json::{json, Value};

use super::common::{object_schema, with_optional_session_id};

pub fn lsp_status_input_schema() -> Value {
    object_schema(with_optional_session_id(vec![(
        "project",
        "string",
        "Full Runner runtime project id (legacy wire form agent:<client_id>:<project_id>).",
        true,
    )]))
}

pub fn document_symbols_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Full Runner runtime project id (legacy wire form agent:<client_id>:<project_id>).",
            true,
        ),
        (
            "path",
            "string",
            "Project-relative UTF-8 path to a supported source file.",
            true,
        ),
        (
            "limit",
            "integer",
            "Maximum symbol nodes to return (default 100, clamped to 1..500).",
            false,
        ),
    ]));
    schema["properties"]["limit"]["minimum"] = json!(1);
    schema["properties"]["limit"]["default"] = json!(100);
    schema
}

pub fn document_diagnostics_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Full Runner runtime project id (legacy wire form agent:<client_id>:<project_id>).",
            true,
        ),
        (
            "path",
            "string",
            "Project-relative UTF-8 path to a supported source file.",
            true,
        ),
        (
            "limit",
            "integer",
            "Maximum normalized diagnostics to return (default 100, clamped to 1..200).",
            false,
        ),
    ]));
    schema["properties"]["limit"]["minimum"] = json!(1);
    schema["properties"]["limit"]["default"] = json!(100);
    schema
}

pub fn hover_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Full Runner runtime project id (legacy wire form agent:<client_id>:<project_id>).",
            true,
        ),
        (
            "path",
            "string",
            "Project-relative UTF-8 path to a supported source file.",
            true,
        ),
        ("line", "integer", "1-based line number.", true),
        (
            "column",
            "integer",
            "1-based Unicode scalar column (end-of-line caret allowed at length+1).",
            true,
        ),
    ]));
    schema["properties"]["line"]["minimum"] = json!(1);
    schema["properties"]["column"]["minimum"] = json!(1);
    schema
}

pub fn workspace_symbols_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Full Runner runtime project id (legacy wire form agent:<client_id>:<project_id>).",
            true,
        ),
        (
            "query",
            "string",
            "Non-empty workspace symbol query after trimming (1..200 characters).",
            true,
        ),
        (
            "limit",
            "integer",
            "Maximum workspace symbols to return (default 50, clamped to 1..200).",
            false,
        ),
    ]));
    schema["properties"]["query"]["minLength"] = json!(1);
    schema["properties"]["query"]["maxLength"] = json!(200);
    schema["properties"]["limit"]["minimum"] = json!(1);
    schema["properties"]["limit"]["default"] = json!(50);
    schema
}

pub fn goto_definition_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Full Runner runtime project id (legacy wire form agent:<client_id>:<project_id>).",
            true,
        ),
        (
            "path",
            "string",
            "Project-relative UTF-8 path to a supported source file.",
            true,
        ),
        ("line", "integer", "1-based line number.", true),
        (
            "column",
            "integer",
            "1-based Unicode scalar column (end-of-line caret allowed at length+1).",
            true,
        ),
        (
            "limit",
            "integer",
            "Maximum locations to return (default 20, clamped to 1..100).",
            false,
        ),
    ]));
    schema["properties"]["line"]["minimum"] = json!(1);
    schema["properties"]["column"]["minimum"] = json!(1);
    schema["properties"]["limit"]["minimum"] = json!(1);
    schema["properties"]["limit"]["default"] = json!(20);
    schema
}

pub fn find_references_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Full Runner runtime project id (legacy wire form agent:<client_id>:<project_id>).",
            true,
        ),
        (
            "path",
            "string",
            "Project-relative UTF-8 path to a supported source file.",
            true,
        ),
        ("line", "integer", "1-based line number.", true),
        (
            "column",
            "integer",
            "1-based Unicode scalar column (end-of-line caret allowed at length+1).",
            true,
        ),
        (
            "include_declaration",
            "boolean",
            "Include the declaration in results (default true).",
            false,
        ),
        (
            "limit",
            "integer",
            "Maximum locations to return (default 50, clamped to 1..200).",
            false,
        ),
    ]));
    schema["properties"]["line"]["minimum"] = json!(1);
    schema["properties"]["column"]["minimum"] = json!(1);
    schema["properties"]["include_declaration"]["default"] = json!(true);
    schema["properties"]["limit"]["minimum"] = json!(1);
    schema["properties"]["limit"]["default"] = json!(50);
    schema
}

pub fn call_hierarchy_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        (
            "project",
            "string",
            "Full Runner runtime project id (legacy wire form agent:<client_id>:<project_id>).",
            true,
        ),
        (
            "path",
            "string",
            "Project-relative UTF-8 path to a supported source file.",
            true,
        ),
        ("line", "integer", "1-based line number.", true),
        (
            "column",
            "integer",
            "1-based Unicode scalar column (end-of-line caret allowed at length+1).",
            true,
        ),
        (
            "direction",
            "string",
            "Call direction: incoming, outgoing, or both (default both).",
            false,
        ),
        (
            "depth",
            "integer",
            "Breadth-first traversal depth (default 1, maximum 2).",
            false,
        ),
        (
            "limit",
            "integer",
            "Global flattened edge limit (default 50, maximum 100).",
            false,
        ),
    ]));
    schema["properties"]["line"]["minimum"] = json!(1);
    schema["properties"]["column"]["minimum"] = json!(1);
    schema["properties"]["direction"]["enum"] = json!(["incoming", "outgoing", "both"]);
    schema["properties"]["direction"]["default"] = json!("both");
    schema["properties"]["depth"]["minimum"] = json!(1);
    schema["properties"]["depth"]["maximum"] = json!(2);
    schema["properties"]["depth"]["default"] = json!(1);
    schema["properties"]["limit"]["minimum"] = json!(1);
    schema["properties"]["limit"]["maximum"] = json!(100);
    schema["properties"]["limit"]["default"] = json!(50);
    schema
}
