//! Runner-side project management tools: `register_project`, `unregister_project`,
//! and `create_project`.
//!
//! Registration and creation route to the selected Runner through the project-op
//! path. Unregistration reuses the shared project lifecycle path so the
//! model-facing tool and `POST /api/projects/unregister` have the same revision
//! CAS, active-Job fence, capability check, uncertain-delivery semantics, and
//! server inventory update. The Runner remains authoritative for its local
//! project registration records in the Runner project registry.
//!
//! The server never writes project config files or creates directories on the
//! Runner host directly. OS permissions and Runner policy
//! (`allow_cwd_anywhere` / `allowed_roots`) remain the real boundary; there is
//! no workspace abstraction.

use serde_json::{json, Value};
use std::time::Duration;

use super::tool_result::{RecoveryKind, ToolResult};
use super::{runner_project_runtime_id, ToolRuntime};
use crate::auth::AuthContext;
use crate::runner_http::{RunnerFeature, RunnerSemanticView};
use crate::runner_protocol::{RunnerProjectSummary, RUNNER_CAPABILITY_PROJECT_PATH_REGISTRATION};

/// Maximum time the runtime waits for a Runner project-op response. Project
/// operations are fast (write a small TOML, maybe create a directory + git
/// init), so 30s is generous while still bounding the caller.
const PROJECT_OP_WAIT_SECS: u64 = 32;
const AUTO_REGISTERED_PROJECT_SOURCE: &str = "auto_registered";

const LIST_PROJECTS_MAX_QUERY_CHARS: usize = 200;
const LIST_PROJECTS_MAX_RESULTS: usize = 100;

#[derive(Debug, Default)]
pub(crate) struct ListProjectsOptions {
    pub(crate) client_id: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) query: Option<String>,
    pub(crate) limit: Option<usize>,
    pub(crate) summary_only: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ProjectCandidate {
    pub(super) runtime_id: String,
    pub(super) client_index: usize,
    pub(super) project_index: usize,
}

fn validate_list_projects_options(
    options: &ListProjectsOptions,
) -> Result<(Option<String>, Option<usize>), ToolResult> {
    if options
        .client_id
        .as_deref()
        .is_some_and(|value| value.is_empty() || value.chars().count() > 128)
    {
        return Err(ToolResult::err_with_output(
            "invalid_client_id: client_id must contain 1..=128 characters".to_string(),
            json!({"error_kind": "invalid_client_id"}),
        ));
    }
    if options
        .project
        .as_deref()
        .is_some_and(|value| value.is_empty() || value.chars().count() > 512)
    {
        return Err(ToolResult::err_with_output(
            "invalid_project: project must contain 1..=512 characters".to_string(),
            json!({"error_kind": "invalid_project"}),
        ));
    }
    let query = match options.query.as_deref() {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(ToolResult::err_with_output(
                    "invalid_query: query must not be empty after trimming".to_string(),
                    json!({"error_kind": "invalid_query"}),
                ));
            }
            if trimmed.chars().count() > LIST_PROJECTS_MAX_QUERY_CHARS {
                return Err(ToolResult::err_with_output(
                    format!(
                        "invalid_query: query exceeds {LIST_PROJECTS_MAX_QUERY_CHARS} characters"
                    ),
                    json!({"error_kind": "invalid_query"}),
                ));
            }
            Some(trimmed.to_lowercase())
        }
        None => None,
    };
    let filtered =
        options.client_id.is_some() || options.project.is_some() || options.query.is_some();
    let limit = options
        .limit
        .map(|limit| limit.clamp(1, LIST_PROJECTS_MAX_RESULTS))
        .or_else(|| filtered.then_some(LIST_PROJECTS_MAX_RESULTS));
    Ok((query, limit))
}

fn project_candidates(
    clients: &[RunnerSemanticView],
    options: &ListProjectsOptions,
    query: Option<&str>,
) -> Vec<ProjectCandidate> {
    let mut candidates = Vec::new();
    for (client_index, client) in clients.iter().enumerate() {
        let view = &client.view;
        if options
            .client_id
            .as_deref()
            .is_some_and(|expected| view.client_id != expected)
        {
            continue;
        }
        for (project_index, project) in view.projects.iter().enumerate() {
            let runtime_id = runner_project_runtime_id(&view.client_id, &project.id);
            if options
                .project
                .as_deref()
                .is_some_and(|expected| runtime_id != expected)
            {
                continue;
            }
            if query.is_some_and(|needle| !project_query_matches(needle, &runtime_id, project)) {
                continue;
            }
            candidates.push(ProjectCandidate {
                runtime_id,
                client_index,
                project_index,
            });
        }
    }
    candidates.sort_by(|a, b| a.runtime_id.cmp(&b.runtime_id));
    candidates
}

impl ToolRuntime {
    pub(crate) async fn list_projects(&self, auth: Option<&AuthContext>) -> ToolResult {
        self.list_projects_with_options(auth, ListProjectsOptions::default())
            .await
    }

    pub(crate) async fn list_projects_with_options(
        &self,
        auth: Option<&AuthContext>,
        options: ListProjectsOptions,
    ) -> ToolResult {
        let (query, limit) = match validate_list_projects_options(&options) {
            Ok(validated) => validated,
            Err(result) => return result,
        };
        let access = crate::runner_http::runner_access_from_auth(auth);
        let clients = self
            .runner_registry
            .list_runner_semantic_views_for_auth(access.as_ref())
            .await;
        self.list_projects_from_semantic_clients(auth, &options, query.as_deref(), limit, &clients)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn list_projects_with_visible_clients_for_test(
        &self,
        auth: Option<&AuthContext>,
        options: ListProjectsOptions,
        clients: &[crate::runner_protocol::RunnerView],
    ) -> ToolResult {
        let (query, limit) = match validate_list_projects_options(&options) {
            Ok(validated) => validated,
            Err(result) => return result,
        };
        let semantic_clients = clients
            .iter()
            .cloned()
            .map(RunnerSemanticView::from_public_view_for_test)
            .collect::<Vec<_>>();
        self.list_projects_from_semantic_clients(
            auth,
            &options,
            query.as_deref(),
            limit,
            &semantic_clients,
        )
        .await
    }

    async fn list_projects_from_semantic_clients(
        &self,
        auth: Option<&AuthContext>,
        options: &ListProjectsOptions,
        query: Option<&str>,
        limit: Option<usize>,
        clients: &[RunnerSemanticView],
    ) -> ToolResult {
        let access = crate::runner_http::runner_access_from_auth(auth);
        let mut candidates = project_candidates(clients, options, query);
        let matched_count = candidates.len();
        if let Some(limit) = limit {
            candidates.truncate(limit);
        }
        let truncated = candidates.len() < matched_count;

        let mut list = Vec::with_capacity(candidates.len());
        for ProjectCandidate {
            runtime_id,
            client_index,
            project_index,
        } in candidates
        {
            // Extract only one selected Project and the small Runner fields used by
            // its projection before awaiting Job state. Candidate staging above
            // never owns or clones a RunnerView (and therefore never clones
            // the Runner's complete projects Vec per match).
            let (
                client_id,
                runner_status,
                connected,
                last_seen,
                project,
                resolved_shell_profile,
                shell_profile_status,
                capabilities,
            ) = {
                let client = &clients[client_index];
                let view = &client.view;
                let project = &view.projects[project_index];
                let shell_profiles = view
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.shell_profiles.as_ref());
                let (resolved_shell_profile, shell_profile_status) =
                    resolve_project_shell_profile(project.shell_profile.as_deref(), shell_profiles);
                (
                    view.client_id.clone(),
                    view.status.clone(),
                    view.connected,
                    view.last_seen,
                    project.clone(),
                    resolved_shell_profile,
                    shell_profile_status,
                    smoke_project_capabilities(client, project),
                )
            };
            let active_jobs = self
                .runner_registry
                .count_active_jobs_for_project(access.as_ref(), &runtime_id)
                .await;
            let value = if options.summary_only {
                json!({
                    "id": runtime_id,
                    "agent_project_id": project.id,
                    "name": project.name,
                    "description": project.description,
                    "executor": "agent",
                    "client_id": client_id,
                    "enabled": !project.disabled,
                    "active_jobs": active_jobs,
                    "source": project_source(&project),
                    // Published v0.4 serialized key; retain the exact spelling through 0.4.x.
                    "agent_status": runner_status,
                    "connected": connected,
                    "resolved_shell_profile": resolved_shell_profile,
                    "shell_profile_status": shell_profile_status,
                    "capabilities": {
                        "git_available": capabilities["git_available"],
                        "recommended_for_smoke": capabilities["recommended_for_smoke"],
                    },
                })
            } else {
                json!({
                    "id": runtime_id,
                    "agent_project_id": project.id,
                    "name": project.name,
                    "path": project.path,
                    "executor": "agent",
                    "client_id": client_id,
                    "allow_patch": project.allow_patch,
                    "description": project.description,
                    "enabled": !project.disabled,
                    "disabled": project.disabled,
                    "revision": project.revision,
                    "active_jobs": active_jobs,
                    "source": project_source(&project),
                    // Published v0.4 serialized key; retain the exact spelling through 0.4.x.
                    "agent_status": runner_status,
                    "connected": connected,
                    "last_seen": last_seen,
                    "shell_profile": project.shell_profile,
                    "resolved_shell_profile": resolved_shell_profile,
                    "shell_profile_status": shell_profile_status,
                    "capabilities": capabilities,
                })
            };
            list.push(value);
        }
        let recommended_for_smoke: Vec<Value> = list
            .iter()
            .filter(|project| {
                project["capabilities"]["recommended_for_smoke"]
                    .as_bool()
                    .unwrap_or(false)
            })
            .filter_map(|project| project["id"].as_str().map(|id| json!(id)))
            .collect();
        ToolResult::ok(json!({
            "count": list.len(),
            "matched_count": matched_count,
            "truncated": truncated,
            "projects": list,
            "recommended_for_smoke": recommended_for_smoke,
        }))
    }

    /// Register an existing directory as a WebCodex project on the selected
    /// agent. See the `ToolCall::RegisterProject` doc comment for the full
    /// contract. The server validates the owner boundary, builds a JSON
    /// payload, routes it to the agent, parses the JSON response, and
    /// refreshes the server-side project cache.
    pub(crate) async fn register_project(
        &self,
        client_id: String,
        id: String,
        name: String,
        path: String,
        description: Option<String>,
        allow_patch: bool,
        overwrite: bool,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.project_op(
            "register_project",
            client_id,
            id,
            name,
            path,
            description,
            allow_patch,
            None,
            false,
            false,
            overwrite,
            auth,
        )
        .await
    }

    /// Remove only one exact Runner project registration. The caller supplies
    /// the revision observed from `list_projects`; the shared lifecycle core
    /// keeps CAS, active-Job fencing, owner filtering, and uncertain-delivery
    /// semantics identical to `POST /api/projects/unregister`.
    pub(crate) async fn unregister_project(
        &self,
        project: String,
        expected_revision: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let response = crate::admin_project_lifecycle::unregister_project_runtime(
            self,
            auth,
            &project,
            &expected_revision,
        )
        .await;
        if (200..300).contains(&response.status) {
            return ToolResult::ok(response.body);
        }
        let error_code = response
            .body
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("operation_failed")
            .to_string();
        let message = if error_code == "operation_indeterminate" {
            "operation_indeterminate: unregister may have completed; call list_projects and reconcile the exact project before retrying".to_string()
        } else {
            error_code.clone()
        };
        ToolResult::err_with_output(message, response.body)
    }

    /// Create a new directory on the selected Runner, or explicitly adopt an
    /// already-existing empty directory, and register it as a WebCodex project.
    /// See the `ToolCall::CreateProject` doc comment for the full contract.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_project(
        &self,
        client_id: String,
        id: String,
        name: String,
        path: String,
        description: Option<String>,
        allow_patch: bool,
        template: Option<String>,
        git_init: bool,
        adopt_existing_empty: bool,
        overwrite: bool,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.project_op(
            "create_project",
            client_id,
            id,
            name,
            path,
            description,
            allow_patch,
            template,
            git_init,
            adopt_existing_empty,
            overwrite,
            auth,
        )
        .await
    }

    /// Ask the selected Runner to resolve an existing registration by
    /// canonical path or persist a new project registration record under its registry
    /// write lock. This internal operation is intentionally absent from the
    /// model-visible tool registry.
    pub(crate) async fn resolve_or_register_project(
        &self,
        client_id: String,
        path: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let access = crate::runner_http::runner_access_from_auth(auth);
        if let Err(error) = validate_project_op_path(&path) {
            return ToolResult::err_with_output(
                error,
                json!({
                    "error_kind": "invalid_project_path",
                    "failure_kind": "invalid_arguments",
                    "field": "path",
                    "state_changed": false,
                }),
            );
        }
        if let Some(client) = self
            .runner_registry
            .get_runner_semantic_view_for_auth(&client_id, access.as_ref())
            .await
        {
            if let Err(error) = self
                .runner_registry
                .assert_runner_access(access.as_ref(), &client_id)
                .await
            {
                return ToolResult::err(error);
            }
            if !client.supports(RunnerFeature::ProjectPathRegistration) {
                return ToolResult::err_with_output(
                    "agent_capability_unavailable: accepted Runner violated the generation-2 project_path_registration baseline",
                    json!({
                        "error_kind": "agent_capability_unavailable",
                        "failure_kind": "capability_unavailable",
                        "reason_code": "runner_generation_baseline_invariant",
                        "capability": RUNNER_CAPABILITY_PROJECT_PATH_REGISTRATION,
                        "state_changed": false,
                    }),
                )
                .with_recovery(RecoveryKind::NoAction, None);
            }
        }
        self.submit_project_op(
            "resolve_or_register_project",
            client_id,
            json!({"path": path}),
            auth,
        )
        .await
    }

    /// Ask the exact Runner to prepare or re-observe one managed detached
    /// worktree and register it as an ordinary runtime Project. `operation_id`
    /// is transport/idempotency identity only and grants no Project authority.
    pub(crate) async fn prepare_managed_worktree(
        &self,
        client_id: String,
        path: String,
        base_ref: Option<String>,
        operation_id: String,
        resume_project_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let access = crate::runner_http::runner_access_from_auth(auth);
        if let Err(error) = validate_project_op_path(&path) {
            return ToolResult::err_with_output(
                error,
                json!({
                    "error_kind": "invalid_project_path",
                    "failure_kind": "invalid_arguments",
                    "field": "path",
                    "state_changed": false,
                }),
            );
        }
        if let Some(client) = self
            .runner_registry
            .get_runner_semantic_view_for_auth(&client_id, access.as_ref())
            .await
        {
            if let Err(error) = self
                .runner_registry
                .assert_runner_access(access.as_ref(), &client_id)
                .await
            {
                return ToolResult::err(error);
            }
            if !client.supports(RunnerFeature::ManagedWorktree) {
                return ToolResult::err_with_output(
                    "agent_capability_unavailable: Runner does not support managed worktree bootstrap",
                    json!({
                        "error_kind": "agent_capability_unavailable",
                        "failure_kind": "capability_unavailable",
                        "capability": crate::runner_protocol::RUNNER_CAPABILITY_MANAGED_WORKTREE,
                        "state_changed": false,
                    }),
                )
                .with_recovery(RecoveryKind::NoAction, None);
            }
        }
        let fresh_managed_bootstrap = resume_project_id.is_none();
        let payload = json!({
            "path": path,
            "base_ref": base_ref,
            "operation_id": operation_id,
            "resume_project_id": resume_project_id,
        });
        let first = self
            .submit_project_op(
                "prepare_managed_worktree",
                client_id.clone(),
                payload.clone(),
                auth,
            )
            .await;
        let indeterminate = !first.success
            && ["error_code", "error_kind"].into_iter().any(|field| {
                first.output.get(field).and_then(Value::as_str) == Some("operation_indeterminate")
            });
        if !indeterminate {
            return first;
        }
        // A lost response may have followed either worktree creation or registry
        // publication. Re-issue the *same* operation identity exactly once: the
        // Runner only re-observes/converges that deterministic target and never
        // creates a second worktree for this tool invocation.
        let mut recovered = self
            .submit_project_op("prepare_managed_worktree", client_id, payload, auth)
            .await;
        if recovered.success
            && fresh_managed_bootstrap
            && recovered.output.get("outcome").and_then(Value::as_str)
                == Some("managed_worktree_recovered")
            && recovered.output.get("registered").and_then(Value::as_bool) == Some(false)
        {
            // This operation_id is freshly generated and never model-supplied.
            // Seeing its already-registered Project after the first response was
            // indeterminate proves the mutation happened earlier in this same
            // work_on_project call, even though the re-observation attempt itself
            // did not write the registry. Normalize to call-level mutation truth.
            recovered.output["registered"] = json!(true);
            recovered.output["created_config"] = json!(true);
            recovered.output["changed"] = json!(true);
        }
        recovered
    }

    /// Shared implementation for both `register_project` and `create_project`.
    /// `kind` is `"register_project"` or `"create_project"`. Fields not
    /// applicable to `register_project` (template, git_init,
    /// adopt_existing_empty) are ignored by the Runner for that kind.
    #[allow(clippy::too_many_arguments)]
    async fn project_op(
        &self,
        kind: &str,
        client_id: String,
        id: String,
        name: String,
        path: String,
        description: Option<String>,
        allow_patch: bool,
        template: Option<String>,
        git_init: bool,
        adopt_existing_empty: bool,
        overwrite: bool,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        // -- basic server-side request shape validation ----------------------
        // The Runner does the authoritative path/policy validation, but the
        // server rejects obviously malformed requests early so the Runner is
        // never bothered with them.
        if let Err(e) = validate_project_op_id(&id) {
            return ToolResult::err(e);
        }
        if let Err(e) = validate_project_op_name(&name) {
            return ToolResult::err(e);
        }
        if let Some(ref desc) = description {
            if let Err(e) = validate_project_op_description(desc) {
                return ToolResult::err(e);
            }
        }
        if let Err(e) = validate_project_op_path(&path) {
            return ToolResult::err(e);
        }

        let payload = json!({
            "kind": kind,
            "client_id": client_id,
            "id": id,
            "name": name,
            "path": path,
            "description": description,
            "allow_patch": allow_patch,
            "template": template,
            "git_init": git_init,
            "adopt_existing_empty": adopt_existing_empty,
            "overwrite": overwrite,
        });
        self.submit_project_op(kind, client_id, payload, auth).await
    }

    /// Shared transport, response parsing, cache-upsert, and owner-boundary
    /// path for public project operations.
    async fn submit_project_op(
        &self,
        kind: &str,
        client_id: String,
        payload: Value,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let access = crate::runner_http::runner_access_from_auth(auth);
        // -- owner boundary + client existence --------------------------------
        let Some(client_view) = self
            .runner_registry
            .get_runner_view_for_auth(&client_id, access.as_ref())
            .await
        else {
            return ToolResult::err(format!(
                "unknown Runner '{}'. Call list_runners to discover registered client_ids.",
                client_id
            ));
        };
        let expected_runner_instance_id = client_view.runner_instance_id.clone();
        if let Err(e) = self
            .runner_registry
            .assert_runner_access(access.as_ref(), &client_id)
            .await
        {
            return ToolResult::err(e);
        }

        // -- route the already validated payload to the agent -----------------
        let payload_str = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => {
                return ToolResult::err(format!("failed to serialize project op payload: {}", e))
            }
        };
        let (request_id, rx) = match self
            .runner_registry
            .enqueue_project_op(
                client_id.clone(),
                kind,
                payload_str,
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(result) => result,
            Err(_) => {
                return ToolResult::err_with_output(
                    "agent_unavailable",
                    json!({"error_code":"agent_unavailable"}),
                )
            }
        };
        let response =
            match tokio::time::timeout(Duration::from_secs(PROJECT_OP_WAIT_SECS), rx).await {
                Ok(Ok(response)) => response,
                Ok(Err(_)) | Err(_) => {
                    self.runner_registry.cancel_request(&request_id).await;
                    return ToolResult::err_with_output(
                        "operation_indeterminate",
                        json!({"error_code":"operation_indeterminate"}),
                    );
                }
            };

        // -- parse the agent response -----------------------------------------
        // The agent returns structured JSON in stdout. On error, stdout may be
        // empty and the error is in the `error` field.
        if let Some(err) = response.error.as_ref() {
            return ToolResult::err(err.clone());
        }
        let stdout = response.stdout.as_deref().unwrap_or("");
        if stdout.is_empty() {
            return ToolResult::err("Runner returned empty project op result");
        }
        let result: Value = match serde_json::from_str::<Value>(stdout) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::err(format!(
                    "failed to parse Runner project op response: {} (stdout: {})",
                    error,
                    truncate_for_error(stdout)
                ))
            }
        };
        if response.exit_code != Some(0) {
            let code = result
                .get("error_code")
                .and_then(Value::as_str)
                .unwrap_or("operation_failed")
                .to_string();
            return ToolResult::err_with_output(code, result);
        }

        // -- commit authoritative server routing projection -------------------
        // Runner persistence already happened exactly once. The caller may see
        // ordinary success only after the authoritative summary is fenced to the
        // same Runner instance and committed into the Server routing projection.
        let Some(project) = parse_project_summary_from_result(&result, &client_id) else {
            return project_projection_reconcile_required(
                &client_id,
                &expected_runner_instance_id,
                &result,
                "authoritative_project_summary_missing",
            );
        };
        if let Err(error) = self
            .runner_registry
            .upsert_runner_project_for_instance(&client_id, &expected_runner_instance_id, project)
            .await
        {
            return project_projection_reconcile_required(
                &client_id,
                &expected_runner_instance_id,
                &result,
                if error.contains("stale or replaced") {
                    "runner_instance_changed_before_projection"
                } else {
                    "server_project_projection_failed"
                },
            );
        }

        ToolResult::ok(result)
    }
}

fn project_projection_reconcile_required(
    client_id: &str,
    runner_instance_id: &str,
    result: &Value,
    reason_code: &str,
) -> ToolResult {
    let project_id = result
        .get("agent_project_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let revision = result
        .get("revision")
        .and_then(Value::as_str)
        .map(str::to_string);
    let state_changed = result
        .get("changed")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    ToolResult::err_with_output(
        "project_projection_reconcile_required",
        json!({
            "error_code": "project_projection_reconcile_required",
            "failure_kind": "reconcile_required",
            "reason_code": reason_code,
            "state_changed": state_changed,
            "client_id": client_id,
            "agent_instance_id": runner_instance_id,
            "agent_project_id": project_id.clone(),
            "revision": revision.clone(),
            "authoritative_outcome": result.get("outcome").cloned().unwrap_or(Value::Null),
            "reconcile": {
                "action": "observe_exact_project_revision_before_retry",
                "project": project_id.map(|project_id| format!("agent:{client_id}:{project_id}")),
                "expected_revision": revision,
            }
        }),
    )
}

fn project_query_matches(needle: &str, runtime_id: &str, project: &RunnerProjectSummary) -> bool {
    [
        Some(runtime_id),
        Some(project.id.as_str()),
        project.name.as_deref(),
        project.description.as_deref(),
        Some(project.path.as_str()),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_lowercase().contains(needle))
}

fn project_source(project: &RunnerProjectSummary) -> &'static str {
    match project.registration_source.as_deref() {
        Some(AUTO_REGISTERED_PROJECT_SOURCE) => AUTO_REGISTERED_PROJECT_SOURCE,
        // A present additive provenance field is authoritative. Known explicit
        // and unknown future wire values both fail closed to ordinary registered
        // provenance so a future string cannot break the project inventory.
        Some(_) => "agent_registered",
        None if project.kind.as_deref() == Some(AUTO_REGISTERED_PROJECT_SOURCE) => {
            AUTO_REGISTERED_PROJECT_SOURCE
        }
        None => "agent_registered",
    }
}

fn project_git_available(project: &crate::runner_protocol::RunnerProjectSummary) -> Option<bool> {
    if project.git_branch.is_some() || project.git_head.is_some() || project.git_dirty.is_some() {
        Some(true)
    } else {
        Some(false)
    }
}

fn smoke_marker_present(project: &crate::runner_protocol::RunnerProjectSummary) -> bool {
    let name = project.name.as_deref().unwrap_or_default();
    [project.id.as_str(), name, project.path.as_str()]
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .any(|value| value.contains("smoke") || value.contains("test") || value.contains("sandbox"))
}

fn smoke_project_capabilities(
    client: &RunnerSemanticView,
    project: &crate::runner_protocol::RunnerProjectSummary,
) -> Value {
    let git_available = project_git_available(project);
    let safe_smoke_project =
        project.allow_patch && client.view.connected && smoke_marker_present(project);
    let supports_artifact_smoke =
        client.supports(RunnerFeature::FileRead) && client.supports(RunnerFeature::FileWrite);
    let supports_cleanup_verification =
        supports_artifact_smoke || git_available.is_some_and(|available| available);
    let recommended_for_smoke = safe_smoke_project
        && git_available.is_some_and(|available| available)
        && supports_cleanup_verification;

    json!({
        "git_available": git_available,
        "safe_smoke_project": safe_smoke_project,
        "supports_artifact_smoke": supports_artifact_smoke,
        "supports_cleanup_verification": supports_cleanup_verification,
        "recommended_for_smoke": recommended_for_smoke,
    })
}

/// Resolve which shell profile a project uses and whether it is configured.
/// Returns `(resolved_name, status)` where:
/// - `resolved_name` = `project_shell_profile` (if set) else the Runner's
///   `default_profile` (if any) else `None`.
/// - `status` = `"configured"` if the resolved name exists in the Runner's
///   configured profiles; `"missing"` if a name resolved but is not
///   configured; `"not_configured"` if no profile resolves at all; and
///   `"unknown"` if the Runner did not report a shell-profiles summary so the
///   configured set cannot be checked.
fn resolve_project_shell_profile(
    project_shell_profile: Option<&str>,
    summary: Option<&crate::runner_protocol::ShellProfilesSummary>,
) -> (Option<String>, &'static str) {
    let resolved = project_shell_profile
        .map(str::to_string)
        .or_else(|| summary.and_then(|s| s.default_profile.clone()));
    match resolved {
        None => (None, "not_configured"),
        Some(name) => match summary {
            None => (Some(name), "unknown"),
            Some(s) => {
                if s.profiles.iter().any(|p| p.name == name) {
                    (Some(name), "configured")
                } else {
                    (Some(name), "missing")
                }
            }
        },
    }
}

// =============================================================================
// Server-side request-shape validation helpers
// =============================================================================

/// Validate the project `id` field server-side. The Runner does the
/// authoritative validation, but this rejects obviously malformed ids early.
/// Rules: non-empty, <= 64 chars, ASCII letters/digits/dash/underscore only,
/// no slash, no backslash, no dot-dot, no NUL.
fn validate_project_op_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id cannot be empty".to_string());
    }
    if id.contains('\0') {
        return Err("id must not contain NUL".to_string());
    }
    if id.len() > 64 {
        return Err("id must be at most 64 characters".to_string());
    }
    if id.contains('/') || id.contains('\\') {
        return Err("id must not contain slash or backslash".to_string());
    }
    if id == ".." || id == "." {
        return Err("id cannot be '.' or '..'".to_string());
    }
    if id.contains("..") {
        return Err("id must not contain dot-dot traversal".to_string());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("id may only contain ASCII letters, digits, '-', and '_'".to_string());
    }
    Ok(())
}

/// Validate the project `name` field server-side: non-empty after trim, <= 120
/// UTF-8 bytes, no NUL.
fn validate_project_op_name(name: &str) -> Result<(), String> {
    if name.contains('\0') {
        return Err("name must not contain NUL".to_string());
    }
    if name.trim().is_empty() {
        return Err("name cannot be empty".to_string());
    }
    if name.len() > 120 {
        return Err("name must be at most 120 UTF-8 bytes".to_string());
    }
    Ok(())
}

/// Validate the optional `description` field: <= 500 UTF-8 bytes, no NUL.
fn validate_project_op_description(desc: &str) -> Result<(), String> {
    if desc.contains('\0') {
        return Err("description must not contain NUL".to_string());
    }
    if desc.len() > 500 {
        return Err("description must be at most 500 UTF-8 bytes".to_string());
    }
    Ok(())
}

pub(super) use webcodex_core::runtime_contract::validate_project_op_path;

/// Truncate a string for inclusion in an error message (bounded).
fn truncate_for_error(s: &str) -> String {
    const MAX_BYTES: usize = 200;
    if s.len() <= MAX_BYTES {
        s.to_string()
    } else {
        let mut end = MAX_BYTES;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Parse a `RunnerProjectSummary` from the Runner's project-op JSON
/// response so the server can upsert it into the cached project list. The
/// response includes `agent_project_id`, `client_id`, `name`, `path`, and
/// `allow_patch` — enough to build a summary that `listProjects` can show
/// immediately.
fn parse_project_summary_from_result(
    result: &Value,
    _client_id: &str,
) -> Option<RunnerProjectSummary> {
    let agent_project_id = result.get("agent_project_id")?.as_str()?;
    let name = result
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let path = result.get("path")?.as_str()?;
    let allow_patch = result
        .get("allow_patch")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    Some(RunnerProjectSummary {
        id: agent_project_id.to_string(),
        name: name.or_else(|| Some(agent_project_id.to_string())),
        path: path.to_string(),
        allow_patch,
        kind: result
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_string),
        registration_source: result
            .get("registration_source")
            .and_then(Value::as_str)
            .map(str::to_string),
        description: result
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        hooks: Vec::new(),
        disabled: result
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        revision: result
            .get("revision")
            .and_then(Value::as_str)
            .map(str::to_string),
        git_branch: None,
        git_head: None,
        git_dirty: None,
        updated_at: chrono::Utc::now().timestamp(),
        shell_profile: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_managed_temporary_kind_is_projected_as_ordinary_registration() {
        let project = RunnerProjectSummary {
            id: "legacy".to_string(),
            name: Some("Legacy".to_string()),
            path: "/tmp/legacy".to_string(),
            allow_patch: true,
            kind: Some("managed_temporary".to_string()),
            registration_source: None,
            description: None,
            hooks: Vec::new(),
            disabled: false,
            revision: None,
            git_branch: None,
            git_head: None,
            git_dirty: None,
            updated_at: 0,
            shell_profile: None,
        };
        assert_eq!(project_source(&project), "agent_registered");
    }

    #[test]
    fn old_runner_auto_registered_kind_is_compatibility_fallback() {
        let mut project = RunnerProjectSummary {
            id: "legacy-auto".to_string(),
            name: Some("Legacy Auto".to_string()),
            path: "/tmp/legacy-auto".to_string(),
            allow_patch: true,
            kind: Some(AUTO_REGISTERED_PROJECT_SOURCE.to_string()),
            registration_source: None,
            description: None,
            hooks: Vec::new(),
            disabled: false,
            revision: None,
            git_branch: None,
            git_head: None,
            git_dirty: None,
            updated_at: 0,
            shell_profile: None,
        };
        assert_eq!(project_source(&project), AUTO_REGISTERED_PROJECT_SOURCE);
        project.registration_source = Some("explicit".to_string());
        assert_eq!(
            project_source(&project),
            "agent_registered",
            "new provenance must override the legacy kind sentinel"
        );
        project.registration_source = Some("future_registration_source".to_string());
        assert_eq!(project_source(&project), "agent_registered");
    }

    #[test]
    fn validate_id_rejects_empty() {
        assert!(validate_project_op_id("").is_err());
    }

    #[test]
    fn validate_id_rejects_nul() {
        assert!(validate_project_op_id("a\0b").is_err());
    }

    #[test]
    fn validate_id_rejects_slash() {
        assert!(validate_project_op_id("a/b").is_err());
    }

    #[test]
    fn validate_id_rejects_backslash() {
        assert!(validate_project_op_id("a\\b").is_err());
    }

    #[test]
    fn validate_id_rejects_dot_dot() {
        assert!(validate_project_op_id("..").is_err());
        assert!(validate_project_op_id("a..b").is_err());
    }

    #[test]
    fn validate_id_rejects_long() {
        let id = "a".repeat(65);
        assert!(validate_project_op_id(&id).is_err());
    }

    #[test]
    fn validate_id_accepts_valid() {
        assert!(validate_project_op_id("my-project").is_ok());
        assert!(validate_project_op_id("hello_123").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty_after_trim() {
        assert!(validate_project_op_name("   ").is_err());
    }

    #[test]
    fn validate_name_rejects_nul() {
        assert!(validate_project_op_name("a\0b").is_err());
    }

    #[test]
    fn validate_path_rejects_relative() {
        assert!(validate_project_op_path("relative/path").is_err());
    }

    #[test]
    fn validate_path_rejects_empty() {
        assert!(validate_project_op_path("").is_err());
    }

    #[test]
    fn validate_path_rejects_nul() {
        assert!(validate_project_op_path("/root/\0").is_err());
    }

    #[test]
    fn validate_path_accepts_cross_platform_absolute_shapes() {
        assert!(validate_project_op_path("/root/git/my-project").is_ok());
        assert!(validate_project_op_path(r"C:\repo").is_ok());
        assert!(validate_project_op_path("c:/repo").is_ok());
        assert!(validate_project_op_path(r"\\?\C:\repo").is_ok());
        assert!(validate_project_op_path(r"\\server\share\repo").is_ok());
        assert!(validate_project_op_path(r"C:repo").is_err());
        assert!(validate_project_op_path(r"\repo").is_err());
        assert!(validate_project_op_path(r"relative\repo").is_err());
    }

    #[test]
    fn validation_bounds_are_utf8_bytes() {
        let name_error = validate_project_op_name(&"界".repeat(41)).unwrap_err();
        assert!(name_error.contains("120 UTF-8 bytes"));

        let description_error = validate_project_op_description(&"界".repeat(167)).unwrap_err();
        assert!(description_error.contains("500 UTF-8 bytes"));
    }

    #[test]
    fn truncate_for_error_never_splits_utf8() {
        let input = "界".repeat(67);
        let truncated = truncate_for_error(&input);
        assert!(truncated.ends_with('…'));
        assert_eq!(truncated.trim_end_matches('…').as_bytes().len(), 198);
    }

    #[test]
    fn parse_summary_extracts_fields() {
        let result = json!({
            "agent_project_id": "my-project",
            "client_id": "oe",
            "name": "My Project",
            "path": "/root/git/my-project",
            "allow_patch": true,
            "description": "desc",
        });
        let summary = parse_project_summary_from_result(&result, "oe").unwrap();
        assert_eq!(summary.id, "my-project");
        assert_eq!(summary.name.as_deref(), Some("My Project"));
        assert_eq!(summary.path, "/root/git/my-project");
        assert!(summary.allow_patch);
        assert!(!summary.disabled);
    }

    #[test]
    fn parse_summary_defaults_name_to_id() {
        let result = json!({
            "agent_project_id": "hello",
            "client_id": "oe",
            "path": "/root/git/hello",
        });
        let summary = parse_project_summary_from_result(&result, "oe").unwrap();
        assert_eq!(summary.name.as_deref(), Some("hello"));
    }

    #[test]
    fn validate_id_rejects_single_dot() {
        assert!(validate_project_op_id(".").is_err());
    }

    #[test]
    fn validate_id_rejects_non_alphanumeric() {
        assert!(validate_project_op_id("a!b").is_err());
        assert!(validate_project_op_id("a b").is_err());
        assert!(validate_project_op_id("a.b").is_err());
    }

    #[test]
    fn validate_description_rejects_nul() {
        assert!(validate_project_op_description("a\0b").is_err());
    }

    #[test]
    fn validate_description_rejects_long() {
        let desc = "a".repeat(501);
        assert!(validate_project_op_description(&desc).is_err());
    }

    #[test]
    fn validate_description_accepts_none() {
        // None/empty description is valid.
        assert!(validate_project_op_description("").is_ok());
    }

    #[test]
    fn validate_name_rejects_long() {
        let name = "a".repeat(121);
        assert!(validate_project_op_name(&name).is_err());
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_project_op_name("My Project").is_ok());
        assert!(validate_project_op_name("A").is_ok());
    }
}
