use super::*;

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
