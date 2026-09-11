use crate::auth::AuthContext;
use crate::connector_runtime::http::{render, runtime};
use crate::connector_runtime::workspace::LocalResultDecision;
use crate::connector_runtime::{
    approval_projection, result_projection, store_error_outcome, validate_opaque_id,
    ConnectorCallOutcome, ConnectorRuntime, TaskCancelInput, TaskReviewInput,
};
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(crate) fn routes() -> Router {
    use crate::route_metadata::{api_path, RouteId};
    Router::new()
        .push(Router::with_path(api_path(RouteId::HostConsoleReadiness)).post(readiness))
        .push(Router::with_path(api_path(RouteId::HostConsoleTasks)).post(tasks))
        .push(Router::with_path(api_path(RouteId::HostConsoleActivity)).post(activity))
        .push(
            Router::with_path(api_path(RouteId::HostConsoleWorkflowSessions))
                .post(workflow_sessions),
        )
        .push(
            Router::with_path(api_path(RouteId::HostConsoleWorkflowSession)).post(workflow_session),
        )
        .push(Router::with_path(api_path(RouteId::HostConsoleTaskReview)).post(task_review))
        .push(Router::with_path(api_path(RouteId::HostConsoleTaskCancel)).post(task_cancel))
        .push(Router::with_path(api_path(RouteId::HostConsoleTaskGuide)).post(task_guide))
        .push(Router::with_path(api_path(RouteId::HostConsoleApprovals)).post(approvals))
        .push(Router::with_path(api_path(RouteId::HostConsoleApprovalDecide)).post(approval_decide))
        .push(Router::with_path(api_path(RouteId::HostConsoleDevices)).post(devices))
        .push(Router::with_path(api_path(RouteId::HostConsoleResultAccept)).post(result_accept))
        .push(Router::with_path(api_path(RouteId::HostConsoleResultReject)).post(result_reject))
        .push(Router::with_path(api_path(RouteId::HostConsoleConnect)).post(connect))
}

fn failure(status: u16, code: &str, message: impl Into<String>) -> ConnectorCallOutcome {
    ConnectorCallOutcome::error(
        status,
        code,
        message,
        false,
        true,
        Some("Correct the request or refresh the current task review."),
        None,
        false,
    )
}

fn prepared(
    req: &Request,
    depot: &Depot,
) -> Result<(Arc<ConnectorRuntime>, AuthContext), ConnectorCallOutcome> {
    crate::auth::require_json_same_origin(req)
        .map_err(|(status, code, message)| failure(status, code, message))?;
    let runtime = runtime(depot).ok_or_else(|| {
        failure(
            404,
            "connector_surface_disabled",
            "this project has not been configured",
        )
    })?;
    let auth = depot
        .obtain::<AuthContext>()
        .cloned()
        .map_err(|_| failure(401, "unauthorized", "authentication required"))?;
    Ok((runtime, auth))
}

macro_rules! prepare {
    ($req:expr, $depot:expr, $res:expr) => {
        match prepared($req, $depot) {
            Ok(context) => context,
            Err(outcome) => return render($res, outcome),
        }
    };
}

fn invalid(res: &mut Response, message: impl Into<String>) {
    render(res, failure(400, "invalid_arguments", message));
}

macro_rules! parse {
    ($ty:ty, $req:expr, $res:expr) => {
        match $req.parse_json::<$ty>().await {
            Ok(input) => input,
            Err(error) => return invalid($res, format!("invalid request: {error}")),
        }
    };
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {
    #[serde(default)]
    include_completed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityInput {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    client: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowSessionsInput {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowSessionInput {
    session_id: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideInput {
    task_id: String,
    result_id: Option<String>,
    /// Reject only: delivered to the model as guidance on its next
    /// capability call, exactly like `task guide`.
    reason: Option<String>,
}

#[handler]
async fn readiness(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, auth) = prepare!(req, depot, res);
    match runtime.readiness(&auth).await {
        Some(report) => res.render(Json(report)),
        None => render(res, failure(401, "unauthorized", "authentication required")),
    }
}

#[handler]
async fn tasks(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, _) = prepare!(req, depot, res);
    let input = parse!(ListInput, req, res);
    match runtime.db.local_reviewable_tasks(
        &runtime.context().project_id,
        input.include_completed,
        20,
    ) {
        Ok(rows) => res.render(Json(json!({ "tasks": rows }))),
        Err(error) => render(res, store_error_outcome(error, None)),
    }
}

#[handler]
async fn activity(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, auth) = prepare!(req, depot, res);
    let input = parse!(ActivityInput, req, res);
    let limit = input.limit.unwrap_or(50).clamp(1, 200);
    let client = input
        .client
        .as_deref()
        .map(str::trim)
        .filter(|client| !client.is_empty());
    // Scoped to the agents this caller can already see in the devices panel.
    // A project credential must not read another project's command previews,
    // paths, or errors out of the shared database.
    let (visibility, allowed) = runtime.activity_visibility(&auth).await;
    match runtime
        .db
        .list_workspace_activity_for_clients(limit, client, visibility, &allowed)
    {
        Ok(rows) => res.render(Json(json!({ "activity": rows }))),
        Err(error) => render(res, failure(500, "activity_store_error", error.to_string())),
    }
}

#[handler]
async fn workflow_sessions(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, _) = prepare!(req, depot, res);
    let input = parse!(WorkflowSessionsInput, req, res);
    res.render(Json(runtime.workflow_sessions_console_list(input.limit)));
}

#[handler]
async fn workflow_session(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, _) = prepare!(req, depot, res);
    let input = parse!(WorkflowSessionInput, req, res);
    if validate_opaque_id(&input.session_id, "wc_sess_", "session_id").is_err() {
        return invalid(res, "invalid workflow session input");
    }
    match runtime.workflow_session_console_detail(&input.session_id, input.limit) {
        Some(detail) => res.render(Json(detail)),
        None => render(
            res,
            failure(
                404,
                "workflow_session_not_found",
                "workflow session not found",
            ),
        ),
    }
}

#[handler]
async fn task_review(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, auth) = prepare!(req, depot, res);
    let input = parse!(TaskReviewInput, req, res);
    if validate_opaque_id(&input.task_id, "wc_task_", "task_id").is_err()
        || input.max_events.is_some()
    {
        return invalid(res, "invalid review input");
    }
    render(res, runtime.host_review(&auth, input).await);
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuideInput {
    task_id: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalDecideInput {
    task_id: String,
    approval_id: String,
    approve: bool,
    #[serde(default)]
    reason: Option<String>,
}

#[handler]
async fn approvals(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, _) = prepare!(req, depot, res);
    match runtime.db.local_pending_connector_approvals(
        &runtime.context().project_id,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(rows) => {
            let pending: Vec<serde_json::Value> = rows
                .iter()
                .map(|(approval, goal)| {
                    let mut projection = approval_projection(approval);
                    projection["task_id"] = json!(approval.task_id);
                    projection["goal"] = json!(goal);
                    projection
                })
                .collect();
            res.render(Json(json!({ "approvals": pending })));
        }
        Err(error) => render(res, store_error_outcome(error, None)),
    }
}

#[handler]
async fn devices(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, auth) = prepare!(req, depot, res);
    let result = runtime.host_devices(&auth).await;
    if result.success {
        res.render(Json(result.output));
    } else {
        render(
            res,
            failure(
                500,
                "devices_unavailable",
                result
                    .error
                    .unwrap_or_else(|| "devices view failed".to_string()),
            ),
        );
    }
}

#[handler]
async fn approval_decide(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, _) = prepare!(req, depot, res);
    let input = parse!(ApprovalDecideInput, req, res);
    let reason = input
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    if validate_opaque_id(&input.task_id, "wc_task_", "task_id").is_err()
        || validate_opaque_id(&input.approval_id, "wc_apr_", "approval_id").is_err()
        || reason.is_some_and(|reason| reason.len() > 500)
    {
        return invalid(res, "invalid approval decision input");
    }
    match runtime.db.decide_connector_approval(
        &input.task_id,
        &runtime.context().project_id,
        &input.approval_id,
        input.approve,
        "host_console",
        reason,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(approval) => res.render(Json(json!({
            "decision": approval.state,
            "approval": approval_projection(&approval)
        }))),
        Err(error) => render(res, store_error_outcome(error, None)),
    }
}

#[handler]
async fn task_guide(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, _) = prepare!(req, depot, res);
    let input = parse!(GuideInput, req, res);
    let message = input.message.trim();
    if validate_opaque_id(&input.task_id, "wc_task_", "task_id").is_err()
        || message.is_empty()
        || message.len() > 2000
    {
        return invalid(res, "guidance message must be 1..=2000 bytes");
    }
    render(res, runtime.host_guide(&input.task_id, message));
}

#[handler]
async fn task_cancel(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, auth) = prepare!(req, depot, res);
    let input = parse!(TaskCancelInput, req, res);
    if validate_opaque_id(&input.task_id, "wc_task_", "task_id").is_err() {
        return invalid(res, "invalid cancel input");
    }
    render(res, runtime.host_cancel(&auth, input).await);
}

async fn decide(req: &mut Request, depot: &Depot, res: &mut Response, accept: bool) {
    let (runtime, _) = prepare!(req, depot, res);
    let input = parse!(DecideInput, req, res);
    let result_valid = input
        .result_id
        .as_deref()
        .is_none_or(|id| validate_opaque_id(id, "wc_result_", "result_id").is_ok());
    let reason = input
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    if validate_opaque_id(&input.task_id, "wc_task_", "task_id").is_err()
        || !result_valid
        || (accept && input.result_id.is_none())
        || reason.is_some_and(|reason| reason.len() > 500)
        || (accept && reason.is_some())
        || (!accept && input.result_id.is_none() && reason.is_some())
    {
        return invalid(res, "invalid decision input");
    }
    let decision = if accept {
        LocalResultDecision::Accept
    } else {
        LocalResultDecision::Reject
    };
    let result = runtime.host_decide(
        &input.task_id,
        input.result_id.as_deref(),
        decision,
        reason,
        chrono::Utc::now().timestamp(),
    );
    match result {
        Ok(result) => res.render(Json(json!({
            "decision": result.decision_status,
            "result": result_projection(&result)
        }))),
        Err(error) => render(res, store_error_outcome(error, None)),
    }
}

/// Non-secret connection targets for the Connect panel. The page composes
/// absolute URLs from its own origin unless a public URL is configured, and
/// credential material never appears in this projection by construction.
#[handler]
async fn connect(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (runtime, _) = prepare!(req, depot, res);
    let public_url = std::env::var("WEBCODEX_PUBLIC_URL")
        .ok()
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty());
    let context = runtime.context();
    res.render(Json(json!({
        "project": context.project_name,
        "profile": context.profile,
        "public_url": public_url,
        "mcp_path": "/mcp",
        "actions_schema_path": "/openapi.json",
        "surface": {
            "mode": "project_bound",
            "capability_count": crate::connector_runtime::surface::CAPABILITY_NAMES.len(),
            "operator_runtime_exposed": false,
        },
        "operator_runtime": {
            "available": true,
            "model_default": false,
            "purpose": "management_development_internal_execution",
            "tool_count": crate::tool_runtime::registered_tool_specs().len(),
            "access": "operator_authenticated_host_runtime",
        },
        "oauth_discovery_path": "/.well-known/oauth-authorization-server"
    })));
}

#[handler]
async fn result_accept(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    decide(req, depot, res, true).await;
}

#[handler]
async fn result_reject(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    decide(req, depot, res, false).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_runtime::activity::{ActivityRecord, ActivityScope};
    use salvo::test::{ResponseExt, TestClient};

    /// The console router behind the same depot the auth middleware fills in:
    /// the handler can only learn who is calling from there, never from the
    /// request body or query.
    fn service(runtime: Arc<ConnectorRuntime>, auth: AuthContext) -> Service {
        Service::new(
            Router::new()
                .hoop(
                    salvo::affix_state::inject(crate::connector_runtime::ConnectorRuntimeSlot(
                        Some(runtime),
                    ))
                    .inject(auth),
                )
                .push(routes()),
        )
    }

    async fn activity_as(
        runtime: &Arc<ConnectorRuntime>,
        auth: &AuthContext,
        body: serde_json::Value,
    ) -> serde_json::Value {
        let service = service(runtime.clone(), auth.clone());
        let mut response = TestClient::post("http://127.0.0.1/console/activity")
            .add_header("host", "127.0.0.1", true)
            .add_header("origin", "http://127.0.0.1", true)
            .add_header("content-type", "application/json", true)
            .json(&body)
            .send(&service)
            .await;
        let body: serde_json::Value = response.take_json().await.unwrap();
        // An error body would trivially satisfy "does not contain the other
        // grant's data", so require a real answer before asserting on it.
        assert!(
            body["activity"].is_array(),
            "expected an activity list, got {body}"
        );
        body
    }

    fn record<'a>(client: &'a str, grant: &str) -> ActivityRecord<'a> {
        ActivityRecord {
            tool: "run_shell",
            project: Some("agent:laptop:demo"),
            surface: "mcp",
            client: Some(client),
            success: true,
            session_id: Some("wc_sess_grant_a_session"),
            command: None,
            paths: vec!["private/a.rs".to_string()],
            error_summary: Some("grant-a-error"),
            scope: ActivityScope::ProjectGrant(grant.to_string()),
        }
    }

    #[tokio::test]
    async fn console_activity_isolates_grants_that_share_a_client_id() {
        let fixture = crate::connector_runtime::execution_tests::console_fixture().await;
        let grant_a = crate::connector_runtime::tests::auth("u1");
        let grant_b = crate::connector_runtime::tests::auth("u2");
        assert_ne!(grant_a.project_grant_id, grant_b.project_grant_id);

        fixture
            .runtime
            .db
            .insert_workspace_activity(
                1,
                &record(
                    &fixture.shared_client_id,
                    grant_a.project_grant_id.as_deref().unwrap(),
                ),
                Some("grant-a-command"),
                50,
            )
            .unwrap();

        // Grant B holds a live `laptop` client, but never wrote this row.
        let denied = activity_as(&fixture.runtime, &grant_b, serde_json::json!({})).await;
        let serialized = serde_json::to_string(&denied).unwrap();
        for secret in [
            "grant-a-command",
            "private/a.rs",
            "grant-a-error",
            "wc_sess_grant_a_session",
        ] {
            assert!(
                !serialized.contains(secret),
                "{secret} leaked: {serialized}"
            );
        }

        // Naming the client explicitly does not get around the grant scope.
        let named = activity_as(
            &fixture.runtime,
            &grant_b,
            serde_json::json!({ "client": fixture.shared_client_id }),
        )
        .await;
        assert!(
            !serde_json::to_string(&named)
                .unwrap()
                .contains("grant-a-command"),
            "{named}"
        );
    }

    #[tokio::test]
    async fn console_activity_scope_is_not_taken_from_the_request() {
        let fixture = crate::connector_runtime::execution_tests::console_fixture().await;
        let grant_a = crate::connector_runtime::tests::auth("u1");
        let grant_b = crate::connector_runtime::tests::auth("u2");
        fixture
            .runtime
            .db
            .insert_workspace_activity(
                1,
                &record(
                    &fixture.shared_client_id,
                    grant_a.project_grant_id.as_deref().unwrap(),
                ),
                Some("grant-a-command"),
                50,
            )
            .unwrap();

        // The request has no field that could carry attribution: the input
        // struct denies unknown fields outright, so an attempt to name a grant
        // is rejected before any lookup.
        let service = service(fixture.runtime.clone(), grant_b.clone());
        let mut response = TestClient::post("http://127.0.0.1/console/activity")
            .add_header("host", "127.0.0.1", true)
            .add_header("origin", "http://127.0.0.1", true)
            .add_header("content-type", "application/json", true)
            .json(&serde_json::json!({
                "scope_id": grant_a.project_grant_id,
                "client": fixture.shared_client_id
            }))
            .send(&service)
            .await;
        let spoofed: serde_json::Value = response.take_json().await.unwrap();
        assert_eq!(spoofed["error"]["code"], "invalid_arguments", "{spoofed}");

        // And a well-formed request from grant B still cannot reach the row,
        // because the scope comes from the depot's AuthContext.
        let honest = activity_as(
            &fixture.runtime,
            &grant_b,
            serde_json::json!({ "client": fixture.shared_client_id }),
        )
        .await;
        assert!(
            honest["activity"].as_array().unwrap().is_empty(),
            "{honest}"
        );
        assert!(!serde_json::to_string(&honest)
            .unwrap()
            .contains("grant-a-command"));
    }

    #[tokio::test]
    async fn console_activity_shows_a_grant_its_own_rows() {
        let fixture = crate::connector_runtime::execution_tests::console_fixture().await;
        let grant_a = crate::connector_runtime::tests::auth("u1");
        fixture
            .runtime
            .db
            .insert_workspace_activity(
                1,
                &record(
                    &fixture.own_client_id,
                    grant_a.project_grant_id.as_deref().unwrap(),
                ),
                Some("grant-a-command"),
                50,
            )
            .unwrap();

        let mine = activity_as(&fixture.runtime, &grant_a, serde_json::json!({})).await;
        let rows = mine["activity"].as_array().expect("activity list");
        assert_eq!(rows.len(), 1, "{mine}");
        assert_eq!(rows[0]["command_preview"], "grant-a-command");
    }

    #[tokio::test]
    async fn workflow_session_console_is_project_scoped_and_request_cannot_choose_project() {
        let fixture = crate::connector_runtime::execution_tests::console_fixture().await;
        let auth = crate::connector_runtime::tests::auth("u1");
        let project = fixture.runtime.context().project_id.clone();
        let sessions = &fixture.runtime.tool_runtime_for_test().sessions;
        let visible =
            sessions.start_session(Some(project.clone()), Some("visible workflow".to_string()));
        let hidden = sessions.start_session(
            Some("agent:elsewhere:hidden".to_string()),
            Some("HIDDEN_SECRET_TITLE".to_string()),
        );
        let start = sessions.record_tool_call_started(
            Some(&visible.session_id),
            crate::tool_runtime::sessions::SessionTransport::Api,
            "read_files",
            &serde_json::json!({
                "project": project,
                "items": [{"path": "src/lib.rs"}],
                "authorization": "Bearer SHOULD_NOT_LEAK"
            }),
            crate::tool_runtime::sessions::session_tool_contract("read_files"),
        );
        sessions.record_tool_call_finished(
            start,
            true,
            &serde_json::json!({"content": "PRIVATE_FILE_CONTENT"}),
            None,
            None,
        );

        let service = service(fixture.runtime.clone(), auth.clone());
        let mut list_response = TestClient::post("http://127.0.0.1/console/workflow-sessions")
            .add_header("host", "127.0.0.1", true)
            .add_header("origin", "http://127.0.0.1", true)
            .add_header("content-type", "application/json", true)
            .json(&serde_json::json!({"limit": 20}))
            .send(&service)
            .await;
        let list: serde_json::Value = list_response.take_json().await.unwrap();
        let serialized = serde_json::to_string(&list).unwrap();
        assert!(serialized.contains(&visible.session_id), "{list}");
        assert!(!serialized.contains(&hidden.session_id), "{list}");
        assert!(!serialized.contains("HIDDEN_SECRET_TITLE"), "{list}");

        let mut detail_response = TestClient::post("http://127.0.0.1/console/workflow-session")
            .add_header("host", "127.0.0.1", true)
            .add_header("origin", "http://127.0.0.1", true)
            .add_header("content-type", "application/json", true)
            .json(&serde_json::json!({"session_id": visible.session_id, "limit": 100}))
            .send(&service)
            .await;
        let detail: serde_json::Value = detail_response.take_json().await.unwrap();
        assert_eq!(detail["activity"][0]["kind"], "Read", "{detail}");
        let detail_text = serde_json::to_string(&detail).unwrap();
        for secret in ["SHOULD_NOT_LEAK", "PRIVATE_FILE_CONTENT", "authorization"] {
            assert!(
                !detail_text.contains(secret),
                "{secret} leaked: {detail_text}"
            );
        }

        for session_id in [
            &hidden.session_id,
            "wc_sess_unknown000000000000000000000000",
        ] {
            let mut response = TestClient::post("http://127.0.0.1/console/workflow-session")
                .add_header("host", "127.0.0.1", true)
                .add_header("origin", "http://127.0.0.1", true)
                .add_header("content-type", "application/json", true)
                .json(&serde_json::json!({"session_id": session_id}))
                .send(&service)
                .await;
            assert_eq!(
                response.status_code.map(|status| status.as_u16()),
                Some(404)
            );
            let body: serde_json::Value = response.take_json().await.unwrap();
            assert_eq!(body["error"]["code"], "workflow_session_not_found");
            assert!(!serde_json::to_string(&body)
                .unwrap()
                .contains("HIDDEN_SECRET_TITLE"));
        }

        let spoofed = TestClient::post("http://127.0.0.1/console/workflow-sessions")
            .add_header("host", "127.0.0.1", true)
            .add_header("origin", "http://127.0.0.1", true)
            .add_header("content-type", "application/json", true)
            .json(&serde_json::json!({"project": "agent:elsewhere:hidden"}))
            .send(&service)
            .await;
        assert_eq!(spoofed.status_code.map(|status| status.as_u16()), Some(400));
    }

    #[tokio::test]
    async fn connect_targets_stay_secret_free() {
        let fixture = crate::connector_runtime::execution_tests::console_fixture().await;
        let auth = crate::connector_runtime::tests::auth("u1");
        let service = service(fixture.runtime.clone(), auth);
        let mut response = TestClient::post("http://127.0.0.1/console/connect")
            .add_header("host", "127.0.0.1", true)
            .add_header("origin", "http://127.0.0.1", true)
            .add_header("content-type", "application/json", true)
            .json(&serde_json::json!({}))
            .send(&service)
            .await;
        let body: serde_json::Value = response.take_json().await.unwrap();
        let public_url = body
            .get("public_url")
            .cloned()
            .expect("connect projection includes public_url");
        assert_eq!(
            body,
            serde_json::json!({
                "project": "project",
                "profile": "personal",
                "public_url": public_url,
                "mcp_path": "/mcp",
                "actions_schema_path": "/openapi.json",
                "surface": {
                    "mode": "project_bound",
                    "capability_count": crate::connector_runtime::surface::CAPABILITY_NAMES.len(),
                    "operator_runtime_exposed": false,
                },
                "operator_runtime": {
                    "available": true,
                    "model_default": false,
                    "purpose": "management_development_internal_execution",
                    "tool_count": crate::tool_runtime::registered_tool_specs().len(),
                    "access": "operator_authenticated_host_runtime",
                },
                "oauth_discovery_path": "/.well-known/oauth-authorization-server",
            }),
            "the screenshot-safe response must not grow credential-bearing fields"
        );
    }

    #[tokio::test]
    async fn result_decisions_reject_malformed_reasons_before_any_lookup() {
        let fixture = crate::connector_runtime::execution_tests::console_fixture().await;
        let auth = crate::connector_runtime::tests::auth("u1");
        let service = service(fixture.runtime.clone(), auth);
        let task_id = "wc_task_0123456789abcdef0123456789abcdef";
        let cases = [
            // An oversized reason never reaches the decision.
            (
                "reject",
                serde_json::json!({ "task_id": task_id, "reason": "r".repeat(501) }),
            ),
            // Accept has no delivery channel for a reason; refusing beats
            // silently discarding what the reviewer typed.
            (
                "accept",
                serde_json::json!({
                    "task_id": task_id,
                    "result_id": "wc_result_0123456789abcdef",
                    "reason": "looks good"
                }),
            ),
            // An interrupted task without a stable result has no future model
            // call to receive decision guidance, so reject the reason instead
            // of claiming it was delivered.
            (
                "reject",
                serde_json::json!({ "task_id": task_id, "reason": "why" }),
            ),
        ];
        for (action, body) in cases {
            let mut response =
                TestClient::post(format!("http://127.0.0.1/console/result/{action}"))
                    .add_header("host", "127.0.0.1", true)
                    .add_header("origin", "http://127.0.0.1", true)
                    .add_header("content-type", "application/json", true)
                    .json(&body)
                    .send(&service)
                    .await;
            assert_eq!(
                response.status_code.map(|status| status.as_u16()),
                Some(400),
                "{action} must refuse the malformed reason"
            );
            let body: serde_json::Value = response.take_json().await.unwrap();
            assert!(
                serde_json::to_string(&body)
                    .unwrap()
                    .contains("invalid decision input"),
                "unexpected error body: {body}"
            );
        }
    }
}
