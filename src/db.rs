use crate::{Channel, CodexGoalRecord, CommandAuditRecord, Message, MessageKind};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::Mutex;

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
            CREATE INDEX IF NOT EXISTS idx_codex_goals_created_at ON codex_goals(created_at DESC);",
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
