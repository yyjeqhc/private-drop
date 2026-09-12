use serde_json::{json, Value};

use super::common::OPTIONAL_EXPLICIT_SESSION_ID_DESCRIPTION;

fn line_scope_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Optional 1-based inclusive source-line safety fence against original batch content. The complete exact match or anchor must be contained. occurrence remains the global source-order exact occurrence and is never renumbered within the scope.",
        "properties": {
            "start_line": {"type": "integer", "minimum": 1},
            "end_line": {"type": "integer", "minimum": 1}
        },
        "required": ["start_line", "end_line"]
    })
}

fn apply_text_edit_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["replace_exact"]},
                    "old_text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Exact text to replace; must be non-empty and is unique by default unless occurrence is supplied."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text. May be empty; omitting it preserves the existing wire behavior of replacing with an empty string."
                    },
                    "occurrence": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional 1-based global source-order exact occurrence selector. line_scope never renumbers occurrence; expected_sha256 remains authoritative."
                    },
                    "line_scope": line_scope_schema()
                },
                "required": ["kind", "old_text"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["delete_exact"]},
                    "old_text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Exact text to delete; must be non-empty and is unique by default unless occurrence is supplied."
                    },
                    "occurrence": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional 1-based global source-order exact occurrence selector. line_scope never renumbers occurrence; expected_sha256 remains authoritative."
                    },
                    "line_scope": line_scope_schema()
                },
                "required": ["kind", "old_text"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["insert_before"]},
                    "anchor_text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Exact anchor before which new_text is inserted; unique by default unless occurrence is supplied."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Text inserted before anchor_text. Empty text is accepted as a provable no-op and ignored without invalidating other edits in the transaction."
                    },
                    "occurrence": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional 1-based global source-order exact occurrence selector. line_scope never renumbers occurrence; expected_sha256 remains authoritative."
                    },
                    "line_scope": line_scope_schema()
                },
                "required": ["kind", "anchor_text", "new_text"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["insert_after"]},
                    "anchor_text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Exact anchor after which new_text is inserted; unique by default unless occurrence is supplied."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Text inserted after anchor_text. Empty text is accepted as a provable no-op and ignored without invalidating other edits in the transaction."
                    },
                    "occurrence": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional 1-based global source-order exact occurrence selector. line_scope never renumbers occurrence; expected_sha256 remains authoritative."
                    },
                    "line_scope": line_scope_schema()
                },
                "required": ["kind", "anchor_text", "new_text"]
            }
        ]
    })
}

fn existing_file_sha256_schema() -> Value {
    json!({
        "type": "string",
        "pattern": "^[a-f0-9]{64}$",
        "description": "Required current-file sha256 for this existing-file change."
    })
}

fn project_path_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "description": description
    })
}

fn apply_file_change_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["edit"]},
                    "path": project_path_schema("Project-relative existing file to edit."),
                    "expected_sha256": existing_file_sha256_schema(),
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 20,
                        "description": "One to 20 exact edits applied transactionally to this file.",
                        "items": apply_text_edit_schema()
                    }
                },
                "required": ["kind", "path", "expected_sha256", "edits"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["create"]},
                    "path": project_path_schema("Project-relative new file path."),
                    "content": {
                        "type": "string",
                        "description": "Complete UTF-8 content for the new file; empty content is valid."
                    }
                },
                "required": ["kind", "path", "content"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["delete"]},
                    "path": project_path_schema("Project-relative existing file to delete."),
                    "expected_sha256": existing_file_sha256_schema()
                },
                "required": ["kind", "path", "expected_sha256"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["rename"]},
                    "path": project_path_schema("Project-relative existing source file."),
                    "to_path": project_path_schema("Project-relative destination path; must differ from path."),
                    "expected_sha256": existing_file_sha256_schema()
                },
                "required": ["kind", "path", "to_path", "expected_sha256"]
            }
        ]
    })
}

pub fn apply_text_edits_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {
                "type": "string",
                "description": "Runner-registered project id."
            },
            "changes": {
                "type": "array",
                "minItems": 1,
                "maxItems": 16,
                "description": "Transactional list of 1..16 file changes. Each change uses the fields declared by its kind; the whole batch is preflighted before mutation.",
                "items": apply_file_change_schema()
            },
            "dry_run": {
                "type": "boolean",
                "description": "If true, compute the plan without writing."
            },
            "session_id": {
                "type": "string",
                "description": OPTIONAL_EXPLICIT_SESSION_ID_DESCRIPTION
            }
        },
        "required": ["project", "changes"],
        "additionalProperties": false
    })
}
