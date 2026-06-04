use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    Text,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub channel: String,
    pub kind: MessageKind,
    pub title: Option<String>,
    pub text: Option<String>,
    pub file_name: Option<String>,
    pub file_path: Option<String>,
    pub file_size: Option<i64>,
    pub mime_type: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateMessageRequest {
    pub channel: String,
    pub title: Option<String>,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct Channel {
    pub name: String,
    pub display_name: String,
    pub message_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandAuditRecord {
    pub id: String,
    pub project: String,
    pub command: String,
    pub command_text: Option<String>,
    pub reason: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub approved_at: Option<i64>,
    pub executed_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub stdout_tail: Option<String>,
    pub stderr_tail: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexGoalRecord {
    pub id: String,
    pub project: String,
    pub title: String,
    pub summary: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub closed_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopTask {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub status: String,
    pub priority: i64,
    pub claimed_by: Option<String>,
    pub last_event: Option<String>,
    pub screenshot_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopTaskEvent {
    pub id: String,
    pub task_id: String,
    pub status: String,
    pub worker: Option<String>,
    pub message: Option<String>,
    pub screenshot_url: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateDesktopTaskRequest {
    pub title: String,
    pub instructions: String,
    #[serde(default)]
    pub priority: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DesktopTaskClaimRequest {
    pub worker: String,
}

#[derive(Debug, Deserialize)]
pub struct DesktopTaskEventRequest {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub screenshot_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DesktopTaskOpRequest {
    pub op: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub screenshot_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpecRecord {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub auth_token: String,
    pub openapi_json: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentModelProfileRecord {
    pub id: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: Option<f64>,
    pub max_rounds: Option<usize>,
    pub updated_at: i64,
}
