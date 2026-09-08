use super::Database;
use crate::models::{ActionEventRecord, ActionEventWorkflowLinkRecord, ActionSessionRecord};
use rusqlite::{params, Connection};

impl Database {
    pub fn insert_action_session(&self, record: &ActionSessionRecord) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO action_sessions (
                session_id, title, note, status, created_at, updated_at, closed_at,
                first_event_at, last_event_at, total_actions, success_count, failed_count,
                timeout_or_unknown_count, warning_count, total_duration_ms,
                changed_files_count, job_ids_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                record.session_id,
                record.title,
                record.note,
                record.status,
                record.created_at,
                record.updated_at,
                record.closed_at,
                record.first_event_at,
                record.last_event_at,
                record.total_actions,
                record.success_count,
                record.failed_count,
                record.timeout_or_unknown_count,
                record.warning_count,
                record.total_duration_ms,
                record.changed_files_count,
                record.job_ids_count,
            ],
        )?;
        Ok(())
    }

    pub fn get_action_session(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<ActionSessionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, title, note, status, created_at, updated_at, closed_at,
                    first_event_at, last_event_at, total_actions, success_count, failed_count,
                    timeout_or_unknown_count, warning_count, total_duration_ms,
                    changed_files_count, job_ids_count
             FROM action_sessions WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![session_id], row_to_action_session)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn list_action_sessions(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<ActionSessionRecord>> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 200) as i64;
        let sql = match status {
            Some(_) => {
                "SELECT session_id, title, note, status, created_at, updated_at, closed_at,
                        first_event_at, last_event_at, total_actions, success_count,
                        failed_count, timeout_or_unknown_count, warning_count,
                        total_duration_ms, changed_files_count, job_ids_count
                 FROM action_sessions
                 WHERE status = ?1
                 ORDER BY COALESCE(last_event_at, created_at) DESC
                 LIMIT ?2"
            }
            None => {
                "SELECT session_id, title, note, status, created_at, updated_at, closed_at,
                        first_event_at, last_event_at, total_actions, success_count,
                        failed_count, timeout_or_unknown_count, warning_count,
                        total_duration_ms, changed_files_count, job_ids_count
                 FROM action_sessions
                 ORDER BY COALESCE(last_event_at, created_at) DESC
                 LIMIT ?1"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = match status {
            Some(status) => stmt.query_map(params![status, limit], row_to_action_session)?,
            None => stmt.query_map(params![limit], row_to_action_session)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn find_recent_open_action_session(
        &self,
        min_last_event_at: i64,
    ) -> anyhow::Result<Option<ActionSessionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, title, note, status, created_at, updated_at, closed_at,
                    first_event_at, last_event_at, total_actions, success_count, failed_count,
                    timeout_or_unknown_count, warning_count, total_duration_ms,
                    changed_files_count, job_ids_count
             FROM action_sessions
             WHERE status = 'open' AND COALESCE(last_event_at, created_at) >= ?1
             ORDER BY COALESCE(last_event_at, created_at) DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![min_last_event_at], row_to_action_session)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn update_action_session_metadata(
        &self,
        session_id: &str,
        title: Option<&str>,
        note: Option<&str>,
        updated_at: i64,
    ) -> anyhow::Result<Option<ActionSessionRecord>> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE action_sessions
             SET title = COALESCE(?2, title),
                 note = COALESCE(?3, note),
                 updated_at = ?4
             WHERE session_id = ?1",
            params![session_id, title, note, updated_at],
        )?;
        drop(conn);
        if changed == 1 {
            self.get_action_session(session_id)
        } else {
            Ok(None)
        }
    }

    pub fn close_action_session(
        &self,
        session_id: &str,
        closed_at: i64,
    ) -> anyhow::Result<Option<ActionSessionRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE action_sessions
             SET status = 'closed', closed_at = ?2, updated_at = ?2
             WHERE session_id = ?1 AND status != 'closed'",
            params![session_id, closed_at],
        )?;
        drop(conn);
        self.get_action_session(session_id)
    }

    pub fn insert_action_event(&self, event: &ActionEventRecord) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        insert_action_event_on_conn(&conn, event)?;
        Ok(())
    }

    pub fn list_action_events(
        &self,
        session_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ActionEventRecord>> {
        let conn = self.conn.lock().unwrap();
        list_action_events_on_conn(&conn, session_id, limit)
    }

    /// Return the total event count and bounded newest rows from one SQLite read
    /// transaction snapshot so coverage metadata cannot race a concurrent append.
    pub fn list_action_events_with_count(
        &self,
        session_id: &str,
        limit: usize,
    ) -> anyhow::Result<(usize, Vec<ActionEventRecord>)> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let count = count_action_events_on_conn(&tx, session_id)?;
        let events = list_action_events_on_conn(&tx, session_id, limit)?;
        tx.commit()?;
        Ok((count, events))
    }

    pub fn append_action_event_and_update_session(
        &self,
        event: &ActionEventRecord,
        workflow_links: &[ActionEventWorkflowLinkRecord],
        success_inc: i64,
        failed_inc: i64,
        timeout_inc: i64,
        warning_inc: i64,
        duration_inc: i64,
        changed_files_count: i64,
        job_ids_count: i64,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        insert_action_event_on_conn(&tx, event)?;
        for link in workflow_links {
            tx.execute(
                "INSERT OR IGNORE INTO action_event_workflow_links (
                    event_id, workflow_session_id, workflow_session_relation, project, linked_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    event.event_id,
                    link.workflow_session_id,
                    link.workflow_session_relation,
                    link.project,
                    link.linked_at_ms,
                ],
            )?;
        }
        tx.execute(
            "UPDATE action_sessions
             SET updated_at = ?2,
                 first_event_at = COALESCE(first_event_at, ?3),
                 last_event_at = ?4,
                 total_actions = total_actions + 1,
                 success_count = success_count + ?5,
                 failed_count = failed_count + ?6,
                 timeout_or_unknown_count = timeout_or_unknown_count + ?7,
                 warning_count = warning_count + ?8,
                 total_duration_ms = total_duration_ms + ?9,
                 changed_files_count = changed_files_count + ?10,
                 job_ids_count = job_ids_count + ?11
             WHERE session_id = ?1",
            params![
                event.session_id,
                event.ended_at,
                event.started_at,
                event.ended_at,
                success_inc,
                failed_inc,
                timeout_inc,
                warning_inc,
                duration_inc,
                changed_files_count,
                job_ids_count,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn insert_action_event_on_conn(conn: &Connection, event: &ActionEventRecord) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO action_events (
            event_id, session_id, started_at, ended_at, duration_ms, endpoint,
            operation, action_name, project, principal_kind, principal_user_id,
            oauth_client_id, status, http_status, error_summary, warning_summary,
            changed_files_json, ids_json, summary_json, request_bytes, response_bytes,
            client_window_key, client_window_source, server_trace_id,
            principal_correlation_kind, principal_correlation_id,
            window_started_at_ms, window_ended_at_ms, window_meaningful,
            recorder_gap_session_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30)",
        params![
            event.event_id,
            event.session_id,
            event.started_at,
            event.ended_at,
            event.duration_ms,
            event.endpoint,
            event.operation,
            event.action_name,
            event.project,
            event.principal_kind,
            event.principal_user_id,
            event.oauth_client_id,
            event.status,
            event.http_status,
            event.error_summary,
            event.warning_summary,
            event.changed_files_json,
            event.ids_json,
            event.summary_json,
            event.request_bytes,
            event.response_bytes,
            event.client_window_key,
            event.client_window_source,
            event.server_trace_id,
            event.principal_correlation_kind,
            event.principal_correlation_id,
            event.window_started_at_ms,
            event.window_ended_at_ms,
            event.window_meaningful,
            event.recorder_gap_session_id,
        ],
    )?;
    Ok(())
}

fn count_action_events_on_conn(conn: &Connection, session_id: &str) -> anyhow::Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM action_events WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

fn list_action_events_on_conn(
    conn: &Connection,
    session_id: &str,
    limit: usize,
) -> anyhow::Result<Vec<ActionEventRecord>> {
    let limit = limit.clamp(1, 500) as i64;
    let mut stmt = conn.prepare(
        "SELECT event_id, session_id, started_at, ended_at, duration_ms, endpoint,
                operation, action_name, project, principal_kind, principal_user_id,
                oauth_client_id, status, http_status, error_summary, warning_summary,
                changed_files_json, ids_json, summary_json, request_bytes, response_bytes,
                client_window_key, client_window_source, server_trace_id,
                principal_correlation_kind, principal_correlation_id,
                window_started_at_ms, window_ended_at_ms, window_meaningful,
                recorder_gap_session_id
         FROM action_events
         WHERE session_id = ?1
         ORDER BY started_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![session_id, limit], row_to_action_event)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn row_to_action_session(row: &rusqlite::Row) -> rusqlite::Result<ActionSessionRecord> {
    Ok(ActionSessionRecord {
        session_id: row.get(0)?,
        title: row.get(1)?,
        note: row.get(2)?,
        status: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        closed_at: row.get(6)?,
        first_event_at: row.get(7)?,
        last_event_at: row.get(8)?,
        total_actions: row.get(9)?,
        success_count: row.get(10)?,
        failed_count: row.get(11)?,
        timeout_or_unknown_count: row.get(12)?,
        warning_count: row.get(13)?,
        total_duration_ms: row.get(14)?,
        changed_files_count: row.get(15)?,
        job_ids_count: row.get(16)?,
    })
}

fn row_to_action_event(row: &rusqlite::Row) -> rusqlite::Result<ActionEventRecord> {
    Ok(ActionEventRecord {
        event_id: row.get(0)?,
        session_id: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        duration_ms: row.get(4)?,
        endpoint: row.get(5)?,
        operation: row.get(6)?,
        action_name: row.get(7)?,
        project: row.get(8)?,
        principal_kind: row.get(9)?,
        principal_user_id: row.get(10)?,
        oauth_client_id: row.get(11)?,
        status: row.get(12)?,
        http_status: row.get(13)?,
        error_summary: row.get(14)?,
        warning_summary: row.get(15)?,
        changed_files_json: row.get(16)?,
        ids_json: row.get(17)?,
        summary_json: row.get(18)?,
        request_bytes: row.get(19)?,
        response_bytes: row.get(20)?,
        client_window_key: row.get(21)?,
        client_window_source: row.get(22)?,
        server_trace_id: row.get(23)?,
        principal_correlation_kind: row.get(24)?,
        principal_correlation_id: row.get(25)?,
        window_started_at_ms: row.get(26)?,
        window_ended_at_ms: row.get(27)?,
        window_meaningful: row.get(28)?,
        recorder_gap_session_id: row.get(29)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_event_count_and_rows_share_one_snapshot_during_concurrent_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot.db");
        let db_reader = Database::open(&path).unwrap();
        {
            let conn = db_reader.conn_for_tests();
            conn.execute(
                "INSERT INTO action_sessions (session_id, status, created_at, updated_at)
                 VALUES ('snapshot-session', 'open', 1, 1)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "WITH RECURSIVE seq(n) AS (
                    SELECT 1
                    UNION ALL
                    SELECT n + 1 FROM seq WHERE n < 200
                 )
                 INSERT INTO action_events (
                    event_id, session_id, started_at, ended_at, duration_ms, endpoint,
                    action_name, status, changed_files_json, ids_json, summary_json
                 )
                 SELECT printf('event-%03d', n), 'snapshot-session', n, n, 0, '/mcp',
                        'toolsCall', 'success', '[]', '{}', '{}'
                 FROM seq;",
            )
            .unwrap();
        }
        let db_writer = Database::open(&path).unwrap();

        let mut reader_conn = db_reader.conn_for_tests();
        let read_tx = reader_conn.transaction().unwrap();
        let available = count_action_events_on_conn(&read_tx, "snapshot-session").unwrap();
        assert_eq!(available, 200);

        db_writer
            .conn_for_tests()
            .execute(
                "INSERT INTO action_events (
                    event_id, session_id, started_at, ended_at, duration_ms, endpoint,
                    action_name, status, changed_files_json, ids_json, summary_json
                 ) VALUES (
                    'event-201', 'snapshot-session', 201, 201, 0, '/mcp',
                    'toolsCall', 'success', '[]', '{}', '{}'
                 )",
                [],
            )
            .unwrap();

        let rows = list_action_events_on_conn(&read_tx, "snapshot-session", 200).unwrap();
        assert_eq!(rows.len(), 200);
        assert!(rows.iter().all(|event| event.event_id != "event-201"));
        read_tx.commit().unwrap();

        let committed_count: i64 = db_writer
            .conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM action_events WHERE session_id = 'snapshot-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(committed_count, 201);
    }
}
