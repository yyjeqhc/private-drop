//! Characterization of the startup-only catalog projection, not resource authority.

use crate::tool_runtime::startup_brief::{
    bounded_extension_description, StartupExtensions, StartupPluginEntry, StartupPluginsCatalog,
    StartupSkillEntry, StartupSkillsCatalog, STARTUP_EXTENSION_CATALOG_HARD_MAX_BYTES,
    STARTUP_PLUGIN_CATALOG_MAX_BYTES, STARTUP_SKILL_CATALOG_MAX_BYTES,
};
use serde_json::{json, Value};

const SKILL_HINT: &str = "Use skills.catalog or skill_list for broader or refreshed discovery.";
const PLUGIN_HINT: &str = "Use plugins.catalog or explicit plugin_tool list and describe for broader or current schema discovery.";

// Independent JSON oracle for the original greedy-prefix wire contract. Keep
// the omitted hint and upstream-incomplete cases explicit: they affect bytes.
fn reference_catalog(
    revision: &str,
    entries: &[Value],
    total_count: usize,
    upstream_truncated: bool,
    max_bytes: usize,
    hint: &str,
) -> Value {
    let project = |returned: &[Value]| {
        let truncated = upstream_truncated || returned.len() < total_count;
        let mut value = json!({
            "status": "available",
            "catalog_revision": revision,
            "total_count": total_count,
            "returned_count": returned.len(),
            "truncated": truncated,
            "entries": returned,
        });
        if truncated {
            value["discovery_hint"] = json!(hint);
        }
        value
    };
    let mut returned = Vec::new();
    for entry in entries {
        let mut candidate = returned.clone();
        candidate.push(entry.clone());
        if serde_json::to_vec(&project(&candidate)).unwrap().len() > max_bytes {
            break;
        }
        returned = candidate;
    }
    project(&returned)
}

fn skill(index: usize, description: &str) -> StartupSkillEntry {
    StartupSkillEntry {
        skill_id: format!("wc_skill_{index:032x}"),
        name: format!("skill-{index}"),
        description: description.to_string(),
        source_scope: if index % 2 == 0 { "project" } else { "runner" }.to_string(),
        trust: if index % 2 == 0 {
            "project_content"
        } else {
            "operator_installed_guidance"
        }
        .to_string(),
        name_conflict: index % 3 == 0,
    }
}

fn plugin(index: usize, description: &str) -> StartupPluginEntry {
    StartupPluginEntry {
        plugin: format!("provider-{index}"),
        name: format!("Provider {index}"),
        tool: format!("tool-{index}"),
        title: (index % 2 == 0).then(|| format!("Tool {index}")),
        description: (index % 3 != 0).then(|| description.to_string()),
        annotations: Default::default(),
    }
}

#[test]
fn startup_catalog_skill_projection_matches_original_prefix_contract() {
    let revision = format!("wc_skillcat_{}", "a".repeat(64));
    for count in [0, 1, 2, 8, 64] {
        for width in [0, 32, 180, 512, 3000] {
            let description = "\u{8d44}\u{6e90}\"\\\n".repeat(width);
            let entries: Vec<_> = (0..count).map(|i| skill(i, &description)).collect();
            let values: Vec<_> = entries.iter().map(|entry| json!(entry)).collect();
            for upstream_truncated in [false, true] {
                let actual = StartupSkillsCatalog::available(
                    revision.clone(),
                    upstream_truncated,
                    entries.clone(),
                );
                assert_eq!(
                    json!(actual),
                    reference_catalog(
                        &revision,
                        &values,
                        count,
                        upstream_truncated,
                        STARTUP_SKILL_CATALOG_MAX_BYTES,
                        SKILL_HINT,
                    ),
                    "count={count}, width={width}, upstream={upstream_truncated}",
                );
                assert!(
                    serde_json::to_vec(&actual).unwrap().len() <= STARTUP_SKILL_CATALOG_MAX_BYTES
                );
            }
        }
    }
}

#[test]
fn startup_catalog_plugin_projection_preserves_provider_total_and_optional_fields() {
    let revision = format!("wc_plugcat_{}", "b".repeat(64));
    for count in [0, 1, 2, 8, 64] {
        for width in [0, 32, 180, 512, 3000] {
            let description = "\u{63d2}\u{4ef6}\"\\\n".repeat(width);
            let entries: Vec<_> = (0..count).map(|i| plugin(i, &description)).collect();
            let values: Vec<_> = entries.iter().map(|entry| json!(entry)).collect();
            for total_count in [count, count + 7] {
                let actual = StartupPluginsCatalog::available(
                    revision.clone(),
                    total_count,
                    entries.clone(),
                );
                assert_eq!(
                    json!(actual),
                    reference_catalog(
                        &revision,
                        &values,
                        total_count,
                        false,
                        STARTUP_PLUGIN_CATALOG_MAX_BYTES,
                        PLUGIN_HINT,
                    ),
                    "count={count}, width={width}, total={total_count}",
                );
                assert!(
                    serde_json::to_vec(&actual).unwrap().len() <= STARTUP_PLUGIN_CATALOG_MAX_BYTES
                );
            }
        }
    }
}

#[test]
fn startup_catalog_unavailable_is_not_a_successfully_empty_catalog() {
    for (value, reason, hint) in [
        (
            json!(StartupSkillsCatalog::unavailable(
                "skills_catalog_unavailable"
            )),
            "skills_catalog_unavailable",
            "Use skills.catalog or skill_list for explicit discovery when available.",
        ),
        (
            json!(StartupPluginsCatalog::unavailable(
                "plugin_runtime_unavailable"
            )),
            "plugin_runtime_unavailable",
            "Use explicit plugin_tool list and describe when Plugin discovery is available.",
        ),
    ] {
        assert_eq!(
            value,
            json!({
                "status": "unavailable",
                "reason_code": reason,
                "total_count": 0,
                "returned_count": 0,
                "truncated": false,
                "entries": [],
                "discovery_hint": hint,
            })
        );
    }
}

#[test]
fn startup_catalog_combined_budget_and_utf8_description_remain_bounded() {
    let description = bounded_extension_description(&"\u{8d44}\u{6e90}\"\\\n".repeat(200));
    assert!(description.len() <= 512);
    let extensions = StartupExtensions {
        skills: StartupSkillsCatalog::available(
            format!("wc_skillcat_{}", "a".repeat(64)),
            false,
            (0..64).map(|i| skill(i, &description)).collect(),
        ),
        plugins: StartupPluginsCatalog::available(
            format!("wc_plugcat_{}", "b".repeat(64)),
            64,
            (0..64).map(|i| plugin(i, &description)).collect(),
        ),
    };
    assert!(extensions.serialized_len() <= STARTUP_EXTENSION_CATALOG_HARD_MAX_BYTES);
    assert!(extensions.skills.truncated);
    assert!(extensions.plugins.truncated);
}
