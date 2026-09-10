use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use webcodex_admin::ServerHttpOptions;

use super::connect::profile::{atomic_write, render_project_file, resolve_project};
use super::{http_post_json_status, read_optional_token, shell_command, validate_user_api_token};

#[cfg(test)]
#[path = "tests/project_activation_identity.rs"]
mod activation_identity_tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRegisterOptions {
    pub(crate) config: PathBuf,
    pub(crate) project: PathBuf,
    pub(crate) json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectActivateOptions {
    pub(crate) config: PathBuf,
    pub(crate) user_token_file: PathBuf,
    pub(crate) project: PathBuf,
    pub(crate) json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRegistration {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) record_path: PathBuf,
    pub(crate) already_registered: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RegistrationPolicy {
    #[serde(default)]
    allow_cwd_anywhere: bool,
    #[serde(default)]
    allowed_roots: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RegistrationRunnerConfig {
    #[serde(default)]
    project_registry_dir: Option<PathBuf>,
    #[serde(default, rename = "projects_dir")]
    legacy_projects_dir: Option<PathBuf>,
    #[serde(default)]
    policy: RegistrationPolicy,
}

#[derive(Debug, Deserialize)]
struct ActivationRunnerConfig {
    server_url: String,
    client_id: String,
    #[serde(default)]
    policy: RegistrationPolicy,
}

#[derive(Debug, Deserialize)]
struct OperatorToolResult {
    success: bool,
    #[serde(default)]
    output: Value,
    #[serde(default)]
    error: Option<String>,
}

fn canonical_existing_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = path.canonicalize().map_err(|error| {
        format!(
            "{label} {} does not exist or cannot be resolved: {error}",
            path.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "{label} {} is not a directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

#[derive(Debug)]
struct PreparedProjectAuthority {
    canonical_project: PathBuf,
    allowed_roots: Vec<PathBuf>,
    authority_changed: bool,
}

fn authorize_canonical_project(
    canonical_project: &Path,
    configured_roots: &[PathBuf],
    allow_cwd_anywhere: bool,
) -> Result<(Vec<PathBuf>, bool), String> {
    // Root discovery may return empty when HOME is absent: this local explicit
    // selection supplies its own root below. The actual policy flag is unchanged.
    let effective_roots = webcodex_runner_config::effective_allowed_roots(configured_roots, true)?;
    let canonical_roots =
        webcodex_runner_config::paths::canonicalize_usable_allowed_roots(&effective_roots);

    match webcodex_runner_config::paths::validate_project_path_policy(
        canonical_project,
        &canonical_roots,
        allow_cwd_anywhere,
    ) {
        // Preserve the pre-existing local registration/output contract: callers
        // see canonical usable roots when no authority extension was needed.
        Ok(()) => Ok((canonical_roots, false)),
        Err(_) => {
            // Only local CLI/Desktop callers reach this helper. The explicit
            // selection grants the exact canonical root, never its parent.
            let mut effective_roots = effective_roots;
            let mut canonical_roots = canonical_roots;
            effective_roots.push(canonical_project.to_path_buf());
            canonical_roots.push(canonical_project.to_path_buf());
            webcodex_runner_config::paths::validate_project_path_policy(
                canonical_project,
                &canonical_roots,
                allow_cwd_anywhere,
            )?;
            Ok((effective_roots, true))
        }
    }
}

fn prepare_project_authority(
    project: &Path,
    configured_roots: &[PathBuf],
    allow_cwd_anywhere: bool,
) -> Result<PreparedProjectAuthority, String> {
    webcodex_runner_config::paths::validate_project_path_ingress(project)?;
    if project
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("project path must not contain parent traversal".to_string());
    }
    let canonical_project = canonical_existing_directory(project, "project path")?;
    let (allowed_roots, authority_changed) =
        authorize_canonical_project(&canonical_project, configured_roots, allow_cwd_anywhere)?;
    if authority_changed
        && project
            .ancestors()
            .any(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink()))
    {
        return Err("project symlink resolves outside allowed_roots; select the canonical directory explicitly".to_string());
    }
    Ok(PreparedProjectAuthority {
        canonical_project,
        allowed_roots,
        authority_changed,
    })
}

fn prepare_exact_project_authority(
    project: &Path,
    configured_roots: &[PathBuf],
    allow_cwd_anywhere: bool,
) -> Result<PreparedProjectAuthority, String> {
    webcodex_runner_config::paths::validate_project_path_ingress(project)?;
    if project
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("project path must not contain parent traversal".to_string());
    }
    let canonical_project = canonical_existing_directory(project, "project path")?;
    let canonical_roots =
        webcodex_runner_config::paths::canonicalize_usable_allowed_roots(configured_roots);
    let exact_root_present = canonical_roots
        .iter()
        .any(|root| webcodex_runner_config::paths::paths_equal(root, &canonical_project));
    let mut allowed_roots = configured_roots.to_vec();
    if !exact_root_present {
        allowed_roots.push(canonical_project.clone());
    }
    let canonical_after =
        webcodex_runner_config::paths::canonicalize_usable_allowed_roots(&allowed_roots);
    webcodex_runner_config::paths::validate_project_path_policy(
        &canonical_project,
        &canonical_after,
        allow_cwd_anywhere,
    )?;
    if !exact_root_present
        && project
            .ancestors()
            .any(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink()))
    {
        return Err("project symlink resolves outside allowed_roots; select the canonical directory explicitly".to_string());
    }
    Ok(PreparedProjectAuthority {
        canonical_project,
        allowed_roots,
        authority_changed: !exact_root_present,
    })
}

fn register_canonical_project(
    project_registry_dir: &Path,
    canonical_project: PathBuf,
    explicit_id: Option<&str>,
) -> Result<ProjectRegistration, String> {
    ensure_registry_directory(project_registry_dir)?;
    let (record_path, project_file, already_registered) =
        resolve_project(project_registry_dir, &canonical_project, explicit_id)?;
    if !already_registered {
        let content = render_project_file(&project_file)?;
        atomic_write(&record_path, content.as_bytes(), false)?;
    }
    Ok(ProjectRegistration {
        id: project_file.id,
        path: canonical_project,
        record_path,
        already_registered,
    })
}

fn ensure_registry_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "project_registry_dir {} must be an absolute path; project_registry_dir is the Runner project registry directory, not a workspace root",
            path.display()
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "project_registry_dir {} is not a real directory; project_registry_dir is the Runner project registry directory, not a workspace root",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).map_err(|error| {
                format!(
                    "failed to create project_registry_dir {}: {error}",
                    path.display()
                )
            })?;
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect project_registry_dir {}: {error}",
                path.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn register_existing_project(
    project_registry_dir: &Path,
    project: &Path,
    configured_roots: &[PathBuf],
    allow_cwd_anywhere: bool,
    explicit_id: Option<&str>,
) -> Result<(ProjectRegistration, Vec<PathBuf>, bool), String> {
    let prepared = prepare_project_authority(project, configured_roots, allow_cwd_anywhere)?;
    let registration = register_canonical_project(
        project_registry_dir,
        prepared.canonical_project,
        explicit_id,
    )?;
    Ok((
        registration,
        prepared.allowed_roots,
        prepared.authority_changed,
    ))
}

fn read_registration_config(path: &Path) -> Result<RegistrationRunnerConfig, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to inspect Runner config {}: {error}",
            path.display()
        )
    })?;
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(format!(
            "Runner config {} is not a regular file",
            path.display()
        ));
    }
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read Runner config {}: {error}", path.display()))?;
    toml::from_str(&content)
        .map_err(|error| format!("failed to parse Runner config {}: {error}", path.display()))
}

fn render_registration_allowed_roots(
    path: &Path,
    content: &str,
    roots: &[PathBuf],
) -> Result<String, String> {
    let mut document = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format!("failed to parse Runner config {}: {error}", path.display()))?;
    if document.get("policy").is_none() {
        document["policy"] = toml_edit::table();
    }
    let policy = document["policy"].as_table_like_mut().ok_or_else(|| {
        format!(
            "Runner config {} has an invalid [policy] table",
            path.display()
        )
    })?;
    let mut allowed_roots = toml_edit::Array::new();
    for root in roots {
        allowed_roots.push(root.to_string_lossy().as_ref());
    }
    policy.insert("allowed_roots", toml_edit::value(allowed_roots));
    Ok(document.to_string())
}

fn persist_registration_allowed_roots_if_unchanged(
    path: &Path,
    expected_content: &str,
    roots: &[PathBuf],
) -> Result<String, String> {
    let current = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read Runner config {}: {error}", path.display()))?;
    if current != expected_content {
        return Err("runner_config_concurrent_change: Runner config changed while project authority was being prepared; retry from the current config".to_string());
    }
    let candidate = render_registration_allowed_roots(path, expected_content, roots)?;
    atomic_write(path, candidate.as_bytes(), true)?;
    Ok(candidate)
}

fn runner_config_candidate_unchanged(path: &Path, expected_content: &str) -> Result<bool, String> {
    std::fs::read_to_string(path)
        .map(|current| current == expected_content)
        .map_err(|error| format!("failed to read Runner config {}: {error}", path.display()))
}

fn persist_registration_allowed_roots(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read Runner config {}: {error}", path.display()))?;
    persist_registration_allowed_roots_if_unchanged(path, &content, roots).map(|_| ())
}

fn registration_project_registry_dir(config: &RegistrationRunnerConfig) -> Result<PathBuf, String> {
    match (
        config.project_registry_dir.as_ref(),
        config.legacy_projects_dir.as_ref(),
    ) {
        (Some(_), Some(_)) => Err(
            "project_registry_dir and legacy projects_dir cannot both be configured; keep exactly one Runner project registry setting"
                .to_string(),
        ),
        (Some(path), None) | (None, Some(path)) => Ok(path.clone()),
        (None, None) => {
            let base = webcodex_runner_config::paths::default_client_config_base_dir()?;
            webcodex_runner_config::paths::select_project_registry_dir(&base)
        }
    }
}

fn read_activation_config(path: &Path) -> Result<(ActivationRunnerConfig, String), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to inspect Runner config {}: {error}",
            path.display()
        )
    })?;
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(format!(
            "Runner config {} is not a regular file",
            path.display()
        ));
    }
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read Runner config {}: {error}", path.display()))?;
    let config = toml::from_str::<ActivationRunnerConfig>(&content)
        .map_err(|error| format!("failed to parse Runner config {}: {error}", path.display()))?;
    if config.server_url.trim().is_empty() || config.client_id.trim().is_empty() {
        return Err("Runner config is missing server_url or client_id".to_string());
    }
    url::Url::parse(&config.server_url)
        .map_err(|_| "Runner config server_url is invalid".to_string())?;
    Ok((config, content))
}

fn reread_activation_config(
    path: &Path,
    server_url: &str,
    client_id: &str,
) -> Result<(ActivationRunnerConfig, String), String> {
    let snapshot = read_activation_config(path)?;
    // A retry can merge policy edits only within the original connection.
    // Re-enrollment must not let this operation extend another Runner's roots.
    if snapshot.0.server_url != server_url || snapshot.0.client_id != client_id {
        return Err("project_activation_config_conflict: Runner connection identity changed during activation; start a new activation from the current configuration".to_string());
    }
    Ok(snapshot)
}

fn server_url_is_loopback(server_url: &str) -> bool {
    url::Url::parse(server_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

async fn operator_tool_call(
    server_url: &str,
    token: &str,
    path: &str,
    body: Value,
) -> Result<OperatorToolResult, String> {
    let server_http = ServerHttpOptions {
        no_system_proxy: server_url_is_loopback(server_url),
        ..ServerHttpOptions::default()
    };
    let (status, content_type, value) =
        http_post_json_status(server_url, &server_http, path, Some(token), body).await?;
    if matches!(status, 404 | 405) {
        return Ok(OperatorToolResult {
            success: false,
            output: json!({
                "failure_kind": "capability_unavailable",
                "error_code": "operator_endpoint_unavailable",
                "http_status": status,
            }),
            error: Some("operator endpoint is unavailable on this Server version".to_string()),
        });
    }
    let Some(value) = value else {
        return Err(format!(
            "operator endpoint returned HTTP {status} without JSON ({content_type})"
        ));
    };
    serde_json::from_value(value)
        .map_err(|_| format!("operator endpoint returned an unexpected response (HTTP {status})"))
}

fn operator_error_code(result: &OperatorToolResult) -> Option<&str> {
    result
        .output
        .get("error_code")
        .and_then(Value::as_str)
        .or_else(|| result.output.get("error_kind").and_then(Value::as_str))
        .or(result.error.as_deref())
}

fn capability_unavailable(result: &OperatorToolResult) -> bool {
    result.output.get("failure_kind").and_then(Value::as_str) == Some("capability_unavailable")
        || operator_error_code(result).is_some_and(|code| code.contains("capability_unavailable"))
}

fn project_policy_denied(result: &OperatorToolResult) -> bool {
    operator_error_code(result) == Some("path_outside_allowed_roots")
}

fn operation_uncertain(result: &OperatorToolResult) -> bool {
    result.output.get("execution_state").and_then(Value::as_str) == Some("outcome_unknown")
        || operator_error_code(result) == Some("operation_indeterminate")
        || operator_error_code(result) == Some("project_projection_reconcile_required")
}

async fn resolve_project_operator(
    server_url: &str,
    token: &str,
    client_id: &str,
    canonical_project: &Path,
) -> Result<OperatorToolResult, String> {
    operator_tool_call(
        server_url,
        token,
        "/api/projects/resolve-or-register",
        json!({
            "client_id": client_id,
            "path": canonical_project.to_string_lossy(),
        }),
    )
    .await
}

async fn reconcile_project_from_inventory(
    server_url: &str,
    token: &str,
    client_id: &str,
    canonical_project: &Path,
) -> Result<Option<Value>, String> {
    let listed = operator_tool_call(
        server_url,
        token,
        "/api/projects/list",
        json!({"client_id": client_id, "limit": 100}),
    )
    .await?;
    if !listed.success {
        return Err(format!(
            "project_activation_reconcile_required: project inventory could not be observed: {}",
            listed.error.as_deref().unwrap_or("list_projects failed")
        ));
    }
    let Some(projects) = listed.output.get("projects").and_then(Value::as_array) else {
        return Ok(None);
    };
    for project in projects {
        let Some(path) = project.get("path").and_then(Value::as_str) else {
            continue;
        };
        let candidate = Path::new(path);
        if webcodex_runner_config::paths::paths_equal(candidate, canonical_project)
            || candidate
                .canonicalize()
                .ok()
                .as_ref()
                .is_some_and(|resolved| {
                    webcodex_runner_config::paths::paths_equal(resolved, canonical_project)
                })
        {
            return Ok(Some(project.clone()));
        }
    }
    Ok(None)
}

async fn reconcile_reload_generation(
    server_url: &str,
    token: &str,
    client_id: &str,
    config_path: &Path,
    expected_candidate: &str,
    expected_generation: u64,
) -> Result<u64, String> {
    // A lost reload response is stateful uncertainty. Do not issue another
    // reload. First ensure the on-disk candidate is still the exact candidate
    // Desktop checked, then observe the active generation through the typed
    // Runner config check contract.
    if !runner_config_candidate_unchanged(config_path, expected_candidate)? {
        return Err("project_activation_config_conflict: Runner config changed while reload outcome was uncertain; re-observe the current operator configuration".to_string());
    }
    let checked = operator_tool_call(
        server_url,
        token,
        "/api/runners/config/check",
        json!({"client_id": client_id}),
    )
    .await
    .map_err(|_| {
        "project_activation_reconcile_required: config reload outcome is uncertain and active generation could not be observed".to_string()
    })?;
    if !checked.success {
        return Err("project_activation_reconcile_required: config reload outcome is uncertain and Runner config check did not complete".to_string());
    }
    if checked.output.get("valid").and_then(Value::as_bool) != Some(true) {
        return Err("project_activation_reconcile_required: config reload outcome is uncertain and the current disk candidate is not valid".to_string());
    }
    if checked
        .output
        .get("restart_required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err("project_activation_restart_required: Runner config now contains startup-only changes while reload outcome is being reconciled".to_string());
    }
    let current_generation = checked
        .output
        .get("current_generation")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "project_activation_reconcile_required: Runner config check did not report active generation while reload outcome was uncertain".to_string()
        })?;
    let applied_generation = expected_generation.checked_add(1).ok_or_else(|| {
        "project_activation_reconcile_required: Runner config generation overflowed while reload outcome was uncertain".to_string()
    })?;
    if current_generation == applied_generation {
        return Ok(current_generation);
    }
    if current_generation == expected_generation {
        return Err("project_activation_reconcile_required: config reload outcome is uncertain and the active generation has not advanced; no retry was attempted".to_string());
    }
    Err(format!(
        "project_activation_config_conflict: active Runner config generation changed from {expected_generation} to {current_generation} while reload outcome was uncertain"
    ))
}

fn render_activation_output(
    opts: &ProjectActivateOptions,
    client_id: &str,
    canonical_project: &Path,
    project: Value,
    authority_changed: bool,
    config_reloaded: bool,
    generation_before: Option<u64>,
    generation_after: Option<u64>,
    reconciled: bool,
) -> Result<String, String> {
    let runtime_project = project
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let agent_project_id = project
        .get("agent_project_id")
        .and_then(Value::as_str)
        .or_else(|| runtime_project.strip_prefix(&format!("agent:{client_id}:")))
        .unwrap_or_default();
    if runtime_project.is_empty() || agent_project_id.is_empty() {
        return Err(
            "project_activation_reconcile_required: authoritative project identity is incomplete"
                .to_string(),
        );
    }
    if opts.json {
        return serde_json::to_string_pretty(&json!({
            "client_id": client_id,
            "project": {
                "id": agent_project_id,
                "runtime_project": runtime_project,
                "path": canonical_project.to_string_lossy(),
            },
            "policy": {
                "authority_changed": authority_changed,
                "config_reloaded": config_reloaded,
                "generation_before": generation_before,
                "generation_after": generation_after,
            },
            "reconciled": reconciled,
        }))
        .map_err(|error| error.to_string());
    }
    Ok(format!(
        "Project ready:\n  {}\n",
        canonical_project.display()
    ))
}

pub(crate) async fn run_project_activate(opts: ProjectActivateOptions) -> Result<String, String> {
    let token = read_optional_token(&Some(opts.user_token_file.clone()), "--user-token-file")?
        .ok_or_else(|| "--user-token-file does not contain a token".to_string())?;
    validate_user_api_token(&token)?;

    let (mut config, mut config_content) = read_activation_config(&opts.config)?;
    let mut prepared = prepare_exact_project_authority(
        &opts.project,
        &config.policy.allowed_roots,
        config.policy.allow_cwd_anywhere,
    )?;
    let canonical_project = prepared.canonical_project.clone();
    let server_url = config.server_url.clone();
    let client_id = config.client_id.clone();
    let mut authority_changed = false;
    let mut config_reloaded = false;
    let mut generation_before = None;
    let mut generation_after = None;
    let mut reload_reconciled = false;

    // Idempotence fast path: if the exact root is already persisted, let the
    // current Runner prove active authority and canonical registration before
    // touching config generation.
    if !prepared.authority_changed {
        match resolve_project_operator(&server_url, &token, &client_id, &canonical_project).await {
            Ok(result) if result.success => {
                return render_activation_output(
                    &opts,
                    &client_id,
                    &canonical_project,
                    result.output,
                    false,
                    false,
                    None,
                    None,
                    false,
                );
            }
            Ok(result) if capability_unavailable(&result) => {
                return Err("project_activation_capability_unavailable: this Runner needs to be refreshed before the new project can be activated".to_string());
            }
            Ok(result) if operation_uncertain(&result) => {
                if let Some(project) = reconcile_project_from_inventory(
                    &server_url,
                    &token,
                    &client_id,
                    &canonical_project,
                )
                .await?
                {
                    return render_activation_output(
                        &opts,
                        &client_id,
                        &canonical_project,
                        project,
                        false,
                        false,
                        None,
                        None,
                        true,
                    );
                }
                return Err("project_activation_reconcile_required: Runner project activation outcome is uncertain and the exact project is not visible yet".to_string());
            }
            Ok(result) if project_policy_denied(&result) => {}
            Ok(result) => {
                return Err(format!(
                    "project activation failed: {}",
                    result
                        .error
                        .as_deref()
                        .unwrap_or("Runner rejected project activation")
                ));
            }
            Err(_) => {
                if let Some(project) = reconcile_project_from_inventory(
                    &server_url,
                    &token,
                    &client_id,
                    &canonical_project,
                )
                .await?
                {
                    return render_activation_output(
                        &opts,
                        &client_id,
                        &canonical_project,
                        project,
                        false,
                        false,
                        None,
                        None,
                        true,
                    );
                }
                return Err("project_activation_reconcile_required: project activation response was lost and the exact project is not visible in Server inventory".to_string());
            }
        }
    }

    for _ in 0..3 {
        if prepared.authority_changed {
            config_content = persist_registration_allowed_roots_if_unchanged(
                &opts.config,
                &config_content,
                &prepared.allowed_roots,
            )?;
            authority_changed = true;
        }

        let checked = operator_tool_call(
            &server_url,
            &token,
            "/api/runners/config/check",
            json!({"client_id": client_id}),
        )
        .await?;
        if !checked.success {
            if capability_unavailable(&checked) {
                return Err("project_activation_capability_unavailable: this Runner does not support hot project activation".to_string());
            }
            return Err(format!(
                "Runner config check failed: {}",
                checked
                    .error
                    .as_deref()
                    .unwrap_or("unknown config check error")
            ));
        }
        if checked.output.get("valid").and_then(Value::as_bool) != Some(true) {
            return Err(
                "Runner config candidate is invalid; active Runner configuration was not changed"
                    .to_string(),
            );
        }
        if checked
            .output
            .get("restart_required")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err("project_activation_restart_required: Runner config contains startup-only changes; refresh this Runner before activating the project".to_string());
        }
        let generation = checked
            .output
            .get("current_generation")
            .and_then(Value::as_u64)
            .ok_or_else(|| "Runner config check did not report current_generation".to_string())?;
        generation_before.get_or_insert(generation);

        // `expected_generation` fences the active snapshot, not an operator edit
        // that has only changed runner.toml on disk. Re-observe the exact
        // candidate after check and refuse to reload if another writer replaced
        // it in that window; the next iteration merges from the latest config.
        if !runner_config_candidate_unchanged(&opts.config, &config_content)? {
            let snapshot = reread_activation_config(&opts.config, &server_url, &client_id)?;
            config = snapshot.0;
            config_content = snapshot.1;
            prepared = prepare_exact_project_authority(
                &canonical_project,
                &config.policy.allowed_roots,
                config.policy.allow_cwd_anywhere,
            )?;
            continue;
        }

        let reload = operator_tool_call(
            &server_url,
            &token,
            "/api/runners/config/reload",
            json!({"client_id": client_id, "expected_generation": generation}),
        )
        .await;
        match reload {
            Ok(result) if result.success => {
                if result
                    .output
                    .get("restart_required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return Err("project_activation_restart_required: Runner reported startup-only changes after reload".to_string());
                }
                config_reloaded = true;
                generation_after = result
                    .output
                    .get("current_generation")
                    .and_then(Value::as_u64);
                break;
            }
            Ok(result) if capability_unavailable(&result) => {
                return Err("project_activation_capability_unavailable: this Runner does not support hot config reload".to_string());
            }
            Ok(result) if operator_error_code(&result) == Some("config_generation_conflict") => {
                let snapshot = reread_activation_config(&opts.config, &server_url, &client_id)?;
                config = snapshot.0;
                config_content = snapshot.1;
                prepared = prepare_exact_project_authority(
                    &canonical_project,
                    &config.policy.allowed_roots,
                    config.policy.allow_cwd_anywhere,
                )?;
                continue;
            }
            Ok(result) if operation_uncertain(&result) => {
                let observed_generation = reconcile_reload_generation(
                    &server_url,
                    &token,
                    &client_id,
                    &opts.config,
                    &config_content,
                    generation,
                )
                .await?;
                config_reloaded = true;
                generation_after = Some(observed_generation);
                reload_reconciled = true;
                break;
            }
            Ok(result) => {
                return Err(format!(
                    "Runner config reload failed: {}",
                    result
                        .error
                        .as_deref()
                        .unwrap_or("unknown config reload error")
                ));
            }
            Err(_) => {
                let observed_generation = reconcile_reload_generation(
                    &server_url,
                    &token,
                    &client_id,
                    &opts.config,
                    &config_content,
                    generation,
                )
                .await?;
                config_reloaded = true;
                generation_after = Some(observed_generation);
                reload_reconciled = true;
                break;
            }
        }
    }

    if !config_reloaded {
        return Err("project_activation_config_conflict: Runner config kept changing; retry from the current operator configuration".to_string());
    }

    match resolve_project_operator(&server_url, &token, &client_id, &canonical_project).await {
        Ok(result) if result.success => render_activation_output(
            &opts,
            &client_id,
            &canonical_project,
            result.output,
            authority_changed,
            config_reloaded,
            generation_before,
            generation_after,
            reload_reconciled,
        ),
        Ok(result) if capability_unavailable(&result) => Err(
            "project_activation_capability_unavailable: this Runner needs to be refreshed before the new project can be activated".to_string(),
        ),
        Ok(result) if operation_uncertain(&result) => {
            if let Some(project) = reconcile_project_from_inventory(
                &server_url,
                &token,
                &client_id,
                &canonical_project,
            )
            .await?
            {
                return render_activation_output(
                    &opts,
                    &client_id,
                    &canonical_project,
                    project,
                    authority_changed,
                    config_reloaded,
                    generation_before,
                    generation_after,
                    true,
                );
            }
            Err("project_activation_reconcile_required: project mutation may have completed but the exact project is not visible in Server inventory".to_string())
        }
        Ok(result) => Err(format!(
            "project activation failed: {}",
            result.error.as_deref().unwrap_or("Runner rejected project activation")
        )),
        Err(_) => {
            if let Some(project) = reconcile_project_from_inventory(
                &server_url,
                &token,
                &client_id,
                &canonical_project,
            )
            .await?
            {
                return render_activation_output(
                    &opts,
                    &client_id,
                    &canonical_project,
                    project,
                    authority_changed,
                    config_reloaded,
                    generation_before,
                    generation_after,
                    true,
                );
            }
            Err("project_activation_reconcile_required: project activation response was lost and the exact project is not visible in Server inventory".to_string())
        }
    }
}

pub(crate) fn run_project_register(opts: ProjectRegisterOptions) -> Result<String, String> {
    let config = read_registration_config(&opts.config)?;
    // Mirror Runner config loading exactly: old/new config spellings normalize
    // to one registry path, and an omitted field uses the shared four-state
    // on-disk selection contract.
    let project_registry_dir = registration_project_registry_dir(&config)?;
    let prepared = prepare_project_authority(
        &opts.project,
        &config.policy.allowed_roots,
        config.policy.allow_cwd_anywhere,
    )?;
    if prepared.authority_changed {
        // Persist the explicit user grant before publishing the project record so a
        // newly registered project cannot outlive the Runner authority it needs.
        persist_registration_allowed_roots(&opts.config, &prepared.allowed_roots)?;
    }
    let authority_changed = prepared.authority_changed;
    let roots = prepared.allowed_roots;
    let registration =
        register_canonical_project(&project_registry_dir, prepared.canonical_project, None)?;
    let runner_reload_required = !registration.already_registered || authority_changed;
    if opts.json {
        return serde_json::to_string_pretty(&serde_json::json!({
            "runner_config": opts.config.to_string_lossy(),
            "project_registry_dir": project_registry_dir.to_string_lossy(),
            "project": {
                "id": registration.id,
                "path": registration.path.to_string_lossy(),
                "record": registration.record_path.to_string_lossy(),
                "already_registered": registration.already_registered,
            },
            "policy": {
                "allow_cwd_anywhere": config.policy.allow_cwd_anywhere,
                "allowed_roots": roots.iter().map(|root| root.to_string_lossy().to_string()).collect::<Vec<_>>(),
            },
            "runner_reload_required": runner_reload_required,
        }))
        .map_err(|error| error.to_string());
    }
    let runner_command = shell_command(&[
        "webcodex".to_string(),
        "runner".to_string(),
        "run".to_string(),
        "--config".to_string(),
        opts.config.to_string_lossy().into_owned(),
    ]);
    if !runner_reload_required {
        return Ok(format!(
            "Project already added:\n  {}\n\nNo Runner restart is required.\n",
            registration.path.display()
        ));
    }
    let restart_guidance = if cfg!(target_os = "linux") {
        format!(
            "Next:\n  If the Runner is in the foreground, stop it with Ctrl-C, then run:\n    {runner_command}\n  If it is installed as a service, use the matching `webcodex runner restart` command instead.\n"
        )
    } else {
        format!(
            "Next:\n  Stop the foreground Runner with Ctrl-C, then run:\n    {runner_command}\n"
        )
    };
    let action = if registration.already_registered {
        "Project already added"
    } else {
        "Project added"
    };
    Ok(format!(
        "{action}:\n  {}\n\nRunner restart required.\n\n{restart_guidance}",
        registration.path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webcodex_cli::test_support::canonical_test_tempdir;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    const TEST_USER_TOKEN: &str = "wc_pat_project_activation_test";

    pub(super) fn activation_config(
        path: &Path,
        server_url: &str,
        roots: &[PathBuf],
        allow_cwd_anywhere: bool,
    ) {
        let roots = roots
            .iter()
            .map(|root| format!("{:?}", root.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            path,
            format!(
                "server_url = {:?}\ntoken = \"runner-secret\"\nclient_id = \"client\"\n\n[policy]\nallow_cwd_anywhere = {allow_cwd_anywhere}\nallowed_roots = [{roots}]\n",
                server_url,
            ),
        )
        .unwrap();
    }

    fn spawn_operator_server(
        responses: Vec<(&'static str, Value)>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        spawn_operator_server_with_hook(responses, |_| {})
    }

    pub(super) fn spawn_operator_server_with_hook(
        responses: Vec<(&'static str, Value)>,
        mut before_response: impl FnMut(usize) + Send + 'static,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for (expected_path, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = vec![0u8; 64 * 1024];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]).to_string();
                assert!(
                    request.starts_with(&format!("POST {expected_path} ")),
                    "unexpected request: {request}"
                );
                assert!(
                    request.contains(&format!("Authorization: Bearer {TEST_USER_TOKEN}"))
                        || request.contains(&format!("authorization: Bearer {TEST_USER_TOKEN}")),
                    "operator request must use the managed user token"
                );
                requests.push(request);
                before_response(requests.len());
                let payload = body.to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                )
                .unwrap();
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    fn request_json(request: &str) -> Value {
        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    fn project_success(path: &Path) -> Value {
        json!({
            "success": true,
            "output": {
                "id": "agent:client:demo",
                "agent_project_id": "demo",
                "client_id": "client",
                "path": path.to_string_lossy(),
                "revision": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "outcome": "registered",
                "changed": true
            }
        })
    }

    fn config_with_policy(
        path: &Path,
        project_registry_dir: &Path,
        roots: &[PathBuf],
        allow_cwd_anywhere: bool,
    ) {
        let roots = roots
            .iter()
            .map(|root| format!("{:?}", root.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            path,
            format!(
                "server_url = \"https://example.test\"\ntoken = \"secret-not-printed\"\nclient_id = \"client\"\nproject_registry_dir = {:?}\n\n[policy]\nallow_cwd_anywhere = {allow_cwd_anywhere}\nallowed_roots = [{roots}]\n",
                project_registry_dir.to_string_lossy(),
            ),
        )
        .unwrap();
    }

    fn config(path: &Path, project_registry_dir: &Path, root: &Path) {
        config_with_policy(path, project_registry_dir, &[root.to_path_buf()], false);
    }

    #[test]
    fn persist_allowed_roots_supports_inline_policy_and_preserves_other_settings() {
        let tmp = canonical_test_tempdir();
        let config_path = tmp.path().join("runner.toml");
        std::fs::write(
            &config_path,
            "# operator configuration\nclient_id = 'test'\npolicy = { allow_cwd_anywhere = true, allowed_roots = [], max_timeout_secs = 42 }\n",
        )
        .unwrap();
        let roots = vec![tmp.path().join("project")];
        persist_registration_allowed_roots(&config_path, &roots).unwrap();
        let content = std::fs::read_to_string(&config_path).unwrap();
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        assert!(content.contains("# operator configuration"));
        assert_eq!(parsed["client_id"].as_str(), Some("test"));
        assert_eq!(parsed["policy"]["allow_cwd_anywhere"].as_bool(), Some(true));
        assert_eq!(parsed["policy"]["max_timeout_secs"].as_integer(), Some(42));
        let config = read_registration_config(&config_path).unwrap();
        assert_eq!(config.policy.allowed_roots, roots);
    }

    #[test]
    fn activation_adds_exact_root_even_when_parent_or_cwd_anywhere_already_authorizes_path() {
        let tmp = canonical_test_tempdir();
        let parent = tmp.path().join("parent");
        let project = parent.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let parent = parent.canonicalize().unwrap();
        let project = project.canonicalize().unwrap();

        let prepared = prepare_exact_project_authority(&project, &[parent.clone()], false).unwrap();
        assert!(prepared.authority_changed);
        assert_eq!(
            prepared.allowed_roots,
            vec![parent.clone(), project.clone()]
        );

        let cwd_anywhere = prepare_exact_project_authority(&project, &[], true).unwrap();
        assert!(cwd_anywhere.authority_changed);
        assert_eq!(cwd_anywhere.allowed_roots, vec![project.clone()]);

        let repeated =
            prepare_exact_project_authority(&project, &[parent, project.clone()], false).unwrap();
        assert!(!repeated.authority_changed);
        assert_eq!(
            repeated
                .allowed_roots
                .iter()
                .filter(|root| root.canonicalize().ok().as_ref() == Some(&project))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn activation_existing_exact_root_and_project_skips_config_reload() {
        let tmp = canonical_test_tempdir();
        let project = tmp.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let canonical_project = project.canonicalize().unwrap();
        let (server_url, server) = spawn_operator_server(vec![(
            "/api/projects/resolve-or-register",
            json!({
                "success": true,
                "output": {
                    "id": "agent:client:demo",
                    "agent_project_id": "demo",
                    "client_id": "client",
                    "path": canonical_project.to_string_lossy(),
                    "revision": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "changed": false
                }
            }),
        )]);
        let config_path = tmp.path().join("runner.toml");
        activation_config(
            &config_path,
            &server_url,
            std::slice::from_ref(&canonical_project),
            false,
        );
        let token_file = tmp.path().join("user-token");
        std::fs::write(&token_file, TEST_USER_TOKEN).unwrap();
        let before = std::fs::read(&config_path).unwrap();
        let output = run_project_activate(ProjectActivateOptions {
            config: config_path.clone(),
            user_token_file: token_file,
            project,
            json: true,
        })
        .await
        .unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["policy"]["authority_changed"], false);
        assert_eq!(output["policy"]["config_reloaded"], false);
        assert_eq!(output["project"]["runtime_project"], "agent:client:demo");
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
        let requests = server.join().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "idempotent activation must not check or reload config"
        );
        assert!(requests[0].starts_with("POST /api/projects/resolve-or-register "));
    }

    #[test]
    fn activation_reload_candidate_fence_detects_post_write_operator_edit() {
        let tmp = canonical_test_tempdir();
        let config_path = tmp.path().join("runner.toml");
        let project = tmp.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        activation_config(&config_path, "http://127.0.0.1:1", &[], false);
        let original = std::fs::read_to_string(&config_path).unwrap();
        let candidate = persist_registration_allowed_roots_if_unchanged(
            &config_path,
            &original,
            &[project.canonicalize().unwrap()],
        )
        .unwrap();
        assert!(runner_config_candidate_unchanged(&config_path, &candidate).unwrap());
        std::fs::write(
            &config_path,
            format!("{candidate}\n# concurrent operator edit\n"),
        )
        .unwrap();
        assert!(!runner_config_candidate_unchanged(&config_path, &candidate).unwrap());
    }

    #[test]
    fn activation_config_write_rejects_concurrent_operator_change() {
        let tmp = canonical_test_tempdir();
        let config_path = tmp.path().join("runner.toml");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let original = "server_url = \"https://example.test\"\nclient_id = \"client\"\n[policy]\nallowed_roots = []\n";
        std::fs::write(&config_path, original).unwrap();
        let concurrent = format!("{original}# concurrent operator edit\n");
        std::fs::write(&config_path, &concurrent).unwrap();

        let error = persist_registration_allowed_roots_if_unchanged(
            &config_path,
            original,
            &[project.canonicalize().unwrap()],
        )
        .unwrap_err();
        assert!(
            error.starts_with("runner_config_concurrent_change:"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), concurrent);
    }

    #[tokio::test]
    async fn activation_generation_conflict_reobserves_and_converges_without_duplicate_root() {
        let tmp = canonical_test_tempdir();
        let project = tmp.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let canonical_project = project.canonicalize().unwrap();
        let responses = vec![
            (
                "/api/runners/config/check",
                json!({"success":true,"output":{"valid":true,"current_generation":1,"restart_required":false}}),
            ),
            (
                "/api/runners/config/reload",
                json!({"success":false,"output":{"execution_state":"not_started","error_code":"config_generation_conflict","current_generation":2},"error":"generation changed"}),
            ),
            (
                "/api/runners/config/check",
                json!({"success":true,"output":{"valid":true,"current_generation":2,"restart_required":false}}),
            ),
            (
                "/api/runners/config/reload",
                json!({"success":true,"output":{"execution_state":"completed","valid":true,"current_generation":3,"restart_required":false}}),
            ),
            (
                "/api/projects/resolve-or-register",
                project_success(&canonical_project),
            ),
        ];
        let (server_url, server) = spawn_operator_server(responses);
        let config_path = tmp.path().join("runner.toml");
        activation_config(&config_path, &server_url, &[], false);
        let token_file = tmp.path().join("user-token");
        std::fs::write(&token_file, TEST_USER_TOKEN).unwrap();

        let output = run_project_activate(ProjectActivateOptions {
            config: config_path.clone(),
            user_token_file: token_file,
            project,
            json: true,
        })
        .await
        .unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["policy"]["generation_before"], 1);
        assert_eq!(output["policy"]["generation_after"], 3);
        assert_eq!(output["project"]["runtime_project"], "agent:client:demo");

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5);
        assert_eq!(request_json(&requests[1])["expected_generation"], 1);
        assert_eq!(request_json(&requests[3])["expected_generation"], 2);
        let parsed: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let roots = parsed["policy"]["allowed_roots"].as_array().unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(
            roots[0].as_str(),
            Some(canonical_project.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn activation_uncertain_reload_rechecks_generation_without_blind_retry() {
        let tmp = canonical_test_tempdir();
        let project = tmp.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let canonical_project = project.canonicalize().unwrap();
        let responses = vec![
            (
                "/api/runners/config/check",
                json!({"success":true,"output":{"valid":true,"current_generation":7,"restart_required":false}}),
            ),
            (
                "/api/runners/config/reload",
                json!({"success":false,"output":{"execution_state":"outcome_unknown","error_code":"operation_indeterminate"},"error":"response lost"}),
            ),
            (
                "/api/runners/config/check",
                json!({"success":true,"output":{"valid":true,"current_generation":8,"restart_required":false}}),
            ),
            (
                "/api/projects/resolve-or-register",
                project_success(&canonical_project),
            ),
        ];
        let (server_url, server) = spawn_operator_server(responses);
        let config_path = tmp.path().join("runner.toml");
        activation_config(&config_path, &server_url, &[], false);
        let token_file = tmp.path().join("user-token");
        std::fs::write(&token_file, TEST_USER_TOKEN).unwrap();

        let output = run_project_activate(ProjectActivateOptions {
            config: config_path,
            user_token_file: token_file,
            project,
            json: true,
        })
        .await
        .unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["reconciled"], true);
        assert_eq!(output["policy"]["generation_after"], 8);
        let requests = server.join().unwrap();
        assert_eq!(
            requests.len(),
            4,
            "uncertain reload must be observed, not blindly repeated"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /api/runners/config/reload "))
                .count(),
            1
        );
        assert!(requests[2].starts_with("POST /api/runners/config/check "));
        assert!(requests[3].starts_with("POST /api/projects/resolve-or-register "));
    }

    #[tokio::test]
    async fn activation_uncertain_reload_with_unchanged_generation_requires_reconcile() {
        let tmp = canonical_test_tempdir();
        let project = tmp.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let responses = vec![
            (
                "/api/runners/config/check",
                json!({"success":true,"output":{"valid":true,"current_generation":7,"restart_required":false}}),
            ),
            (
                "/api/runners/config/reload",
                json!({"success":false,"output":{"execution_state":"outcome_unknown","error_code":"operation_indeterminate"},"error":"response lost"}),
            ),
            (
                "/api/runners/config/check",
                json!({"success":true,"output":{"valid":true,"current_generation":7,"restart_required":false}}),
            ),
        ];
        let (server_url, server) = spawn_operator_server(responses);
        let config_path = tmp.path().join("runner.toml");
        activation_config(&config_path, &server_url, &[], false);
        let token_file = tmp.path().join("user-token");
        std::fs::write(&token_file, TEST_USER_TOKEN).unwrap();

        let error = run_project_activate(ProjectActivateOptions {
            config: config_path,
            user_token_file: token_file,
            project,
            json: true,
        })
        .await
        .unwrap_err();
        assert!(
            error.starts_with("project_activation_reconcile_required:"),
            "{error}"
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /api/runners/config/reload "))
                .count(),
            1,
            "uncertain reload with unchanged generation must never be retried"
        );
        assert!(
            !requests
                .iter()
                .any(|request| request.starts_with("POST /api/projects/resolve-or-register ")),
            "Project mutation must wait until reload generation is reconciled"
        );
    }

    #[tokio::test]
    async fn activation_uncertain_project_reconciles_inventory_without_second_registration() {
        let tmp = canonical_test_tempdir();
        let project = tmp.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let canonical_project = project.canonicalize().unwrap();
        let responses = vec![
            (
                "/api/projects/resolve-or-register",
                json!({"success":false,"output":{"execution_state":"outcome_unknown","error_code":"operation_indeterminate","state_changed":true},"error":"response lost"}),
            ),
            (
                "/api/projects/list",
                json!({"success":true,"output":{"projects":[{"id":"agent:client:demo","agent_project_id":"demo","client_id":"client","path":canonical_project.to_string_lossy()}]}}),
            ),
        ];
        let (server_url, server) = spawn_operator_server(responses);
        let config_path = tmp.path().join("runner.toml");
        activation_config(
            &config_path,
            &server_url,
            std::slice::from_ref(&canonical_project),
            false,
        );
        let token_file = tmp.path().join("user-token");
        std::fs::write(&token_file, TEST_USER_TOKEN).unwrap();

        let output = run_project_activate(ProjectActivateOptions {
            config: config_path,
            user_token_file: token_file,
            project,
            json: true,
        })
        .await
        .unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["reconciled"], true);
        assert_eq!(output["policy"]["config_reloaded"], false);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /api/projects/resolve-or-register "))
                .count(),
            1,
            "uncertain Project operation must reconcile inventory instead of registering again"
        );
    }

    #[test]
    fn register_existing_directory_is_idempotent_and_respects_registry_path() {
        let tmp = canonical_test_tempdir();
        let root = tmp.path().join("root");
        let project = root.join("demo");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&project).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config(&config_path, &registry, &root);

        let first = run_project_register(ProjectRegisterOptions {
            config: config_path.clone(),
            project: project.clone(),
            json: true,
        })
        .unwrap();
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(first["project"]["id"], "demo");
        assert_eq!(first["project"]["already_registered"], false);
        assert_eq!(first["runner_reload_required"], true);
        assert_eq!(
            first["runner_config"],
            config_path.to_string_lossy().as_ref()
        );
        assert!(first.get("agent_config").is_none());
        assert_eq!(
            first["project_registry_dir"],
            registry.to_string_lossy().as_ref()
        );
        assert!(first.get("projects_dir").is_none());
        assert!(registry.join("demo.toml").is_file());
        assert!(!first.to_string().contains("secret-not-printed"));

        let second = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project,
            json: true,
        })
        .unwrap();
        let second: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(second["project"]["id"], "demo");
        assert_eq!(second["project"]["already_registered"], true);
        assert_eq!(second["runner_reload_required"], false);
    }

    #[test]
    fn human_registration_output_prioritizes_project_and_reload_action() {
        let tmp = canonical_test_tempdir();
        let root = tmp.path().join("root");
        let project = root.join("demo");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&project).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config(&config_path, &registry, &root);

        let first = run_project_register(ProjectRegisterOptions {
            config: config_path.clone(),
            project: project.clone(),
            json: false,
        })
        .unwrap();
        assert!(first.contains("Project added:"), "{first}");
        let canonical_project = project.canonicalize().unwrap();
        assert!(
            first.contains(&canonical_project.display().to_string()),
            "{first}"
        );
        assert!(first.contains("Runner restart required."), "{first}");
        assert!(first.contains("webcodex runner run --config"), "{first}");
        assert_eq!(
            first.contains("installed as a service"),
            cfg!(target_os = "linux"),
            "{first}"
        );
        assert!(!first.contains("project registry"), "{first}");
        assert!(!first.contains("project record"), "{first}");

        let second = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project,
            json: false,
        })
        .unwrap();
        assert!(second.contains("Project already added:"), "{second}");
        assert!(
            second.contains("No Runner restart is required."),
            "{second}"
        );
    }

    #[test]
    fn omitted_project_registry_dir_uses_the_same_default_as_runner_config_loading() {
        let _guard = crate::webcodex_cli::test_support::env_test_guard();
        let config = RegistrationRunnerConfig {
            project_registry_dir: None,
            legacy_projects_dir: None,
            policy: RegistrationPolicy::default(),
        };
        let base = webcodex_runner_config::paths::default_client_config_base_dir().unwrap();
        let expected = webcodex_runner_config::paths::select_project_registry_dir(&base).unwrap();
        assert_eq!(
            registration_project_registry_dir(&config).unwrap(),
            expected
        );
    }

    #[test]
    fn registration_config_accepts_legacy_projects_dir_alias() {
        let legacy = PathBuf::from("/tmp/legacy-projects.d");
        let config = RegistrationRunnerConfig {
            project_registry_dir: None,
            legacy_projects_dir: Some(legacy.clone()),
            policy: RegistrationPolicy::default(),
        };
        assert_eq!(registration_project_registry_dir(&config).unwrap(), legacy);
    }

    #[test]
    fn registration_config_rejects_both_registry_fields() {
        let config = RegistrationRunnerConfig {
            project_registry_dir: Some(PathBuf::from("/tmp/project-registry")),
            legacy_projects_dir: Some(PathBuf::from("/tmp/projects.d")),
            policy: RegistrationPolicy::default(),
        };
        let error = registration_project_registry_dir(&config).unwrap_err();
        assert!(error.contains("cannot both be configured"), "{error}");
    }

    #[test]
    fn explicit_local_selection_persists_only_exact_root_and_is_idempotent() {
        let tmp = canonical_test_tempdir();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside/repo");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config(&config_path, &registry, &root);
        let opts = ProjectRegisterOptions {
            config: config_path.clone(),
            project: outside.clone(),
            json: true,
        };
        run_project_register(opts.clone()).unwrap();
        let before = std::fs::read(&config_path).unwrap();
        let parsed = read_registration_config(&config_path).unwrap();
        assert_eq!(
            parsed.policy.allowed_roots,
            vec![root, outside.canonicalize().unwrap()]
        );
        run_project_register(opts).unwrap();
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
    }

    #[test]
    fn stale_first_root_does_not_block_later_matching_root() {
        let tmp = canonical_test_tempdir();
        let stale = tmp.path().join("deleted-project");
        let root = tmp.path().join("root");
        let project = root.join("demo");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&project).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(&config_path, &registry, &[stale, root.clone()], false);

        let output = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project,
            json: true,
        })
        .expect("a stale unrelated root must not block a later matching root");
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["project"]["already_registered"], false);
        assert_eq!(
            output["policy"]["allowed_roots"],
            serde_json::json!([root.canonicalize().unwrap().to_string_lossy().to_string()])
        );
        assert!(registry.join("demo.toml").is_file());
    }

    #[test]
    fn stale_root_and_valid_nonmatching_root_allow_explicit_selection() {
        let tmp = canonical_test_tempdir();
        let stale = tmp.path().join("deleted-project");
        let allowed = tmp.path().join("allowed");
        let project = tmp.path().join("outside");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(&config_path, &registry, &[stale, allowed], false);

        let output = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project,
            json: false,
        })
        .unwrap();
        assert!(output.contains("Project"));
        assert!(registry.is_dir());
    }

    #[test]
    fn all_stale_roots_allow_explicit_selection() {
        let tmp = canonical_test_tempdir();
        let project = tmp.path().join("project");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&project).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(
            &config_path,
            &registry,
            &[
                tmp.path().join("deleted-one"),
                tmp.path().join("deleted-two"),
            ],
            false,
        );

        let output = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project,
            json: false,
        })
        .unwrap();
        assert!(output.contains("Project"));
        assert!(registry.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn project_symlink_escape_remains_denied() {
        let tmp = canonical_test_tempdir();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let escape = root.join("escape");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config(&config_path, &registry, &root);

        let error = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project: escape,
            json: false,
        })
        .unwrap_err();
        assert!(error.contains("outside allowed_roots"), "{error}");
        assert!(!registry.exists());
    }

    #[test]
    fn explicit_selection_supplies_authority_without_home() {
        let tmp = canonical_test_tempdir();
        let _lock = crate::webcodex_cli::test_support::env_test_guard();
        let _env = crate::webcodex_cli::test_support::EnvGuard::new()
            .remove("HOME")
            .remove("USERPROFILE")
            .remove("HOMEDRIVE")
            .remove("HOMEPATH");
        let project = tmp.path().join("repo");
        std::fs::create_dir(&project).unwrap();
        let (_, roots, changed) =
            register_existing_project(&tmp.path().join("registry"), &project, &[], false, None)
                .unwrap();
        assert!(changed);
        assert_eq!(roots, vec![project.canonicalize().unwrap()]);
    }

    #[test]
    fn local_selection_rejects_parent_traversal() {
        let tmp = canonical_test_tempdir();
        #[cfg(windows)]
        let traversal = PathBuf::from(format!(r"{}\..", tmp.path().display()));
        #[cfg(not(windows))]
        let traversal = tmp.path().join("../");
        let error =
            register_existing_project(&tmp.path().join("registry"), &traversal, &[], false, None)
                .unwrap_err();
        assert!(error.contains("parent traversal"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn raw_unc_and_verbatim_unc_proceed_to_project_canonicalization() {
        let tmp = canonical_test_tempdir();
        let registry = tmp.path().join("registry");
        for network in [
            PathBuf::from(r"\\server\share\webcodex-unreachable-repo"),
            PathBuf::from(r"\\?\UNC\server\share\webcodex-unreachable-repo"),
        ] {
            let error =
                register_existing_project(&registry, &network, &[], true, None).unwrap_err();
            assert!(
                error.contains("does not exist or cannot be resolved"),
                "supported network ingress must reach canonicalization: {error}"
            );
            assert!(
                !error.contains("unsupported Windows project namespace"),
                "{error}"
            );
        }
        assert!(!registry.exists());
    }

    #[cfg(windows)]
    #[test]
    fn unsupported_windows_namespaces_still_fail_before_filesystem_resolution() {
        let tmp = canonical_test_tempdir();
        let registry = tmp.path().join("registry");
        for unsupported in [
            PathBuf::from(r"\\.\device\repo"),
            PathBuf::from(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\repo"),
        ] {
            let error =
                register_existing_project(&registry, &unsupported, &[], true, None).unwrap_err();
            assert!(
                error.contains("unsupported Windows project namespace"),
                "{error}"
            );
            assert!(
                !error.contains("does not exist or cannot be resolved"),
                "{error}"
            );
        }
        assert!(!registry.exists());
    }

    #[cfg(windows)]
    #[test]
    fn user_selected_network_project_authorizes_only_the_exact_canonical_root() {
        let network = PathBuf::from(r"\\?\UNC\NAS\work\repo");
        let configured = vec![PathBuf::from(r"C:\stale-local-root")];
        let (roots, changed) = authorize_canonical_project(&network, &configured, true).unwrap();
        assert!(
            changed,
            "explicit network project selection must extend local authority"
        );
        assert_eq!(roots.len(), configured.len() + 1);
        assert!(roots
            .iter()
            .any(|root| webcodex_runner_config::paths::paths_equal(root, &network)));
        assert!(!roots.iter().any(|root| {
            webcodex_runner_config::paths::paths_equal(root, Path::new(r"\\nas\work"))
                || webcodex_runner_config::paths::paths_equal(root, Path::new(r"\\nas\share"))
        }));
    }

    #[cfg(windows)]
    #[test]
    fn persisted_network_authority_keeps_the_exact_root_in_runner_config() {
        let tmp = canonical_test_tempdir();
        let registry = tmp.path().join("registry");
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(
            &config_path,
            &registry,
            &[PathBuf::from(r"C:\existing")],
            false,
        );
        let network = PathBuf::from(r"\\?\UNC\NAS\work\repo");
        persist_registration_allowed_roots(
            &config_path,
            &[PathBuf::from(r"C:\existing"), network.clone()],
        )
        .unwrap();
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("secret-not-printed"));
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        let roots = parsed["policy"]["allowed_roots"].as_array().unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().any(|value| {
            value.as_str().is_some_and(|root| {
                webcodex_runner_config::paths::paths_equal(Path::new(root), &network)
            })
        }));
    }

    #[cfg(windows)]
    #[test]
    fn raw_local_disk_path_proceeds_to_project_canonicalization() {
        let tmp = canonical_test_tempdir();
        let registry = tmp.path().join("registry");
        let missing = PathBuf::from(r"C:\webcodex-definitely-missing-p2-regression\repo");

        let error = register_existing_project(&registry, &missing, &[], true, None).unwrap_err();
        assert!(
            error.contains("does not exist or cannot be resolved"),
            "local-disk ingress should proceed to canonicalization: {error}"
        );
        assert!(!error.contains("not on a local disk drive"), "{error}");
        assert!(!registry.exists());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_system_root_selection_grants_exact_authority() {
        let tmp = canonical_test_tempdir();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let _guard = crate::webcodex_cli::test_support::env_test_guard();
        let _env = crate::webcodex_cli::test_support::EnvGuard::new()
            .set_os("HOME", home.as_os_str().to_os_string());
        let registry = tmp.path().join("registry");
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(&config_path, &registry, &[], true);

        let output = run_project_register(ProjectRegisterOptions {
            config: config_path.clone(),
            project: PathBuf::from("/etc").canonicalize().unwrap(),
            json: true,
        })
        .unwrap();
        assert!(output.contains("runner_reload_required"));
        let parsed = read_registration_config(&config_path).unwrap();
        assert!(parsed
            .policy
            .allowed_roots
            .contains(&PathBuf::from("/etc").canonicalize().unwrap()));
        assert!(!parsed.policy.allowed_roots.contains(&PathBuf::from("/")));
    }

    #[test]
    fn cwd_anywhere_still_allows_an_ordinary_directory_outside_explicit_roots() {
        let tmp = canonical_test_tempdir();
        let authority = tmp.path().join("authority");
        let project = tmp.path().join("ordinary-project");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&authority).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(&config_path, &registry, &[authority], true);

        let output = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project,
            json: true,
        })
        .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["project"]["already_registered"], false);
        assert!(registry.join("ordinary-project.toml").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_dangerous_root_authority_allows_intentional_registration() {
        let tmp = canonical_test_tempdir();
        let registry = tmp.path().join("registry");
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(&config_path, &registry, &[PathBuf::from("/etc")], true);

        let output = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project: PathBuf::from("/etc"),
            json: true,
        })
        .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["project"]["id"], "etc");
        assert!(registry.join("etc.toml").is_file());
    }

    #[test]
    fn different_paths_with_same_basename_get_stable_collision_suffix() {
        let tmp = canonical_test_tempdir();
        let root = tmp.path().join("root");
        let one = root.join("one/demo");
        let two = root.join("two/demo");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let first = register_existing_project(&registry, &one, &[root.clone()], false, None)
            .unwrap()
            .0;
        let second = register_existing_project(&registry, &two, &[root], false, None)
            .unwrap()
            .0;
        assert_eq!(first.id, "demo");
        assert!(second.id.starts_with("demo-"), "{}", second.id);
        assert_ne!(first.id, second.id);
    }
}
