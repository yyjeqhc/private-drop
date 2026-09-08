use crate::models::{
    WindowActivityEventRecord, WindowActivitySummaryRecord, WindowSessionLinkSummaryRecord,
    WindowWorkflowAffinityRecord, WindowWorkflowLinkRecord, WindowWorkflowSessionSummaryRecord,
};
use crate::Database;
use rusqlite::{params, Connection};
use std::collections::BTreeSet;

pub const MAX_WINDOW_ACTIVITY_LIMIT: usize = 500;
pub const MAX_WINDOW_LINK_LIMIT: usize = 100;

fn bounded_limit(limit: usize, max: usize) -> i64 {
    limit.clamp(1, max) as i64
}

fn principal_predicate<'a>(
    principal: Option<(&'a str, &'a str)>,
) -> (&'static str, Option<&'a str>, Option<&'a str>) {
    match principal {
        Some((kind, id)) => (
            "AND e.principal_correlation_kind = ?2 AND e.principal_correlation_id = ?3",
            Some(kind),
            Some(id),
        ),
        None => ("", None, None),
    }
}

impl Database {
    /// Bounded newest-first Window summaries. `principal=None` is reserved for
    /// callers that already hold canonical runtime-wide administrator/bootstrap
    /// authority; knowing a Window key never bypasses this filter.
    pub fn list_window_activity_summaries(
        &self,
        principal: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<WindowActivitySummaryRecord>> {
        let conn = self.conn.lock().unwrap();
        let (principal_sql, kind, id) = principal_predicate(principal);
        let sql = format!(
            "SELECT e.client_window_key,
                    MAX(e.client_window_source),
                    MAX(e.window_ended_at_ms),
                    MAX(CASE WHEN e.action_name = 'toolsCall' THEN e.window_ended_at_ms END),
                    MAX(CASE WHEN e.window_meaningful = 1 THEN e.window_ended_at_ms END),
                    COUNT(DISTINCT l.workflow_session_id),
                    COUNT(DISTINCT CASE WHEN e.recorder_gap_session_id IS NOT NULL THEN e.event_id END)
             FROM action_events e
             LEFT JOIN action_event_workflow_links l ON l.event_id = e.event_id
             WHERE e.client_window_key IS NOT NULL
               AND e.window_ended_at_ms IS NOT NULL
               {principal_sql}
             GROUP BY e.client_window_key
             ORDER BY MAX(e.window_ended_at_ms) DESC, e.client_window_key ASC
             LIMIT ?1"
        );
        let limit = bounded_limit(limit, MAX_WINDOW_LINK_LIMIT);
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = match (kind, id) {
            (Some(kind), Some(id)) => stmt.query(params![limit, kind, id])?,
            _ => stmt.query(params![limit])?,
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(WindowActivitySummaryRecord {
                client_window_key: row.get(0)?,
                client_window_source: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                last_seen_at_ms: row.get(2)?,
                last_tool_call_at_ms: row.get(3)?,
                last_meaningful_activity_at_ms: row.get(4)?,
                linked_session_count: usize::try_from(row.get::<_, i64>(5)?).unwrap_or(usize::MAX),
                recorder_gap_count: usize::try_from(row.get::<_, i64>(6)?).unwrap_or(usize::MAX),
            });
        }
        Ok(out)
    }

    pub fn count_window_activity_summaries(
        &self,
        principal: Option<(&str, &str)>,
    ) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count = match principal {
            Some((kind, id)) => conn.query_row(
                "SELECT COUNT(DISTINCT client_window_key)
                 FROM action_events
                 WHERE client_window_key IS NOT NULL
                   AND window_ended_at_ms IS NOT NULL
                   AND principal_correlation_kind = ?1
                   AND principal_correlation_id = ?2",
                params![kind, id],
                |row| row.get::<_, i64>(0),
            )?,
            None => conn.query_row(
                "SELECT COUNT(DISTINCT client_window_key)
                 FROM action_events
                 WHERE client_window_key IS NOT NULL
                   AND window_ended_at_ms IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )?,
        };
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub fn get_window_activity_summary(
        &self,
        window_key: &str,
        principal: Option<(&str, &str)>,
    ) -> anyhow::Result<Option<WindowActivitySummaryRecord>> {
        let conn = self.conn.lock().unwrap();
        let row = match principal {
            Some((kind, id)) => conn
                .query_row(
                    "SELECT e.client_window_key, MAX(e.client_window_source),
                            MAX(e.window_ended_at_ms),
                            MAX(CASE WHEN e.action_name = 'toolsCall' THEN e.window_ended_at_ms END),
                            MAX(CASE WHEN e.window_meaningful = 1 THEN e.window_ended_at_ms END),
                            COUNT(DISTINCT l.workflow_session_id),
                            COUNT(DISTINCT CASE WHEN e.recorder_gap_session_id IS NOT NULL THEN e.event_id END)
                     FROM action_events e
                     LEFT JOIN action_event_workflow_links l ON l.event_id = e.event_id
                     WHERE e.client_window_key = ?1
                       AND e.window_ended_at_ms IS NOT NULL
                       AND e.principal_correlation_kind = ?2
                       AND e.principal_correlation_id = ?3
                     GROUP BY e.client_window_key",
                    params![window_key, kind, id],
                    window_summary_from_row,
                )
                .optional()?,
            None => conn
                .query_row(
                    "SELECT e.client_window_key, MAX(e.client_window_source),
                            MAX(e.window_ended_at_ms),
                            MAX(CASE WHEN e.action_name = 'toolsCall' THEN e.window_ended_at_ms END),
                            MAX(CASE WHEN e.window_meaningful = 1 THEN e.window_ended_at_ms END),
                            COUNT(DISTINCT l.workflow_session_id),
                            COUNT(DISTINCT CASE WHEN e.recorder_gap_session_id IS NOT NULL THEN e.event_id END)
                     FROM action_events e
                     LEFT JOIN action_event_workflow_links l ON l.event_id = e.event_id
                     WHERE e.client_window_key = ?1
                       AND e.window_ended_at_ms IS NOT NULL
                     GROUP BY e.client_window_key",
                    params![window_key],
                    window_summary_from_row,
                )
                .optional()?,
        };
        Ok(row)
    }

    pub fn list_window_activity_events(
        &self,
        window_key: &str,
        principal: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<WindowActivityEventRecord>> {
        let conn = self.conn.lock().unwrap();
        let (principal_sql, kind, id) = principal_predicate(principal);
        let sql = format!(
            "SELECT e.event_id, e.client_window_key, e.client_window_source,
                    e.server_trace_id, e.window_started_at_ms, e.window_ended_at_ms,
                    e.duration_ms, e.action_name, e.operation, e.project, e.status,
                    e.window_meaningful, e.recorder_gap_session_id
             FROM action_events e
             WHERE e.client_window_key = ?1
               AND e.window_started_at_ms IS NOT NULL
               AND e.window_ended_at_ms IS NOT NULL
               {principal_sql}
             ORDER BY e.window_started_at_ms DESC, e.event_id DESC
             LIMIT ?4"
        );
        let limit = bounded_limit(limit, MAX_WINDOW_ACTIVITY_LIMIT);
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = match (kind, id) {
            (Some(kind), Some(id)) => stmt.query(params![window_key, kind, id, limit])?,
            _ => {
                let sql = sql.replace("LIMIT ?4", "LIMIT ?2");
                drop(stmt);
                let mut stmt = conn.prepare(&sql)?;
                let records = collect_window_events(&conn, &mut stmt, params![window_key, limit])?;
                return Ok(records);
            }
        };
        collect_window_event_rows(&conn, &mut rows)
    }

    /// Latest authoritative Window/Session relation for diagnostic continuity.
    /// `work_on_project` wins a same-millisecond tie because it is the explicit
    /// create/resume/switch boundary; this remains a non-authoritative hint.
    pub fn latest_window_workflow_affinity(
        &self,
        window_key: &str,
        principal_kind: &str,
        principal_id: &str,
        project: &str,
    ) -> anyhow::Result<Option<WindowWorkflowAffinityRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT l.workflow_session_id, l.project, l.workflow_session_relation, l.linked_at_ms
             FROM action_event_workflow_links l
             JOIN action_events e ON e.event_id = l.event_id
             WHERE e.client_window_key = ?1
               AND e.principal_correlation_kind = ?2
               AND e.principal_correlation_id = ?3
               AND l.project = ?4
             ORDER BY l.linked_at_ms DESC,
                      CASE l.workflow_session_relation WHEN 'work_on_project' THEN 0 ELSE 1 END ASC,
                      e.event_id DESC
             LIMIT 1",
            params![window_key, principal_kind, principal_id, project],
            |row| {
                Ok(WindowWorkflowAffinityRecord {
                    workflow_session_id: row.get(0)?,
                    project: row.get(1)?,
                    relation: row.get(2)?,
                    linked_at_ms: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_window_workflow_sessions(
        &self,
        window_key: &str,
        principal: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<WindowWorkflowSessionSummaryRecord>> {
        let conn = self.conn.lock().unwrap();
        let (principal_sql, kind, id) = principal_predicate(principal);
        let sql = format!(
            "SELECT l.workflow_session_id, MAX(l.project), MIN(l.linked_at_ms),
                    MAX(l.linked_at_ms), COUNT(*),
                    GROUP_CONCAT(DISTINCT l.workflow_session_relation)
             FROM action_event_workflow_links l
             JOIN action_events e ON e.event_id = l.event_id
             WHERE e.client_window_key = ?1 {principal_sql}
             GROUP BY l.workflow_session_id
             ORDER BY MAX(l.linked_at_ms) DESC, l.workflow_session_id ASC
             LIMIT ?4"
        );
        let limit = bounded_limit(limit, MAX_WINDOW_LINK_LIMIT);
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = match (kind, id) {
            (Some(kind), Some(id)) => stmt.query(params![window_key, kind, id, limit])?,
            _ => {
                let sql = sql.replace("LIMIT ?4", "LIMIT ?2");
                drop(stmt);
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query(params![window_key, limit])?;
                return collect_window_session_rows(&mut rows);
            }
        };
        collect_window_session_rows(&mut rows)
    }

    pub fn list_session_linked_windows(
        &self,
        workflow_session_id: &str,
        principal: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<WindowSessionLinkSummaryRecord>> {
        let conn = self.conn.lock().unwrap();
        let limit = bounded_limit(limit, MAX_WINDOW_LINK_LIMIT);
        match principal {
            Some((kind, id)) => {
                let mut stmt = conn.prepare(
                    "WITH linked AS (
                        SELECT e.client_window_key AS window_key,
                               MAX(e.client_window_source) AS source,
                               MIN(l.linked_at_ms) AS first_linked,
                               MAX(l.linked_at_ms) AS last_linked,
                               COUNT(*) AS relation_count,
                               GROUP_CONCAT(DISTINCT l.workflow_session_relation) AS relations
                        FROM action_event_workflow_links l
                        JOIN action_events e ON e.event_id = l.event_id
                        WHERE l.workflow_session_id = ?1
                          AND e.client_window_key IS NOT NULL
                          AND e.principal_correlation_kind = ?2
                          AND e.principal_correlation_id = ?3
                        GROUP BY e.client_window_key
                    )
                    SELECT linked.window_key, linked.source, linked.first_linked,
                           linked.last_linked, linked.relation_count, linked.relations,
                           (SELECT MAX(a.window_ended_at_ms)
                            FROM action_events a
                            WHERE a.client_window_key = linked.window_key
                              AND a.principal_correlation_kind = ?2
                              AND a.principal_correlation_id = ?3),
                           (SELECT MAX(CASE WHEN a.window_meaningful = 1 THEN a.window_ended_at_ms END)
                            FROM action_events a
                            WHERE a.client_window_key = linked.window_key
                              AND a.principal_correlation_kind = ?2
                              AND a.principal_correlation_id = ?3),
                           (SELECT COUNT(*)
                            FROM action_events gap
                            WHERE gap.client_window_key = linked.window_key
                              AND gap.recorder_gap_session_id = ?1
                              AND gap.principal_correlation_kind = ?2
                              AND gap.principal_correlation_id = ?3)
                    FROM linked
                    ORDER BY linked.last_linked DESC, linked.window_key ASC
                    LIMIT ?4",
                )?;
                let mut rows = stmt.query(params![workflow_session_id, kind, id, limit])?;
                collect_session_window_rows(&mut rows)
            }
            None => {
                let mut stmt = conn.prepare(
                    "WITH linked AS (
                        SELECT e.client_window_key AS window_key,
                               MAX(e.client_window_source) AS source,
                               MIN(l.linked_at_ms) AS first_linked,
                               MAX(l.linked_at_ms) AS last_linked,
                               COUNT(*) AS relation_count,
                               GROUP_CONCAT(DISTINCT l.workflow_session_relation) AS relations
                        FROM action_event_workflow_links l
                        JOIN action_events e ON e.event_id = l.event_id
                        WHERE l.workflow_session_id = ?1
                          AND e.client_window_key IS NOT NULL
                        GROUP BY e.client_window_key
                    )
                    SELECT linked.window_key, linked.source, linked.first_linked,
                           linked.last_linked, linked.relation_count, linked.relations,
                           (SELECT MAX(a.window_ended_at_ms)
                            FROM action_events a
                            WHERE a.client_window_key = linked.window_key),
                           (SELECT MAX(CASE WHEN a.window_meaningful = 1 THEN a.window_ended_at_ms END)
                            FROM action_events a
                            WHERE a.client_window_key = linked.window_key),
                           (SELECT COUNT(*)
                            FROM action_events gap
                            WHERE gap.client_window_key = linked.window_key
                              AND gap.recorder_gap_session_id = ?1)
                    FROM linked
                    ORDER BY linked.last_linked DESC, linked.window_key ASC
                    LIMIT ?2",
                )?;
                let mut rows = stmt.query(params![workflow_session_id, limit])?;
                collect_session_window_rows(&mut rows)
            }
        }
    }
}

use rusqlite::OptionalExtension;

fn window_summary_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<WindowActivitySummaryRecord> {
    Ok(WindowActivitySummaryRecord {
        client_window_key: row.get(0)?,
        client_window_source: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        last_seen_at_ms: row.get(2)?,
        last_tool_call_at_ms: row.get(3)?,
        last_meaningful_activity_at_ms: row.get(4)?,
        linked_session_count: usize::try_from(row.get::<_, i64>(5)?).unwrap_or(usize::MAX),
        recorder_gap_count: usize::try_from(row.get::<_, i64>(6)?).unwrap_or(usize::MAX),
    })
}

fn collect_window_events<P: rusqlite::Params>(
    conn: &Connection,
    stmt: &mut rusqlite::Statement<'_>,
    params: P,
) -> anyhow::Result<Vec<WindowActivityEventRecord>> {
    let mut rows = stmt.query(params)?;
    collect_window_event_rows(conn, &mut rows)
}

fn collect_window_event_rows(
    conn: &Connection,
    rows: &mut rusqlite::Rows<'_>,
) -> anyhow::Result<Vec<WindowActivityEventRecord>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let event_id: String = row.get(0)?;
        out.push(WindowActivityEventRecord {
            event_id: event_id.clone(),
            client_window_key: row.get(1)?,
            client_window_source: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            server_trace_id: row.get(3)?,
            started_at_ms: row.get(4)?,
            ended_at_ms: row.get(5)?,
            duration_ms: row.get(6)?,
            action_name: row.get(7)?,
            operation: row.get(8)?,
            project: row.get(9)?,
            status: row.get(10)?,
            meaningful: row.get(11)?,
            recorder_gap_session_id: row.get(12)?,
            workflow_links: workflow_links_for_event(conn, &event_id)?,
        });
    }
    Ok(out)
}

fn workflow_links_for_event(
    conn: &Connection,
    event_id: &str,
) -> anyhow::Result<Vec<WindowWorkflowLinkRecord>> {
    let mut stmt = conn.prepare(
        "SELECT workflow_session_id, project, workflow_session_relation, linked_at_ms
         FROM action_event_workflow_links
         WHERE event_id = ?1
         ORDER BY linked_at_ms ASC, workflow_session_relation ASC, workflow_session_id ASC",
    )?;
    let rows = stmt.query_map(params![event_id], |row| {
        Ok(WindowWorkflowLinkRecord {
            workflow_session_id: row.get(0)?,
            project: row.get(1)?,
            relation: row.get(2)?,
            linked_at_ms: row.get(3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn relation_set(csv: Option<String>) -> Vec<String> {
    csv.unwrap_or_default()
        .split(',')
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn collect_window_session_rows(
    rows: &mut rusqlite::Rows<'_>,
) -> anyhow::Result<Vec<WindowWorkflowSessionSummaryRecord>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(WindowWorkflowSessionSummaryRecord {
            workflow_session_id: row.get(0)?,
            project: row.get(1)?,
            first_linked_at_ms: row.get(2)?,
            last_linked_at_ms: row.get(3)?,
            relation_count: usize::try_from(row.get::<_, i64>(4)?).unwrap_or(usize::MAX),
            relations: relation_set(row.get(5)?),
        });
    }
    Ok(out)
}

fn collect_session_window_rows(
    rows: &mut rusqlite::Rows<'_>,
) -> anyhow::Result<Vec<WindowSessionLinkSummaryRecord>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(WindowSessionLinkSummaryRecord {
            client_window_key: row.get(0)?,
            client_window_source: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            first_linked_at_ms: row.get(2)?,
            last_linked_at_ms: row.get(3)?,
            relation_count: usize::try_from(row.get::<_, i64>(4)?).unwrap_or(usize::MAX),
            relations: relation_set(row.get(5)?),
            last_seen_at_ms: row.get(6)?,
            last_meaningful_activity_at_ms: row.get(7)?,
            recorder_gap_count: usize::try_from(row.get::<_, i64>(8)?).unwrap_or(usize::MAX),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ActionEventRecord, ActionEventWorkflowLinkRecord, ActionSessionRecord};

    fn event(id: &str, window: &str, principal: &str, project: &str, at: i64) -> ActionEventRecord {
        ActionEventRecord {
            event_id: id.to_string(),
            session_id: "audit-session".to_string(),
            started_at: at / 1000,
            ended_at: at / 1000,
            duration_ms: 1,
            endpoint: "/mcp".to_string(),
            operation: Some("read_files".to_string()),
            action_name: "toolsCall".to_string(),
            project: Some(project.to_string()),
            principal_kind: None,
            principal_user_id: None,
            oauth_client_id: None,
            status: "success".to_string(),
            http_status: Some(200),
            error_summary: None,
            warning_summary: None,
            changed_files_json: "[]".to_string(),
            ids_json: "{}".to_string(),
            summary_json: "{}".to_string(),
            request_bytes: None,
            response_bytes: None,
            client_window_key: Some(window.to_string()),
            client_window_source: Some("openai-session".to_string()),
            server_trace_id: Some(format!("trace-{id}")),
            principal_correlation_kind: Some("username".to_string()),
            principal_correlation_id: Some(principal.to_string()),
            window_started_at_ms: Some(at),
            window_ended_at_ms: Some(at + 1),
            window_meaningful: true,
            recorder_gap_session_id: None,
        }
    }

    fn seed_session(db: &Database) {
        db.insert_action_session(&ActionSessionRecord {
            session_id: "audit-session".to_string(),
            title: None,
            note: None,
            status: "open".to_string(),
            created_at: 1,
            updated_at: 1,
            closed_at: None,
            first_event_at: None,
            last_event_at: None,
            total_actions: 0,
            success_count: 0,
            failed_count: 0,
            timeout_or_unknown_count: 0,
            warning_count: 0,
            total_duration_ms: 0,
            changed_files_count: 0,
            job_ids_count: 0,
        })
        .unwrap();
    }

    fn append(db: &Database, event: ActionEventRecord, links: &[(&str, &str)]) {
        let records = links
            .iter()
            .map(|(session, relation)| ActionEventWorkflowLinkRecord {
                event_id: event.event_id.clone(),
                workflow_session_id: (*session).to_string(),
                workflow_session_relation: (*relation).to_string(),
                project: event.project.clone(),
                linked_at_ms: event.window_ended_at_ms.unwrap(),
            })
            .collect::<Vec<_>>();
        db.append_action_event_and_update_session(&event, &records, 1, 0, 0, 0, 1, 0, 0)
            .unwrap();
    }

    #[test]
    fn window_session_queries_are_many_to_many_and_principal_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("window-activity.db")).unwrap();
        seed_session(&db);
        append(
            &db,
            event("a1", "w1", "alice", "agent:r:p", 1_000),
            &[("s1", "recording")],
        );
        append(
            &db,
            event("a2", "w1", "alice", "agent:r:p", 2_000),
            &[("s2", "work_on_project")],
        );
        append(
            &db,
            event("a3", "w2", "alice", "agent:r:p", 3_000),
            &[("s2", "recording")],
        );
        let mut gap = event("a-gap", "w1", "alice", "agent:r:p", 3_500);
        gap.recorder_gap_session_id = Some("s2".to_string());
        append(&db, gap, &[]);
        append(
            &db,
            event("b1", "wb", "bob", "agent:b:p", 4_000),
            &[("sb", "recording")],
        );

        let w1 = db
            .list_window_workflow_sessions("w1", Some(("username", "alice")), 20)
            .unwrap();
        assert_eq!(w1.len(), 2);
        assert!(w1.iter().any(|link| link.workflow_session_id == "s1"));
        assert!(w1.iter().any(|link| link.workflow_session_id == "s2"));
        let s2 = db
            .list_session_linked_windows("s2", Some(("username", "alice")), 20)
            .unwrap();
        assert_eq!(s2.len(), 2);
        assert_eq!(
            s2.iter()
                .find(|link| link.client_window_key == "w1")
                .map(|link| link.recorder_gap_count),
            Some(1),
            "unrecorded Window activity must remain separate from the Session link event yet still surface as a recorder gap"
        );
        assert!(db
            .list_window_activity_events("wb", Some(("username", "alice")), 20)
            .unwrap()
            .is_empty());
        let affinity = db
            .latest_window_workflow_affinity("w1", "username", "alice", "agent:r:p")
            .unwrap()
            .unwrap();
        assert_eq!(affinity.workflow_session_id, "s2");
        assert_eq!(affinity.relation, "work_on_project");
    }
}
