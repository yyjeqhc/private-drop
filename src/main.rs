#![recursion_limit = "256"]

use salvo::cors::Cors;
use salvo::prelude::*;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
#[cfg(test)]
use uuid::Uuid;

mod agent;
mod auth;
mod codex;
mod config;
mod db;
mod desktop;
mod drop_api;
mod models;
mod openapi;
mod projects;
mod web;

pub(crate) use auth::{get_config, get_db, json_error, AuthMiddleware};
pub(crate) use config::load_startup_env_files;
#[cfg(test)]
pub(crate) use config::parse_env_file_line;
pub use config::Config;
pub use db::Database;
pub(crate) use desktop::{
    append_desktop_task_event, claim_desktop_task, claim_next_desktop_task, create_desktop_task,
    desktop_task_op, get_desktop_task_detail, list_desktop_tasks,
};
pub(crate) use drop_api::{
    create_message, delete_message, download_file, get_message, health, list_channels,
    list_messages, upload_file,
};
pub use models::{
    AgentModelProfileRecord, AgentSpecRecord, Channel, CodexGoalRecord, CommandAuditRecord,
    CreateDesktopTaskRequest, CreateMessageRequest, DesktopTask, DesktopTaskClaimRequest,
    DesktopTaskEvent, DesktopTaskEventRequest, DesktopTaskOpRequest, Message, MessageKind,
};
pub(crate) use openapi::{codex_openapi_compact_json, codex_openapi_json, openapi_json};
pub(crate) use web::{
    agent_playground_page, channel_page, channels_page, desktop_page, desktop_task_page,
    frontend_app_js, frontend_styles_css, home_page, login_page, message_page, send_page,
};

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env_loads = load_startup_env_files().map_err(std::io::Error::other)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    for load in &env_loads {
        tracing::info!(
            "Loaded env file {} ({} variables set)",
            load.path.display(),
            load.loaded_count
        );
    }
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
                .push(Router::with_path("files").post(upload_file))
                .push(Router::with_path("desktop/task_op").post(desktop_task_op))
                .push(
                    Router::with_path("desktop/tasks")
                        .get(list_desktop_tasks)
                        .post(create_desktop_task),
                )
                .push(Router::with_path("desktop/tasks/claim_next").post(claim_next_desktop_task))
                .push(Router::with_path("desktop/tasks/{id}").get(get_desktop_task_detail))
                .push(Router::with_path("desktop/tasks/{id}/claim").post(claim_desktop_task))
                .push(Router::with_path("desktop/tasks/{id}/event").post(append_desktop_task_event))
                .push(Router::with_path("agent/run").post(agent::run_agent))
                .push(
                    Router::with_path("agent/specs")
                        .get(agent::list_agent_specs)
                        .post(agent::save_agent_spec),
                )
                .push(
                    Router::with_path("agent/specs/{id}")
                        .get(agent::get_agent_spec)
                        .delete(agent::delete_agent_spec),
                ),
        );

    let assets_router = Router::with_path("assets")
        .push(Router::with_path("app.js").get(frontend_app_js))
        .push(Router::with_path("styles.css").get(frontend_styles_css));

    let web_router = Router::new()
        .push(Router::with_path("login").get(login_page))
        .push(Router::with_path("channels").get(channels_page))
        .push(Router::with_path("send").get(send_page))
        .push(Router::with_path("desktop").get(desktop_page))
        .push(Router::with_path("agent/playground").get(agent_playground_page))
        .push(Router::with_path("c/{channel}").get(channel_page))
        .push(Router::with_path("m/{id}").get(message_page))
        .push(Router::with_path("desktop/tasks/{id}").get(desktop_task_page))
        .push(Router::with_path("").get(home_page));

    let openapi_router = Router::with_path("openapi.json").get(openapi_json);
    let codex_openapi_router = Router::with_path("codex-openapi.json").get(codex_openapi_json);
    let codex_openapi_compact_router =
        Router::with_path("codex-openapi-compact.json").get(codex_openapi_compact_json);

    let mut router = Router::new()
        .hoop(affix_state::inject(config.clone()))
        .hoop(affix_state::inject(db.clone()))
        .hoop(cors.into_handler())
        .push(api_router)
        .push(openapi_router)
        .push(codex_openapi_router)
        .push(codex_openapi_compact_router)
        .push(assets_router)
        .push(web_router);

    // Add Codex API routes if projects config is loaded
    if let Some(projects_cfg) = projects_config {
        router = router.hoop(affix_state::inject(projects_cfg)).push(
            Router::with_path("api/codex")
                .hoop(AuthMiddleware)
                .push(Router::with_path("context").post(codex::codex_context))
                .push(Router::with_path("projects").post(codex::codex_projects))
                .push(Router::with_path("context_batch").post(codex::codex_context_batch))
                .push(Router::with_path("apply_patch").post(codex::codex_apply_patch))
                .push(Router::with_path("edit").post(codex::codex_edit))
                .push(Router::with_path("artifact").post(codex::codex_artifact))
                .push(Router::with_path("git").post(codex::codex_git))
                .push(Router::with_path("command").post(codex::codex_command))
                .push(Router::with_path("command_request").post(codex::codex_command_request))
                .push(Router::with_path("command_request_op").post(codex::codex_command_request_op))
                .push(Router::with_path("job").post(codex::codex_job))
                .push(
                    Router::with_path("command_request_raw").post(codex::codex_command_request_raw),
                )
                .push(Router::with_path("command_requests").post(codex::codex_command_requests))
                .push(
                    Router::with_path("command_request_batch")
                        .post(codex::codex_command_request_batch),
                )
                .push(Router::with_path("command_approve").post(codex::codex_command_approve))
                .push(Router::with_path("command_reject").post(codex::codex_command_reject))
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
    tracing::info!(
        "Compact Codex OpenAPI: http://localhost:{}/codex-openapi-compact.json",
        port
    );
    Server::new(acceptor).serve(router).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_env_file_line_basic() {
        let parsed = parse_env_file_line("DROP_ADDR=127.0.0.1:8080")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.0, "DROP_ADDR");
        assert_eq!(parsed.1, "127.0.0.1:8080");
    }

    #[test]
    fn test_parse_env_file_line_quotes_and_export() {
        let parsed = parse_env_file_line("export RUST_LOG='info,codex.metrics=info'")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.0, "RUST_LOG");
        assert_eq!(parsed.1, "info,codex.metrics=info");
    }

    #[test]
    fn test_parse_env_file_line_ignores_empty_and_comments() {
        assert!(parse_env_file_line("").is_none());
        assert!(parse_env_file_line("  # comment").is_none());
    }

    #[test]
    fn test_parse_env_file_line_rejects_invalid_key() {
        assert!(parse_env_file_line("drop_token=x").unwrap().is_err());
        assert!(parse_env_file_line("DROP TOKEN=x").unwrap().is_err());
    }

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
    fn test_command_request_claim_is_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("drop.db")).unwrap();
        let record = CommandAuditRecord {
            id: "req-1".to_string(),
            project: "p".to_string(),
            command: "smoke".to_string(),
            command_text: Some("echo ok".to_string()),
            reason: Some("test".to_string()),
            status: "pending".to_string(),
            created_at: 1,
            approved_at: None,
            executed_at: None,
            exit_code: None,
            stdout_tail: None,
            stderr_tail: None,
            error: None,
        };
        db.insert_command_request(&record).unwrap();
        let claimed = db
            .claim_command_request_for_execution("req-1", 2, 0)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, "running");
        assert_eq!(claimed.approved_at, Some(2));
        assert_eq!(claimed.command_text.as_deref(), Some("echo ok"));
        let second = db
            .claim_command_request_for_execution("req-1", 3, 0)
            .unwrap();
        assert!(second.is_none());
        let current = db.get_command_request("req-1").unwrap().unwrap();
        assert_eq!(current.status, "running");
        assert_eq!(current.approved_at, Some(2));
    }

    #[test]
    fn test_command_request_claim_respects_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("drop.db")).unwrap();
        let record = CommandAuditRecord {
            id: "old-req".to_string(),
            project: "p".to_string(),
            command: "smoke".to_string(),
            command_text: Some("echo ok".to_string()),
            reason: None,
            status: "pending".to_string(),
            created_at: 10,
            approved_at: None,
            executed_at: None,
            exit_code: None,
            stdout_tail: None,
            stderr_tail: None,
            error: None,
        };
        db.insert_command_request(&record).unwrap();
        let claimed = db
            .claim_command_request_for_execution("old-req", 100, 50)
            .unwrap();
        assert!(claimed.is_none());
        let current = db.get_command_request("old-req").unwrap().unwrap();
        assert_eq!(current.status, "pending");
    }

    #[test]
    fn test_command_request_reject_only_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("drop.db")).unwrap();
        let record = CommandAuditRecord {
            id: "reject-req".to_string(),
            project: "p".to_string(),
            command: "smoke".to_string(),
            command_text: Some("echo ok".to_string()),
            reason: None,
            status: "pending".to_string(),
            created_at: 1,
            approved_at: None,
            executed_at: None,
            exit_code: None,
            stdout_tail: None,
            stderr_tail: None,
            error: None,
        };
        db.insert_command_request(&record).unwrap();
        let rejected = db
            .reject_command_request("reject-req", 2, "no")
            .unwrap()
            .unwrap();
        assert_eq!(rejected.status, "rejected");
        assert_eq!(rejected.error.as_deref(), Some("no"));
        let second = db.reject_command_request("reject-req", 3, "again").unwrap();
        assert!(second.is_none());
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
