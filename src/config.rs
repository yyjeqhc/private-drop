use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub data_dir: PathBuf,
    pub token: Option<String>,
    pub max_text_size: usize,
    pub oauth2: OAuth2Config,
}

/// Server-side QUIC Runner transport configuration. Sourced from
/// `WEBCODEX_QUIC_*` env vars, mirroring the project's env-var config pattern.
/// Kept as a standalone struct (not embedded in [`Config`]) so existing
/// `Config { ... }` test literals and constructors are untouched. The listener
/// is **off by default**; operators must explicitly set
/// `WEBCODEX_QUIC_ENABLED=true` and provide a cert/key.
///
/// This is a custom WebCodex QUIC stream transport, NOT HTTP/3. Nginx is
/// not involved; the server listens on UDP directly. The cert/key paths are
/// NOT hardcoded to production Let's Encrypt paths — they are read from env so
/// dev/staging/prod can differ. Paths are validated at listener startup so a
/// missing cert produces a clear runtime error.
#[derive(Debug, Clone)]
pub struct QuicServerConfig {
    pub enabled: bool,
    pub listen: String,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub alpn: String,
}

/// Non-sensitive server-side QUIC listener status exposed through
/// `runtime_status`. This intentionally carries only operator-safe fields:
/// listen address, ALPN, started flag, and a short sanitized error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuicRuntimeStatus {
    pub enabled: bool,
    pub listen: String,
    pub alpn: String,
    pub listener_started: bool,
    pub last_error: Option<String>,
}

impl QuicServerConfig {
    /// Parse from `WEBCODEX_QUIC_*` env vars. Disabled by default.
    pub fn from_env() -> Self {
        Self {
            enabled: env_flag("WEBCODEX_QUIC_ENABLED").unwrap_or(false),
            listen: std::env::var("WEBCODEX_QUIC_LISTEN")
                .unwrap_or_else(|_| "0.0.0.0:8443".to_string()),
            cert: std::env::var("WEBCODEX_QUIC_CERT")
                .map(PathBuf::from)
                .unwrap_or_default(),
            key: std::env::var("WEBCODEX_QUIC_KEY")
                .map(PathBuf::from)
                .unwrap_or_default(),
            alpn: std::env::var("WEBCODEX_QUIC_ALPN")
                .unwrap_or_else(|_| crate::runner_protocol::RUNNER_QUIC_ALPN_V1.to_string()),
        }
    }

    /// Validate that the required cert/key paths are present and readable when
    /// the listener is enabled. Returns a clear error naming the missing field;
    /// never reads or returns file *contents* (in particular, never the key).
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.cert.as_os_str().is_empty() {
            return Err("WEBCODEX_QUIC_CERT is not set; QUIC listener requires a cert path".into());
        }
        if self.key.as_os_str().is_empty() {
            return Err("WEBCODEX_QUIC_KEY is not set; QUIC listener requires a key path".into());
        }
        if !self.cert.exists() {
            return Err(format!(
                "WEBCODEX_QUIC_CERT path does not exist: {}",
                self.cert.display()
            ));
        }
        if !self.key.exists() {
            return Err(format!(
                "WEBCODEX_QUIC_KEY path does not exist: {}",
                self.key.display()
            ));
        }
        if self.alpn.trim().is_empty() {
            return Err("WEBCODEX_QUIC_ALPN cannot be empty".into());
        }
        Ok(())
    }

    pub fn runtime_status(&self) -> QuicRuntimeStatus {
        QuicRuntimeStatus {
            enabled: self.enabled,
            listen: self.listen.clone(),
            alpn: self.alpn.clone(),
            listener_started: false,
            last_error: None,
        }
    }
}

impl QuicRuntimeStatus {
    pub fn mark_started(&mut self) {
        self.listener_started = true;
        self.last_error = None;
    }

    pub fn mark_error(&mut self, error: impl AsRef<str>) {
        self.listener_started = false;
        self.last_error = Some(sanitize_quic_runtime_error(error.as_ref()));
    }
}

pub fn sanitize_quic_runtime_error(error: &str) -> String {
    let compact = error
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    let lower = compact.to_ascii_lowercase();

    if lower.contains("webcodex_quic_cert is not set") {
        return "WEBCODEX_QUIC_CERT is not set; QUIC listener requires a cert path".to_string();
    }
    if lower.contains("webcodex_quic_key is not set") {
        return "WEBCODEX_QUIC_KEY is not set; QUIC listener requires a key path".to_string();
    }
    if lower.contains("webcodex_quic_cert") && lower.contains("does not exist") {
        return "WEBCODEX_QUIC_CERT path does not exist".to_string();
    }
    if lower.contains("webcodex_quic_key") && lower.contains("does not exist") {
        return "WEBCODEX_QUIC_KEY path does not exist".to_string();
    }
    if lower.contains("quic cert") {
        if lower.contains("no certificates") {
            return "QUIC cert contains no certificates".to_string();
        }
        if lower.contains("parse") {
            return "failed to parse QUIC cert".to_string();
        }
        if lower.contains("open") {
            return "failed to open QUIC cert".to_string();
        }
    }
    if lower.contains("quic key") || lower.contains("private key") {
        if lower.contains("no private key") {
            return "QUIC key contains no private key".to_string();
        }
        if lower.contains("parse") {
            return "failed to parse QUIC key".to_string();
        }
        if lower.contains("open") {
            return "failed to open QUIC key".to_string();
        }
    }

    compact.chars().take(240).collect()
}

impl Default for QuicServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "0.0.0.0:8443".to_string(),
            cert: PathBuf::new(),
            key: PathBuf::new(),
            alpn: crate::runner_protocol::RUNNER_QUIC_ALPN_V1.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EnvFileLoad {
    pub(crate) path: PathBuf,
    pub(crate) loaded_count: usize,
}

pub(crate) fn parse_env_file_line(line: &str) -> Option<Result<(String, String), String>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim();
    let Some((key, value)) = line.split_once('=') else {
        return Some(Err("missing '='".to_string()));
    };
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Some(Err(format!("invalid env key '{}'", key)));
    }
    let value = value.trim();
    let value = if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    };
    Some(Ok((key.to_string(), value)))
}

fn load_env_file(path: &Path) -> Result<EnvFileLoad, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read env file {}: {}", path.display(), e))?;
    let mut loaded_count = 0;
    for (idx, line) in content.lines().enumerate() {
        let Some(parsed) = parse_env_file_line(line) else {
            continue;
        };
        let (key, value) = parsed.map_err(|e| {
            format!(
                "failed to parse env file {} line {}: {}",
                path.display(),
                idx + 1,
                e
            )
        })?;
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, value);
            loaded_count += 1;
        }
    }
    Ok(EnvFileLoad {
        path: path.to_path_buf(),
        loaded_count,
    })
}

pub(crate) fn env_flag(key: &str) -> Option<bool> {
    let value = std::env::var(key).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Operator-selected tool-request trace detail.
///
/// Compatibility is deliberate: the historical boolean `true` remains the
/// metadata-only mode. Raw arguments/results are persisted only when the
/// operator explicitly selects `full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolRequestTraceMode {
    Off,
    Metadata,
    Full,
}

impl ToolRequestTraceMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Metadata => "metadata",
            Self::Full => "full",
        }
    }
}

pub(crate) fn tool_request_trace_mode() -> ToolRequestTraceMode {
    #[cfg(test)]
    if !crate::test_support::tool_request_trace_env_visible_for_current_thread() {
        return ToolRequestTraceMode::Off;
    }
    let Ok(value) = std::env::var("WEBCODEX_TOOL_REQUEST_TRACE") else {
        return ToolRequestTraceMode::Off;
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "metadata" => ToolRequestTraceMode::Metadata,
        "full" => ToolRequestTraceMode::Full,
        "0" | "false" | "no" | "off" => ToolRequestTraceMode::Off,
        _ => ToolRequestTraceMode::Off,
    }
}

pub(crate) fn tool_request_trace_enabled() -> bool {
    tool_request_trace_mode() != ToolRequestTraceMode::Off
}

pub(crate) fn tool_request_trace_dir() -> PathBuf {
    if let Ok(path) = std::env::var("WEBCODEX_TOOL_REQUEST_TRACE_DIR") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    std::env::var("WEBCODEX_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data"))
        .join("tool-request-traces")
}

pub(crate) fn tool_request_trace_retention_hours() -> u64 {
    std::env::var("WEBCODEX_TOOL_REQUEST_TRACE_RETENTION_HOURS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(168)
}

pub(crate) fn tool_request_trace_max_total_bytes() -> u64 {
    std::env::var("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(2 * 1024 * 1024 * 1024)
}

/// Optional operator override for MCP `tools/list` compact schema projection.
///
/// `true` omits `outputSchema` from MCP discovery while preserving name,
/// description, inputSchema, annotations, and adapter metadata. `false` restores
/// the full discovery schema. Unset or invalid values defer to the selected
/// RuntimeExposure policy rather than choosing a process-wide default here.
pub(crate) fn mcp_compact_schemas_override() -> Option<bool> {
    env_flag("WEBCODEX_MCP_COMPACT_SCHEMAS")
}

/// Global Server switch for optional MCP App presentation resources and metadata.
///
/// Apps are enabled by default. Setting `WEBCODEX_MCP_APPS_ENABLED=false` keeps
/// canonical MCP tools/results and non-App resources available while suppressing
/// App capability advertisement, tool linkage, presentation metadata, and static
/// App resource reads. Invalid values follow `env_flag` and fall back to the
/// default enabled behavior.
fn mcp_apps_enabled_from_flag(flag: Option<bool>) -> bool {
    flag.unwrap_or(true)
}

pub(crate) fn mcp_apps_enabled() -> bool {
    mcp_apps_enabled_from_flag(env_flag("WEBCODEX_MCP_APPS_ENABLED"))
}

pub(crate) fn load_startup_env_files() -> Result<Vec<EnvFileLoad>, String> {
    if let Ok(path) = std::env::var("WEBCODEX_ENV_FILE") {
        return Ok(vec![load_env_file(Path::new(&path))?]);
    }
    let candidates = [
        PathBuf::from("./webcodex.env"),
        PathBuf::from("/opt/webcodex/webcodex.env"),
        PathBuf::from("/etc/webcodex/webcodex.env"),
    ];
    let mut loaded = Vec::new();
    for path in candidates {
        if path.exists() {
            loaded.push(load_env_file(&path)?);
        }
    }
    Ok(loaded)
}

/// OAuth2 server configuration, sourced from `WEBCODEX_OAUTH2_*` env vars.
///
/// Embedded in [`Config`] but **disabled by default**, so it does not change
/// existing runtime behavior unless explicitly enabled with
/// `WEBCODEX_OAUTH2_ENABLED=true`.
///
/// The first OAuth2 implementation uses opaque DB-backed tokens. JWT/JWKS/OIDC
/// can be added later as an extension.
#[derive(Debug, Clone)]
pub struct OAuth2Config {
    /// Public issuer URL for `/.well-known/*` metadata. Defaults to
    /// `WEBCODEX_OAUTH2_ISSUER`, falling back to `WEBCODEX_PUBLIC_URL`.
    pub issuer: Option<String>,
    /// Whether the OAuth2 subsystem is active. Default `false`.
    pub enabled: bool,
    /// Access token time-to-live in seconds. Default `3600` (1 hour).
    pub access_token_ttl_secs: i64,
    /// Refresh token time-to-live in seconds. Default `2592000` (30 days).
    pub refresh_token_ttl_secs: i64,
    /// Authorization code time-to-live in seconds. Default `300` (5 minutes).
    pub authorization_code_ttl_secs: i64,
    /// Whether PKCE (S256) is required for authorization code flows. Default
    /// `true`.
    pub require_pkce: bool,
    /// Whether the public shared-key OAuth bridge authorize flow is enabled.
    /// Default `false`.
    pub shared_key_bridge_enabled: bool,
    /// Exact server-generated OAuth client IDs whose active registrations may
    /// use the ChatGPT MCP host-file import path. Empty by default.
    pub trusted_mcp_file_client_ids: Vec<String>,
    /// Exact project grant active for a project-first OAuth share session.
    /// Unset on managed/self-hosted OAuth servers.
    pub project_share_grant_id: Option<String>,
    /// Ephemeral share-session fence. A new `webcodex share --auth oauth`
    /// process uses a new value, invalidating every older project-share grant.
    pub project_share_session_id: Option<String>,
}

impl Default for OAuth2Config {
    fn default() -> Self {
        Self {
            issuer: None,
            enabled: false,
            access_token_ttl_secs: 3600,
            refresh_token_ttl_secs: 2_592_000,
            authorization_code_ttl_secs: 300,
            require_pkce: true,
            shared_key_bridge_enabled: false,
            trusted_mcp_file_client_ids: Vec::new(),
            project_share_grant_id: None,
            project_share_session_id: None,
        }
    }
}

fn normalize_trusted_mcp_file_client_id(value: &str) -> Option<String> {
    let value = crate::auth::validate_allowed_client_id(value).ok()?;
    let random = value.strip_prefix("wc_client_")?;
    if random.len() != 64
        || !random
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(value)
}

impl OAuth2Config {
    pub fn from_env() -> Self {
        let enabled = env_flag("WEBCODEX_OAUTH2_ENABLED").unwrap_or(false);
        // OAuth2-specific issuer takes precedence over the generic public URL.
        let issuer = std::env::var("WEBCODEX_OAUTH2_ISSUER")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("WEBCODEX_PUBLIC_URL")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            });
        let access_token_ttl_secs = std::env::var("WEBCODEX_OAUTH2_ACCESS_TOKEN_TTL_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(3600);
        let refresh_token_ttl_secs = std::env::var("WEBCODEX_OAUTH2_REFRESH_TOKEN_TTL_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(2_592_000);
        let authorization_code_ttl_secs = std::env::var("WEBCODEX_OAUTH2_AUTH_CODE_TTL_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(300);
        let require_pkce = env_flag("WEBCODEX_OAUTH2_REQUIRE_PKCE").unwrap_or(true);
        let shared_key_bridge_enabled =
            env_flag("WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE").unwrap_or(false);
        let trusted_mcp_file_client_ids =
            std::env::var("WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS")
                .ok()
                .map(|value| {
                    let mut client_ids = Vec::new();
                    for value in value.split(',') {
                        let Some(client_id) = normalize_trusted_mcp_file_client_id(value) else {
                            continue;
                        };
                        if !client_ids.contains(&client_id) {
                            client_ids.push(client_id);
                        }
                    }
                    client_ids
                })
                .unwrap_or_default();
        let project_share_grant_id = std::env::var("WEBCODEX_OAUTH2_PROJECT_SHARE_GRANT_ID")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let project_share_session_id = std::env::var("WEBCODEX_OAUTH2_PROJECT_SHARE_SESSION_ID")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Self {
            issuer,
            enabled,
            access_token_ttl_secs,
            refresh_token_ttl_secs,
            authorization_code_ttl_secs,
            require_pkce,
            shared_key_bridge_enabled,
            trusted_mcp_file_client_ids,
            project_share_grant_id,
            project_share_session_id,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            addr: std::env::var("WEBCODEX_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            data_dir: std::env::var("WEBCODEX_DATA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./data")),
            token: std::env::var("WEBCODEX_TOKEN").ok(),
            max_text_size: 2 * 1024 * 1024,
            oauth2: OAuth2Config::from_env(),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("webcodex.db")
    }

    pub fn session_ledger_path(&self) -> PathBuf {
        if self.data_dir.is_absolute() {
            return self.data_dir.join("sessions.json");
        }
        runtime_state_dir().join("sessions.json")
    }

    pub fn runtime_state_dir(&self) -> PathBuf {
        if self.data_dir.is_absolute() {
            return self.data_dir.clone();
        }
        runtime_state_dir()
    }

    pub fn is_auth_enabled(&self) -> bool {
        self.token.is_some()
    }

    pub fn validate_token(&self, token: &str) -> bool {
        self.token
            .as_ref()
            .map(|t| constant_time_eq(t.as_bytes(), token.as_bytes()))
            .unwrap_or(false)
    }
}

pub(crate) fn runtime_state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("webcodex");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/webcodex");
    }
    std::env::temp_dir().join("webcodex")
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for i in 0..max_len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        diff |= (av ^ bv) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quic_server_config_defaults_to_disabled() {
        let cfg = QuicServerConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.listen, "0.0.0.0:8443");
        assert_eq!(cfg.alpn, "webcodex-runner/1");
        // A disabled config is always valid (no cert/key required).
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn quic_server_config_enabled_requires_cert_and_key_paths() {
        // Missing cert path.
        let cfg = QuicServerConfig {
            enabled: true,
            listen: "0.0.0.0:8443".to_string(),
            cert: PathBuf::new(),
            key: PathBuf::from("/tmp/key.pem"),
            alpn: "webcodex-runner/1".to_string(),
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("WEBCODEX_QUIC_CERT"), "err was: {err}");

        // Missing key path.
        let cfg = QuicServerConfig {
            enabled: true,
            listen: "0.0.0.0:8443".to_string(),
            cert: PathBuf::from("/tmp/cert.pem"),
            key: PathBuf::new(),
            alpn: "webcodex-runner/1".to_string(),
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("WEBCODEX_QUIC_KEY"), "err was: {err}");
    }

    #[test]
    fn quic_server_config_enabled_rejects_nonexistent_paths() {
        let cfg = QuicServerConfig {
            enabled: true,
            listen: "0.0.0.0:8443".to_string(),
            cert: PathBuf::from("/definitely/does/not/exist/cert.pem"),
            key: PathBuf::from("/definitely/does/not/exist/key.pem"),
            alpn: "webcodex-runner/1".to_string(),
        };
        let err = cfg.validate().unwrap_err();
        // Names the missing path without dumping file contents.
        assert!(err.contains("does not exist"), "err was: {err}");
        assert!(!err.contains("BEGIN PRIVATE KEY"));
        assert!(!err.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn quic_runtime_error_sanitizer_removes_cert_and_key_paths() {
        let cert = sanitize_quic_runtime_error(
            "WEBCODEX_QUIC_CERT path does not exist: /etc/letsencrypt/live/example/fullchain.pem",
        );
        assert_eq!(cert, "WEBCODEX_QUIC_CERT path does not exist");
        assert!(!cert.contains("/etc/letsencrypt"));

        let key = sanitize_quic_runtime_error(
            "failed to parse QUIC key /etc/letsencrypt/live/example/privkey.pem: bad pem",
        );
        assert_eq!(key, "failed to parse QUIC key");
        assert!(!key.contains("privkey.pem"));
    }

    #[test]
    fn quic_server_config_from_env_disabled_by_default() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.remove("WEBCODEX_QUIC_ENABLED");
        env.remove("WEBCODEX_QUIC_LISTEN");
        env.remove("WEBCODEX_QUIC_CERT");
        env.remove("WEBCODEX_QUIC_KEY");
        env.remove("WEBCODEX_QUIC_ALPN");
        let cfg = QuicServerConfig::from_env();
        assert!(!cfg.enabled);
        assert_eq!(cfg.listen, "0.0.0.0:8443");
        assert_eq!(cfg.alpn, "webcodex-runner/1");
    }

    #[test]
    fn constant_time_eq_matches_byte_equality() {
        assert!(constant_time_eq(b"secret123", b"secret123"));
        assert!(!constant_time_eq(b"secret123", b"secret124"));
        assert!(!constant_time_eq(b"secret123", b"secret1234"));
        assert!(!constant_time_eq(b"secret123", b""));
    }

    #[test]
    fn oauth2_config_defaults_to_disabled() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.remove("WEBCODEX_SHARED_KEY_ENABLED");
        env.remove("WEBCODEX_ALLOW_ANONYMOUS");
        env.remove("WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE");
        env.remove("WEBCODEX_OAUTH2_ENABLED");
        env.remove("WEBCODEX_PUBLIC_URL");
        env.remove("WEBCODEX_OAUTH2_ISSUER");
        env.remove("WEBCODEX_OAUTH2_ACCESS_TOKEN_TTL_SECS");
        env.remove("WEBCODEX_OAUTH2_REFRESH_TOKEN_TTL_SECS");
        env.remove("WEBCODEX_OAUTH2_AUTH_CODE_TTL_SECS");
        env.remove("WEBCODEX_OAUTH2_REQUIRE_PKCE");
        env.remove("WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE");
        env.remove("WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS");

        let cfg = OAuth2Config::from_env();
        assert!(!cfg.enabled);
        assert!(cfg.issuer.is_none());
        assert_eq!(cfg.access_token_ttl_secs, 3600);
        assert_eq!(cfg.refresh_token_ttl_secs, 2_592_000);
        assert_eq!(cfg.authorization_code_ttl_secs, 300);
        assert!(cfg.require_pkce);
        assert!(!cfg.shared_key_bridge_enabled);
        assert!(cfg.trusted_mcp_file_client_ids.is_empty());
    }

    #[test]
    fn oauth2_config_from_env_parses_overrides() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_OAUTH2_ENABLED", "true");
        env.set("WEBCODEX_OAUTH2_ISSUER", "https://example.com");
        env.set("WEBCODEX_OAUTH2_ACCESS_TOKEN_TTL_SECS", "1800");
        env.set("WEBCODEX_OAUTH2_REFRESH_TOKEN_TTL_SECS", "86400");
        env.set("WEBCODEX_OAUTH2_AUTH_CODE_TTL_SECS", "600");
        env.set("WEBCODEX_OAUTH2_REQUIRE_PKCE", "false");
        env.set("WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE", "true");
        let trusted_a = format!("wc_client_{}", "a".repeat(64));
        let trusted_b = format!("wc_client_{}", "b".repeat(64));
        env.set(
            "WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS",
            format!(
                " {trusted_a} , invalid-client, {trusted_a}, {trusted_b}, wc_client_{} ",
                "A".repeat(64)
            ),
        );

        let cfg = OAuth2Config::from_env();
        assert!(cfg.enabled);
        assert_eq!(cfg.issuer.as_deref(), Some("https://example.com"));
        assert_eq!(cfg.access_token_ttl_secs, 1800);
        assert_eq!(cfg.refresh_token_ttl_secs, 86400);
        assert_eq!(cfg.authorization_code_ttl_secs, 600);
        assert!(!cfg.require_pkce);
        assert!(cfg.shared_key_bridge_enabled);
        assert_eq!(cfg.trusted_mcp_file_client_ids, vec![trusted_a, trusted_b]);

        env.remove("WEBCODEX_OAUTH2_ENABLED");
        env.remove("WEBCODEX_OAUTH2_ISSUER");
        env.remove("WEBCODEX_OAUTH2_ACCESS_TOKEN_TTL_SECS");
        env.remove("WEBCODEX_OAUTH2_REFRESH_TOKEN_TTL_SECS");
        env.remove("WEBCODEX_OAUTH2_AUTH_CODE_TTL_SECS");
        env.remove("WEBCODEX_OAUTH2_REQUIRE_PKCE");
        env.remove("WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS");
    }

    #[test]
    fn oauth2_config_issuer_prefers_oauth2_issuer_over_public_url() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_PUBLIC_URL", "https://pub.example.com");
        env.set("WEBCODEX_OAUTH2_ISSUER", "https://issuer.example.com");

        let cfg = OAuth2Config::from_env();
        assert_eq!(cfg.issuer.as_deref(), Some("https://issuer.example.com"));

        env.remove("WEBCODEX_OAUTH2_ISSUER");
        // Falls back to WEBCODEX_PUBLIC_URL when OAUTH2_ISSUER is absent.
        let cfg = OAuth2Config::from_env();
        assert_eq!(cfg.issuer.as_deref(), Some("https://pub.example.com"));

        env.remove("WEBCODEX_PUBLIC_URL");
    }

    #[test]
    fn load_startup_env_files_explicit_path_loads_webcodex_env() {
        let mut env = crate::test_support::TestEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let new_file = dir.path().join("webcodex.env");
        std::fs::write(&new_file, "WEBCODEX_TOKEN=new\n").unwrap();
        env.set("WEBCODEX_ENV_FILE", &new_file);
        env.remove("WEBCODEX_TOKEN");

        let loads = load_startup_env_files().unwrap();
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0].path, new_file);
        assert_eq!(std::env::var("WEBCODEX_TOKEN").unwrap(), "new");

        env.remove("WEBCODEX_ENV_FILE");
    }

    #[test]
    fn startup_env_precedence_is_default_then_file_then_process_environment() {
        let mut env = crate::test_support::TestEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("webcodex.env");
        std::fs::write(&env_file, "WEBCODEX_MCP_COMPACT_SCHEMAS=true\n").unwrap();
        env.set("WEBCODEX_ENV_FILE", &env_file);
        env.remove("WEBCODEX_MCP_COMPACT_SCHEMAS");

        assert_eq!(mcp_compact_schemas_override(), None);
        let from_file = load_startup_env_files().unwrap();
        assert_eq!(from_file[0].loaded_count, 1);
        assert_eq!(mcp_compact_schemas_override(), Some(true));

        env.set("WEBCODEX_MCP_COMPACT_SCHEMAS", "false");
        let with_process_override = load_startup_env_files().unwrap();
        assert_eq!(with_process_override[0].loaded_count, 0);
        assert_eq!(mcp_compact_schemas_override(), Some(false));

        env.remove("WEBCODEX_ENV_FILE");
        env.remove("WEBCODEX_MCP_COMPACT_SCHEMAS");
    }
    #[test]
    fn mcp_compact_schemas_override_distinguishes_unset_true_and_false() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.remove("WEBCODEX_MCP_COMPACT_SCHEMAS");
        assert_eq!(mcp_compact_schemas_override(), None);
        env.set("WEBCODEX_MCP_COMPACT_SCHEMAS", "true");
        assert_eq!(mcp_compact_schemas_override(), Some(true));
        env.set("WEBCODEX_MCP_COMPACT_SCHEMAS", "1");
        assert_eq!(mcp_compact_schemas_override(), Some(true));
        env.set("WEBCODEX_MCP_COMPACT_SCHEMAS", "false");
        assert_eq!(mcp_compact_schemas_override(), Some(false));
        env.set("WEBCODEX_MCP_COMPACT_SCHEMAS", "maybe");
        // Invalid values are treated as unset by env_flag and defer to the
        // RuntimeExposure-specific default.
        assert_eq!(mcp_compact_schemas_override(), None);
        env.remove("WEBCODEX_MCP_COMPACT_SCHEMAS");
    }

    #[test]
    fn mcp_apps_default_on_and_can_be_disabled() {
        assert!(mcp_apps_enabled_from_flag(None));
        assert!(mcp_apps_enabled_from_flag(Some(true)));
        assert!(!mcp_apps_enabled_from_flag(Some(false)));
    }

    #[test]
    fn tool_request_trace_defaults_off() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.remove("WEBCODEX_TOOL_REQUEST_TRACE");
        assert_eq!(tool_request_trace_mode(), ToolRequestTraceMode::Off);
        assert!(!tool_request_trace_enabled());
    }

    #[test]
    fn tool_request_trace_modes_preserve_true_as_metadata() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "true");
        assert_eq!(tool_request_trace_mode(), ToolRequestTraceMode::Metadata);
        assert!(tool_request_trace_enabled());
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "metadata");
        assert_eq!(tool_request_trace_mode(), ToolRequestTraceMode::Metadata);
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        assert_eq!(tool_request_trace_mode(), ToolRequestTraceMode::Full);
        assert!(tool_request_trace_enabled());
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "false");
        assert_eq!(tool_request_trace_mode(), ToolRequestTraceMode::Off);
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "maybe");
        assert_eq!(tool_request_trace_mode(), ToolRequestTraceMode::Off);
        env.remove("WEBCODEX_TOOL_REQUEST_TRACE");
    }

    #[test]
    fn tool_request_trace_test_env_is_visible_only_to_opted_in_threads() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        assert_eq!(tool_request_trace_mode(), ToolRequestTraceMode::Full);

        let unrelated = std::thread::spawn(tool_request_trace_mode).join().unwrap();
        assert_eq!(unrelated, ToolRequestTraceMode::Off);

        let explicit_reader = std::thread::spawn(|| {
            let _reader = crate::test_support::TestToolRequestTraceEnvReaderGuard::new();
            tool_request_trace_mode()
        })
        .join()
        .unwrap();
        assert_eq!(explicit_reader, ToolRequestTraceMode::Full);
    }
}
