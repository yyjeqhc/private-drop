use super::{parse_json_body, parse_optional_json_body, render_result, require_runtime};
use crate::action_audit::ActionAudit;
use crate::tool_runtime::ToolCall;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::Value;

/// `POST /api/projects/register` — thin REST wrapper over
/// `ToolCall::RegisterProject`. Mutation with side effects; registers an
/// existing directory as a WebCodex project on the selected Runner. Dedicated
/// GPT Action (`registerProject`); also reachable via callRuntimeTool / MCP
/// tools/call.
#[derive(Debug, Deserialize)]
struct RegisterProjectRequest {
    pub client_id: String,
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "crate::tool_runtime::default_true")]
    pub allow_patch: bool,
    #[serde(default)]
    pub overwrite: bool,
}

/// `POST /api/projects/create` — thin REST wrapper over
/// `ToolCall::CreateProject`. Mutation with side effects; creates a new directory
/// or explicitly adopts an existing empty directory on the selected Runner, then
/// registers it as a WebCodex project.
/// Dedicated GPT Action (`createProject`); also reachable via callRuntimeTool
/// / MCP tools/call.
#[derive(Debug, Deserialize)]
struct CreateProjectRequest {
    pub client_id: String,
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "crate::tool_runtime::default_true")]
    pub allow_patch: bool,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub git_init: bool,
    #[serde(default)]
    pub adopt_existing_empty: bool,
    #[serde(default)]
    pub overwrite: bool,
    /// Narrow pre-0.4 ingress sentinels. This dedicated endpoint historically
    /// ignores unrelated unknown fields, so naming only retired semantic fields
    /// keeps that behavior without introducing a generic compatibility-field bag.
    #[serde(default, rename = "managed_temporary_project")]
    pub retired_managed_temporary_project: Option<Value>,
    #[serde(default, rename = "allow_existing_empty")]
    pub retired_allow_existing_empty: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnregisterProjectRequest {
    project: String,
    expected_revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveOrRegisterProjectRequest {
    client_id: String,
    path: String,
}

#[handler]
pub async fn projects_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/list", "listProjects");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    // Body remains optional for compatibility. Non-empty malformed JSON must
    // fail closed instead of silently widening a targeted request to the full
    // caller-visible registry.
    let Some(arguments) = parse_optional_json_body(req, res).await else {
        return;
    };
    let call = match ToolCall::from_tool_name("list_projects", arguments) {
        Ok(call) => call,
        Err(error) => {
            render_result(
                res,
                &audit,
                "list_projects",
                None,
                crate::tool_runtime::ToolResult::err(error),
            );
            return;
        }
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime.dispatch_with_auth(call, auth.as_ref()).await;
    render_result(res, &audit, "list_projects", None, result);
}

/// `ToolCall::RegisterProject`. Registers an existing directory as a
/// WebCodex project on the selected Runner. Mutation with side effects; executes
/// on the selected Runner and is constrained by Runner policy.
#[handler]
pub async fn projects_register(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/register", "registerProject");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<RegisterProjectRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RegisterProject {
                client_id: body.client_id,
                id: body.id,
                name: body.name,
                path: body.path,
                description: body.description,
                allow_patch: body.allow_patch,
                overwrite: body.overwrite,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "register_project", None, result);
}

/// `ToolCall::CreateProject`. Creates a new directory or explicitly adopts an
/// existing empty directory on the selected Runner, then registers it as a
/// WebCodex project. Mutation with side effects; executes on the selected Runner
/// and is constrained by Runner policy.
#[handler]
pub async fn projects_create(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/create", "createProject");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<CreateProjectRequest>(req, res).await else {
        return;
    };
    if body.retired_managed_temporary_project.is_some() {
        render_result(
            res,
            &audit,
            "create_project",
            None,
            crate::tool_runtime::ToolResult::err(
                "invalid arguments for create_project: field 'managed_temporary_project' is no longer supported; use ordinary explicit project creation",
            ),
        );
        return;
    }
    if body.retired_allow_existing_empty.is_some() {
        render_result(
            res,
            &audit,
            "create_project",
            None,
            crate::tool_runtime::ToolResult::err(
                "invalid arguments for create_project: field 'allow_existing_empty' is no longer supported; use 'adopt_existing_empty' to explicitly adopt an existing empty directory",
            ),
        );
        return;
    }
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::CreateProject {
                client_id: body.client_id,
                id: body.id,
                name: body.name,
                path: body.path,
                description: body.description,
                allow_patch: body.allow_patch,
                template: body.template,
                git_init: body.git_init,
                adopt_existing_empty: body.adopt_existing_empty,
                overwrite: body.overwrite,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "create_project", None, result);
}

/// `POST /api/projects/resolve-or-register` — hidden operator path bootstrap.
/// The request carries only an exact Runner identity and path, then delegates to
/// the same ModelHidden ToolRuntime convergence primitive used by workflow
/// bootstrap. The Server never writes Runner project TOML here.
#[handler]
pub async fn projects_resolve_or_register(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/projects/resolve-or-register",
        "resolveOrRegisterProject",
    );
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<ResolveOrRegisterProjectRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .resolve_or_register_project(body.client_id, body.path, auth.as_ref())
        .await;
    render_result(res, &audit, "resolve_or_register_project", None, result);
}

/// `POST /api/projects/unregister` — narrow ordinary authenticated project
/// lifecycle endpoint. It exposes only exact unregister and reuses the same
/// Runner capability, active-job fencing, and Server inventory update as the
/// admin lifecycle surface.
#[handler]
pub async fn projects_unregister(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/unregister", "unregisterProject");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<UnregisterProjectRequest>(req, res).await else {
        return;
    };
    let Some(auth) = depot.obtain::<crate::auth::AuthContext>().ok().cloned() else {
        res.status_code(StatusCode::UNAUTHORIZED);
        res.render(crate::json_error(StatusCode::UNAUTHORIZED, "Unauthorized"));
        return;
    };
    if auth.is_agent_token()
        || auth.is_open_anonymous()
        || !auth.has_scope(crate::auth::scopes::SCOPE_PROJECT_WRITE)
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(crate::json_error(StatusCode::FORBIDDEN, "Forbidden"));
        return;
    }
    let Some(db) = crate::get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database not configured",
        ));
        return;
    };
    let service = crate::admin_project_lifecycle::AdminProjectLifecycleService::new(runtime, db);
    let response = service
        .unregister_authorized(&auth, &body.project, &body.expected_revision)
        .await;
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let success = status.is_success();
    let error = response
        .body
        .pointer("/error/code")
        .and_then(Value::as_str)
        .map(str::to_string);
    audit.record(
        crate::action_audit::ActionAuditRecord::new("unregister_project", success, status)
            .error(error)
            .ids(serde_json::json!({"project": body.project})),
    );
    res.status_code(status);
    res.render(Json(response.body));
}
