use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use webcodex_admin::{build_server_http_client, ServerHttpOptions};

use super::super::connections::{canonical_server_url, ensure_real_directory_tree};
use super::super::http::{post_json_authed, ApiCall};
use super::super::profiles::{
    client_output_dir_for_profile, client_state_dir_for_profile, default_client_base_dir,
    default_client_state_base_dir, validate_client_profile,
};
use super::process::{local_runner_state_summary, stop_runner_unlocked};
use super::profile::{
    read_existing_runner_config, read_project_files, stored_project_matches,
    validate_existing_regular_file, ExistingRunnerConfig, ProfileLock, ProjectFile,
};

const MAX_AMBIGUOUS_PROFILES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisconnectOptions {
    pub(crate) project: PathBuf,
    pub(crate) profile: Option<String>,
    pub(crate) config_base: Option<PathBuf>,
    pub(crate) state_base: Option<PathBuf>,
    pub(crate) server_http: ServerHttpOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisconnectResult {
    pub(crate) outcome: String,
    pub(crate) profile: String,
    pub(crate) project: PathBuf,
    pub(crate) project_id: String,
    pub(crate) runner_action: String,
}

impl DisconnectResult {
    pub(crate) fn render(&self) -> String {
        format!(
            "Disconnected WebCodex project.\n  outcome: {}\n  profile: {}\n  project: {}\n  project_id: {}\n  runner: {}\n",
            self.outcome,
            self.profile,
            self.project.display(),
            self.project_id,
            self.runner_action
        )
    }
}

#[derive(Debug, Clone)]
struct ProjectRegistration {
    profile: String,
    profile_dir: PathBuf,
    state_dir: PathBuf,
    project_path: PathBuf,
    project: ProjectFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExactRegistrationObservation {
    Present,
    Absent,
    Unknown,
}

#[derive(Debug)]
enum LiveUnregisterError {
    NoUnregister(String),
    OutcomeUnknown(&'static str),
}

pub(crate) async fn run_disconnect(opts: DisconnectOptions) -> Result<DisconnectResult, String> {
    let canonical_project = opts.project.canonicalize().map_err(|error| {
        format!(
            "project path {} does not exist or cannot be resolved: {error}",
            opts.project.display()
        )
    })?;
    if !canonical_project.is_dir() {
        return Err(format!(
            "project path {} is not a directory",
            canonical_project.display()
        ));
    }
    let explicit_profile = opts
        .profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?;
    let config_base = opts
        .config_base
        .clone()
        .map(Ok)
        .unwrap_or_else(default_client_base_dir)?;
    let state_base = opts
        .state_base
        .clone()
        .map(Ok)
        .unwrap_or_else(default_client_state_base_dir)?;
    if !config_base.is_dir() || !state_base.is_dir() {
        return Err(format!(
            "no hosted `webcodex connect` profile registers project {}",
            canonical_project.display()
        ));
    }
    let config_base = ensure_real_directory_tree(&config_base)?;
    let state_base = ensure_real_directory_tree(&state_base)?;

    let candidate = resolve_registration(
        &config_base,
        &state_base,
        &canonical_project,
        explicit_profile.as_deref(),
    )?;
    let _lock = ProfileLock::acquire(&candidate.state_dir)?;

    // Re-read under the same profile lock used by connect. The exact TOML must
    // still be a regular file and still name this canonical repository before
    // either the remote or local registration is changed.
    validate_existing_regular_file(&candidate.project_path)?;
    let project_registry_dir =
        webcodex_runner_config::paths::select_project_registry_dir(&candidate.profile_dir)?;
    let current_projects = read_project_files(&project_registry_dir)?;
    let current = current_projects
        .iter()
        .find(|(path, project)| {
            path == &candidate.project_path && stored_project_matches(project, &canonical_project)
        })
        .ok_or_else(|| {
            "hosted project registration changed while disconnect was starting; retry after inspecting the profile"
                .to_string()
        })?;
    if current.1.id != candidate.project.id {
        return Err(
            "hosted project registration changed while disconnect was starting; refusing to guess"
                .to_string(),
        );
    }

    let config_path =
        webcodex_runner_config::paths::resolve_runner_config_path(&candidate.profile_dir)?;
    let config = read_existing_runner_config(&config_path)?
        .ok_or_else(|| format!("hosted profile {} has no Runner config", candidate.profile))?;
    let runtime_project_id = format!("agent:{}:{}", config.client_id, current.1.id);
    let runner = local_runner_state_summary(&candidate.state_dir)?;
    if !runner.managed {
        return Err(format!(
            "profile {} is not a hosted `webcodex connect` profile",
            candidate.profile
        ));
    }

    let outcome = if runner.running {
        let observer_token = super::oauth::observer_token_for_disconnect(
            &candidate.profile_dir,
            &config_base,
            &config,
        )?
        .unwrap_or_else(|| config.token.clone());
        let remote_outcome = match unregister_live_project(
            &config,
            &opts.server_http,
            &runtime_project_id,
            &observer_token,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(LiveUnregisterError::NoUnregister(error)) => return Err(error),
            Err(LiveUnregisterError::OutcomeUnknown(reason)) => {
                let observation = observe_exact_registration(&candidate, &canonical_project);
                return Err(render_uncertain_unregister_error(reason, observation));
            }
        };
        // The Runner's authoritative unregister normally removes this exact
        // registration before the terminal response reaches the CLI. Re-observe
        // under the still-held ProfileLock: accept an already-absent file, remove
        // the exact file only if it is still present, and fail closed if the path
        // can no longer be identified safely.
        match observe_exact_registration(&candidate, &canonical_project) {
            ExactRegistrationObservation::Present => {
                remove_exact_registration(&candidate.project_path)?;
            }
            ExactRegistrationObservation::Absent => {}
            ExactRegistrationObservation::Unknown => {
                return Err(
                    "Server/Runner reported a terminal unregister outcome, but the exact local registration state could not be safely observed; no local cleanup or Runner stop was attempted"
                        .to_string(),
                );
            }
        }
        remote_outcome
    } else {
        remove_exact_registration(&candidate.project_path)?;
        "local_unregistered".to_string()
    };

    let remaining = read_project_files(&project_registry_dir)?.len();
    let runner_action =
        runner_action_after_disconnect(runner.running, remaining, &candidate.state_dir);

    Ok(DisconnectResult {
        outcome,
        profile: candidate.profile,
        project: canonical_project,
        project_id: runtime_project_id,
        runner_action: runner_action.to_string(),
    })
}

fn runner_action_after_disconnect(
    runner_running: bool,
    remaining: usize,
    state_dir: &Path,
) -> &'static str {
    if runner_running && remaining == 0 {
        return match stop_runner_unlocked(state_dir) {
            Ok(true) => "stopped",
            Ok(false) => "already_stopped",
            Err(_) => "stop_failed",
        };
    }
    if runner_running {
        "kept_running"
    } else {
        "not_running"
    }
}

fn resolve_registration(
    config_base: &Path,
    state_base: &Path,
    canonical_project: &Path,
    explicit_profile: Option<&str>,
) -> Result<ProjectRegistration, String> {
    let names = if let Some(profile) = explicit_profile {
        vec![profile.to_string()]
    } else {
        let clients = config_base.join("clients");
        let mut names = match std::fs::read_dir(&clients) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                .filter(|name| validate_client_profile(name).is_ok())
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(format!(
                    "failed to inspect hosted profiles {}: {error}",
                    clients.display()
                ))
            }
        };
        names.sort();
        names
    };

    let mut matches = Vec::new();
    for profile in names {
        let profile_dir = client_output_dir_for_profile(config_base, &profile);
        let state_dir = client_state_dir_for_profile(state_base, &profile);
        let marker = super::process::local_runner_profile_marker(&state_dir);
        if !marker.exists() {
            if explicit_profile.is_some() {
                return Err(format!(
                    "profile {profile} is not a hosted `webcodex connect` profile"
                ));
            }
            continue;
        }
        validate_existing_regular_file(&marker)?;
        let project_registry_dir =
            webcodex_runner_config::paths::select_project_registry_dir(&profile_dir)?;
        let project_matches = read_project_files(&project_registry_dir)?
            .into_iter()
            .filter(|(_, project)| stored_project_matches(project, canonical_project))
            .collect::<Vec<_>>();
        if project_matches.len() > 1 {
            return Err(format!(
                "profile {profile} contains more than one registration for the same canonical repository; refusing to guess"
            ));
        }
        if let Some((project_path, project)) = project_matches.into_iter().next() {
            matches.push(ProjectRegistration {
                profile,
                profile_dir,
                state_dir,
                project_path,
                project,
            });
        } else if explicit_profile.is_some() {
            return Err(format!(
                "profile {profile} does not register project {}",
                canonical_project.display()
            ));
        }
    }
    if matches.len() > 1 {
        let mut names = matches
            .iter()
            .map(|candidate| candidate.profile.clone())
            .collect::<Vec<_>>();
        names.sort();
        let shown = names
            .iter()
            .take(MAX_AMBIGUOUS_PROFILES)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if names.len() > MAX_AMBIGUOUS_PROFILES {
            format!(", +{} more", names.len() - MAX_AMBIGUOUS_PROFILES)
        } else {
            String::new()
        };
        return Err(format!(
            "more than one hosted profile registers this exact repository ({shown}{suffix}); rerun with --profile NAME"
        ));
    }
    matches.pop().ok_or_else(|| {
        format!(
            "no hosted `webcodex connect` profile registers project {}",
            canonical_project.display()
        )
    })
}

async fn unregister_live_project(
    config: &ExistingRunnerConfig,
    server_http: &ServerHttpOptions,
    runtime_project_id: &str,
    observer_token: &str,
) -> Result<String, LiveUnregisterError> {
    let server =
        canonical_server_url(&config.server_url).map_err(LiveUnregisterError::NoUnregister)?;
    let list = post_json_authed(ApiCall {
        server_url: &server.url,
        server_http,
        token: observer_token,
        path: "/api/projects/list",
        body: json!({}),
    })
    .await
    .map_err(LiveUnregisterError::NoUnregister)?;
    let projects = list
        .pointer("/output/projects")
        .or_else(|| list.get("projects"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            LiveUnregisterError::NoUnregister(
                "Server returned an invalid project inventory; unregister was not dispatched"
                    .to_string(),
            )
        })?;
    let project = projects
        .iter()
        .find(|project| project.get("id").and_then(Value::as_str) == Some(runtime_project_id))
        .ok_or_else(|| {
            LiveUnregisterError::NoUnregister(
                "live Runner project is not present in the authenticated Server inventory; unregister was not dispatched"
                    .to_string(),
            )
        })?;
    let expected_revision = project
        .get("revision")
        .and_then(Value::as_str)
        .filter(|revision| valid_revision(revision))
        .ok_or_else(|| {
            LiveUnregisterError::NoUnregister(
                "Server project inventory omitted a valid revision; unregister was not dispatched"
                    .to_string(),
            )
        })?;
    let response = post_live_unregister(
        &server.url,
        server_http,
        observer_token,
        runtime_project_id,
        expected_revision,
    )
    .await?;
    if response.get("project").and_then(Value::as_str) != Some(runtime_project_id) {
        return Err(LiveUnregisterError::OutcomeUnknown(
            "Server unregister response did not identify the requested project",
        ));
    }
    let outcome = response
        .get("outcome")
        .and_then(Value::as_str)
        .filter(|outcome| matches!(*outcome, "unregistered" | "already_unregistered"))
        .ok_or(LiveUnregisterError::OutcomeUnknown(
            "Server did not return a terminal unregister outcome",
        ))?;
    Ok(outcome.to_string())
}

async fn post_live_unregister(
    server_url: &str,
    server_http: &ServerHttpOptions,
    token: &str,
    runtime_project_id: &str,
    expected_revision: &str,
) -> Result<Value, LiveUnregisterError> {
    let url = format!(
        "{}/api/projects/unregister",
        server_url.trim_end_matches('/')
    );
    let client = build_server_http_client(server_http).map_err(|_| {
        LiveUnregisterError::NoUnregister(
            "failed to configure Server HTTP client; unregister was not dispatched".to_string(),
        )
    })?;
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(&json!({
            "project": runtime_project_id,
            "expected_revision": expected_revision,
        }))
        .send()
        .await
        .map_err(|_| {
            LiveUnregisterError::OutcomeUnknown(
                "Server transport did not return an unregister response",
            )
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|_| {
        LiveUnregisterError::OutcomeUnknown("Server unregister response could not be read")
    })?;
    if !status.is_success() {
        let parsed = serde_json::from_str::<Value>(&text).ok();
        let code = parsed
            .as_ref()
            .and_then(|value| value.pointer("/error/code"))
            .and_then(Value::as_str);
        if matches!(code, Some("operation_indeterminate" | "operation_failed")) {
            return Err(LiveUnregisterError::OutcomeUnknown(
                "Server could not prove a terminal unregister outcome",
            ));
        }
        if let Some(code) = code.filter(|code| known_no_unregister_error(code)) {
            return Err(LiveUnregisterError::NoUnregister(format!(
                "Server rejected unregister without a project removal outcome: {code}"
            )));
        }
        if matches!(status.as_u16(), 400 | 401 | 403) {
            return Err(LiveUnregisterError::NoUnregister(format!(
                "Server rejected unregister before Runner dispatch: HTTP {}",
                status.as_u16()
            )));
        }
        return Err(LiveUnregisterError::OutcomeUnknown(
            "Server returned an unclassified unregister failure",
        ));
    }
    serde_json::from_str(&text).map_err(|_| {
        LiveUnregisterError::OutcomeUnknown("Server unregister response was not valid JSON")
    })
}

fn known_no_unregister_error(code: &str) -> bool {
    matches!(
        code,
        "active_jobs_conflict"
            | "agent_unavailable"
            | "unsupported_runner_version"
            | "revision_conflict"
            | "project_not_found"
            | "invalid_request"
    )
}

fn observe_exact_registration(
    candidate: &ProjectRegistration,
    canonical_project: &Path,
) -> ExactRegistrationObservation {
    let metadata = match std::fs::symlink_metadata(&candidate.project_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ExactRegistrationObservation::Absent
        }
        Err(_) => return ExactRegistrationObservation::Unknown,
    };
    if metadata.is_symlink() || !metadata.is_file() {
        return ExactRegistrationObservation::Unknown;
    }
    let content = match std::fs::read_to_string(&candidate.project_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ExactRegistrationObservation::Absent
        }
        Err(_) => return ExactRegistrationObservation::Unknown,
    };
    let observed: ProjectFile = match toml::from_str(&content) {
        Ok(project) => project,
        Err(_) => return ExactRegistrationObservation::Unknown,
    };
    if observed.id == candidate.project.id && stored_project_matches(&observed, canonical_project) {
        ExactRegistrationObservation::Present
    } else {
        ExactRegistrationObservation::Unknown
    }
}

fn render_uncertain_unregister_error(
    reason: &str,
    observation: ExactRegistrationObservation,
) -> String {
    match observation {
        ExactRegistrationObservation::Present => format!(
            "disconnect outcome unknown after live structured unregister: {reason}; exact local registration is still present; no retry or recreation was attempted"
        ),
        ExactRegistrationObservation::Absent => format!(
            "disconnect outcome unknown after live structured unregister: {reason}; exact local registration is no longer present; Server inventory state was not inferred and no retry or recreation was attempted"
        ),
        ExactRegistrationObservation::Unknown => format!(
            "disconnect outcome unknown after live structured unregister: {reason}; exact local registration state could not be safely observed; Server inventory state was not inferred and no retry or recreation was attempted"
        ),
    }
}

fn valid_revision(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn remove_exact_registration(path: &Path) -> Result<(), String> {
    validate_existing_regular_file(path)?;
    std::fs::remove_file(path).map_err(|error| {
        format!(
            "failed to remove hosted project registration {}: {error}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webcodex_cli::test_support::canonical_test_tempdir;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn write_profile(
        config_base: &Path,
        state_base: &Path,
        profile: &str,
        project: &Path,
        project_id: &str,
    ) -> (PathBuf, PathBuf) {
        let profile_dir = client_output_dir_for_profile(config_base, profile);
        let state_dir = client_state_dir_for_profile(state_base, profile);
        std::fs::create_dir_all(profile_dir.join("project-registry")).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            profile_dir.join("runner.toml"),
            "server_url = \"http://127.0.0.1:1\"\ntoken = \"shared-key\"\nclient_id = \"client\"\n",
        )
        .unwrap();
        std::fs::write(
            profile_dir
                .join("project-registry")
                .join(format!("{project_id}.toml")),
            format!(
                "id = \"{project_id}\"\npath = {:?}\n",
                project.to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(
            super::super::process::local_runner_profile_marker(&state_dir),
            "profile = \"test\"\n",
        )
        .unwrap();
        (profile_dir, state_dir)
    }

    #[test]
    fn canonical_matching_and_multi_profile_ambiguity_are_exact() {
        let tmp = canonical_test_tempdir();
        let config = tmp.path().join("config");
        let state = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(config.join("clients")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        write_profile(&config, &state, "one", &project, "repo");
        let canonical = project.canonicalize().unwrap();
        let one = resolve_registration(&config, &state, &canonical, None).unwrap();
        assert_eq!(one.profile, "one");
        write_profile(&config, &state, "two", &project, "different-id");
        let error = resolve_registration(&config, &state, &canonical, None).unwrap_err();
        assert!(error.contains("one, two"), "{error}");
        let explicit = resolve_registration(&config, &state, &canonical, Some("two")).unwrap();
        assert_eq!(explicit.project.id, "different-id");
    }

    #[test]
    fn resolve_registration_accepts_a_single_legacy_registry_layout() {
        let tmp = canonical_test_tempdir();
        let config = tmp.path().join("config");
        let state = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(config.join("clients")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let (profile_dir, _) = write_profile(&config, &state, "one", &project, "repo");
        std::fs::rename(
            profile_dir.join("project-registry"),
            profile_dir.join("projects.d"),
        )
        .unwrap();

        let resolved = resolve_registration(
            &config,
            &state,
            &project.canonicalize().unwrap(),
            Some("one"),
        )
        .unwrap();
        assert_eq!(resolved.project.id, "repo");
        assert_eq!(resolved.profile_dir, profile_dir);
        assert!(profile_dir.join("projects.d/repo.toml").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn project_registration_symlink_fails_closed() {
        use std::os::unix::fs::symlink;
        let tmp = canonical_test_tempdir();
        let config = tmp.path().join("config");
        let state = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(config.join("clients")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let (profile_dir, _) = write_profile(&config, &state, "one", &project, "repo");
        let actual = profile_dir.join("project-registry/repo.toml");
        let target = profile_dir.join("actual.toml");
        std::fs::rename(&actual, &target).unwrap();
        symlink(&target, &actual).unwrap();
        let error = resolve_registration(&config, &state, &project.canonicalize().unwrap(), None)
            .unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn non_regular_project_registration_fails_closed() {
        let tmp = canonical_test_tempdir();
        let config = tmp.path().join("config");
        let state = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(config.join("clients/one/project-registry/bad.toml")).unwrap();
        std::fs::create_dir_all(state.join("clients/one")).unwrap();
        std::fs::write(
            super::super::process::local_runner_profile_marker(&state.join("clients/one")),
            "profile = \"one\"\n",
        )
        .unwrap();
        let error = resolve_registration(&config, &state, &project.canonicalize().unwrap(), None)
            .unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[tokio::test]
    async fn offline_unregister_preserves_other_project_and_repository() {
        let tmp = canonical_test_tempdir();
        let config = tmp.path().join("config");
        let state = tmp.path().join("state");
        let project = tmp.path().join("repo");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(project.join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(config.join("clients")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let (profile_dir, _) = write_profile(&config, &state, "one", &project, "repo");
        std::fs::write(
            profile_dir.join("project-registry/other.toml"),
            format!("id = \"other\"\npath = {:?}\n", other.to_string_lossy()),
        )
        .unwrap();
        let result = run_disconnect(DisconnectOptions {
            project: project.clone(),
            profile: None,
            config_base: Some(config),
            state_base: Some(state),
            server_http: ServerHttpOptions::default(),
        })
        .await
        .unwrap();
        assert_eq!(result.outcome, "local_unregistered");
        assert_eq!(result.runner_action, "not_running");
        assert!(profile_dir.join("project-registry/other.toml").is_file());
        assert!(profile_dir.join("runner.toml").is_file());
        assert_eq!(
            std::fs::read_to_string(project.join("keep.txt")).unwrap(),
            "keep"
        );
        assert!(project.join(".git").is_dir());
    }

    #[tokio::test]
    async fn offline_disconnect_accepts_legacy_agent_toml_only() {
        let tmp = canonical_test_tempdir();
        let config = tmp.path().join("config");
        let state = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(config.join("clients")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let (profile_dir, _) = write_profile(&config, &state, "legacy", &project, "repo");
        std::fs::rename(
            profile_dir.join("runner.toml"),
            profile_dir.join("agent.toml"),
        )
        .unwrap();

        let result = run_disconnect(DisconnectOptions {
            project,
            profile: Some("legacy".to_string()),
            config_base: Some(config),
            state_base: Some(state),
            server_http: ServerHttpOptions::default(),
        })
        .await
        .unwrap();
        assert_eq!(result.outcome, "local_unregistered");
        assert!(profile_dir.join("agent.toml").is_file());
        assert!(!profile_dir.join("runner.toml").exists());
    }

    #[tokio::test]
    async fn offline_disconnect_rejects_dual_runner_config_names() {
        let tmp = canonical_test_tempdir();
        let config = tmp.path().join("config");
        let state = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(config.join("clients")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let (profile_dir, _) = write_profile(&config, &state, "dual", &project, "repo");
        std::fs::copy(
            profile_dir.join("runner.toml"),
            profile_dir.join("agent.toml"),
        )
        .unwrap();

        let error = run_disconnect(DisconnectOptions {
            project,
            profile: Some("dual".to_string()),
            config_base: Some(config),
            state_base: Some(state),
            server_http: ServerHttpOptions::default(),
        })
        .await
        .unwrap_err();
        assert!(error.contains("runner.toml"));
        assert!(error.contains("agent.toml"));
        assert!(error.contains("refusing to guess"));
        assert!(profile_dir.join("project-registry/repo.toml").is_file());
    }

    #[tokio::test]
    async fn offline_oauth_unregister_does_not_require_managed_login() {
        let tmp = canonical_test_tempdir();
        let config = tmp.path().join("config");
        let state = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(config.join("clients")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let (profile_dir, _) = write_profile(&config, &state, "oauth", &project, "repo");
        std::fs::write(
            profile_dir.join("runner.toml"),
            "server_url = \"https://example.test\"\ntoken = \"wc_agent_runner-only\"\nclient_id = \"client\"\n",
        )
        .unwrap();
        // Deliberately name a managed login that does not exist. Offline
        // disconnect must not consult it because no remote unregister occurs.
        std::fs::write(
            profile_dir.join("oauth-connect.toml"),
            "version = 1\nserver_url = \"https://example.test\"\nusername = \"missing-user\"\noauth_client_id = \"wc_client_existing\"\noauth_client_secret = \"wc_csec_existing\"\noauth_redirect_uri = \"https://client.example/callback\"\nallowed_scopes = [\"runtime:read\"]\nagent_token_id = \"token-id\"\n",
        )
        .unwrap();

        let result = run_disconnect(DisconnectOptions {
            project: project.clone(),
            profile: Some("oauth".to_string()),
            config_base: Some(config),
            state_base: Some(state),
            server_http: ServerHttpOptions::default(),
        })
        .await
        .unwrap();
        assert_eq!(result.outcome, "local_unregistered");
        assert_eq!(result.runner_action, "not_running");
        assert!(project.join(".git").is_dir());
    }

    #[tokio::test]
    async fn live_unregister_uses_structured_endpoint_and_revision() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = vec![0u8; 16 * 1024];
                let read = stream.read(&mut request).unwrap();
                let text = String::from_utf8_lossy(&request[..read]);
                let (path, body) = if index == 0 {
                    assert!(text.starts_with("POST /api/projects/list "), "{text}");
                    (
                        "/api/projects/list",
                        json!({"success":true,"output":{"projects":[{"id":"agent:client:repo","revision":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}),
                    )
                } else {
                    assert!(text.starts_with("POST /api/projects/unregister "), "{text}");
                    assert!(text.contains("agent:client:repo"), "{text}");
                    assert!(text.contains("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "{text}");
                    (
                        "/api/projects/unregister",
                        json!({"operation":"unregister","project":"agent:client:repo","outcome":"unregistered","changed":true}),
                    )
                };
                let _ = path;
                let payload = body.to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                )
                .unwrap();
            }
        });
        let config = ExistingRunnerConfig {
            server_url: format!("http://{address}"),
            token: "shared-key".to_string(),
            client_id: "client".to_string(),
        };
        let outcome = unregister_live_project(
            &config,
            &ServerHttpOptions {
                proxy: None,
                no_system_proxy: true,
            },
            "agent:client:repo",
            &config.token,
        )
        .await
        .unwrap();
        assert_eq!(outcome, "unregistered");
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_live_unregister_accepts_runner_removed_registration_and_stops_last_runner()
    {
        use std::os::unix::fs::PermissionsExt;

        let tmp = canonical_test_tempdir();
        let config_base = tmp.path().join("config");
        let state_base = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(project.join("keep.txt"), "keep").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (profile_dir, state_dir) =
            write_profile(&config_base, &state_base, "one", &project, "repo");
        std::fs::write(
            profile_dir.join("runner.toml"),
            format!(
                "server_url = \"http://{address}\"\ntoken = \"shared-key\"\nclient_id = \"client\"\n"
            ),
        )
        .unwrap();
        let registration = profile_dir.join("project-registry/repo.toml");

        let runner = tmp.path().join("webcodex-runner");
        std::fs::write(
            &runner,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            super::super::process::ensure_runner_unlocked(
                &runner,
                &profile_dir.join("runner.toml"),
                &state_dir,
            )
            .unwrap(),
            super::super::process::RunnerStart::Started
        );
        assert!(local_runner_state_summary(&state_dir).unwrap().running);

        let registration_for_server = registration.clone();
        let server = thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = vec![0u8; 16 * 1024];
                let read = stream.read(&mut request).unwrap();
                let text = String::from_utf8_lossy(&request[..read]);
                let payload = if index == 0 {
                    assert!(text.starts_with("POST /api/projects/list "), "{text}");
                    json!({"success":true,"output":{"projects":[{"id":"agent:client:repo","revision":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}).to_string()
                } else {
                    assert!(text.starts_with("POST /api/projects/unregister "), "{text}");
                    std::fs::remove_file(&registration_for_server).unwrap();
                    json!({"operation":"unregister","project":"agent:client:repo","outcome":"unregistered","changed":true}).to_string()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                )
                .unwrap();
            }
        });

        let result = run_disconnect(DisconnectOptions {
            project: project.clone(),
            profile: Some("one".to_string()),
            config_base: Some(config_base),
            state_base: Some(state_base),
            server_http: ServerHttpOptions {
                proxy: None,
                no_system_proxy: true,
            },
        })
        .await
        .unwrap();

        assert_eq!(result.outcome, "unregistered");
        assert_eq!(result.runner_action, "stopped");
        assert!(!registration.exists());
        assert!(!local_runner_state_summary(&state_dir).unwrap().running);
        assert_eq!(
            std::fs::read_to_string(project.join("keep.txt")).unwrap(),
            "keep"
        );
        assert!(project.join(".git").is_dir());
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lost_unregister_response_reobserves_runner_removed_registration_as_absent() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = canonical_test_tempdir();
        let config_base = tmp.path().join("config");
        let state_base = tmp.path().join("state");
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(project.join("keep.txt"), "keep").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (profile_dir, state_dir) =
            write_profile(&config_base, &state_base, "one", &project, "repo");
        std::fs::write(
            profile_dir.join("runner.toml"),
            format!(
                "server_url = \"http://{address}\"\ntoken = \"shared-key\"\nclient_id = \"client\"\n"
            ),
        )
        .unwrap();
        let registration = profile_dir.join("project-registry/repo.toml");

        let runner = tmp.path().join("webcodex-runner");
        std::fs::write(
            &runner,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            super::super::process::ensure_runner_unlocked(
                &runner,
                &profile_dir.join("runner.toml"),
                &state_dir,
            )
            .unwrap(),
            super::super::process::RunnerStart::Started
        );
        assert!(local_runner_state_summary(&state_dir).unwrap().running);

        let registration_for_server = registration.clone();
        let server = thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = vec![0u8; 16 * 1024];
                let read = stream.read(&mut request).unwrap();
                let text = String::from_utf8_lossy(&request[..read]);
                if index == 0 {
                    assert!(text.starts_with("POST /api/projects/list "), "{text}");
                    let payload = json!({"success":true,"output":{"projects":[{"id":"agent:client:repo","revision":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}).to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    )
                    .unwrap();
                } else {
                    assert!(text.starts_with("POST /api/projects/unregister "), "{text}");
                    assert!(text.contains("agent:client:repo"), "{text}");
                    std::fs::remove_file(&registration_for_server).unwrap();
                    // Simulate Runner authoritative removal followed by a lost
                    // Server/transport response. Dropping the stream sends no
                    // terminal unregister outcome back to the CLI.
                }
            }
        });

        let error = run_disconnect(DisconnectOptions {
            project: project.clone(),
            profile: Some("one".to_string()),
            config_base: Some(config_base),
            state_base: Some(state_base),
            server_http: ServerHttpOptions {
                proxy: None,
                no_system_proxy: true,
            },
        })
        .await
        .unwrap_err();

        assert!(error.contains("disconnect outcome unknown"), "{error}");
        assert!(
            error.contains("exact local registration is no longer present"),
            "{error}"
        );
        assert!(!error.contains("was preserved"), "{error}");
        assert!(!registration.exists());
        assert_eq!(
            std::fs::read_to_string(project.join("keep.txt")).unwrap(),
            "keep"
        );
        assert!(project.join(".git").is_dir());
        assert!(
            local_runner_state_summary(&state_dir).unwrap().running,
            "uncertain disconnect must not infer inventory or stop the Runner"
        );
        stop_runner_unlocked(&state_dir).unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn missing_live_inventory_reports_unregister_not_dispatched() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0u8; 16 * 1024];
            let read = stream.read(&mut request).unwrap();
            let text = String::from_utf8_lossy(&request[..read]);
            assert!(text.starts_with("POST /api/projects/list "), "{text}");
            let payload = json!({"success":true,"output":{"projects":[]}}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            )
            .unwrap();
        });
        let config = ExistingRunnerConfig {
            server_url: format!("http://{address}"),
            token: "shared-key".to_string(),
            client_id: "client".to_string(),
        };
        let error = unregister_live_project(
            &config,
            &ServerHttpOptions {
                proxy: None,
                no_system_proxy: true,
            },
            "agent:client:repo",
            &config.token,
        )
        .await
        .unwrap_err();
        match error {
            LiveUnregisterError::NoUnregister(message) => {
                assert!(
                    message.contains("unregister was not dispatched"),
                    "{message}"
                );
                assert!(!message.contains("was preserved"), "{message}");
            }
            other => panic!("expected pre-dispatch failure, got {other:?}"),
        }
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn last_project_disconnect_stops_managed_runner() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = canonical_test_tempdir();
        let runner = tmp.path().join("webcodex-runner");
        std::fs::write(
            &runner,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = tmp.path().join("runner.toml");
        std::fs::write(&config, "server_url='http://example.test'\n").unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(
            super::super::process::local_runner_profile_marker(&state),
            "profile = \"disconnect-test\"\n",
        )
        .unwrap();
        assert_eq!(
            super::super::process::ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            super::super::process::RunnerStart::Started
        );
        assert!(local_runner_state_summary(&state).unwrap().running);

        assert_eq!(runner_action_after_disconnect(true, 0, &state), "stopped");
        assert!(!local_runner_state_summary(&state).unwrap().running);
    }

    #[test]
    fn output_is_bounded_non_secret_metadata() {
        let result = DisconnectResult {
            outcome: "unregistered".to_string(),
            profile: "demo".to_string(),
            project: PathBuf::from("/tmp/repo"),
            project_id: "agent:client:repo".to_string(),
            runner_action: "kept_running".to_string(),
        };
        let rendered = result.render();
        assert!(rendered.contains("profile: demo"));
        assert!(rendered.contains("project_id: agent:client:repo"));
        assert!(!rendered.contains("shared-key"));
        assert!(!rendered.contains("runner.toml"));
        assert!(!rendered.contains("agent.toml"));
    }

    #[test]
    fn project_revision_validation_is_exact() {
        assert!(valid_revision(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(!valid_revision("sha256:abc"));
    }
}
