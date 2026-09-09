use super::*;
use crate::auth::middleware::enforce_project_connector_surface;

fn bootstrap_ctx() -> AuthContext {
    bootstrap_context()
}

fn user_ctx(username: &str) -> AuthContext {
    AuthContext {
        user_id: Some(format!("user-{}", username)),
        username: Some(username.to_string()),
        api_key_id: Some("key-1".to_string()),
        role: Some("user".to_string()),
        scopes: vec![SCOPE_RUNTIME_READ.to_string()],
        token_kind: Some("user".to_string()),
        ..AuthContext::new(AuthKind::ApiToken)
    }
}

fn agent_ctx(username: &str, client_id: &str, scopes: Vec<String>) -> AuthContext {
    AuthContext {
        user_id: Some(format!("user-{}", username)),
        username: Some(username.to_string()),
        api_key_id: Some("key-agent".to_string()),
        role: Some("user".to_string()),
        scopes,
        token_kind: Some("agent".to_string()),
        allowed_client_id: Some(client_id.to_string()),
        ..AuthContext::new(AuthKind::AgentToken)
    }
}

#[test]
fn agent_token_auth_context_identifies_as_agent_token() {
    let ctx = agent_ctx(
        "alice",
        "alice-laptop",
        vec![
            SCOPE_AGENT_REGISTER.to_string(),
            SCOPE_AGENT_POLL.to_string(),
        ],
    );
    assert!(ctx.is_agent_token());
    assert_ne!(ctx.kind, AuthKind::ApiToken);
    assert!(!ctx.is_bootstrap());
    assert_eq!(ctx.token_kind.as_deref(), Some("agent"));
    assert_eq!(ctx.allowed_client_id.as_deref(), Some("alice-laptop"));
    assert_eq!(ctx.username.as_deref(), Some("alice"));
}

#[test]
fn user_token_auth_context_does_not_get_agent_kind() {
    let ctx = user_ctx("alice");
    assert_eq!(ctx.kind, AuthKind::ApiToken);
    assert!(!ctx.is_agent_token());
    assert_eq!(ctx.token_kind.as_deref(), Some("user"));
    assert!(ctx.allowed_client_id.is_none());
}

#[test]
fn is_runner_transport_path_allows_only_the_seven_exact_paths() {
    // The seven Runner transport endpoints an agent token may call.
    assert!(is_runner_transport_path("/api/shell/agent/register"));
    assert!(is_runner_transport_path("/api/shell/agent/offline"));
    assert!(is_runner_transport_path("/api/shell/agent/poll"));
    assert!(is_runner_transport_path("/api/shell/agent/result"));
    assert!(is_runner_transport_path(
        "/api/shell/agent/persistent_shell_result"
    ));
    assert!(is_runner_transport_path("/api/shell/agent/job_update"));
    assert!(is_runner_transport_path("/api/agents/ws"));

    // Everything else is rejected — including paths that look similar.
    assert!(!is_runner_transport_path("/api/agent-tokens/create"));
    assert!(!is_runner_transport_path("/api/agent-tokens/register_hash"));
    assert!(!is_runner_transport_path("/api/agent-tokens/list"));
    assert!(!is_runner_transport_path("/api/agent-tokens/revoke"));
    assert!(!is_runner_transport_path("/api/pairing/create"));
    assert!(!is_runner_transport_path("/api/pairing/enroll"));
    assert!(!is_runner_transport_path("/api/runtime/status"));
    assert!(!is_runner_transport_path("/api/tools/list"));
    assert!(!is_runner_transport_path("/api/tools/call"));
    assert!(!is_runner_transport_path("/api/projects/list"));
    assert!(!is_runner_transport_path("/api/jobs/list"));
    assert!(!is_runner_transport_path("/mcp"));
    assert!(!is_runner_transport_path("/api/audit/sessions"));
    assert!(!is_runner_transport_path("/api/users/list"));
    assert!(!is_runner_transport_path("/api/tokens/list"));
    // Prefix and trailing-slash aliases must not pass (exact match required).
    assert!(!is_runner_transport_path("/api/shell/agent/register/extra"));
    assert!(!is_runner_transport_path("/api/agents/ws/extra"));
    assert!(!is_runner_transport_path("/api/agents/ws/"));
    assert!(!is_runner_transport_path(""));
}

// -----------------------------------------------------------------------
// HTTP-level central gate tests
// -----------------------------------------------------------------------
//
// These build a minimal router that mounts a generic echo handler behind
// the shared AuthMiddleware on a representative set of paths. The handler
// is intentionally trivial — the point is to verify the Phase 3 central
// gate in AuthMiddleware rejects agent tokens before any handler runs, and
// that bootstrap/user tokens still reach the handler.

use crate::test_support::{
    seed_oauth_client_named as gate_seed_oauth_client, seed_user as gate_seed_user,
    test_config as gate_test_config, test_config_oauth2 as gate_test_config_oauth2,
    test_db as gate_test_db,
};
use salvo::prelude::{affix_state, Json};
use salvo::test::{ResponseExt, TestClient};
use salvo::Router;
use salvo::Service;
use std::path::PathBuf;
use std::sync::Arc;

/// Create an agent token for `username` bound to `client_id` via the DB
/// layer directly, returning the plaintext token. Used by the gate tests
/// so we exercise the real AuthMiddleware path.
fn gate_mint_agent_token(
    db: &crate::Database,
    user: &crate::models::UserRecord,
    client_id: &str,
) -> String {
    let plaintext = generate_agent_token();
    let prefix = token_prefix(&plaintext);
    let hash = hash_token(&plaintext);
    let now = chrono::Utc::now().timestamp();
    let record = crate::models::ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        name: "agent".to_string(),
        key_prefix: prefix,
        created_at: now,
        last_used_at: None,
        revoked_at: None,
        scopes: "agent:register agent:poll agent:result agent:job_update".to_string(),
        expires_at: None,
        kind: crate::models::TOKEN_KIND_AGENT.to_string(),
        allowed_client_id: Some(client_id.to_string()),
    };
    db.insert_api_key(&record, &hash).unwrap();
    plaintext
}

/// Create a user token for `username` via the DB layer directly.
fn gate_mint_user_token_with_scopes(
    db: &crate::Database,
    user: &crate::models::UserRecord,
    scopes: &str,
) -> String {
    let plaintext = generate_api_token();
    let prefix = token_prefix(&plaintext);
    let hash = hash_token(&plaintext);
    let now = chrono::Utc::now().timestamp();
    let record = crate::models::ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        name: "user".to_string(),
        key_prefix: prefix,
        created_at: now,
        last_used_at: None,
        revoked_at: None,
        scopes: scopes.to_string(),
        expires_at: None,
        kind: crate::models::TOKEN_KIND_USER.to_string(),
        allowed_client_id: None,
    };
    db.insert_api_key(&record, &hash).unwrap();
    plaintext
}

fn gate_mint_user_token(db: &crate::Database, user: &crate::models::UserRecord) -> String {
    gate_mint_user_token_with_scopes(db, user, "runtime:read project:read project:write job:run")
}

fn gate_mint_account_credential(db: &crate::Database, user: &crate::models::UserRecord) -> String {
    let plaintext = generate_account_credential();
    let prefix = token_prefix(&plaintext);
    let hash = hash_token(&plaintext);
    let now = chrono::Utc::now().timestamp();
    let record = crate::models::AccountCredentialRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        credential_prefix: prefix,
        created_at: now,
        last_used_at: None,
        revoked_at: None,
    };
    db.insert_account_credential(&record, &hash).unwrap();
    plaintext
}

/// Seed an OAuth2 access token and return `(record, plaintext_token)`.
fn gate_seed_oauth_access_token(
    db: &crate::Database,
    client: &crate::models::OAuthClientRecord,
    user: &crate::models::UserRecord,
    scopes: &str,
) -> (crate::models::OAuthAccessTokenRecord, String) {
    gate_seed_oauth_access_token_with_shared_key_hash(db, client, user, scopes, None)
}

fn gate_seed_oauth_access_token_with_shared_key_hash(
    db: &crate::Database,
    client: &crate::models::OAuthClientRecord,
    user: &crate::models::UserRecord,
    scopes: &str,
    shared_key_hash: Option<&str>,
) -> (crate::models::OAuthAccessTokenRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext = generate_oauth_access_token();
    let token_hash = hash_token(&plaintext);
    let record = crate::models::OAuthAccessTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash,
        client_id: client.client_id.clone(),
        subject_kind: "managed_user".to_string(),
        subject_id: user.id.clone(),
        user_id: Some(user.id.clone()),
        scopes: scopes.to_string(),
        resource: None,
        shared_key_hash: shared_key_hash.map(str::to_string),
        created_at: now,
        expires_at: now + 3600,
        revoked_at: None,
        last_used_at: None,
    };
    db.insert_oauth_access_token(&record).unwrap();
    (record, plaintext)
}

fn gate_seed_shared_key_oauth_access_token(
    db: &crate::Database,
    client: &crate::models::OAuthClientRecord,
    scopes: &str,
    shared_key_hash: &str,
) -> (crate::models::OAuthAccessTokenRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext = generate_oauth_access_token();
    let token_hash = hash_token(&plaintext);
    let record = crate::models::OAuthAccessTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash,
        client_id: client.client_id.clone(),
        subject_kind: "shared_key".to_string(),
        subject_id: shared_key_hash.to_string(),
        user_id: None,
        scopes: scopes.to_string(),
        resource: None,
        shared_key_hash: Some(shared_key_hash.to_string()),
        created_at: now,
        expires_at: now + 3600,
        revoked_at: None,
        last_used_at: None,
    };
    db.insert_oauth_access_token(&record).unwrap();
    (record, plaintext)
}
/// Trivial handler that returns 200 OK JSON. Reaching it proves the central
/// gate did not reject the request.
#[salvo::handler]
async fn echo_ok(_req: &mut salvo::Request, _depot: &mut salvo::Depot, res: &mut salvo::Response) {
    res.render(Json(serde_json::json!({"ok": true})));
}

/// Build a router mirroring the production path shapes the central gate
/// must protect, each backed by the same trivial echo handler.
fn gate_router(config: Arc<crate::Config>, db: Arc<crate::Database>) -> Router {
    Router::new()
        .hoop(affix_state::inject(config))
        .hoop(affix_state::inject(db))
        // /mcp at the root (not under /api).
        .push(
            Router::with_path("mcp")
                .hoop(AuthMiddleware)
                .get(echo_ok)
                .post(echo_ok),
        )
        .push(
            Router::with_path("api")
                .hoop(AuthMiddleware)
                .push(Router::with_path("runtime/status").post(echo_ok))
                .push(Router::with_path("tools/list").post(echo_ok))
                .push(Router::with_path("tools/call").post(echo_ok))
                .push(Router::with_path("future/authenticated-route").post(echo_ok))
                .push(Router::with_path("projects/list").post(echo_ok))
                .push(Router::with_path("projects/read_file").post(echo_ok))
                .push(Router::with_path("projects/run_job").post(echo_ok))
                .push(Router::with_path("jobs/list").post(echo_ok))
                .push(Router::with_path("audit/sessions").post(echo_ok))
                .push(Router::with_path("audit/session").post(echo_ok))
                .push(Router::with_path("audit/stats").post(echo_ok))
                .push(Router::with_path("users/me").post(echo_ok))
                .push(Router::with_path("users/list").post(echo_ok))
                .push(Router::with_path("tokens/list").post(echo_ok))
                .push(Router::with_path("tokens/register_hash").post(echo_ok))
                .push(Router::with_path("tokens/revoke").post(echo_ok))
                .push(Router::with_path("agent-tokens/register_hash").post(echo_ok))
                .push(Router::with_path("agent-tokens/list").post(echo_ok))
                .push(Router::with_path("shell/agent/register").post(echo_ok))
                .push(Router::with_path("shell/agent/offline").post(echo_ok))
                .push(Router::with_path("shell/agent/poll").post(echo_ok))
                .push(Router::with_path("shell/agent/result").post(echo_ok))
                .push(Router::with_path("shell/agent/persistent_shell_result").post(echo_ok))
                .push(Router::with_path("shell/agent/job_update").post(echo_ok))
                .push(Router::with_path("agents/ws").get(echo_ok)),
        )
        .push(
            Router::with_path("oauth/authorize")
                .hoop(AuthMiddleware)
                .get(echo_ok),
        )
}

fn gate_status(resp: &salvo::Response) -> salvo::http::StatusCode {
    resp.status_code.unwrap_or(salvo::http::StatusCode::OK)
}

async fn gate_send(
    service: &salvo::Service,
    path: &str,
    auth: Option<&str>,
) -> (salvo::http::StatusCode, serde_json::Value) {
    let mut req = TestClient::post(format!("http://localhost{}", path));
    if path == "/api/agents/ws"
        || path.starts_with("/api/agents/ws?")
        || path == "/oauth/authorize"
        || path.starts_with("/oauth/authorize?")
    {
        // These endpoints are GET-mounted in this test router.
        req = TestClient::get(format!("http://localhost{}", path));
    }
    if let Some(token) = auth {
        req = req.bearer_auth(token);
    }
    let mut resp = req.send(service).await;
    let status = gate_status(&resp);
    let body = resp
        .take_json::<serde_json::Value>()
        .await
        .ok()
        .unwrap_or(serde_json::json!({"_raw": "<no json body>"}));
    (status, body)
}

#[tokio::test]
async fn gate_token_class_route_matrix_preserves_authority_boundaries() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let agent_token = gate_mint_agent_token(&db, &user, "alice-laptop");
    let user_token = gate_mint_user_token(&db, &user);
    let service = Service::new(gate_router(config, db));

    for path in ["/api/shell/agent/register", "/api/agents/ws"] {
        let (status, body) = gate_send(&service, path, Some(&agent_token)).await;
        assert_eq!(status, salvo::http::StatusCode::OK, "{path}: {body:?}");
        assert_eq!(body["ok"], true);
    }

    for path in [
        "/api/runtime/status",
        "/api/tools/list",
        "/api/projects/list",
        "/mcp",
        "/api/agent-tokens/list",
        "/api/tokens/list",
    ] {
        let (status, body) = gate_send(&service, path, Some(&agent_token)).await;
        assert_eq!(
            status,
            salvo::http::StatusCode::FORBIDDEN,
            "agent token should be forbidden on {path}: {body:?}"
        );
        if path == "/api/runtime/status" {
            assert!(body["error"]
                .as_str()
                .unwrap_or("")
                .contains("agent tokens are only allowed"));
        }
    }

    for path in [
        "/api/runtime/status",
        "/api/tools/list",
        "/api/projects/list",
    ] {
        let (status, body) = gate_send(&service, path, Some(&user_token)).await;
        assert_eq!(status, salvo::http::StatusCode::OK, "{path}: {body:?}");
    }

    for path in [
        "/api/runtime/status",
        "/api/tools/list",
        "/api/projects/list",
        "/api/shell/agent/register",
        "/api/agent-tokens/list",
    ] {
        let (status, body) = gate_send(&service, path, Some("secret")).await;
        assert_eq!(
            status,
            salvo::http::StatusCode::OK,
            "bootstrap should reach {path}: {body:?}"
        );
    }
}

#[test]
fn account_control_path_allowlist_is_exact() {
    assert!(is_account_control_path("/api/users/me"));
    assert!(is_account_control_path("/api/tokens/list"));
    assert!(is_account_control_path("/api/tokens/register_hash"));
    assert!(is_account_control_path("/api/tokens/revoke"));
    assert!(is_account_control_path("/api/agent-tokens/register_hash"));
    assert!(!is_account_control_path("/api/runtime/status"));
    assert!(!is_account_control_path("/api/projects/list"));
    assert!(!is_account_control_path("/api/tools/list"));
    assert!(!is_account_control_path("/mcp"));
    assert!(!is_account_control_path("/api/users/me/extra"));
    assert!(!is_account_control_path("/api/users/me/"));
}

#[tokio::test]
async fn gate_account_credential_can_call_account_control_endpoints_and_updates_last_used() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let credential = gate_mint_account_credential(&db, &user);
    let credential_hash = hash_token(&credential);
    let before = db
        .get_account_credential_by_hash(&credential_hash)
        .unwrap()
        .unwrap();
    assert!(before.last_used_at.is_none());
    let service = Service::new(gate_router(config, db.clone()));
    for path in [
        "/api/users/me",
        "/api/tokens/list",
        "/api/tokens/register_hash",
        "/api/tokens/revoke",
        "/api/agent-tokens/register_hash",
    ] {
        let (status, body) = gate_send(&service, path, Some(&credential)).await;
        assert_eq!(
            status,
            salvo::http::StatusCode::OK,
            "{} body: {:?}",
            path,
            body
        );
    }
    let after = db
        .get_account_credential_by_hash(&credential_hash)
        .unwrap()
        .unwrap();
    assert!(after.last_used_at.is_some());
}

#[tokio::test]
async fn gate_account_credential_cannot_call_runtime_project_tool_or_mcp_paths() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let credential = gate_mint_account_credential(&db, &user);
    let service = Service::new(gate_router(config, db));
    for path in [
        "/api/runtime/status",
        "/api/projects/list",
        "/api/tools/list",
        "/api/shell/agent/register",
        "/mcp",
    ] {
        let (status, body) = gate_send(&service, path, Some(&credential)).await;
        assert_eq!(status, salvo::http::StatusCode::FORBIDDEN, "{}", path);
        assert_eq!(
            body["error"],
            "account credentials may only access account control endpoints"
        );
    }
}

#[tokio::test]
async fn gate_disabled_user_account_credential_is_rejected() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let credential = gate_mint_account_credential(&db, &user);
    db.set_user_disabled(&user.id, true, chrono::Utc::now().timestamp())
        .unwrap();
    let service = Service::new(gate_router(config, db));
    let (status, body) = gate_send(&service, "/api/users/me", Some(&credential)).await;
    assert_eq!(status, salvo::http::StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "Unauthorized");
}

#[tokio::test]
async fn gate_websocket_query_token_is_rejected_and_bearer_remains_authoritative() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let service = Service::new(gate_router(config, db));

    for path in [
        "/api/runtime/status?token=secret",
        "/api/agents/ws?token=secret",
    ] {
        let (status, body) = gate_send(&service, path, None).await;
        assert_eq!(
            status,
            salvo::http::StatusCode::UNAUTHORIZED,
            "{path}: {body:?}"
        );
        assert_eq!(body["error"], "Unauthorized");
    }

    // Query text is inert when a valid Bearer credential is present.
    let (status, body) = gate_send(
        &service,
        "/api/agents/ws?token=wrong-query-token",
        Some("secret"),
    )
    .await;
    assert_eq!(status, salvo::http::StatusCode::OK, "body: {body:?}");
    assert_eq!(body["ok"], true);

    let (status, body) = gate_send(
        &service,
        "/api/agents/ws?token=secret",
        Some("wrong-header-token"),
    )
    .await;
    assert_eq!(status, salvo::http::StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "Unauthorized");

    let mut resp = TestClient::get("http://localhost/api/agents/ws?token=secret")
        .add_header("authorization", "Basic ignored", true)
        .send(&service)
        .await;
    assert_eq!(gate_status(&resp), salvo::http::StatusCode::UNAUTHORIZED);
    let body = resp.take_json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["error"], "Unauthorized");

    let (status, body) = gate_send(&service, "/api/runtime/status", Some("secret")).await;
    assert_eq!(status, salvo::http::StatusCode::OK, "body: {body:?}");
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn gate_forbidden_response_is_json_not_html() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let agent_token = gate_mint_agent_token(&db, &user, "alice-laptop");
    let service = Service::new(gate_router(config, db));
    let resp = TestClient::post("http://localhost/api/runtime/status")
        .bearer_auth(&agent_token)
        .send(&service)
        .await;
    assert_eq!(gate_status(&resp), salvo::http::StatusCode::FORBIDDEN);
    let ct = resp.headers().get("content-type").unwrap();
    assert!(
        ct.to_str().unwrap().contains("application/json"),
        "forbidden response must be JSON, got: {:?}",
        ct
    );
}

#[tokio::test]
async fn gate_unauthorized_response_is_json_not_html() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let service = Service::new(gate_router(config, db));
    let resp = TestClient::post("http://localhost/api/runtime/status")
        .send(&service)
        .await;
    assert_eq!(gate_status(&resp), salvo::http::StatusCode::UNAUTHORIZED);
    let ct = resp.headers().get("content-type").unwrap();
    assert!(
        ct.to_str().unwrap().contains("application/json"),
        "unauthorized response must be JSON, got: {:?}",
        ct
    );
}

// -----------------------------------------------------------------------
// TokenVerifier trait tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn pat_verifier_bootstrap_when_auth_disabled() {
    let config = crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: None, // auth disabled
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    };
    let verifier = PatVerifier;
    let result = verifier.verify(&config, None, "anything").await.unwrap();
    let ctx = result.expect("should return bootstrap context");
    assert!(ctx.is_bootstrap);
    assert_eq!(ctx.kind, AuthKind::Bootstrap);
}

#[tokio::test]
async fn pat_verifier_bootstrap_token() {
    let config = crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("secret".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    };
    let verifier = PatVerifier;
    let result = verifier.verify(&config, None, "secret").await.unwrap();
    let ctx = result.expect("should return bootstrap context");
    assert!(ctx.is_bootstrap);
}

#[tokio::test]
async fn pat_verifier_rejects_unknown_token_without_db() {
    let config = crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("secret".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    };
    let verifier = PatVerifier;
    let result = verifier
        .verify(&config, None, "wc_pat_bogus")
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "unknown token without DB should return None"
    );
}

#[tokio::test]
async fn oauth2_verifier_ignores_non_wc_oat_tokens() {
    let config = crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("secret".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config {
            enabled: true,
            ..crate::OAuth2Config::default()
        },
    };
    let verifier = OAuth2Verifier;
    // Non-wc_oat_ tokens should return Ok(None) (not recognized).
    for token in &[
        "some-oauth2-jwt",
        "wc_pat_abc123",
        "wc_agent_abc123",
        "wc_acct_abc123",
        "wc_ort_abc123",
        "wc_oac_abc123",
        "wc_csec_abc123",
        "wc_client_abc123",
    ] {
        let result = verifier.verify(&config, None, token).await.unwrap();
        assert!(
            result.is_none(),
            "non-wc_oat_ token '{}' should return None",
            token
        );
    }
}

#[tokio::test]
async fn oauth2_verifier_returns_none_when_oauth2_disabled() {
    let config = crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("secret".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(), // enabled: false
    };
    let verifier = OAuth2Verifier;
    let result = verifier
        .verify(&config, None, "wc_oat_sometoken")
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "OAuth2 disabled should return None for wc_oat_* tokens"
    );
}

#[tokio::test]
async fn oauth2_verifier_accepts_valid_access_token() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
    let (_at, plaintext) = gate_seed_oauth_access_token(&db, &client, &user, "runtime:read");

    let verifier = OAuth2Verifier;
    let result = verifier
        .verify(&config, Some(&db), &plaintext)
        .await
        .unwrap();
    let ctx = result.expect("valid access token should return AuthContext");
    assert_eq!(ctx.kind, AuthKind::OAuth2Token);
    assert_eq!(ctx.user_id.as_deref(), Some(user.id.as_str()));
    assert_eq!(ctx.username.as_deref(), Some(user.username.as_str()));
    assert_eq!(ctx.role.as_deref(), Some("user"));
    assert!(ctx.scopes.contains(&"runtime:read".to_string()));
    assert!(!ctx.is_bootstrap);
    assert_eq!(ctx.token_kind.as_deref(), Some("oauth2"));
    assert_eq!(
        ctx.allowed_client_id.as_deref(),
        Some(client.client_id.as_str())
    );
    assert_eq!(ctx.shared_key_hash, None);
}

#[tokio::test]
async fn oauth2_verifier_ignores_managed_user_stray_shared_key_hash() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
    let (_at, plaintext) = gate_seed_oauth_access_token_with_shared_key_hash(
        &db,
        &client,
        &user,
        "runtime:read",
        Some("test-hash-a"),
    );

    let verifier = OAuth2Verifier;
    let ctx = verifier
        .verify(&config, Some(&db), &plaintext)
        .await
        .unwrap()
        .expect("managed-user access token should verify");
    assert_eq!(ctx.kind, AuthKind::OAuth2Token);
    assert_eq!(ctx.user_id.as_deref(), Some(user.id.as_str()));
    assert_eq!(ctx.username.as_deref(), Some(user.username.as_str()));
    assert_eq!(ctx.token_kind.as_deref(), Some("oauth2"));
    assert_eq!(ctx.shared_key_hash, None);
    assert!(!ctx.is_oauth_shared_key_subject());
}

#[tokio::test]
async fn oauth2_verifier_accepts_shared_key_subject_without_user_lookup() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let owner = gate_seed_user(&db, "owner");
    let (client, _secret) = gate_seed_oauth_client(&db, &owner, "Test App");
    let (at, plaintext) = gate_seed_shared_key_oauth_access_token(
        &db,
        &client,
        "runtime:read project:read",
        "test-hash-a",
    );
    db.set_user_disabled(&owner.id, true, chrono::Utc::now().timestamp())
        .unwrap();

    let verifier = OAuth2Verifier;
    let ctx = verifier
        .verify(&config, Some(&db), &plaintext)
        .await
        .unwrap()
        .expect("shared-key OAuth2 subject should verify");
    assert_eq!(ctx.kind, AuthKind::OAuth2Token);
    assert_eq!(ctx.user_id, None);
    assert_eq!(ctx.username, None);
    assert_eq!(ctx.api_key_id.as_deref(), Some(at.id.as_str()));
    assert_eq!(ctx.role.as_deref(), Some("shared-key"));
    assert_eq!(ctx.token_kind.as_deref(), Some("oauth2_shared_key"));
    assert_eq!(
        ctx.allowed_client_id.as_deref(),
        Some(client.client_id.as_str())
    );
    assert_eq!(ctx.shared_key_hash.as_deref(), Some("test-hash-a"));
    assert!(ctx.is_oauth_shared_key_subject());
    assert!(ctx.scopes.contains(&"runtime:read".to_string()));
    assert!(!ctx.is_admin());

    let stored = db
        .get_oauth_access_token_by_hash(&at.token_hash)
        .unwrap()
        .unwrap();
    assert!(stored.last_used_at.is_some());
}

#[tokio::test]
async fn oauth2_verifier_rejects_invalid_subject_combinations() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let owner = gate_seed_user(&db, "owner");
    let (client, _secret) = gate_seed_oauth_client(&db, &owner, "Test App");
    let now = chrono::Utc::now().timestamp();

    let cases = [
        (
            "managed-missing-user",
            "managed_user",
            owner.id.as_str(),
            None,
            None,
            "managed-user OAuth2 token missing user_id",
        ),
        (
            "managed-mismatch",
            "managed_user",
            "other-user",
            Some(owner.id.as_str()),
            None,
            "managed-user OAuth2 subject_id does not match user_id",
        ),
        (
            "shared-with-user",
            "shared_key",
            "hash-a",
            Some(owner.id.as_str()),
            Some("hash-a"),
            "shared-key OAuth2 token must not include user_id",
        ),
        (
            "shared-missing-hash",
            "shared_key",
            "hash-a",
            None,
            None,
            "shared-key OAuth2 token missing shared_key_hash",
        ),
        (
            "shared-mismatch",
            "shared_key",
            "hash-a",
            None,
            Some("hash-b"),
            "shared-key OAuth2 subject_id does not match shared_key_hash",
        ),
        (
            "unknown-kind",
            "surprise",
            "subject",
            None,
            None,
            "unsupported OAuth2 subject",
        ),
    ];

    for (label, subject_kind, subject_id, user_id, shared_key_hash, expected_error) in cases {
        let plaintext = generate_oauth_access_token();
        let token_hash = hash_token(&plaintext);
        let id = uuid::Uuid::new_v4().to_string();
        {
            let conn = db.conn_for_tests();
            conn.execute(
                "INSERT INTO oauth_access_tokens (
                        id, token_hash, client_id, subject_kind, subject_id, user_id,
                        scopes, resource, shared_key_hash, created_at, expires_at,
                        revoked_at, last_used_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, ?10, NULL, NULL)",
                rusqlite::params![
                    id,
                    token_hash,
                    client.client_id,
                    subject_kind,
                    subject_id,
                    user_id,
                    "runtime:read",
                    shared_key_hash,
                    now,
                    now + 3600,
                ],
            )
            .unwrap();
        }

        let verifier = OAuth2Verifier;
        let err = verifier
            .verify(&config, Some(&db), &plaintext)
            .await
            .expect_err(label);
        assert_eq!(err, expected_error, "{label}");
        let conn = db.conn_for_tests();
        let last_used: Option<i64> = conn
            .query_row(
                "SELECT last_used_at FROM oauth_access_tokens WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(last_used, None, "{label}");
    }
}

#[tokio::test]
async fn oauth2_verifier_rejects_invalid_access_token_state_matrix() {
    enum InvalidAccessToken {
        Unknown,
        Expired,
        Revoked,
        RevokedClient,
        DisabledUser,
    }

    for (label, case) in [
        ("unknown", InvalidAccessToken::Unknown),
        ("expired", InvalidAccessToken::Expired),
        ("revoked", InvalidAccessToken::Revoked),
        ("revoked client", InvalidAccessToken::RevokedClient),
        ("disabled user", InvalidAccessToken::DisabledUser),
    ] {
        let config = gate_test_config_oauth2(Some("secret"));
        let (_tmp, db) = gate_test_db();
        let plaintext = match case {
            InvalidAccessToken::Unknown => "wc_oat_nonexistenttoken".to_string(),
            InvalidAccessToken::Expired => {
                let user = gate_seed_user(&db, "alice");
                let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
                let now = chrono::Utc::now().timestamp();
                let plaintext = generate_oauth_access_token();
                db.insert_oauth_access_token(&crate::models::OAuthAccessTokenRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    token_hash: hash_token(&plaintext),
                    client_id: client.client_id,
                    subject_kind: "managed_user".to_string(),
                    subject_id: user.id.clone(),
                    user_id: Some(user.id),
                    scopes: "runtime:read".to_string(),
                    resource: None,
                    shared_key_hash: None,
                    created_at: now - 7200,
                    expires_at: now - 1,
                    revoked_at: None,
                    last_used_at: None,
                })
                .unwrap();
                plaintext
            }
            InvalidAccessToken::Revoked => {
                let user = gate_seed_user(&db, "alice");
                let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
                let (token, plaintext) =
                    gate_seed_oauth_access_token(&db, &client, &user, "runtime:read");
                db.revoke_oauth_access_token(&token.id, chrono::Utc::now().timestamp())
                    .unwrap();
                plaintext
            }
            InvalidAccessToken::RevokedClient => {
                let user = gate_seed_user(&db, "alice");
                let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
                let (_token, plaintext) =
                    gate_seed_oauth_access_token(&db, &client, &user, "runtime:read");
                db.revoke_oauth_client(&client.id, chrono::Utc::now().timestamp())
                    .unwrap();
                plaintext
            }
            InvalidAccessToken::DisabledUser => {
                let user = gate_seed_user(&db, "alice");
                let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
                let (_token, plaintext) =
                    gate_seed_oauth_access_token(&db, &client, &user, "runtime:read");
                db.set_user_disabled(&user.id, true, chrono::Utc::now().timestamp())
                    .unwrap();
                plaintext
            }
        };
        let result = OAuth2Verifier.verify(&config, Some(&db), &plaintext).await;
        assert!(result.is_err(), "{label} access token should return Err");
    }
}

#[tokio::test]
async fn oauth2_verifier_ignores_non_access_oauth_credential_kinds() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
    let now = chrono::Utc::now().timestamp();
    let refresh = generate_oauth_refresh_token();
    db.insert_oauth_refresh_token(&crate::models::OAuthRefreshTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash: hash_token(&refresh),
        client_id: client.client_id.clone(),
        subject_kind: "managed_user".to_string(),
        subject_id: user.id.clone(),
        user_id: Some(user.id),
        scopes: "runtime:read".to_string(),
        resource: None,
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 2_592_000,
        revoked_at: None,
        last_used_at: None,
        rotated_from_id: None,
    })
    .unwrap();

    for (label, token) in [
        ("refresh token", refresh.as_str()),
        ("authorization code", "wc_oac_sometoken"),
        ("client secret", "wc_csec_sometoken"),
    ] {
        let result = OAuth2Verifier
            .verify(&config, Some(&db), token)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "{label} must not authenticate as an access token"
        );
    }
}

#[tokio::test]
async fn oauth2_verifier_updates_last_used_on_success() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
    let (at, plaintext) = gate_seed_oauth_access_token(&db, &client, &user, "runtime:read");

    // Verify last_used_at is initially None.
    assert!(at.last_used_at.is_none());

    let verifier = OAuth2Verifier;
    let result = verifier
        .verify(&config, Some(&db), &plaintext)
        .await
        .unwrap();
    assert!(result.is_some());

    // Verify last_used_at is now set.
    let conn = db.conn_for_tests();
    let last_used: Option<i64> = conn
        .query_row(
            "SELECT last_used_at FROM oauth_access_tokens WHERE id = ?1",
            [&at.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        last_used.is_some(),
        "last_used_at should be set after successful verification"
    );
}

#[tokio::test]
async fn oauth2_verifier_does_not_update_last_used_on_failure() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
    let (at, plaintext) = gate_seed_oauth_access_token(&db, &client, &user, "runtime:read");

    // Revoke the token.
    let now = chrono::Utc::now().timestamp();
    db.revoke_oauth_access_token(&at.id, now).unwrap();

    let verifier = OAuth2Verifier;
    let result = verifier.verify(&config, Some(&db), &plaintext).await;
    assert!(result.is_err());

    // last_used_at should still be None — failed verification should not
    // update it. Note: the token is revoked so get_oauth_access_token_by_hash
    // returns None, so we can't directly check. But we verify the error path
    // doesn't panic or succeed.
}

#[tokio::test]
async fn oauth2_verifier_accepts_current_project_share_and_preserves_project_identity() {
    let mut config = (*gate_test_config_oauth2(Some("secret"))).clone();
    config.oauth2.project_share_grant_id = Some("wc_pgrant_111111111111111111111111".to_string());
    config.oauth2.project_share_session_id = Some(
        "wc_share_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
    );
    let config = Arc::new(config);
    let (_tmp, db) = gate_test_db();
    let secret = generate_oauth_client_secret();
    let client = crate::models::OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: generate_oauth_client_id(),
        client_secret_hash: hash_token(&secret),
        name: "project-share".to_string(),
        owner_user_id: None,
        owner_project_grant_id: Some("wc_pgrant_111111111111111111111111".to_string()),
        owner_shared_key_hash: None,
        redirect_uris: "https://client.example/callback".to_string(),
        allowed_scopes: "runtime:read project:read project:write job:run".to_string(),
        created_at: chrono::Utc::now().timestamp(),
        revoked_at: None,
    };
    db.insert_oauth_client(&client).unwrap();
    let plaintext = generate_oauth_access_token();
    let now = chrono::Utc::now().timestamp();
    db.insert_oauth_access_token(&crate::models::OAuthAccessTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash: hash_token(&plaintext),
        client_id: client.client_id.clone(),
        subject_kind: PROJECT_SHARE_OAUTH_SUBJECT_KIND.to_string(),
        subject_id: "wc_pgrant_111111111111111111111111|wc_share_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        user_id: None,
        scopes: "runtime:read project:read project:write job:run".to_string(),
        resource: Some("https://share.example/mcp".to_string()),
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 3600,
        revoked_at: None,
        last_used_at: None,
    })
    .unwrap();

    let ctx = OAuth2Verifier
        .verify(&config, Some(&db), &plaintext)
        .await
        .unwrap()
        .expect("current project-share token should verify");
    assert!(ctx.is_oauth_project_subject());
    assert_eq!(
        ctx.project_grant_id.as_deref(),
        Some("wc_pgrant_111111111111111111111111")
    );
    assert_eq!(
        ctx.token_kind.as_deref(),
        Some(PROJECT_SHARE_OAUTH_TOKEN_KIND)
    );
    assert!(enforce_project_connector_surface(true, &ctx, "/mcp").is_ok());
    for path in crate::route_metadata::iter_routes()
        .filter(|spec| spec.surface == crate::route_metadata::RouteSurface::RunnerTransport)
        .map(|spec| spec.path)
    {
        assert!(enforce_token_surface(&ctx, path).is_err());
    }
}

#[tokio::test]
async fn oauth2_verifier_rejects_stale_project_share_session() {
    let mut config = (*gate_test_config_oauth2(Some("secret"))).clone();
    config.oauth2.project_share_grant_id = Some("wc_pgrant_111111111111111111111111".to_string());
    config.oauth2.project_share_session_id = Some(
        "wc_share_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
    );
    let config = Arc::new(config);
    let (_tmp, db) = gate_test_db();
    let client = crate::models::OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: generate_oauth_client_id(),
        client_secret_hash: hash_token(&generate_oauth_client_secret()),
        name: "project-share".to_string(),
        owner_user_id: None,
        owner_project_grant_id: Some("wc_pgrant_111111111111111111111111".to_string()),
        owner_shared_key_hash: None,
        redirect_uris: "https://client.example/callback".to_string(),
        allowed_scopes: "runtime:read project:read project:write job:run".to_string(),
        created_at: chrono::Utc::now().timestamp(),
        revoked_at: None,
    };
    db.insert_oauth_client(&client).unwrap();
    let plaintext = generate_oauth_access_token();
    let now = chrono::Utc::now().timestamp();
    db.insert_oauth_access_token(&crate::models::OAuthAccessTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash: hash_token(&plaintext),
        client_id: client.client_id,
        subject_kind: PROJECT_SHARE_OAUTH_SUBJECT_KIND.to_string(),
        subject_id: "wc_pgrant_111111111111111111111111|wc_share_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        user_id: None,
        scopes: "runtime:read".to_string(),
        resource: Some("https://share.example/mcp".to_string()),
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 3600,
        revoked_at: None,
        last_used_at: None,
    })
    .unwrap();
    assert!(OAuth2Verifier
        .verify(&config, Some(&db), &plaintext)
        .await
        .is_err());
}

// -----------------------------------------------------------------------
// enforce_token_surface tests
// -----------------------------------------------------------------------

#[test]
fn enforce_token_surface_matrix() {
    let mut account = user_ctx("alice");
    account.kind = AuthKind::AccountCredential;
    let mut oauth2 = user_ctx("alice");
    oauth2.kind = AuthKind::OAuth2Token;
    let runner_transport = [
        "/api/shell/agent/register",
        "/api/shell/agent/offline",
        "/api/shell/agent/poll",
        "/api/shell/agent/result",
        "/api/shell/agent/persistent_shell_result",
        "/api/shell/agent/job_update",
        "/api/agents/ws",
    ];
    let runtime_paths = [
        "/api/runtime/status",
        "/api/tools/list",
        "/api/projects/list",
        "/mcp",
    ];
    let lightweight_account_rejected: Vec<&str> = crate::route_metadata::iter_routes()
        .filter(|spec| spec.surface == crate::route_metadata::RouteSurface::AccountControl)
        .map(|spec| spec.path)
        .collect();
    let mut open_rejected = lightweight_account_rejected.clone();
    open_rejected.extend(
        crate::route_metadata::iter_routes()
            .filter(|spec| spec.surface == crate::route_metadata::RouteSurface::RunnerTransport)
            .map(|spec| spec.path),
    );

    // Rows: (label, ctx, allowed paths, rejected paths, rejection message
    // substring; "" skips the message check). Every rejection must be
    // FORBIDDEN — the surface gate never masquerades as unauthenticated.
    let cases: Vec<(&str, AuthContext, Vec<&str>, Vec<&str>, &str)> = vec![
        (
            "bootstrap",
            bootstrap_ctx(),
            vec![
                "/api/runtime/status",
                "/api/shell/agent/register",
                "/api/users/me",
            ],
            vec![],
            "",
        ),
        (
            "user pat",
            user_ctx("alice"),
            vec![
                "/api/runtime/status",
                "/api/tools/list",
                "/api/projects/list",
            ],
            runner_transport.to_vec(),
            "",
        ),
        (
            "agent token",
            agent_ctx("alice", "laptop", vec![SCOPE_AGENT_REGISTER.to_string()]),
            runner_transport.to_vec(),
            vec![
                "/api/runtime/status",
                "/api/tools/list",
                "/api/projects/list",
                "/mcp",
                "/api/users/me",
            ],
            "agent tokens",
        ),
        (
            "shared key",
            shared_key_context("k"),
            runtime_paths
                .iter()
                .chain(runner_transport.iter())
                .copied()
                .collect(),
            lightweight_account_rejected,
            "",
        ),
        (
            "open anonymous",
            open_anonymous_context(),
            runtime_paths.to_vec(),
            open_rejected,
            "",
        ),
        (
            "account credential",
            account,
            vec![
                "/api/users/me",
                "/api/tokens/list",
                "/api/tokens/register_hash",
                "/api/tokens/revoke",
                "/api/agent-tokens/register_hash",
            ],
            vec![
                "/api/runtime/status",
                "/api/projects/list",
                "/api/tools/list",
                "/mcp",
                "/api/shell/agent/register",
            ],
            "account credentials",
        ),
        (
            "oauth2 token",
            oauth2,
            vec![
                "/api/runtime/status",
                "/api/projects/list",
                "/api/tools/list",
                "/api/jobs/list",
                "/mcp",
            ],
            runner_transport.to_vec(),
            "",
        ),
    ];
    for (label, ctx, allowed, rejected, expected_msg) in &cases {
        for path in allowed {
            assert!(
                enforce_token_surface(ctx, path).is_ok(),
                "{label} should be allowed on {path}"
            );
        }
        for path in rejected {
            let result = enforce_token_surface(ctx, path);
            assert!(result.is_err(), "{label} should be rejected on {path}");
            let (status, msg) = result.unwrap_err();
            assert_eq!(status, StatusCode::FORBIDDEN, "{label} on {path}");
            assert!(
                msg.contains(expected_msg),
                "{label} rejection on {path} should mention '{expected_msg}', got: {msg}"
            );
        }
    }
}

// -----------------------------------------------------------------------
// Shared-key / open-anonymous quick-start auth tests
// -----------------------------------------------------------------------

#[test]
fn shared_key_context_is_non_admin_with_key_hash() {
    let ctx = shared_key_context("abc123");
    assert!(!ctx.is_bootstrap);
    assert!(!ctx.is_admin());
    assert!(ctx.is_shared_key());
    assert!(ctx.is_lightweight());
    assert!(ctx.shared_key_hash.is_some());
    // Same key → same hash (deterministic grouping).
    let ctx2 = shared_key_context("abc123");
    assert_eq!(ctx.shared_key_hash, ctx2.shared_key_hash);
    // Different key → different hash.
    let ctx3 = shared_key_context("xyz789");
    assert_ne!(ctx.shared_key_hash, ctx3.shared_key_hash);
}

#[test]
fn open_anonymous_context_is_non_admin() {
    let ctx = open_anonymous_context();
    assert!(!ctx.is_bootstrap);
    assert!(!ctx.is_admin());
    assert!(ctx.is_open_anonymous());
    assert!(ctx.is_lightweight());
    assert!(ctx.shared_key_hash.is_none());
}

#[test]
fn lightweight_contexts_have_no_admin_scope() {
    let sk = shared_key_context("k");
    assert!(!sk.scopes.iter().any(|s| s == SCOPE_ADMIN));
    let open = open_anonymous_context();
    assert!(!open.scopes.iter().any(|s| s == SCOPE_ADMIN));
    // A direct shared key retains interactive runtime/project access, Computer
    // Use, and the four narrow Runner transport scopes needed by its Runner.
    assert!(sk.scopes.contains(&SCOPE_RUNTIME_READ.to_string()));
    assert!(sk.scopes.contains(&SCOPE_PROJECT_WRITE.to_string()));
    assert!(sk.scopes.contains(&SCOPE_COMPUTER_READ.to_string()));
    assert!(sk.scopes.contains(&SCOPE_COMPUTER_CONTROL.to_string()));
    assert!(
        !sk.scopes.contains(&SCOPE_COMPUTER_LAUNCH.to_string()),
        "direct shared-key quick-start must not implicitly gain application launch authority"
    );
    assert!(
        !sk.scopes.contains(&SCOPE_COMPUTER_DISPLAY_READ.to_string()),
        "direct shared-key quick-start must not implicitly gain full-display authority"
    );
    assert!(
        !sk.scopes
            .contains(&SCOPE_COMPUTER_POINTER_CONTROL.to_string()),
        "direct shared-key quick-start must not implicitly gain pointer authority"
    );
    assert!(!sk
        .scopes
        .contains(&SCOPE_COMPUTER_CLIPBOARD_READ.to_string()));
    assert!(!sk
        .scopes
        .contains(&SCOPE_COMPUTER_CLIPBOARD_WRITE.to_string()));
    for scope in AGENT_SCOPES {
        assert!(sk.scopes.contains(&scope.to_string()));
    }
    // Open anonymous and project credentials do not inherit Computer or Agent
    // transport authority from the direct shared-key quick-start profile.
    assert!(!open.scopes.contains(&SCOPE_COMPUTER_READ.to_string()));
    assert!(!open.scopes.contains(&SCOPE_COMPUTER_CONTROL.to_string()));
    assert!(!open.scopes.contains(&SCOPE_COMPUTER_LAUNCH.to_string()));
    assert!(!open
        .scopes
        .contains(&SCOPE_COMPUTER_DISPLAY_READ.to_string()));
    assert!(!open.scopes.contains(&SCOPE_AGENT_REGISTER.to_string()));
    assert!(!open
        .scopes
        .contains(&SCOPE_COMPUTER_POINTER_CONTROL.to_string()));
    assert!(!open
        .scopes
        .contains(&SCOPE_COMPUTER_CLIPBOARD_READ.to_string()));
    assert!(!open
        .scopes
        .contains(&SCOPE_COMPUTER_CLIPBOARD_WRITE.to_string()));

    let project = crate::auth::shared_key::project_credential_context("wc_pgrant_1111111111111111");
    assert!(project.is_project_credential());
    assert!(!project.is_lightweight());
    assert!(!project.scopes.contains(&SCOPE_COMPUTER_READ.to_string()));
    assert!(!project.scopes.contains(&SCOPE_COMPUTER_CONTROL.to_string()));
    assert!(!project.scopes.contains(&SCOPE_COMPUTER_LAUNCH.to_string()));
    assert!(!project
        .scopes
        .contains(&SCOPE_COMPUTER_DISPLAY_READ.to_string()));
    assert!(!project.scopes.contains(&SCOPE_AGENT_REGISTER.to_string()));
    assert!(!project
        .scopes
        .contains(&SCOPE_COMPUTER_POINTER_CONTROL.to_string()));
    assert!(!project
        .scopes
        .contains(&SCOPE_COMPUTER_CLIPBOARD_READ.to_string()));
    assert!(!project
        .scopes
        .contains(&SCOPE_COMPUTER_CLIPBOARD_WRITE.to_string()));
}

#[test]
fn managed_token_prefix_detected() {
    assert!(is_managed_token_prefix("wc_boot_abc"));
    assert!(is_managed_token_prefix("wc_pat_xyz"));
    assert!(is_managed_token_prefix("wc_agent_123"));
    assert!(is_managed_token_prefix("wc_oat_def"));
    assert!(is_managed_token_prefix("wc_ort_refresh"));
    assert!(!is_managed_token_prefix("abc123"));
    assert!(!is_managed_token_prefix("my-shared-key"));
    assert!(!is_managed_token_prefix("wrong-token"));
    assert!(!is_managed_token_prefix(""));
}

#[tokio::test]
async fn shared_key_fallback_gated_by_env_and_prefix() {
    let env = crate::auth::AuthEnvGuard::auth_required();
    let config = crate::Config {
        addr: "0.0.0.0:8080".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("secret".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    };

    // Shared-key disabled (default): unknown non-wc token → None (reject).
    let r = authenticate_bearer(&config, None, Some("my-key")).await;
    assert!(
        r.is_none(),
        "unknown token should be rejected when shared-key disabled"
    );

    // Shared-key enabled: unknown non-wc token → Some (shared-key context).
    env.enable_direct_shared_key();
    let r = authenticate_bearer(&config, None, Some("my-key")).await;
    assert!(r.is_some(), "non-wc token should be accepted as shared-key");
    let ctx = r.unwrap();
    assert!(ctx.is_shared_key());
    assert!(!ctx.is_admin());

    // Shared-key enabled but any wc_-prefixed invalid managed credential →
    // None (reject), never a shared-key downgrade.
    for token in [
        "wc_pat_invalid",
        "wc_agent_invalid",
        "wc_acct_invalid",
        "wc_oat_invalid",
    ] {
        let r = authenticate_bearer(&config, None, Some(token)).await;
        assert!(r.is_none(), "{token} must not fall back to shared-key");
    }

    // Empty or whitespace-only bearer values must not become sha256("")
    // shared-key groups.
    let r = authenticate_bearer(&config, None, Some("")).await;
    assert!(r.is_none(), "empty token must be rejected");
    let r = authenticate_bearer(&config, None, Some("   \t")).await;
    assert!(r.is_none(), "whitespace token must be rejected");
}

#[tokio::test]
async fn shared_key_fallback_and_oauth_bridge_flags_are_independent() {
    let env = crate::auth::AuthEnvGuard::auth_required();
    let config = crate::Config {
        addr: "0.0.0.0:8080".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("secret".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::from_env(),
    };

    env.enable_oauth2_shared_key_bridge();
    assert!(crate::OAuth2Config::from_env().shared_key_bridge_enabled);
    let r = authenticate_bearer(&config, None, Some("my-key")).await;
    assert!(
        r.is_none(),
        "OAuth bridge flag must not enable direct Bearer shared-key fallback"
    );

    env.disable_oauth2_shared_key_bridge();
    env.enable_direct_shared_key();
    assert!(!crate::OAuth2Config::from_env().shared_key_bridge_enabled);
    let r = authenticate_bearer(&config, None, Some("my-key")).await;
    assert!(
        r.is_some_and(|ctx| ctx.is_shared_key()),
        "direct shared-key fallback must not require OAuth bridge"
    );
}

// -----------------------------------------------------------------------
// authenticate() verifier chain tests
// -----------------------------------------------------------------------

// authenticate() verifier chain tests
// Note: bootstrap and basic PAT verification are covered by the
// pat_verifier_* tests above. The tests below exercise the full chain
// with DB-backed verifiers.

#[tokio::test]
async fn authenticate_returns_none_for_unknown_token_with_db() {
    let config = crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("secret".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    };
    let (_tmp, db) = gate_test_db();
    let result = authenticate(&config, Some(&db), "wc_pat_bogus")
        .await
        .unwrap();
    assert!(result.is_none(), "unknown token should return None");
}

#[tokio::test]
async fn authenticate_returns_api_token_for_valid_user_pat() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let token = gate_mint_user_token(&db, &user);
    let result = authenticate(&config, Some(&db), &token).await.unwrap();
    let ctx = result.expect("should return auth context");
    assert_eq!(ctx.kind, AuthKind::ApiToken);
    assert_eq!(ctx.username.as_deref(), Some("alice"));
    assert!(!ctx.is_bootstrap);
}

#[tokio::test]
async fn authenticate_returns_agent_token_for_valid_agent_token() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let token = gate_mint_agent_token(&db, &user, "alice-laptop");
    let result = authenticate(&config, Some(&db), &token).await.unwrap();
    let ctx = result.expect("should return auth context");
    assert_eq!(ctx.kind, AuthKind::AgentToken);
    assert_eq!(ctx.username.as_deref(), Some("alice"));
    assert_eq!(ctx.allowed_client_id.as_deref(), Some("alice-laptop"));
}

#[tokio::test]
async fn authenticate_returns_account_credential_for_valid_credential() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let credential = gate_mint_account_credential(&db, &user);
    let result = authenticate(&config, Some(&db), &credential).await.unwrap();
    let ctx = result.expect("should return auth context");
    assert_eq!(ctx.kind, AuthKind::AccountCredential);
    assert_eq!(ctx.username.as_deref(), Some("alice"));
}

#[tokio::test]
async fn authenticate_rejects_disabled_user_pat() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let token = gate_mint_user_token(&db, &user);
    db.set_user_disabled(&user.id, true, chrono::Utc::now().timestamp())
        .unwrap();
    let result = authenticate(&config, Some(&db), &token).await;
    assert!(result.is_err(), "disabled user should return Err");
}

#[tokio::test]
async fn authenticate_rejects_expired_token() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let plaintext = generate_api_token();
    let prefix = token_prefix(&plaintext);
    let hash = hash_token(&plaintext);
    let now = chrono::Utc::now().timestamp();
    let record = crate::models::ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        name: "expired".to_string(),
        key_prefix: prefix,
        created_at: now,
        last_used_at: None,
        revoked_at: None,
        scopes: "runtime:read".to_string(),
        expires_at: Some(now - 3600), // expired 1 hour ago
        kind: crate::models::TOKEN_KIND_USER.to_string(),
        allowed_client_id: None,
    };
    db.insert_api_key(&record, &hash).unwrap();
    let result = authenticate(&config, Some(&db), &plaintext).await;
    assert!(result.is_err(), "expired token should return Err");
}

#[tokio::test]
async fn authenticate_oauth2_stub_does_not_break_pat_fallback() {
    // PatVerifier runs before OAuth2Verifier for PAT-shaped tokens, so PAT
    // authentication should remain stable while OAuth2 support is present in
    // the chain.
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let token = gate_mint_user_token(&db, &user);
    // authenticate runs PatVerifier first, which should succeed.
    let result = authenticate(&config, Some(&db), &token).await.unwrap();
    assert!(
        result.is_some(),
        "PAT should still work with OAuth2 verifier in chain"
    );
    let ctx = result.unwrap();
    assert_eq!(ctx.kind, AuthKind::ApiToken);
}

// -----------------------------------------------------------------------
// authenticate_bearer() integration tests (verifier chain for QUIC path)
// -----------------------------------------------------------------------

#[tokio::test]
async fn authenticate_bearer_bootstrap_and_no_token() {
    let _env = AuthEnvGuard::auth_required();
    // Auth disabled → bootstrap.
    let config = crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: None,
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    };
    let ctx = authenticate_bearer(&config, None, Some("anything"))
        .await
        .expect("auth disabled should return bootstrap");
    assert!(ctx.is_bootstrap);

    // Valid bootstrap token → bootstrap.
    let config = crate::Config {
        token: Some("secret".to_string()),
        ..config
    };
    let ctx = authenticate_bearer(&config, None, Some("secret"))
        .await
        .expect("bootstrap token should return bootstrap");
    assert!(ctx.is_bootstrap);

    // No token → None.
    let result = authenticate_bearer(&config, None, None).await;
    assert!(result.is_none(), "no token should return None");
}

// The following authenticate_bearer tests cover QUIC-specific rejection
// that is NOT tested by the authenticate() chain tests above.

#[tokio::test]
async fn authenticate_bearer_rejects_account_credential() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let credential = gate_mint_account_credential(&db, &user);
    let result = authenticate_bearer(&config, Some(&db), Some(&credential)).await;
    assert!(
        result.is_none(),
        "account credentials must be rejected on Runner transport"
    );
}

#[tokio::test]
async fn authenticate_bearer_rejects_oauth2_access_token() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let owner = gate_seed_user(&db, "owner");
    let (client, _secret) = gate_seed_oauth_client(&db, &owner, "Test App");
    let (at, plaintext) =
        gate_seed_shared_key_oauth_access_token(&db, &client, "runtime:read", "test-hash-a");
    assert!(at.last_used_at.is_none(), "precondition");
    let result = authenticate_bearer(&config, Some(&db), Some(&plaintext)).await;
    assert!(
        result.is_none(),
        "OAuth2 access tokens must be rejected on Runner transport (QUIC)"
    );
    // last_used_at must NOT be updated — the token was pre-rejected
    // before OAuth2Verifier ran.
    let conn = db.conn_for_tests();
    let last_used: Option<i64> = conn
        .query_row(
            "SELECT last_used_at FROM oauth_access_tokens WHERE id = ?1",
            [&at.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        last_used.is_none(),
        "last_used_at must not be updated on forbidden surface"
    );
}

// Table-driven: authenticate_bearer positive and negative paths
// that exercise the QUIC/agent-transport surface (not HTTP middleware).
#[tokio::test]
async fn authenticate_bearer_accepts_and_rejects_by_token_type() {
    let _env = AuthEnvGuard::auth_required();
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let user_token = gate_mint_user_token(&db, &user);
    let agent_token = gate_mint_agent_token(&db, &user, "quic-client");

    // Disabled user: user PAT should be rejected.
    let disabled = {
        let now = chrono::Utc::now().timestamp();
        let u = crate::models::UserRecord {
            id: uuid::Uuid::new_v4().to_string(),
            username: "disabled-user".to_string(),
            created_at: now,
            disabled: 1,
            display_name: None,
            role: "user".to_string(),
            disabled_at: Some(now),
            updated_at: Some(now),
        };
        db.create_user(&u).unwrap();
        u
    };
    let disabled_token = gate_mint_user_token(&db, &disabled);

    let cases: Vec<(&str, Option<&str>, bool)> = vec![
        ("valid user PAT", Some(&user_token), true),
        ("valid agent token", Some(&agent_token), true),
        ("unknown token", Some("invalid-garbage-token"), false),
        ("disabled user PAT", Some(&disabled_token), false),
    ];

    for (label, token, should_succeed) in &cases {
        let result = authenticate_bearer(&config, Some(&db), *token).await;
        if *should_succeed {
            assert!(result.is_some(), "{label}: expected Some, got None");
        } else {
            assert!(result.is_none(), "{label}: expected None, got Some");
        }
    }
}

// Regression for the parallel env pollution class: a test that enables
// shared-key or open-anonymous mode under AuthEnvGuard must never leak
// into another test's auth_required window. The toggler thread plays the
// role of "some other test"; every rejection assert below runs inside its
// own auth_required guard. Any future writer that bypasses the shared
// TEST_ENV_LOCK makes this test fail loudly instead of flaking elsewhere.
#[tokio::test]
async fn unknown_token_rejection_survives_concurrent_guarded_mode_toggles() {
    let config = crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("canary-secret".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    };

    let toggler = std::thread::spawn(|| {
        for _ in 0..40 {
            let env = AuthEnvGuard::new();
            env.enable_direct_shared_key();
            env.enable_open_anonymous();
            assert!(shared_key_enabled(), "toggler must see its own enable");
            assert!(allow_anonymous_enabled(), "toggler must see its own enable");
            // Drop restores the pre-toggle values under the same lock.
        }
    });

    for round in 0..40 {
        let _env = AuthEnvGuard::auth_required();
        assert!(
            !shared_key_enabled(),
            "round {round}: shared-key mode leaked into an auth_required window"
        );
        assert!(
            !allow_anonymous_enabled(),
            "round {round}: open-anonymous mode leaked into an auth_required window"
        );
        let unknown = authenticate_bearer(&config, None, Some("canary-unknown-key")).await;
        assert!(
            unknown.is_none(),
            "round {round}: unknown token was accepted as a shared key"
        );
        let missing = authenticate_bearer(&config, None, None).await;
        assert!(
            missing.is_none(),
            "round {round}: missing token was accepted anonymously"
        );
    }

    toggler.join().expect("toggler thread must not panic");
}

// -----------------------------------------------------------------------
// AuthMiddleware integration: OAuth2 access token on HTTP surface
// -----------------------------------------------------------------------

#[tokio::test]
async fn auth_middleware_accepts_valid_oauth2_access_token() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
    let (at, plaintext) = gate_seed_oauth_access_token(&db, &client, &user, "runtime:read");
    assert!(at.last_used_at.is_none(), "precondition");

    let service = salvo::Service::new(gate_router(config, db.clone()));
    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .add_header("authorization", format!("Bearer {}", plaintext), true)
        .send(&service)
        .await;
    assert_eq!(
        resp.status_code,
        Some(StatusCode::OK),
        "valid OAuth2 access token should be accepted by AuthMiddleware"
    );
    // last_used_at MUST be updated on successful verification.
    let conn = db.conn_for_tests();
    let last_used: Option<i64> = conn
        .query_row(
            "SELECT last_used_at FROM oauth_access_tokens WHERE id = ?1",
            [&at.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        last_used.is_some(),
        "last_used_at must be updated on successful HTTP auth"
    );
}

#[tokio::test]
async fn auth_middleware_rejects_refresh_token_as_bearer() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");

    // Create a refresh token.
    let now = chrono::Utc::now().timestamp();
    let plaintext = generate_oauth_refresh_token();
    let token_hash = hash_token(&plaintext);
    let record = crate::models::OAuthRefreshTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash,
        client_id: client.client_id.clone(),
        subject_kind: "managed_user".to_string(),
        subject_id: user.id.clone(),
        user_id: Some(user.id.clone()),
        scopes: "runtime:read".to_string(),
        resource: None,
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 2_592_000,
        revoked_at: None,
        last_used_at: None,
        rotated_from_id: None,
    };
    db.insert_oauth_refresh_token(&record).unwrap();

    let service = salvo::Service::new(gate_router(config, db));
    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .add_header("authorization", format!("Bearer {}", plaintext), true)
        .send(&service)
        .await;
    assert_eq!(
        resp.status_code,
        Some(StatusCode::UNAUTHORIZED),
        "refresh token should not be accepted as bearer"
    );
}

#[tokio::test]
async fn auth_middleware_rejects_bridge_oauth2_on_agent_path_without_updating_last_used() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let owner = gate_seed_user(&db, "owner");
    let (client, _secret) = gate_seed_oauth_client(&db, &owner, "Test App");
    let (at, plaintext) =
        gate_seed_shared_key_oauth_access_token(&db, &client, "runtime:read", "test-hash-a");
    assert_eq!(at.shared_key_hash.as_deref(), Some("test-hash-a"));
    assert!(at.last_used_at.is_none(), "precondition");

    let service = salvo::Service::new(gate_router(config, db.clone()));
    let resp = salvo::test::TestClient::post("http://localhost/api/shell/agent/register")
        .add_header("authorization", format!("Bearer {}", plaintext), true)
        .send(&service)
        .await;
    assert_eq!(
        resp.status_code,
        Some(StatusCode::FORBIDDEN),
        "OAuth2 token on Runner transport path should be 403"
    );
    // last_used_at must NOT be updated — the token was pre-rejected
    // before OAuth2Verifier ran.
    let conn = db.conn_for_tests();
    let last_used: Option<i64> = conn
        .query_row(
            "SELECT last_used_at FROM oauth_access_tokens WHERE id = ?1",
            [&at.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        last_used.is_none(),
        "last_used_at must not be updated on forbidden Runner transport path"
    );
}

async fn gate_oauth2_token_with_scopes(scopes: &str) -> (tempfile::TempDir, Service, String) {
    let config = gate_test_config_oauth2(Some("secret"));
    let (tmp, db) = gate_test_db();
    let owner = gate_seed_user(&db, "owner");
    let (client, _secret) = gate_seed_oauth_client(&db, &owner, "Test App");
    let (_at, plaintext) =
        gate_seed_shared_key_oauth_access_token(&db, &client, scopes, "test-hash-a");
    let service = Service::new(gate_router(config, db));
    (tmp, service, plaintext)
}

fn assert_insufficient_scope(status: StatusCode, body: &serde_json::Value, expected: Option<&str>) {
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {:?}", body);
    assert_eq!(body["error"], "insufficient_scope");
    if let Some(scope) = expected {
        assert!(
            body["error_description"]
                .as_str()
                .unwrap_or("")
                .contains(scope),
            "body: {:?}",
            body
        );
    }
}

#[tokio::test]
async fn oauth2_scope_gate_matrix() {
    // A granted scope opens exactly its own surface…
    for (scopes, path) in [
        ("runtime:read", "/api/runtime/status"),
        ("project:read", "/api/projects/read_file"),
        ("job:run", "/api/projects/run_job"),
    ] {
        let (_tmp, service, token) = gate_oauth2_token_with_scopes(scopes).await;
        let (status, body) = gate_send(&service, path, Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "{scopes} on {path}: {body:?}");
    }
    // …every other surface stays closed with insufficient_scope, including
    // the OAuth authorize page, the Runner transport, and unknown routes
    // (fail closed).
    let insufficient: [(&str, &str, Option<&str>); 6] = [
        (
            "project:read",
            "/api/runtime/status",
            Some(SCOPE_RUNTIME_READ),
        ),
        (
            "runtime:read",
            "/api/projects/read_file",
            Some(SCOPE_PROJECT_READ),
        ),
        (
            "project:write",
            "/api/projects/run_job",
            Some(SCOPE_JOB_RUN),
        ),
        ("runtime:read", "/oauth/authorize", None),
        ("runtime:read", "/api/shell/agent/register", None),
        ("runtime:read", "/api/future/authenticated-route", None),
    ];
    for (scopes, path, expected_scope) in insufficient {
        let (_tmp, service, token) = gate_oauth2_token_with_scopes(scopes).await;
        let (status, body) = gate_send(&service, path, Some(&token)).await;
        assert_insufficient_scope(status, &body, expected_scope);
    }
    // Shared-key-minted OAuth2 tokens can never reach account control,
    // with or without account:manage.
    for scopes in ["account:manage", "runtime:read"] {
        let (_tmp, service, token) = gate_oauth2_token_with_scopes(scopes).await;
        let (status, body) = gate_send(&service, "/api/users/me", Some(&token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{scopes}: {body:?}");
        assert_eq!(
            body["error"].as_str(),
            Some("shared-key principals are not allowed on account control endpoints")
        );
    }
}

#[tokio::test]
async fn managed_oauth2_token_with_account_manage_can_call_users_me() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
    let (_at, token) = gate_seed_oauth_access_token(&db, &client, &user, "account:manage");
    let service = Service::new(gate_router(config, db));
    let (status, body) = gate_send(&service, "/api/users/me", Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "body: {:?}", body);
}

#[tokio::test]
async fn api_token_obeys_declared_scope_and_unknown_route_policy() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let user_token = gate_mint_user_token(&db, &user);
    let runtime_only_token = gate_mint_user_token_with_scopes(&db, &user, SCOPE_RUNTIME_READ);
    let account_token = gate_mint_user_token_with_scopes(
        &db,
        &user,
        &format!("{SCOPE_RUNTIME_READ} {SCOPE_ACCOUNT_MANAGE}"),
    );
    let service = Service::new(gate_router(config, db));

    for path in [
        "/api/runtime/status",
        "/api/projects/read_file",
        "/api/projects/run_job",
    ] {
        let (status, body) = gate_send(&service, path, Some(&user_token)).await;
        assert_eq!(status, StatusCode::OK, "{} body: {:?}", path, body);
    }

    let (status, body) = gate_send(
        &service,
        "/api/projects/read_file",
        Some(&runtime_only_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body:?}");
    assert_ne!(body["error"], "insufficient_scope");
    assert!(body["error"]
        .as_str()
        .unwrap_or("")
        .contains(SCOPE_PROJECT_READ));

    let (status, body) = gate_send(&service, "/api/users/me", Some(&runtime_only_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body:?}");
    assert!(body["error"]
        .as_str()
        .unwrap_or("")
        .contains(SCOPE_ACCOUNT_MANAGE));

    let (status, body) = gate_send(&service, "/api/users/me", Some(&account_token)).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");

    let (status, body) = gate_send(
        &service,
        "/api/future/authenticated-route",
        Some(&runtime_only_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body:?}");
    assert!(body["error"]
        .as_str()
        .unwrap_or("")
        .contains("no declared scope policy"));
}

#[tokio::test]
async fn api_token_without_account_manage_cannot_read_audit_routes() {
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let runtime_only_token = gate_mint_user_token_with_scopes(&db, &user, SCOPE_RUNTIME_READ);
    let service = Service::new(gate_router(config, db));

    for path in [
        "/api/audit/sessions",
        "/api/audit/session",
        "/api/audit/stats",
    ] {
        let mut resp = TestClient::post(format!("http://localhost{path}"))
            .bearer_auth(&runtime_only_token)
            .send(&service)
            .await;
        assert_eq!(gate_status(&resp), StatusCode::FORBIDDEN, "path: {path}");
        assert!(
            !resp.headers.contains_key("www-authenticate"),
            "PAT scope denial must not use OAuth challenge framing on {path}"
        );
        let body = resp.take_json::<serde_json::Value>().await.unwrap();
        assert_ne!(body["error"], "insufficient_scope", "path: {path}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or("")
                .contains(SCOPE_ACCOUNT_MANAGE),
            "path: {path}, body: {body:?}"
        );
    }
}

#[tokio::test]
async fn managed_oauth2_without_account_manage_gets_audit_scope_challenge() {
    let config = gate_test_config_oauth2(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Audit Scope App");
    let (_at, token) = gate_seed_oauth_access_token(&db, &client, &user, SCOPE_RUNTIME_READ);
    let service = Service::new(gate_router(config, db));

    for path in [
        "/api/audit/sessions",
        "/api/audit/session",
        "/api/audit/stats",
    ] {
        let mut resp = TestClient::post(format!("http://localhost{path}"))
            .bearer_auth(&token)
            .send(&service)
            .await;
        assert_eq!(gate_status(&resp), StatusCode::FORBIDDEN, "path: {path}");
        let challenge = resp
            .headers
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(
            challenge.contains("error=\"insufficient_scope\""),
            "OAuth audit scope denial must use insufficient_scope on {path}: {challenge}"
        );
        let body = resp.take_json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["error"], "insufficient_scope", "path: {path}");
        assert!(
            body["error_description"]
                .as_str()
                .unwrap_or("")
                .contains(SCOPE_ACCOUNT_MANAGE),
            "path: {path}, body: {body:?}"
        );
    }
}

// ------------------------------------------------------------------
// WWW-Authenticate resource_metadata in 401 responses
// ------------------------------------------------------------------

#[tokio::test]
async fn auth_middleware_unauthorized_includes_resource_metadata() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let mut config = gate_test_config_oauth2(Some("test-token"));
    Arc::get_mut(&mut config).unwrap().oauth2.issuer =
        Some("https://codex.example.com".to_string());
    let (_tmp, db) = gate_test_db();
    let service = salvo::Service::new(gate_router(config, db));
    // No Authorization header → 401
    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let challenge = resp
        .headers
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        challenge.contains("Bearer"),
        "WWW-Authenticate should contain Bearer, got: {}",
        challenge
    );
    assert!(
        challenge.contains("resource_metadata="),
        "WWW-Authenticate should contain resource_metadata, got: {}",
        challenge
    );
    assert!(
        challenge.contains(".well-known/oauth-protected-resource"),
        "WWW-Authenticate should reference metadata endpoint, got: {}",
        challenge
    );
}

#[tokio::test]
async fn auth_middleware_unauthorized_includes_resource_metadata_with_issuer() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let mut config_inner = gate_test_config_oauth2(Some("test-token"));
    // Set a specific issuer
    Arc::get_mut(&mut config_inner).unwrap().oauth2.issuer =
        Some("https://codex.example.com".to_string());
    let (_tmp, db) = gate_test_db();
    let service = salvo::Service::new(gate_router(config_inner, db));
    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let challenge = resp
        .headers
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        challenge.contains("https://codex.example.com/.well-known/oauth-protected-resource"),
        "WWW-Authenticate should use issuer URL, got: {}",
        challenge
    );
}

#[tokio::test]
async fn auth_middleware_lightweight_empty_and_open_paths() {
    let env = crate::auth::AuthEnvGuard::auth_required();
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let service = salvo::Service::new(gate_router(config.clone(), db.clone()));

    let (status, body) = gate_send(&service, "/api/shell/agent/register", Some("my-key")).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "shared-key disabled must reject Runner transport: {body:?}"
    );

    env.enable_direct_shared_key();
    env.disable_open_anonymous();

    let (status, body) = gate_send(&service, "/api/runtime/status", Some("my-key")).await;
    assert_eq!(status, StatusCode::OK, "body: {:?}", body);
    for path in crate::route_metadata::iter_routes()
        .filter(|spec| spec.surface == crate::route_metadata::RouteSurface::RunnerTransport)
        .map(|spec| spec.path)
    {
        let (status, body) = gate_send(&service, path, Some("my-key")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "direct shared key should reach Runner transport {path}: {body:?}"
        );
    }

    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .add_header("authorization", "Bearer ", true)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));

    let (status, body) = gate_send(&service, "/api/runtime/status", Some("wc_pat_invalid")).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "invalid managed-prefix token must not fall back to shared-key: {:?}",
        body
    );

    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .add_header("authorization", "Bearer    ", true)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));

    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));

    env.enable_open_anonymous();
    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let resp = salvo::test::TestClient::post("http://localhost/api/shell/agent/register")
        .send(&service)
        .await;
    assert_eq!(
        resp.status_code,
        Some(StatusCode::FORBIDDEN),
        "open-anonymous must not register a Runner"
    );
}

#[tokio::test]
async fn auth_middleware_direct_shared_key_and_oauth_bridge_flags_are_independent() {
    let env = crate::auth::AuthEnvGuard::auth_required();
    let config = gate_test_config(Some("secret"));
    let (_tmp, db) = gate_test_db();
    let service = salvo::Service::new(gate_router(config, db));

    env.enable_oauth2_shared_key_bridge();
    assert!(crate::OAuth2Config::from_env().shared_key_bridge_enabled);
    let (status, body) = gate_send(&service, "/api/runtime/status", Some("my-key")).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "OAuth bridge flag must not enable direct Bearer shared-key fallback: {:?}",
        body
    );

    env.disable_oauth2_shared_key_bridge();
    env.enable_direct_shared_key();
    assert!(!crate::OAuth2Config::from_env().shared_key_bridge_enabled);
    let (status, body) = gate_send(&service, "/api/runtime/status", Some("my-key")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "direct shared-key fallback should not require OAuth bridge: {:?}",
        body
    );
    let mut resp = TestClient::post("http://localhost/api/future/authenticated-route")
        .bearer_auth("my-key")
        .send(&service)
        .await;
    assert_eq!(gate_status(&resp), StatusCode::FORBIDDEN);
    assert!(
        !resp.headers.contains_key("www-authenticate"),
        "direct shared-key denial must not emit an OAuth insufficient_scope challenge"
    );
    let body = resp.take_json::<serde_json::Value>().await.unwrap();
    assert_ne!(body["error"], "insufficient_scope");
    assert!(body["error"]
        .as_str()
        .unwrap_or("")
        .contains("no declared scope policy"));
}

#[tokio::test]
async fn auth_middleware_forbidden_uses_insufficient_scope_challenge() {
    let config = gate_test_config_oauth2(Some("test-token"));
    let (_tmp, db) = gate_test_db();
    let user = gate_seed_user(&db, "alice");
    let (client, _secret) = gate_seed_oauth_client(&db, &user, "Test App");
    let (_at, plaintext) = gate_seed_oauth_access_token(&db, &client, &user, "runtime:read");

    let service = salvo::Service::new(gate_router(config, db));
    // OAuth2 token on Runner transport path → 403, not 401
    let resp = salvo::test::TestClient::post("http://localhost/api/shell/agent/register")
        .add_header("authorization", format!("Bearer {}", plaintext), true)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::FORBIDDEN));
    let challenge = resp
        .headers
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        challenge.contains("error=\"insufficient_scope\""),
        "403 should include insufficient_scope challenge, got: {}",
        challenge
    );
    assert!(
        !challenge.contains("resource_metadata="),
        "403 scope challenge should not include resource metadata, got: {}",
        challenge
    );
}

#[tokio::test]
async fn auth_middleware_no_challenge_when_oauth2_disabled() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let config = gate_test_config(Some("test-token"));
    let (_tmp, db) = gate_test_db();
    let service = salvo::Service::new(gate_router(config, db));
    let resp = salvo::test::TestClient::post("http://localhost/api/runtime/status")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    // When OAuth2 is disabled, no WWW-Authenticate challenge
    assert!(
        !resp.headers.contains_key("www-authenticate"),
        "should not include WWW-Authenticate when OAuth2 is disabled"
    );
}
