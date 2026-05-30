use crate::{get_config, get_db, json_error, CreateMessageRequest, Message, MessageKind};
use salvo::prelude::*;
use std::path::PathBuf;
use uuid::Uuid;

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
