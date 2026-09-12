use super::{
    effective_register_owner, enforce_register_owner, enforce_runner_transport, get_registry,
    require_runner_transport_scope, runner_access_from_auth,
};
use crate::runner_protocol::{
    RunnerJobUpdateRequest, RunnerJobUpdateResponse, RunnerOfflineRequest, RunnerOfflineResponse,
    RunnerPersistentShellResultRequest, RunnerPersistentShellResultResponse, RunnerPollPayload,
    RunnerPollResponse, RunnerRegisterRequest, RunnerRegisterResponse, RunnerResultPayload,
    RunnerResultResponse,
};
use salvo::prelude::*;

#[handler]
pub async fn runner_register(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(registry) = get_registry(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(RunnerRegisterResponse {
            success: false,
            client: None,
            error: Some("Runner registry not configured".to_string()),
        }));
        return;
    };
    let body: RunnerRegisterRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerRegisterResponse {
                success: false,
                client: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    // Phase 3: Runner transport endpoints require bootstrap or an agent token
    // with the agent:register scope. User tokens are rejected.
    if let Err(e) = require_runner_transport_scope(auth.as_ref(), crate::auth::SCOPE_AGENT_REGISTER)
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerRegisterResponse {
            success: false,
            client: None,
            error: Some(e),
        }));
        return;
    }
    if let Err(e) = enforce_register_owner(auth.as_ref(), &body.client_id, body.owner.as_deref()) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerRegisterResponse {
            success: false,
            client: None,
            error: Some(e),
        }));
        return;
    }
    // Resolve the effective owner: an agent token fills the owner from its
    // own username; bootstrap keeps the request body owner.
    let mut body = body;
    body.owner = effective_register_owner(auth.as_ref(), body.owner.as_deref());
    let access = runner_access_from_auth(auth.as_ref());
    match registry.register_with_auth(body, access.as_ref()).await {
        Ok(client) => res.render(Json(RunnerRegisterResponse {
            success: true,
            client: Some(client),
            error: None,
        })),
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerRegisterResponse {
                success: false,
                client: None,
                error: Some(e),
            }));
        }
    }
}

#[handler]
pub async fn runner_offline(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(registry) = get_registry(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(RunnerOfflineResponse {
            success: false,
            error: Some("Runner registry not configured".to_string()),
        }));
        return;
    };
    let body: RunnerOfflineRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerOfflineResponse {
                success: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    if let Err(e) = require_runner_transport_scope(auth.as_ref(), crate::auth::SCOPE_AGENT_REGISTER)
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerOfflineResponse {
            success: false,
            error: Some(e),
        }));
        return;
    }
    if let Err(e) = enforce_runner_transport(auth.as_ref(), &body.client_id) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerOfflineResponse {
            success: false,
            error: Some(e),
        }));
        return;
    }
    let access = runner_access_from_auth(auth.as_ref());
    if let Err(e) = registry
        .assert_runner_access(access.as_ref(), &body.client_id)
        .await
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerOfflineResponse {
            success: false,
            error: Some(e),
        }));
        return;
    }
    match registry
        .reconcile_polling_disconnect(&body.client_id, &body.runner_instance_id)
        .await
    {
        Ok(_) => res.render(Json(RunnerOfflineResponse {
            success: true,
            error: None,
        })),
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerOfflineResponse {
                success: false,
                error: Some(e),
            }));
        }
    }
}

#[handler]
pub async fn runner_poll(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(registry) = get_registry(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(RunnerPollResponse {
            success: false,
            request: None,
            error: Some("Runner registry not configured".to_string()),
            project_inventory: None,
        }));
        return;
    };
    let body: RunnerPollPayload = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerPollResponse {
                success: false,
                request: None,
                error: Some(format!("Invalid JSON: {}", e)),
                project_inventory: None,
            }));
            return;
        }
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    if let Err(e) = require_runner_transport_scope(auth.as_ref(), crate::auth::SCOPE_AGENT_POLL) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerPollResponse {
            success: false,
            request: None,
            error: Some(e),
            project_inventory: None,
        }));
        return;
    }
    if let Err(e) = enforce_runner_transport(auth.as_ref(), &body.request.client_id) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerPollResponse {
            success: false,
            request: None,
            error: Some(e),
            project_inventory: None,
        }));
        return;
    }
    let access = runner_access_from_auth(auth.as_ref());
    if let Err(e) = registry
        .assert_runner_access(access.as_ref(), &body.request.client_id)
        .await
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerPollResponse {
            success: false,
            request: None,
            error: Some(e),
            project_inventory: None,
        }));
        return;
    }
    let _ = registry
        .update_runtime_metadata(
            &body.request.client_id,
            &body.request.runner_instance_id,
            body.tool_providers,
            body.mcp_gateway_providers,
        )
        .await;
    let client_id = body.request.client_id.clone();
    let runner_instance_id = body.request.runner_instance_id.clone();
    let inventory_page = body.project_inventory_page;
    match registry.poll(body.request).await {
        Ok(request) => {
            let project_inventory = if let Some(page) = inventory_page {
                registry
                    .apply_project_inventory_page(&client_id, &runner_instance_id, page)
                    .await
                    .ok()
            } else {
                None
            };
            res.render(Json(RunnerPollResponse {
                success: true,
                request,
                error: None,
                project_inventory,
            }))
        }
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerPollResponse {
                success: false,
                request: None,
                error: Some(e),
                project_inventory: None,
            }));
        }
    }
}

#[handler]
pub async fn runner_result(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(registry) = get_registry(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(RunnerResultResponse {
            success: false,
            error: Some("Runner registry not configured".to_string()),
        }));
        return;
    };
    let body: RunnerResultPayload = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerResultResponse {
                success: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    if let Err(e) = require_runner_transport_scope(auth.as_ref(), crate::auth::SCOPE_AGENT_RESULT) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerResultResponse {
            success: false,
            error: Some(e),
        }));
        return;
    }
    if let Err(e) = enforce_runner_transport(auth.as_ref(), &body.result.client_id) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerResultResponse {
            success: false,
            error: Some(e),
        }));
        return;
    }
    let access = runner_access_from_auth(auth.as_ref());
    if let Err(e) = registry
        .assert_runner_access(access.as_ref(), &body.result.client_id)
        .await
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerResultResponse {
            success: false,
            error: Some(e),
        }));
        return;
    }
    match registry.complete(body).await {
        Ok(()) => res.render(Json(RunnerResultResponse {
            success: true,
            error: None,
        })),
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerResultResponse {
                success: false,
                error: Some(e),
            }));
        }
    }
}

#[handler]
pub async fn runner_persistent_shell_result(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) {
    let Some(registry) = get_registry(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(RunnerPersistentShellResultResponse {
            success: false,
            error: Some("Runner registry not configured".to_string()),
        }));
        return;
    };
    let body: RunnerPersistentShellResultRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(error) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerPersistentShellResultResponse {
                success: false,
                error: Some(format!("Invalid JSON: {error}")),
            }));
            return;
        }
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    if let Err(error) =
        require_runner_transport_scope(auth.as_ref(), crate::auth::SCOPE_AGENT_RESULT)
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerPersistentShellResultResponse {
            success: false,
            error: Some(error),
        }));
        return;
    }
    if let Err(error) = enforce_runner_transport(auth.as_ref(), &body.client_id) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerPersistentShellResultResponse {
            success: false,
            error: Some(error),
        }));
        return;
    }
    let access = runner_access_from_auth(auth.as_ref());
    if let Err(error) = registry
        .assert_runner_access(access.as_ref(), &body.client_id)
        .await
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerPersistentShellResultResponse {
            success: false,
            error: Some(error),
        }));
        return;
    }
    match registry.complete_persistent_shell(body).await {
        Ok(()) => res.render(Json(RunnerPersistentShellResultResponse {
            success: true,
            error: None,
        })),
        Err(error) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerPersistentShellResultResponse {
                success: false,
                error: Some(error),
            }));
        }
    }
}

#[handler]
pub async fn runner_job_update(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(registry) = get_registry(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(RunnerJobUpdateResponse {
            success: false,
            job: None,
            error: Some("Runner registry not configured".to_string()),
        }));
        return;
    };
    let body: RunnerJobUpdateRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerJobUpdateResponse {
                success: false,
                job: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    if let Err(e) =
        require_runner_transport_scope(auth.as_ref(), crate::auth::SCOPE_AGENT_JOB_UPDATE)
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerJobUpdateResponse {
            success: false,
            job: None,
            error: Some(e),
        }));
        return;
    }
    if let Err(e) = enforce_runner_transport(auth.as_ref(), &body.client_id) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerJobUpdateResponse {
            success: false,
            job: None,
            error: Some(e),
        }));
        return;
    }
    let access = runner_access_from_auth(auth.as_ref());
    if let Err(e) = registry
        .assert_runner_access(access.as_ref(), &body.client_id)
        .await
    {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(RunnerJobUpdateResponse {
            success: false,
            job: None,
            error: Some(e),
        }));
        return;
    }
    match registry.update_job(body).await {
        Ok(job) => res.render(Json(RunnerJobUpdateResponse {
            success: true,
            job: Some(job),
            error: None,
        })),
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(RunnerJobUpdateResponse {
                success: false,
                job: None,
                error: Some(e),
            }));
        }
    }
}
