use super::super::projects::{ListProjectsOptions, ProjectCandidate};
use super::super::*;
use super::support::*;
use crate::runner_protocol::{RunnerBuildInfo, RunnerCapabilities, RunnerRegisterRequest};

fn list_projects_call(
    client_id: Option<&str>,
    project: Option<&str>,
    query: Option<&str>,
    limit: Option<usize>,
    summary_only: bool,
) -> ToolCall {
    ToolCall::ListProjects {
        client_id: client_id.map(str::to_string),
        project: project.map(str::to_string),
        query: query.map(str::to_string),
        limit,
        summary_only,
    }
}

fn list_runners_call(
    client_id: Option<&str>,
    client_ids: Option<&[&str]>,
    include_projects: Option<bool>,
    summary_only: bool,
) -> ToolCall {
    ToolCall::ListRunners {
        client_id: client_id.map(str::to_string),
        client_ids: client_ids.map(|ids| ids.iter().map(|id| (*id).to_string()).collect()),
        include_projects,
        summary_only,
    }
}

fn runtime_status_call(client_id: Option<&str>, compact: bool) -> ToolCall {
    ToolCall::RuntimeStatus {
        compact,
        summary_only: false,
        client_id: client_id.map(str::to_string),
    }
}

async fn register_target_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    projects: Vec<crate::runner_protocol::RunnerProjectSummary>,
    build: Option<RunnerBuildInfo>,
) {
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build,
            job_concurrency_limit: Some(4),
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: format!("inst-{client_id}"),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: Some(format!("Runner {client_id}")),
            owner: None,
            hostname: Some(format!("host-{client_id}")),
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(RunnerCapabilities {
                git: true,
                async_shell_jobs: true,
                ..Default::default()
            }),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        &format!("inst-{client_id}"),
        projects,
    )
    .await;
}

async fn register_target_agent_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    auth: &crate::auth::AuthContext,
) {
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: Some(4),
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: format!("inst-{client_id}"),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities::default(),
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        &format!("inst-{client_id}"),
        vec![registered_project(
            project_id,
            &format!("/tmp/{client_id}/{project_id}"),
        )],
    )
    .await;
}

fn managed_discovery_auth(username: &str) -> crate::auth::AuthContext {
    let mut auth = auth_context(Some(username), false);
    auth.scopes = vec![
        crate::auth::SCOPE_RUNTIME_READ.to_string(),
        crate::auth::SCOPE_PROJECT_READ.to_string(),
    ];
    auth
}

async fn register_managed_target_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    owner: &str,
    project_id: &str,
    project_path: &str,
) {
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: Some(4),
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: format!("inst-{client_id}"),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: Some(format!("{owner} private runner")),
            owner: Some(owner.to_string()),
            hostname: Some(format!("{owner}-private-host")),
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(
                RunnerCapabilities::default(),
            ),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        &format!("inst-{client_id}"),
        vec![registered_project(project_id, project_path)],
    )
    .await;
}

fn large_fixture_projects(count: usize) -> Vec<crate::runner_protocol::RunnerProjectSummary> {
    (0..count)
        .map(|index| {
            let mut project = registered_project(
                &format!("project-{index:04}"),
                &format!("/tmp/large-fixture/project-{index:04}"),
            );
            project.name = Some(format!("Large Fixture Project {index:04}"));
            project.description = Some("large-fixture".to_string());
            project
        })
        .collect()
}

#[test]
fn project_candidate_staging_is_index_only() {
    let candidate = ProjectCandidate {
        runtime_id: "agent:special:project-0007".to_string(),
        client_index: 3,
        project_index: 7,
    };
    let ProjectCandidate {
        runtime_id,
        client_index,
        project_index,
    } = candidate;
    assert_eq!(runtime_id, "agent:special:project-0007");
    assert_eq!(client_index, 3);
    assert_eq!(project_index, 7);
}

#[tokio::test]
async fn list_projects_batch_job_counts_join_exact_projects_and_skip_empty_selection() {
    let runtime = test_runtime();
    register_target_agent(
        &runtime,
        "batch",
        vec![
            registered_project("a", "/tmp/batch-a"),
            registered_project("b", "/tmp/batch-b"),
        ],
        None,
    )
    .await;
    for id in ["a", "a", "b"] {
        runtime
            .runner_registry
            .start_job_with_metadata(
                crate::runner_protocol::ShellJobOpRequest {
                    op: "start".into(),
                    client_id: Some("batch".into()),
                    cwd: None,
                    command: Some("echo fixture".into()),
                    timeout_secs: Some(60),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "fixture".into(),
                webcodex_runner_registry::ShellJobStartMetadata {
                    project_id: Some(format!("agent:batch:{id}")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let listed = runtime
        .dispatch_with_auth(
            list_projects_call(Some("batch"), None, None, None, true),
            None,
        )
        .await;
    assert!(listed.success, "{:?}", listed.error);
    assert_eq!(listed.output["projects"][0]["id"], "agent:batch:a");
    assert_eq!(listed.output["projects"][0]["active_jobs"], 2);
    assert_eq!(listed.output["projects"][1]["active_jobs"], 1);
    assert_eq!(runtime.runner_registry.project_job_scan_count_for_test(), 1);
    let empty = runtime
        .dispatch_with_auth(
            list_projects_call(Some("missing"), None, None, None, true),
            None,
        )
        .await;
    assert!(empty.success);
    assert_eq!(empty.output["count"], 0);
    assert_eq!(runtime.runner_registry.project_job_scan_count_for_test(), 1);
}

#[tokio::test]
async fn list_projects_large_single_runner_inventory_preserves_linear_staging_contract() {
    for project_count in [256usize, 1024] {
        let runtime = test_runtime();
        register_target_agent(&runtime, "special", Vec::new(), None).await;
        let mut clients = runtime.runner_registry.list_runners_for_auth(None).await;
        assert_eq!(clients.len(), 1);
        clients[0].projects = large_fixture_projects(project_count);

        let exact_index = if project_count == 1024 { 999 } else { 255 };
        let exact_id = format!("agent:special:project-{exact_index:04}");
        let exact = runtime
            .list_projects_with_visible_clients_for_test(
                None,
                ListProjectsOptions {
                    project: Some(exact_id.clone()),
                    limit: Some(1),
                    summary_only: true,
                    ..Default::default()
                },
                &clients,
            )
            .await;
        assert!(exact.success, "{:?}", exact.error);
        assert_eq!(exact.output["count"], 1);
        assert_eq!(exact.output["matched_count"], 1);
        assert_eq!(exact.output["truncated"], false);
        assert_eq!(exact.output["projects"][0]["id"], exact_id);
        assert_eq!(runtime.runner_registry.project_job_scan_count_for_test(), 1);

        let broad = runtime
            .list_projects_with_visible_clients_for_test(
                None,
                ListProjectsOptions {
                    client_id: Some("special".to_string()),
                    query: Some("large-fixture".to_string()),
                    limit: Some(10),
                    summary_only: true,
                    ..Default::default()
                },
                &clients,
            )
            .await;
        assert!(broad.success, "{:?}", broad.error);
        assert_eq!(broad.output["matched_count"], project_count);
        assert_eq!(broad.output["count"], 10);
        assert_eq!(broad.output["truncated"], true);
        let broad_ids = broad.output["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        let expected_ids = (0..10)
            .map(|index| format!("agent:special:project-{index:04}"))
            .collect::<Vec<_>>();
        assert_eq!(broad_ids, expected_ids);
        assert_eq!(runtime.runner_registry.project_job_scan_count_for_test(), 2);

        let full = runtime
            .list_projects_with_visible_clients_for_test(
                None,
                ListProjectsOptions::default(),
                &clients,
            )
            .await;
        assert!(full.success, "{:?}", full.error);
        assert_eq!(full.output["matched_count"], project_count);
        assert_eq!(full.output["count"], project_count);
        assert_eq!(full.output["truncated"], false);
        assert_eq!(
            runtime.runner_registry.project_job_scan_count_for_test(),
            3,
            "a full inventory must scan Jobs once, not once per Project"
        );
        assert_eq!(
            full.output["projects"][0]["id"],
            "agent:special:project-0000"
        );
        assert_eq!(
            full.output["projects"][project_count - 1]["id"],
            format!("agent:special:project-{:04}", project_count - 1)
        );
        let first = &full.output["projects"][0];
        assert!(
            first.get("path").is_some(),
            "legacy full projection lost path: {first}"
        );
        assert!(
            first.get("allow_patch").is_some(),
            "legacy full projection lost allow_patch: {first}"
        );
        assert!(
            first.get("revision").is_some(),
            "legacy full projection lost revision: {first}"
        );
    }
}

#[tokio::test]
async fn list_projects_targets_visible_inventory_before_limit_and_compacts() {
    let runtime = test_runtime();
    let mut projects = (0..63)
        .map(|index| {
            registered_project(
                &format!("ordinary-{index:03}"),
                &format!("/tmp/ordinary-{index:03}"),
            )
        })
        .collect::<Vec<_>>();
    let mut target = registered_project("webcodex-target", "/root/git/webcodex-target");
    target.name = Some("WebCodex Target".to_string());
    target.description = Some("Focused Runtime Inventory".to_string());
    target.git_branch = Some("feat/targeted-inventory".to_string());
    target.git_head = Some("abc123".to_string());
    target.git_dirty = Some(false);
    projects.push(target);
    register_target_agent(&runtime, "special", projects, None).await;
    let mini_projects = (0..60)
        .map(|index| {
            registered_project(
                &format!("mini-ordinary-{index:03}"),
                &format!("/tmp/mini-ordinary-{index:03}"),
            )
        })
        .collect::<Vec<_>>();
    register_target_agent(&runtime, "mini", mini_projects, None).await;

    let full = runtime
        .dispatch(list_projects_call(None, None, None, None, false))
        .await;
    assert!(full.success, "{:?}", full.error);
    assert_eq!(full.output["count"], 124);

    let focused = runtime
        .dispatch(list_projects_call(
            Some("special"),
            None,
            Some("WEBCODEX"),
            Some(1),
            true,
        ))
        .await;
    assert!(focused.success, "{:?}", focused.error);
    assert_eq!(focused.output["matched_count"], 1);
    assert_eq!(focused.output["count"], 1);
    assert_eq!(focused.output["truncated"], false);
    let project = &focused.output["projects"][0];
    assert_eq!(project["id"], "agent:special:webcodex-target");
    for omitted in [
        "path",
        "revision",
        "last_seen",
        "allow_patch",
        "shell_profile",
    ] {
        assert!(
            project.get(omitted).is_none(),
            "summary_only leaked {omitted}: {project}"
        );
    }
    let capabilities = project["capabilities"].as_object().unwrap();
    assert_eq!(capabilities.len(), 2);
    assert!(capabilities.contains_key("git_available"));
    assert!(capabilities.contains_key("recommended_for_smoke"));

    let exact = runtime
        .dispatch(list_projects_call(
            None,
            Some("agent:special:webcodex-target"),
            None,
            Some(1),
            false,
        ))
        .await;
    assert!(exact.success, "{:?}", exact.error);
    assert_eq!(exact.output["count"], 1);
    assert_eq!(
        exact.output["projects"][0]["id"],
        "agent:special:webcodex-target"
    );

    let wrong_runner = runtime
        .dispatch(list_projects_call(
            Some("mini"),
            Some("agent:special:webcodex-target"),
            None,
            Some(1),
            false,
        ))
        .await;
    assert!(wrong_runner.success);
    assert_eq!(wrong_runner.output["count"], 0);

    let again = runtime
        .dispatch(list_projects_call(
            Some("special"),
            None,
            Some("webcodex"),
            Some(1),
            true,
        ))
        .await;
    assert_eq!(focused.output, again.output);

    let bounded_query = runtime
        .dispatch(list_projects_call(None, None, Some("ordinary"), None, true))
        .await;
    assert!(bounded_query.success);
    assert_eq!(bounded_query.output["matched_count"], 123);
    assert_eq!(bounded_query.output["count"], 100);
    assert_eq!(bounded_query.output["truncated"], true);

    for query in ["   ".to_string(), "x".repeat(201)] {
        let invalid = runtime
            .dispatch(list_projects_call(None, None, Some(&query), None, false))
            .await;
        assert!(!invalid.success);
        assert_eq!(invalid.output["error_kind"], "invalid_query");
    }
}

#[tokio::test]
async fn list_projects_filters_only_after_authorization_visibility() {
    let runtime = test_runtime();
    let auth_a = shared_key_auth_context("targeted-a");
    let auth_b = shared_key_auth_context("targeted-b");
    register_target_agent_for_auth(&runtime, "client-a", "visible-a", &auth_a).await;
    register_target_agent_for_auth(&runtime, "client-b", "private-b", &auth_b).await;

    for call in [
        list_projects_call(None, Some("agent:client-b:private-b"), None, None, false),
        list_projects_call(None, None, Some("private-b"), None, false),
        list_projects_call(Some("client-b"), None, None, None, false),
    ] {
        let result = runtime.dispatch_with_auth(call, Some(&auth_a)).await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["count"], 0);
        assert_eq!(result.output["matched_count"], 0);
    }

    let hidden_agent = runtime
        .dispatch_with_auth(
            list_runners_call(Some("client-b"), None, Some(false), true),
            Some(&auth_a),
        )
        .await;
    assert!(hidden_agent.success);
    assert_eq!(hidden_agent.output["count"], 0);

    let hidden_status = runtime
        .dispatch_with_auth(runtime_status_call(Some("client-b"), true), Some(&auth_a))
        .await;
    assert!(!hidden_status.success);
    assert_eq!(hidden_status.output["error_kind"], "unknown_client_id");
}

#[tokio::test]
async fn managed_users_discover_only_their_own_runner_project_metadata() {
    let runtime = test_runtime();
    let alice = managed_discovery_auth("alice");
    let bob = managed_discovery_auth("bob");
    let bootstrap = bootstrap_auth_context();

    register_managed_target_agent(
        &runtime,
        "alice-laptop",
        "alice",
        "common",
        "/home/alice/laptop/common",
    )
    .await;
    register_managed_target_agent(
        &runtime,
        "alice-server",
        "alice",
        "common",
        "/srv/alice/common",
    )
    .await;
    register_managed_target_agent(
        &runtime,
        "bob-runner",
        "bob",
        "bob-private",
        "/home/bob/private",
    )
    .await;

    let alice_agents = runtime
        .dispatch_with_auth(
            list_runners_call(None, None, Some(true), false),
            Some(&alice),
        )
        .await;
    assert!(alice_agents.success, "{:?}", alice_agents.error);
    assert_eq!(alice_agents.output["count"], 2);
    let alice_agents_text = alice_agents.output.to_string();
    assert!(!alice_agents_text.contains("bob-runner"));
    assert!(!alice_agents_text.contains("bob-private"));
    assert!(!alice_agents_text.contains("/home/bob/private"));
    assert!(!alice_agents_text.contains("bob-private-host"));

    let alice_projects = runtime
        .dispatch_with_auth(
            list_projects_call(None, None, None, None, false),
            Some(&alice),
        )
        .await;
    assert!(alice_projects.success, "{:?}", alice_projects.error);
    assert_eq!(alice_projects.output["count"], 2);
    assert!(!alice_projects.output.to_string().contains("bob-private"));

    for call in [
        list_projects_call(
            None,
            Some("agent:bob-runner:bob-private"),
            None,
            None,
            false,
        ),
        list_projects_call(None, None, Some("bob-private"), None, false),
        list_projects_call(Some("bob-runner"), None, None, None, false),
    ] {
        let hidden = runtime.dispatch_with_auth(call, Some(&alice)).await;
        assert!(hidden.success, "{:?}", hidden.error);
        assert_eq!(hidden.output["count"], 0);
        assert_eq!(hidden.output["matched_count"], 0);
    }

    let hidden_agent = runtime
        .dispatch_with_auth(
            list_runners_call(Some("bob-runner"), None, Some(false), true),
            Some(&alice),
        )
        .await;
    assert!(hidden_agent.success);
    assert_eq!(hidden_agent.output["count"], 0);

    let alice_status = runtime
        .dispatch_with_auth(runtime_status_call(None, true), Some(&alice))
        .await;
    assert!(alice_status.success, "{:?}", alice_status.error);
    assert_eq!(alice_status.output["agents"]["count"], 2);
    assert!(!alice_status.output.to_string().contains("bob-runner"));
    let hidden_status = runtime
        .dispatch_with_auth(runtime_status_call(Some("bob-runner"), true), Some(&alice))
        .await;
    assert!(!hidden_status.success);
    assert_eq!(hidden_status.output["error_kind"], "unknown_client_id");

    let ambiguous = runtime
        .resolve_project_input_for_auth("common", Some(&alice))
        .await
        .unwrap_err();
    assert_eq!(
        ambiguous.kind,
        super::super::project_resolution::ProjectResolverErrorKind::AmbiguousProject
    );
    assert_eq!(ambiguous.candidates.len(), 2);
    assert!(ambiguous
        .candidates
        .iter()
        .all(|candidate| candidate.client_id.starts_with("alice-")));
    assert!(!ambiguous.to_message().contains("bob"));

    let bob_agents = runtime
        .dispatch_with_auth(list_runners_call(None, None, Some(true), false), Some(&bob))
        .await;
    assert!(bob_agents.success, "{:?}", bob_agents.error);
    assert_eq!(bob_agents.output["count"], 1);
    assert_eq!(bob_agents.output["agents"][0]["client_id"], "bob-runner");

    let admin_agents = runtime
        .dispatch_with_auth(
            list_runners_call(None, None, Some(false), true),
            Some(&bootstrap),
        )
        .await;
    assert!(admin_agents.success, "{:?}", admin_agents.error);
    assert_eq!(admin_agents.output["count"], 3);
}

#[tokio::test]
async fn list_runners_supports_exact_batch_and_compact_projection() {
    let runtime = test_runtime();
    register_target_agent(
        &runtime,
        "sf",
        vec![registered_project("server", "/tmp/server")],
        None,
    )
    .await;
    register_target_agent(
        &runtime,
        "special",
        vec![
            registered_project("webcodex", "/root/git/webcodex"),
            registered_project("other", "/tmp/other"),
        ],
        None,
    )
    .await;
    register_target_agent(
        &runtime,
        "mini",
        vec![registered_project("mini-proj", "/tmp/mini")],
        None,
    )
    .await;

    let legacy = runtime
        .dispatch(list_runners_call(None, None, None, false))
        .await;
    assert!(legacy.success);
    assert_eq!(legacy.output["count"], 3);
    assert!(legacy.output["agents"][0].get("projects").is_some());

    let focused = runtime
        .dispatch(list_runners_call(Some("special"), None, Some(false), true))
        .await;
    assert!(focused.success, "{:?}", focused.error);
    assert_eq!(focused.output["count"], 1);
    assert!(focused.output.get("clients").is_none());
    let agent = &focused.output["agents"][0];
    assert_eq!(agent["client_id"], "special");
    assert_eq!(agent["projects_count"], 2);
    for omitted in [
        "projects",
        "capabilities",
        "host_context",
        "policy",
        "shell_profiles",
    ] {
        assert!(
            agent.get(omitted).is_none(),
            "summary_only leaked {omitted}: {agent}"
        );
    }

    let batch = runtime
        .dispatch(list_runners_call(
            None,
            Some(&["special", "mini"]),
            Some(false),
            false,
        ))
        .await;
    assert!(batch.success);
    let ids = batch.output["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["client_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["mini", "special"]);
    assert!(batch.output["agents"]
        .as_array()
        .unwrap()
        .iter()
        .all(|agent| agent.get("projects").is_none()));

    let duplicate = runtime
        .dispatch(list_runners_call(
            None,
            Some(&["special", "special"]),
            None,
            false,
        ))
        .await;
    assert!(!duplicate.success);
    assert_eq!(duplicate.output["error_kind"], "invalid_client_ids");

    let too_many_ids = (0..9)
        .map(|index| format!("client-{index}"))
        .collect::<Vec<_>>();
    let too_many = runtime
        .dispatch(ToolCall::ListRunners {
            client_id: None,
            client_ids: Some(too_many_ids),
            include_projects: None,
            summary_only: false,
        })
        .await;
    assert!(!too_many.success);
    assert_eq!(too_many.output["error_kind"], "invalid_client_ids");

    let unknown = runtime
        .dispatch(list_runners_call(Some("not-registered"), None, None, false))
        .await;
    assert!(unknown.success);
    assert_eq!(unknown.output["count"], 0);
}

#[tokio::test]
async fn runtime_status_focus_is_not_polluted_by_unrelated_runner_mismatch() {
    let runtime = test_runtime();
    let special_build = RunnerBuildInfo {
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        git_commit: Some("A".to_string()),
        git_dirty: Some(false),
    };
    let mini_build = RunnerBuildInfo {
        version: Some("0.0.1".to_string()),
        git_commit: Some("B".to_string()),
        git_dirty: Some(false),
    };
    register_target_agent(
        &runtime,
        "special",
        vec![registered_project("webcodex", "/tmp/webcodex")],
        Some(special_build),
    )
    .await;
    register_target_agent(
        &runtime,
        "mini",
        vec![registered_project("mini", "/tmp/mini")],
        Some(mini_build),
    )
    .await;

    let visible_clients = runtime.runner_registry.list_runners_for_auth(None).await;
    let synthetic = super::super::runtime_info::version_compatibility_for_test(
        &visible_clients,
        env!("CARGO_PKG_VERSION"),
        Some("A"),
        Some(false),
    );
    assert_eq!(synthetic["source_alignment"]["status"], "different");
    let synthetic_runners = synthetic["runners"].as_array().unwrap();
    let synthetic_special = synthetic_runners
        .iter()
        .find(|runner| runner["client_id"] == "special")
        .unwrap();
    let synthetic_mini = synthetic_runners
        .iter()
        .find(|runner| runner["client_id"] == "mini")
        .unwrap();
    assert_eq!(synthetic_special["source_alignment"]["status"], "aligned");
    assert_eq!(synthetic_mini["source_alignment"]["status"], "different");

    let global = runtime.dispatch(runtime_status_call(None, false)).await;
    assert!(global.success);
    assert_eq!(
        global.output["version_compatibility"]["status"],
        "version_mismatch"
    );
    let global_special = global.output["version_compatibility"]["runners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|runner| runner["client_id"] == "special")
        .unwrap();

    let special = runtime
        .dispatch(runtime_status_call(Some("special"), true))
        .await;
    assert!(special.success, "{:?}", special.error);
    assert_eq!(special.output["focus"]["client_id"], "special");
    assert_eq!(
        special.output["version_compatibility"]["status"],
        "compatible"
    );
    assert_eq!(
        special.output["focus"]["source_alignment"],
        global_special["source_alignment"]
    );
    assert_eq!(
        special.output["fleet_summary"]["mismatched_agents_count"],
        1
    );
    let serialized = special.output.to_string();
    assert!(!serialized.contains("inst-mini"));
    assert!(!serialized.contains("/tmp/mini"));

    let mini = runtime
        .dispatch(runtime_status_call(Some("mini"), true))
        .await;
    assert!(mini.success);
    assert_eq!(mini.output["focus"]["client_id"], "mini");
    assert_eq!(
        mini.output["version_compatibility"]["status"],
        "version_mismatch"
    );

    let unknown = runtime
        .dispatch(runtime_status_call(Some("missing"), true))
        .await;
    assert!(!unknown.success);
    assert_eq!(unknown.output["error_kind"], "unknown_client_id");
}

#[tokio::test]
async fn runtime_status_focus_preserves_selected_stale_runner_truth() {
    let runtime = test_runtime();
    register_target_agent(
        &runtime,
        "special",
        vec![registered_project("webcodex", "/tmp/webcodex")],
        None,
    )
    .await;
    runtime
        .runner_registry
        .set_last_seen_for_test("special", chrono::Utc::now().timestamp() - 120)
        .await;

    let focused = runtime
        .dispatch(runtime_status_call(Some("special"), false))
        .await;
    assert!(focused.success, "{:?}", focused.error);
    assert_eq!(focused.output["focus"]["connected"], false);
    assert_eq!(focused.output["focus"]["status"], "stale");
    assert_eq!(focused.output["agents"]["count"], 1);
}

#[test]
fn targeted_inventory_schemas_and_tool_parsing_are_bounded() {
    let specs = registered_tool_specs();
    let project_spec = specs
        .iter()
        .find(|spec| spec.name == "list_projects")
        .unwrap();
    let project_props = project_spec.input_schema["properties"].as_object().unwrap();
    assert_eq!(project_props["query"]["maxLength"], 200);
    assert!(project_props["limit"].get("maximum").is_none());
    assert!(project_props["limit"]["description"]
        .as_str()
        .is_some_and(|description| description.contains("clamped to 100")));
    assert!(project_spec.description.contains("exact client_id/project"));

    let agent_spec = specs
        .iter()
        .find(|spec| spec.name == "list_runners")
        .unwrap();
    let agent_props = agent_spec.input_schema["properties"].as_object().unwrap();
    assert_eq!(agent_props["client_ids"]["minItems"], 1);
    assert_eq!(agent_props["client_ids"]["maxItems"], 8);
    assert_eq!(agent_props["client_ids"]["uniqueItems"], true);

    let status_spec = specs
        .iter()
        .find(|spec| spec.name == "runtime_status")
        .unwrap();
    assert_eq!(
        status_spec.input_schema["properties"]["client_id"]["maxLength"],
        128
    );
    assert!(status_spec.description.contains("exact client_id"));

    let jobs_spec = specs.iter().find(|spec| spec.name == "list_jobs").unwrap();
    let job_props = jobs_spec.input_schema["properties"].as_object().unwrap();
    assert!(job_props.contains_key("project"));
    assert!(job_props.contains_key("session_id"));
    assert!(job_props["limit"].get("maximum").is_none());
    assert!(job_props["limit"]["description"]
        .as_str()
        .is_some_and(|description| description.contains("clamped to 100")));
    assert!(jobs_spec.description.contains("project/session_id"));

    let projects = ToolCall::from_tool_name(
        "list_projects",
        serde_json::json!({
            "client_id": "special",
            "project": "agent:special:webcodex",
            "query": "webcodex",
            "limit": 3,
            "summary_only": true,
        }),
    )
    .unwrap();
    assert!(matches!(
        projects,
        ToolCall::ListProjects {
            client_id: Some(_),
            project: Some(_),
            query: Some(_),
            limit: Some(3),
            summary_only: true,
        }
    ));

    let agents = ToolCall::from_tool_name(
        "list_runners",
        serde_json::json!({
            "client_ids": ["special", "mini"],
            "include_projects": false,
            "summary_only": true,
        }),
    )
    .unwrap();
    assert!(matches!(
        agents,
        ToolCall::ListRunners {
            client_id: None,
            client_ids: Some(_),
            include_projects: Some(false),
            summary_only: true,
        }
    ));

    let status = ToolCall::from_tool_name(
        "runtime_status",
        serde_json::json!({"client_id": "special", "compact": true}),
    )
    .unwrap();
    assert!(matches!(
        status,
        ToolCall::RuntimeStatus {
            client_id: Some(_),
            compact: true,
            summary_only: false,
        }
    ));

    let jobs = ToolCall::from_tool_name(
        "list_jobs",
        serde_json::json!({
            "project": "agent:special:webcodex",
            "session_id": "wc_sess_example",
            "status": "running",
            "limit": 2,
        }),
    )
    .unwrap();
    assert!(matches!(
        jobs,
        ToolCall::ListJobs {
            limit: Some(2),
            status: Some(_),
            project: Some(_),
            session_id: Some(_),
        }
    ));
}

#[test]
fn targeted_inventory_tool_calls_reject_unknown_filter_fields() {
    for (tool, arguments, typo) in [
        (
            "list_projects",
            serde_json::json!({"clinet_id": "special"}),
            "clinet_id",
        ),
        (
            "list_runners",
            serde_json::json!({"clinet_id": "special"}),
            "clinet_id",
        ),
        (
            "runtime_status",
            serde_json::json!({"clinet_id": "special"}),
            "clinet_id",
        ),
        (
            "list_jobs",
            serde_json::json!({"sesion_id": "wc_sess_example"}),
            "sesion_id",
        ),
    ] {
        let error = ToolCall::from_tool_name(tool, arguments)
            .expect_err("unknown targeted-inventory fields must not widen the query");
        assert!(error.contains("unknown field"), "{tool}: {error}");
        assert!(error.contains(typo), "{tool}: {error}");
    }
}

#[test]
fn targeted_inventory_audit_summaries_do_not_persist_raw_query_or_id_filters() {
    let query = "/private/worktree/needle";
    let raw = serde_json::json!({
        "client_id": "special",
        "project": "agent:special:secret-project",
        "query": query,
        "limit": 4,
        "summary_only": true,
    });
    let summary =
        super::super::tool_audit::session_log_arguments_for_tool_request("list_projects", &raw);
    assert_eq!(summary["client_id_present"], true);
    assert_eq!(summary["project_present"], true);
    assert_eq!(summary["query_present"], true);
    assert_eq!(summary["query_length"], query.chars().count());
    let serialized = summary.to_string();
    assert!(!serialized.contains(query));
    assert!(!serialized.contains("secret-project"));
    assert!(!serialized.contains("special"));

    let defensive = super::super::sessions::session_input_summary_for_tool("list_projects", &raw);
    let defensive_text = defensive.to_string();
    assert!(!defensive_text.contains(query));
    assert!(!defensive_text.contains("secret-project"));
}
