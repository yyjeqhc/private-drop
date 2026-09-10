use super::*;

#[test]
fn mcp_gateway_register_request_projects_bounded_provider_inventory_without_local_launch_details() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path().join("config/project-registry"));
    cfg.mcp_gateway.providers = vec![webcodex_runner::config::McpGatewayProviderConfig {
        id: "local-tools".to_string(),
        name: "Local tools".to_string(),
        executable: "/private/operator/bin/local-tools-mcp".to_string(),
        args: vec!["--secret-profile".to_string()],
        cwd: Some("/private/operator/provider-workdir".to_string()),
        env_from_env: std::collections::BTreeMap::from([(
            "GITHUB_TOKEN".to_string(),
            "OPERATOR_GITHUB_TOKEN".to_string(),
        )]),
        timeout_secs: Some(5),
    }];

    let body = build_register_request(&cfg, "runner-instance", 0);
    let providers = body
        .policy
        .as_ref()
        .and_then(|policy| policy.mcp_gateway_providers.as_ref())
        .expect("MCP provider inventory");
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].provider_id, "local-tools");
    assert_eq!(providers[0].name, "Local tools");
    assert!(!providers[0].provider_instance_id.is_empty());
    let serialized = serde_json::to_string(providers).unwrap();
    assert!(!serialized.contains("/private/operator/bin"));
    assert!(!serialized.contains("--secret-profile"));
    assert!(!serialized.contains("/private/operator/provider-workdir"));
    assert!(!serialized.contains("GITHUB_TOKEN"));
    assert!(!serialized.contains("OPERATOR_GITHUB_TOKEN"));
    assert!(!serialized.contains("timeout_secs"));
}

#[test]
fn current_runner_registration_advertises_v2_and_complete_generation_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path().join("config/project-registry"));
    let body = build_register_request(&cfg, "baseline-instance", 0);
    assert_eq!(
        body.runner_protocol_generation,
        RUNNER_PROTOCOL_GENERATION_V2
    );

    let capabilities = serde_json::to_value(&body.capabilities).unwrap();
    assert_eq!(
        RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES.len(),
        22
    );
    assert!(
        !RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES
            .contains(&"apply_text_edit_line_scope"),
        "line scope is additive and must not become a generation-2 registration baseline"
    );
    assert!(
        !RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES.contains(&"apply_patch"),
        "apply_patch is additive and must not become a generation-2 registration baseline"
    );
    assert!(
        !RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES
            .contains(&"apply_patch_match_metadata"),
        "the 0.4 patch success contract is additive and must not become a generation-2 baseline"
    );
    assert!(
        !RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES
            .contains(&"apply_patch_matching_mode"),
        "matching_mode is additive and must not become a generation-2 registration baseline"
    );
    assert!(
        !RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES
            .contains(&"apply_patch_strict_matching"),
        "strict patch matching is additive and must not become a generation-2 registration baseline"
    );
    assert_eq!(
        capabilities
            .get("apply_patch_match_metadata")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "current Runner must explicitly advertise the current apply_patch success contract"
    );
    assert_eq!(
        capabilities
            .get("apply_patch_matching_mode")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "current Runner must explicitly advertise enum-based apply_patch matching"
    );
    assert_eq!(
        capabilities
            .get("apply_patch_strict_matching")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "current Runner must explicitly advertise strict patch matching"
    );
    for capability in RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES {
        assert_eq!(
            capabilities
                .get(*capability)
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "current Runner must advertise V2 baseline capability {capability}"
        );
    }
}

#[test]
fn computer_register_request_announces_platform_capabilities_and_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path().join("config/project-registry"));
    // A stale or hand-edited config cannot force capability advertisement:
    // registration replaces it with the result of the real host probe.
    cfg.capabilities = Some(RunnerCapabilities {
        computer_observe: true,
        computer_application_discovery: true,
        computer_application_launch: true,
        computer_display_observe: true,
        computer_snapshot_region: true,
        computer_accessibility_observe: true,
        computer_element_state: true,
        computer_control: true,
        computer_scroll_to_element: true,
        computer_key_input: true,
        computer_window_activate: true,
        computer_text_input: true,
        project_lifecycle: false,
        project_path_registration: false,
        ..Default::default()
    });
    let body = build_register_request(&cfg, "inst-1", 0);
    assert_eq!(body.runner_instance_id, "inst-1");
    assert_eq!(
        body.runner_protocol_generation,
        RUNNER_PROTOCOL_GENERATION_V2
    );
    // Verify all effective capabilities are advertised from the real host probe.
    let caps = body.capabilities;
    assert!(caps.shell);
    assert!(caps.file_read);
    assert!(caps.file_write);
    assert!(caps.artifact_export_chunk_read);
    assert!(caps.artifact_export_streaming_metadata);
    assert!(caps.structured_file_delete);
    assert!(caps.apply_text_edit_occurrence);
    assert!(caps.apply_text_edit_line_scope);
    assert!(caps.apply_patch);
    assert!(caps.async_jobs);
    assert!(caps.async_shell_jobs);
    assert_eq!(
        caps.ssh_shell,
        SshConnectionPool::is_available(),
        "one-shot/background SSH capability must match the platform backend and local OpenSSH availability"
    );
    assert_eq!(
        caps.persistent_shell,
        webcodex_persistent_shell::local_shell_supported(),
        "local persistent-shell capability must match the platform transport compiled into this Runner"
    );
    assert_eq!(
        caps.ssh_persistent_shell,
        SshConnectionPool::persistent_shell_available(),
        "SSH persistent-shell capability must match the platform backend and local OpenSSH availability"
    );
    assert!(caps.structured_validation_argv);
    assert!(caps.structured_cargo_test_count_assertion);
    assert!(caps.structured_cargo_test_execution_policy);
    assert!(caps.structured_go_test_json);
    assert!(caps.structured_go_test_tool);
    assert!(caps.structured_go_test_packages);
    assert!(caps.structured_process_argv);
    assert!(caps.structured_script_payload);
    assert!(caps.structured_script_javascript);
    assert!(caps.structured_script_typescript);
    assert!(caps.internal_posix_script);
    assert!(caps.structured_execution_jobs);
    assert!(caps.lsp_read_only_navigation);
    assert!(caps.lsp_call_hierarchy);
    assert!(caps.project_lifecycle);
    assert!(caps.project_path_registration);
    assert_eq!(
        caps.computer_observe,
        cfg!(any(target_os = "macos", windows)),
        "computer observation is advertised only when this Runner binary has a supported native implementation"
    );
    assert_eq!(
        caps.computer_application_discovery,
        cfg!(any(target_os = "macos", windows)),
        "computer application discovery is advertised only by native macOS/Windows implementations"
    );
    assert_eq!(
        caps.computer_application_launch,
        cfg!(any(target_os = "macos", windows)),
        "computer application launch is independently advertised only by native macOS/Windows implementations"
    );
    assert_eq!(
        caps.computer_clipboard_read,
        cfg!(any(target_os = "macos", windows)),
        "clipboard read is independently advertised only by native macOS/Windows implementations"
    );
    assert_eq!(
        caps.computer_clipboard_write,
        cfg!(any(target_os = "macos", windows)),
        "clipboard write is independently advertised only by native macOS/Windows implementations"
    );
    assert_eq!(
        caps.computer_display_observe,
        cfg!(any(target_os = "macos", windows)),
        "full-display observation is independently advertised only by exact native macOS/Windows display backends"
    );
    assert_eq!(
        caps.computer_snapshot_region,
        cfg!(any(target_os = "macos", windows)),
        "computer region snapshot is independently advertised only when native window capture is supported"
    );
    assert_eq!(
        caps.computer_accessibility_observe,
        cfg!(any(target_os = "macos", windows)),
        "computer accessibility observation is advertised only by native AX/UIA implementations"
    );
    assert_eq!(
        caps.computer_element_state,
        cfg!(any(target_os = "macos", windows)),
        "computer element state is independently advertised only by native AX/UIA implementations"
    );
    assert_eq!(
        caps.computer_control,
        cfg!(any(target_os = "macos", windows)),
        "computer control is independently advertised only by native macOS/Windows implementations"
    );
    assert_eq!(
        caps.computer_scroll_to_element,
        cfg!(any(target_os = "macos", windows)),
        "computer scroll-to-element is independently advertised only by native macOS/Windows implementations"
    );
    assert_eq!(
        caps.computer_key_input,
        cfg!(any(target_os = "macos", windows)),
        "computer key input is independently advertised only by native macOS/Windows implementations"
    );
    assert_eq!(
        caps.computer_window_activate,
        cfg!(any(target_os = "macos", windows)),
        "computer window activation is independently advertised only by native macOS/Windows implementations"
    );
    assert_eq!(
        caps.computer_text_input,
        cfg!(any(target_os = "macos", windows)),
        "computer text input is independently advertised only by native macOS/Windows implementations"
    );
}

#[test]
fn phase_e2_register_request_reports_effective_job_concurrency_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path().join("config/project-registry"));
    for (configured, expected) in [(None, 4), (Some(1), 1), (Some(8), 8), (Some(64), 64)] {
        cfg.max_concurrent_jobs = configured;
        let body = build_register_request(&cfg, "inst-limit", 0);
        assert_eq!(
            body.job_concurrency_limit,
            Some(expected),
            "configured={configured:?}"
        );
    }
}

#[test]
fn register_request_carries_sanitized_shell_profiles_summary() {
    // A config with one profile carrying a secret env value and a secret
    // init_script body. The sanitized summary must report the profile name,
    // has_init_script=true, and env_keys_count, but MUST NOT include the env
    // value or the init_script body.
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path().join("config/project-registry"));
    let secret_env = "DO_NOT_LEAK_THIS_ENV_VALUE";
    let secret_script = "DO_NOT_LEAK_THIS_INIT_SCRIPT_BODY";
    cfg.shell = shell_with_profiles(
        Some("rust"),
        vec![(
            "rust",
            ShellProfileConfig {
                program: Some("sh".to_string()),
                args: Some(vec!["-c".to_string()]),
                env: profile_env(&[("SECRET_KEY", secret_env)]),
                init_script: Some(secret_script.to_string()),
                ..ShellProfileConfig::default()
            },
        )],
    );
    let body = build_register_request(&cfg, "inst-1", 0);
    let policy = body.policy.expect("agent registers a policy");
    let summary = policy
        .shell_profiles
        .as_ref()
        .expect("sanitized shell profiles summary is present");
    assert_eq!(summary.default_profile.as_deref(), Some("rust"));
    assert_eq!(summary.configured_count, 1);
    assert_eq!(summary.profiles.len(), 1);
    let entry = &summary.profiles[0];
    assert_eq!(entry.name, "rust");
    assert!(entry.has_init_script);
    assert_eq!(entry.env_keys_count, 1);
    assert_eq!(entry.program, "sh");
    assert_eq!(entry.args_count, 1);
    // Sanitization: the rendered summary never carries env values or the
    // init_script body.
    let rendered = serde_json::to_string(summary).unwrap();
    assert!(!rendered.contains(secret_env), "{rendered}");
    assert!(!rendered.contains(secret_script), "{rendered}");
}
