use serde_json::{json, Value};

use super::common::{object_schema, with_optional_session_id, UNIFIED_DIFF_FIELD_DESCRIPTION};

pub fn apply_patch_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        (
            "patch",
            "string",
            "Codex apply_patch DSL using *** Begin Patch with Add File, Update File, Delete File, optional Move to, @@ context, and optional *** End of File markers.",
            true,
        ),
        (
            "dry_run",
            "boolean",
            "If true, fully parse and preflight the patch without writing any file.",
            false,
        ),
        (
            "matching_mode",
            "string",
            "Positioning policy. unique (default) tries Exact, TrimEnd, Trim, then Normalized and requires exactly one final mutation target at the selected tier; a repeated @@ anchor is allowed when old_lines still resolves to one target, while anchored pure additions require a unique anchor. exact_unique additionally requires Exact and unique at every textual positioning decision and is intended for an explicit stale-context/concurrency fence after reading exact current source. first_match is only for explicitly requested permissive compatibility and deterministically selects the first eligible candidate in the highest-priority tier.",
            false,
        ),
    ]));
    schema["properties"]["patch"]["minLength"] = json!(1);
    schema["properties"]["patch"]["maxLength"] =
        json!(webcodex_core::apply_patch_shared::MAX_CODEX_PATCH_BYTES);
    schema["properties"]["dry_run"]["default"] = json!(false);
    schema["properties"]["matching_mode"]["enum"] =
        json!(["first_match", "unique", "exact_unique"]);
    schema["properties"]["matching_mode"]["default"] = json!("unique");
    schema
}

pub fn apply_unified_diff_input_schema() -> Value {
    let mut schema = object_schema(with_optional_session_id(vec![
        ("project", "string", "Runner-registered project id.", true),
        ("diff", "string", UNIFIED_DIFF_FIELD_DESCRIPTION, true),
        (
            "deny_sensitive_paths",
            "boolean",
            "Optional fail-safe sensitive-path policy. Defaults to true; when true, any sensitive-path warning blocks mutation before git apply --check is dispatched.",
            false,
        ),
    ]));
    schema["properties"]["diff"]["maxLength"] =
        json!(webcodex_core::runtime_contract::MAX_UNIFIED_DIFF_BYTES);
    schema["properties"]["deny_sensitive_paths"]["default"] = json!(true);
    schema
}
