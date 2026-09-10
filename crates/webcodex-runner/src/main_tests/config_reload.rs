use super::*;

fn reload_toml(
    client_id: &str,
    max_jobs: Option<usize>,
    max_timeout: u64,
    max_output: usize,
    shell_program: &str,
    strategy: &str,
    claude_enabled: bool,
    claude_command: &str,
    search_mapping: &str,
) -> String {
    let max_jobs = max_jobs
        .map(|value| format!("max_concurrent_jobs = {value}\n"))
        .unwrap_or_default();
    format!(
        r#"server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "{client_id}"
owner = "alice"
poll_interval_ms = 1000
plugins.request_timeout_secs = 30
{max_jobs}
# Explicit project_registry_dir: load_config materializes the default from the
# per-user config base, which depends on ambient HOME/USERPROFILE that other
# tests mutate.
project_registry_dir = "project-registry"
policy.allow_raw_shell = true
policy.allow_cwd_anywhere = true
policy.allowed_roots = ["/"]
policy.max_timeout_secs = {max_timeout}
policy.max_output_bytes = {max_output}
shell.program = "{shell_program}"
shell.args = ["-c"]
tool_providers.strategy = "{strategy}"
tool_providers.claude_code.enabled = {claude_enabled}
tool_providers.claude_code.command = "{claude_command}"
tool_providers.claude_code.args = ["mcp", "serve"]
tool_providers.claude_code.timeout_secs = 30
[tool_providers.claude_code.mapping]
search_project_text = "{search_mapping}"
"#
    )
}

fn reload_fixture() -> (tempfile::TempDir, PathBuf, ReloadableRunnerConfig) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runner.toml");
    std::fs::write(
        &path,
        reload_toml(
            "oe",
            None,
            60,
            1024,
            "sh",
            "native",
            false,
            "claude",
            "project_search_generation_1",
        ),
    )
    .unwrap();
    let runtime = ReloadableRunnerConfig::new(load_config(&path).unwrap(), path.clone());
    (tmp, path, runtime)
}

#[test]
fn reload_field_classification_is_exhaustive_and_allowlisted() {
    let startup = test_config(PathBuf::from("projects-a"));
    let mut hot_only = startup.clone();
    hot_only.policy.max_timeout_secs += 1;
    hot_only.shell.program = "bash".to_string();
    hot_only.skills.roots.push(PathBuf::from("live-skill-root"));
    hot_only.plugins.request_timeout_secs += 1;
    hot_only.tool_providers.strategy =
        webcodex_runner::config::ToolProviderStrategy::ClaudeCodeThenNative;
    assert!(webcodex_runner::config::restart_required_fields(&startup, &hot_only).is_empty());

    let mut changed = hot_only;
    changed.server_url.push_str("/other");
    changed.token.push('2');
    changed.client_id.push('2');
    changed.display_name = Some("changed".to_string());
    changed.owner = Some("changed".to_string());
    changed.hostname = Some("changed".to_string());
    changed.host_context = Some(runner_protocol::RunnerHostContext {
        role: Some("primary_development".to_string()),
        runtime: Some("Prefer this Runner for ordinary development.".to_string()),
        ..Default::default()
    });
    changed.project_registry_dir = Some(PathBuf::from("projects-b"));
    changed.poll_interval_ms += 1;
    changed.capabilities = Some(RunnerCapabilities::default());
    changed.max_concurrent_jobs = Some(4);
    changed.mcp_gateway.request_timeout_secs += 1;
    changed.transport = Some(TRANSPORT_QUIC.to_string());
    changed.websocket_connect_timeout_secs += 1;
    changed.quic = Some(quic_client_config());
    assert_eq!(
            webcodex_runner::config::restart_required_fields(&startup, &changed).join(" "),
            "capabilities client_id display_name hostname host_context max_concurrent_jobs mcp_gateway owner poll_interval_ms project_registry_dir quic server_url token transport websocket_connect_timeout_secs"
        );
}

#[test]
fn skill_roots_config_change_is_hot_reloadable_and_generation_fenced() {
    let (tmp, path, runtime) = reload_fixture();
    let old = runtime.snapshot();
    assert!(old.skills.roots.is_empty());
    let live_root = tmp.path().join("live-skills");
    let candidate = format!(
        "{}\n[skills]\nroots = [{:?}]\n",
        reload_toml(
            "oe",
            None,
            60,
            1024,
            "sh",
            "native",
            false,
            "claude",
            "project_search_generation_1",
        ),
        live_root.to_string_lossy().as_ref()
    );
    std::fs::write(&path, candidate).unwrap();

    let checked = runtime.check_config();
    assert_eq!(checked.valid, Some(true));
    assert!(!checked.restart_required);
    assert!(checked.restart_required_fields.is_empty());
    assert_eq!(checked.current_generation, Some(1));
    assert!(old.skills.roots.is_empty());

    let reloaded = runtime.reload_config(1);
    assert_eq!(reloaded.valid, Some(true));
    assert!(!reloaded.restart_required);
    assert_eq!(reloaded.current_generation, Some(2));
    let active = runtime.snapshot();
    assert_eq!(active.generation, 2);
    assert_eq!(active.skills.roots, vec![live_root]);
    assert!(old.skills.roots.is_empty());
}

#[test]
fn plugin_only_config_change_is_live_and_never_requires_runner_restart() {
    let (_tmp, path, runtime) = reload_fixture();
    let candidate = reload_toml(
        "oe",
        None,
        60,
        1024,
        "sh",
        "native",
        false,
        "claude",
        "project_search_generation_1",
    )
    .replace(
        "plugins.request_timeout_secs = 30",
        "plugins.request_timeout_secs = 31",
    );
    std::fs::write(&path, candidate).unwrap();

    let checked = runtime.check_config();
    assert_eq!(checked.valid, Some(true));
    assert!(!checked.restart_required);
    assert!(checked.restart_required_fields.is_empty());
    assert_eq!(checked.current_generation, Some(1));

    let reloaded = runtime.reload_config(1);
    assert_eq!(reloaded.valid, Some(true));
    assert!(!reloaded.restart_required);
    assert!(reloaded.restart_required_fields.is_empty());
    assert_eq!(reloaded.current_generation, Some(2));
    assert_eq!(runtime.snapshot().generation, 2);
}

#[test]
fn valid_reload_switches_one_complete_generation_and_preserves_old_snapshot() {
    let (_tmp, path, runtime) = reload_fixture();
    let old = runtime.snapshot();

    assert_eq!(
        old.external_tools.configured_search_tool_name(),
        Some("project_search_generation_1")
    );
    assert_eq!(
        old.external_tools.status().claude_code.process_state,
        "not_started"
    );

    std::fs::write(
        &path,
        reload_toml(
            "oe",
            None,
            120,
            2048,
            "bash",
            "claude_code_then_native",
            false,
            "claude",
            "project_search_generation_2",
        ),
    )
    .unwrap();
    let status = runtime.reload();
    let new = runtime.snapshot();

    assert_eq!(status.last_reload_result, "success");
    assert_eq!(status.generation, 2);
    assert!(!status.restart_required);
    assert_eq!(
        (
            old.generation,
            old.policy.max_timeout_secs,
            old.shell.program.as_str()
        ),
        (1, 60, "sh")
    );
    assert_eq!(old.external_tools.status().strategy, "native");
    assert_eq!(
        old.external_tools.configured_search_tool_name(),
        Some("project_search_generation_1")
    );
    assert_eq!(
        (
            new.policy.max_timeout_secs,
            new.policy.max_output_bytes,
            new.shell.program.as_str()
        ),
        (120, 2048, "bash")
    );
    assert_eq!(
        new.external_tools.status().strategy,
        "claude_code_then_native"
    );
    assert_eq!(
        new.external_tools.configured_search_tool_name(),
        Some("project_search_generation_2")
    );
    assert_eq!(
        new.external_tools.status().claude_code.process_state,
        "not_started"
    );
}

#[test]
fn failed_reload_keeps_generation_and_can_recover() {
    let (_tmp, path, runtime) = reload_fixture();
    let old = runtime.snapshot();

    std::fs::remove_file(&path).unwrap();
    let status = runtime.reload();
    assert_eq!(status.generation, 1);
    assert_eq!(
        status.last_reload_error_code.as_deref(),
        Some("config_read_failed")
    );
    assert!(status.last_reload_error_field.is_none());
    assert!(status.last_reload_error_reason.is_none());

    for (candidate, code, field, reason) in [
        (
            "{ invalid toml".to_string(),
            "config_parse_failed",
            None,
            None,
        ),
        (
            reload_toml(
                "oe",
                None,
                60,
                1024,
                "",
                "native",
                false,
                "claude",
                "project_search_generation_1",
            ),
            "config_validation_failed",
            None,
            None,
        ),
        (
            reload_toml(
                "oe",
                Some(999),
                60,
                1024,
                "sh",
                "native",
                false,
                "claude",
                "project_search_generation_1",
            ),
            "config_validation_failed",
            Some("max_concurrent_jobs"),
            Some("out_of_range"),
        ),
        (
            reload_toml(
                "oe",
                None,
                60,
                1024,
                "sh",
                "native",
                true,
                "",
                "project_search_generation_1",
            ),
            "provider_config_invalid",
            None,
            None,
        ),
    ] {
        std::fs::write(&path, candidate).unwrap();
        let status = runtime.reload();
        assert_eq!(status.generation, 1);
        assert_eq!(status.last_reload_result, "failure");
        assert_eq!(status.last_reload_error_code.as_deref(), Some(code));
        assert_eq!(status.last_reload_error_field.as_deref(), field);
        assert_eq!(status.last_reload_error_reason.as_deref(), reason);
    }
    assert_eq!(old.policy.max_timeout_secs, 60);
    let serialized = serde_json::to_string(&runtime.snapshot().reload_status()).unwrap();
    assert!(!serialized.contains(path.to_string_lossy().as_ref()));
    assert!(!serialized.contains("test-token"));
    assert!(!serialized.contains("999"));

    std::fs::write(
        &path,
        reload_toml(
            "oe",
            None,
            90,
            1024,
            "sh",
            "native",
            false,
            "claude",
            "project_search_generation_2",
        ),
    )
    .unwrap();
    let recovered = runtime.reload();
    assert_eq!(recovered.generation, 2);
    assert!(recovered.last_reload_error_field.is_none());
    assert!(recovered.last_reload_error_reason.is_none());
    assert_eq!(runtime.snapshot().policy.max_timeout_secs, 90);
}

#[test]
fn mixed_reload_applies_hot_fields_and_reports_static_restart_fields() {
    let (_tmp, path, runtime) = reload_fixture();
    std::fs::write(
        &path,
        reload_toml(
            "oe-new",
            Some(8),
            180,
            4096,
            "bash",
            "native",
            false,
            "claude",
            "project_search_generation_2",
        ),
    )
    .unwrap();

    let status = runtime.reload();
    let active = runtime.snapshot();
    assert_eq!(status.last_reload_result, "partial");
    assert!(status.restart_required);
    assert_eq!(
        status.restart_required_fields,
        ["client_id", "max_concurrent_jobs"]
    );
    assert_eq!(
        (
            active.policy.max_timeout_secs,
            active.policy.max_output_bytes,
            active.shell.program.as_str()
        ),
        (180, 4096, "bash")
    );
}

#[test]
fn first_class_check_validates_candidate_without_mutating_active_config() {
    let (_tmp, path, runtime) = reload_fixture();
    let before = runtime.snapshot();
    let before_status = before.reload_status();
    std::fs::write(
        &path,
        reload_toml(
            "oe-new",
            Some(8),
            180,
            4096,
            "bash",
            "native",
            false,
            "claude",
            "project_search_generation_2",
        ),
    )
    .unwrap();

    let checked = runtime.check_config();
    assert_eq!(
        checked.execution_state,
        runner_protocol::RunnerConfigExecutionState::Completed
    );
    assert_eq!(checked.valid, Some(true));
    assert_eq!(checked.current_generation, Some(1));
    assert!(checked.restart_required);
    assert_eq!(
        checked.restart_required_fields,
        vec!["client_id".to_string(), "max_concurrent_jobs".to_string()]
    );

    let after = runtime.snapshot();
    assert!(Arc::ptr_eq(&before, &after));
    assert_eq!(after.reload_status(), before_status);
    assert_eq!(after.policy.max_timeout_secs, 60);
    assert_eq!(after.shell.program, "sh");
    assert_eq!(
        after.external_tools.configured_search_tool_name(),
        Some("project_search_generation_1")
    );
}

#[test]
fn first_class_check_reports_sanitized_parse_and_structural_failures() {
    let (_tmp, path, runtime) = reload_fixture();
    let before = runtime.snapshot();

    std::fs::write(
        &path,
        "token = \"never-leak-check-secret\"\nclient_id = \"oe\"\n{ malformed toml",
    )
    .unwrap();
    let malformed = runtime.check_config();
    assert_eq!(malformed.valid, Some(false));
    assert_eq!(malformed.current_generation, Some(1));
    assert_eq!(malformed.error_code.as_deref(), Some("config_parse_failed"));
    assert!(malformed.error_field.is_none());
    assert!(malformed.error_reason.is_none());

    std::fs::write(
        &path,
        reload_toml(
            "oe",
            Some(999),
            60,
            1024,
            "sh",
            "native",
            false,
            "claude",
            "project_search_generation_1",
        ),
    )
    .unwrap();
    let structural = runtime.check_config();
    assert_eq!(structural.valid, Some(false));
    assert_eq!(structural.current_generation, Some(1));
    assert_eq!(
        structural.error_code.as_deref(),
        Some("config_validation_failed")
    );
    assert_eq!(
        structural.error_field.as_deref(),
        Some("max_concurrent_jobs")
    );
    assert_eq!(structural.error_reason.as_deref(), Some("out_of_range"));

    let serialized = format!(
        "{} {}",
        serde_json::to_string(&malformed).unwrap(),
        serde_json::to_string(&structural).unwrap()
    );
    assert!(!serialized.contains("never-leak-check-secret"));
    assert!(!serialized.contains("test-token"));
    assert!(!serialized.contains(path.to_string_lossy().as_ref()));
    assert!(!serialized.contains("999"));
    assert!(Arc::ptr_eq(&before, &runtime.snapshot()));
    assert_eq!(runtime.snapshot().reload_status().generation, 1);
    assert_eq!(
        runtime.snapshot().reload_status().last_reload_result,
        "not_attempted"
    );
}

#[test]
fn first_class_reload_applies_hot_candidate_once_and_fences_stale_generation() {
    let (_tmp, path, runtime) = reload_fixture();
    let old = runtime.snapshot();
    std::fs::write(
        &path,
        reload_toml(
            "oe",
            None,
            120,
            2048,
            "bash",
            "native",
            false,
            "claude",
            "project_search_generation_2",
        ),
    )
    .unwrap();

    let reloaded = runtime.reload_config(1);
    assert_eq!(
        reloaded.execution_state,
        runner_protocol::RunnerConfigExecutionState::Completed
    );
    assert_eq!(reloaded.valid, Some(true));
    assert_eq!(reloaded.current_generation, Some(2));
    assert!(!reloaded.restart_required);
    assert_eq!(runtime.snapshot().policy.max_timeout_secs, 120);
    assert_eq!(old.policy.max_timeout_secs, 60);

    std::fs::write(&path, "{ malformed but generation fence must win").unwrap();
    let stale = runtime.reload_config(1);
    assert_eq!(
        stale.execution_state,
        runner_protocol::RunnerConfigExecutionState::NotStarted
    );
    assert_eq!(stale.valid, None);
    assert_eq!(stale.current_generation, Some(2));
    assert_eq!(
        stale.error_code.as_deref(),
        Some("config_generation_conflict")
    );
    assert_eq!(runtime.snapshot().generation, 2);
    assert_eq!(runtime.snapshot().policy.max_timeout_secs, 120);
}

#[test]
fn first_class_invalid_reload_preserves_active_snapshot_and_generation() {
    let (_tmp, path, runtime) = reload_fixture();
    let before = runtime.snapshot();
    std::fs::write(&path, "{ invalid runner config").unwrap();

    let rejected = runtime.reload_config(1);
    assert_eq!(
        rejected.execution_state,
        runner_protocol::RunnerConfigExecutionState::Completed
    );
    assert_eq!(rejected.valid, Some(false));
    assert_eq!(rejected.current_generation, Some(1));
    assert_eq!(rejected.error_code.as_deref(), Some("config_parse_failed"));
    let after = runtime.snapshot();
    assert!(Arc::ptr_eq(&before, &after));
    assert_eq!(after.generation, 1);
    assert_eq!(after.policy.max_timeout_secs, 60);
}

#[test]
fn first_class_partial_reload_reports_exact_restart_only_fields() {
    let (_tmp, path, runtime) = reload_fixture();
    std::fs::write(
        &path,
        reload_toml(
            "oe-new",
            Some(8),
            180,
            4096,
            "bash",
            "native",
            false,
            "claude",
            "project_search_generation_2",
        ),
    )
    .unwrap();

    let result = runtime.reload_config(1);
    assert_eq!(result.valid, Some(true));
    assert_eq!(result.current_generation, Some(2));
    assert!(result.restart_required);
    assert_eq!(
        result.restart_required_fields,
        vec!["client_id".to_string(), "max_concurrent_jobs".to_string()]
    );
    let active = runtime.snapshot();
    assert_eq!(active.policy.max_timeout_secs, 180);
    assert_eq!(active.policy.max_output_bytes, 4096);
    assert_eq!(active.shell.program, "bash");
}

#[test]
fn formal_reload_and_legacy_trigger_share_one_generation_sequence() {
    let (_tmp, path, runtime) = reload_fixture();
    std::fs::write(
        &path,
        reload_toml(
            "oe",
            None,
            90,
            1024,
            "sh",
            "native",
            false,
            "claude",
            "project_search_generation_2",
        ),
    )
    .unwrap();
    assert_eq!(runtime.reload_config(1).current_generation, Some(2));

    std::fs::write(
        &path,
        reload_toml(
            "oe",
            None,
            120,
            1024,
            "sh",
            "native",
            false,
            "claude",
            "project_search_generation_3",
        ),
    )
    .unwrap();
    let legacy_trigger = runtime.reload();
    assert_eq!(legacy_trigger.generation, 3);
    assert_eq!(runtime.snapshot().generation, 3);
    assert_eq!(runtime.snapshot().policy.max_timeout_secs, 120);
}

#[cfg(unix)]
#[test]
fn unix_sighup_listener_still_triggers_authoritative_reload() {
    let _guard = test_env_lock();
    let (_tmp, path, runtime) = reload_fixture();
    let runtime = Arc::new(runtime);
    let listener = webcodex_runner::install_reload_listener(Arc::clone(&runtime)).unwrap();
    std::fs::write(
        &path,
        reload_toml(
            "oe",
            None,
            150,
            1024,
            "sh",
            "native",
            false,
            "claude",
            "project_search_sighup",
        ),
    )
    .unwrap();

    let sent = unsafe { libc::kill(libc::getpid(), libc::SIGHUP) };
    assert_eq!(sent, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while runtime.snapshot().generation == 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(runtime.snapshot().generation, 2);
    assert_eq!(runtime.snapshot().policy.max_timeout_secs, 150);
    runtime.begin_shutdown();
    listener.join().unwrap();
}
