use serde_json::{json, Value};

use webcodex_core::runtime_contract::{
    MAX_SKILL_LIST_LIMIT, MAX_SKILL_QUERY_CHARS, MAX_SKILL_READ_LINES,
    MAX_SKILL_RESOURCE_PATH_CHARS,
};
use webcodex_core::skill_store::{
    MAX_OPERATOR_SKILL_KEY_CHARS, MAX_SKILL_STORE_IDEMPOTENCY_KEY_CHARS,
    MAX_SKILL_STORE_VERSIONS_LIMIT,
};

pub fn skill_list_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {"type": "string", "minLength": 1, "description": "Required authorized runtime Project id."},
            "query": {"type": "string", "maxLength": MAX_SKILL_QUERY_CHARS, "description": "Optional bounded case-insensitive substring filter over Skill name and description only."},
            "offset": {"type": "integer", "minimum": 0},
            "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SKILL_LIST_LIMIT},
            "expected_catalog_revision": {"type": "string", "pattern": "^wc_skillcat_[0-9a-f]{64}$", "description": "Optional catalog revision guard. If current discovery differs, the call fails with skill_catalog_changed rather than continuing an old offset."},
            "session_id": {"type": "string", "description": "Optional explicit Workflow Session for this tool call. No implicit current-Session fallback is used."}
        },
        "required": ["project"],
        "additionalProperties": false
    })
}

pub fn skill_versions_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {"type": "string", "minLength": 1},
            "skill_key": {"type": "string", "minLength": 1, "maxLength": MAX_OPERATOR_SKILL_KEY_CHARS, "pattern": "^[A-Za-z0-9._-]+$"},
            "offset": {"type": "integer", "minimum": 0},
            "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SKILL_STORE_VERSIONS_LIMIT},
            "session_id": {"type": "string"}
        },
        "required": ["project", "skill_key"],
        "additionalProperties": false
    })
}

pub fn skill_install_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {"type": "string", "minLength": 1, "description": "Authorized source Project; target operator store is the exact Runner owning this Project."},
            "skill_key": {"type": "string", "minLength": 1, "maxLength": MAX_OPERATOR_SKILL_KEY_CHARS, "pattern": "^[A-Za-z0-9._-]+$", "description": "Stable logical operator Skill key. It is not a filesystem path."},
            "artifact_path": {"type": "string", "minLength": 1, "maxLength": 1024, "description": "Project-relative existing ZIP artifact path. Native paths and URLs are not accepted."},
            "expected_artifact_sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
            "idempotency_key": {"type": "string", "minLength": 1, "maxLength": MAX_SKILL_STORE_IDEMPOTENCY_KEY_CHARS},
            "activate": {"type": "boolean", "default": false},
            "expected_state_revision": {"type": "string", "pattern": "^wc_skillstate_[0-9a-f]{64}$", "description": "CAS guard required when activating into an existing logical Skill state."},
            "session_id": {"type": "string"}
        },
        "required": ["project", "skill_key", "artifact_path", "expected_artifact_sha256", "idempotency_key"],
        "additionalProperties": false
    })
}

fn skill_state_mutation_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {"type": "string", "minLength": 1},
            "skill_key": {"type": "string", "minLength": 1, "maxLength": MAX_OPERATOR_SKILL_KEY_CHARS, "pattern": "^[A-Za-z0-9._-]+$"},
            "package_revision": {"type": "string", "pattern": "^wc_skillpkg_[0-9a-f]{64}$"},
            "expected_state_revision": {"type": "string", "pattern": "^wc_skillstate_[0-9a-f]{64}$"},
            "idempotency_key": {"type": "string", "minLength": 1, "maxLength": MAX_SKILL_STORE_IDEMPOTENCY_KEY_CHARS},
            "session_id": {"type": "string"}
        },
        "required": ["project", "skill_key", "package_revision", "expected_state_revision", "idempotency_key"],
        "additionalProperties": false
    })
}

pub fn skill_activate_input_schema() -> Value {
    skill_state_mutation_schema()
}

pub fn skill_remove_revision_input_schema() -> Value {
    skill_state_mutation_schema()
}

pub fn skill_read_file_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {"type": "string", "minLength": 1, "description": "Required authorized runtime Project id."},
            "skill_id": {"type": "string", "pattern": "^wc_skill_[0-9a-f]{32}$", "description": "Opaque Skill identity returned by skills.catalog or skill_list; it selects one exact source/package without exposing native Runner paths."},
            "path": {"type": "string", "minLength": 1, "maxLength": MAX_SKILL_RESOURCE_PATH_CHARS, "description": "Skill-package-relative UTF-8 text resource path. Defaults to SKILL.md; absolute paths and traversal are forbidden."},
            "start_line": {"type": "integer", "minimum": 1},
            "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SKILL_READ_LINES},
            "expected_definition_revision": {"type": "string", "pattern": "^[0-9a-f]{64}$", "description": "Optional SKILL.md content digest guard. A mismatch fails with skill_definition_changed and does not return resource text."},
            "expected_package_revision": {"type": "string", "pattern": "^wc_skillpkg_[0-9a-f]{64}$", "description": "Operator-installed Skills only. Pins the current active immutable package revision; stale values fail with skill_package_changed before resource text is returned."},
            "session_id": {"type": "string", "description": "Optional explicit Workflow Session for this tool call. No implicit current-Session fallback is used."}
        },
        "required": ["project", "skill_id"],
        "additionalProperties": false
    })
}
