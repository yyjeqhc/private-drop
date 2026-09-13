//! Surface-shaping helpers for model-facing runtime discovery.
//!
//! These functions keep MCP-compatible tool specs, GPT Action compact
//! manifests, and bounded `list_tools` filtering close together while leaving
//! dispatch and authorization flow in `mod.rs`.

use super::kernel::ToolProtocolCapabilities;
use super::metadata::ToolAuthorityPolicy;
use super::registry::{
    accepted_flattened_args_for_spec, registered_tool_specs,
    stateless_operator_extension_tool_specs,
};
use super::runtime::ToolRuntime;
use super::tool_definition::{
    available_tool_manifest_intent_names, is_model_visible_tool_name, resolve_tool_manifest_intent,
    runtime_tool_category, runtime_tool_metadata, runtime_tool_operator_extension_family,
    ToolManifestIntent, ToolOperatorExtensionFamily, TOOL_CATEGORY_ARTIFACT, TOOL_CATEGORY_EDIT,
    TOOL_CATEGORY_GIT, TOOL_CATEGORY_PATCH, TOOL_CATEGORY_RUNTIME, TOOL_CATEGORY_SESSION,
    TOOL_CATEGORY_VALIDATION, TOOL_DISCOVERY_GROUPS, TOOL_RECOMMENDED_FLOWS,
};
use super::tool_inputs::ListToolsOptions;
use super::tool_result::ToolResult;
use super::tool_spec::ToolSpec;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

const TOOL_MANIFEST_SELECTION_DESCRIPTION_MAX_CHARS: usize = 180;
const TOOL_MANIFEST_CANONICAL_KEYS: &[&str] = &[
    "schema_version",
    "tool_count",
    "count",
    "returned_count",
    "total_count",
    "filtered_count",
    "tool_name",
    "contract",
    "category",
    "intent",
    "available_intents",
    "filtered",
    "categories_requested",
    "limit",
    "truncated",
    "truncation_reason",
    "limit_applied",
    "requested_limit",
    "categories",
    "tools",
    "risk_summary",
    "recommended_flows",
];

pub(crate) fn registered_tool_categories() -> Value {
    let mut categories = serde_json::Map::new();
    for group in TOOL_DISCOVERY_GROUPS {
        let tools = group
            .tools
            .iter()
            .filter(|name| is_model_visible_tool_name(name))
            .map(|name| Value::String((*name).to_string()))
            .collect::<Vec<_>>();
        categories.insert(group.name.to_string(), Value::Array(tools));
    }
    Value::Object(categories)
}

/// Short GPT-facing flow hints. Their compact-discovery summary budget remains
/// independently capped at 300 characters.
pub(crate) fn recommended_flows() -> Vec<&'static str> {
    TOOL_RECOMMENDED_FLOWS
        .iter()
        .map(|flow| flow.summary)
        .collect()
}

fn tool_manifest_specs(
    capabilities: ToolProtocolCapabilities,
    model_surface: crate::model_surface::ModelSurface,
) -> Vec<ToolSpec> {
    let mut specs = registered_tool_specs();
    if model_surface.supports_operator_extensions() {
        specs.extend(
            stateless_operator_extension_tool_specs()
                .into_iter()
                .filter(|spec| tool_manifest_extension_capability_allows(&spec.name, capabilities)),
        );
    }
    specs
}

fn tool_manifest_extension_capability_allows(
    tool_name: &str,
    capabilities: ToolProtocolCapabilities,
) -> bool {
    match runtime_tool_operator_extension_family(tool_name) {
        Some(ToolOperatorExtensionFamily::SkillRuntime) => capabilities.skill_runtime,
        Some(ToolOperatorExtensionFamily::SkillManagement) => capabilities.skill_management,
        Some(
            ToolOperatorExtensionFamily::MemoryRuntime
            | ToolOperatorExtensionFamily::MemoryManagement,
        ) => capabilities.memory_surface,
        Some(ToolOperatorExtensionFamily::TraceDiagnostics) => capabilities.trace_diagnostics,
        None => {
            // New extension families must declare an explicit server-owned protocol
            // capability before discovery can expose them.
            false
        }
    }
}

fn tool_manifest_route(
    spec: &ToolSpec,
    model_surface: crate::model_surface::ModelSurface,
) -> (&'static str, Option<&'static str>) {
    if is_model_visible_tool_name(spec.name.as_str()) {
        model_surface.runtime_tool_invocation_route(spec.name.as_str())
    } else {
        model_surface
            .runtime_tool_invocation_route_with_operator_extension(spec.name.as_str(), true)
    }
}

impl ToolRuntime {
    pub(crate) const LIST_TOOLS_MAX_LIMIT: usize = 256;

    pub(crate) fn list_tools_payload(&self, options: ListToolsOptions) -> Value {
        let specs = registered_tool_specs();
        let total_count = specs.len();
        let filtered_indexes = list_tools_filtered_indexes(&specs, &options);
        let filtered_count = filtered_indexes.len();
        let bounded_request = options.summary_only
            || options.category.is_some()
            || options.features.is_some()
            || options.limit.is_some();
        let effective_limit = options
            .limit
            .map(|limit| limit.clamp(1, Self::LIST_TOOLS_MAX_LIMIT))
            .unwrap_or(Self::LIST_TOOLS_MAX_LIMIT);
        let returned_indexes: Vec<usize> = if bounded_request {
            filtered_indexes
                .iter()
                .copied()
                .take(effective_limit)
                .collect()
        } else {
            filtered_indexes
        };
        let truncated = filtered_count > returned_indexes.len();
        let requested_limit = options.limit;
        let names: Vec<String> = returned_indexes
            .iter()
            .map(|index| specs[*index].name.clone())
            .collect();
        let all_summary_tools = build_list_tools_summary_entries(&specs);
        let tools = if options.summary_only {
            returned_indexes
                .iter()
                .map(|index| all_summary_tools[*index].clone())
                .collect()
        } else {
            returned_indexes
                .iter()
                .map(|index| serde_json::to_value(&specs[*index]).unwrap_or(Value::Null))
                .collect()
        };

        let mut output = json!({
            "tools": Value::Array(tools),
            "names": names,
            "count": returned_indexes.len(),
            "returned_count": returned_indexes.len(),
            "total_count": total_count,
            "filtered_count": filtered_count,
            "truncated": truncated,
            "truncation_reason": if truncated { Some("limit") } else { None },
            "limit_applied": options.limit.is_some(),
            "requested_limit": requested_limit,
            "category": options.category,
            "features": options.features,
            "limit": if bounded_request { Some(effective_limit) } else { None },
            "categories": if bounded_request {
                build_manifest_categories(&specs)
            } else {
                registered_tool_categories()
            },
            "recommended_flows": recommended_flows(),
            "recommended_next": "For daily GPT Action discovery, call callRuntimeTool with tool=tool_manifest. Use full listRuntimeTools only when debugging schemas.",
            "hint": "Full listRuntimeTools responses include schemas and may be large. Use summary_only=true with category, features, or limit for focused discovery.",
        });
        if !bounded_request {
            output["filtered_count"] = json!(total_count);
            output["total_count"] = json!(total_count);
            output["truncated"] = json!(false);
            output["truncation_reason"] = Value::Null;
            output["limit_applied"] = json!(false);
            output["requested_limit"] = Value::Null;
            output["category"] = Value::Null;
            output["features"] = Value::Null;
            output["limit"] = Value::Null;
        }
        output
    }

    /// Return compact, bounded runtime discovery. List/filter mode stays
    /// schema-free; exact tool_name mode exposes one tool's input schema while
    /// intentionally omitting its output schema. Never exposes tokens, secrets,
    /// or internal paths. Intent views only filter and rank discovery output;
    /// they do not change tool behavior, policy, permissions, execution, or
    /// finish verdict semantics. Intended as a lightweight alternative to
    /// `list_tools` for long-running tasks where full catalog schemas cause
    /// ResponseTooLargeError.
    pub(super) async fn tool_manifest(
        &self,
        tool_name: Option<String>,
        category: Option<String>,
        intent: Option<String>,
        include_recommended_flows: bool,
        include_risk_summary: bool,
        protocol_capabilities: ToolProtocolCapabilities,
    ) -> ToolResult {
        if let Some(tool_name) = tool_name {
            if category.is_some() || intent.is_some() {
                return tool_manifest_exact_filter_conflict_result();
            }
            return match self.tool_manifest_exact_payload(
                &tool_name,
                include_recommended_flows,
                include_risk_summary,
                protocol_capabilities,
            ) {
                Ok(payload) => ToolResult::ok(payload),
                Err(result) => result,
            };
        }
        match self.tool_manifest_payload(
            category,
            intent,
            include_recommended_flows,
            include_risk_summary,
            protocol_capabilities,
        ) {
            Ok(payload) => ToolResult::ok(payload),
            Err(result) => result,
        }
    }

    fn tool_manifest_exact_payload(
        &self,
        raw_tool_name: &str,
        include_recommended_flows: bool,
        include_risk_summary: bool,
        protocol_capabilities: ToolProtocolCapabilities,
    ) -> Result<Value, ToolResult> {
        let tool_name = raw_tool_name.trim();
        let model_surface = self.model_surface().ok_or_else(|| {
            ToolResult::err(
                "tool_manifest is unavailable under the project_connector runtime exposure"
                    .to_string(),
            )
        })?;
        if tool_name.is_empty() {
            return Err(unknown_tool_manifest_tool_result(tool_name));
        }
        let specs = tool_manifest_specs(protocol_capabilities, model_surface);
        let tool_count = specs.len();
        let Some(spec) = specs.iter().find(|spec| spec.name == tool_name) else {
            return Err(unknown_tool_manifest_tool_result(tool_name));
        };
        let category = runtime_tool_category(spec.name.as_str());
        let metadata = runtime_tool_metadata(spec.name.as_str());
        let (availability, gateway_tool) = tool_manifest_route(spec, model_surface);
        let mut exact_categories = serde_json::Map::new();
        exact_categories.insert(category.to_string(), json!([spec.name]));
        let mut output = json!({
            "schema_version": 1,
            "tool_count": tool_count,
            "count": 1,
            "returned_count": 1,
            "total_count": tool_count,
            "filtered_count": 1,
            "tool_name": spec.name,
            "contract": {
                "name": spec.name,
                "description": spec.description,
                "effect": metadata.effect.manifest_label(),
                "risk": metadata.risk.session_risk_class(),
                "approval": metadata.approval.manifest_label(),
                "idempotency": metadata.idempotency.manifest_label(),
                "input_schema": spec.input_schema,
                "annotations": spec.annotations,
                "availability": availability,
                "gateway_tool": gateway_tool,
            },
            "category": category,
            "intent": Value::Null,
            "available_intents": available_tool_manifest_intent_names(),
            "filtered": true,
            "categories_requested": Value::Null,
            "limit": Value::Null,
            "truncated": false,
            "truncation_reason": Value::Null,
            "limit_applied": false,
            "requested_limit": Value::Null,
            "categories": Value::Object(exact_categories),
            "tools": [compact_manifest_tool_entry(spec, model_surface)],
        });
        if include_risk_summary {
            output["risk_summary"] = build_risk_summary(&[spec]);
        }
        if include_recommended_flows {
            output["recommended_flows"] =
                Value::Array(tool_manifest_recommended_flows_for_visible_tools([spec
                    .name
                    .as_str()]));
        }
        Ok(output)
    }

    pub(crate) fn compact_tool_manifest_payload(&self) -> Value {
        self.tool_manifest_payload(None, None, true, true, ToolProtocolCapabilities::default())
            .expect("default tool_manifest payload without intent must succeed")
    }

    pub(crate) fn compact_tool_manifest_payload_bounded(
        &self,
        categories: Option<Vec<String>>,
        intent: Option<String>,
        limit: Option<usize>,
    ) -> Result<Value, ToolResult> {
        if categories.is_none() && intent.is_none() && limit.is_none() {
            return Ok(self.compact_tool_manifest_payload());
        }
        self.tool_manifest_payload_for_categories(
            categories,
            intent,
            limit,
            true,
            true,
            ToolProtocolCapabilities::default(),
        )
    }

    fn tool_manifest_payload(
        &self,
        category: Option<String>,
        intent: Option<String>,
        include_recommended_flows: bool,
        include_risk_summary: bool,
        protocol_capabilities: ToolProtocolCapabilities,
    ) -> Result<Value, ToolResult> {
        self.tool_manifest_payload_for_categories(
            category.map(|category| vec![category]),
            intent,
            None,
            include_recommended_flows,
            include_risk_summary,
            protocol_capabilities,
        )
    }

    fn tool_manifest_payload_for_categories(
        &self,
        categories: Option<Vec<String>>,
        intent: Option<String>,
        limit: Option<usize>,
        include_recommended_flows: bool,
        include_risk_summary: bool,
        protocol_capabilities: ToolProtocolCapabilities,
    ) -> Result<Value, ToolResult> {
        let resolved_intent = match intent {
            None => None,
            Some(raw) => match resolve_tool_manifest_intent(&raw) {
                Ok(intent) => intent,
                Err(unknown) => {
                    return Err(unknown_tool_manifest_intent_result(&unknown));
                }
            },
        };
        let model_surface = self.model_surface().ok_or_else(|| {
            ToolResult::err(
                "tool_manifest is unavailable under the project_connector runtime exposure"
                    .to_string(),
            )
        })?;

        let specs = tool_manifest_specs(protocol_capabilities, model_surface);
        let tool_count = specs.len();
        let categories_requested = normalize_tool_manifest_categories(categories);
        let category = categories_requested
            .as_ref()
            .and_then(|categories| (categories.len() == 1).then(|| categories[0].clone()));

        // Build the categories map from the full tool set so the caller can
        // always see valid categories even when filtering.
        let categories = build_manifest_categories(&specs);
        let available_intents = available_tool_manifest_intent_names();

        // Apply optional intent ranking, then optional category filter, then limit.
        let filtered_specs: Vec<&ToolSpec> =
            filter_manifest_specs(&specs, resolved_intent, categories_requested.as_ref());
        let filtered_count = filtered_specs.len();
        let requested_limit = limit;
        let limit = limit.map(|limit| limit.clamp(1, 100));
        let truncated = limit.is_some_and(|limit| filtered_count > limit);
        let limit_applied = requested_limit.is_some();
        let filtered =
            categories_requested.is_some() || resolved_intent.is_some() || limit.is_some();
        let intent_name = resolved_intent.map(|intent| intent.name);
        let returned_specs: Vec<&ToolSpec> = match limit {
            Some(limit) => filtered_specs.into_iter().take(limit).collect(),
            None => filtered_specs,
        };
        let risk_summary = include_risk_summary.then(|| build_risk_summary(&returned_specs));
        let tools: Vec<Value> = returned_specs
            .iter()
            .map(|spec| compact_manifest_tool_entry(spec, model_surface))
            .collect();

        let mut output = json!({
            "schema_version": 1,
            "tool_count": tool_count,
            "count": tools.len(),
            "returned_count": tools.len(),
            "total_count": tool_count,
            "filtered_count": filtered_count,
            "tool_name": Value::Null,
            "contract": Value::Null,
            "category": category,
            "intent": intent_name,
            "available_intents": available_intents,
            "filtered": filtered,
            "categories_requested": categories_requested,
            "limit": limit,
            "truncated": truncated,
            "truncation_reason": if truncated { Some("limit") } else { None },
            "limit_applied": limit_applied,
            "requested_limit": requested_limit,
            "categories": categories,
            "tools": tools,
        });

        if let Some(risk_summary) = risk_summary {
            output["risk_summary"] = risk_summary;
        }

        if include_recommended_flows {
            output["recommended_flows"] = Value::Array(if filtered {
                tool_manifest_recommended_flows_for_visible_tools(
                    returned_specs.iter().map(|spec| spec.name.as_str()),
                )
            } else {
                tool_manifest_recommended_flows()
            });
        }

        Ok(output)
    }
}

fn filter_manifest_specs<'a>(
    specs: &'a [ToolSpec],
    intent: Option<&'static ToolManifestIntent>,
    categories_requested: Option<&Vec<String>>,
) -> Vec<&'a ToolSpec> {
    let by_name: HashMap<&str, &ToolSpec> = specs
        .iter()
        .map(|spec| (spec.name.as_str(), spec))
        .collect();

    let ordered: Vec<&ToolSpec> = match intent {
        Some(intent) => intent
            .tools
            .iter()
            .filter_map(|name| by_name.get(*name).copied())
            .collect(),
        None => specs.iter().collect(),
    };

    match categories_requested {
        Some(requested) => ordered
            .into_iter()
            .filter(|spec| {
                let category = runtime_tool_category(spec.name.as_str());
                requested.iter().any(|requested| requested == category)
            })
            .collect(),
        None => ordered,
    }
}

fn unknown_tool_manifest_intent_result(unknown: &str) -> ToolResult {
    let available_intents = available_tool_manifest_intent_names();
    ToolResult::err_with_output(
        format!(
            "unknown tool_manifest intent '{}'. Available intents: {}.",
            unknown,
            available_intents.join(", ")
        ),
        json!({
            "code": "unknown_tool_manifest_intent",
            "intent": unknown,
            "available_intents": available_intents,
            "message": format!(
                "unknown tool_manifest intent '{}'; use one of: {}",
                unknown,
                available_intents.join(", ")
            ),
        }),
    )
}

fn unknown_tool_manifest_tool_result(tool_name: &str) -> ToolResult {
    ToolResult::err_with_output(
        format!("unknown tool_manifest tool_name '{tool_name}'"),
        json!({
            "code": "unknown_tool_manifest_tool",
            "tool_name": tool_name,
            "message": format!("unknown model-visible runtime tool '{tool_name}'; use category or intent discovery to find a valid name"),
        }),
    )
}

fn tool_manifest_exact_filter_conflict_result() -> ToolResult {
    ToolResult::err_with_output(
        "tool_manifest tool_name cannot be combined with category or intent",
        json!({
            "code": "tool_manifest_exact_filter_conflict",
            "message": "tool_name selects one exact contract; omit category and intent",
        }),
    )
}

pub(super) fn list_tools_filtered_indexes(
    specs: &[ToolSpec],
    options: &ListToolsOptions,
) -> Vec<usize> {
    specs
        .iter()
        .enumerate()
        .filter(|(_, spec)| {
            let name = spec.name.as_str();
            options
                .category
                .as_deref()
                .map(|category| runtime_tool_category(name) == category)
                .unwrap_or(true)
                && options
                    .features
                    .as_deref()
                    .map(|features| list_tool_matches_features(name, features))
                    .unwrap_or(true)
        })
        .map(|(index, _)| index)
        .collect()
}

pub(super) fn normalize_tool_manifest_categories(
    categories: Option<Vec<String>>,
) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for category in categories.unwrap_or_default() {
        let category = category.trim();
        if category.is_empty() || out.iter().any(|existing| existing == category) {
            continue;
        }
        out.push(category.to_string());
    }
    (!out.is_empty()).then_some(out)
}

pub(super) fn build_list_tools_summary_entries(specs: &[ToolSpec]) -> Vec<Value> {
    specs
        .iter()
        .map(|spec| {
            let name = spec.name.as_str();
            let m = runtime_tool_metadata(name);
            json!({
                "name": name,
                "description": spec.description,
                "category": runtime_tool_category(name),
                "effect": m.effect.manifest_label(),
                "risk": m.risk.session_risk_class(),
                "approval": m.approval.manifest_label(),
                "idempotency": m.idempotency.manifest_label(),
                "read_only": m.effect.read_only_hint(),
                "requires_project": m.requires_project,
                "annotations": spec.annotations,
            })
        })
        .collect()
}

fn manifest_authority(policy: ToolAuthorityPolicy) -> Value {
    match policy {
        ToolAuthorityPolicy::Require(scope) => json!({
            "policy": "require",
            "scopes": [scope],
        }),
        ToolAuthorityPolicy::RequireAny(scopes) => json!({
            "policy": "require_any",
            "scopes": scopes,
        }),
        ToolAuthorityPolicy::RequireAll(scopes) => json!({
            "policy": "require_all",
            "scopes": scopes,
        }),
        ToolAuthorityPolicy::Unknown => json!({
            "policy": "unknown",
            "scopes": [],
        }),
    }
}

fn manifest_route_projection(availability: Option<&str>, gateway_tool: Option<&Value>) -> Value {
    let mode = availability.unwrap_or("unavailable");
    let mut route = serde_json::Map::new();
    route.insert("mode".to_string(), Value::String(mode.to_string()));
    if mode == "gateway" {
        if let Some(via) = gateway_tool.and_then(Value::as_str) {
            route.insert("via".to_string(), Value::String(via.to_string()));
        }
    }
    Value::Object(route)
}

fn selection_description(description: &str) -> String {
    let description = description.trim();
    if let Some(end) = description.char_indices().find_map(|(index, ch)| {
        let end = index + ch.len_utf8();
        if !matches!(ch, '.' | '!' | '?')
            || description[..end].chars().count() > TOOL_MANIFEST_SELECTION_DESCRIPTION_MAX_CHARS
        {
            return None;
        }
        description[end..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
            .then_some(end)
    }) {
        return description[..end].trim().to_string();
    }

    if description.chars().count() <= TOOL_MANIFEST_SELECTION_DESCRIPTION_MAX_CHARS {
        return description.to_string();
    }

    let content_limit = TOOL_MANIFEST_SELECTION_DESCRIPTION_MAX_CHARS - 1;
    let byte_end = description
        .char_indices()
        .nth(content_limit - 1)
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(description.len());
    let prefix = &description[..byte_end];
    let cut = prefix
        .char_indices()
        .rev()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(index))
        .filter(|index| *index > TOOL_MANIFEST_SELECTION_DESCRIPTION_MAX_CHARS / 2)
        .unwrap_or(byte_end);
    format!("{}…", description[..cut].trim_end())
}

#[cfg(test)]
mod selection_description_tests {
    use super::*;

    #[test]
    fn selection_description_ignores_periods_inside_technical_tokens() {
        assert_eq!(
            selection_description(
                "Inspect startup-bound runner.toml path. This never changes authority."
            ),
            "Inspect startup-bound runner.toml path."
        );
        assert_eq!(
            selection_description("Read foo.rs safely. Then continue."),
            "Read foo.rs safely."
        );
        assert_eq!(
            selection_description("Supports v0.4.0 clients. Newer versions are also accepted."),
            "Supports v0.4.0 clients."
        );
    }

    #[test]
    fn selection_description_prefers_real_sentence_boundary() {
        assert_eq!(
            selection_description("First sentence. Second sentence."),
            "First sentence."
        );
    }

    #[test]
    fn selection_description_fallback_is_bounded_and_unicode_safe() {
        let ascii = "selection token ".repeat(30);
        let ascii_summary = selection_description(&ascii);
        assert!(ascii_summary.ends_with('…'));
        assert!(
            ascii_summary.chars().count() <= TOOL_MANIFEST_SELECTION_DESCRIPTION_MAX_CHARS,
            "{ascii_summary}"
        );

        let unicode = "界".repeat(240);
        let unicode_summary = selection_description(&unicode);
        assert!(unicode_summary.ends_with('…'));
        assert!(
            unicode_summary.chars().count() <= TOOL_MANIFEST_SELECTION_DESCRIPTION_MAX_CHARS,
            "{unicode_summary}"
        );
        assert!(unicode_summary
            .trim_end_matches('…')
            .chars()
            .all(|ch| ch == '界'));
    }
}

/// Project the canonical manifest only after Session/audit consumers have seen
/// it. This preserves the compatibility/full diagnostic result internally while
/// making ordinary model discovery answer only the next selection/call decision.
/// Unknown output sidecars are preserved verbatim.
pub(super) fn sparsify_tool_manifest_model_result(result: &mut ToolResult) {
    if !result.success {
        return;
    }
    let Some(output) = result.output.as_object() else {
        return;
    };
    let canonical = output.clone();
    let mut projected = serde_json::Map::new();

    if let Some(contract) = canonical.get("contract").and_then(Value::as_object) {
        for key in [
            "name",
            "description",
            "input_schema",
            "effect",
            "risk",
            "approval",
            "idempotency",
            "annotations",
        ] {
            if let Some(value) = contract.get(key) {
                projected.insert(key.to_string(), value.clone());
            }
        }
        projected.insert(
            "route".to_string(),
            manifest_route_projection(
                contract.get("availability").and_then(Value::as_str),
                contract.get("gateway_tool"),
            ),
        );
        if let Some(authority) = canonical
            .get("tools")
            .and_then(Value::as_array)
            .and_then(|tools| tools.first())
            .and_then(|tool| tool.get("authority"))
        {
            projected.insert("authority".to_string(), authority.clone());
        }
        if let Some(flows) = canonical
            .get("recommended_flows")
            .and_then(Value::as_array)
            .filter(|flows| !flows.is_empty())
        {
            projected.insert("recommended_flows".to_string(), Value::Array(flows.clone()));
        }
    } else if canonical.get("filtered").and_then(Value::as_bool) == Some(true) {
        let specs = registered_tool_specs();
        let descriptions: HashMap<&str, &str> = specs
            .iter()
            .map(|spec| (spec.name.as_str(), spec.description.as_str()))
            .collect();
        let tools = canonical
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|tool| {
                        let mut entry = serde_json::Map::new();
                        if let Some(name) = tool.get("name").and_then(Value::as_str) {
                            entry.insert("name".to_string(), Value::String(name.to_string()));
                            if let Some(description) = descriptions.get(name) {
                                entry.insert(
                                    "description".to_string(),
                                    Value::String(selection_description(description)),
                                );
                            }
                        }
                        entry.insert(
                            "route".to_string(),
                            manifest_route_projection(
                                tool.get("availability").and_then(Value::as_str),
                                tool.get("gateway_tool"),
                            ),
                        );
                        if let Some(requires_project) = tool.get("requires_project") {
                            entry.insert("requires_project".to_string(), requires_project.clone());
                        }
                        if let Some(effect) = tool.get("effect") {
                            entry.insert("effect".to_string(), effect.clone());
                        }
                        if tool.get("effect").and_then(Value::as_str) != Some("observe") {
                            if let Some(risk) = tool.get("risk") {
                                entry.insert("risk".to_string(), risk.clone());
                            }
                        }
                        Value::Object(entry)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        projected.insert("tools".to_string(), Value::Array(tools));
        for key in ["intent", "category"] {
            if let Some(value) = canonical.get(key).filter(|value| !value.is_null()) {
                projected.insert(key.to_string(), value.clone());
            }
        }
        if canonical.get("truncated").and_then(Value::as_bool) == Some(true) {
            for key in [
                "truncated",
                "truncation_reason",
                "returned_count",
                "filtered_count",
                "limit",
            ] {
                if let Some(value) = canonical.get(key) {
                    projected.insert(key.to_string(), value.clone());
                }
            }
        }
        if let Some(flows) = canonical
            .get("recommended_flows")
            .and_then(Value::as_array)
            .filter(|flows| !flows.is_empty())
        {
            projected.insert("recommended_flows".to_string(), Value::Array(flows.clone()));
        }
    } else {
        for key in [
            "schema_version",
            "tool_count",
            "categories",
            "available_intents",
            "risk_summary",
            "recommended_flows",
        ] {
            if let Some(value) = canonical.get(key) {
                projected.insert(key.to_string(), value.clone());
            }
        }
    }

    for (key, value) in canonical {
        if !TOOL_MANIFEST_CANONICAL_KEYS.contains(&key.as_str()) {
            projected.insert(key, value);
        }
    }
    result.output = Value::Object(projected);
}

pub(super) fn compact_manifest_tool_entry(
    spec: &ToolSpec,
    model_surface: crate::model_surface::ModelSurface,
) -> Value {
    let name = spec.name.as_str();
    let m = runtime_tool_metadata(name);
    let (availability, gateway_tool) = tool_manifest_route(spec, model_surface);
    json!({
        "name": name,
        "category": runtime_tool_category(name),
        "accepted_flattened_args": accepted_flattened_args_for_spec(spec),
        "deprecated_or_unsupported_args": [],
        "provider": m.provider_id,
        "effect": m.effect.manifest_label(),
        "risk": m.risk.session_risk_class(),
        "approval": m.approval.manifest_label(),
        "idempotency": m.idempotency.manifest_label(),
        "read_only": m.effect.read_only_hint(),
        "requires_project": m.requires_project,
        "path_hint": m.path_hint.manifest_label(),
        "destructive": m.destructive,
        "shell_like": m.shell_like,
        "authority": manifest_authority(m.authority),
        "availability": availability,
        "gateway_tool": gateway_tool,
    })
}

fn list_tool_matches_features(name: &str, features: &str) -> bool {
    features
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter_map(normalize_feature)
        .any(|feature| list_tool_matches_feature(name, feature.as_str()))
}

fn normalize_feature(feature: &str) -> Option<String> {
    let normalized = feature.trim().to_ascii_lowercase().replace('-', "_");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn list_tool_matches_feature(name: &str, feature: &str) -> bool {
    let category = runtime_tool_category(name);
    if category == feature {
        return true;
    }
    match feature {
        "artifact" => category == TOOL_CATEGORY_ARTIFACT,
        "artifact_upload" | "upload" => name.starts_with("artifact_upload_"),
        "read" => {
            runtime_tool_metadata(name).effect.read_only_hint()
                || name.starts_with("read_")
                || name.contains("_read_")
        }
        "edit" => matches!(category, TOOL_CATEGORY_EDIT | TOOL_CATEGORY_PATCH),
        "session" => category == TOOL_CATEGORY_SESSION,
        "git" => category == TOOL_CATEGORY_GIT,
        "validation" => category == TOOL_CATEGORY_VALIDATION,
        "runtime" => category == TOOL_CATEGORY_RUNTIME,
        other => name.contains(other),
    }
}

/// Build the categories map from runtime tool specs. Each category
/// maps to a sorted list of tool names.
pub(super) fn build_manifest_categories(specs: &[ToolSpec]) -> Value {
    let mut map: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for spec in specs {
        let name = spec.name.as_str();
        let category = runtime_tool_category(name);
        map.entry(category).or_default().push(name.to_string());
    }
    let result: serde_json::Map<String, Value> = map
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                Value::Array(v.into_iter().map(Value::String).collect()),
            )
        })
        .collect();
    Value::Object(result)
}

/// Build the risk summary map from the returned compact manifest specs.
pub(super) fn build_risk_summary(specs: &[&ToolSpec]) -> Value {
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for spec in specs {
        let risk = runtime_tool_metadata(spec.name.as_str())
            .risk
            .session_risk_class();
        *counts.entry(risk).or_insert(0) += 1;
    }
    let result: serde_json::Map<String, Value> = counts
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::from(v)))
        .collect();
    Value::Object(result)
}

/// Short, bounded list of recommended tool flows for common tasks. Each
/// entry references only known tool names. Kept under 10 entries.
pub(super) fn tool_manifest_recommended_flows() -> Vec<Value> {
    TOOL_RECOMMENDED_FLOWS
        .iter()
        .map(|flow| {
            json!({
                "name": flow.name,
                "purpose": flow.manifest_purpose,
                "tools": flow.tools,
            })
        })
        .collect()
}

/// Project recommended flows onto the tools actually returned by a filtered
/// manifest. Keeps original flow tool order, drops duplicates, and omits flows
/// that project to an empty tool list. Unfiltered callers should use the full
/// global flows instead.
pub(super) fn tool_manifest_recommended_flows_for_visible_tools<'a, I>(
    visible_tools: I,
) -> Vec<Value>
where
    I: IntoIterator<Item = &'a str>,
{
    let visible: std::collections::HashSet<&str> = visible_tools.into_iter().collect();
    TOOL_RECOMMENDED_FLOWS
        .iter()
        .filter_map(|flow| {
            let mut seen = std::collections::HashSet::new();
            let tools: Vec<&str> = flow
                .tools
                .iter()
                .copied()
                .filter(|tool| visible.contains(*tool) && seen.insert(*tool))
                .collect();
            if tools.is_empty() {
                return None;
            }
            Some(json!({
                "name": flow.name,
                "purpose": flow.manifest_purpose,
                "tools": tools,
            }))
        })
        .collect()
}
