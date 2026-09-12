use crate::Database;
use rusqlite::{params, Connection};
use webcodex_core::runner_job_receipt::{
    RetainedJobReceipt, RunnerAccessGroup, JOB_RECEIPT_PAYLOAD_MAX_BYTES,
};
use webcodex_core::runner_protocol::JOB_INVENTORY_MAX_TERMINAL_JOBS;

impl Database {
    /// Insert immutable terminal evidence. Replay cannot replace either the
    /// verdict, historical partition or deadline, including concurrent writers.
    pub fn upsert_job_receipt(&self, receipt: &RetainedJobReceipt, now: i64) -> anyhow::Result<()> {
        receipt.validate(now).map_err(anyhow::Error::msg)?;
        let payload = serde_json::to_string(&receipt.snapshot)?;
        anyhow::ensure!(
            payload.len() <= JOB_RECEIPT_PAYLOAD_MAX_BYTES,
            "oversized Job receipt"
        );
        let (auth_kind, auth_partition) = match &receipt.auth_group {
            Some(RunnerAccessGroup::SharedKey(group)) => ("shared_key", Some(group.as_str())),
            Some(RunnerAccessGroup::ProjectGrant(group)) => ("project_grant", Some(group.as_str())),
            Some(RunnerAccessGroup::OpenAnonymous) => ("open_anonymous", None),
            None if receipt.owner_at_admission.is_some() => ("managed_owner", None),
            None => ("managed_unowned", None),
        };
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        prune_expired(&tx, now)?;
        tx.execute("INSERT INTO wc_job_receipts (job_id, client_id, runner_instance_id, auth_kind, auth_partition, owner_at_admission, kind, snapshot, terminal_observed_at, expires_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(job_id) DO NOTHING",
            params![receipt.snapshot.job_id, receipt.client_id, receipt.runner_instance_id, auth_kind, auth_partition, receipt.owner_at_admission, receipt.kind, payload, receipt.terminal_observed_at, receipt.expires_at])?;
        // Bound history by logical Runner, across process replacements and auth
        // partitions. Oldest-first, with a stable tie break for same-second jobs.
        tx.execute(
            "DELETE FROM wc_job_receipts WHERE job_id IN (
            SELECT job_id FROM wc_job_receipts WHERE client_id = ?1
            ORDER BY terminal_observed_at DESC, job_id DESC LIMIT -1 OFFSET ?2)",
            params![receipt.client_id, JOB_INVENTORY_MAX_TERMINAL_JOBS as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn prune_job_receipts(&self, now: i64) -> anyhow::Result<usize> {
        Ok(prune_expired(&self.conn.lock().unwrap(), now)?)
    }

    pub fn load_job_receipts(&self, now: i64) -> anyhow::Result<Vec<RetainedJobReceipt>> {
        let conn = self.conn.lock().unwrap();
        prune_expired(&conn, now)?;
        // Also repair excessive history from an older/manual database. Each
        // payload is size-checked in SQLite before being materialized in Rust.
        conn.execute(
            "DELETE FROM wc_job_receipts WHERE job_id IN (
            SELECT job_id FROM (SELECT job_id, ROW_NUMBER() OVER (
                PARTITION BY client_id ORDER BY terminal_observed_at DESC, job_id DESC) AS n
                FROM wc_job_receipts) WHERE n > ?1)",
            [JOB_INVENTORY_MAX_TERMINAL_JOBS as i64],
        )?;
        let mut stmt = conn.prepare("SELECT job_id, client_id, runner_instance_id, auth_kind, auth_partition, owner_at_admission, kind,
            CASE WHEN length(CAST(snapshot AS BLOB)) <= ?1 THEN snapshot ELSE NULL END,
            terminal_observed_at, expires_at,
            length(CAST(job_id AS BLOB)) <= 128 AND length(CAST(client_id AS BLOB)) <= 128
            AND length(CAST(runner_instance_id AS BLOB)) <= 128 AND length(CAST(auth_kind AS BLOB)) <= 32
            AND (auth_partition IS NULL OR length(CAST(auth_partition AS BLOB)) <= 256)
            AND (owner_at_admission IS NULL OR length(CAST(owner_at_admission AS BLOB)) <= 256)
            AND length(CAST(kind AS BLOB)) <= 128
            FROM wc_job_receipts ORDER BY client_id, terminal_observed_at, job_id")?;
        let rows = stmt.query_map([JOB_RECEIPT_PAYLOAD_MAX_BYTES as i64], |row| {
            if !row.get::<_, bool>(10)? {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?;
        let mut receipts = Vec::new();
        let mut skipped = 0;
        for row in rows {
            let receipt = (|| -> Option<RetainedJobReceipt> {
                let (
                    job_id,
                    client_id,
                    runner_instance_id,
                    auth_kind,
                    partition,
                    owner_at_admission,
                    kind,
                    payload,
                    terminal_observed_at,
                    expires_at,
                ) = row.ok()?;
                let auth_group = match (auth_kind.as_str(), partition) {
                    ("shared_key", Some(group)) => Some(RunnerAccessGroup::SharedKey(group)),
                    ("project_grant", Some(group)) => Some(RunnerAccessGroup::ProjectGrant(group)),
                    ("open_anonymous", None) => Some(RunnerAccessGroup::OpenAnonymous),
                    ("managed_owner", None) if owner_at_admission.is_some() => None,
                    ("managed_unowned", None) if owner_at_admission.is_none() => None,
                    _ => return None,
                };
                let snapshot = serde_json::from_str(&payload?).ok()?;
                let receipt = RetainedJobReceipt {
                    client_id,
                    runner_instance_id,
                    auth_group,
                    owner_at_admission,
                    kind,
                    snapshot,
                    terminal_observed_at,
                    expires_at,
                };
                if receipt.snapshot.job_id != job_id || receipt.validate(now).is_err() {
                    return None;
                }
                Some(receipt)
            })();
            if let Some(receipt) = receipt {
                receipts.push(receipt);
            } else {
                skipped += 1;
            }
        }
        if skipped > 0 {
            tracing::warn!(count = skipped, "skipping malformed terminal Job receipts");
        }
        Ok(receipts)
    }
}

fn prune_expired(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM wc_job_receipts WHERE expires_at <= ?1", [now])
}
