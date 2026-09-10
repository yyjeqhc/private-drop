use super::*;

#[tokio::test]
async fn registry_allows_session_scoped_run_without_ssh_resource() {
    let registry = RunnerRegistry::default();
    registry
        .register(current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "xrh".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(
                RunnerCapabilities::default(),
            ),
            policy: None,
        }))
        .await
        .unwrap();

    let (request_id, _rx) = registry
        .enqueue_run_with_ssh(
            ShellRunRequest {
                client_id: "xrh".to_string(),
                cwd: None,
                command: "echo local".to_string(),
                stdin: None,
                timeout_secs: 10,
                wait_timeout_secs: 1,
            },
            "test".to_string(),
            None,
            Some("wc_sess_local".to_string()),
        )
        .await
        .unwrap();
    assert!(!registry.cancel_request(&request_id).await);

    let error = registry
        .enqueue_run_with_ssh(
            ShellRunRequest {
                client_id: "xrh".to_string(),
                cwd: None,
                command: "echo remote".to_string(),
                stdin: None,
                timeout_secs: 10,
                wait_timeout_secs: 1,
            },
            "test".to_string(),
            Some("tmp".to_string()),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.contains("ssh_session_required"), "{error}");
}

#[tokio::test]
async fn javascript_script_enqueue_requires_additive_runner_capability() {
    let registry = RunnerRegistry::default();
    let registration = |javascript: bool| {
        let mut capabilities =
            crate::test_support::current_runner_capabilities(RunnerCapabilities::default());
        capabilities.structured_script_javascript = javascript;
        current_runner_registration(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: "script-cap".to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities,
            policy: None,
        })
    };
    registry.register(registration(false)).await.unwrap();

    let javascript = ShellScriptPayload {
        language: ShellScriptLanguage::Javascript,
        script: "console.log('hello');\n".to_string(),
        args: Vec::new(),
    };
    let error = registry
        .enqueue_script(
            "script-cap".to_string(),
            None,
            javascript.clone(),
            None,
            10,
            1,
            "test".to_string(),
        )
        .await
        .unwrap_err();
    assert!(error.contains("structured_script_javascript"), "{error}");

    // The additive fence must not disable languages understood by older typed-script Runners.
    let (bash_request_id, _rx) = registry
        .enqueue_script(
            "script-cap".to_string(),
            None,
            ShellScriptPayload {
                language: ShellScriptLanguage::Bash,
                script: "printf ok\n".to_string(),
                args: Vec::new(),
            },
            None,
            10,
            1,
            "test".to_string(),
        )
        .await
        .unwrap();
    assert!(!registry.cancel_request(&bash_request_id).await);

    registry.register(registration(true)).await.unwrap();
    let (request_id, _rx) = registry
        .enqueue_script(
            "script-cap".to_string(),
            None,
            javascript,
            None,
            10,
            1,
            "test".to_string(),
        )
        .await
        .unwrap();
    assert!(!registry.cancel_request(&request_id).await);
}

#[tokio::test]
async fn registry_rejects_unknown_client_run() {
    let registry = RunnerRegistry::default();
    let err = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "missing".to_string(),
                cwd: None,
                command: "pwd".to_string(),
                stdin: None,
                timeout_secs: 10,
                wait_timeout_secs: 1,
            },
            "test".to_string(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("unknown shell client"));
}
