use crate::{get_db, json_error, AgentModelProfileRecord, AgentSpecRecord};
use reqwest::blocking::Client;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};
use url::Url;
use uuid::Uuid;

const DEFAULT_MODEL_PROFILE_ID: &str = "default";
const MAX_TIMELINE_BODY_CHARS: usize = 12_000;
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOOL_CALLS_TOTAL: usize = 30;
const DEFAULT_MAX_RUN_SECS: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTool {
    pub name: String,
    pub description: String,
    pub path: String,
    pub method: String,
    pub parameters: Value,
}

#[derive(Debug, Serialize)]
pub struct AgentSpecView {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub auth_token_masked: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openapi_json: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub tools: Vec<AgentTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AgentModelProfileView {
    pub base_url: String,
    pub api_key_masked: String,
    pub model: String,
    pub temperature: Option<f64>,
    pub max_rounds: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct SaveAgentSpecRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub auth_token: Option<String>,
    pub openapi_json: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentRunRequest {
    pub spec_id: String,
    pub model_base_url: String,
    #[serde(default)]
    pub model_api_key: Option<String>,
    pub model: String,
    #[serde(default)]
    pub temperature: Option<f64>,
    pub system_prompt: String,
    pub user_message: String,
    #[serde(default)]
    pub max_rounds: Option<usize>,
    #[serde(default)]
    pub max_run_secs: Option<u64>,
}

#[derive(Debug)]
struct LimitedBody {
    text: String,
    truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct AgentRunResponse {
    pub success: bool,
    pub final_response: Option<String>,
    pub rounds: usize,
    pub stopped_reason: String,
    pub timeline: Vec<TimelineEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEvent {
    AssistantMessage {
        round: usize,
        content: Option<String>,
        latency_ms: u128,
    },
    ToolCall {
        round: usize,
        tool_call_id: String,
        name: String,
        arguments: Value,
    },
    ToolResponse {
        round: usize,
        tool_call_id: String,
        name: String,
        status: Option<u16>,
        duration_ms: u128,
        response_preview: String,
        truncated: bool,
        error: Option<String>,
    },
    Error {
        round: usize,
        message: String,
    },
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ModelToolCall>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ModelToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ModelToolFunction,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ModelToolFunction {
    name: String,
    arguments: String,
}

fn mask_secret(secret: &str) -> String {
    if secret.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    format!(
        "{}...{}",
        chars.iter().take(4).collect::<String>(),
        chars
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>()
    )
}

fn trim_trailing_slash(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn validate_base_url(value: &str, field: &str) -> Result<String, String> {
    let trimmed = trim_trailing_slash(value);
    let parsed =
        Url::parse(&trimmed).map_err(|e| format!("{} must be a valid URL: {}", field, e))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("{} must use http or https", field));
    }
    if parsed.host_str().unwrap_or_default().is_empty() {
        return Err(format!("{} must include a host", field));
    }
    // This is a personal intranet debugging tool, so private/link-local/localhost
    // addresses are intentionally allowed for now.
    Ok(trimmed)
}

fn spec_to_view(record: AgentSpecRecord, include_json: bool) -> AgentSpecView {
    let parsed = extract_tools_from_openapi(&record.openapi_json);
    let (tools, parse_error) = match parsed {
        Ok(tools) => (tools, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    AgentSpecView {
        id: record.id,
        name: record.name,
        base_url: record.base_url,
        auth_token_masked: mask_secret(&record.auth_token),
        openapi_json: include_json.then_some(record.openapi_json),
        created_at: record.created_at,
        updated_at: record.updated_at,
        tools,
        parse_error,
    }
}

fn build_agent_spec_record(
    body: SaveAgentSpecRequest,
    existing: Option<&AgentSpecRecord>,
    now: i64,
) -> Result<AgentSpecRecord, String> {
    if body.name.trim().is_empty() || body.base_url.trim().is_empty() {
        return Err("name and base_url are required".to_string());
    }
    let base_url = validate_base_url(&body.base_url, "base_url")?;
    let auth_token = match body.auth_token {
        Some(token) if !token.trim().is_empty() => token,
        _ => existing
            .map(|record| record.auth_token.clone())
            .unwrap_or_default(),
    };
    Ok(AgentSpecRecord {
        id: existing
            .map(|record| record.id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        name: body.name.trim().to_string(),
        base_url,
        auth_token,
        openapi_json: body.openapi_json,
        created_at: existing.map(|record| record.created_at).unwrap_or(now),
        updated_at: now,
    })
}

fn profile_to_view(profile: Option<AgentModelProfileRecord>) -> AgentModelProfileView {
    match profile {
        Some(profile) => AgentModelProfileView {
            base_url: profile.base_url,
            api_key_masked: mask_secret(&profile.api_key),
            model: profile.model,
            temperature: profile.temperature,
            max_rounds: profile.max_rounds,
        },
        None => AgentModelProfileView {
            base_url: String::new(),
            api_key_masked: String::new(),
            model: String::new(),
            temperature: Some(0.2),
            max_rounds: Some(6),
        },
    }
}

fn json_schema_object() -> Value {
    json!({"type":"object","properties":{},"additionalProperties":true})
}

fn resolve_ref(root: &Value, value: &Value) -> Value {
    let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
        return value.clone();
    };
    let Some(pointer) = reference.strip_prefix('#') else {
        return value.clone();
    };
    root.pointer(pointer)
        .cloned()
        .unwrap_or_else(|| value.clone())
}

fn extract_request_schema(root: &Value, op: &Value) -> Value {
    let Some(schema) = op
        .get("requestBody")
        .and_then(|v| v.get("content"))
        .and_then(|v| v.get("application/json"))
        .and_then(|v| v.get("schema"))
    else {
        return json_schema_object();
    };
    resolve_ref(root, schema)
}

fn has_json_request_body(op: &Value) -> bool {
    match op.get("requestBody") {
        None => true,
        Some(request_body) => request_body
            .get("content")
            .and_then(Value::as_object)
            .map(|content| content.contains_key("application/json"))
            .unwrap_or(false),
    }
}

pub fn extract_tools_from_openapi(openapi_json: &str) -> Result<Vec<AgentTool>, String> {
    let root: Value =
        serde_json::from_str(openapi_json).map_err(|e| format!("Invalid OpenAPI JSON: {}", e))?;
    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| "OpenAPI JSON must contain a paths object".to_string())?;
    let mut tools = Vec::new();
    for (path, item) in paths {
        let Some(post) = item.get("post") else {
            continue;
        };
        let Some(operation_id) = post.get("operationId").and_then(Value::as_str) else {
            continue;
        };
        if !has_json_request_body(post) {
            continue;
        }
        let description = post
            .get("description")
            .or_else(|| post.get("summary"))
            .and_then(Value::as_str)
            .unwrap_or(operation_id)
            .to_string();
        tools.push(AgentTool {
            name: operation_id.to_string(),
            description,
            path: path.to_string(),
            method: "POST".to_string(),
            parameters: extract_request_schema(&root, post),
        });
    }
    Ok(tools)
}

fn openai_tools(tools: &[AgentTool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters
                }
            })
        })
        .collect()
}

fn preview_body(text: &str) -> String {
    if text.chars().count() <= MAX_TIMELINE_BODY_CHARS {
        return text.to_string();
    }
    let mut out = text
        .chars()
        .take(MAX_TIMELINE_BODY_CHARS)
        .collect::<String>();
    out.push_str("\n...[truncated]");
    out
}

fn read_limited_body<R: Read>(reader: R, max_bytes: u64) -> std::io::Result<LimitedBody> {
    let mut limited = reader.take(max_bytes.saturating_add(1));
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes)?;
    let truncated = bytes.len() as u64 > max_bytes;
    if truncated {
        bytes.truncate(max_bytes as usize);
    }
    Ok(LimitedBody {
        text: String::from_utf8_lossy(&bytes).to_string(),
        truncated,
    })
}

fn chat_completions_url(base_url: &str) -> String {
    let base = trim_trailing_slash(base_url);
    if base.ends_with("/chat/completions") {
        base
    } else if base.ends_with("/v1") {
        format!("{}/chat/completions", base)
    } else {
        format!("{}/chat/completions", base)
    }
}

fn action_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        trim_trailing_slash(base_url),
        path.trim_start_matches('/')
    )
}

fn call_action(
    client: &Client,
    spec: &AgentSpecRecord,
    tool: &AgentTool,
    args: &Value,
) -> (Option<u16>, u128, String, bool, Option<String>) {
    let start = Instant::now();
    let result = client
        .post(action_url(&spec.base_url, &tool.path))
        .bearer_auth(&spec.auth_token)
        .json(args)
        .send();
    let duration_ms = start.elapsed().as_millis();
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            match read_limited_body(resp, MAX_RESPONSE_BYTES) {
                Ok(body) => (
                    Some(status),
                    duration_ms,
                    preview_body(&body.text),
                    body.truncated,
                    None,
                ),
                Err(e) => (
                    Some(status),
                    duration_ms,
                    String::new(),
                    false,
                    Some(format!("Failed to read action response: {}", e)),
                ),
            }
        }
        Err(e) => (
            None,
            duration_ms,
            String::new(),
            false,
            Some(format!("Action request failed: {}", e)),
        ),
    }
}

fn run_tool_loop(
    req: AgentRunRequest,
    spec: AgentSpecRecord,
    model_api_key: String,
) -> AgentRunResponse {
    let tools = match extract_tools_from_openapi(&spec.openapi_json) {
        Ok(tools) => tools,
        Err(e) => {
            return AgentRunResponse {
                success: false,
                final_response: None,
                rounds: 0,
                stopped_reason: "spec_parse_error".to_string(),
                timeline: vec![TimelineEvent::Error {
                    round: 0,
                    message: e.clone(),
                }],
                error: Some(e),
            }
        }
    };
    let tool_map: HashMap<String, AgentTool> = tools
        .iter()
        .map(|tool| (tool.name.clone(), tool.clone()))
        .collect();
    let max_rounds = req.max_rounds.unwrap_or(6).clamp(1, 20);
    let client_builder = Client::builder().timeout(Duration::from_secs(60));
    #[cfg(test)]
    let client_builder = client_builder.no_proxy();
    let client = match client_builder.build() {
        Ok(client) => client,
        Err(e) => {
            return AgentRunResponse {
                success: false,
                final_response: None,
                rounds: 0,
                stopped_reason: "client_error".to_string(),
                timeline: vec![TimelineEvent::Error {
                    round: 0,
                    message: e.to_string(),
                }],
                error: Some(e.to_string()),
            }
        }
    };
    let mut timeline = Vec::new();
    let mut messages = vec![
        json!({"role":"system","content":req.system_prompt}),
        json!({"role":"user","content":req.user_message}),
    ];
    let tool_defs = openai_tools(&tools);
    let url = chat_completions_url(&req.model_base_url);
    let run_started = Instant::now();
    let max_run_secs = req.max_run_secs.unwrap_or(DEFAULT_MAX_RUN_SECS).max(1);
    let mut tool_calls_total = 0usize;

    for round in 1..=max_rounds {
        if run_started.elapsed() >= Duration::from_secs(max_run_secs) {
            return AgentRunResponse {
                success: false,
                final_response: None,
                rounds: round.saturating_sub(1),
                stopped_reason: "max_run_secs_exceeded".to_string(),
                timeline,
                error: Some("Max run seconds exceeded".to_string()),
            };
        }
        let mut body = Map::new();
        body.insert("model".to_string(), json!(req.model));
        body.insert("messages".to_string(), Value::Array(messages.clone()));
        if !tool_defs.is_empty() {
            body.insert("tools".to_string(), Value::Array(tool_defs.clone()));
            body.insert("tool_choice".to_string(), json!("auto"));
        }
        if let Some(temp) = req.temperature {
            body.insert("temperature".to_string(), json!(temp));
        }
        let started = Instant::now();
        let response = client
            .post(&url)
            .bearer_auth(&model_api_key)
            .json(&Value::Object(body))
            .send();
        let latency_ms = started.elapsed().as_millis();
        let response = match response {
            Ok(resp) => resp,
            Err(e) => {
                timeline.push(TimelineEvent::Error {
                    round,
                    message: format!("Model request failed: {}", e),
                });
                return AgentRunResponse {
                    success: false,
                    final_response: None,
                    rounds: round,
                    stopped_reason: "model_error".to_string(),
                    timeline,
                    error: Some(e.to_string()),
                };
            }
        };
        let status = response.status();
        let response_body = match read_limited_body(response, MAX_RESPONSE_BYTES) {
            Ok(body) => body,
            Err(e) => {
                timeline.push(TimelineEvent::Error {
                    round,
                    message: format!("Failed to read model response: {}", e),
                });
                return AgentRunResponse {
                    success: false,
                    final_response: None,
                    rounds: round,
                    stopped_reason: "model_error".to_string(),
                    timeline,
                    error: Some(e.to_string()),
                };
            }
        };
        if response_body.truncated {
            let msg = format!(
                "Model response exceeded {} bytes and was truncated",
                MAX_RESPONSE_BYTES
            );
            timeline.push(TimelineEvent::Error {
                round,
                message: msg.clone(),
            });
            return AgentRunResponse {
                success: false,
                final_response: None,
                rounds: round,
                stopped_reason: "model_response_too_large".to_string(),
                timeline,
                error: Some(msg),
            };
        }
        if !status.is_success() {
            let msg = format!(
                "Model returned HTTP {}: {}",
                status.as_u16(),
                preview_body(&response_body.text)
            );
            timeline.push(TimelineEvent::Error {
                round,
                message: msg.clone(),
            });
            return AgentRunResponse {
                success: false,
                final_response: None,
                rounds: round,
                stopped_reason: "model_http_error".to_string(),
                timeline,
                error: Some(msg),
            };
        }
        let parsed: ChatCompletionResponse = match serde_json::from_str(&response_body.text) {
            Ok(parsed) => parsed,
            Err(e) => {
                let msg = format!("Invalid model response JSON: {}", e);
                timeline.push(TimelineEvent::Error {
                    round,
                    message: msg.clone(),
                });
                return AgentRunResponse {
                    success: false,
                    final_response: None,
                    rounds: round,
                    stopped_reason: "model_parse_error".to_string(),
                    timeline,
                    error: Some(msg),
                };
            }
        };
        let Some(choice) = parsed.choices.into_iter().next() else {
            let msg = "Model response has no choices".to_string();
            timeline.push(TimelineEvent::Error {
                round,
                message: msg.clone(),
            });
            return AgentRunResponse {
                success: false,
                final_response: None,
                rounds: round,
                stopped_reason: "model_parse_error".to_string(),
                timeline,
                error: Some(msg),
            };
        };
        let assistant_message = choice.message;
        timeline.push(TimelineEvent::AssistantMessage {
            round,
            content: assistant_message.content.clone(),
            latency_ms,
        });
        let assistant_tool_calls = assistant_message.tool_calls.clone();
        let mut assistant_json = Map::new();
        assistant_json.insert("role".to_string(), json!("assistant"));
        assistant_json.insert(
            "content".to_string(),
            assistant_message
                .content
                .clone()
                .map_or(Value::Null, Value::String),
        );
        if !assistant_tool_calls.is_empty() {
            assistant_json.insert("tool_calls".to_string(), json!(assistant_tool_calls));
        }
        messages.push(Value::Object(assistant_json));
        if assistant_tool_calls.is_empty() {
            let final_response = messages
                .last()
                .and_then(|v| v.get("content"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
            return AgentRunResponse {
                success: true,
                final_response,
                rounds: round,
                stopped_reason: "assistant_done".to_string(),
                timeline,
                error: None,
            };
        }
        for call in assistant_tool_calls {
            tool_calls_total += 1;
            if tool_calls_total > MAX_TOOL_CALLS_TOTAL {
                return AgentRunResponse {
                    success: false,
                    final_response: None,
                    rounds: round,
                    stopped_reason: "max_tool_calls_total_exceeded".to_string(),
                    timeline,
                    error: Some("Max total tool calls exceeded".to_string()),
                };
            }
            if run_started.elapsed() >= Duration::from_secs(max_run_secs) {
                return AgentRunResponse {
                    success: false,
                    final_response: None,
                    rounds: round,
                    stopped_reason: "max_run_secs_exceeded".to_string(),
                    timeline,
                    error: Some("Max run seconds exceeded".to_string()),
                };
            }
            if call.call_type != "function" {
                let msg = format!("Unsupported tool call type: {}", call.call_type);
                timeline.push(TimelineEvent::Error {
                    round,
                    message: msg.clone(),
                });
                messages.push(json!({
                    "role":"tool",
                    "tool_call_id": call.id,
                    "content": json!({"error": msg}).to_string()
                }));
                continue;
            }
            let args: Value = match serde_json::from_str(&call.function.arguments) {
                Ok(args) => args,
                Err(e) => json!({"_raw": call.function.arguments, "_parse_error": e.to_string()}),
            };
            timeline.push(TimelineEvent::ToolCall {
                round,
                tool_call_id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: args.clone(),
            });
            let Some(tool) = tool_map.get(&call.function.name) else {
                let msg = format!("Unknown tool: {}", call.function.name);
                timeline.push(TimelineEvent::Error {
                    round,
                    message: msg.clone(),
                });
                messages.push(json!({
                    "role":"tool",
                    "tool_call_id": call.id,
                    "content": json!({"error": msg}).to_string()
                }));
                continue;
            };
            let (status, duration_ms, response_preview, truncated, error) =
                call_action(&client, &spec, tool, &args);
            timeline.push(TimelineEvent::ToolResponse {
                round,
                tool_call_id: call.id.clone(),
                name: call.function.name.clone(),
                status,
                duration_ms,
                response_preview: response_preview.clone(),
                truncated,
                error: error.clone(),
            });
            messages.push(json!({
                "role":"tool",
                "tool_call_id": call.id,
                "content": json!({
                    "status": status,
                    "response": response_preview,
                    "truncated": truncated,
                    "error": error
                }).to_string()
            }));
        }
    }

    AgentRunResponse {
        success: false,
        final_response: None,
        rounds: max_rounds,
        stopped_reason: "max_rounds_exceeded".to_string(),
        timeline,
        error: Some("Max rounds exceeded before assistant produced a final response".to_string()),
    }
}

#[handler]
pub async fn list_agent_specs(depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    match db.list_agent_specs() {
        Ok(specs) => res.render(Json(serde_json::json!({
            "specs": specs.into_iter().map(|record| spec_to_view(record, false)).collect::<Vec<_>>(),
            "model_profile": profile_to_view(db.get_agent_model_profile(DEFAULT_MODEL_PROFILE_ID).ok().flatten())
        }))),
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            ));
        }
    }
}

#[handler]
pub async fn save_agent_spec(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let body: SaveAgentSpecRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(json_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {}", e),
            ));
            return;
        }
    };
    if let Err(e) = extract_tools_from_openapi(&body.openapi_json) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(json_error(StatusCode::BAD_REQUEST, &e));
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let existing = match body.id.as_deref().filter(|id| !id.trim().is_empty()) {
        Some(id) => match db.get_agent_spec(id) {
            Ok(Some(record)) => Some(record),
            Ok(None) => {
                res.status_code(StatusCode::NOT_FOUND);
                res.render(json_error(StatusCode::NOT_FOUND, "Spec not found"));
                return;
            }
            Err(e) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &e.to_string(),
                ));
                return;
            }
        },
        None => None,
    };
    let record = match build_agent_spec_record(body, existing.as_ref(), now) {
        Ok(record) => record,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(json_error(StatusCode::BAD_REQUEST, &e));
            return;
        }
    };
    match db.upsert_agent_spec(&record) {
        Ok(()) => res.render(Json(spec_to_view(record, false))),
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            ));
        }
    }
}

#[handler]
pub async fn get_agent_spec(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let id = req.param::<String>("id").unwrap_or_default();
    match db.get_agent_spec(&id) {
        Ok(Some(record)) => res.render(Json(spec_to_view(record, true))),
        Ok(None) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(json_error(StatusCode::NOT_FOUND, "Spec not found"));
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            ));
        }
    }
}

#[handler]
pub async fn delete_agent_spec(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let id = req.param::<String>("id").unwrap_or_default();
    match db.delete_agent_spec(&id) {
        Ok(deleted) => res.render(Json(json!({"deleted": deleted, "id": id}))),
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            ));
        }
    }
}

#[handler]
pub async fn run_agent(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let body: AgentRunRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(json_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {}", e),
            ));
            return;
        }
    };
    let Some(spec) = db.get_agent_spec(&body.spec_id).ok().flatten() else {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(json_error(StatusCode::NOT_FOUND, "Spec not found"));
        return;
    };
    if body.model_base_url.trim().is_empty() || body.model.trim().is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(json_error(
            StatusCode::BAD_REQUEST,
            "model_base_url and model are required",
        ));
        return;
    }
    let model_base_url = match validate_base_url(&body.model_base_url, "model_base_url") {
        Ok(base_url) => base_url,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(json_error(StatusCode::BAD_REQUEST, &e));
            return;
        }
    };
    let existing_profile = db
        .get_agent_model_profile(DEFAULT_MODEL_PROFILE_ID)
        .ok()
        .flatten();
    let model_api_key = body
        .model_api_key
        .clone()
        .filter(|key| !key.trim().is_empty())
        .or_else(|| existing_profile.as_ref().map(|p| p.api_key.clone()))
        .unwrap_or_default();
    if model_api_key.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(json_error(
            StatusCode::BAD_REQUEST,
            "model_api_key is required",
        ));
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let profile = AgentModelProfileRecord {
        id: DEFAULT_MODEL_PROFILE_ID.to_string(),
        base_url: model_base_url,
        api_key: model_api_key.clone(),
        model: body.model.clone(),
        temperature: body.temperature,
        max_rounds: body.max_rounds,
        updated_at: now,
    };
    if let Err(e) = db.upsert_agent_model_profile(&profile) {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
        ));
        return;
    }
    let mut run_body = body;
    run_body.model_base_url = profile.base_url.clone();
    let join =
        tokio::task::spawn_blocking(move || run_tool_loop(run_body, spec, model_api_key)).await;
    match join {
        Ok(response) => res.render(Json(response)),
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Agent run task failed: {}", e),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant as TestInstant};

    fn compact_fixture() -> String {
        let mut spec: Value = serde_json::from_str(include_str!("../data/openapi.json")).unwrap();
        spec["paths"] = json!({
            "/api/codex/context": spec["paths"]["/api/codex/context"].clone(),
            "/api/codex/job": spec["paths"]["/api/codex/job"].clone()
        });
        serde_json::to_string(&spec).unwrap()
    }

    fn demo_openapi() -> String {
        json!({
            "openapi": "3.1.0",
            "paths": {
                "/api/demo": {
                    "post": {
                        "operationId": "demoOp",
                        "description": "Demo operation",
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {"type":"object","properties":{"name":{"type":"string"}}}
                                }
                            }
                        }
                    }
                }
            }
        })
        .to_string()
    }

    #[test]
    fn extracts_tools_from_basic_openapi() {
        let text = json!({
            "openapi": "3.1.0",
            "paths": {
                "/api/demo": {
                    "post": {
                        "operationId": "demoOp",
                        "description": "Demo operation",
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {"name": {"type": "string"}},
                                        "required": ["name"]
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
        .to_string();
        let tools = extract_tools_from_openapi(&text).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "demoOp");
        assert_eq!(tools[0].path, "/api/demo");
        assert_eq!(tools[0].parameters["properties"]["name"]["type"], "string");
    }

    #[test]
    fn extracts_run_job_and_project_context_tools_from_compact_openapi() {
        let tools = extract_tools_from_openapi(&compact_fixture()).unwrap();
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"runJobOp"));
        assert!(names.contains(&"getProjectContext"));
        let run_job = tools.iter().find(|tool| tool.name == "runJobOp").unwrap();
        assert_eq!(run_job.method, "POST");
        assert!(run_job.parameters.is_object());
    }

    #[test]
    fn chat_completions_url_appends_endpoint() {
        assert_eq!(
            chat_completions_url("https://example.com/v1"),
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://example.com/v1/chat/completions"),
            "https://example.com/v1/chat/completions"
        );
    }

    #[test]
    fn update_spec_with_empty_auth_token_keeps_existing_token() {
        let existing = AgentSpecRecord {
            id: "spec-1".to_string(),
            name: "old".to_string(),
            base_url: "https://old.example".to_string(),
            auth_token: "keep-me".to_string(),
            openapi_json: demo_openapi(),
            created_at: 10,
            updated_at: 11,
        };
        let record = build_agent_spec_record(
            SaveAgentSpecRequest {
                id: Some(existing.id.clone()),
                name: "new".to_string(),
                base_url: "https://new.example/".to_string(),
                auth_token: Some(String::new()),
                openapi_json: demo_openapi(),
            },
            Some(&existing),
            20,
        )
        .unwrap();
        assert_eq!(record.id, "spec-1");
        assert_eq!(record.auth_token, "keep-me");
        assert_eq!(record.base_url, "https://new.example");
        assert_eq!(record.created_at, 10);
        assert_eq!(record.updated_at, 20);
    }

    #[test]
    fn base_url_rejects_non_http_schemes() {
        assert!(validate_base_url("file:///tmp/spec", "base_url").is_err());
        assert!(validate_base_url("ssh://example.com", "model_base_url").is_err());
        assert!(validate_base_url("https://example.com", "base_url").is_ok());
        assert!(validate_base_url("http://127.0.0.1:8080", "base_url").is_ok());
    }

    #[test]
    fn extracts_tools_skips_non_json_post() {
        let text = json!({
            "openapi": "3.1.0",
            "paths": {
                "/api/json": {
                    "post": {
                        "operationId": "jsonOp",
                        "requestBody": {"content": {"application/json": {"schema": {"type":"object"}}}}
                    }
                },
                "/api/form": {
                    "post": {
                        "operationId": "formOp",
                        "requestBody": {"content": {"multipart/form-data": {"schema": {"type":"object"}}}}
                    }
                }
            }
        })
        .to_string();
        let tools = extract_tools_from_openapi(&text).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "jsonOp");
    }

    #[test]
    fn tool_loop_uses_mock_model_and_action() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let hits = Arc::new(Mutex::new(Vec::<String>::new()));
        let hits_for_thread = hits.clone();
        let handle = thread::spawn(move || {
            let mut model_calls = 0;
            let deadline = TestInstant::now() + Duration::from_secs(10);
            while TestInstant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut received = Vec::new();
                let mut buf = [0_u8; 1024];
                loop {
                    let n = match stream.read(&mut buf) {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if n == 0 {
                        break;
                    }
                    received.extend_from_slice(&buf[..n]);
                    let Some(header_end) = received.windows(4).position(|w| w == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&received[..header_end]).to_string();
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if received.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&received).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                hits_for_thread.lock().unwrap().push(path.clone());
                let body = if path == "/v1/chat/completions" {
                    model_calls += 1;
                    if model_calls == 1 {
                        json!({
                            "choices": [{
                                "message": {
                                    "content": null,
                                    "tool_calls": [{
                                        "id": "call_1",
                                        "type": "function",
                                        "function": {"name": "demoOp", "arguments": "{\"name\":\"Ada\"}"}
                                    }]
                                }
                            }]
                        })
                    } else {
                        json!({
                            "choices": [{
                                "message": {"content": "Tool completed", "tool_calls": []}
                            }]
                        })
                    }
                } else {
                    json!({"ok": true, "hello": "Ada"})
                }
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                if model_calls >= 2 && hits_for_thread.lock().unwrap().len() >= 3 {
                    break;
                }
            }
        });

        let base = format!("http://{}", addr);
        let spec = AgentSpecRecord {
            id: "spec".to_string(),
            name: "demo".to_string(),
            base_url: base.clone(),
            auth_token: "action-token".to_string(),
            openapi_json: demo_openapi(),
            created_at: 1,
            updated_at: 1,
        };
        let response = run_tool_loop(
            AgentRunRequest {
                spec_id: "spec".to_string(),
                model_base_url: format!("{}/v1", base),
                model_api_key: Some("model-token".to_string()),
                model: "mock".to_string(),
                temperature: Some(0.0),
                system_prompt: "system".to_string(),
                user_message: "hello".to_string(),
                max_rounds: Some(3),
                max_run_secs: None,
            },
            spec,
            "model-token".to_string(),
        );
        let _ = TcpStream::connect(addr);
        let _ = TcpStream::connect(addr);
        handle.join().unwrap();
        assert!(response.success, "{:?}", response.error);
        assert_eq!(response.final_response.as_deref(), Some("Tool completed"));
        let paths = hits.lock().unwrap().clone();
        assert_eq!(
            paths,
            vec![
                "/v1/chat/completions".to_string(),
                "/api/demo".to_string(),
                "/v1/chat/completions".to_string()
            ]
        );
    }

    #[test]
    fn max_rounds_is_clamped_to_twenty() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let hits = Arc::new(Mutex::new(0usize));
        let hits_for_thread = hits.clone();
        let handle = thread::spawn(move || {
            let deadline = TestInstant::now() + Duration::from_secs(10);
            while TestInstant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let mut buf = [0_u8; 4096];
                let _ = stream.read(&mut buf);
                *hits_for_thread.lock().unwrap() += 1;
                let body = json!({
                    "choices": [{
                        "message": {
                            "content": null,
                            "tool_calls": [{
                                "id": "call_loop",
                                "type": "function",
                                "function": {"name": "missingTool", "arguments": "{}"}
                            }]
                        }
                    }]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                if *hits_for_thread.lock().unwrap() >= 20 {
                    break;
                }
            }
        });
        let base = format!("http://{}", addr);
        let response = run_tool_loop(
            AgentRunRequest {
                spec_id: "spec".to_string(),
                model_base_url: format!("{}/v1", base),
                model_api_key: Some("model-token".to_string()),
                model: "mock".to_string(),
                temperature: Some(0.0),
                system_prompt: "system".to_string(),
                user_message: "hello".to_string(),
                max_rounds: Some(99),
                max_run_secs: Some(10),
            },
            AgentSpecRecord {
                id: "spec".to_string(),
                name: "demo".to_string(),
                base_url: base,
                auth_token: "action-token".to_string(),
                openapi_json: demo_openapi(),
                created_at: 1,
                updated_at: 1,
            },
            "model-token".to_string(),
        );
        let _ = TcpStream::connect(addr);
        handle.join().unwrap();
        assert!(!response.success);
        assert_eq!(response.rounds, 20);
        assert_eq!(response.stopped_reason, "max_rounds_exceeded");
        assert_eq!(*hits.lock().unwrap(), 20);
    }
}
