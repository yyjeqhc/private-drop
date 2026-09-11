use super::project_resolution::ResolvedProject;
use super::startup_brief::{
    builtin_coding_workflow_projection, project_instructions_context_projection,
};
use super::{ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use serde_json::{json, Value};
use std::collections::HashSet;

pub(crate) const TOOL_CALL_CONTEXT_REQUEST_FIELD: &str = "context_request";
pub(crate) const MAX_CONTEXT_REQUEST_ITEMS: usize = 8;
pub(crate) const MAX_CONTEXT_REQUEST_KEY_CHARS: usize = 64;
pub(crate) const MAX_CONTEXT_PROJECTION_BYTES: usize = 20 * 1024;
const PLUGIN_CATALOG_SCOPES: &[&str] = &[
    crate::auth::SCOPE_PROJECT_READ,
    crate::auth::SCOPE_PLUGIN_INSPECT,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextMaterialScopePolicy {
    Public,
    Require(&'static str),
    RequireAll(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextMaterialSurface {
    AnySidecar,
    SkillRuntime,
    MemorySurface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextMaterialSpec {
    pub(crate) key: &'static str,
    pub(crate) project_required: bool,
    pub(crate) scope_policy: ContextMaterialScopePolicy,
    pub(crate) surface: ContextMaterialSurface,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ContextMaterialCapabilities {
    pub(crate) skill_runtime: bool,
    pub(crate) memory_surface: bool,
}

pub(crate) const CONTEXT_MATERIAL_SPECS: &[ContextMaterialSpec] = &[
    ContextMaterialSpec {
        key: "project.instructions",
        project_required: true,
        scope_policy: ContextMaterialScopePolicy::Require(crate::auth::SCOPE_PROJECT_READ),
        surface: ContextMaterialSurface::AnySidecar,
    },
    ContextMaterialSpec {
        key: "webcodex.workflow",
        project_required: false,
        scope_policy: ContextMaterialScopePolicy::Public,
        surface: ContextMaterialSurface::AnySidecar,
    },
    ContextMaterialSpec {
        key: "skills.catalog",
        project_required: true,
        scope_policy: ContextMaterialScopePolicy::Require(crate::auth::SCOPE_PROJECT_READ),
        surface: ContextMaterialSurface::SkillRuntime,
    },
    ContextMaterialSpec {
        key: "plugins.catalog",
        project_required: true,
        scope_policy: ContextMaterialScopePolicy::RequireAll(PLUGIN_CATALOG_SCOPES),
        surface: ContextMaterialSurface::AnySidecar,
    },
    ContextMaterialSpec {
        key: "memory.bootstrap",
        project_required: true,
        scope_policy: ContextMaterialScopePolicy::RequireAll(
            webcodex_core::authority::MEMORY_READ_SCOPES,
        ),
        surface: ContextMaterialSurface::MemorySurface,
    },
];

pub(crate) fn context_material_keys_csv() -> String {
    CONTEXT_MATERIAL_SPECS
        .iter()
        .map(|spec| spec.key)
        .collect::<Vec<_>>()
        .join(", ")
}

fn context_material_spec(key: &str) -> Option<&'static ContextMaterialSpec> {
    CONTEXT_MATERIAL_SPECS.iter().find(|spec| spec.key == key)
}

fn context_material_surface_available(
    surface: ContextMaterialSurface,
    capabilities: ContextMaterialCapabilities,
) -> bool {
    match surface {
        ContextMaterialSurface::AnySidecar => true,
        ContextMaterialSurface::SkillRuntime => capabilities.skill_runtime,
        ContextMaterialSurface::MemorySurface => capabilities.memory_surface,
    }
}

fn context_material_scope_available(
    policy: ContextMaterialScopePolicy,
    auth: Option<&AuthContext>,
) -> bool {
    match policy {
        ContextMaterialScopePolicy::Public => true,
        ContextMaterialScopePolicy::Require(scope) => {
            auth.is_some_and(|auth| auth.has_scope(scope))
        }
        ContextMaterialScopePolicy::RequireAll(scopes) => {
            auth.is_some_and(|auth| scopes.iter().all(|scope| auth.has_scope(scope)))
        }
    }
}

fn projection_envelope(materials: Vec<Value>, truncated: bool) -> Value {
    json!({
        "timing": "post_tool",
        "applies_to_current_effect": false,
        "materials": materials,
        "truncated": truncated,
    })
}

fn fits_projection_budget(materials: &[Value], truncated: bool) -> bool {
    serde_json::to_vec(&projection_envelope(materials.to_vec(), truncated))
        .map(|bytes| bytes.len() <= MAX_CONTEXT_PROJECTION_BYTES)
        .unwrap_or(false)
}

fn unavailable(key: &str, reason_code: &str) -> Value {
    json!({
        "key": key,
        "status": "unavailable",
        "reason_code": reason_code,
    })
}

fn scope_unavailable_reason(key: &str) -> &'static str {
    if key == "plugins.catalog" {
        "plugin_inspect_scope_unavailable"
    } else {
        "context_material_scope_unavailable"
    }
}

impl ToolRuntime {
    pub(crate) async fn add_requested_context_projection(
        &self,
        result: &mut ToolResult,
        requested: &[String],
        resolved_project: Option<&ResolvedProject>,
        auth: Option<&AuthContext>,
        capabilities: ContextMaterialCapabilities,
    ) {
        if requested.is_empty() {
            return;
        }

        let mut seen = HashSet::new();
        let mut materials = Vec::new();
        let mut truncated = false;
        for key in requested
            .iter()
            .map(|key| key.trim())
            .filter(|key| !key.is_empty())
            .filter(|key| seen.insert((*key).to_string()))
            .take(MAX_CONTEXT_REQUEST_ITEMS)
        {
            let material = if let Some(spec) = context_material_spec(key) {
                if !context_material_surface_available(spec.surface, capabilities) {
                    unavailable(key, "context_material_surface_unavailable")
                } else if spec.project_required && resolved_project.is_none() {
                    unavailable(key, "project_target_unavailable")
                } else if !context_material_scope_available(spec.scope_policy, auth) {
                    unavailable(key, scope_unavailable_reason(key))
                } else {
                    match key {
                        "project.instructions" => {
                            let project =
                                resolved_project.expect("registry requires project target");
                            let snapshot =
                                self.load_coding_project_instructions(&project.config).await;
                            let projection = project_instructions_context_projection(&snapshot);
                            if snapshot.scan_complete {
                                json!({
                                    "key": key,
                                    "status": "available",
                                    "projection": projection,
                                })
                            } else {
                                json!({
                                    "key": key,
                                    "status": "unavailable",
                                    "reason_code": "project_instructions_observation_incomplete",
                                    "projection": projection,
                                })
                            }
                        }
                        "skills.catalog" => {
                            let project =
                                resolved_project.expect("registry requires project target");
                            match self.skills_catalog_context_projection(project, auth).await {
                                Ok(projection) => json!({
                                    "key": key,
                                    "status": "available",
                                    "projection": projection,
                                }),
                                Err(reason_code) => unavailable(key, reason_code),
                            }
                        }
                        "plugins.catalog" => {
                            let project =
                                resolved_project.expect("registry requires project target");
                            match self
                                .plugin_project_catalog_context_projection(project, auth)
                                .await
                            {
                                Ok(projection) => json!({
                                    "key": key,
                                    "status": "available",
                                    "projection": projection,
                                }),
                                Err(reason_code) => unavailable(key, reason_code),
                            }
                        }
                        "memory.bootstrap" => {
                            let project =
                                resolved_project.expect("registry requires project target");
                            match self.memory_bootstrap_context_projection(project) {
                                Ok(projection) => json!({
                                    "key": key,
                                    "status": "available",
                                    "projection": projection,
                                }),
                                Err(reason_code) => unavailable(key, reason_code),
                            }
                        }
                        "webcodex.workflow" => json!({
                            "key": key,
                            "status": "available",
                            "projection": builtin_coding_workflow_projection(),
                        }),
                        _ => unreachable!("context material registry/provider match drifted"),
                    }
                }
            } else {
                json!({
                    "key": key,
                    "status": "unsupported",
                    "reason_code": "unsupported_context_material",
                })
            };

            let mut candidate = materials.clone();
            candidate.push(material.clone());
            if fits_projection_budget(&candidate, truncated) {
                materials.push(material);
                continue;
            }

            truncated = true;
            let bounded = unavailable(key, "context_projection_budget_exceeded");
            let mut bounded_candidate = materials.clone();
            bounded_candidate.push(bounded.clone());
            if fits_projection_budget(&bounded_candidate, true) {
                materials.push(bounded);
            }
        }

        let projection = projection_envelope(materials, truncated);
        debug_assert!(
            serde_json::to_vec(&projection)
                .map(|bytes| bytes.len() <= MAX_CONTEXT_PROJECTION_BYTES)
                .unwrap_or(false),
            "context projection must stay inside its independent budget"
        );
        let mut output = match std::mem::take(&mut result.output) {
            Value::Object(output) => output,
            other => {
                let mut output = serde_json::Map::new();
                output.insert("value".to_string(), other);
                output
            }
        };
        output.insert("context_projection".to_string(), projection);
        result.output = Value::Object(output);
    }
}
