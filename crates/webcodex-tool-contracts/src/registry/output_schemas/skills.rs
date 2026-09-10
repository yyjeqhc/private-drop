use serde_json::{json, Value};

use super::common::{array_schema, nullable_schema, schema_type, wrapped_output_schema};
use webcodex_core::skill_metadata::{MAX_SKILL_DESCRIPTION_CHARS, MAX_SKILL_NAME_CHARS};

fn descriptor_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "skill_id": {"type": "string", "pattern": "^wc_skill_[0-9a-f]{32}$"},
            "name": {"type": "string", "maxLength": MAX_SKILL_NAME_CHARS},
            "description": {"type": "string", "maxLength": MAX_SKILL_DESCRIPTION_CHARS},
            "definition_revision": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
            "source_scope": {"type": "string", "enum": ["project", "runner"]},
            "trust": {"type": "string", "enum": ["project_content", "operator_configured_guidance", "operator_installed_guidance"]},
            "package_revision": {"anyOf": [{"type":"string","pattern":"^wc_skillpkg_[0-9a-f]{64}$"},{"type":"null"}]},
            "name_conflict": {"type": "boolean"}
        },
        "required": ["skill_id", "name", "description", "definition_revision", "source_scope", "trust", "package_revision", "name_conflict"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_trust_distinguishes_configured_and_managed_runner_guidance() {
        let schema = descriptor_schema();
        let values = schema["properties"]["trust"]["enum"]
            .as_array()
            .expect("trust enum");
        assert!(values.iter().any(|value| value == "project_content"));
        assert!(values
            .iter()
            .any(|value| value == "operator_configured_guidance"));
        assert!(values
            .iter()
            .any(|value| value == "operator_installed_guidance"));
    }
}

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    match name {
        "skill_list" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved Project id.")),
            (
                "catalog_revision",
                schema_type(
                    "string",
                    "Digest of the freshly observed deterministic Skill catalog state.",
                ),
            ),
            (
                "total_count",
                schema_type("integer", "Valid Skills matching the optional query."),
            ),
            (
                "returned_count",
                schema_type("integer", "Descriptors returned in this page."),
            ),
            (
                "offset",
                schema_type(
                    "integer",
                    "Page offset within the filtered deterministic catalog.",
                ),
            ),
            (
                "next_offset",
                nullable_schema(
                    "integer",
                    "Next offset, or null when this page is complete.",
                ),
            ),
            (
                "truncated",
                schema_type("boolean", "Whether more filtered descriptors remain."),
            ),
            (
                "skills",
                array_schema(
                    descriptor_schema(),
                    "Lightweight Skill descriptors; never SKILL.md bodies.",
                ),
            ),
            (
                "invalid_count",
                schema_type(
                    "integer",
                    "Malformed/invalid packages isolated from the valid catalog.",
                ),
            ),
            (
                "diagnostics",
                array_schema(
                    json!({"type":"object","additionalProperties":true}),
                    "Bounded reason-code-only invalid package diagnostics.",
                ),
            ),
            (
                "discovery_truncated",
                schema_type(
                    "boolean",
                    "Whether the hard discovery package ceiling was reached.",
                ),
            ),
            (
                "error_kind",
                schema_type("string", "Stable guard/error code on failure."),
            ),
            (
                "state_changed",
                schema_type("boolean", "Always false for Skill runtime failures."),
            ),
        ])),
        "skill_read_file" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved Project id.")),
            (
                "skill_id",
                schema_type("string", "Opaque project-scoped Skill identity."),
            ),
            ("name", schema_type("string", "Skill metadata name.")),
            ("source_scope", schema_type("string", "Selected Skill source: project or runner.")),
            ("trust", schema_type("string", "Guidance trust label for the selected source; never execution authority.")),
            (
                "package_revision",
                nullable_schema(
                    "string",
                    "Active immutable whole-package revision for runner-installed Skills; null for project Skills.",
                ),
            ),
            (
                "definition_revision",
                schema_type(
                    "string",
                    "Current SKILL.md content digest only; not a package-tree revision.",
                ),
            ),
            (
                "path",
                schema_type("string", "Package-relative resource path."),
            ),
            (
                "sha256",
                schema_type("string", "Full current resource-file SHA-256."),
            ),
            (
                "text",
                schema_type("string", "Bounded UTF-8 selected text range."),
            ),
            (
                "start_line",
                schema_type("integer", "Effective 1-based selected start line."),
            ),
            (
                "end_line",
                nullable_schema("integer", "Last returned line, or null when none."),
            ),
            (
                "returned_lines",
                schema_type("integer", "Returned source lines."),
            ),
            (
                "has_more",
                schema_type("boolean", "Whether resource lines remain."),
            ),
            (
                "next_start_line",
                nullable_schema("integer", "Continuation line when has_more."),
            ),
            (
                "error_kind",
                schema_type("string", "Stable guard/error code on failure."),
            ),
            (
                "state_changed",
                schema_type("boolean", "Always false for Skill runtime failures."),
            ),
        ])),
        "skill_versions" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved Project id.")),
            ("skill_id", schema_type("string", "Runner-scoped opaque Skill identity.")),
            ("skill_key", schema_type("string", "Stable logical operator Skill key.")),
            ("state_revision", schema_type("string", "CAS revision for installed/active state.")),
            ("active_package_revision", nullable_schema("string", "Currently active immutable package revision.")),
            ("total_count", schema_type("integer", "Installed immutable revision count.")),
            ("offset", schema_type("integer", "Returned page offset.")),
            ("next_offset", nullable_schema("integer", "Continuation offset.")),
            ("versions", array_schema(json!({
                "type":"object",
                "properties": {
                    "package_revision":{"type":"string"},
                    "definition_revision":{"type":"string"},
                    "name":{"type":"string"},
                    "description":{"type":"string"},
                    "file_count":{"type":"integer"},
                    "total_bytes":{"type":"integer"},
                    "installed_at_unix_ms":{"type":"integer"}
                },
                "additionalProperties": false
            }), "Bounded immutable revision metadata; never package bodies or native paths.")),
            ("error_kind", schema_type("string", "Stable error code on failure.")),
            ("state_changed", schema_type("boolean", "Always false for version observation.")),
        ])),

        "skill_install" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Authorized source Project id.")),
            ("skill_id", schema_type("string", "Runner-scoped opaque Skill identity.")),
            ("skill_key", schema_type("string", "Logical operator Skill key.")),
            ("package_revision", schema_type("string", "Immutable whole-package content revision.")),
            ("definition_revision", schema_type("string", "SKILL.md SHA-256 only.")),
            ("artifact_sha256", schema_type("string", "Verified source ZIP SHA-256.")),
            ("file_count", schema_type("integer", "Validated package file count.")),
            ("total_bytes", schema_type("integer", "Validated decompressed package bytes.")),
            ("installed", schema_type("boolean", "Whether this call committed a new immutable revision.")),
            ("activated", schema_type("boolean", "Whether this call changed active state.")),
            ("replayed", schema_type("boolean", "Whether the result reconciled the same idempotent intent.")),
            ("state_revision", schema_type("string", "Current CAS state revision.")),
            ("active_package_revision", nullable_schema("string", "Current active package revision.")),
            ("outcome_unknown", schema_type("boolean", "True only when dispatch may have executed but result cannot be reconciled yet.")),
            ("recovery_kind", schema_type("string", "reconcile when uncertain state requires observation.")),
            ("recovery_tool", schema_type("string", "skill_versions when reconciliation is required.")),
            ("reconcile_with", schema_type("string", "Stable reconciliation tool name.")),
            ("retry_same_idempotency_key", schema_type("boolean", "When true, any retry must reuse the same logical idempotency key; never invent a new key.")),
            ("error_kind", schema_type("string", "Stable error code on failure.")),
            ("state_changed", schema_type("boolean", "Observed mutation flag when outcome is known.")),
        ])),
        "skill_activate" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved Project id.")),
            ("skill_id", schema_type("string", "Runner-scoped opaque Skill identity.")),
            ("skill_key", schema_type("string", "Logical operator Skill key.")),
            ("previous_active_package_revision", nullable_schema("string", "Previous active package revision.")),
            ("active_package_revision", schema_type("string", "Current active package revision.")),
            ("state_revision", schema_type("string", "Current CAS state revision.")),
            ("changed", schema_type("boolean", "Whether active state changed.")),
            ("replayed", schema_type("boolean", "Whether same-key reconciliation supplied this result.")),
            ("outcome_unknown", schema_type("boolean", "Whether dispatch outcome must be reconciled with the same key.")),
            ("recovery_kind", schema_type("string", "reconcile when uncertain state requires observation.")),
            ("recovery_tool", schema_type("string", "skill_versions when reconciliation is required.")),
            ("reconcile_with", schema_type("string", "Stable reconciliation tool name.")),
            ("retry_same_idempotency_key", schema_type("boolean", "When true, any retry must reuse the same logical idempotency key.")),
            ("error_kind", schema_type("string", "Stable error code on failure.")),
            ("state_changed", schema_type("boolean", "Observed mutation flag when outcome is known.")),
        ])),
        "skill_remove_revision" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved Project id.")),
            ("skill_id", schema_type("string", "Runner-scoped opaque Skill identity.")),
            ("skill_key", schema_type("string", "Logical operator Skill key.")),
            ("package_revision", schema_type("string", "Target immutable package revision.")),
            ("state_revision", schema_type("string", "Current CAS state revision.")),
            ("removed", schema_type("boolean", "Whether the inactive revision was removed.")),
            ("replayed", schema_type("boolean", "Whether same-key reconciliation supplied this result.")),
            ("outcome_unknown", schema_type("boolean", "Whether dispatch outcome must be reconciled with the same key.")),
            ("recovery_kind", schema_type("string", "reconcile when uncertain state requires observation.")),
            ("recovery_tool", schema_type("string", "skill_versions when reconciliation is required.")),
            ("reconcile_with", schema_type("string", "Stable reconciliation tool name.")),
            ("retry_same_idempotency_key", schema_type("boolean", "When true, any retry must reuse the same logical idempotency key.")),
            ("error_kind", schema_type("string", "Stable error code on failure.")),
            ("state_changed", schema_type("boolean", "Observed mutation flag when outcome is known.")),
        ])),
        _ => None,
    }
}
