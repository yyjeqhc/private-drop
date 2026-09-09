use super::*;

#[test]
fn register_project_writes_valid_toml_into_project_registry_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    let policy = project_policy(tmp.path());
    let req = project_request(
        "register_project",
        serde_json::json!({
            "id": "demo",
            "name": "Demo",
            "path": project_dir.to_string_lossy(),
            "description": "A demo project",
            "allow_patch": false
        }),
    );

    let value = project_ok(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(value["created_config"], true);
    assert_eq!(value["overwritten"], false);
    assert_eq!(
        value["project_record_path"], value["projects_config_path"],
        "legacy projects_config_path must remain an additive alias"
    );
    let expected_record_path =
        std::fs::canonicalize(project_registry_dir.join("demo.toml")).unwrap();
    assert_eq!(
        value["project_record_path"],
        expected_record_path.to_string_lossy().as_ref()
    );

    let content = std::fs::read_to_string(project_registry_dir.join("demo.toml")).unwrap();
    let parsed = parse_runner_project_toml(&content).unwrap();
    assert_eq!(parsed.id, "demo");
    assert_eq!(parsed.name.as_deref(), Some("Demo"));
    assert_eq!(parsed.path, project_dir.to_string_lossy());
    assert_eq!(parsed.registration_source, None);
    assert!(!parsed.allow_patch);
}

#[test]
fn resolve_or_register_project_persists_and_reuses_canonical_directory_without_touching_it() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("Example Repo");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    std::fs::write(project_dir.join("keep.txt"), "unchanged").unwrap();
    let target_entries_before = std::fs::read_dir(&project_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let policy = project_policy(tmp.path());

    let first = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": project_dir.join(".").to_string_lossy()}),
        ),
    ));
    assert_eq!(first["outcome"], "auto_registered");
    assert_eq!(first["registered"], true);
    assert_eq!(first["changed"], true);
    assert_eq!(first["registration_source"], "auto_registered");
    assert_eq!(
        first["kind"], "auto_registered",
        "legacy kind sentinel is a wire-only old-Server compatibility projection"
    );
    let project_id = first["agent_project_id"].as_str().unwrap();
    assert!(project_id.starts_with("example-repo-"), "{project_id}");
    assert!(project_id.len() <= 64);

    let config_path = project_registry_dir.join(format!("{project_id}.toml"));
    let persisted =
        parse_runner_project_toml(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(persisted.id, project_id);
    assert_eq!(
        Path::new(&persisted.path),
        project_dir.canonicalize().unwrap()
    );
    assert_eq!(persisted.kind, None);
    assert_eq!(
        persisted.registration_source.as_deref(),
        Some("auto_registered")
    );
    let persisted_text = std::fs::read_to_string(&config_path).unwrap();
    assert!(persisted_text.contains("registration_source = \"auto_registered\""));
    assert!(!persisted_text.contains("kind = \"auto_registered\""));
    assert!(persisted.allow_patch);
    assert_eq!(
        std::fs::read_to_string(project_dir.join("keep.txt")).unwrap(),
        "unchanged"
    );
    let target_entries_after = std::fs::read_dir(&project_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(target_entries_after, target_entries_before);
    assert!(!project_dir.join(".git").exists());

    let reloaded = load_runner_project_summaries_from_dir(&project_registry_dir);
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].id, project_id);
    assert_eq!(
        reloaded[0].registration_source.as_deref(),
        Some("auto_registered")
    );
    assert_eq!(
        reloaded[0].kind.as_deref(),
        Some("auto_registered"),
        "legacy kind sentinel is wire-only rolling-upgrade compatibility"
    );
    let second = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": project_dir.to_string_lossy()}),
        ),
    ));
    assert_eq!(second["outcome"], "reused_existing_registration");
    assert_eq!(second["registered"], false);
    assert_eq!(second["agent_project_id"], project_id);
    assert_eq!(
        std::fs::read_dir(&project_registry_dir).unwrap().count(),
        1,
        "retry created a duplicate registration"
    );
}

#[cfg(unix)]
#[test]
fn resolve_or_register_project_reuses_symlink_target() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    let link = tmp.path().join("repo-link");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    symlink(&project_dir, &link).unwrap();
    let policy = project_policy(tmp.path());

    let first = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": project_dir.to_string_lossy()}),
        ),
    ));
    let second = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": link.to_string_lossy()}),
        ),
    ));
    assert_eq!(second["outcome"], "reused_existing_registration");
    assert_eq!(second["agent_project_id"], first["agent_project_id"]);
    assert_eq!(
        Path::new(second["path"].as_str().unwrap()),
        project_dir.canonicalize().unwrap()
    );
}

#[test]
fn resolve_or_register_project_prefers_manual_id_and_distinguishes_same_basenames() {
    let tmp = tempfile::tempdir().unwrap();
    let first_parent = tmp.path().join("first");
    let second_parent = tmp.path().join("second");
    let manual_dir = tmp.path().join("manual");
    let project_registry_dir = tmp.path().join("project-registry");
    for directory in [
        first_parent.join("repo"),
        second_parent.join("repo"),
        manual_dir.clone(),
    ] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::create_dir(&project_registry_dir).unwrap();
    std::fs::write(
        project_registry_dir.join("friendly.toml"),
        format!(
            "id = \"friendly\"\nname = \"Friendly\"\npath = {:?}\nallow_patch = true\n",
            manual_dir.to_string_lossy()
        ),
    )
    .unwrap();
    let policy = project_policy(tmp.path());

    let manual = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": manual_dir.join(".").to_string_lossy()}),
        ),
    ));
    assert_eq!(manual["outcome"], "reused_existing_registration");
    assert_eq!(manual["agent_project_id"], "friendly");

    let first = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": first_parent.join("repo").to_string_lossy()}),
        ),
    ));
    let second = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": second_parent.join("repo").to_string_lossy()}),
        ),
    ));
    assert_ne!(first["agent_project_id"], second["agent_project_id"]);
    assert!(first["agent_project_id"]
        .as_str()
        .unwrap()
        .starts_with("repo-"));
    assert!(second["agent_project_id"]
        .as_str()
        .unwrap()
        .starts_with("repo-"));
}

#[test]
fn resolve_or_register_project_fails_closed_for_disabled_and_ambiguous_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    std::fs::create_dir(&project_registry_dir).unwrap();
    std::fs::write(
        project_registry_dir.join("disabled.toml"),
        format!(
            "id = \"disabled\"\npath = {:?}\ndisabled = true\n",
            project_dir.to_string_lossy()
        ),
    )
    .unwrap();
    let policy = project_policy(tmp.path());

    let disabled = project_error_value(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": project_dir.to_string_lossy()}),
        ),
    ));
    assert_eq!(disabled["error_kind"], "project_disabled");
    assert_eq!(disabled["matching_project_id"], "disabled");
    assert_eq!(disabled["state_changed"], false);
    assert!(
        !disabled.to_string().contains(project_dir.to_str().unwrap()),
        "disabled error leaked the absolute path"
    );

    std::fs::write(
        project_registry_dir.join("alpha.toml"),
        format!(
            "id = \"alpha\"\npath = {:?}\n",
            project_dir.to_string_lossy()
        ),
    )
    .unwrap();
    std::fs::write(
        project_registry_dir.join("zeta.toml"),
        format!(
            "id = \"zeta\"\npath = {:?}\n",
            project_dir.join(".").to_string_lossy()
        ),
    )
    .unwrap();
    let ambiguous = project_error_value(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": project_dir.to_string_lossy()}),
        ),
    ));
    assert_eq!(ambiguous["error_kind"], "ambiguous_project_path");
    assert_eq!(
        ambiguous["matching_project_ids"],
        serde_json::json!(["alpha", "disabled", "zeta"])
    );
    assert_eq!(ambiguous["state_changed"], false);
}

#[test]
fn resolve_or_register_project_rejects_invalid_non_directory_and_disallowed_paths() {
    let allowed = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let project_registry_dir = allowed.path().join("project-registry");
    let file = allowed.path().join("file.txt");
    std::fs::write(&file, "not a directory").unwrap();
    let policy = project_policy(allowed.path());

    for (path, expected) in [
        ("relative/path".to_string(), "invalid_project_path"),
        (
            allowed.path().join("missing").to_string_lossy().to_string(),
            "project_path_not_found",
        ),
        (
            file.to_string_lossy().to_string(),
            "project_path_not_directory",
        ),
        (
            outside.path().to_string_lossy().to_string(),
            "path_outside_allowed_roots",
        ),
    ] {
        let error = project_error_value(handle_resolve_or_register_project(
            &policy,
            &project_registry_dir,
            &project_request(
                "resolve_or_register_project",
                serde_json::json!({"path": path}),
            ),
        ));
        assert_eq!(error["error_kind"], expected);
        assert_eq!(error["state_changed"], false);
    }
    assert!(!project_registry_dir.exists());

    let unrestricted = RunnerPolicy {
        allow_cwd_anywhere: true,
        allowed_roots: Vec::new(),
        ..RunnerPolicy::default()
    };
    // Dangerous system roots are platform-specific: `/etc` on Unix,
    // `C:\Windows` on Windows (drive roots are also rejected).
    #[cfg(windows)]
    let dangerous_path = "C:\\Windows";
    #[cfg(not(windows))]
    let dangerous_path = "/etc";
    let dangerous = project_error_value(handle_resolve_or_register_project(
        &unrestricted,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": dangerous_path}),
        ),
    ));
    assert_eq!(dangerous["error_kind"], "path_outside_allowed_roots");
}

#[cfg(windows)]
#[test]
fn resolve_or_register_project_network_paths_reach_filesystem_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let project_registry_dir = tmp.path().join("project-registry");
    let policy = project_policy(tmp.path());

    for network_path in [
        r"\\server\share\webcodex-unreachable-repo",
        r"\\?\UNC\server\share\webcodex-unreachable-repo",
    ] {
        let error = project_error_value(handle_resolve_or_register_project(
            &policy,
            &project_registry_dir,
            &project_request(
                "resolve_or_register_project",
                serde_json::json!({"path": network_path}),
            ),
        ));
        assert_eq!(
            error["error_kind"], "project_path_not_found",
            "supported network path {network_path} must reach filesystem resolution"
        );
        assert_eq!(error["state_changed"], false);
    }
    assert!(!project_registry_dir.exists());
}

#[cfg(windows)]
#[test]
fn resolve_or_register_project_rejects_unsupported_windows_namespaces() {
    let tmp = tempfile::tempdir().unwrap();
    let project_registry_dir = tmp.path().join("project-registry");
    let policy = project_policy(tmp.path());
    for unsupported in [
        r"\\.\device\repo",
        r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\repo",
    ] {
        let error = project_error_value(handle_resolve_or_register_project(
            &policy,
            &project_registry_dir,
            &project_request(
                "resolve_or_register_project",
                serde_json::json!({"path": unsupported}),
            ),
        ));
        assert_eq!(error["error_kind"], "windows_project_path_unsupported");
        assert_eq!(error["state_changed"], false);
    }
    assert!(!project_registry_dir.exists());
}

#[cfg(windows)]
#[test]
fn model_facing_policy_does_not_auto_authorize_network_share_with_allow_anywhere() {
    let policy = RunnerPolicy {
        allow_cwd_anywhere: true,
        allowed_roots: Vec::new(),
        ..RunnerPolicy::default()
    };
    let error = validate_project_path_policy(&policy, Path::new(r"\\server\share\repo"))
        .expect_err("model-facing RunnerPolicy must not auto-authorize a network share");
    assert!(error.contains("outside allowed_roots"), "{error}");
}

#[cfg(windows)]
#[test]
fn resolve_or_register_project_accepts_local_drive_and_verbatim_disk_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    let policy = project_policy(tmp.path());

    // Plain local-drive path registers normally.
    let plain = project_dir.to_string_lossy().to_string();
    assert!(Path::new(&plain).is_absolute());
    let first = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": plain}),
        ),
    ));
    assert_eq!(first["outcome"], "auto_registered");
    let project_id = first["agent_project_id"].as_str().unwrap().to_string();

    // The canonicalized `\\?\C:\...` spelling of the same directory must
    // reuse the registration instead of minting a duplicate identity. The
    // verbatim form is built from the plain path: `canonicalize()` already
    // returns `\\?\`-prefixed paths on modern Rust, so re-prefixing those
    // would double the prefix.
    let raw = project_dir.to_string_lossy().to_string();
    let verbatim = if raw.starts_with(r"\\?\") {
        raw
    } else {
        // `\\?\` + the raw path: the prefix itself ends with a backslash.
        format!(r"\\?\{raw}")
    };
    let second = project_ok(handle_resolve_or_register_project(
        &policy,
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": verbatim}),
        ),
    ));
    assert_eq!(second["outcome"], "reused_existing_registration");
    assert_eq!(second["agent_project_id"], project_id);
    assert_eq!(
        std::fs::read_dir(&project_registry_dir).unwrap().count(),
        1,
        "the \\\\?\\ spelling created a duplicate project identity"
    );
}

#[cfg(windows)]
#[test]
fn register_project_network_path_reaches_filesystem_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let project_registry_dir = tmp.path().join("project-registry");
    let policy = project_policy(tmp.path());

    let result = handle_project_op(
        &policy,
        &project_registry_dir,
        &project_request(
            "register_project",
            serde_json::json!({
                "id": "demo",
                "name": "Demo",
                "path": r"\\server\share\webcodex-unreachable-repo",
                "description": "UNC project",
                "allow_patch": false
            }),
        ),
    );
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("does not exist or cannot be canonicalized"),
        "supported UNC registration must reach filesystem resolution: {error}"
    );
    assert!(
        !error.contains("windows_project_path_unsupported"),
        "{error}"
    );
    assert!(!project_registry_dir.exists());
}

#[test]
fn concurrent_path_resolution_converges_on_one_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    let policy = project_policy(tmp.path());
    let mut workers = Vec::new();
    for _ in 0..2 {
        let project_dir = project_dir.clone();
        let project_registry_dir = project_registry_dir.clone();
        let policy = policy.clone();
        workers.push(std::thread::spawn(move || {
            project_ok(handle_resolve_or_register_project(
                &policy,
                &project_registry_dir,
                &project_request(
                    "resolve_or_register_project",
                    serde_json::json!({"path": project_dir.to_string_lossy()}),
                ),
            ))
        }));
    }
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results[0]["agent_project_id"],
        results[1]["agent_project_id"]
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| result["registered"] == true)
            .count(),
        1
    );
    assert_eq!(std::fs::read_dir(&project_registry_dir).unwrap().count(), 1);
}

#[test]
fn auto_project_id_collision_extends_hash_without_overwriting() {
    use sha2::{Digest, Sha256};

    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    let other_dir = tmp.path().join("other");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    std::fs::create_dir(&other_dir).unwrap();
    std::fs::create_dir(&project_registry_dir).unwrap();
    let canonical = project_dir.canonicalize().unwrap();
    // Match the Runner's project identity: raw bytes on Unix, normalized
    // (lowercased, `\\?\` stripped) on Windows.
    #[cfg(windows)]
    let identity = webcodex_runner_config::paths::normalize_path_identity(&canonical);
    #[cfg(not(windows))]
    let identity = canonical.to_string_lossy().to_string();
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    let colliding_id = format!("repo-{}", &digest[..8]);
    let colliding_config = format!(
        "id = {:?}\npath = {:?}\n",
        colliding_id,
        other_dir.to_string_lossy()
    );
    std::fs::write(
        project_registry_dir.join(format!("{colliding_id}.toml")),
        &colliding_config,
    )
    .unwrap();

    let result = project_ok(handle_resolve_or_register_project(
        &project_policy(tmp.path()),
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": project_dir.to_string_lossy()}),
        ),
    ));
    let generated = result["agent_project_id"].as_str().unwrap();
    assert_ne!(generated, colliding_id);
    assert_eq!(generated, format!("repo-{}", &digest[..12]));
    assert_eq!(
        std::fs::read_to_string(project_registry_dir.join(format!("{colliding_id}.toml"))).unwrap(),
        colliding_config
    );
}

#[test]
fn path_registration_publish_failure_leaves_no_config_or_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    webcodex_runner::projects::fail_next_project_publish_before_rename();

    let error = project_error_value(handle_resolve_or_register_project(
        &project_policy(tmp.path()),
        &project_registry_dir,
        &project_request(
            "resolve_or_register_project",
            serde_json::json!({"path": project_dir.to_string_lossy()}),
        ),
    ));
    assert_eq!(error["error_kind"], "operation_failed");
    assert_eq!(error["state_changed"], false);
    assert_eq!(std::fs::read_dir(&project_registry_dir).unwrap().count(), 0);
}

#[test]
fn register_project_overwrite_semantics_are_accurate() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    let policy = project_policy(tmp.path());
    let payload = |overwrite| {
        serde_json::json!({
            "id": "demo",
            "name": "Demo",
            "path": project_dir.to_string_lossy(),
            "overwrite": overwrite
        })
    };

    let first = project_ok(handle_project_op(
        &policy,
        &project_registry_dir,
        &project_request("register_project", payload(false)),
    ));
    assert_eq!(first["created_config"], true);
    assert_eq!(first["overwritten"], false);

    let retry = project_ok(handle_project_op(
        &policy,
        &project_registry_dir,
        &project_request("register_project", payload(false)),
    ));
    assert_eq!(retry["recovered"], true);
    assert_eq!(retry["changed"], false);

    let overwritten = project_ok(handle_project_op(
        &policy,
        &project_registry_dir,
        &project_request("register_project", payload(true)),
    ));
    assert_eq!(overwritten["created_config"], false);
    assert_eq!(overwritten["overwritten"], true);
}
