use salvo::cors::Cors;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

mod codex;
mod projects;

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

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub data_dir: PathBuf,
    pub token: Option<String>,
    pub max_text_size: usize,
    pub max_file_size: usize,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            addr: std::env::var("DROP_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            data_dir: std::env::var("DROP_DATA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./data")),
            token: std::env::var("DROP_TOKEN").ok(),
            max_text_size: 2 * 1024 * 1024,
            max_file_size: 100 * 1024 * 1024,
        }
    }
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("drop.db")
    }
    pub fn uploads_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }
    pub fn is_auth_enabled(&self) -> bool {
        self.token.is_some()
    }
    pub fn validate_token(&self, token: &str) -> bool {
        self.token.as_ref().map(|t| t == token).unwrap_or(false)
    }
}

use rusqlite::{params, Connection};

pub struct Database {
    conn: Mutex<Connection>,
}

fn row_to_message(row: &rusqlite::Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        channel: row.get(1)?,
        kind: match row.get::<_, String>(2)?.as_str() {
            "file" => MessageKind::File,
            _ => MessageKind::Text,
        },
        title: row.get(3)?,
        text: row.get(4)?,
        file_name: row.get(5)?,
        file_path: row.get(6)?,
        file_size: row.get(7)?,
        mime_type: row.get(8)?,
        created_at: row.get(9)?,
        expires_at: row.get(10)?,
    })
}

impl Database {
    pub fn open(db_path: &PathBuf) -> anyhow::Result<Self> {
        let conn = Connection::open(db_path)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init_tables()?;
        Ok(db)
    }

    fn init_tables(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                channel TEXT NOT NULL,
                kind TEXT NOT NULL CHECK(kind IN ('text', 'file')),
                title TEXT,
                text TEXT,
                file_name TEXT,
                file_path TEXT,
                file_size INTEGER,
                mime_type TEXT,
                created_at INTEGER NOT NULL,
                expires_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_messages_channel ON messages(channel);
            CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at DESC);",
        )?;
        Ok(())
    }

    pub fn insert_message(&self, message: &Message) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (id, channel, kind, title, text, file_name, file_path, file_size, mime_type, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                message.id, message.channel,
                match message.kind { MessageKind::Text => "text", MessageKind::File => "file" },
                message.title, message.text, message.file_name, message.file_path,
                message.file_size, message.mime_type, message.created_at, message.expires_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_message(&self, id: &str) -> anyhow::Result<Option<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, channel, kind, title, text, file_name, file_path, file_size, mime_type, created_at, expires_at FROM messages WHERE id = ?1"
        )?;
        let mut rows = stmt.query_map(params![id], row_to_message)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn delete_message(&self, id: &str) -> anyhow::Result<Option<Message>> {
        let message = self.get_message(id)?;
        if message.is_some() {
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        }
        Ok(message)
    }

    pub fn list_messages(
        &self,
        channel: Option<&str>,
        limit: usize,
        before: Option<i64>,
    ) -> anyhow::Result<(Vec<Message>, bool)> {
        let conn = self.conn.lock().unwrap();
        let mut messages = Vec::new();

        let sql = match (channel, before) {
            (Some(_), Some(_)) => "SELECT id, channel, kind, title, text, file_name, file_path, file_size, mime_type, created_at, expires_at FROM messages WHERE channel = ?1 AND created_at < ?2 ORDER BY created_at DESC LIMIT ?3",
            (Some(_), None) => "SELECT id, channel, kind, title, text, file_name, file_path, file_size, mime_type, created_at, expires_at FROM messages WHERE channel = ?1 ORDER BY created_at DESC LIMIT ?2",
            (None, Some(_)) => "SELECT id, channel, kind, title, text, file_name, file_path, file_size, mime_type, created_at, expires_at FROM messages WHERE created_at < ?1 ORDER BY created_at DESC LIMIT ?2",
            (None, None) => "SELECT id, channel, kind, title, text, file_name, file_path, file_size, mime_type, created_at, expires_at FROM messages ORDER BY created_at DESC LIMIT ?1",
        };

        let mut stmt = conn.prepare(sql)?;
        let query_limit = limit as i64 + 1;
        let rows = match (channel, before) {
            (Some(ch), Some(before_ts)) => {
                stmt.query_map(params![ch, before_ts, query_limit], row_to_message)?
            }
            (Some(ch), None) => stmt.query_map(params![ch, query_limit], row_to_message)?,
            (None, Some(before_ts)) => {
                stmt.query_map(params![before_ts, query_limit], row_to_message)?
            }
            (None, None) => stmt.query_map(params![query_limit], row_to_message)?,
        };

        for row in rows {
            messages.push(row?);
        }
        let has_more = messages.len() > limit;
        messages.truncate(limit);
        Ok((messages, has_more))
    }

    pub fn list_channels(&self) -> anyhow::Result<Vec<Channel>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT channel, COUNT(*) as cnt FROM messages GROUP BY channel ORDER BY cnt DESC",
        )?;
        let default_channels = vec![
            ("inbox", "Inbox"),
            ("xline", "Xline"),
            ("thesis", "Thesis"),
            ("packfix", "Packfix"),
            ("omo", "OMO"),
            ("files", "Files"),
        ];
        let mut channels: Vec<Channel> = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                let display_name = default_channels
                    .iter()
                    .find(|(n, _)| *n == name.as_str())
                    .map(|(_, d)| d.to_string())
                    .unwrap_or_else(|| name.clone());
                Ok(Channel {
                    name,
                    display_name,
                    message_count: count,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        for (name, display_name) in default_channels {
            if !channels.iter().any(|c| c.name == name) {
                channels.push(Channel {
                    name: name.to_string(),
                    display_name: display_name.to_string(),
                    message_count: 0,
                });
            }
        }
        Ok(channels)
    }
}

fn get_config(depot: &Depot) -> Option<Arc<Config>> {
    depot.obtain::<Arc<Config>>().ok().cloned()
}

fn get_db(depot: &Depot) -> Option<Arc<Database>> {
    depot.obtain::<Arc<Database>>().ok().cloned()
}

fn check_auth(req: &Request, config: &Config) -> bool {
    if !config.is_auth_enabled() {
        return true;
    }
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if let Some(t) = token {
        return config.validate_token(t);
    }
    req.query::<String>("token")
        .map(|t| config.validate_token(&t))
        .unwrap_or(false)
}

fn json_error(_status: StatusCode, msg: &str) -> Json<serde_json::Value> {
    Json(serde_json::json!({"error": msg}))
}

struct AuthMiddleware;

#[async_trait]
impl Handler for AuthMiddleware {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let Some(config) = get_config(depot) else {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No config"));
            ctrl.skip_rest();
            return;
        };
        if !check_auth(req, &config) {
            res.status_code(StatusCode::UNAUTHORIZED);
            res.render(json_error(StatusCode::UNAUTHORIZED, "Unauthorized"));
            ctrl.skip_rest();
            return;
        }
        ctrl.call_next(req, depot, res).await;
    }
}

#[handler]
pub async fn health(res: &mut Response) {
    res.render(Json(
        serde_json::json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}),
    ));
}

#[handler]
pub async fn list_channels(depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    match db.list_channels() {
        Ok(channels) => res.render(Json(channels)),
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
pub async fn list_messages(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let channel = req.query::<String>("channel");
    let limit = req.query::<usize>("limit").unwrap_or(50).min(200);
    let before = req.query::<i64>("before");
    match db.list_messages(channel.as_deref(), limit, before) {
        Ok((messages, has_more)) => res.render(Json(serde_json::json!({
            "messages": messages, "total": messages.len(), "has_more": has_more
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
pub async fn create_message(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(config) = get_config(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No config"));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let body: CreateMessageRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(json_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {}", e),
            ));
            return;
        }
    };
    if body.text.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(json_error(StatusCode::BAD_REQUEST, "Text cannot be empty"));
        return;
    }
    if body.text.len() > config.max_text_size {
        res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
        res.render(json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Payload too large",
        ));
        return;
    }
    let channel = if body.channel.is_empty() {
        "inbox".to_string()
    } else {
        body.channel
    };
    let now = chrono::Utc::now().timestamp();
    let message = Message {
        id: Uuid::new_v4().to_string(),
        channel,
        kind: MessageKind::Text,
        title: body.title,
        text: Some(body.text),
        file_name: None,
        file_path: None,
        file_size: None,
        mime_type: None,
        created_at: now,
        expires_at: None,
    };
    match db.insert_message(&message) {
        Ok(_) => res.render(Json(message)),
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
pub async fn get_message(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let id = req.param::<String>("id").unwrap_or_default();
    match db.get_message(&id) {
        Ok(Some(message)) => res.render(Json(message)),
        Ok(None) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(json_error(StatusCode::NOT_FOUND, "Message not found"));
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
pub async fn delete_message(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(config) = get_config(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No config"));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let id = req.param::<String>("id").unwrap_or_default();
    match db.delete_message(&id) {
        Ok(Some(message)) => {
            if message.kind == MessageKind::File {
                if let Some(file_path) = &message.file_path {
                    let full_path = config.uploads_dir().join(file_path);
                    if let Ok(canonical) = full_path.canonicalize() {
                        let canonical_uploads = config
                            .uploads_dir()
                            .canonicalize()
                            .unwrap_or_else(|_| config.uploads_dir());
                        if canonical.starts_with(&canonical_uploads) {
                            let _ = std::fs::remove_file(canonical);
                        }
                    }
                }
            }
            res.render(Json(serde_json::json!({"deleted": true, "id": id})));
        }
        Ok(None) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(json_error(StatusCode::NOT_FOUND, "Message not found"));
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
pub async fn upload_file(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(config) = get_config(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No config"));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let channel = req
        .query::<String>("channel")
        .unwrap_or_else(|| "files".to_string());
    let file = match req.file("file").await {
        Some(f) => f,
        None => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(json_error(StatusCode::BAD_REQUEST, "No file provided"));
            return;
        }
    };
    let file_size = file.size() as i64;
    if file_size > config.max_file_size as i64 {
        res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
        res.render(json_error(StatusCode::PAYLOAD_TOO_LARGE, "File too large"));
        return;
    }
    let file_id = Uuid::new_v4().to_string();
    let original_name = file.name().unwrap_or("unknown").to_string();
    let extension = PathBuf::from(&original_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e))
        .unwrap_or_default();
    let safe_filename = format!("{}{}", file_id, extension);
    let mime_type = file
        .content_type()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let file_path = config.uploads_dir().join(&safe_filename);
    if let Err(e) = std::fs::copy(file.path(), &file_path) {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to save file: {}", e),
        ));
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let message = Message {
        id: file_id,
        channel,
        kind: MessageKind::File,
        title: Some(original_name.clone()),
        text: None,
        file_name: Some(original_name),
        file_path: Some(safe_filename),
        file_size: Some(file_size),
        mime_type: Some(mime_type),
        created_at: now,
        expires_at: None,
    };
    match db.insert_message(&message) {
        Ok(_) => res.render(Json(message)),
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
pub async fn download_file(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(config) = get_config(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No config"));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"));
        return;
    };
    let id = req.param::<String>("file_id").unwrap_or_default();
    let message = match db.get_message(&id) {
        Ok(Some(msg)) => msg,
        Ok(None) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(json_error(StatusCode::NOT_FOUND, "File not found"));
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
    };
    if message.kind != MessageKind::File {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(json_error(StatusCode::NOT_FOUND, "Not a file message"));
        return;
    }
    let Some(file_path) = &message.file_path else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "File path missing",
        ));
        return;
    };
    let full_path = config.uploads_dir().join(file_path);
    let canonical = match full_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(json_error(StatusCode::NOT_FOUND, "File not found on disk"));
            return;
        }
    };
    let canonical_uploads = config
        .uploads_dir()
        .canonicalize()
        .unwrap_or_else(|_| config.uploads_dir());
    if !canonical.starts_with(&canonical_uploads) {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(json_error(StatusCode::NOT_FOUND, "File not found"));
        return;
    }
    let filename = message.file_name.unwrap_or_else(|| "download".to_string());
    // Sanitize filename for Content-Disposition: strip path separators and control chars
    let safe_display_name: String = filename
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0' | '\r' | '\n'))
        .collect();
    let content_type = message
        .mime_type
        .unwrap_or_else(|| "application/octet-stream".to_string());

    match std::fs::read(&canonical) {
        Ok(bytes) => {
            res.add_header(
                "content-disposition",
                &format!(
                    "attachment; filename=\"{}\"",
                    safe_display_name.replace('"', "_")
                ),
                true,
            )
            .ok();
            res.add_header("content-type", &content_type, true).ok();
            res.add_header("content-length", &bytes.len().to_string(), true)
                .ok();
            res.body(bytes);
        }
        Err(_) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(json_error(StatusCode::NOT_FOUND, "File not found on disk"));
        }
    }
}

fn public_url() -> String {
    std::env::var("DROP_PUBLIC_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:8080".to_string())
}

fn project_enum_from_depot(depot: &Depot) -> serde_json::Value {
    if let Ok(projects) = depot.obtain::<Arc<projects::ProjectsConfig>>() {
        let mut names: Vec<String> = projects.projects.keys().cloned().collect();
        names.sort();
        serde_json::json!(names)
    } else {
        serde_json::json!(["private-drop", "private-drop-v4", "gpt-sandbox", "paper"])
    }
}

#[handler]
pub async fn openapi_json(res: &mut Response) {
    match serde_json::from_str::<serde_json::Value>(include_str!("../data/openapi.json")) {
        Ok(mut spec) => {
            spec["openapi"] = serde_json::Value::String("3.1.0".to_string());
            spec["servers"] = serde_json::json!([{
                "url": public_url(),
                "description": "Public server"
            }]);
            res.render(Json(spec));
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Invalid OpenAPI schema: {}", e),
            ));
        }
    }
}

#[handler]
pub async fn codex_openapi_json(depot: &mut Depot, res: &mut Response) {
    let project_enum = project_enum_from_depot(depot);
    let mut spec =
        match serde_json::from_str::<serde_json::Value>(include_str!("../data/openapi.json")) {
            Ok(spec) => spec,
            Err(e) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &e.to_string(),
                ));
                return;
            }
        };
    spec["openapi"] = serde_json::json!("3.1.0");
    spec["servers"] = serde_json::json!([{ "url": public_url(), "description": "Public server" }]);
    spec["info"] = serde_json::json!({"title":"Private Drop Codex API","version":env!("CARGO_PKG_VERSION"),"description":"Codex-only project API. Message, file, and channel APIs are excluded."});
    spec["paths"] = serde_json::json!({
        "/api/codex/context": spec["paths"]["/api/codex/context"].clone(),
        "/api/codex/apply_patch": spec["paths"]["/api/codex/apply_patch"].clone(),
        "/api/codex/edit": spec["paths"]["/api/codex/edit"].clone(),
        "/api/codex/check": spec["paths"]["/api/codex/check"].clone(),
        "/api/codex/report": spec["paths"]["/api/codex/report"].clone()
    });
    spec["components"]["schemas"] = serde_json::json!({
        "ContextRequest": spec["components"]["schemas"]["ContextRequest"].clone(),
        "ContextResponse": spec["components"]["schemas"]["ContextResponse"].clone(),
        "PatchRequest": spec["components"]["schemas"]["PatchRequest"].clone(),
        "PatchResponse": spec["components"]["schemas"]["PatchResponse"].clone(),
        "ReplaceTextEdit": spec["components"]["schemas"]["ReplaceTextEdit"].clone(),
        "ReplaceRangeEdit": spec["components"]["schemas"]["ReplaceRangeEdit"].clone(),
        "AppendFileEdit": spec["components"]["schemas"]["AppendFileEdit"].clone(),
        "CreateFileEdit": spec["components"]["schemas"]["CreateFileEdit"].clone(),
        "WriteFileEdit": spec["components"]["schemas"]["WriteFileEdit"].clone(),
        "EditRequest": spec["components"]["schemas"]["EditRequest"].clone(),
        "EditResponse": spec["components"]["schemas"]["EditResponse"].clone(),
        "CheckRequest": spec["components"]["schemas"]["CheckRequest"].clone(),
        "CheckResponse": spec["components"]["schemas"]["CheckResponse"].clone(),
        "ReportRequest": spec["components"]["schemas"]["ReportRequest"].clone(),
        "ReportResponse": spec["components"]["schemas"]["ReportResponse"].clone()
    });
    for name in [
        "ContextRequest",
        "PatchRequest",
        "EditRequest",
        "CheckRequest",
        "ReportRequest",
    ] {
        spec["components"]["schemas"][name]["properties"]["project"]["enum"] = project_enum.clone();
        spec["components"]["schemas"][name]["properties"]["project"]["description"] = serde_json::json!("Whitelisted project name; not a report channel such as omo, inbox, xline, thesis, packfix, or files.");
    }
    spec["components"]["schemas"]["ReportRequest"]["properties"]["channel"]["description"] =
        serde_json::json!("Report channel; not the project field.");
    res.render(Json(spec));
}

// ============================================================================
// Web UI
// ============================================================================

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn app_shell(title: &str, page_js: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{title} - Private Drop</title>
<style>
*{{margin:0;padding:0;box-sizing:border-box}}
body{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:#f5f5f5;color:#333}}
.container{{max-width:800px;margin:0 auto;padding:16px}}
.header{{background:#2c3e50;color:white;padding:16px;margin-bottom:16px;border-radius:8px}}
.header h1{{font-size:1.5em}}
.nav{{display:flex;gap:8px;margin-bottom:16px;flex-wrap:wrap}}
.nav a{{padding:8px 16px;background:#3498db;color:white;text-decoration:none;border-radius:4px;font-size:14px}}
.nav a:hover{{background:#2980b9}}
.card{{background:white;border-radius:8px;padding:16px;margin-bottom:12px;box-shadow:0 2px 4px rgba(0,0,0,0.1)}}
.card-header{{display:flex;justify-content:space-between;align-items:center;margin-bottom:8px}}
.card-title{{font-weight:bold;font-size:1.1em;word-break:break-word}}
.card-meta{{color:#666;font-size:0.9em}}
.card-text{{white-space:pre-wrap;word-break:break-word;line-height:1.5}}
.btn{{padding:8px 16px;border:none;border-radius:4px;cursor:pointer;font-size:14px;text-decoration:none;display:inline-block}}
.btn-primary{{background:#3498db;color:white}}
.btn-danger{{background:#e74c3c;color:white}}
.btn-success{{background:#27ae60;color:white}}
.btn-sm{{padding:4px 12px;font-size:12px}}
.form-group{{margin-bottom:12px}}
.form-group label{{display:block;margin-bottom:4px;font-weight:bold}}
.form-group input,.form-group textarea,.form-group select{{width:100%;padding:10px;border:1px solid #ddd;border-radius:4px;font-size:14px}}
.form-group textarea{{min-height:150px;resize:vertical}}
.form-actions{{display:flex;gap:8px}}
.file-info{{display:flex;align-items:center;gap:8px}}
.file-icon{{font-size:24px}}
.file-size{{color:#666;font-size:0.9em}}
.token-form{{max-width:400px;margin:100px auto}}
.alert{{padding:12px;border-radius:4px;margin-bottom:12px}}
.alert-error{{background:#f8d7da;color:#721c24}}
.alert-success{{background:#d4edda;color:#155724}}
.channel-badge{{display:inline-block;padding:2px 8px;background:#ecf0f1;border-radius:12px;font-size:0.8em}}
.loading{{text-align:center;padding:40px;color:#666}}
@media(max-width:600px){{.container{{padding:8px}}.header{{padding:12px}}.card{{padding:12px}}}}
</style>
</head>
<body>
<div class="container">
<div class="header"><h1>Private Drop</h1></div>
<div class="nav"><a href="/channels">Channels</a><a href="/c/inbox">Inbox</a><a href="/c/files">Files</a><a href="/send">Send</a></div>
<div id="app"><div class="loading">Loading...</div></div>
</div>
<script>
function getToken(){{return localStorage.getItem('drop_token')||''}}
function setToken(t){{localStorage.setItem('drop_token',t)}}
function clearToken(){{localStorage.removeItem('drop_token')}}
function requireToken(){{if(!getToken()){{window.location.href='/login';return false}}return true}}
async function apiCall(url,options={{}}){{const token=getToken();if(!token){{window.location.href='/login';return null}}const headers={{...options.headers}};headers['Authorization']='Bearer '+token;const resp=await fetch(url,{{...options,headers}});if(resp.status===401){{clearToken();window.location.href='/login';return null}}return resp}}
function escapeHtml(s){{const d=document.createElement('div');d.textContent=s;return d.innerHTML}}
function formatSize(b){{if(b<1024)return b+' B';if(b<1048576)return(b/1024).toFixed(1)+' KB';if(b<1073741824)return(b/1048576).toFixed(1)+' MB';return(b/1073741824).toFixed(2)+' GB'}}
function fmtTime(ts){{const d=new Date(ts*1000);return d.toLocaleString()}}
async function deleteMsg(id){{if(!confirm('Delete this message?'))return;const r=await apiCall('/api/messages/'+id,{{method:'DELETE'}});if(r&&r.ok)window.location.reload()}}
function copyText(t){{navigator.clipboard.writeText(t).then(()=>alert('Copied!'))}}
{page_js}
</script>
</body>
</html>"#,
        title = html_escape(title),
        page_js = page_js
    )
}

#[handler]
pub async fn login_page(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let page_js = r#"
(function(){
    if(getToken()){window.location.href='/c/inbox';return}
    document.getElementById('app').innerHTML=
        '<div class="token-form"><div class="card">'+
        '<h2 style="margin-bottom:16px">Login</h2>'+
        '<div id="err"></div>'+
        '<form id="lf">'+
        '<div class="form-group"><label for="token">Access Token</label>'+
        '<input type="password" id="token" placeholder="Enter your token" required autofocus></div>'+
        '<div class="form-actions"><button type="submit" class="btn btn-primary">Login</button></div>'+
        '</form></div></div>';
    document.getElementById('lf').addEventListener('submit',function(e){
        e.preventDefault();
        var t=document.getElementById('token').value.trim();
        if(!t)return;
        setToken(t);
        window.location.href='/c/inbox';
    });
})()
"#;
    res.render(Text::Html(app_shell("Login", page_js)));
}

#[handler]
pub async fn home_page(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    // Client-side: redirect to /c/inbox if logged in, else /login
    let page_js = r#"
(function(){
    if(!getToken()){window.location.href='/login';return}
    window.location.href='/channels';
})()
"#;
    res.render(Text::Html(app_shell("Home", page_js)));
}

#[handler]
pub async fn channels_page(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let page_js = r#"
(async function(){
    if(!requireToken())return;
    var app=document.getElementById('app');
    try{
        var r=await apiCall('/api/channels');
        if(!r)return;
        if(!r.ok){var d=await r.json();app.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to load channels')+'</div>';return}
        var channels=await r.json();
        var html='<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px"><h2>Channels</h2><a href="/send" class="btn btn-primary">Send</a></div>';
        if(channels.length===0){
            html+='<div class="card"><p style="color:#666;text-align:center">No channels yet</p></div>';
        }else{
            channels.forEach(function(ch){
                html+='<a href="/c/'+encodeURIComponent(ch.name)+'" style="color:inherit;text-decoration:none"><div class="card"><div class="card-header"><div><div class="card-title">'+escapeHtml(ch.display_name||ch.name)+'</div><div class="card-meta">'+escapeHtml(ch.name)+'</div></div><span class="channel-badge">'+ch.message_count+' message'+(ch.message_count===1?'':'s')+'</span></div></div></a>';
            });
        }
        app.innerHTML=html;
    }catch(e){
        app.innerHTML='<div class="alert alert-error">Error: '+escapeHtml(e.message)+'</div>';
    }
})()
"#;
    res.render(Text::Html(app_shell("Channels", page_js)));
}

#[handler]
pub async fn channel_page(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let channel = req.param::<String>("channel").unwrap_or_default();
    let page_js = format!(
        r#"
(async function(){{
    if(!requireToken())return;
    var ch={channel_json};
    var app=document.getElementById('app');
    try{{
        var r=await apiCall('/api/messages?channel='+encodeURIComponent(ch)+'&limit=50');
        if(!r)return;
        if(!r.ok){{var d=await r.json();app.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to load')+'</div>';return}}
        var data=await r.json();
        var html='<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px">'+
            '<div><a href="/channels" style="display:inline-block;margin-bottom:8px;color:#3498db;text-decoration:none">← Back to Channels</a>'+
            '<h2>'+escapeHtml(ch)+'</h2></div>'+
            '<a href="/send?channel='+encodeURIComponent(ch)+'" class="btn btn-primary">Send</a></div>';
        if(data.messages.length===0){{
            html+='<div class="card"><p style="color:#666;text-align:center">No messages yet</p></div>';
        }}else{{
            data.messages.forEach(function(m){{
                var title=m.title||(m.kind==='file'?(m.file_name||'File'):'Text');
                var ts=fmtTime(m.created_at);
                var body='';
                if(m.kind==='text'){{
                    var t=m.text||'';
                    body='<div class="card-text">'+escapeHtml(t.length>200?t.substring(0,200)+'...':t)+'</div>';
                }}else{{
                    body='<div class="file-info"><span class="file-icon">📎</span><div>'+
                        '<div style="font-weight:bold">'+escapeHtml(m.file_name||'unknown')+'</div>'+
                        '<div class="file-size">'+formatSize(m.file_size||0)+'</div></div></div>';
                }}
                var actions='';
                if(m.kind==='text'){{
                    actions='<button class="btn btn-sm btn-primary js-copy" data-text-id="t-'+m.id+'">Copy</button> '+
                        '<button class="btn btn-sm btn-danger js-delete" data-delete-id="'+m.id+'">Del</button>';
                }}else{{
                    actions='<a href="/api/files/'+m.id+'" class="btn btn-sm btn-success" download>Download</a> '+
                        '<button class="btn btn-sm btn-danger js-delete" data-delete-id="'+m.id+'">Del</button>';
                }}
                html+='<div class="card" id="t-'+m.id+'"><div class="card-header"><div>'+
                    '<div class="card-title"><a href="/m/'+m.id+'" style="color:inherit;text-decoration:none">'+escapeHtml(title)+'</a></div>'+
                    '<div class="card-meta">'+ts+'</div></div>'+
                    '<div class="form-actions">'+actions+'</div></div>'+body+'</div>';
            }});
        }}
        app.innerHTML=html;
        app.addEventListener('click',function(e){{
            var btn=e.target.closest('.js-copy');
            if(btn){{var el=document.getElementById(btn.getAttribute('data-text-id'));if(el)copyText(el.textContent);return}}
            btn=e.target.closest('.js-delete');
            if(btn){{deleteMsg(btn.getAttribute('data-delete-id'));return}}
        }});
    }}catch(e){{
        app.innerHTML='<div class="alert alert-error">Error: '+escapeHtml(e.message)+'</div>';
    }}
}})()
"#,
        channel_json = serde_json::to_string(&channel).unwrap()
    );
    res.render(Text::Html(app_shell(&channel, &page_js)));
}

#[handler]
pub async fn message_page(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let id = req.param::<String>("id").unwrap_or_default();
    let page_js = format!(
        r#"
(async function(){{
    if(!requireToken())return;
    var msgId={id_json};
    var app=document.getElementById('app');
    try{{
        var r=await apiCall('/api/messages/'+msgId);
        if(!r)return;
        if(!r.ok){{app.innerHTML='<div class="alert alert-error">Message not found</div>';return}}
        var m=await r.json();
        var ts=fmtTime(m.created_at);
        var html='<div class="card"><div style="display:flex;justify-content:space-between;margin-bottom:12px">'+
            '<div><span class="channel-badge">'+escapeHtml(m.channel)+'</span> <span class="card-meta">'+ts+'</span></div>'+
            '<div class="form-actions">';
        if(m.kind==='text'){{
            html+='<button class="btn btn-sm btn-primary js-copy" data-text-id="ft">Copy</button> ';
        }}else{{
            html+='<a href="/api/files/'+m.id+'" class="btn btn-sm btn-success" download>Download</a> ';
        }}
        html+='<button class="btn btn-sm btn-danger js-delete" data-delete-id="'+m.id+'">Del</button></div></div>';
        if(m.kind==='text'){{
            html+='<div id="ft" class="card-text" style="max-height:none">'+escapeHtml(m.text||'')+'</div>';
        }}else{{
            html+='<div class="file-info" style="font-size:1.2em"><span class="file-icon" style="font-size:48px">📎</span>'+
                '<div><div style="font-weight:bold;font-size:1.2em">'+escapeHtml(m.file_name||'unknown')+'</div>'+
                '<div class="file-size">'+formatSize(m.file_size||0)+'</div>'+
                '<div class="file-size">'+escapeHtml(m.mime_type||'')+'</div></div></div>';
        }}
        html+='</div>';
        var title=m.title||(m.kind==='file'?(m.file_name||'File'):'Message');
        app.innerHTML='<h2 style="margin-bottom:16px">'+escapeHtml(title)+'</h2>'+html;
        app.addEventListener('click',function(e){{
            var btn=e.target.closest('.js-copy');
            if(btn){{var el=document.getElementById(btn.getAttribute('data-text-id'));if(el)copyText(el.textContent);return}}
            btn=e.target.closest('.js-delete');
            if(btn){{deleteMsg(btn.getAttribute('data-delete-id'));return}}
        }});
    }}catch(e){{
        app.innerHTML='<div class="alert alert-error">Error: '+escapeHtml(e.message)+'</div>';
    }}
}})()
"#,
        id_json = serde_json::to_string(&id).unwrap()
    );
    res.render(Text::Html(app_shell("Message", &page_js)));
}

#[handler]
pub async fn send_page(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let default_channel = req
        .query::<String>("channel")
        .unwrap_or_else(|| "inbox".to_string());
    let page_js = format!(
        r#"
(async function(){{
    if(!requireToken())return;
    var defCh={channel_json};
    var app=document.getElementById('app');
    app.innerHTML=
        '<h2 style="margin-bottom:16px">Send Message</h2>'+
        '<div id="msg"></div>'+
        '<div class="card"><h3 style="margin-bottom:12px">Text Message</h3>'+
        '<form id="sf">'+
        '<div class="form-group"><label for="channel">Channel</label>'+
        '<select id="channel">'+
        '<option value="inbox">inbox</option>'+
        '<option value="xline">xline</option>'+
        '<option value="thesis">thesis</option>'+
        '<option value="packfix">packfix</option>'+
        '<option value="omo">omo</option>'+
        '<option value="files">files</option>'+
        '</select></div>'+
        '<div class="form-group"><label for="title">Title (optional)</label>'+
        '<input type="text" id="title" placeholder="Message title"></div>'+
        '<div class="form-group"><label for="text">Text</label>'+
        '<textarea id="text" placeholder="Paste your text here..." rows="10" required></textarea></div>'+
        '<div class="form-actions"><button type="submit" class="btn btn-primary">Send</button></div>'+
        '</form></div>'+
        '<div class="card" style="margin-top:16px"><h3 style="margin-bottom:12px">Upload File</h3>'+
        '<form id="ff">'+
        '<div class="form-group"><label for="file">File</label>'+
        '<input type="file" id="file" required></div>'+
        '<div class="form-actions"><button type="submit" class="btn btn-success">Upload</button></div>'+
        '</form></div>';
    document.getElementById('channel').value=defCh;
    document.getElementById('sf').addEventListener('submit',async function(e){{
        e.preventDefault();
        var ch=document.getElementById('channel').value;
        var title=document.getElementById('title').value||null;
        var text=document.getElementById('text').value;
        var msgEl=document.getElementById('msg');
        try{{
            var r=await apiCall('/api/messages',{{
                method:'POST',
                headers:{{'Content-Type':'application/json'}},
                body:JSON.stringify({{channel:ch,title:title,text:text}})
            }});
            if(!r)return;
            if(r.ok){{
                window.location.href='/c/'+encodeURIComponent(ch);
            }}else{{
                var d=await r.json();
                msgEl.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to send')+'</div>';
            }}
        }}catch(err){{
            msgEl.innerHTML='<div class="alert alert-error">'+escapeHtml(err.message)+'</div>';
        }}
    }});
    document.getElementById('ff').addEventListener('submit',async function(e){{
        e.preventDefault();
        var ch=document.getElementById('channel').value;
        var fileInput=document.getElementById('file');
        var msgEl=document.getElementById('msg');
        if(!fileInput.files[0])return;
        var fd=new FormData();
        fd.append('file',fileInput.files[0]);
        try{{
            var r=await apiCall('/api/files?channel='+encodeURIComponent(ch),{{method:'POST',body:fd}});
            if(!r)return;
            if(r.ok){{
                window.location.href='/c/'+encodeURIComponent(ch);
            }}else{{
                var d=await r.json();
                msgEl.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to upload')+'</div>';
            }}
        }}catch(err){{
            msgEl.innerHTML='<div class="alert alert-error">'+escapeHtml(err.message)+'</div>';
        }}
    }});
}})()
"#,
        channel_json = serde_json::to_string(&default_channel).unwrap()
    );
    res.render(Text::Html(app_shell("Send", &page_js)));
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let config = Config::from_env();
    if !config.is_auth_enabled() {
        tracing::warn!(
            "DROP_TOKEN is not set! Running in development mode without authentication."
        );
        tracing::warn!("Set DROP_TOKEN environment variable to enable authentication.");
    }
    tracing::info!("Starting Private Drop v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Data directory: {:?}", config.data_dir);
    let addr = config.addr.clone();
    tracing::info!("Listening on: {}", addr);
    std::fs::create_dir_all(config.uploads_dir())?;
    let db = Database::open(&config.db_path())?;
    tracing::info!("Database initialized at {:?}", config.db_path());

    // Set max payload size to 2MB for text messages
    salvo::http::request::set_global_secure_max_size(config.max_text_size);

    // Load projects config for Codex API
    let projects_config = match projects::ProjectsConfig::load() {
        Ok(cfg) => {
            tracing::info!(
                "Loaded projects config with {} projects",
                cfg.projects.len()
            );
            Some(Arc::new(cfg))
        }
        Err(e) => {
            tracing::warn!(
                "Projects config not loaded: {}. Codex API will be disabled.",
                e
            );
            None
        }
    };

    let cors = Cors::permissive();
    let config = Arc::new(config);
    let db = Arc::new(db);

    let api_router = Router::with_path("api")
        .push(Router::with_path("health").get(health))
        .push(
            Router::new()
                .hoop(AuthMiddleware)
                .push(Router::with_path("channels").get(list_channels))
                .push(
                    Router::with_path("messages")
                        .get(list_messages)
                        .post(create_message),
                )
                .push(
                    Router::with_path("messages/{id}")
                        .get(get_message)
                        .delete(delete_message),
                )
                .push(Router::with_path("files/{file_id}").get(download_file))
                .push(Router::with_path("files").post(upload_file)),
        );

    let web_router = Router::new()
        .push(Router::with_path("login").get(login_page))
        .push(Router::with_path("channels").get(channels_page))
        .push(Router::with_path("send").get(send_page))
        .push(Router::with_path("c/{channel}").get(channel_page))
        .push(Router::with_path("m/{id}").get(message_page))
        .push(Router::with_path("").get(home_page));

    let openapi_router = Router::with_path("openapi.json").get(openapi_json);
    let codex_openapi_router = Router::with_path("codex-openapi.json").get(codex_openapi_json);

    let mut router = Router::new()
        .hoop(affix_state::inject(config.clone()))
        .hoop(affix_state::inject(db.clone()))
        .hoop(cors.into_handler())
        .push(api_router)
        .push(openapi_router)
        .push(codex_openapi_router)
        .push(web_router);

    // Add Codex API routes if projects config is loaded
    if let Some(projects_cfg) = projects_config {
        router = router.hoop(affix_state::inject(projects_cfg)).push(
            Router::with_path("api/codex")
                .hoop(AuthMiddleware)
                .push(Router::with_path("context").post(codex::codex_context))
                .push(Router::with_path("apply_patch").post(codex::codex_apply_patch))
                .push(Router::with_path("edit").post(codex::codex_edit))
                .push(Router::with_path("check").post(codex::codex_check))
                .push(Router::with_path("report").post(codex::codex_report)),
        );
    }

    let acceptor = TcpListener::new(addr.clone()).bind().await;
    tracing::info!("Server started successfully!");
    let port = addr.split(':').last().unwrap_or("8080");
    tracing::info!("Web UI: http://localhost:{}", port);
    tracing::info!("API: http://localhost:{}/api", port);
    tracing::info!("OpenAPI: http://localhost:{}/openapi.json", port);
    tracing::info!(
        "Codex OpenAPI: http://localhost:{}/codex-openapi.json",
        port
    );
    Server::new(acceptor).serve(router).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uuid_generation_not_empty() {
        let id = Uuid::new_v4().to_string();
        assert!(!id.is_empty());
        assert_eq!(id.len(), 36); // UUID v4 with hyphens
        assert!(id.contains('-'));
    }

    #[test]
    fn test_uuid_generation_unique() {
        let id1 = Uuid::new_v4().to_string();
        let id2 = Uuid::new_v4().to_string();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_config_from_env_defaults() {
        // Clear env vars to test defaults
        std::env::remove_var("DROP_ADDR");
        std::env::remove_var("DROP_DATA");
        std::env::remove_var("DROP_TOKEN");

        let config = Config::from_env();
        assert_eq!(config.addr, "0.0.0.0:8080");
        assert_eq!(config.data_dir, PathBuf::from("./data"));
        assert_eq!(config.token, None);
        assert!(!config.is_auth_enabled());
        assert_eq!(config.max_text_size, 2 * 1024 * 1024);
        assert_eq!(config.max_file_size, 100 * 1024 * 1024);
    }

    #[test]
    fn test_config_validate_token() {
        let config = Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: PathBuf::from("./data"),
            token: Some("secret123".to_string()),
            max_text_size: 2 * 1024 * 1024,
            max_file_size: 100 * 1024 * 1024,
        };
        assert!(config.is_auth_enabled());
        assert!(config.validate_token("secret123"));
        assert!(!config.validate_token("wrong"));
        assert!(!config.validate_token(""));
    }

    #[test]
    fn test_config_validate_token_none() {
        let config = Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: PathBuf::from("./data"),
            token: None,
            max_text_size: 2 * 1024 * 1024,
            max_file_size: 100 * 1024 * 1024,
        };
        assert!(!config.is_auth_enabled());
        // When no token is set, validation always returns false
        assert!(!config.validate_token("anything"));
    }

    #[test]
    fn test_filename_sanitization() {
        // Test that path separators are stripped from display names
        let filename = "test/file\\name.txt";
        let safe: String = filename
            .chars()
            .filter(|c| !matches!(c, '/' | '\\' | '\0' | '\r' | '\n'))
            .collect();
        assert_eq!(safe, "testfilename.txt");
    }

    #[test]
    fn test_filename_sanitization_quotes() {
        let filename = "file\"name.txt";
        let safe = filename.replace('"', "_");
        assert_eq!(safe, "file_name.txt");
    }

    #[test]
    fn test_message_serialization() {
        let msg = Message {
            id: "test-id".to_string(),
            channel: "inbox".to_string(),
            kind: MessageKind::Text,
            title: Some("Test".to_string()),
            text: Some("Hello".to_string()),
            file_name: None,
            file_path: None,
            file_size: None,
            mime_type: None,
            created_at: 1234567890,
            expires_at: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("test-id"));
        assert!(json.contains("inbox"));
        assert!(json.contains("text"));
    }
}
