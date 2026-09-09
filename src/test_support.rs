//! Shared test-only helpers.
//!
//! Single authoritative home for the `Config` builders, temp-dir `Database`
//! constructor, and DB row seeding that the HTTP/auth test modules previously
//! each carried a byte-identical copy of. Compiled only under `cfg(test)`
//! (see the `mod test_support` declaration in `main.rs`).

use std::path::PathBuf;
use std::sync::Arc;

thread_local! {
    static TOOL_REQUEST_TRACE_ENV_READ_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Test-only opt-in for reading the process-global tool-request trace env.
///
/// libtest executes tests in parallel within one process. Trace tests still use
/// [`TestEnvGuard`] to serialize env mutation, but unrelated test threads must
/// not observe a temporary `WEBCODEX_TOOL_REQUEST_TRACE=full` and start feeding
/// the same bounded global trace writer. Child threads intentionally exercising
/// trace behavior can opt in explicitly with this guard while the parent keeps
/// the canonical env lock held.
pub(crate) struct TestToolRequestTraceEnvReaderGuard;

impl TestToolRequestTraceEnvReaderGuard {
    pub(crate) fn new() -> Self {
        TOOL_REQUEST_TRACE_ENV_READ_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for TestToolRequestTraceEnvReaderGuard {
    fn drop(&mut self) {
        TOOL_REQUEST_TRACE_ENV_READ_DEPTH.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0, "tool-request trace env reader depth underflow");
            depth.set(current.saturating_sub(1));
        });
    }
}

pub(crate) fn tool_request_trace_env_visible_for_current_thread() -> bool {
    TOOL_REQUEST_TRACE_ENV_READ_DEPTH.with(|depth| depth.get() > 0)
}

/// Panic-safe process-global environment mutation guard for server tests.
///
/// The guard holds the canonical server test env lock for its full lifetime,
/// snapshots each variable only before the first mutation, and restores the
/// original process environment during unwinding as well as ordinary drop.
pub(crate) struct TestEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: std::collections::BTreeMap<String, Option<std::ffi::OsString>>,
    tool_request_trace_env_reader: Option<TestToolRequestTraceEnvReaderGuard>,
}

impl TestEnvGuard {
    pub(crate) fn new() -> Self {
        Self {
            _lock: crate::admin_cli::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            previous: std::collections::BTreeMap::new(),
            tool_request_trace_env_reader: None,
        }
    }

    fn remember(&mut self, name: &str) {
        if name == "WEBCODEX_TOOL_REQUEST_TRACE" && self.tool_request_trace_env_reader.is_none() {
            self.tool_request_trace_env_reader = Some(TestToolRequestTraceEnvReaderGuard::new());
        }
        if !self.previous.contains_key(name) {
            self.previous
                .insert(name.to_string(), std::env::var_os(name));
        }
    }

    pub(crate) fn set(&mut self, name: &str, value: impl AsRef<std::ffi::OsStr>) {
        self.remember(name);
        std::env::set_var(name, value);
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.remember(name);
        std::env::remove_var(name);
    }
}

impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// Minimal `Config` for tests (token sets whether auth is enabled).
pub(crate) fn test_config(token: Option<&str>) -> Arc<crate::Config> {
    Arc::new(crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: token.map(str::to_string),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config::default(),
    })
}

/// Like [`test_config`] but with OAuth2 enabled (1h access-token TTL,
/// 30d refresh-token TTL).
pub(crate) fn test_config_oauth2(token: Option<&str>) -> Arc<crate::Config> {
    Arc::new(crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: token.map(str::to_string),
        max_text_size: 2 * 1024 * 1024,
        oauth2: crate::OAuth2Config {
            enabled: true,
            access_token_ttl_secs: 3600,
            refresh_token_ttl_secs: 2_592_000,
            ..crate::OAuth2Config::default()
        },
    })
}

/// Create an empty Database in a temp dir. The TempDir must be kept alive
/// for the lifetime of the returned Database so the sqlite file is not
/// deleted mid-test.
pub(crate) fn test_db() -> (tempfile::TempDir, Arc<crate::Database>) {
    let tmp = tempfile::tempdir().unwrap();
    let db = crate::Database::open(&tmp.path().join("test.db")).unwrap();
    (tmp, Arc::new(db))
}

pub(crate) fn runner_access(
    auth: &crate::auth::AuthContext,
) -> webcodex_runner_registry::RunnerAccess {
    crate::runner_http::runner_access_from_auth(Some(auth))
        .expect("authenticated root test context must project to RunnerAccess")
}

/// Upgrade a server-test Runner capability fixture to the current registration
/// contract while preserving caller-selected RegistrationRequired features.
/// Tests that exercise registration rejection should construct their wire
/// capabilities directly instead of using this helper.
pub(crate) fn current_runner_capabilities(
    capabilities: crate::runner_protocol::RunnerCapabilities,
) -> crate::runner_protocol::RunnerCapabilities {
    let mut value = serde_json::to_value(capabilities).expect("serialize Runner test capabilities");
    let object = value
        .as_object_mut()
        .expect("Runner test capabilities must serialize as an object");
    for capability in
        crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES
    {
        object.insert((*capability).to_string(), serde_json::Value::Bool(true));
    }
    serde_json::from_value(value).expect("deserialize canonical Runner test capabilities")
}

/// Canonicalize an ordinary unit-test Runner registration to the current
/// generation-2 contract. Protocol-admission tests intentionally bypass this
/// helper so missing/old generations remain observable failures.
pub(crate) fn current_runner_registration(
    mut registration: crate::runner_protocol::RunnerRegisterRequest,
) -> crate::runner_protocol::RunnerRegisterRequest {
    registration.runner_protocol_generation = crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2;
    registration.capabilities = current_runner_capabilities(registration.capabilities);
    registration
}

static TEST_PROJECT_INVENTORY_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// Publish one authoritative paged project-inventory snapshot for a registered
/// test Runner. Tests must use the same post-registration protocol as production
/// rather than smuggling projects through the retired inline registration field.
pub(crate) async fn apply_project_inventory_snapshot(
    registry: &crate::runner_http::RunnerRegistry,
    client_id: &str,
    runner_instance_id: &str,
    projects: Vec<crate::runner_protocol::RunnerProjectSummary>,
) {
    use std::sync::atomic::Ordering;

    let snapshot_sequence = TEST_PROJECT_INVENTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let generation = format!("test-inventory-{snapshot_sequence}");
    let chunks = projects
        .chunks(crate::runner_protocol::PROJECT_INVENTORY_PAGE_MAX_SUMMARIES)
        .collect::<Vec<_>>();
    if chunks.is_empty() {
        registry
            .apply_project_inventory_page(
                client_id,
                runner_instance_id,
                crate::runner_protocol::ShellProjectInventoryPage {
                    generation,
                    snapshot_sequence,
                    page_index: 0,
                    total_reported: 0,
                    complete: true,
                    projects: Vec::new(),
                },
            )
            .await
            .expect("empty test project inventory snapshot");
        return;
    }

    let last = chunks.len() - 1;
    for (index, chunk) in chunks.into_iter().enumerate() {
        registry
            .apply_project_inventory_page(
                client_id,
                runner_instance_id,
                crate::runner_protocol::ShellProjectInventoryPage {
                    generation: generation.clone(),
                    snapshot_sequence,
                    page_index: index as u32,
                    total_reported: projects.len(),
                    complete: index == last,
                    projects: chunk.to_vec(),
                },
            )
            .await
            .expect("test project inventory page");
    }
}

/// Bootstrap helper: create a user with the given role directly via the DB
/// so tests can mint tokens for them.
pub(crate) fn seed_user_with_role(
    db: &crate::Database,
    username: &str,
    role: &str,
) -> crate::models::UserRecord {
    let now = chrono::Utc::now().timestamp();
    let user = crate::models::UserRecord {
        id: uuid::Uuid::new_v4().to_string(),
        username: username.to_string(),
        created_at: now,
        disabled: 0,
        display_name: None,
        role: role.to_string(),
        disabled_at: None,
        updated_at: Some(now),
    };
    db.create_user(&user).unwrap();
    user
}

/// [`seed_user_with_role`] with the default `"user"` role.
pub(crate) fn seed_user(db: &crate::Database, username: &str) -> crate::models::UserRecord {
    seed_user_with_role(db, username, "user")
}

/// Shared body for the two `seed_oauth_client*` shapes below.
fn seed_oauth_client_record(
    db: &crate::Database,
    user: &crate::models::UserRecord,
    name: &str,
    allowed_scopes: &str,
) -> (crate::models::OAuthClientRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext_secret = crate::auth::generate_oauth_client_secret();
    let record = crate::models::OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: crate::auth::generate_oauth_client_id(),
        client_secret_hash: crate::auth::hash_token(&plaintext_secret),
        name: name.to_string(),
        owner_user_id: Some(user.id.clone()),
        owner_project_grant_id: None,
        owner_shared_key_hash: None,
        redirect_uris: "https://example.com/callback".to_string(),
        allowed_scopes: allowed_scopes.to_string(),
        created_at: now,
        revoked_at: None,
    };
    db.insert_oauth_client(&record).unwrap();
    (record, plaintext_secret)
}

/// Seed an OAuth2 client ("Test App") owned by `user` with the broad scope
/// set used by the mcp/runtime_http HTTP tests. The plaintext secret is
/// discarded.
pub(crate) fn seed_oauth_client(
    db: &crate::Database,
    user: &crate::models::UserRecord,
) -> crate::models::OAuthClientRecord {
    seed_oauth_client_record(
        db,
        user,
        "Test App",
        "runtime:read project:read project:write job:run account:manage",
    )
    .0
}

/// Seed a named OAuth2 client with the narrow `runtime:read project:read`
/// scope set and return `(record, plaintext_secret)`.
pub(crate) fn seed_oauth_client_named(
    db: &crate::Database,
    user: &crate::models::UserRecord,
    name: &str,
) -> (crate::models::OAuthClientRecord, String) {
    seed_oauth_client_record(db, user, name, "runtime:read project:read")
}
