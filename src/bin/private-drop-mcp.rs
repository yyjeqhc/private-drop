use reqwest::blocking::Client;
use serde_json::{json, Map, Value};
use std::env;
use std::io::{self, BufRead, Write};
use std::time::Duration;

const SERVER_NAME: &str = "private-drop-mcp";
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const FALLBACK_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

#[derive(Debug, Clone)]
struct McpServer {
    http: Client,
    base_url: String,
    token: Option<String>,
    default_action_session_id: Option<String>,
}

#[derive(Debug, Clone)]
struct McpError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl McpError {
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
            data: None,
        }
    }

    fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

type McpResult = Result<Value, McpError>;

fn main() {
    if handle_cli_flag() {
        return;
    }
    let server = match McpServer::from_env() {
        Ok(server) => server,
        Err(err) => {
            let response = jsonrpc_error(Value::Null, err.code, &err.message, err.data);
            let _ = writeln!(io::stdout(), "{}", response);
            return;
        }
    };
    server.serve_stdio();
}

fn handle_cli_flag() -> bool {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{} {}", SERVER_NAME, env!("CARGO_PKG_VERSION"));
        return true;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "{SERVER_NAME} {}\n\nMCP stdio bridge for Private Drop.\n\nEnvironment:\n  PRIVATE_DROP_MCP_HTTP_BASE        Private Drop base URL (default: {DEFAULT_BASE_URL})\n  PRIVATE_DROP_MCP_TOKEN            Bearer token override\n  DROP_TOKEN                        Bearer token fallback\n  PRIVATE_DROP_MCP_ACTION_SESSION_ID Optional default audit session id\n  PRIVATE_DROP_MCP_TIMEOUT_SECS     HTTP timeout (default: {DEFAULT_TIMEOUT_SECS})",
            env!("CARGO_PKG_VERSION")
        );
        return true;
    }
    false
}

impl McpServer {
    fn from_env() -> Result<Self, McpError> {
        let base_url = env::var("PRIVATE_DROP_MCP_HTTP_BASE")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let timeout_secs = env::var("PRIVATE_DROP_MCP_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let token = env_first(["PRIVATE_DROP_MCP_TOKEN", "DROP_TOKEN"]);
        let default_action_session_id = env_first(["PRIVATE_DROP_MCP_ACTION_SESSION_ID"]);
        let http = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|err| McpError::internal(format!("Failed to build HTTP client: {err}")))?;
        Ok(Self {
            http,
            base_url,
            token,
            default_action_session_id,
        })
    }

    fn serve_stdio(&self) {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<Value>(&line) {
                Ok(value) => self.handle_jsonrpc_value(value),
                Err(err) => Some(jsonrpc_error(
                    Value::Null,
                    -32700,
                    "Parse error",
                    Some(json!({ "detail": err.to_string() })),
                )),
            };
            if let Some(response) = response {
                if writeln!(stdout, "{}", response).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        }
    }

    fn handle_jsonrpc_value(&self, value: Value) -> Option<Value> {
        if let Value::Array(items) = value {
            if items.is_empty() {
                return Some(jsonrpc_error(
                    Value::Null,
                    -32600,
                    "Invalid Request",
                    Some(json!({ "detail": "Batch must not be empty" })),
                ));
            }
            let responses = items
                .into_iter()
                .filter_map(|item| self.handle_jsonrpc_value(item))
                .collect::<Vec<_>>();
            if responses.is_empty() {
                None
            } else {
                Some(Value::Array(responses))
            }
        } else {
            self.handle_jsonrpc_request(value)
        }
    }

    fn handle_jsonrpc_request(&self, value: Value) -> Option<Value> {
        let Some(object) = value.as_object() else {
            return Some(jsonrpc_error(
                Value::Null,
                -32600,
                "Invalid Request",
                Some(json!({ "detail": "Request must be a JSON object" })),
            ));
        };
        let id = object.get("id").cloned();
        let method = object.get("method").and_then(Value::as_str);
        let Some(method) = method else {
            return id.map(|id| {
                jsonrpc_error(
                    id,
                    -32600,
                    "Invalid Request",
                    Some(json!({ "detail": "method is required" })),
                )
            });
        };
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        if id.is_none() {
            let _ = self.handle_notification(method, params);
            return None;
        }
        let id = id.unwrap();
        match self.handle_method(method, params) {
            Ok(result) => Some(jsonrpc_result(id, result)),
            Err(err) => Some(jsonrpc_error(id, err.code, &err.message, err.data)),
        }
    }

    fn handle_notification(&self, method: &str, _params: Value) -> McpResult {
        match method {
            "notifications/initialized" | "notifications/cancelled" => Ok(json!({})),
            _ => Ok(json!({})),
        }
    }

    fn handle_method(&self, method: &str, params: Value) -> McpResult {
        match method {
            "initialize" => Ok(initialize_result(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.handle_tools_call(params),
            "resources/list" => Ok(json!({ "resources": resources() })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "resources/read" => self.handle_resources_read(params),
            "prompts/list" => Ok(json!({ "prompts": prompts() })),
            "prompts/get" => handle_prompts_get(params),
            _ => Err(McpError::method_not_found(method)),
        }
    }

    fn handle_tools_call(&self, params: Value) -> McpResult {
        let params = params
            .as_object()
            .ok_or_else(|| McpError::invalid_params("tools/call params must be an object"))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::invalid_params("tools/call params.name is required"))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut body = object_from_value(arguments)?;
        let session_id = remove_optional_string(&mut body, "action_session_id")
            .or_else(|| self.default_action_session_id.clone());
        let path = match name {
            "list_projects" => "/api/codex/projects",
            "get_project_context_batch" => "/api/codex/context_batch",
            "apply_project_edit" => "/api/codex/edit",
            "save_artifact" => "/api/codex/artifact",
            "run_project_git" => "/api/codex/git",
            "run_command_request_op" => "/api/codex/command_request_op",
            "run_job_op" => "/api/codex/job",
            "run_project_check" => "/api/codex/check",
            "write_report" => "/api/codex/report",
            "action_session_op" => "/api/codex/action_sessions",
            _ => return Err(McpError::invalid_params(format!("Unknown tool: {name}"))),
        };
        Ok(self.tool_post(path, Value::Object(body), session_id))
    }

    fn handle_resources_read(&self, params: Value) -> McpResult {
        let params = params
            .as_object()
            .ok_or_else(|| McpError::invalid_params("resources/read params must be an object"))?;
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::invalid_params("resources/read params.uri is required"))?;
        let (mime_type, text) = match uri {
            "private-drop://projects" => (
                "application/json",
                self.http_post_text(
                    "/api/codex/projects",
                    &json!({}),
                    self.default_action_session_id.as_deref(),
                )?,
            ),
            "private-drop://schema/gpt" => (
                "application/json",
                self.http_get_text("/codex-openapi-gpt.json")?,
            ),
            "private-drop://schema/compact" => (
                "application/json",
                self.http_get_text("/codex-openapi-compact.json")?,
            ),
            "private-drop://workflow" => ("text/markdown", workflow_resource_text().to_string()),
            _ => {
                return Err(McpError::invalid_params(format!(
                    "Unknown resource URI: {uri}"
                )))
            }
        };
        Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": mime_type,
                "text": text
            }]
        }))
    }

    fn tool_post(&self, path: &str, body: Value, session_id: Option<String>) -> Value {
        match self.http_post_json(path, &body, session_id.as_deref()) {
            Ok((status, payload)) if (200..300).contains(&status) => tool_text(payload, false),
            Ok((status, payload)) => tool_text(
                json!({
                    "success": false,
                    "status": status,
                    "error": "Private Drop HTTP request failed",
                    "body": payload
                }),
                true,
            ),
            Err(err) => tool_text(
                json!({
                    "success": false,
                    "error": err.message,
                    "data": err.data
                }),
                true,
            ),
        }
    }

    fn http_post_json(
        &self,
        path: &str,
        body: &Value,
        session_id: Option<&str>,
    ) -> Result<(u16, Value), McpError> {
        let url = self.url(path);
        let mut request = self.http.post(&url).json(body);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        if let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) {
            request = request.header("X-Action-Session-Id", session_id.trim());
        }
        let response = request
            .send()
            .map_err(|err| McpError::internal(format!("HTTP POST failed: {err}")))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .map_err(|err| McpError::internal(format!("HTTP response read failed: {err}")))?;
        Ok((status, parse_response_text(&text)))
    }

    fn http_post_text(
        &self,
        path: &str,
        body: &Value,
        session_id: Option<&str>,
    ) -> Result<String, McpError> {
        let (status, payload) = self.http_post_json(path, body, session_id)?;
        if !(200..300).contains(&status) {
            return Err(
                McpError::internal("Private Drop HTTP request failed").with_data(json!({
                    "status": status,
                    "body": payload
                })),
            );
        }
        serde_json::to_string(&payload)
            .map_err(|err| McpError::internal(format!("JSON encode failed: {err}")))
    }

    fn http_get_text(&self, path: &str) -> Result<String, McpError> {
        let url = self.url(path);
        let mut request = self.http.get(&url);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .map_err(|err| McpError::internal(format!("HTTP GET failed: {err}")))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .map_err(|err| McpError::internal(format!("HTTP response read failed: {err}")))?;
        if !(200..300).contains(&status) {
            return Err(
                McpError::internal("Private Drop HTTP request failed").with_data(json!({
                    "status": status,
                    "body": parse_response_text(&text)
                })),
            );
        }
        Ok(text)
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{}", self.base_url, path)
        } else {
            format!("{}/{}", self.base_url, path)
        }
    }
}

fn env_first<const N: usize>(names: [&str; N]) -> Option<String> {
    names.into_iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn jsonrpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({
        "code": code,
        "message": message
    });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error
    })
}

fn initialize_result(params: Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(FALLBACK_PROTOCOL_VERSION);
    let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        FALLBACK_PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": protocol_version,
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION")
        },
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "listChanged": false },
            "prompts": { "listChanged": false }
        },
        "instructions": "Use list_projects first, then batch context reads. Prefer summary/minimal response modes and action_session_id for multi-step work."
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "list_projects",
            "description": "List configured projects, capabilities, checks, commands, and instance metadata.",
            "inputSchema": object_schema(json!({
                "action_session_id": string_schema("Optional audit session id.")
            }), vec![]),
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "get_project_context_batch",
            "description": "Batch project reads: overview, tree, search, grep_context, read_file, git_status, git_diff, outlines, outputs.",
            "inputSchema": object_schema(json!({
                "project": string_schema("Runtime project name."),
                "requests": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "mode": {
                                "type": "string",
                                "enum": ["overview", "tree", "search", "grep_context", "read_file", "markdown_outline", "read_section", "agent_context", "git_status", "git_diff", "experiment_outputs"]
                            },
                            "path": { "type": "string" },
                            "query": { "type": "string" },
                            "if_fingerprint": { "type": "string" },
                            "start_line": { "type": "integer", "minimum": 1 },
                            "limit": { "type": "integer", "minimum": 1 },
                            "max_depth": { "type": "integer", "minimum": 1 }
                        },
                        "required": ["mode"]
                    }
                },
                "max_total_chars": { "type": "integer", "minimum": 1 },
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["project", "requests"]),
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "apply_project_edit",
            "description": "Apply structured text or binary edits. Use dry_run and response_mode=summary for larger changes.",
            "inputSchema": object_schema(json!({
                "project": string_schema("Runtime project name."),
                "reason": string_schema("Short reason for audit history."),
                "dry_run": { "type": "boolean" },
                "response_mode": { "type": "string", "enum": ["full", "summary", "minimal"] },
                "expected_fingerprints": {
                    "type": "object",
                    "additionalProperties": { "type": "string" }
                },
                "edits": {
                    "type": "array",
                    "items": edit_operation_schema()
                },
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["project", "edits"]),
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        },
        {
            "name": "save_artifact",
            "description": "Save generated/uploaded/base64/URL artifacts into a project, optionally writing companion markdown.",
            "inputSchema": object_schema(json!({
                "project": string_schema("Runtime project name."),
                "op": { "type": "string", "enum": ["save_base64", "save_upload", "save_url", "save_generated"] },
                "path": string_schema("Project-relative output path."),
                "reason": string_schema("Short reason for audit history."),
                "allow_overwrite": { "type": "boolean" },
                "base64_content": { "type": "string" },
                "source_file": { "type": "string" },
                "file_id": { "type": "string" },
                "source_url": { "type": "string" },
                "chatgpt_estuary_url": { "type": "string" },
                "mime_type": { "type": "string" },
                "file_name": { "type": "string" },
                "alt_text": { "type": "string" },
                "companion_markdown_path": { "type": "string" },
                "companion_markdown_template": { "type": "string" },
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["project", "op", "path"]),
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        },
        {
            "name": "run_project_git",
            "description": "Run allowed git operations: status, diff, log, add, commit, or amend-no-edit.",
            "inputSchema": object_schema(json!({
                "project": string_schema("Runtime project name."),
                "operation": { "type": "string", "enum": ["status", "diff", "log", "add", "commit", "commit_amend_no_edit"] },
                "paths": { "type": "array", "items": { "type": "string" } },
                "message": { "type": "string" },
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["project", "operation"]),
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "run_command_request_op",
            "description": "Create, list, approve, reject, or run trusted raw command requests within project policy.",
            "inputSchema": object_schema(json!({
                "op": { "type": "string" },
                "project": { "type": "string" },
                "command": { "type": "string" },
                "command_text": { "type": "string" },
                "script_path": { "type": "string" },
                "script_args": { "type": "array", "items": { "type": "string" } },
                "script_text": { "type": "string" },
                "timeout_secs": { "type": "integer", "minimum": 1 },
                "response_mode": { "type": "string", "enum": ["summary", "full", "minimal"] },
                "reason": { "type": "string" },
                "title": { "type": "string" },
                "summary": { "type": "string" },
                "goal_id": { "type": "string" },
                "ttl_secs": { "type": "integer" },
                "requests": { "type": "array", "items": { "type": "object" } },
                "request_id": { "type": "string" },
                "request_ids": { "type": "array", "items": { "type": "string" } },
                "status": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1 },
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["op"]),
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "run_job_op",
            "description": "Create, poll, recover, stop, list, log, or summarize async jobs. Prefer status detail=basic.",
            "inputSchema": object_schema(json!({
                "op": { "type": "string" },
                "project": { "type": "string" },
                "goal_id": { "type": "string" },
                "job_id": { "type": "string" },
                "client_request_id": { "type": "string" },
                "suite": { "type": "string" },
                "command": { "type": "string" },
                "script_path": { "type": "string" },
                "script_args": { "type": "array", "items": { "type": "string" } },
                "script_text": { "type": "string" },
                "trusted": { "type": "boolean" },
                "commands": { "type": "array", "items": { "type": "string" } },
                "reason": { "type": "string" },
                "status": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1 },
                "tail_lines": { "type": "integer", "minimum": 1 },
                "max_runtime_secs": { "type": "integer", "minimum": 1 },
                "since_line": { "type": "integer", "minimum": 1 },
                "detail": { "type": "string", "enum": ["basic", "logs"] },
                "response_mode": { "type": "string", "enum": ["summary", "minimal"] },
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["op"]),
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "run_project_check",
            "description": "Run a configured project check suite.",
            "inputSchema": object_schema(json!({
                "project": string_schema("Runtime project name."),
                "suite": string_schema("Configured check suite name."),
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["project", "suite"]),
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "write_report",
            "description": "Write a completion/status report to Private Drop and the local reports directory.",
            "inputSchema": object_schema(json!({
                "project": string_schema("Runtime project name."),
                "status": string_schema("Short status, such as completed or blocked."),
                "title": string_schema("Report title."),
                "summary": string_schema("Report body."),
                "channel": { "type": "string", "default": "omo" },
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["project", "status", "title", "summary"]),
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "action_session_op",
            "description": "List, inspect, rename, close, or summarize recorded action sessions.",
            "inputSchema": object_schema(json!({
                "op": { "type": "string", "enum": ["list", "get", "events", "stats", "rename", "close"] },
                "session_id": { "type": "string" },
                "status": { "type": "string" },
                "title": { "type": "string" },
                "note": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1 },
                "action_session_id": string_schema("Optional audit session id.")
            }), vec!["op"]),
            "annotations": { "readOnlyHint": false }
        }
    ])
}

fn resources() -> Value {
    json!([
        {
            "uri": "private-drop://projects",
            "name": "Private Drop projects",
            "description": "Live project capability list from /api/codex/projects.",
            "mimeType": "application/json"
        },
        {
            "uri": "private-drop://schema/gpt",
            "name": "GPT Actions schema",
            "description": "The GPT-optimized OpenAPI schema served by Private Drop.",
            "mimeType": "application/json"
        },
        {
            "uri": "private-drop://schema/compact",
            "name": "Compact Codex schema",
            "description": "The compact Codex OpenAPI schema served by Private Drop.",
            "mimeType": "application/json"
        },
        {
            "uri": "private-drop://workflow",
            "name": "Workflow guidance",
            "description": "Concise workflow guidance for low-request Private Drop use.",
            "mimeType": "text/markdown"
        }
    ])
}

fn prompts() -> Value {
    json!([
        {
            "name": "project_startup",
            "description": "Start work with minimal project discovery and batched context.",
            "arguments": [
                { "name": "project", "description": "Optional target project name.", "required": false },
                { "name": "goal", "description": "Optional task goal.", "required": false }
            ]
        },
        {
            "name": "safe_edit_workflow",
            "description": "Read, fingerprint, edit, and verify without wasting calls.",
            "arguments": [
                { "name": "project", "description": "Target project name.", "required": false },
                { "name": "paths", "description": "Comma-separated paths.", "required": false }
            ]
        },
        {
            "name": "long_job_workflow",
            "description": "Create and poll async jobs with recovery and incremental logs.",
            "arguments": [
                { "name": "project", "description": "Target project name.", "required": false },
                { "name": "goal_id", "description": "Optional active goal id.", "required": false }
            ]
        },
        {
            "name": "final_report_workflow",
            "description": "Finish work with validation, git status, report, and session close.",
            "arguments": [
                { "name": "project", "description": "Target project name.", "required": false },
                { "name": "status", "description": "Final status.", "required": false }
            ]
        }
    ])
}

fn handle_prompts_get(params: Value) -> McpResult {
    let params = params
        .as_object()
        .ok_or_else(|| McpError::invalid_params("prompts/get params must be an object"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::invalid_params("prompts/get params.name is required"))?;
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let text = match name {
        "project_startup" => project_startup_prompt(&arguments),
        "safe_edit_workflow" => safe_edit_workflow_prompt(&arguments),
        "long_job_workflow" => long_job_workflow_prompt(&arguments),
        "final_report_workflow" => final_report_workflow_prompt(&arguments),
        _ => return Err(McpError::invalid_params(format!("Unknown prompt: {name}"))),
    };
    Ok(json!({
        "description": prompt_description(name),
        "messages": [{
            "role": "user",
            "content": {
                "type": "text",
                "text": text
            }
        }]
    }))
}

fn prompt_description(name: &str) -> &'static str {
    match name {
        "project_startup" => "Start a Private Drop coding session.",
        "safe_edit_workflow" => "Use guarded edits and verification.",
        "long_job_workflow" => "Use background jobs efficiently.",
        "final_report_workflow" => "Close out work cleanly.",
        _ => "Private Drop workflow prompt.",
    }
}

fn project_startup_prompt(args: &Map<String, Value>) -> String {
    let project = prompt_arg(args, "project").unwrap_or("<choose from list_projects>");
    let goal = prompt_arg(args, "goal").unwrap_or("<state current goal>");
    format!(
        "Goal: {goal}\n\nUse Private Drop MCP efficiently:\n1. Call list_projects once if project is unknown.\n2. For project {project}, call get_project_context_batch with overview, git_status, tree, and targeted read_file/search requests.\n3. Reuse result_metadata fingerprints in later read_file calls and expected_fingerprints before edits.\n4. Prefer summary or minimal response modes when output is large."
    )
}

fn safe_edit_workflow_prompt(args: &Map<String, Value>) -> String {
    let project = prompt_arg(args, "project").unwrap_or("<project>");
    let paths = prompt_arg(args, "paths").unwrap_or("<target paths>");
    format!(
        "Project: {project}\nPaths: {paths}\n\nEdit workflow:\n1. Read relevant files with get_project_context_batch and keep fingerprints from result_metadata.\n2. Use apply_project_edit with focused edits, reason, response_mode=summary, and expected_fingerprints for edited files.\n3. If an edit fails due to a fingerprint mismatch, reread only the changed files.\n4. Verify with run_project_git status/diff and the smallest useful check."
    )
}

fn long_job_workflow_prompt(args: &Map<String, Value>) -> String {
    let project = prompt_arg(args, "project").unwrap_or("<project>");
    let goal_id = prompt_arg(args, "goal_id").unwrap_or("<goal_id if required>");
    format!(
        "Project: {project}\nGoal id: {goal_id}\n\nAsync job workflow:\n1. Create jobs with run_job_op op=create/check/create_batch and a client_request_id.\n2. Poll op=status with detail=basic first.\n3. Use op=log only when output is needed, passing since_line from next_cursor.\n4. If the client timed out or lost state, use op=recover with job_id or client_request_id."
    )
}

fn final_report_workflow_prompt(args: &Map<String, Value>) -> String {
    let project = prompt_arg(args, "project").unwrap_or("<project>");
    let status = prompt_arg(args, "status").unwrap_or("completed");
    format!(
        "Project: {project}\nStatus: {status}\n\nCloseout workflow:\n1. Run run_project_git status and targeted diff.\n2. Run the smallest relevant configured check, or explain why it was not run.\n3. Write a concise write_report summary with changed files, checks, and residual risk.\n4. If using an action_session_id, close or rename the action session for later review."
    )
}

fn prompt_arg<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
}

fn workflow_resource_text() -> &'static str {
    "# Private Drop MCP Workflow\n\n- Start with `list_projects`; cache the selected project and capabilities.\n- Use `get_project_context_batch` for related reads instead of many small calls.\n- Reuse `result_metadata.fingerprint` with `if_fingerprint` and `expected_fingerprints`.\n- Use `apply_project_edit` with `response_mode=summary` for multi-file edits.\n- Use `run_job_op` status `detail=basic`; read logs incrementally with `since_line`.\n- Pass `action_session_id` on related calls to group audit history.\n"
}

fn object_schema(properties: Value, required: Vec<&str>) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": true
    })
}

fn string_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "description": description
    })
}

fn edit_operation_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": {
                "type": "string",
                "enum": [
                    "replace_text",
                    "replace_range",
                    "append_file",
                    "create_file",
                    "write_file",
                    "create_binary_file",
                    "write_binary_file",
                    "create_binary_artifact",
                    "write_binary_artifact",
                    "create_binary_file_from_upload",
                    "write_binary_file_from_upload",
                    "create_binary_file_from_url",
                    "write_binary_file_from_url"
                ]
            },
            "path": { "type": "string" },
            "old_text": { "type": "string" },
            "new_text": { "type": "string" },
            "occurrence": { "type": "integer", "minimum": 1 },
            "start_line": { "type": "integer", "minimum": 1 },
            "end_line": { "type": "integer", "minimum": 1 },
            "text": { "type": "string" },
            "content": { "type": "string" },
            "base64_content": { "type": "string" },
            "source_file": { "type": "string" },
            "source_url": { "type": "string" },
            "allow_overwrite": { "type": "boolean" }
        },
        "required": ["type", "path"],
        "additionalProperties": true
    })
}

fn tool_text(payload: Value, is_error: bool) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": compact_json(&payload)
        }],
        "isError": is_error
    })
}

fn compact_json(payload: &Value) -> String {
    serde_json::to_string(payload).unwrap_or_else(|_| payload.to_string())
}

fn parse_response_text(text: &str) -> Value {
    serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

fn object_from_value(value: Value) -> Result<Map<String, Value>, McpError> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| McpError::invalid_params("tool arguments must be an object"))
}

fn remove_optional_string(object: &mut Map<String, Value>, key: &str) -> Option<String> {
    object.remove(key).and_then(|value| match value {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_optional_string_strips_session_from_body() {
        let mut body = Map::new();
        body.insert(
            "action_session_id".to_string(),
            Value::String(" session-1 ".to_string()),
        );
        body.insert("project".to_string(), Value::String("p".to_string()));
        let session = remove_optional_string(&mut body, "action_session_id");
        assert_eq!(session.as_deref(), Some("session-1"));
        assert!(!body.contains_key("action_session_id"));
        assert_eq!(body["project"], Value::String("p".to_string()));
    }

    #[test]
    fn initialize_negotiates_known_protocol() {
        let response = initialize_result(json!({ "protocolVersion": "2025-03-26" }));
        assert_eq!(response["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn unknown_protocol_falls_back() {
        let response = initialize_result(json!({ "protocolVersion": "2099-01-01" }));
        assert_eq!(response["protocolVersion"], FALLBACK_PROTOCOL_VERSION);
    }

    #[test]
    fn tool_list_contains_gpt_core_tools() {
        let tool_defs = tools();
        let names = tool_defs
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"list_projects"));
        assert!(names.contains(&"get_project_context_batch"));
        assert!(names.contains(&"apply_project_edit"));
        assert!(names.contains(&"run_job_op"));
        assert!(names.contains(&"action_session_op"));
        assert_eq!(names.len(), 10);
    }
}
