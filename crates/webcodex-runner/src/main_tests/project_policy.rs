use super::*;

#[test]
fn register_project_rejects_path_outside_allowed_roots() {
    let allowed = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let project_registry_dir = allowed.path().join("project-registry");
    let policy = project_policy(allowed.path());
    let req = project_request(
        "register_project",
        serde_json::json!({
            "id": "outside",
            "name": "Outside",
            "path": outside.path().to_string_lossy()
        }),
    );

    let err = project_err(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(err, "path_outside_allowed_roots");
    assert!(!project_registry_dir.join("outside.toml").exists());
}

#[test]
fn register_project_rejects_dangerous_subpaths_without_explicit_root() {
    let policy = RunnerPolicy {
        allow_cwd_anywhere: true,
        allowed_roots: Vec::new(),
        ..RunnerPolicy::default()
    };

    // Dangerous system roots are platform-specific: the well-known Unix trees,
    // or the Windows OS trees (which must still be local-disk paths to reach
    // the dangerous-root check at all).
    #[cfg(windows)]
    let dangerous_paths: &[&str] = &[
        r"C:\Windows\System32\drivers\etc",
        r"C:\Program Files\WebCodex",
        r"C:\Program Files (x86)\something",
    ];
    #[cfg(not(windows))]
    let dangerous_paths: &[&str] = &[
        "/etc/nginx",
        "/usr/local",
        "/var/lib",
        "/proc/self",
        "/dev/shm",
    ];
    for path in dangerous_paths {
        let err = validate_project_path_policy(&policy, Path::new(path)).unwrap_err();
        assert!(err.contains("dangerous system root"), "{path}: {err}");
    }

    #[cfg(windows)]
    let safe_path = r"C:\Users\alice\projects";
    #[cfg(not(windows))]
    let safe_path = "/usr2/local";
    validate_project_path_policy(&policy, Path::new(safe_path)).unwrap();
}

#[test]
fn load_config_defaults_empty_allowed_roots_to_home() {
    let _guard = test_env_lock();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runner.toml");
        std::fs::write(
            &path,
            "server_url = \"http://x\"\ntoken = \"t\"\nclient_id = \"c\"\nproject_registry_dir = \"project-registry\"\n",
        )
        .unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(
            cfg.policy.allowed_roots,
            vec![home],
            "empty allowed_roots must default to HOME"
        );
    }
}

#[test]
fn runner_config_defaults_allow_cwd_anywhere_to_false() {
    let base = "server_url = \"http://x\"\ntoken = \"t\"\nclient_id = \"c\"\nproject_registry_dir = \"project-registry\"\n";

    // This is a serde/default-policy invariant, not a per-user path test. Parse
    // the fixture directly so it cannot observe ambient HOME/USERPROFILE.
    for (label, body) in [
        ("no [policy] section", base.to_string()),
        (
            "[policy] without allow_cwd_anywhere",
            format!("{base}\n[policy]\nallow_raw_shell = true\n"),
        ),
    ] {
        let cfg: RunnerConfig = toml::from_str(&body).unwrap();
        assert!(
            !cfg.policy.allow_cwd_anywhere,
            "{label}: allow_cwd_anywhere must default to false"
        );
    }
}

#[test]
fn default_policy_denies_paths_outside_allowed_roots() {
    // The shipped default must not resolve an absolute path outside the
    // configured roots. `RunnerPolicy::default()` has no roots at all, so
    // every path is out of bounds.
    let policy = RunnerPolicy::default();
    assert!(!policy.allow_cwd_anywhere);
    let err = resolve_requested_path(&policy, Some("/tmp"), "/etc/passwd")
        .expect_err("default policy must not reach /etc/passwd");
    assert!(err.contains("outside allowed_roots"), "{err}");

    // With HOME as the root — what `effective_allowed_roots` fills in — a
    // path inside the root still resolves, so the default is restrictive
    // rather than broken.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("in-bounds.txt"), "ok").unwrap();
    let scoped = RunnerPolicy {
        allowed_roots: vec![root.clone()],
        ..RunnerPolicy::default()
    };
    resolve_requested_path(&scoped, Some(root.to_str().unwrap()), "in-bounds.txt")
        .expect("in-bounds path must still resolve under the fail-closed default");
}

#[test]
fn configured_skill_roots_do_not_expand_generic_file_authority() {
    let project = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    let project_root = project.path().canonicalize().unwrap();
    let skill_root = skills.path().canonicalize().unwrap();
    let outside_file = skill_root.join("outside.txt");
    std::fs::write(&outside_file, "must stay outside project authority").unwrap();

    let config = RunnerConfig {
        policy: RunnerPolicy {
            allowed_roots: vec![project_root.clone()],
            ..RunnerPolicy::default()
        },
        skills: crate::webcodex_runner::config::SkillsConfig {
            roots: vec![skill_root.clone()],
        },
        ..test_config(project_root)
    };
    assert_eq!(config.skills.roots, vec![skill_root]);
    let error = resolve_requested_path(
        &config.policy,
        None,
        outside_file.to_string_lossy().as_ref(),
    )
    .expect_err("configured Skill roots must not grant generic file authority");
    assert!(error.contains("outside allowed_roots"), "{error}");
}

#[test]
fn load_config_explicit_allowed_roots_override_home_default() {
    let _guard = test_env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runner.toml");
    std::fs::write(
            &path,
            "server_url = \"http://x\"\ntoken = \"t\"\nclient_id = \"c\"\nproject_registry_dir = \"project-registry\"\n[policy]\nallowed_roots = [\"/root/git\"]\n",
        )
        .unwrap();
    let cfg = load_config(&path).unwrap();
    assert_eq!(
        cfg.policy.allowed_roots,
        vec![PathBuf::from("/root/git")],
        "explicit allowed_roots must override the HOME default"
    );
}

#[test]
fn load_config_empty_roots_without_home_and_no_cwd_anywhere_errors() {
    let _guard = test_env_lock();
    // Windows derives the allowed-root default from USERPROFILE, so both
    // home sources must be absent to exercise the fail-closed branch.
    let _env = EnvGuard::new()
        .remove("HOME")
        .remove("USERPROFILE")
        .remove("APPDATA");
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runner.toml");
    std::fs::write(
        &path,
        "server_url = \"http://x\"\ntoken = \"t\"\nclient_id = \"c\"\nproject_registry_dir = \"project-registry\"\n\
             [policy]\nallow_cwd_anywhere = false\n",
    )
    .unwrap();
    let err = load_config(&path).unwrap_err();
    assert!(err.contains("allowed_roots is empty"));
}
