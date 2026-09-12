use super::*;
use crate::{
    JobReceiptStore, NoopRunnerRegistryTelemetry, RetainedJobReceipt, RunnerAccess,
    RunnerAccessGroup,
};
use std::sync::{Arc, Mutex, Weak};

#[derive(Debug, Default)]
struct MemoryReceipts {
    rows: Mutex<Vec<RetainedJobReceipt>>,
    fail: bool,
    registry: Mutex<Option<Weak<crate::receipts::ReceiptRegistryState>>>,
}
impl JobReceiptStore for MemoryReceipts {
    fn upsert(&self, receipt: &RetainedJobReceipt) -> Result<(), String> {
        if let Some(registry) = self
            .registry
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
        {
            assert!(
                registry.is_unlocked_for_test(),
                "storage must run after registry unlock"
            );
        }
        if self.fail {
            return Err("injected failure".into());
        }
        let mut rows = self.rows.lock().unwrap();
        if !rows
            .iter()
            .any(|row| row.snapshot.job_id == receipt.snapshot.job_id)
        {
            rows.push(receipt.clone());
        }
        Ok(())
    }
    fn load(&self, now: i64) -> Result<Vec<RetainedJobReceipt>, String> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.expires_at > now)
            .cloned()
            .collect())
    }
    fn prune(&self, now: i64) -> Result<(), String> {
        self.rows.lock().unwrap().retain(|row| row.expires_at > now);
        Ok(())
    }
}
async fn durable(store: &Arc<MemoryReceipts>) -> RunnerRegistry {
    let registry = RunnerRegistry::with_job_receipt_store(
        Arc::new(NoopRunnerRegistryTelemetry),
        store.clone(),
    )
    .await;
    *store.registry.lock().unwrap() = Some(Arc::downgrade(&registry.inner));
    registry
}
fn access(owner: Option<&str>, group: Option<RunnerAccessGroup>) -> RunnerAccess {
    RunnerAccess {
        username: owner.map(str::to_string),
        group,
        global_visibility: false,
        owner_bypass: false,
    }
}

#[tokio::test]
async fn receipts_all_sequenced_terminal_classes_restore_without_execution_authority() {
    for status in [
        "completed",
        "failed",
        "stopped",
        "timeout",
        "timed_out",
        "lost",
        "cancelled",
    ] {
        let store = Arc::new(MemoryReceipts::default());
        let a = durable(&store).await;
        register(&a, INSTANCE_A, empty_inventory()).await;
        let (job, _) = start_and_take_over(&a, INSTANCE_A).await;
        let mut terminal = update(INSTANCE_A, &job.job_id, 2, status, None, true);
        terminal.exit_code = Some(if status == "failed" { 7 } else { 0 });
        terminal.log_snapshot = Some(ShellJobLogSnapshot {
            stdout: stream("retained output\n", 20, true),
            stderr: stream("retained error\n", 4, true),
        });
        a.update_job(terminal).await.unwrap();
        let original = a.job_log(&job.job_id, None, None, None).await.unwrap();
        let original_receipt = store.rows.lock().unwrap()[0].clone();
        drop(a);
        let b = durable(&store).await;
        assert_eq!(
            b.get_job_for_auth(Some(&access(Some("tester"), None)), &job.job_id)
                .await
                .unwrap()
                .status,
            status
        );
        assert_eq!(b.list_jobs(None).await.len(), 1);
        let restored = b.job_log(&job.job_id, None, None, None).await.unwrap();
        assert_eq!(restored.0.exit_code, original.0.exit_code);
        assert_eq!(
            (&restored.1, &restored.2, restored.3, restored.4),
            (&original.1, &original.2, original.3, original.4)
        );
        assert_ne!(restored.0.observation_token, original.0.observation_token);
        register(&b, INSTANCE_B, empty_inventory()).await;
        assert_eq!(
            b.stop_job(&job.job_id, "tester".into())
                .await
                .unwrap()
                .status,
            status
        );
        let inner = b.inner.lock().await;
        assert!(inner.pending_by_id.is_empty());
        assert!(inner.request_to_job.is_empty());
        assert!(inner
            .queues_by_runner
            .values()
            .all(|queue| queue.is_empty()));
        assert!(inner.jobs_by_id[&job.job_id]
            .detached_idempotency_intent
            .is_none());
        assert_eq!(
            inner.jobs_by_id[&job.job_id]
                .observation
                .terminal_observed_at,
            Some(original_receipt.terminal_observed_at)
        );
    }
}

#[tokio::test]
async fn receipts_owner_is_fixed_before_runner_reregistration_and_groups_are_isolated() {
    for group in [
        None,
        Some(RunnerAccessGroup::SharedKey("a".repeat(64))),
        Some(RunnerAccessGroup::ProjectGrant("grant-a".into())),
        Some(RunnerAccessGroup::OpenAnonymous),
    ] {
        let store = Arc::new(MemoryReceipts::default());
        let a = durable(&store).await;
        let caller = access(Some("tester"), group.clone());
        a.register_with_auth(
            register_request(INSTANCE_A, empty_inventory()),
            Some(&caller),
        )
        .await
        .unwrap();
        let job = a
            .start_job_with_metadata_for_access(
                start_request("echo safe"),
                "tester".into(),
                ShellJobStartMetadata::default(),
                Some(&caller),
                None,
            )
            .await
            .unwrap();
        a.stop_job_for_auth(Some(&caller), &job.job_id, "tester".into())
            .await
            .unwrap();
        drop(a);
        let b = durable(&store).await;
        assert!(b.get_job_for_auth(Some(&caller), &job.job_id).await.is_ok());
        for other in [
            access(Some("mallory"), None),
            access(None, Some(RunnerAccessGroup::SharedKey("b".repeat(64)))),
            access(
                None,
                Some(RunnerAccessGroup::ProjectGrant("grant-b".into())),
            ),
        ] {
            assert!(b.get_job_for_auth(Some(&other), &job.job_id).await.is_err());
            assert!(b.list_jobs_for_auth(Some(&other), None).await.is_empty());
        }
        let global = RunnerAccess {
            global_visibility: true,
            ..access(None, None)
        };
        assert!(b.get_job_for_auth(Some(&global), &job.job_id).await.is_ok());
        let mut replacement = register_request(INSTANCE_B, empty_inventory());
        replacement.owner = Some("mallory".into());
        b.register(replacement).await.unwrap();
        assert!(b.get_job_for_auth(Some(&caller), &job.job_id).await.is_ok());
        assert!(b
            .get_job_for_auth(Some(&access(Some("mallory"), None)), &job.job_id)
            .await
            .is_err());
    }
}

#[tokio::test]
async fn receipts_active_hidden_cleanup_and_detached_are_never_saved() {
    let store = Arc::new(MemoryReceipts::default());
    let a = durable(&store).await;
    register(&a, INSTANCE_A, empty_inventory()).await;
    let (active, _) = start_and_take_over(&a, INSTANCE_A).await;
    a.update_job(update(
        INSTANCE_A,
        &active.job_id,
        1,
        "running",
        None,
        false,
    ))
    .await
    .unwrap();
    for visibility in [
        ShellJobVisibility::HiddenUntilHandoff,
        ShellJobVisibility::CleanupPending,
    ] {
        let job = a
            .start_job_with_metadata(
                start_request("echo hidden"),
                "tester".into(),
                ShellJobStartMetadata {
                    visibility,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        a.update_job(update(
            INSTANCE_A,
            &job.job_id,
            2,
            "completed",
            Some("hidden\n"),
            true,
        ))
        .await
        .unwrap();
    }
    let (detached, _) = start_and_take_over(&a, INSTANCE_A).await;
    // Capture eligibility independently of the detached supervisor mechanism.
    a.inner
        .lock()
        .await
        .jobs_by_id
        .get_mut(&detached.job_id)
        .unwrap()
        .kind = "run_detached_process".into();
    a.update_job(update(
        INSTANCE_A,
        &detached.job_id,
        2,
        "completed",
        None,
        true,
    ))
    .await
    .unwrap();
    assert!(store.rows.lock().unwrap().is_empty());
    drop(a);
    let b = durable(&store).await;
    register(&b, INSTANCE_B, empty_inventory()).await;
    assert!(b.list_jobs(None).await.is_empty());
    assert!(b.get_job(&active.job_id).await.is_err());
    assert!(b.inner.lock().await.pending_by_id.is_empty());
}

#[tokio::test]
async fn receipts_inventory_replay_and_repeated_restore_keep_deadline_and_verdict() {
    let store = Arc::new(MemoryReceipts::default());
    let a = durable(&store).await;
    register(&a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&a, INSTANCE_A).await;
    a.update_job(update(
        INSTANCE_A,
        &job.job_id,
        2,
        "completed",
        Some("done\n"),
        true,
    ))
    .await
    .unwrap();
    let deadline = {
        let mut rows = store.rows.lock().unwrap();
        rows[0].terminal_observed_at = now_ts() - JOB_TERMINAL_RETENTION_SECS + 10;
        rows[0].expires_at = rows[0].terminal_observed_at + JOB_TERMINAL_RETENTION_SECS;
        rows[0].expires_at
    };
    drop(a);
    for _ in 0..3 {
        let b = durable(&store).await;
        let snapshot =
            snapshot_from_request(&job, &request, "failed", 99, stream("changed\n", 1, false));
        register(
            &b,
            INSTANCE_A,
            ShellJobInventory {
                active_complete: true,
                jobs: vec![snapshot],
            },
        )
        .await;
        assert_eq!(b.get_job(&job.job_id).await.unwrap().status, "completed");
        assert_eq!(b.list_jobs(None).await.len(), 1);
        assert_eq!(store.rows.lock().unwrap()[0].expires_at, deadline);
        b.inner
            .lock()
            .await
            .jobs_by_id
            .get_mut(&job.job_id)
            .unwrap()
            .observation
            .receipt_expires_at = Some(now_ts());
        assert!(b.get_job(&job.job_id).await.is_err());
        assert!(b.list_jobs(None).await.is_empty());
        assert!(b.job_log(&job.job_id, None, None, None).await.is_err());
    }
}

#[tokio::test]
async fn receipts_failure_keeps_completed_result_and_reconciliation_lost_is_captured() {
    let store = Arc::new(MemoryReceipts {
        fail: true,
        ..Default::default()
    });
    let a = durable(&store).await;
    register(&a, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&a, INSTANCE_A).await;
    a.update_job(update(
        INSTANCE_A,
        &job.job_id,
        2,
        "completed",
        Some("done\n"),
        true,
    ))
    .await
    .unwrap();
    assert_eq!(a.get_job(&job.job_id).await.unwrap().exit_code, Some(0));
    assert_eq!(
        a.job_log(&job.job_id, None, None, None)
            .await
            .unwrap()
            .1
            .as_deref(),
        Some("done\n")
    );
    assert!(store.rows.lock().unwrap().is_empty());
    let store = Arc::new(MemoryReceipts::default());
    let a = durable(&store).await;
    register(&a, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&a, INSTANCE_A).await;
    register(&a, INSTANCE_B, empty_inventory()).await;
    assert_eq!(store.rows.lock().unwrap()[0].snapshot.status, "lost");
    drop(a);
    assert_eq!(
        durable(&store)
            .await
            .get_job(&job.job_id)
            .await
            .unwrap()
            .status,
        "lost"
    );
}

#[tokio::test]
async fn receipts_polling_completion_preserves_full_server_bounded_streams() {
    let store = Arc::new(MemoryReceipts::default());
    let a = durable(&store).await;
    let mut registration = register_request(INSTANCE_A, empty_inventory());
    registration.capabilities.job_state_reconciliation = false;
    registration.job_inventory = None;
    a.register(registration).await.unwrap();
    let (job, request) = start_and_take_over(&a, INSTANCE_A).await;
    a.complete(crate::runner_protocol::RunnerResultRequest {
        client_id: CLIENT_ID.into(),
        runner_instance_id: INSTANCE_A.into(),
        request_id: request.request_id,
        exit_code: Some(0),
        stdout: Some("stdout\n".repeat(100_000)),
        stderr: Some("stderr\n".repeat(100_000)),
        duration_ms: Some(20),
        error: None,
    })
    .await
    .unwrap();
    let original = a
        .job_log(&job.job_id, None, None, Some(100_000))
        .await
        .unwrap();
    assert!(original.0.stdout_log_truncated);
    assert!(
        store.rows.lock().unwrap()[0].snapshot.stdout.tail.len() > JOB_SNAPSHOT_STREAM_MAX_BYTES
    );
    drop(a);
    let b = durable(&store).await;
    let restored = b
        .job_log(&job.job_id, None, None, Some(100_000))
        .await
        .unwrap();
    assert_eq!(
        (&restored.1, &restored.2, restored.3, restored.4),
        (&original.1, &original.2, original.3, original.4)
    );
    assert_eq!(restored.0.status, "completed");
    assert_eq!(restored.0.exit_code, Some(0));
}

#[tokio::test]
async fn receipts_protocol_violation_and_recovery_sweep_capture_final_evidence() {
    let store = Arc::new(MemoryReceipts::default());
    let a = durable(&store).await;
    register(&a, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&a, INSTANCE_A).await;
    let mut invalid = update(INSTANCE_A, &job.job_id, 1, "running", None, false);
    invalid.validation_progress = Some(ShellJobValidationProgress {
        completed: 0,
        current_step: None,
        failed_step: None,
    });
    a.update_job(invalid).await.unwrap();
    assert_eq!(store.rows.lock().unwrap()[0].snapshot.status, "failed");
    let (lost, _) = start_and_take_over(&a, INSTANCE_A).await;
    a.update_job(update(INSTANCE_A, &lost.job_id, 1, "running", None, false))
        .await
        .unwrap();
    drive_into_recovering(&a, &lost.job_id, INSTANCE_A).await;
    age_recovering_since(&a, &lost.job_id, job_recovery_grace_secs() + 1).await;
    recovery_timeout_sweep(&a).await;
    let rows = store.rows.lock().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .find(|row| row.snapshot.job_id == lost.job_id)
            .unwrap()
            .snapshot
            .status,
        "lost"
    );
}

#[tokio::test]
async fn receipts_validation_argv_is_omitted_and_inventory_cannot_restore_it() {
    let store = Arc::new(MemoryReceipts::default());
    let a = durable(&store).await;
    register(&a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&a, INSTANCE_A).await;
    {
        let mut inner = a.inner.lock().await;
        let record = inner.jobs_by_id.get_mut(&job.job_id).unwrap();
        record.validation = cargo_validation_start_metadata(None, None, None).validation;
        record.validation_steps = vec!["test".into()];
    }
    let mut terminal = update(INSTANCE_A, &job.job_id, 2, "completed", None, true);
    terminal.validation_progress = Some(ShellJobValidationProgress {
        completed: 1,
        current_step: None,
        failed_step: None,
    });
    a.update_job(terminal).await.unwrap();
    assert!(store.rows.lock().unwrap()[0]
        .snapshot
        .context
        .validation
        .is_none());
    drop(a);
    let b = durable(&store).await;
    let mut snapshot = snapshot_from_request(&job, &request, "completed", 2, stream("", 1, false));
    snapshot.context.validation = cargo_validation_start_metadata(None, None, None).validation;
    snapshot.context.validation_steps = vec!["test".into()];
    snapshot.validation_progress = Some(ShellJobValidationProgress {
        completed: 1,
        current_step: None,
        failed_step: None,
    });
    register(
        &b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        },
    )
    .await;
    assert!(b.get_job(&job.job_id).await.unwrap().validation.is_none());
    assert!(b.inner.lock().await.pending_by_id.is_empty());
}

#[tokio::test]
async fn receipts_unowned_admission_preserves_only_existing_global_visibility() {
    let store = Arc::new(MemoryReceipts::default());
    let a = durable(&store).await;
    let mut registration = register_request(INSTANCE_A, empty_inventory());
    registration.owner = None;
    a.register(registration).await.unwrap();
    let job = a
        .start_job(start_request("echo safe"), "bootstrap".into())
        .await
        .unwrap();
    a.stop_job(&job.job_id, "bootstrap".into()).await.unwrap();
    assert_eq!(store.rows.lock().unwrap().len(), 1);
    drop(a);
    let b = durable(&store).await;
    assert!(b
        .get_job_for_auth(Some(&access(Some("tester"), None)), &job.job_id)
        .await
        .is_err());
    let global = RunnerAccess {
        global_visibility: true,
        ..access(None, None)
    };
    assert!(b.get_job_for_auth(Some(&global), &job.job_id).await.is_ok());
    register(&b, INSTANCE_B, empty_inventory()).await;
    assert!(b
        .get_job_for_auth(Some(&access(Some("tester"), None)), &job.job_id)
        .await
        .is_err());
}
