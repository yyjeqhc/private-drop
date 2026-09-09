use super::*;

pub(super) fn test_config(oauth2: OAuth2Config) -> Arc<crate::Config> {
    Arc::new(crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: Some("bootstrap-token".to_string()),
        max_text_size: 2 * 1024 * 1024,
        oauth2,
    })
}

pub(super) fn oauth2_enabled() -> OAuth2Config {
    OAuth2Config {
        enabled: true,
        require_pkce: true,
        access_token_ttl_secs: 3600,
        refresh_token_ttl_secs: 2_592_000,
        ..OAuth2Config::default()
    }
}

pub(super) fn oauth2_enabled_bridge() -> OAuth2Config {
    OAuth2Config {
        shared_key_bridge_enabled: true,
        ..oauth2_enabled()
    }
}

pub(super) const TEST_PROJECT_GRANT_ID: &str = "wc_pgrant_111111111111111111111111";
pub(super) const TEST_PROJECT_SHARE_SESSION_ID: &str =
    "wc_share_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(super) const TEST_PROJECT_SHARE_SESSION_ID_2: &str =
    "wc_share_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub(super) fn oauth2_enabled_project_share(session_id: &str) -> OAuth2Config {
    OAuth2Config {
        issuer: Some("https://share.example".to_string()),
        project_share_grant_id: Some(TEST_PROJECT_GRANT_ID.to_string()),
        project_share_session_id: Some(session_id.to_string()),
        ..oauth2_enabled()
    }
}

pub(super) fn oauth2_enabled_no_pkce() -> OAuth2Config {
    OAuth2Config {
        enabled: true,
        require_pkce: false,
        access_token_ttl_secs: 3600,
        refresh_token_ttl_secs: 2_592_000,
        ..OAuth2Config::default()
    }
}

pub(super) fn oauth2_enabled_with_issuer(issuer: &str) -> OAuth2Config {
    OAuth2Config {
        issuer: Some(issuer.to_string()),
        ..oauth2_enabled()
    }
}

pub(super) fn oauth2_enabled_no_pkce_with_issuer(issuer: &str) -> OAuth2Config {
    OAuth2Config {
        issuer: Some(issuer.to_string()),
        ..oauth2_enabled_no_pkce()
    }
}

pub(super) fn oauth2_disabled() -> OAuth2Config {
    OAuth2Config {
        enabled: false,
        ..OAuth2Config::default()
    }
}

pub(super) fn test_db() -> (tempfile::TempDir, Arc<crate::Database>) {
    let tmp = tempfile::tempdir().unwrap();
    let db = crate::Database::open(&tmp.path().join("oauth.db")).unwrap();
    (tmp, Arc::new(db))
}

pub(super) fn authorize_query_without(missing: &str) -> String {
    [
        ("response_type", "code"),
        ("client_id", "client-1"),
        ("redirect_uri", "https://client.example/cb"),
        ("scope", "runtime:read"),
        ("state", "keep+this value"),
        ("code_challenge", "challenge-1"),
        ("code_challenge_method", "S256"),
    ]
    .into_iter()
    .filter(|(key, _)| *key != missing)
    .map(|(key, value)| {
        format!(
            "{}={}",
            urlencoding::encode(key),
            urlencoding::encode(value)
        )
    })
    .collect::<Vec<_>>()
    .join("&")
}

pub(super) fn valid_authorize_query() -> String {
    authorize_query_without("")
}
pub(super) fn seed_user(db: &crate::Database, username: &str) -> UserRecord {
    let now = chrono::Utc::now().timestamp();
    let user = UserRecord {
        id: uuid::Uuid::new_v4().to_string(),
        username: username.to_string(),
        created_at: now,
        disabled: 0,
        display_name: None,
        role: "user".to_string(),
        disabled_at: None,
        updated_at: Some(now),
    };
    db.create_user(&user).unwrap();
    user
}

pub(super) fn seed_client(
    db: &crate::Database,
    user: &UserRecord,
    name: &str,
) -> (OAuthClientRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext_secret = crate::auth::generate_oauth_client_secret();
    let secret_hash = hash_token(&plaintext_secret);
    let record = OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: crate::auth::generate_oauth_client_id(),
        client_secret_hash: secret_hash,
        name: name.to_string(),
        owner_user_id: Some(user.id.clone()),
        owner_project_grant_id: None,
        owner_shared_key_hash: None,
        redirect_uris: "https://example.com/callback".to_string(),
        allowed_scopes: "runtime:read project:read".to_string(),
        created_at: now,
        revoked_at: None,
    };
    db.insert_oauth_client(&record).unwrap();
    (record, plaintext_secret)
}

pub(super) fn seed_client_with_redirects_and_scopes(
    db: &crate::Database,
    user: &UserRecord,
    redirect_uris: &str,
    allowed_scopes: &str,
) -> OAuthClientRecord {
    let now = chrono::Utc::now().timestamp();
    let plaintext_secret = crate::auth::generate_oauth_client_secret();
    let secret_hash = hash_token(&plaintext_secret);
    let record = OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: crate::auth::generate_oauth_client_id(),
        client_secret_hash: secret_hash,
        name: "authorize-client".to_string(),
        owner_user_id: Some(user.id.clone()),
        owner_project_grant_id: None,
        owner_shared_key_hash: None,
        redirect_uris: redirect_uris.to_string(),
        allowed_scopes: allowed_scopes.to_string(),
        created_at: now,
        revoked_at: None,
    };
    db.insert_oauth_client(&record).unwrap();
    record
}

pub(super) fn seed_shared_key_bridge_client(
    db: &crate::Database,
    shared_key: &str,
    redirect_uri: &str,
    allowed_scopes: &str,
) -> (OAuthClientRecord, String) {
    let plaintext_secret = crate::auth::generate_oauth_client_secret();
    let record = OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: crate::auth::generate_oauth_client_id(),
        client_secret_hash: hash_token(&plaintext_secret),
        name: "Shared-key Bridge App".to_string(),
        owner_user_id: None,
        owner_project_grant_id: None,
        owner_shared_key_hash: Some(shared_key_hash_of(shared_key)),
        redirect_uris: redirect_uri.to_string(),
        allowed_scopes: allowed_scopes.to_string(),
        created_at: chrono::Utc::now().timestamp(),
        revoked_at: None,
    };
    db.insert_oauth_client(&record).unwrap();
    (record, plaintext_secret)
}

pub(super) fn seed_project_share_client(
    db: &crate::Database,
    grant_id: &str,
    redirect_uri: &str,
) -> (OAuthClientRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext_secret = crate::auth::generate_oauth_client_secret();
    let record = OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: crate::auth::generate_oauth_client_id(),
        client_secret_hash: hash_token(&plaintext_secret),
        name: "project-share-client".to_string(),
        owner_user_id: None,
        owner_project_grant_id: Some(grant_id.to_string()),
        owner_shared_key_hash: None,
        redirect_uris: redirect_uri.to_string(),
        allowed_scopes: crate::auth::PROJECT_SHARE_OAUTH_SCOPES.join(" "),
        created_at: now,
        revoked_at: None,
    };
    db.insert_oauth_client(&record).unwrap();
    (record, plaintext_secret)
}

pub(super) fn seed_user_token(db: &crate::Database, user: &UserRecord) -> String {
    let plaintext = generate_api_token();
    let hash = hash_token(&plaintext);
    let now = chrono::Utc::now().timestamp();
    let record = ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        name: "authorize-user-token".to_string(),
        key_prefix: token_prefix(&plaintext),
        created_at: now,
        last_used_at: None,
        revoked_at: None,
        scopes: "runtime:read project:read project:write job:run".to_string(),
        expires_at: None,
        kind: TOKEN_KIND_USER.to_string(),
        allowed_client_id: None,
    };
    db.insert_api_key(&record, &hash).unwrap();
    plaintext
}

pub(super) fn seed_agent_token(db: &crate::Database, user: &UserRecord) -> String {
    let plaintext = generate_agent_token();
    let hash = hash_token(&plaintext);
    let now = chrono::Utc::now().timestamp();
    let record = ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        name: "authorize-agent-token".to_string(),
        key_prefix: token_prefix(&plaintext),
        created_at: now,
        last_used_at: None,
        revoked_at: None,
        scopes: "agent:poll agent:result".to_string(),
        expires_at: None,
        kind: TOKEN_KIND_AGENT.to_string(),
        allowed_client_id: Some("alice-laptop".to_string()),
    };
    db.insert_api_key(&record, &hash).unwrap();
    plaintext
}

pub(super) fn seed_account_credential(db: &crate::Database, user: &UserRecord) -> String {
    let plaintext = generate_account_credential();
    let hash = hash_token(&plaintext);
    let now = chrono::Utc::now().timestamp();
    let record = AccountCredentialRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        credential_prefix: token_prefix(&plaintext),
        created_at: now,
        last_used_at: None,
        revoked_at: None,
    };
    db.insert_account_credential(&record, &hash).unwrap();
    plaintext
}

pub(super) fn authorize_url(params: &[(&str, &str)]) -> String {
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("http://localhost/oauth/authorize?{}", query)
}

pub(super) fn valid_authorize_url(client: &OAuthClientRecord, redirect_uri: &str) -> String {
    authorize_url(&[
        ("response_type", "code"),
        ("client_id", &client.client_id),
        ("redirect_uri", redirect_uri),
        ("scope", "runtime:read"),
        ("state", "state-1"),
        ("code_challenge", "challenge-1"),
        ("code_challenge_method", "S256"),
    ])
}

pub(super) fn valid_bridge_authorize_url(
    client: &OAuthClientRecord,
    redirect_uri: &str,
    scope: &str,
) -> String {
    authorize_url(&[
        ("bridge", "shared_key"),
        ("response_type", "code"),
        ("client_id", &client.client_id),
        ("redirect_uri", redirect_uri),
        ("scope", scope),
        ("state", "state-1"),
        ("code_challenge", "challenge-1"),
        ("code_challenge_method", "S256"),
    ])
}

pub(super) fn bridge_form_body(
    client: &OAuthClientRecord,
    redirect_uri: &str,
    scope: &str,
    shared_key: &str,
) -> String {
    form_body(&[
        ("bridge", "shared_key"),
        ("response_type", "code"),
        ("client_id", &client.client_id),
        ("redirect_uri", redirect_uri),
        ("scope", scope),
        ("state", "state-1"),
        ("code_challenge", "challenge-1"),
        ("code_challenge_method", "S256"),
        ("shared_key", shared_key),
    ])
}

pub(super) fn valid_authorize_url_with_resource(
    client: &OAuthClientRecord,
    redirect_uri: &str,
    resource: &str,
) -> String {
    authorize_url(&[
        ("response_type", "code"),
        ("client_id", &client.client_id),
        ("redirect_uri", redirect_uri),
        ("scope", "runtime:read"),
        ("state", "state-1"),
        ("code_challenge", "challenge-1"),
        ("code_challenge_method", "S256"),
        ("resource", resource),
    ])
}

pub(super) fn authorized_get(url: &str, token: &str) -> salvo::test::RequestBuilder {
    TestClient::get(url).add_header("authorization", format!("Bearer {}", token), true)
}

pub(super) fn auth_code_count(db: &crate::Database) -> i64 {
    let conn = db.conn_for_tests();
    conn.query_row(
        "SELECT COUNT(*) FROM oauth_authorization_codes",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

pub(super) fn location_header(resp: &Response) -> Option<String> {
    resp.headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

pub(super) fn assert_no_location(resp: &Response) {
    assert!(
        location_header(resp).is_none(),
        "direct errors must not include Location"
    );
}

pub(super) async fn assert_authorize_direct_400(
    service: &Service,
    db: &crate::Database,
    url: &str,
    token: &str,
) {
    let before = auth_code_count(db);
    let resp = authorized_get(url, token).send(service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    assert_no_location(&resp);
    assert_eq!(auth_code_count(db), before);
}

pub(super) async fn assert_authorize_redirect_error(
    service: &Service,
    db: &crate::Database,
    url: &str,
    token: &str,
    expected_base: &str,
    expected_error: &str,
    expected_state: Option<&str>,
) -> String {
    let before = auth_code_count(db);
    let resp = authorized_get(url, token).send(service).await;
    assert_eq!(resp.status_code, Some(StatusCode::FOUND));
    assert_eq!(auth_code_count(db), before);
    let location = location_header(&resp).expect("redirect error should set Location");
    assert!(
        location.starts_with(expected_base),
        "Location {} should start with {}",
        location,
        expected_base
    );
    let parsed = url::Url::parse(&location).unwrap();
    let params: std::collections::HashMap<String, String> =
        parsed.query_pairs().into_owned().collect();
    assert_eq!(
        params.get("error").map(String::as_str),
        Some(expected_error)
    );
    if let Some(expected_state) = expected_state {
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(expected_state)
        );
    } else {
        assert!(!params.contains_key("state"));
    }
    location
}

pub(super) async fn authorize_success(
    service: &Service,
    db: &crate::Database,
    url: &str,
    token: &str,
) -> (Response, String, url::Url, String) {
    let before = auth_code_count(db);
    let resp = authorized_get(url, token).send(service).await;
    assert_eq!(resp.status_code, Some(StatusCode::FOUND));
    assert_eq!(auth_code_count(db), before + 1);
    let location = location_header(&resp).expect("success should set Location");
    let parsed = url::Url::parse(&location).unwrap();
    let params: std::collections::HashMap<String, String> =
        parsed.query_pairs().into_owned().collect();
    let code = params
        .get("code")
        .expect("success redirect should include code")
        .clone();
    assert!(code.starts_with("wc_oac_"));
    assert!(!params.contains_key("access_token"));
    assert!(!params.contains_key("refresh_token"));
    (resp, location, parsed, code)
}

pub(super) fn auth_code_by_plaintext(
    db: &crate::Database,
    plaintext_code: &str,
) -> OAuthAuthorizationCodeRecord {
    db.get_oauth_authorization_code_by_hash(&hash_token(plaintext_code))
        .unwrap()
        .expect("authorization code row should exist")
}

pub(super) fn pkce_s256_challenge(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

pub(super) fn seed_project_share_auth_code(
    db: &crate::Database,
    client: &OAuthClientRecord,
    grant_id: &str,
    session_id: &str,
    redirect_uri: &str,
    scopes: &str,
    code_challenge: Option<&str>,
) -> (OAuthAuthorizationCodeRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext_code = generate_oauth_authorization_code();
    let record = OAuthAuthorizationCodeRecord {
        id: uuid::Uuid::new_v4().to_string(),
        code_hash: hash_token(&plaintext_code),
        client_id: client.client_id.clone(),
        subject_kind: crate::auth::PROJECT_SHARE_OAUTH_SUBJECT_KIND.to_string(),
        subject_id: format!("{grant_id}|{session_id}"),
        user_id: None,
        redirect_uri: redirect_uri.to_string(),
        scopes: scopes.to_string(),
        resource: Some("https://share.example/mcp".to_string()),
        code_challenge: code_challenge.map(str::to_string),
        code_challenge_method: code_challenge.map(|_| "S256".to_string()),
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 300,
        used_at: None,
        revoked_at: None,
    };
    db.insert_oauth_authorization_code(&record, &record.code_hash)
        .unwrap();
    (record, plaintext_code)
}

pub(super) fn seed_auth_code(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    redirect_uri: &str,
    scopes: &str,
    code_challenge: Option<&str>,
    code_challenge_method: Option<&str>,
) -> (OAuthAuthorizationCodeRecord, String) {
    seed_auth_code_with_resource(
        db,
        client,
        user,
        redirect_uri,
        scopes,
        code_challenge,
        code_challenge_method,
        None,
    )
}

pub(super) fn seed_auth_code_with_resource(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    redirect_uri: &str,
    scopes: &str,
    code_challenge: Option<&str>,
    code_challenge_method: Option<&str>,
    resource: Option<&str>,
) -> (OAuthAuthorizationCodeRecord, String) {
    seed_auth_code_with_resource_and_shared_key_hash(
        db,
        client,
        user,
        redirect_uri,
        scopes,
        code_challenge,
        code_challenge_method,
        resource,
        None,
    )
}

pub(super) fn seed_auth_code_with_shared_key_hash(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    redirect_uri: &str,
    scopes: &str,
    shared_key_hash: &str,
) -> (OAuthAuthorizationCodeRecord, String) {
    seed_auth_code_with_resource_and_shared_key_hash(
        db,
        client,
        user,
        redirect_uri,
        scopes,
        None,
        None,
        None,
        Some(shared_key_hash),
    )
}

pub(super) fn seed_auth_code_with_resource_and_shared_key_hash(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    redirect_uri: &str,
    scopes: &str,
    code_challenge: Option<&str>,
    code_challenge_method: Option<&str>,
    resource: Option<&str>,
    shared_key_hash: Option<&str>,
) -> (OAuthAuthorizationCodeRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext_code = generate_oauth_authorization_code();
    let code_hash = hash_token(&plaintext_code);
    let record = OAuthAuthorizationCodeRecord {
        id: uuid::Uuid::new_v4().to_string(),
        code_hash,
        client_id: client.client_id.clone(),
        subject_kind: shared_key_hash
            .map(|_| "shared_key")
            .unwrap_or("managed_user")
            .to_string(),
        subject_id: shared_key_hash.unwrap_or(&user.id).to_string(),
        user_id: shared_key_hash
            .map(|_| ())
            .map_or(Some(user.id.clone()), |_| None),
        redirect_uri: redirect_uri.to_string(),
        scopes: scopes.to_string(),
        code_challenge: code_challenge.map(str::to_string),
        code_challenge_method: code_challenge_method.map(str::to_string),
        resource: resource.map(str::to_string),
        shared_key_hash: shared_key_hash.map(str::to_string),
        created_at: now,
        expires_at: now + 300,
        used_at: None,
        revoked_at: None,
    };
    db.insert_oauth_authorization_code(&record, &record.code_hash)
        .unwrap();
    (record, plaintext_code)
}

/// Seed a refresh token directly into the database. Returns the record
/// and the plaintext token.
pub(super) fn seed_refresh_token(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    scopes: &str,
) -> (crate::models::OAuthRefreshTokenRecord, String) {
    seed_refresh_token_with_resource(db, client, user, scopes, None)
}

pub(super) fn seed_refresh_token_with_resource(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    scopes: &str,
    resource: Option<&str>,
) -> (crate::models::OAuthRefreshTokenRecord, String) {
    seed_refresh_token_with_resource_and_shared_key_hash(db, client, user, scopes, resource, None)
}

pub(super) fn seed_refresh_token_with_shared_key_hash(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    scopes: &str,
    shared_key_hash: &str,
) -> (crate::models::OAuthRefreshTokenRecord, String) {
    seed_refresh_token_with_resource_and_shared_key_hash(
        db,
        client,
        user,
        scopes,
        None,
        Some(shared_key_hash),
    )
}

pub(super) fn seed_refresh_token_with_resource_and_shared_key_hash(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    scopes: &str,
    resource: Option<&str>,
    shared_key_hash: Option<&str>,
) -> (crate::models::OAuthRefreshTokenRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext = crate::auth::generate_oauth_refresh_token();
    let token_hash = hash_token(&plaintext);
    let record = crate::models::OAuthRefreshTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash,
        client_id: client.client_id.clone(),
        subject_kind: shared_key_hash
            .map(|_| "shared_key")
            .unwrap_or("managed_user")
            .to_string(),
        subject_id: shared_key_hash.unwrap_or(&user.id).to_string(),
        user_id: shared_key_hash
            .map(|_| ())
            .map_or(Some(user.id.clone()), |_| None),
        scopes: scopes.to_string(),
        resource: resource.map(str::to_string),
        shared_key_hash: shared_key_hash.map(str::to_string),
        created_at: now,
        expires_at: now + 2_592_000, // 30 days
        revoked_at: None,
        last_used_at: None,
        rotated_from_id: None,
    };
    db.insert_oauth_refresh_token(&record).unwrap();
    (record, plaintext)
}

/// Seed an access token directly into the database. Returns the record
/// and the plaintext token.
pub(super) fn seed_access_token(
    db: &crate::Database,
    client: &OAuthClientRecord,
    user: &UserRecord,
    scopes: &str,
) -> (crate::models::OAuthAccessTokenRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext = crate::auth::generate_oauth_access_token();
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
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 3600, // 1 hour
        revoked_at: None,
        last_used_at: None,
    };
    db.insert_oauth_access_token(&record).unwrap();
    (record, plaintext)
}

#[handler]
pub(super) async fn test_agent_register_handler(res: &mut Response) {
    res.render(Json(serde_json::json!({"ok": true})));
}

pub(super) fn build_router(config: Arc<crate::Config>, db: Arc<crate::Database>) -> Router {
    let session_store = Arc::new(AuthorizeSessionStore::new());
    build_router_with_session_and_registry(
        config,
        db,
        session_store,
        Arc::new(crate::RunnerRegistry::default()),
    )
}

pub(super) fn build_router_with_session_and_registry(
    config: Arc<crate::Config>,
    db: Arc<crate::Database>,
    session_store: Arc<AuthorizeSessionStore>,
    registry: Arc<crate::RunnerRegistry>,
) -> Router {
    Router::new()
        .hoop(salvo::prelude::affix_state::inject(config))
        .hoop(salvo::prelude::affix_state::inject(db))
        .hoop(salvo::prelude::affix_state::inject(session_store))
        .hoop(salvo::prelude::affix_state::inject(registry))
        .push(Router::with_path("oauth/token").post(oauth_token))
        .push(Router::with_path("oauth/revoke").post(oauth_revoke))
        .push(
            Router::with_path("oauth/authorize")
                .get(oauth_authorize)
                .push(Router::with_path("login").post(oauth_authorize_login))
                .push(Router::with_path("consent").post(oauth_authorize_consent))
                .push(Router::with_path("bridge").post(oauth_authorize_bridge))
                .push(Router::with_path("project").post(oauth_authorize_project)),
        )
        .push(
            Router::with_path("api/oauth/clients")
                .hoop(crate::AuthMiddleware)
                .push(Router::with_path("create").post(oauth_clients_create))
                .push(Router::with_path("list").post(oauth_clients_list))
                .push(Router::with_path("update_scopes").post(oauth_clients_update_scopes))
                .push(Router::with_path("revoke").post(oauth_clients_revoke)),
        )
        .push(
            Router::with_path("api/oauth/shared-key-client/provision")
                .hoop(crate::AuthMiddleware)
                .post(oauth_shared_key_client_provision),
        )
        .push(
            Router::with_path("api/shell/agent/register")
                .hoop(crate::AuthMiddleware)
                .post(test_agent_register_handler),
        )
        .push(Router::with_path(".well-known/oauth-protected-resource").get(oauth_metadata))
        .push(
            Router::with_path(".well-known/oauth-authorization-server")
                .get(oauth_authorization_server_metadata),
        )
}

pub(super) fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build a POST request with the correct Content-Type header.
pub(super) fn post_form(url: &str, body: String) -> salvo::test::RequestBuilder {
    TestClient::post(url)
        .add_header("content-type", "application/x-www-form-urlencoded", true)
        .body(body)
}

/// Return `(access_token_count, refresh_token_count)` from the DB.
pub(super) fn oauth_token_counts(db: &crate::Database) -> (i64, i64) {
    let conn = db.conn_for_tests();
    let at: i64 = conn
        .query_row("SELECT COUNT(*) FROM oauth_access_tokens", [], |row| {
            row.get(0)
        })
        .unwrap();
    let rt: i64 = conn
        .query_row("SELECT COUNT(*) FROM oauth_refresh_tokens", [], |row| {
            row.get(0)
        })
        .unwrap();
    (at, rt)
}

pub(super) fn access_token_resource_by_plaintext(
    db: &crate::Database,
    plaintext_token: &str,
) -> Option<String> {
    db.conn_for_tests()
        .query_row(
            "SELECT resource FROM oauth_access_tokens WHERE token_hash = ?1",
            [&hash_token(plaintext_token)],
            |row| row.get(0),
        )
        .unwrap()
}

pub(super) fn access_token_shared_key_hash_by_plaintext(
    db: &crate::Database,
    plaintext_token: &str,
) -> Option<String> {
    db.conn_for_tests()
        .query_row(
            "SELECT shared_key_hash FROM oauth_access_tokens WHERE token_hash = ?1",
            [&hash_token(plaintext_token)],
            |row| row.get(0),
        )
        .unwrap()
}

pub(super) fn access_token_subject_by_plaintext(
    db: &crate::Database,
    plaintext_token: &str,
) -> (String, String, Option<String>, Option<String>) {
    db.conn_for_tests()
            .query_row(
                "SELECT subject_kind, subject_id, user_id, shared_key_hash FROM oauth_access_tokens WHERE token_hash = ?1",
                [&hash_token(plaintext_token)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
}

pub(super) fn refresh_token_resource_by_plaintext(
    db: &crate::Database,
    plaintext_token: &str,
) -> Option<String> {
    db.conn_for_tests()
        .query_row(
            "SELECT resource FROM oauth_refresh_tokens WHERE token_hash = ?1",
            [&hash_token(plaintext_token)],
            |row| row.get(0),
        )
        .unwrap()
}

pub(super) fn refresh_token_shared_key_hash_by_plaintext(
    db: &crate::Database,
    plaintext_token: &str,
) -> Option<String> {
    db.conn_for_tests()
        .query_row(
            "SELECT shared_key_hash FROM oauth_refresh_tokens WHERE token_hash = ?1",
            [&hash_token(plaintext_token)],
            |row| row.get(0),
        )
        .unwrap()
}

pub(super) fn refresh_token_subject_by_plaintext(
    db: &crate::Database,
    plaintext_token: &str,
) -> (String, String, Option<String>, Option<String>) {
    db.conn_for_tests()
            .query_row(
                "SELECT subject_kind, subject_id, user_id, shared_key_hash FROM oauth_refresh_tokens WHERE token_hash = ?1",
                [&hash_token(plaintext_token)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
}
