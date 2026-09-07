//! Stable, host- or transport-owned chat-window identity.
//!
//! The raw host/transport value is never persisted or returned by a tool. Runtime
//! and connector state use only the domain-separated SHA-256 key below, always
//! together with the authenticated subject and exact project identity.

use salvo::http::header::{HeaderValue, SET_COOKIE};
use salvo::prelude::{Request, Response};
use sha2::{Digest, Sha256};

pub(crate) const MCP_SESSION_HEADER: &str = "mcp-session-id";
const OPENAI_CONVERSATION_HEADER: &str = "openai-conversation-id";
const WINDOW_COOKIE: &str = "webcodex_window";
const WINDOW_COOKIE_MAX_AGE_SECS: i64 = 60 * 60 * 24 * 90;
const MAX_OPAQUE_ID_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientWindow {
    key: String,
    source: &'static str,
}

impl ClientWindow {
    pub(crate) fn from_opaque(source: &'static str, value: &str) -> Option<Self> {
        let value = valid_opaque_id(value)?;
        let mut hasher = Sha256::new();
        hasher.update(b"webcodex.client-window.v1\0");
        hasher.update(source.as_bytes());
        hasher.update(b"\0");
        hasher.update(value.as_bytes());
        Some(Self {
            key: format!("{:x}", hasher.finalize()),
            source,
        })
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn source(&self) -> &'static str {
        self.source
    }

    #[cfg(test)]
    pub(crate) fn for_test(value: &str) -> Self {
        Self::from_opaque("test", value).expect("test window identity")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct McpWindow {
    pub(crate) identity: Option<ClientWindow>,
    /// Newly minted protocol session id returned on initialize.
    pub(crate) issued_session_id: Option<String>,
}

/// Resolve an MCP window. Protocol version 2025-06-18 lets the server mint a
/// session id during initialize; subsequent requests echo it in the header.
/// A tools/call without that header deliberately gets no continuity identity
/// instead of falling back to a credential-wide key.
pub(crate) fn mcp_window(req: &Request, initialize: bool) -> McpWindow {
    if let Some(raw) = request_header(req, MCP_SESSION_HEADER) {
        return McpWindow {
            identity: ClientWindow::from_opaque("mcp", raw),
            issued_session_id: None,
        };
    }
    if !initialize {
        return McpWindow {
            identity: None,
            issued_session_id: None,
        };
    }
    let raw = format!("wc_mcp_{}", uuid::Uuid::new_v4().simple());
    McpWindow {
        identity: ClientWindow::from_opaque("mcp", &raw),
        issued_session_id: Some(raw),
    }
}

/// Resolve stateless ChatGPT MCP conversation identity from host-provided
/// request metadata. The opaque OpenAI value is domain-separated and hashed
/// immediately; it is never persisted or returned raw. Missing or malformed
/// metadata deliberately yields no continuity identity instead of falling back
/// to auth-wide or transport-wide inference.
pub(crate) fn stateless_mcp_window(params: &serde_json::Value) -> McpWindow {
    let identity = params
        .get("_meta")
        .and_then(|meta| meta.get("openai/session"))
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| ClientWindow::from_opaque("openai-session", raw));
    McpWindow {
        identity,
        issued_session_id: None,
    }
}

pub(crate) fn set_mcp_session_header(res: &mut Response, session_id: &str) {
    if let Ok(value) = HeaderValue::from_str(session_id) {
        res.headers_mut().insert(MCP_SESSION_HEADER, value);
    }
}

/// Resolve an HTTP/API window without adding a public operation or request
/// field. Hosted Actions provide a conversation-scoped header; first-party
/// HTTP clients otherwise receive an opaque HttpOnly cookie and must use one
/// cookie jar per logical window.
pub(crate) fn api_window(req: &Request, res: &mut Response) -> ClientWindow {
    if let Some(raw) = request_header(req, OPENAI_CONVERSATION_HEADER) {
        if let Some(window) = ClientWindow::from_opaque("openai-conversation", raw) {
            return window;
        }
    }
    if let Some(raw) = request_cookie(req, WINDOW_COOKIE) {
        if let Some(window) = ClientWindow::from_opaque("http-cookie", raw) {
            return window;
        }
    }

    let raw = format!("wc_win_{}", uuid::Uuid::new_v4().simple());
    let window =
        ClientWindow::from_opaque("http-cookie", &raw).expect("generated window id is valid");
    let mut cookie = format!(
        "{WINDOW_COOKIE}={raw}; Max-Age={WINDOW_COOKIE_MAX_AGE_SECS}; Path=/; HttpOnly; SameSite=Lax"
    );
    if request_is_https(req) {
        cookie.push_str("; Secure");
    }
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(SET_COOKIE, value);
    }
    window
}

fn request_header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(valid_opaque_id)
}

fn request_cookie<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    let header = req.headers().get("cookie")?.to_str().ok()?;
    header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value).and_then(valid_opaque_id)
    })
}

fn valid_opaque_id(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= MAX_OPAQUE_ID_BYTES
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)))
    .then_some(value)
}

fn request_is_https(req: &Request) -> bool {
    request_header(req, "x-forwarded-proto")
        .is_some_and(|value| value.eq_ignore_ascii_case("https"))
        || req.uri().scheme_str() == Some("https")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_window_values_are_domain_separated_and_not_retained() {
        let mcp = ClientWindow::from_opaque("mcp", "same-value").unwrap();
        let http = ClientWindow::from_opaque("http-cookie", "same-value").unwrap();
        assert_ne!(mcp.key(), http.key());
        assert!(!mcp.key().contains("same-value"));
        assert_eq!(mcp.key().len(), 64);
    }

    #[test]
    fn malformed_or_oversized_ids_are_rejected() {
        assert!(ClientWindow::from_opaque("mcp", "").is_none());
        assert!(ClientWindow::from_opaque("mcp", "contains space").is_none());
        assert!(ClientWindow::from_opaque("mcp", &"x".repeat(257)).is_none());
    }

    #[test]
    fn stateless_mcp_window_uses_openai_session_metadata() {
        let params = serde_json::json!({
            "name": "runtime_status",
            "arguments": {},
            "_meta": {"openai/session": "chat-session-opaque-value"}
        });

        let first = stateless_mcp_window(&params);
        let second = stateless_mcp_window(&params);
        let identity = first
            .identity
            .expect("OpenAI session should yield window identity");
        let repeated = second
            .identity
            .expect("same OpenAI session should yield identity");

        assert_eq!(identity.key(), repeated.key());
        assert_eq!(identity.source(), "openai-session");
        assert!(!identity.key().contains("chat-session-opaque-value"));
        assert!(first.issued_session_id.is_none());
    }

    #[test]
    fn stateless_mcp_window_without_valid_openai_session_stays_unidentified() {
        for params in [
            serde_json::json!({"name": "runtime_status", "arguments": {}}),
            serde_json::json!({"_meta": {}}),
            serde_json::json!({"_meta": {"openai/session": ""}}),
            serde_json::json!({"_meta": {"openai/session": 42}}),
            serde_json::json!({"_meta": {"openai/session": "contains space"}}),
        ] {
            let window = stateless_mcp_window(&params);
            assert!(
                window.identity.is_none(),
                "unexpected identity for {params}"
            );
            assert!(window.issued_session_id.is_none());
        }
    }
}
