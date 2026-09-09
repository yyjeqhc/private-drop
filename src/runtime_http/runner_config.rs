use super::{parse_json_body, render_result, require_runtime};
use crate::action_audit::ActionAudit;
use crate::tool_runtime::ToolCall;
use salvo::prelude::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerConfigCheckRequest {
    client_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerConfigReloadRequest {
    client_id: String,
    expected_generation: u64,
}

/// Hidden operator surface over the existing typed Runner config check. Route
/// auth owns runtime:read; ToolRuntime still enforces exact Runner visibility,
/// instance ownership, and the RunnerConfigControl capability.
#[handler]
pub async fn runner_config_check(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/runners/config/check", "runnerConfigCheck");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<RunnerConfigCheckRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunnerConfigCheck {
                client_id: body.client_id,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "runner_config_check", None, result);
}

/// Hidden operator surface over the existing generation-CAS Runner config
/// reload. It does not accept a config path and never bypasses ToolRuntime's
/// exact Runner/capability fence or the shared authority decision gate.
#[handler]
pub async fn runner_config_reload(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/runners/config/reload",
        "runnerConfigReload",
    );
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<RunnerConfigReloadRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunnerConfigReload {
                client_id: body.client_id,
                expected_generation: body.expected_generation,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "runner_config_reload", None, result);
}
