use super::*;
use crate::runner_protocol::{
    RunnerPollRequest, RunnerRegisterRequest, ShellCommandExecutionState,
};
use webcodex_core::configured_skills::ConfiguredSkillRootsRequest;

fn configured_skills_registration(instance: &str, read: bool) -> RunnerRegisterRequest {
    current_runner_registration(RunnerRegisterRequest {
        client_id: "configured-skills-runner".to_string(),
        runner_instance_id: instance.to_string(),
        runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
        display_name: None,
        owner: Some("alice".to_string()),
        hostname: None,
        capabilities: RunnerCapabilities {
            configured_skill_roots_read: read,
            ..Default::default()
        },
        host_context: None,
        policy: None,
        process_started_at: None,
        build: None,
        job_concurrency_limit: None,
        job_inventory: None,
        coding_agent_providers: None,
        coding_agent_inventory: None,
    })
}

fn alice() -> RunnerAccess {
    auth_context(Some("alice"), false)
}

#[tokio::test]
async fn configured_skill_roots_enqueue_requires_exact_instance_and_explicit_capability() {
    let registry = RunnerRegistry::default();
    registry
        .register(configured_skills_registration("instance-a", false))
        .await
        .unwrap();
    let auth = alice();

    let legacy_capability_error = registry
        .enqueue_configured_skill_roots(
            "configured-skills-runner",
            "instance-a",
            ConfiguredSkillRootsRequest::List,
            Some(&auth),
            "test".to_string(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        legacy_capability_error,
        "configured_skill_roots_capability_unavailable: exact Runner does not support configured_skill_roots_read"
    );

    let capability_error = registry
        .enqueue_configured_skill_roots_typed(
            "configured-skills-runner",
            "instance-a",
            ConfiguredSkillRootsRequest::List,
            Some(&auth),
            "test".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        capability_error,
        EnqueueConfiguredSkillRootsError::UnsupportedCapability {
            capability: "configured_skill_roots_read",
            ..
        }
    ));

    registry
        .register(configured_skills_registration("instance-a", true))
        .await
        .unwrap();
    let stale_error = registry
        .enqueue_configured_skill_roots_typed(
            "configured-skills-runner",
            "replacement-instance",
            ConfiguredSkillRootsRequest::List,
            Some(&auth),
            "test".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        stale_error,
        EnqueueConfiguredSkillRootsError::RunnerChanged { .. }
    ));

    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty());
}

#[tokio::test]
async fn configured_skill_roots_dequeue_rejects_replacement_runner_before_dispatch() {
    let registry = RunnerRegistry::default();
    registry
        .register(configured_skills_registration("instance-a", true))
        .await
        .unwrap();
    let auth = alice();
    let (_request_id, receiver) = registry
        .enqueue_configured_skill_roots_typed(
            "configured-skills-runner",
            "instance-a",
            ConfiguredSkillRootsRequest::List,
            Some(&auth),
            "test".to_string(),
        )
        .await
        .unwrap();

    // Normal replacement registration drains stale synchronous work. Mutate
    // only the exact process lease here to prove dequeue independently fences
    // a later process that recycles the same stable client_id.
    {
        let mut inner = registry.inner.lock().await;
        inner
            .runners
            .get_mut("configured-skills-runner")
            .unwrap()
            .runner_instance_id = "instance-b".to_string();
    }
    let polled = registry
        .poll(RunnerPollRequest {
            client_id: "configured-skills-runner".to_string(),
            runner_instance_id: "instance-b".to_string(),
        })
        .await
        .unwrap();
    assert!(polled.is_none());
    let response = receiver.await.unwrap();
    assert!(!response.success);
    assert_eq!(response.request_dispatched, Some(false));
    assert_eq!(
        response.command_execution_state,
        Some(ShellCommandExecutionState::NotStarted)
    );
    assert!(response.error.as_deref().is_some_and(|error| {
        error.contains("configured Skill roots target Runner changed before dispatch")
    }));

    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty());
    assert!(inner
        .queues_by_runner
        .get("configured-skills-runner")
        .is_none_or(|queue| queue.is_empty()));
}
