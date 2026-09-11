use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};
use webcodex_admin::{build_server_http_client, ServerHttpOptions};

/// A single authenticated JSON POST against the server. Reuses
/// `build_admin_request` to construct the path/body for known admin commands,
/// but accepts arbitrary `(path, body)` so setup can issue its own calls.
pub(crate) struct ApiCall<'a> {
    pub(crate) server_url: &'a str,
    pub(crate) server_http: &'a ServerHttpOptions,
    pub(crate) token: &'a str,
    pub(crate) path: &'a str,
    pub(crate) body: Value,
}

pub(crate) async fn post_json_authed(call: ApiCall<'_>) -> Result<Value, String> {
    let url = format!("{}{}", call.server_url.trim_end_matches('/'), call.path);
    let client = build_server_http_client(call.server_http)?;
    let resp = client
        .post(url)
        .bearer_auth(call.token)
        .json(&call.body)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e).replace(call.token, "[redacted]"))?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response: {}", e))?;
    if !status.is_success() {
        return Err(format_error_body(status.as_u16(), &content_type, &text)
            .replace(call.token, "[redacted]"));
    }
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "failed to parse JSON response: {} (content-type: {})",
            e, content_type
        )
    })
}

pub(crate) async fn post_json_unauthed(
    server_url: &str,
    server_http: &ServerHttpOptions,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    let url = format!("{}{}", server_url.trim_end_matches('/'), path);
    let client = build_server_http_client(server_http)?;
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e))?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response: {}", e))?;
    if !status.is_success() {
        return Err(format_error_body(status.as_u16(), &content_type, &text));
    }
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "failed to parse JSON response: {} (content-type: {})",
            e, content_type
        )
    })
}

/// Format an error response without echoing the bearer token. For JSON
/// errors, surface the server's `error` field (sanitized). For non-JSON
/// errors, report status + content-type only (never the body).
pub(crate) fn format_error_body(status: u16, content_type: &str, body: &str) -> String {
    if content_type
        .split(';')
        .next()
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("application/json"))
    {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            if let Some(error) = value.get("error").and_then(Value::as_str) {
                return format!("request failed: HTTP {}: {}", status, error);
            }
            return format!("request failed: HTTP {}: {}", status, value);
        }
    }
    format!(
        "request failed: HTTP {} (content-type: {})",
        status, content_type
    )
}

pub(crate) async fn http_post_json_status(
    server_url: &str,
    server_http: &ServerHttpOptions,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> Result<(u16, String, Option<Value>), String> {
    let url = format!("{}{}", server_url.trim_end_matches('/'), path);
    let client = build_server_http_client(server_http)?;
    let mut req = client.post(url).json(&body);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| {
        let error = format!("request failed: {}", e);
        token.map_or_else(|| error.clone(), |token| error.replace(token, "[redacted]"))
    })?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let text = resp.text().await.map_err(|e| {
        let error = format!("failed to read response: {}", e);
        token.map_or_else(|| error.clone(), |token| error.replace(token, "[redacted]"))
    })?;
    let json = if content_type
        .split(';')
        .next()
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("application/json"))
    {
        serde_json::from_str::<Value>(&text).ok()
    } else {
        None
    };
    Ok((status, content_type, json))
}

pub(crate) async fn call_runtime_tool_status(
    server_url: &str,
    server_http: &ServerHttpOptions,
    token: Option<&str>,
    tool: &str,
    params: Value,
) -> Result<(u16, String, Option<Value>), String> {
    http_post_json_status(
        server_url,
        server_http,
        "/api/tools/call",
        token,
        json!({"tool": tool, "params": params}),
    )
    .await
}

#[derive(Debug, Clone)]
pub(crate) struct HttpStatusSummary {
    pub(crate) reachable: bool,
    pub(crate) status_code: Option<u16>,
    pub(crate) content_type: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) output: Option<Value>,
}

pub(crate) async fn fetch_runtime_status(
    url: &str,
    server_http: &ServerHttpOptions,
    token: Option<&str>,
) -> Result<HttpStatusSummary, String> {
    let endpoint = format!("{}/api/runtime/status", url.trim_end_matches('/'));
    let client = build_server_http_client(server_http)?;
    let mut req = client.post(endpoint).json(&json!({}));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            let error = format!("request failed: {}", e);
            return Ok(HttpStatusSummary {
                reachable: false,
                status_code: None,
                content_type: None,
                error: Some(
                    token.map_or_else(|| error.clone(), |token| error.replace(token, "[redacted]")),
                ),
                output: None,
            });
        }
    };
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    if !status.is_success() {
        return Ok(HttpStatusSummary {
            reachable: false,
            status_code: Some(status.as_u16()),
            content_type: Some(content_type),
            error: None,
            output: None,
        });
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response: {}", e))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "failed to parse JSON response: {} (content-type: {})",
            e, content_type
        )
    })?;
    let output = value.get("output").cloned().or(Some(value));
    Ok(HttpStatusSummary {
        reachable: true,
        status_code: Some(status.as_u16()),
        content_type: Some(content_type),
        error: None,
        output,
    })
}
