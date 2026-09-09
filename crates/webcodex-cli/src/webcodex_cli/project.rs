use serde::Deserialize;
use std::path::{Path, PathBuf};

use super::connect::profile::{atomic_write, render_project_file, resolve_project};
use super::shell_command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRegisterOptions {
    pub(crate) config: PathBuf,
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
    let effective_roots =
        webcodex_runner_config::effective_allowed_roots(configured_roots, allow_cwd_anywhere)?;
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
        Err(error) => {
            #[cfg(windows)]
            if webcodex_runner_config::paths::is_windows_network_share_path(canonical_project) {
                let mut effective_roots = effective_roots;
                let mut canonical_roots = canonical_roots;
                // Local CLI/Desktop project selection is an explicit user grant. Add only the
                // canonical project itself; never infer authority for its parent/share.
                effective_roots.push(canonical_project.to_path_buf());
                canonical_roots.push(canonical_project.to_path_buf());
                webcodex_runner_config::paths::validate_project_path_policy(
                    canonical_project,
                    &canonical_roots,
                    allow_cwd_anywhere,
                )?;
                return Ok((effective_roots, true));
            }

            Err(error)
        }
    }
}

fn prepare_project_authority(
    project: &Path,
    configured_roots: &[PathBuf],
    allow_cwd_anywhere: bool,
) -> Result<PreparedProjectAuthority, String> {
    webcodex_runner_config::paths::validate_project_path_ingress(project)?;
    let canonical_project = canonical_existing_directory(project, "project path")?;
    let (allowed_roots, authority_changed) =
        authorize_canonical_project(&canonical_project, configured_roots, allow_cwd_anywhere)?;
    Ok(PreparedProjectAuthority {
        canonical_project,
        allowed_roots,
        authority_changed,
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

fn persist_registration_allowed_roots(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read Runner config {}: {error}", path.display()))?;
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
    atomic_write(path, document.to_string().as_bytes(), true)?;
    Ok(())
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
        // newly registered network project cannot outlive the Runner authority it needs.
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
    fn outside_allowed_roots_is_rejected_without_registry_mutation() {
        let tmp = canonical_test_tempdir();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config(&config_path, &registry, &root);
        let error = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project: outside,
            json: false,
        })
        .unwrap_err();
        assert!(error.contains("outside allowed_roots"), "{error}");
        assert!(!registry.exists());
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
    fn stale_root_and_valid_nonmatching_root_remain_denied() {
        let tmp = canonical_test_tempdir();
        let stale = tmp.path().join("deleted-project");
        let allowed = tmp.path().join("allowed");
        let project = tmp.path().join("outside");
        let registry = tmp.path().join("registry");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(&config_path, &registry, &[stale, allowed], false);

        let error = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project,
            json: false,
        })
        .unwrap_err();
        assert!(error.contains("outside allowed_roots"), "{error}");
        assert!(!registry.exists());
    }

    #[test]
    fn all_stale_roots_remain_denied() {
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

        let error = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project,
            json: false,
        })
        .unwrap_err();
        assert!(error.contains("outside allowed_roots"), "{error}");
        assert!(error.contains("allow_cwd_anywhere is false"), "{error}");
        assert!(!registry.exists());
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
    fn stale_roots_do_not_relax_allow_cwd_anywhere_false() {
        let tmp = canonical_test_tempdir();
        let project = tmp.path().join("ordinary-project");
        std::fs::create_dir_all(&project).unwrap();
        let stale = tmp.path().join("deleted-project");

        let denied_registry = tmp.path().join("denied-registry");
        let denied = register_existing_project(
            &denied_registry,
            &project,
            std::slice::from_ref(&stale),
            false,
            None,
        )
        .unwrap_err();
        assert!(denied.contains("allow_cwd_anywhere is false"), "{denied}");
        assert!(!denied_registry.exists());

        let allowed_registry = tmp.path().join("allowed-registry");
        register_existing_project(&allowed_registry, &project, &[stale], true, None)
            .expect("the existing allow_cwd_anywhere relaxation must remain unchanged");
        assert!(allowed_registry.join("ordinary-project.toml").is_file());
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
    fn cwd_anywhere_rejects_dangerous_root_without_mutating_registry() {
        let tmp = canonical_test_tempdir();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let _guard = crate::webcodex_cli::test_support::env_test_guard();
        let _env = crate::webcodex_cli::test_support::EnvGuard::new()
            .set_os("HOME", home.as_os_str().to_os_string());
        let registry = tmp.path().join("registry");
        let config_path = tmp.path().join("runner.toml");
        config_with_policy(&config_path, &registry, &[], true);

        let error = run_project_register(ProjectRegisterOptions {
            config: config_path,
            project: PathBuf::from("/etc"),
            json: true,
        })
        .unwrap_err();
        assert!(error.contains("dangerous system root"), "{error}");
        assert!(
            !registry.exists(),
            "policy failure must not mutate registry"
        );
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
