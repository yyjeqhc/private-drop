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
