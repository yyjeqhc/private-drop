//! Root adapter keeps SQLite out of the authoritative registry crate.
use std::sync::Arc;
use webcodex_runner_registry::{JobReceiptStore, RetainedJobReceipt, RunnerRegistry};

struct SqliteJobReceiptStore(Arc<crate::Database>);

impl std::fmt::Debug for SqliteJobReceiptStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SqliteJobReceiptStore")
    }
}

impl JobReceiptStore for SqliteJobReceiptStore {
    fn upsert(&self, receipt: &RetainedJobReceipt) -> Result<(), String> {
        self.0
            .upsert_job_receipt(receipt, chrono::Utc::now().timestamp())
            .map_err(|_| "Job receipt write failed".to_string())
    }
    fn load(&self, now: i64) -> Result<Vec<RetainedJobReceipt>, String> {
        self.0
            .load_job_receipts(now)
            .map_err(|_| "Job receipt read failed".to_string())
    }
    fn prune(&self, now: i64) -> Result<(), String> {
        self.0
            .prune_job_receipts(now)
            .map(|_| ())
            .map_err(|_| "Job receipt prune failed".to_string())
    }
}

/// Used before production starts accepting any Runner or tool traffic.
pub(crate) async fn production_registry(db: Arc<crate::Database>) -> RunnerRegistry {
    RunnerRegistry::with_job_receipt_store(
        crate::runner_http::tool_request_trace_telemetry(),
        Arc::new(SqliteJobReceiptStore(db)),
    )
    .await
}
