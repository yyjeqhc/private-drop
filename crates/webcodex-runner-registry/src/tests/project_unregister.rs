use super::*;

#[tokio::test]
async fn project_active_job_batch_preserves_visibility_lifecycle_and_bounds() {
    use crate::state::{JobLifecycleState, JobRecoveryPhase, JobRecoveryState, ShellJobVisibility};
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "batch-inst").await;
    let target = "agent:oe:target";
    let other = "agent:oe:other";
    let empty = "agent:oe:empty";
    let mut ids = Vec::new();
    for index in 0..112 {
        let job = registry
            .start_job_with_metadata(
                ShellJobOpRequest {
                    op: "start".into(),
                    client_id: Some("oe".into()),
                    cwd: None,
                    command: Some("echo fixture-only".into()),
                    timeout_secs: Some(60),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "alice".into(),
                ShellJobStartMetadata {
                    project_id: Some(target.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        ids.push(job.job_id);
        let mut inner = registry.inner.lock().await;
        let job = inner.jobs_by_id.get_mut(ids.last().unwrap()).unwrap();
        match index {
            0 => job.visibility = ShellJobVisibility::HiddenUntilHandoff,
            1 => job.visibility = ShellJobVisibility::CleanupPending,
            2 => job.lifecycle = JobLifecycleState::Completed,
            3 => job.project_id = None,
            4 => job.project_id = Some(other.into()),
            5 => {
                job.lifecycle = JobLifecycleState::Running;
                job.recovery = JobRecoveryState {
                    phase: Some(JobRecoveryPhase::Recovering),
                    recovering_since: Some(now_ts() - job_recovery_grace_secs() - 1),
                    ..Default::default()
                };
            }
            6 => {
                job.lifecycle = JobLifecycleState::Running;
                job.recovery = JobRecoveryState {
                    phase: Some(JobRecoveryPhase::Recovering),
                    recovering_since: Some(now_ts()),
                    ..Default::default()
                };
            }
            7 => job.auth_group = shared_key_access("foreign").group,
            _ => {}
        }
    }
    let alice = auth_context(Some("alice"), false);
    let bob = auth_context(Some("bob"), false);
    assert_eq!(registry.project_job_scan_count_for_test(), 0);
    let counts = registry
        .count_active_jobs_for_projects(Some(&alice), &[target, other, empty, target])
        .await;
    assert_eq!(registry.project_job_scan_count_for_test(), 1);
    assert_eq!(counts.len(), 3);
    assert_eq!(counts[target], 105); // 104 queued + one still-recovering Job.
    assert_eq!(counts[other], 1);
    assert_eq!(counts[empty], 0);
    assert_eq!(
        registry.inner.lock().await.jobs_by_id[&ids[5]].lifecycle,
        JobLifecycleState::Lost
    );
    let denied = registry
        .count_active_jobs_for_projects(Some(&bob), &[target, other])
        .await;
    assert!(denied.values().all(|count| *count == 0));
    let grouped = registry
        .count_active_jobs_for_projects(Some(&shared_key_access("foreign")), &[target])
        .await;
    assert_eq!(grouped[target], 1);
    let global = registry
        .count_active_jobs_for_projects(None, &[target])
        .await;
    assert_eq!(global[target], 106);
    assert!(!global.contains_key(other));
    let scans = registry.project_job_scan_count_for_test();
    assert!(registry
        .count_active_jobs_for_projects(None, &[])
        .await
        .is_empty());
    assert_eq!(registry.project_job_scan_count_for_test(), scans);
    assert_eq!(
        registry
            .count_active_jobs_for_project(Some(&alice), target)
            .await,
        counts[target]
    );
}

#[tokio::test]
async fn project_active_job_query_is_not_truncated_and_unregister_fences_starts() {
    let registry = RunnerRegistry::default();
    register_with_instance(&registry, "oe", "inst-jobs").await;
    let request = |command: &str| ShellJobOpRequest {
        op: "start".to_string(),
        client_id: Some("oe".to_string()),
        cwd: None,
        command: Some(command.to_string()),
        timeout_secs: Some(60),
        job_id: None,
        since_stdout_line: None,
        since_stderr_line: None,
        tail_lines: None,
        limit: None,
        codex: None,
    };
    let target = "agent:oe:target";
    let target_job = registry
        .start_job_with_metadata(
            request("sleep 60"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(target.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    {
        let mut inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&target_job.job_id)
            .unwrap()
            .created_at = 0;
    }
    for index in 0..101 {
        registry
            .start_job_with_metadata(
                request(&format!("echo {index}")),
                "tester".to_string(),
                ShellJobStartMetadata {
                    project_id: Some(format!("agent:oe:other-{index}")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(registry.list_jobs(Some(100)).await.len(), 100);
    assert_eq!(
        registry.count_active_jobs_for_project(None, target).await,
        1
    );
    assert_eq!(
        registry
            .begin_project_unregister(None, target)
            .await
            .unwrap(),
        1
    );

    {
        let mut inner = registry.inner.lock().await;
        let job = inner.jobs_by_id.get_mut(&target_job.job_id).unwrap();
        job.lifecycle = super::super::state::JobLifecycleState::Completed;
        job.ended_at = Some(now_ts());
    }
    assert_eq!(
        registry
            .begin_project_unregister(None, target)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        registry
            .begin_project_unregister(None, target)
            .await
            .unwrap(),
        0
    );
    registry.end_project_unregister(target).await;
    let blocked = registry
        .start_job_with_metadata(
            request("echo blocked"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(target.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(blocked, "project_unregister_in_progress");
    registry.end_project_unregister(target).await;
    registry
        .start_job_with_metadata(
            request("echo allowed"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(target.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}
