use super::*;

const MCP_IMPORT_TRUSTED_REDIRECT: &str = "https://chatgpt.example/connector/oauth/webcodex-test";

struct McpImportStartupEnvGuard {
    _env_lock: std::sync::MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl McpImportStartupEnvGuard {
    fn new() -> Self {
        const NAMES: &[&str] = &[
            "WEBCODEX_ENV_FILE",
            "WEBCODEX_TOKEN",
            "WEBCODEX_OAUTH2_ENABLED",
            "WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS",
        ];
        let env_lock = crate::admin_cli::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = NAMES
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        Self {
            _env_lock: env_lock,
            previous,
        }
    }

    fn set(&self, name: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        std::env::set_var(name, value);
    }
}

impl Drop for McpImportStartupEnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn mcp_import_config_from_startup_env(
    trusted_client_id: &str,
    env_file_client_id: &str,
) -> Arc<crate::Config> {
    let guard = McpImportStartupEnvGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let env_file = dir.path().join("webcodex.env");
    std::fs::write(
        &env_file,
        format!(
            "WEBCODEX_OAUTH2_ENABLED=false\nWEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS={env_file_client_id}\n"
        ),
    )
    .unwrap();

    // Production startup loads env files first, but an already-present process
    // environment is authoritative and load_env_file deliberately does not
    // replace it. This matches the live deployment shape being debugged.
    guard.set("WEBCODEX_ENV_FILE", &env_file);
    guard.set("WEBCODEX_TOKEN", "startup-env-bootstrap-token");
    guard.set("WEBCODEX_OAUTH2_ENABLED", "true");
    guard.set(
        "WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS",
        trusted_client_id,
    );
    let loads = crate::config::load_startup_env_files().unwrap();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0].path, env_file);
    assert_eq!(
        loads[0].loaded_count, 0,
        "explicit env file must not override already-present OAuth trust settings"
    );

    let config = Arc::new(crate::Config::from_env());
    assert!(config.oauth2.enabled);
    assert_eq!(
        config.oauth2.trusted_mcp_file_client_ids,
        vec![trusted_client_id.to_string()]
    );
    config
}

async fn lock_mcp_import_test() -> tokio::sync::MutexGuard<'static, ()> {
    crate::tool_runtime::conversation_import::lock_import_test_network().await
}

fn run_mcp_import_in_large_stack_test_thread<F, Fut>(test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    // These end-to-end import tests retain the OAuth/router/import fixture across
    // several awaits. Keep their larger stack local to this test group instead
    // of raising RUST_MIN_STACK for the whole suite.
    let result = std::thread::Builder::new()
        .name("mcp-file-import-test".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build isolated file-import test runtime")
                .block_on(test());
        })
        .expect("spawn isolated file-import test thread")
        .join();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

struct McpImportNetworkOverride;

impl McpImportNetworkOverride {
    fn set(base_url: String) -> Self {
        crate::tool_runtime::conversation_import::set_import_test_download_base_url(Some(base_url));
        crate::tool_runtime::conversation_import::set_import_test_resolved_ips(Some(vec![
            "8.8.8.8".parse().unwrap(),
        ]));
        crate::tool_runtime::conversation_import::reset_import_test_dns_resolution_count();
        Self
    }

    fn without_download() -> Self {
        crate::tool_runtime::conversation_import::set_import_test_download_base_url(None);
        crate::tool_runtime::conversation_import::set_import_test_resolved_ips(Some(vec![
            "8.8.8.8".parse().unwrap(),
        ]));
        crate::tool_runtime::conversation_import::reset_import_test_dns_resolution_count();
        Self
    }
}

impl Drop for McpImportNetworkOverride {
    fn drop(&mut self) {
        crate::tool_runtime::conversation_import::set_import_test_download_base_url(None);
        crate::tool_runtime::conversation_import::set_import_test_resolved_ips(None);
        crate::tool_runtime::conversation_import::reset_import_test_dns_resolution_count();
    }
}

struct McpImportMockServer {
    base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for McpImportMockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn mcp_import_http_response(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {status}\r\n").into_bytes();
    for (name, value) in headers {
        response.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    response.extend_from_slice(b"\r\n");
    response.extend_from_slice(body);
    response
}

async fn start_mcp_import_mock_server(response: Vec<u8>) -> McpImportMockServer {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0_u8; 4096];
        let _ = stream.read(&mut buf).await;
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
    });
    McpImportMockServer {
        base_url: format!("http://{addr}"),
        handle,
    }
}

fn mcp_import_config(trusted_client_ids: &[&str]) -> Arc<crate::Config> {
    let mut config = (*test_config_oauth2(Some("secret"))).clone();
    config.oauth2.trusted_mcp_file_client_ids = trusted_client_ids
        .iter()
        .map(|value| (*value).to_string())
        .collect();
    Arc::new(config)
}

fn seed_mcp_import_client(
    db: &crate::Database,
    user: &crate::models::UserRecord,
    name: &str,
    redirect_uris: &str,
) -> crate::models::OAuthClientRecord {
    let record = crate::models::OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: crate::auth::generate_oauth_client_id(),
        client_secret_hash: crate::auth::hash_token("test-client-secret"),
        name: name.to_string(),
        owner_user_id: Some(user.id.clone()),
        owner_project_grant_id: None,
        owner_shared_key_hash: None,
        redirect_uris: redirect_uris.to_string(),
        allowed_scopes: "project:write".to_string(),
        created_at: chrono::Utc::now().timestamp(),
        revoked_at: None,
    };
    db.insert_oauth_client(&record).unwrap();
    record
}

fn seed_mcp_import_pat(db: &crate::Database, user: &crate::models::UserRecord) -> String {
    let plaintext = crate::auth::generate_api_token();
    let record = crate::models::ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        name: "mcp-import-client-manager".to_string(),
        key_prefix: crate::auth::token_prefix(&plaintext),
        created_at: chrono::Utc::now().timestamp(),
        last_used_at: None,
        revoked_at: None,
        scopes: "runtime:read project:read project:write job:run account:manage".to_string(),
        expires_at: None,
        kind: crate::models::TOKEN_KIND_USER.to_string(),
        allowed_client_id: None,
    };
    db.insert_api_key(&record, &crate::auth::hash_token(&plaintext))
        .unwrap();
    plaintext
}

fn mcp_import_oauth_auth(client_id: &str) -> crate::auth::AuthContext {
    let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token);
    auth.username = Some("alice".to_string());
    auth.token_kind = Some("oauth2".to_string());
    auth.scopes = vec![crate::auth::SCOPE_PROJECT_WRITE.to_string()];
    auth.allowed_client_id = Some(client_id.to_string());
    auth
}

async fn mcp_import_runtime(
    root: &std::path::Path,
    owner: Option<&str>,
) -> (Arc<ToolRuntime>, Arc<crate::runner_http::RunnerRegistry>) {
    mcp_import_runtime_with_surface(root, owner, ModelSurface::FullOperatorRuntime).await
}

async fn mcp_import_runtime_with_surface(
    root: &std::path::Path,
    owner: Option<&str>,
    model_surface: ModelSurface,
) -> (Arc<ToolRuntime>, Arc<crate::runner_http::RunnerRegistry>) {
    use crate::runner_protocol::{RunnerCapabilities, RunnerProjectSummary, RunnerRegisterRequest};
    let registry = Arc::new(crate::runner_http::RunnerRegistry::default());
    registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: "importer".to_string(),
                runner_instance_id: "inst-import".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: owner.map(str::to_string),
                hostname: None,
                host_context: None,
                capabilities: RunnerCapabilities {
                    file_write: true,
                    ..Default::default()
                },
                policy: None,
            },
        ))
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &registry,
        "importer",
        "inst-import",
        vec![RunnerProjectSummary {
            id: "demo".to_string(),
            name: Some("demo".to_string()),
            path: root.to_string_lossy().into_owned(),
            allow_patch: true,
            kind: None,
            registration_source: None,
            description: None,
            hooks: Vec::new(),
            disabled: false,
            revision: None,
            git_branch: None,
            git_head: None,
            git_dirty: None,
            updated_at: chrono::Utc::now().timestamp(),
            shell_profile: None,
        }],
    )
    .await;
    let runtime = Arc::new(
        ToolRuntime::new_for_tests_with_runner_registry(registry.clone())
            .with_model_surface(model_surface),
    );
    (runtime, registry)
}

async fn complete_mcp_import_save(
    registry: Arc<crate::runner_http::RunnerRegistry>,
    expected_bytes: Vec<u8>,
) {
    use crate::runner_protocol::{RunnerPollRequest, RunnerResultRequest};
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    async fn next_request(
        registry: &crate::runner_http::RunnerRegistry,
    ) -> crate::runner_protocol::RunnerRequest {
        loop {
            if let Some(request) = registry
                .poll(RunnerPollRequest {
                    client_id: "importer".to_string(),
                    runner_instance_id: "inst-import".to_string(),
                })
                .await
                .unwrap()
            {
                return request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    let request = next_request(&registry).await;
    assert_eq!(request.kind, "file_artifact_upload_begin");
    let begin: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
    assert!(begin.get("download_url").is_none());
    assert!(begin.get("download_link").is_none());
    assert!(begin.get("openaiFileIdRefs").is_none());
    assert_eq!(
        begin["max_bytes"],
        crate::tool_runtime::files::MAX_PROJECT_ARTIFACT_UPLOAD_BYTES
    );
    assert_eq!(begin["expected_bytes"], expected_bytes.len());
    let path = begin["path"].as_str().unwrap().to_string();
    let mime_type = begin["mime_type"].as_str().unwrap().to_string();
    let upload_id = "wc_upload_mcp_import_fixture";
    registry
        .complete(RunnerResultRequest {
            client_id: "importer".to_string(),
            runner_instance_id: "inst-import".to_string(),
            request_id: request.request_id,
            exit_code: Some(0),
            stdout: Some(
                json!({
                    "path": path,
                    "upload_id": upload_id,
                    "received_bytes": 0,
                    "next_offset": 0,
                    "expected_bytes": expected_bytes.len(),
                    "expected_sha256": null,
                    "max_bytes": crate::tool_runtime::files::MAX_PROJECT_ARTIFACT_UPLOAD_BYTES,
                    "mime_type": mime_type,
                    "committed": false
                })
                .to_string(),
            ),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();

    let mut bytes = Vec::new();
    loop {
        let request = next_request(&registry).await;
        let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
        assert_eq!(payload["path"], path);
        assert_eq!(payload["upload_id"], upload_id);
        match request.kind.as_str() {
            "file_artifact_upload_chunk" => {
                assert_eq!(
                    payload["max_chunk_bytes"],
                    crate::tool_runtime::files::MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES
                );
                assert_eq!(payload["offset"], bytes.len());
                let chunk = base64::engine::general_purpose::STANDARD
                    .decode(payload["content_base64"].as_str().unwrap())
                    .unwrap();
                assert!(!chunk.is_empty());
                assert!(
                    chunk.len()
                        <= crate::tool_runtime::files::MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES
                );
                bytes.extend_from_slice(&chunk);
                registry
                    .complete(RunnerResultRequest {
                        client_id: "importer".to_string(),
                        runner_instance_id: "inst-import".to_string(),
                        request_id: request.request_id,
                        exit_code: Some(0),
                        stdout: Some(
                            json!({
                                "path": path,
                                "upload_id": upload_id,
                                "received_bytes": bytes.len(),
                                "next_offset": bytes.len(),
                                "expected_bytes": expected_bytes.len(),
                                "expected_sha256": null,
                                "max_bytes": crate::tool_runtime::files::MAX_PROJECT_ARTIFACT_UPLOAD_BYTES,
                                "mime_type": mime_type,
                                "committed": false
                            })
                            .to_string(),
                        ),
                        stderr: None,
                        duration_ms: Some(1),
                        error: None,
                    })
                    .await
                    .unwrap();
            }
            "file_artifact_upload_finish" => {
                assert_eq!(bytes, expected_bytes);
                let sha256 = format!("{:x}", Sha256::digest(&bytes));
                registry
                    .complete(RunnerResultRequest {
                        client_id: "importer".to_string(),
                        runner_instance_id: "inst-import".to_string(),
                        request_id: request.request_id,
                        exit_code: Some(0),
                        stdout: Some(
                            json!({
                                "path": path,
                                "upload_id": upload_id,
                                "bytes": bytes.len(),
                                "received_bytes": bytes.len(),
                                "expected_bytes": expected_bytes.len(),
                                "expected_sha256": null,
                                "sha256": sha256,
                                "mime_type": mime_type,
                                "committed": true
                            })
                            .to_string(),
                        ),
                        stderr: None,
                        duration_ms: Some(1),
                        error: None,
                    })
                    .await
                    .unwrap();
                break;
            }
            other => panic!("unexpected MCP import artifact request: {other}"),
        }
    }
}

async fn complete_mcp_import_until_abort(
    registry: Arc<crate::runner_http::RunnerRegistry>,
) -> usize {
    use crate::runner_protocol::{RunnerPollRequest, RunnerResultRequest};
    use base64::Engine as _;

    async fn next_request(
        registry: &crate::runner_http::RunnerRegistry,
    ) -> crate::runner_protocol::RunnerRequest {
        loop {
            if let Some(request) = registry
                .poll(RunnerPollRequest {
                    client_id: "importer".to_string(),
                    runner_instance_id: "inst-import".to_string(),
                })
                .await
                .unwrap()
            {
                return request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    let request = next_request(&registry).await;
    assert_eq!(request.kind, "file_artifact_upload_begin");
    let begin: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
    assert!(begin["expected_bytes"].is_null());
    let path = begin["path"].as_str().unwrap().to_string();
    let mime_type = begin["mime_type"].as_str().unwrap().to_string();
    let upload_id = "wc_upload_mcp_abort_fixture";
    registry
        .complete(RunnerResultRequest {
            client_id: "importer".to_string(),
            runner_instance_id: "inst-import".to_string(),
            request_id: request.request_id,
            exit_code: Some(0),
            stdout: Some(
                json!({
                    "path": path,
                    "upload_id": upload_id,
                    "received_bytes": 0,
                    "next_offset": 0,
                    "expected_bytes": null,
                    "expected_sha256": null,
                    "max_bytes": crate::tool_runtime::files::MAX_PROJECT_ARTIFACT_UPLOAD_BYTES,
                    "mime_type": mime_type,
                    "committed": false
                })
                .to_string(),
            ),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();

    let mut received_bytes = 0usize;
    let mut chunk_count = 0usize;
    loop {
        let request = next_request(&registry).await;
        let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
        assert_eq!(payload["path"], path);
        assert_eq!(payload["upload_id"], upload_id);
        match request.kind.as_str() {
            "file_artifact_upload_chunk" => {
                assert_eq!(payload["offset"], received_bytes);
                let chunk = base64::engine::general_purpose::STANDARD
                    .decode(payload["content_base64"].as_str().unwrap())
                    .unwrap();
                assert!(!chunk.is_empty());
                assert!(
                    chunk.len()
                        <= crate::tool_runtime::files::MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES
                );
                received_bytes += chunk.len();
                chunk_count += 1;
                registry
                    .complete(RunnerResultRequest {
                        client_id: "importer".to_string(),
                        runner_instance_id: "inst-import".to_string(),
                        request_id: request.request_id,
                        exit_code: Some(0),
                        stdout: Some(
                            json!({
                                "path": path,
                                "upload_id": upload_id,
                                "received_bytes": received_bytes,
                                "next_offset": received_bytes,
                                "expected_bytes": null,
                                "expected_sha256": null,
                                "max_bytes": crate::tool_runtime::files::MAX_PROJECT_ARTIFACT_UPLOAD_BYTES,
                                "mime_type": mime_type,
                                "committed": false
                            })
                            .to_string(),
                        ),
                        stderr: None,
                        duration_ms: Some(1),
                        error: None,
                    })
                    .await
                    .unwrap();
            }
            "file_artifact_upload_abort" => {
                registry
                    .complete(RunnerResultRequest {
                        client_id: "importer".to_string(),
                        runner_instance_id: "inst-import".to_string(),
                        request_id: request.request_id,
                        exit_code: Some(0),
                        stdout: Some(
                            json!({
                                "path": path,
                                "upload_id": upload_id,
                                "received_bytes": received_bytes,
                                "temp_file_removed": true,
                                "sidecar_removed": true,
                                "final_file_exists": false,
                                "committed": false
                            })
                            .to_string(),
                        ),
                        stderr: None,
                        duration_ms: Some(1),
                        error: None,
                    })
                    .await
                    .unwrap();
                return chunk_count;
            }
            other => panic!("unexpected over-limit MCP import request: {other}"),
        }
    }
}

#[tokio::test]
async fn pat_created_replacement_client_with_same_redirect_remains_untrusted() {
    let (_db_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let trusted =
        seed_mcp_import_client(&db, &user, "ChatGPT WebCodex", MCP_IMPORT_TRUSTED_REDIRECT);
    let config = mcp_import_config(&[trusted.client_id.as_str()]);
    let pat = seed_mcp_import_pat(&db, &user);
    let service = Service::new(build_mcp_import_oauth_management_router(
        config.clone(),
        db.clone(),
    ));

    let mut revoked = TestClient::post("http://localhost/api/oauth/clients/revoke")
        .bearer_auth(&pat)
        .json(&json!({"client_id": trusted.client_id}))
        .send(&service)
        .await;
    assert_eq!(revoked.status_code, Some(StatusCode::OK));
    let revoked_body: Value = revoked.take_json().await.unwrap();
    assert_eq!(revoked_body["success"], true);

    let mut created = TestClient::post("http://localhost/api/oauth/clients/create")
        .bearer_auth(&pat)
        .json(&json!({
            "name": "ChatGPT WebCodex",
            "redirect_uris": [MCP_IMPORT_TRUSTED_REDIRECT],
            "allowed_scopes": ["project:write"]
        }))
        .send(&service)
        .await;
    assert_eq!(created.status_code, Some(StatusCode::OK));
    let created_body: Value = created.take_json().await.unwrap();
    let replacement_client_id = created_body["client"]["client_id"]
        .as_str()
        .expect("replacement client id");
    assert!(replacement_client_id.starts_with("wc_client_"));
    assert_ne!(replacement_client_id, trusted.client_id);
    assert_eq!(
        created_body["client"]["redirect_uris"][0],
        MCP_IMPORT_TRUSTED_REDIRECT
    );

    // Model the strongest downstream case: even if the PAT holder completes
    // OAuth for the replacement and obtains a valid access token, its
    // allowed_client_id is the new server-generated ID and cannot match the
    // operator-controlled trust configuration for the revoked client.
    let replacement_auth = mcp_import_oauth_auth(replacement_client_id);
    assert_eq!(
        mcp_host_file_import_trust_from_state(&config, &db, Some(&replacement_auth)),
        HostFileImportTrust::Untrusted
    );
}

#[test]
fn mcp_file_import_trust_decision_reports_exact_failure_stage() {
    let mut config = (*test_config_oauth2(Some("secret"))).clone();
    let (_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client =
        seed_mcp_import_client(&db, &user, "ChatGPT WebCodex", MCP_IMPORT_TRUSTED_REDIRECT);
    config.oauth2.trusted_mcp_file_client_ids = vec![client.client_id.clone()];

    let missing_auth = mcp_host_file_import_trust_decision_from_state(&config, &db, None);
    assert_eq!(missing_auth.reason, HostFileImportTrustReason::MissingAuth);

    let api_auth = crate::auth::AuthContext::new(crate::auth::AuthKind::ApiToken);
    let not_oauth = mcp_host_file_import_trust_decision_from_state(&config, &db, Some(&api_auth));
    assert_eq!(not_oauth.reason, HostFileImportTrustReason::NotOAuthToken);

    let mut missing_id = mcp_import_oauth_auth(&client.client_id);
    missing_id.allowed_client_id = None;
    assert_eq!(
        mcp_host_file_import_trust_decision_from_state(&config, &db, Some(&missing_id)).reason,
        HostFileImportTrustReason::MissingAllowedClientId
    );

    let mut disabled = config.clone();
    disabled.oauth2.enabled = false;
    assert_eq!(
        mcp_host_file_import_trust_decision_from_state(
            &disabled,
            &db,
            Some(&mcp_import_oauth_auth(&client.client_id))
        )
        .reason,
        HostFileImportTrustReason::OAuthDisabled
    );

    let other_client_id = crate::auth::generate_oauth_client_id();
    assert_eq!(
        mcp_host_file_import_trust_decision_from_state(
            &config,
            &db,
            Some(&mcp_import_oauth_auth(&other_client_id))
        )
        .reason,
        HostFileImportTrustReason::ClientIdNotConfigured
    );

    let unknown_configured_id = crate::auth::generate_oauth_client_id();
    let mut unknown_config = config.clone();
    unknown_config.oauth2.trusted_mcp_file_client_ids = vec![unknown_configured_id.clone()];
    assert_eq!(
        mcp_host_file_import_trust_decision_from_state(
            &unknown_config,
            &db,
            Some(&mcp_import_oauth_auth(&unknown_configured_id))
        )
        .reason,
        HostFileImportTrustReason::ClientRegistrationMissingOrRevoked
    );

    let trusted = mcp_host_file_import_trust_decision_from_state(
        &config,
        &db,
        Some(&mcp_import_oauth_auth(&client.client_id)),
    );
    assert_eq!(trusted.reason, HostFileImportTrustReason::Trusted);
    assert_eq!(trusted.trust, HostFileImportTrust::TrustedOAuthClient);
    assert_eq!(trusted.client_id_configured, Some(true));
    assert_eq!(trusted.active_client_registration_found, Some(true));
}

#[test]
fn adaptive_gateway_file_import_preserves_target_aware_host_trust() {
    run_mcp_import_in_large_stack_test_thread(
        adaptive_gateway_file_import_preserves_target_aware_host_trust_impl,
    );
}

async fn adaptive_gateway_file_import_preserves_target_aware_host_trust_impl() {
    let _lock = lock_mcp_import_test().await;
    let (_db_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client =
        seed_mcp_import_client(&db, &user, "ChatGPT WebCodex", MCP_IMPORT_TRUSTED_REDIRECT);
    let token = seed_oauth_access_token(&db, &client, &user, "project:write");
    let project_tmp = tempfile::tempdir().unwrap();
    let (runtime, _registry) = mcp_import_runtime_with_surface(
        project_tmp.path(),
        Some("alice"),
        ModelSurface::AdaptiveRuntime,
    )
    .await;
    let service = Service::new(build_test_router(
        mcp_import_config(&[client.client_id.as_str()]),
        db,
        runtime,
    ));

    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        mcp_2026_params(json!({
            "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
            "arguments": {
                "tool": "import_conversation_files_to_project",
                "arguments": {
                    "project": "agent:importer:demo",
                    "openaiFileIdRefs": []
                }
            }
        })),
    )
    .await;

    let decision = take_last_mcp_host_file_import_trust_decision()
        .expect("gateway target must be recognized before host-file trust selection");
    assert_eq!(decision.reason, HostFileImportTrustReason::Trusted);
    assert_eq!(decision.trust, HostFileImportTrust::TrustedOAuthClient);
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["structuredContent"]["success"], false);
    assert!(body["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap_or("")
        .contains("1..=10 files"));
}

#[test]
fn oauth_mcp_file_import_startup_env_stateless_2026_crosses_provenance_gate() {
    run_mcp_import_in_large_stack_test_thread(
        oauth_mcp_file_import_startup_env_stateless_2026_crosses_provenance_gate_impl,
    );
}

async fn oauth_mcp_file_import_startup_env_stateless_2026_crosses_provenance_gate_impl() {
    use crate::auth::{OAuth2Verifier, TokenVerifier};
    use sha2::{Digest, Sha256};

    let _lock = lock_mcp_import_test().await;
    let pptx = b"startup-env-stateless-trusted-pptx".to_vec();
    let expected_sha256 = format!("{:x}", Sha256::digest(&pptx));
    let server = start_mcp_import_mock_server(mcp_import_http_response(
        "200 OK",
        &[("Content-Length", pptx.len().to_string())],
        &pptx,
    ))
    .await;
    let _network = McpImportNetworkOverride::set(server.base_url.clone());

    let (_db_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client =
        seed_mcp_import_client(&db, &user, "ChatGPT WebCodex", MCP_IMPORT_TRUSTED_REDIRECT);
    let token = seed_oauth_access_token(&db, &client, &user, "project:write");
    let env_file_other_client_id = crate::auth::generate_oauth_client_id();
    let config = mcp_import_config_from_startup_env(&client.client_id, &env_file_other_client_id);

    // Stage A: the same verifier used by AuthMiddleware preserves the exact
    // OAuth client id on the authenticated context.
    let verifier = OAuth2Verifier;
    let verified = verifier
        .verify(config.as_ref(), Some(&db), &token)
        .await
        .unwrap()
        .expect("seeded OAuth access token must authenticate");
    assert_eq!(verified.kind, crate::auth::AuthKind::OAuth2Token);
    assert_eq!(verified.token_kind.as_deref(), Some("oauth2"));
    assert_eq!(
        verified.allowed_client_id.as_deref(),
        Some(client.client_id.as_str())
    );

    // Stage B: the parsed startup Config + active DB registration grants only
    // the exact configured OAuth client.
    let decision = mcp_host_file_import_trust_decision_from_state(
        config.as_ref(),
        db.as_ref(),
        Some(&verified),
    );
    assert_eq!(decision.reason, HostFileImportTrustReason::Trusted);
    assert_eq!(decision.trust, HostFileImportTrust::TrustedOAuthClient);

    let project_tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_import_runtime(project_tmp.path(), Some("alice")).await;
    let service = Service::new(build_test_router(config, db, runtime));
    let agent = tokio::spawn(complete_mcp_import_save(registry, pptx.clone()));
    let temporary_url = "https://download.example/temporary-secret-token/stateless-import.pptx";
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        mcp_2026_params(json!({
            "name": "import_conversation_files_to_project",
            "arguments": {
                "project": "agent:importer:demo",
                "openaiFileIdRefs": [{
                    "download_url": temporary_url,
                    "file_id": "file_stateless_host_rewritten",
                    "mime_type": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                    "file_name": "source.pptx"
                }],
                "output_dir": "paper/export",
                "targets": ["stateless-import.pptx"],
                "overwrite": false,
                "trusted_mcp_host_file_import": false
            }
        })),
    )
    .await;
    // Stage C: observe the exact mcp_post trust decision after AuthMiddleware.
    let mcp_decision = take_last_mcp_host_file_import_trust_decision()
        .expect("mcp_post must evaluate host-file trust for the import tool");
    assert_eq!(mcp_decision.reason, HostFileImportTrustReason::Trusted);
    assert_eq!(mcp_decision.trust, HostFileImportTrust::TrustedOAuthClient);

    // Stages D-E: the kernel injects the internal provenance bit after JSON
    // deserialization and dispatch crosses the pre-network provenance gate to
    // SaveProjectArtifact.
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["resultType"], "complete", "body: {body:?}");
    assert_eq!(body["result"]["isError"], false, "body: {body:?}");
    tokio::time::timeout(std::time::Duration::from_secs(5), agent)
        .await
        .unwrap_or_else(|_| panic!("save_project_artifact fixture timed out; body: {body:?}"))
        .unwrap();
    let imported = &body["result"]["structuredContent"]["output"]["imported"][0];
    assert_eq!(imported["path"], "paper/export/stateless-import.pptx");
    assert_eq!(imported["bytes_written"], pptx.len());
    assert_eq!(imported["sha256"], expected_sha256);
    assert_eq!(
        crate::tool_runtime::conversation_import::import_test_dns_resolution_count(),
        1
    );
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(!serialized.contains(temporary_url));
    assert!(!serialized.contains("file_stateless_host_rewritten"));
}

#[test]
fn oauth_mcp_file_import_trusted_client_saves_pptx() {
    run_mcp_import_in_large_stack_test_thread(oauth_mcp_file_import_trusted_client_saves_pptx_impl);
}

async fn oauth_mcp_file_import_trusted_client_saves_pptx_impl() {
    use sha2::{Digest, Sha256};

    let _lock = lock_mcp_import_test().await;
    let pptx = b"trusted-chatgpt-pptx-attachment".to_vec();
    let expected_sha256 = format!("{:x}", Sha256::digest(&pptx));
    let server = start_mcp_import_mock_server(mcp_import_http_response(
        "200 OK",
        &[("Content-Length", pptx.len().to_string())],
        &pptx,
    ))
    .await;
    let _network = McpImportNetworkOverride::set(server.base_url.clone());

    let (_db_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client =
        seed_mcp_import_client(&db, &user, "ChatGPT WebCodex", MCP_IMPORT_TRUSTED_REDIRECT);
    let token = seed_oauth_access_token(&db, &client, &user, "project:write");
    let project_tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_import_runtime(project_tmp.path(), Some("alice")).await;
    let service = Service::new(build_test_router(
        mcp_import_config(&[client.client_id.as_str()]),
        db.clone(),
        runtime,
    ));
    let agent = tokio::spawn(complete_mcp_import_save(registry, pptx.clone()));
    let temporary_url = "https://download.example/temporary-secret-token/import-test.pptx";
    let (status, body, _) = oauth_mcp_request(
        &service,
        &token,
        "tools/call",
        json!({
            "name": "import_conversation_files_to_project",
            "arguments": {
                "project": "agent:importer:demo",
                "openaiFileIdRefs": [{
                    "download_url": temporary_url,
                    "file_id": "file_host_rewritten",
                    "mime_type": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                    "file_name": "source.pptx"
                }],
                "output_dir": "paper/export",
                "targets": ["import-test.pptx"],
                "overwrite": false
            }
        }),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), agent)
        .await
        .expect("save_project_artifact fixture timed out")
        .unwrap();

    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["isError"], false, "body: {body:?}");
    let imported = &body["result"]["structuredContent"]["output"]["imported"][0];
    assert_eq!(imported["path"], "paper/export/import-test.pptx");
    assert_eq!(imported["bytes_written"], pptx.len());
    assert_eq!(imported["sha256"], expected_sha256);
    assert_eq!(
        imported["mime_type"],
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    );
    assert_eq!(
        crate::tool_runtime::conversation_import::import_test_dns_resolution_count(),
        1,
        "trusted hostname must be resolved exactly once before the request"
    );

    let (summary, error): (String, String) = {
        let conn = db.conn_for_tests();
        conn.query_row(
            "SELECT summary_json, COALESCE(error_summary, '') FROM action_events ORDER BY started_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    assert!(!summary.contains(temporary_url));
    assert!(!error.contains(temporary_url));
}

#[test]
fn oauth_mcp_file_import_trusted_download_guards_remain_bounded() {
    run_mcp_import_in_large_stack_test_thread(
        oauth_mcp_file_import_trusted_download_guards_remain_bounded_impl,
    );
}

async fn oauth_mcp_file_import_trusted_download_guards_remain_bounded_impl() {
    let _lock = lock_mcp_import_test().await;
    let (_db_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let client =
        seed_mcp_import_client(&db, &user, "ChatGPT WebCodex", MCP_IMPORT_TRUSTED_REDIRECT);
    let token = seed_oauth_access_token(&db, &client, &user, "project:write");
    let project_tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_import_runtime(project_tmp.path(), Some("alice")).await;
    let service = Service::new(build_test_router(
        mcp_import_config(&[client.client_id.as_str()]),
        db,
        runtime,
    ));
    let temporary_url = "https://download.example/DO-NOT-LOG/trusted-guard.pptx";
    let params = json!({
        "name": "import_conversation_files_to_project",
        "arguments": {
            "project": "agent:importer:demo",
            "openaiFileIdRefs": [{
                "download_url": temporary_url,
                "file_id": "file_host_guard",
                "mime_type": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "file_name": "guard.pptx"
            }],
            "targets": ["guard.pptx"]
        }
    });

    let cases = vec![
        (
            mcp_import_http_response(
                "302 Found",
                &[("Location", "https://public.example/other".to_string())],
                b"",
            ),
            "HTTP 302",
            false,
        ),
        (
            mcp_import_http_response(
                "200 OK",
                &[(
                    "Content-Length",
                    (crate::tool_runtime::conversation_import::MAX_IMPORT_FILE_BYTES + 1)
                        .to_string(),
                )],
                b"",
            ),
            "exceeds",
            false,
        ),
        (
            mcp_import_http_response(
                "200 OK",
                &[],
                &vec![b'x'; crate::tool_runtime::conversation_import::MAX_IMPORT_FILE_BYTES + 1],
            ),
            "exceeds",
            true,
        ),
    ];

    for (response, expected_error, requires_upload_cleanup) in cases {
        let server = start_mcp_import_mock_server(response).await;
        let network = McpImportNetworkOverride::set(server.base_url.clone());
        let upload_fixture = requires_upload_cleanup
            .then(|| tokio::spawn(complete_mcp_import_until_abort(registry.clone())));
        let (status, body, _) =
            oauth_mcp_request(&service, &token, "tools/call", params.clone()).await;
        if let Some(upload_fixture) = upload_fixture {
            tokio::time::timeout(std::time::Duration::from_secs(10), upload_fixture)
                .await
                .expect("over-limit import abort fixture timed out")
                .unwrap();
            assert!(!project_tmp.path().join("guard.pptx").exists());
        }
        assert_eq!(status, StatusCode::OK, "body: {body:?}");
        assert_eq!(body["result"]["isError"], true, "body: {body:?}");
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(serialized.contains(expected_error), "body: {body:?}");
        assert!(!serialized.contains(temporary_url));
        assert_eq!(
            crate::tool_runtime::conversation_import::import_test_dns_resolution_count(),
            1
        );
        drop(network);
        drop(server);
    }
}

#[test]
fn mcp_file_import_untrusted_callers_fail_before_dns() {
    run_mcp_import_in_large_stack_test_thread(
        mcp_file_import_untrusted_callers_fail_before_dns_impl,
    );
}

async fn mcp_file_import_untrusted_callers_fail_before_dns_impl() {
    let _lock = lock_mcp_import_test().await;
    let _network = McpImportNetworkOverride::without_download();
    let (_db_tmp, db) = test_db();
    let user = seed_user(&db, "alice");
    let ordinary = seed_mcp_import_client(
        &db,
        &user,
        "Ordinary MCP Client",
        "https://ordinary.example/oauth/callback",
    );
    let token = seed_oauth_access_token(&db, &ordinary, &user, "project:write");
    let project_tmp = tempfile::tempdir().unwrap();
    let (runtime, _registry) = mcp_import_runtime(project_tmp.path(), Some("alice")).await;
    let configured_but_unrelated_client_id = crate::auth::generate_oauth_client_id();
    let service = Service::new(build_test_router(
        mcp_import_config(&[configured_but_unrelated_client_id.as_str()]),
        db,
        runtime,
    ));
    let temporary_url = "https://download.example/SHOULD-NOT-BE-RESOLVED/file.pptx";
    let params = json!({
        "name": "import_conversation_files_to_project",
        "arguments": {
            "project": "agent:importer:demo",
            "openaiFileIdRefs": [{
                "download_url": temporary_url,
                "file_id": "caller-controlled-id",
                "mime_type": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "file_name": "safe.pptx"
            }],
            "targets": ["safe.pptx"]
        }
    });

    let (status, body, _) = oauth_mcp_request(&service, &token, "tools/call", params.clone()).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["isError"], true);
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(serialized.contains("explicitly trusted OAuth MCP client"));
    assert!(!serialized.contains(temporary_url));
    assert_eq!(
        crate::tool_runtime::conversation_import::import_test_dns_resolution_count(),
        0,
        "ordinary OAuth client must be rejected before DNS/network"
    );

    crate::tool_runtime::conversation_import::reset_import_test_dns_resolution_count();
    let (status, body, _) = oauth_mcp_request(&service, "secret", "tools/call", params).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["result"]["isError"], true);
    assert!(serde_json::to_string(&body)
        .unwrap()
        .contains("explicitly trusted OAuth MCP client"));
    assert_eq!(
        crate::tool_runtime::conversation_import::import_test_dns_resolution_count(),
        0,
        "raw/API-token MCP client must be rejected before DNS/network"
    );
}

fn build_mcp_import_oauth_management_router(
    config: Arc<crate::Config>,
    db: Arc<crate::Database>,
) -> Router {
    Router::new()
        .hoop(affix_state::inject(config))
        .hoop(affix_state::inject(db))
        .push(
            Router::with_path("api/oauth/clients")
                .hoop(crate::AuthMiddleware)
                .push(Router::with_path("create").post(crate::oauth_http::oauth_clients_create))
                .push(Router::with_path("revoke").post(crate::oauth_http::oauth_clients_revoke)),
        )
}
