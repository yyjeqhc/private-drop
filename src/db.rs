use crate::{
    AgentModelProfileRecord, AgentSpecRecord, Channel, CodexGoalRecord, CommandAuditRecord,
    DesktopTask, DesktopTaskEvent, Message, MessageKind,
};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Database {
    conn: Mutex<Connection>,
}

fn row_to_desktop_task(row: &rusqlite::Row) -> rusqlite::Result<DesktopTask> {
    Ok(DesktopTask {
        id: row.get(0)?,
        title: row.get(1)?,
        instructions: row.get(2)?,
        status: row.get(3)?,
        priority: row.get(4)?,
        claimed_by: row.get(5)?,
        last_event: row.get(6)?,
        screenshot_url: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn row_to_desktop_task_event(row: &rusqlite::Row) -> rusqlite::Result<DesktopTaskEvent> {
    Ok(DesktopTaskEvent {
        id: row.get(0)?,
        task_id: row.get(1)?,
        status: row.get(2)?,
        worker: row.get(3)?,
        message: row.get(4)?,
        screenshot_url: row.get(5)?,
        created_at: row.get(6)?,
    })
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

fn row_to_agent_spec(row: &rusqlite::Row) -> rusqlite::Result<AgentSpecRecord> {
    Ok(AgentSpecRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        base_url: row.get(2)?,
        auth_token: row.get(3)?,
        openapi_json: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_agent_model_profile(row: &rusqlite::Row) -> rusqlite::Result<AgentModelProfileRecord> {
    Ok(AgentModelProfileRecord {
        id: row.get(0)?,
        base_url: row.get(1)?,
        api_key: row.get(2)?,
        model: row.get(3)?,
        temperature: row.get(4)?,
        max_rounds: row.get::<_, Option<i64>>(5)?.map(|v| v as usize),
        updated_at: row.get(6)?,
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
            CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at DESC);

            CREATE TABLE IF NOT EXISTS command_requests (
                id TEXT PRIMARY KEY,
                project TEXT NOT NULL,
                command TEXT NOT NULL,
                command_text TEXT,
                reason TEXT,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                approved_at INTEGER,
                executed_at INTEGER,
                exit_code INTEGER,
                stdout_tail TEXT,
                stderr_tail TEXT,
                error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_command_requests_created_at ON command_requests(created_at DESC);

            CREATE TABLE IF NOT EXISTS codex_goals (
                id TEXT PRIMARY KEY,
                project TEXT NOT NULL,
                title TEXT NOT NULL,
                summary TEXT,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                closed_at INTEGER,
                error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_codex_goals_created_at ON codex_goals(created_at DESC);

            CREATE TABLE IF NOT EXISTS desktop_tasks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                instructions TEXT NOT NULL,
                status TEXT NOT NULL,
                priority INTEGER NOT NULL,
                claimed_by TEXT,
                last_event TEXT,
                screenshot_url TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_desktop_tasks_status_priority ON desktop_tasks(status, priority DESC, created_at ASC);
            CREATE INDEX IF NOT EXISTS idx_desktop_tasks_updated_at ON desktop_tasks(updated_at DESC);

            CREATE TABLE IF NOT EXISTS desktop_task_events (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                status TEXT NOT NULL,
                worker TEXT,
                message TEXT,
                screenshot_url TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_desktop_task_events_task_created ON desktop_task_events(task_id, created_at ASC);

            CREATE TABLE IF NOT EXISTS agent_specs (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                base_url TEXT NOT NULL,
                auth_token TEXT NOT NULL,
                openapi_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_agent_specs_updated_at ON agent_specs(updated_at DESC);

            CREATE TABLE IF NOT EXISTS agent_model_profiles (
                id TEXT PRIMARY KEY,
                base_url TEXT NOT NULL,
                api_key TEXT NOT NULL,
                model TEXT NOT NULL,
                temperature REAL,
                max_rounds INTEGER,
                updated_at INTEGER NOT NULL
            );",

        )?;
        let has_command_text = {
            let mut stmt = conn.prepare("PRAGMA table_info(command_requests)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for row in rows {
                if row? == "command_text" {
                    found = true;
                    break;
                }
            }
            found
        };
        if !has_command_text {
            conn.execute(
                "ALTER TABLE command_requests ADD COLUMN command_text TEXT",
                [],
            )?;
        }
        Ok(())
    }

    pub fn upsert_agent_spec(&self, record: &AgentSpecRecord) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_specs (id, name, base_url, auth_token, openapi_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                base_url = excluded.base_url,
                auth_token = excluded.auth_token,
                openapi_json = excluded.openapi_json,
                updated_at = excluded.updated_at",
            params![
                record.id,
                record.name,
                record.base_url,
                record.auth_token,
                record.openapi_json,
                record.created_at,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_agent_specs(&self) -> anyhow::Result<Vec<AgentSpecRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, base_url, auth_token, openapi_json, created_at, updated_at
             FROM agent_specs ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_agent_spec)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn get_agent_spec(&self, id: &str) -> anyhow::Result<Option<AgentSpecRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, base_url, auth_token, openapi_json, created_at, updated_at
             FROM agent_specs WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_agent_spec)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn delete_agent_spec(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute("DELETE FROM agent_specs WHERE id = ?1", params![id])?;
        Ok(changed == 1)
    }

    pub fn upsert_agent_model_profile(
        &self,
        record: &AgentModelProfileRecord,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_model_profiles (id, base_url, api_key, model, temperature, max_rounds, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                base_url = excluded.base_url,
                api_key = excluded.api_key,
                model = excluded.model,
                temperature = excluded.temperature,
                max_rounds = excluded.max_rounds,
                updated_at = excluded.updated_at",
            params![
                record.id,
                record.base_url,
                record.api_key,
                record.model,
                record.temperature,
                record.max_rounds.map(|v| v as i64),
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_agent_model_profile(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<AgentModelProfileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, base_url, api_key, model, temperature, max_rounds, updated_at
             FROM agent_model_profiles WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_agent_model_profile)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
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

    pub fn insert_goal(&self, record: &CodexGoalRecord) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO codex_goals (id, project, title, summary, status, created_at, expires_at, closed_at, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.id,
                record.project,
                record.title,
                record.summary,
                record.status,
                record.created_at,
                record.expires_at,
                record.closed_at,
                record.error,
            ],
        )?;
        Ok(())
    }

    pub fn get_goal(&self, id: &str) -> anyhow::Result<Option<CodexGoalRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, project, title, summary, status, created_at, expires_at, closed_at, error FROM codex_goals WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(CodexGoalRecord {
                id: row.get(0)?,
                project: row.get(1)?,
                title: row.get(2)?,
                summary: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                expires_at: row.get(6)?,
                closed_at: row.get(7)?,
                error: row.get(8)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn list_goals(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<CodexGoalRecord>> {
        let limit = limit.clamp(1, 100) as i64;
        let conn = self.conn.lock().unwrap();
        let sql = match (project, status) {
            (Some(_), Some(_)) => "SELECT id, project, title, summary, status, created_at, expires_at, closed_at, error FROM codex_goals WHERE project = ?1 AND status = ?2 ORDER BY created_at DESC LIMIT ?3",
            (Some(_), None) => "SELECT id, project, title, summary, status, created_at, expires_at, closed_at, error FROM codex_goals WHERE project = ?1 ORDER BY created_at DESC LIMIT ?2",
            (None, Some(_)) => "SELECT id, project, title, summary, status, created_at, expires_at, closed_at, error FROM codex_goals WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2",
            (None, None) => "SELECT id, project, title, summary, status, created_at, expires_at, closed_at, error FROM codex_goals ORDER BY created_at DESC LIMIT ?1",
        };
        let mut stmt = conn.prepare(sql)?;
        let map_row = |row: &rusqlite::Row| {
            Ok(CodexGoalRecord {
                id: row.get(0)?,
                project: row.get(1)?,
                title: row.get(2)?,
                summary: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                expires_at: row.get(6)?,
                closed_at: row.get(7)?,
                error: row.get(8)?,
            })
        };
        let rows = match (project, status) {
            (Some(project), Some(status)) => {
                stmt.query_map(params![project, status, limit], map_row)?
            }
            (Some(project), None) => stmt.query_map(params![project, limit], map_row)?,
            (None, Some(status)) => stmt.query_map(params![status, limit], map_row)?,
            (None, None) => stmt.query_map(params![limit], map_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn update_goal_status(
        &self,
        id: &str,
        status: &str,
        closed_at: i64,
        error: Option<&str>,
    ) -> anyhow::Result<Option<CodexGoalRecord>> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE codex_goals SET status = ?2, closed_at = ?3, error = ?4 WHERE id = ?1",
            params![id, status, closed_at, error],
        )?;
        drop(conn);
        if changed == 1 {
            self.get_goal(id)
        } else {
            Ok(None)
        }
    }

    pub fn update_pending_goal_status(
        &self,
        id: &str,
        status: &str,
        closed_at: Option<i64>,
        error: Option<&str>,
    ) -> anyhow::Result<Option<CodexGoalRecord>> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE codex_goals SET status = ?2, closed_at = ?3, error = ?4 WHERE id = ?1 AND status = 'pending'",
            params![id, status, closed_at, error],
        )?;
        drop(conn);
        if changed == 1 {
            self.get_goal(id)
        } else {
            Ok(None)
        }
    }

    pub fn insert_command_request(&self, record: &CommandAuditRecord) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO command_requests (id, project, command, command_text, reason, status, created_at, approved_at, executed_at, exit_code, stdout_tail, stderr_tail, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.id,
                record.project,
                record.command,
                record.command_text,
                record.reason,
                record.status,
                record.created_at,
                record.approved_at,
                record.executed_at,
                record.exit_code,
                record.stdout_tail,
                record.stderr_tail,
                record.error,
            ],
        )?;
        Ok(())
    }

    pub fn get_command_request(&self, id: &str) -> anyhow::Result<Option<CommandAuditRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, project, command, command_text, reason, status, created_at, approved_at, executed_at, exit_code, stdout_tail, stderr_tail, error FROM command_requests WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(CommandAuditRecord {
                id: row.get(0)?,
                project: row.get(1)?,
                command: row.get(2)?,
                command_text: row.get(3)?,
                reason: row.get(4)?,
                status: row.get(5)?,
                created_at: row.get(6)?,
                approved_at: row.get(7)?,
                executed_at: row.get(8)?,
                exit_code: row.get(9)?,
                stdout_tail: row.get(10)?,
                stderr_tail: row.get(11)?,
                error: row.get(12)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn list_command_requests(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<CommandAuditRecord>> {
        let limit = limit.clamp(1, 100) as i64;
        let conn = self.conn.lock().unwrap();
        let sql = match (project, status) {
            (Some(_), Some(_)) => "SELECT id, project, command, command_text, reason, status, created_at, approved_at, executed_at, exit_code, stdout_tail, stderr_tail, error FROM command_requests WHERE project = ?1 AND status = ?2 ORDER BY created_at DESC LIMIT ?3",
            (Some(_), None) => "SELECT id, project, command, command_text, reason, status, created_at, approved_at, executed_at, exit_code, stdout_tail, stderr_tail, error FROM command_requests WHERE project = ?1 ORDER BY created_at DESC LIMIT ?2",
            (None, Some(_)) => "SELECT id, project, command, command_text, reason, status, created_at, approved_at, executed_at, exit_code, stdout_tail, stderr_tail, error FROM command_requests WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2",
            (None, None) => "SELECT id, project, command, command_text, reason, status, created_at, approved_at, executed_at, exit_code, stdout_tail, stderr_tail, error FROM command_requests ORDER BY created_at DESC LIMIT ?1",
        };
        let mut stmt = conn.prepare(sql)?;
        let map_row = |row: &rusqlite::Row| {
            Ok(CommandAuditRecord {
                id: row.get(0)?,
                project: row.get(1)?,
                command: row.get(2)?,
                command_text: row.get(3)?,
                reason: row.get(4)?,
                status: row.get(5)?,
                created_at: row.get(6)?,
                approved_at: row.get(7)?,
                executed_at: row.get(8)?,
                exit_code: row.get(9)?,
                stdout_tail: row.get(10)?,
                stderr_tail: row.get(11)?,
                error: row.get(12)?,
            })
        };
        let rows = match (project, status) {
            (Some(project), Some(status)) => {
                stmt.query_map(params![project, status, limit], map_row)?
            }
            (Some(project), None) => stmt.query_map(params![project, limit], map_row)?,
            (None, Some(status)) => stmt.query_map(params![status, limit], map_row)?,
            (None, None) => stmt.query_map(params![limit], map_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn reject_command_request(
        &self,
        id: &str,
        rejected_at: i64,
        error: &str,
    ) -> anyhow::Result<Option<CommandAuditRecord>> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE command_requests SET status = 'rejected', executed_at = ?2, error = ?3 WHERE id = ?1 AND status = 'pending'",
            params![id, rejected_at, error],
        )?;
        drop(conn);
        if changed == 1 {
            self.get_command_request(id)
        } else {
            Ok(None)
        }
    }

    pub fn expire_command_request(
        &self,
        id: &str,
        expired_at: i64,
        error: &str,
    ) -> anyhow::Result<Option<CommandAuditRecord>> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE command_requests SET status = 'expired', executed_at = ?2, error = ?3 WHERE id = ?1 AND status = 'pending'",
            params![id, expired_at, error],
        )?;
        drop(conn);
        if changed == 1 {
            self.get_command_request(id)
        } else {
            Ok(None)
        }
    }

    pub fn claim_command_request_for_execution(
        &self,
        id: &str,
        approved_at: i64,
        min_created_at: i64,
    ) -> anyhow::Result<Option<CommandAuditRecord>> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE command_requests SET status = 'running', approved_at = ?2 WHERE id = ?1 AND status = 'pending' AND created_at >= ?3",
            params![id, approved_at, min_created_at],
        )?;
        drop(conn);
        if changed == 1 {
            self.get_command_request(id)
        } else {
            Ok(None)
        }
    }

    pub fn update_command_request_result(&self, record: &CommandAuditRecord) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE command_requests SET status = ?2, approved_at = ?3, executed_at = ?4, exit_code = ?5, stdout_tail = ?6, stderr_tail = ?7, error = ?8 WHERE id = ?1",
            params![
                record.id,
                record.status,
                record.approved_at,
                record.executed_at,
                record.exit_code,
                record.stdout_tail,
                record.stderr_tail,
                record.error,
            ],
        )?;
        Ok(())
    }

    fn insert_desktop_task_event_locked(
        conn: &Connection,
        task_id: &str,
        status: &str,
        worker: Option<&str>,
        message: Option<&str>,
        screenshot_url: Option<&str>,
        now: i64,
    ) -> anyhow::Result<()> {
        conn.execute(
            "INSERT INTO desktop_task_events (id, task_id, status, worker, message, screenshot_url, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                uuid::Uuid::new_v4().to_string(),
                task_id,
                status,
                worker,
                message,
                screenshot_url,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn insert_desktop_task(&self, task: &DesktopTask) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO desktop_tasks (id, title, instructions, status, priority, claimed_by, last_event, screenshot_url, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                task.id,
                task.title,
                task.instructions,
                task.status,
                task.priority,
                task.claimed_by,
                task.last_event,
                task.screenshot_url,
                task.created_at,
                task.updated_at,
            ],
        )?;
        Self::insert_desktop_task_event_locked(
            &conn,
            &task.id,
            &task.status,
            None,
            task.last_event.as_deref(),
            None,
            task.created_at,
        )?;
        Ok(())
    }

    pub fn get_desktop_task(&self, id: &str) -> anyhow::Result<Option<DesktopTask>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, instructions, status, priority, claimed_by, last_event, screenshot_url, created_at, updated_at FROM desktop_tasks WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_desktop_task)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn list_desktop_task_events(&self, task_id: &str) -> anyhow::Result<Vec<DesktopTaskEvent>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, task_id, status, worker, message, screenshot_url, created_at
             FROM desktop_task_events WHERE task_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![task_id], row_to_desktop_task_event)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_desktop_tasks(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<DesktopTask>> {
        let limit = limit.clamp(1, 100) as i64;
        let conn = self.conn.lock().unwrap();
        let sql = match status {
            Some(_) => "SELECT id, title, instructions, status, priority, claimed_by, last_event, screenshot_url, created_at, updated_at FROM desktop_tasks WHERE status = ?1 ORDER BY priority DESC, created_at ASC LIMIT ?2",
            None => "SELECT id, title, instructions, status, priority, claimed_by, last_event, screenshot_url, created_at, updated_at FROM desktop_tasks ORDER BY updated_at DESC LIMIT ?1",
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = match status {
            Some(status) => stmt.query_map(params![status, limit], row_to_desktop_task)?,
            None => stmt.query_map(params![limit], row_to_desktop_task)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn claim_desktop_task(
        &self,
        id: &str,
        worker: &str,
        now: i64,
    ) -> anyhow::Result<Option<DesktopTask>> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE desktop_tasks SET status = 'running', claimed_by = ?2, last_event = ?3, updated_at = ?4 WHERE id = ?1 AND status = 'pending'",
            params![id, worker, format!("claimed by {}", worker), now],
        )?;
        drop(conn);
        if changed == 1 {
            let conn = self.conn.lock().unwrap();
            Self::insert_desktop_task_event_locked(
                &conn,
                id,
                "running",
                Some(worker),
                Some(&format!("claimed by {}", worker)),
                None,
                now,
            )?;
            drop(conn);
            self.get_desktop_task(id)
        } else {
            Ok(None)
        }
    }

    pub fn claim_next_desktop_task(
        &self,
        worker: &str,
        now: i64,
    ) -> anyhow::Result<Option<DesktopTask>> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE desktop_tasks
             SET status = 'running', claimed_by = ?1, last_event = ?2, updated_at = ?3
             WHERE id = (
                 SELECT id FROM desktop_tasks
                 WHERE status = 'pending'
                 ORDER BY priority DESC, created_at ASC
                 LIMIT 1
             )",
            params![worker, format!("claimed by {}", worker), now],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT id, title, instructions, status, priority, claimed_by, last_event, screenshot_url, created_at, updated_at
             FROM desktop_tasks
             WHERE status = 'running' AND claimed_by = ?1 AND updated_at = ?2
             ORDER BY priority DESC, created_at ASC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![worker, now], row_to_desktop_task)?;
        let task = match rows.next() {
            Some(row) => Some(row?),
            None => None,
        };
        if let Some(task) = task.as_ref() {
            Self::insert_desktop_task_event_locked(
                &conn,
                &task.id,
                "running",
                Some(worker),
                Some(&format!("claimed by {}", worker)),
                None,
                now,
            )?;
        }
        Ok(task)
    }

    pub fn update_desktop_task_event(
        &self,
        id: &str,
        status: Option<&str>,
        worker: Option<&str>,
        message: Option<&str>,
        screenshot_url: Option<&str>,
        now: i64,
    ) -> anyhow::Result<Option<DesktopTask>> {
        let existing = match self.get_desktop_task(id)? {
            Some(task) => task,
            None => return Ok(None),
        };
        let next_status = status.unwrap_or(&existing.status);
        let next_worker = worker.or(existing.claimed_by.as_deref());
        let next_event = message.or(existing.last_event.as_deref());
        let next_screenshot = screenshot_url.or(existing.screenshot_url.as_deref());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE desktop_tasks SET status = ?2, claimed_by = ?3, last_event = ?4, screenshot_url = ?5, updated_at = ?6 WHERE id = ?1",
            params![id, next_status, next_worker, next_event, next_screenshot, now],
        )?;
        Self::insert_desktop_task_event_locked(
            &conn,
            id,
            next_status,
            next_worker,
            next_event,
            next_screenshot,
            now,
        )?;
        drop(conn);
        self.get_desktop_task(id)
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
