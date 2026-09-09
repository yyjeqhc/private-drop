use super::*;

#[test]
fn create_project_basic_creates_readme_and_gitignore() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("new-project");
    let project_registry_dir = tmp.path().join("project-registry");
    let policy = project_policy(tmp.path());
    let req = project_request(
        "create_project",
        serde_json::json!({
            "id": "basic",
            "name": "Basic",
            "path": project_dir.to_string_lossy(),
            "description": "Basic template",
            "template": "basic"
        }),
    );

    let value = project_ok(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(value["created_directory"], true);
    assert!(project_dir.join("README.md").exists());
    assert!(project_dir.join(".gitignore").exists());
    assert!(std::fs::read_to_string(project_dir.join("README.md"))
        .unwrap()
        .contains("Basic template"));
}

#[cfg(windows)]
#[test]
fn create_project_does_not_expand_to_network_shares() {
    let tmp = tempfile::tempdir().unwrap();
    let project_registry_dir = tmp.path().join("project-registry");
    let policy = RunnerPolicy {
        allow_cwd_anywhere: true,
        ..RunnerPolicy::default()
    };
    let error = project_error_value(handle_project_op(
        &policy,
        &project_registry_dir,
        &project_request(
            "create_project",
            serde_json::json!({
                "id": "network",
                "name": "Network",
                "path": r"\\server\share\new-project",
                "template": "empty"
            }),
        ),
    ));
    assert_eq!(error["error_code"], "windows_project_path_unsupported");
    assert!(!project_registry_dir.exists());
}

#[test]
fn create_project_rejects_existing_non_empty_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("existing");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    let keep = project_dir.join("keep.txt");
    std::fs::write(&keep, "keep").unwrap();
    let policy = project_policy(tmp.path());
    let req = project_request(
        "create_project",
        serde_json::json!({
            "id": "existing",
            "name": "Existing",
            "path": project_dir.to_string_lossy(),
            "template": "basic",
            "adopt_existing_empty": true
        }),
    );

    let err = project_err(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(err, "path_not_empty");
    assert_eq!(std::fs::read_to_string(keep).unwrap(), "keep");
}

#[test]
fn create_project_requires_explicit_adoption_of_existing_empty_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("existing-empty");
    let project_registry_dir = tmp.path().join("project-registry");
    std::fs::create_dir(&project_dir).unwrap();
    let policy = project_policy(tmp.path());
    let req = project_request(
        "create_project",
        serde_json::json!({
            "id": "existing-empty",
            "name": "Existing Empty",
            "path": project_dir.to_string_lossy(),
            "template": "empty"
        }),
    );

    let err = project_err(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(err, "path_exists");
    assert!(project_dir.is_dir());
    assert!(std::fs::read_dir(&project_dir).unwrap().next().is_none());
    assert!(!project_registry_dir.exists());

    let adopted_req = project_request(
        "create_project",
        serde_json::json!({
            "id": "existing-empty",
            "name": "Existing Empty",
            "path": project_dir.to_string_lossy(),
            "template": "empty",
            "adopt_existing_empty": true
        }),
    );
    let adopted = project_ok(handle_project_op(
        &policy,
        &project_registry_dir,
        &adopted_req,
    ));
    assert_eq!(adopted["created_directory"], false);
    assert_eq!(adopted["created_config"], true);
    assert!(std::fs::read_dir(&project_dir).unwrap().next().is_none());
}

#[test]
fn create_project_rejects_unknown_template() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("new-project");
    let project_registry_dir = tmp.path().join("project-registry");
    let policy = project_policy(tmp.path());
    let req = project_request(
        "create_project",
        serde_json::json!({
            "id": "badtemplate",
            "name": "Bad Template",
            "path": project_dir.to_string_lossy(),
            "template": "cargo"
        }),
    );

    let err = project_err(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(err, "invalid_request");
    assert!(!project_dir.exists());
}

#[test]
fn create_project_created_config_and_overwritten_semantics_are_accurate() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("empty-project");
    let project_registry_dir = tmp.path().join("project-registry");
    let policy = project_policy(tmp.path());
    let payload = |overwrite| {
        serde_json::json!({
            "id": "empty",
            "name": "Empty",
            "path": project_dir.to_string_lossy(),
            "template": "empty",
            "adopt_existing_empty": true,
            "overwrite": overwrite
        })
    };

    let first = project_ok(handle_project_op(
        &policy,
        &project_registry_dir,
        &project_request("create_project", payload(false)),
    ));
    assert_eq!(first["created_directory"], true);
    assert_eq!(first["created_config"], true);
    assert_eq!(first["overwritten"], false);

    let second = project_ok(handle_project_op(
        &policy,
        &project_registry_dir,
        &project_request("create_project", payload(true)),
    ));
    assert_eq!(second["created_directory"], false);
    assert_eq!(second["created_config"], false);
    assert_eq!(second["overwritten"], true);
}

#[test]
fn create_project_empty_template_with_description_creates_no_project_files() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("empty-with-description");
    let project_registry_dir = tmp.path().join("project-registry");
    let policy = project_policy(tmp.path());
    let req = project_request(
        "create_project",
        serde_json::json!({
            "id": "empty-with-description",
            "name": "Empty With Description",
            "path": project_dir.to_string_lossy(),
            "description": "Registration metadata only",
            "template": "empty"
        }),
    );

    let value = project_ok(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(value["created_directory"], true);
    assert!(project_dir.is_dir());
    assert!(std::fs::read_dir(&project_dir).unwrap().next().is_none());

    let recovered = project_ok(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(recovered["recovered"], true);
    assert_eq!(recovered["changed"], false);
    assert!(std::fs::read_dir(&project_dir).unwrap().next().is_none());
}

#[test]
fn create_project_cleanup_removes_only_files_created_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("existing-empty");
    std::fs::create_dir(&project_dir).unwrap();
    let project_registry_file = tmp.path().join("project-registry-is-file");
    std::fs::write(&project_registry_file, "not a dir").unwrap();
    let policy = project_policy(tmp.path());
    let req = project_request(
        "create_project",
        serde_json::json!({
            "id": "cleanup",
            "name": "Cleanup",
            "path": project_dir.to_string_lossy(),
            "template": "basic",
            "adopt_existing_empty": true
        }),
    );

    let err = project_err(handle_project_op(&policy, &project_registry_file, &req));
    assert_eq!(err, "operation_failed");
    assert!(project_dir.exists());
    assert!(!project_dir.join("README.md").exists());
    assert!(!project_dir.join(".gitignore").exists());
}

#[test]
fn create_project_does_not_delete_pre_existing_files() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("existing");
    std::fs::create_dir(&project_dir).unwrap();
    let pre_existing = project_dir.join("pre-existing.txt");
    std::fs::write(&pre_existing, "original").unwrap();
    let project_registry_file = tmp.path().join("project-registry-is-file");
    std::fs::write(&project_registry_file, "not a dir").unwrap();
    let policy = project_policy(tmp.path());
    let req = project_request(
        "create_project",
        serde_json::json!({
            "id": "keep",
            "name": "Keep",
            "path": project_dir.to_string_lossy(),
            "template": "basic",
            "adopt_existing_empty": true
        }),
    );

    let err = project_err(handle_project_op(&policy, &project_registry_file, &req));
    assert_eq!(err, "path_not_empty");
    assert_eq!(std::fs::read_to_string(pre_existing).unwrap(), "original");
}

#[test]
fn legacy_managed_temporary_create_request_fails_before_filesystem_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let project_registry_dir = tmp.path().join("project-registry");
    let project_dir = tmp.path().join("must-not-be-created");
    let policy = project_policy(tmp.path());
    let req = project_request(
        "create_project",
        serde_json::json!({
            "managed_temporary_project": true,
            "id": "legacy",
            "name": "Legacy",
            "path": project_dir.to_string_lossy(),
        }),
    );

    let err = project_err(handle_project_op(&policy, &project_registry_dir, &req));
    assert_eq!(err, "managed_temporary_projects_retired");
    assert!(!project_dir.exists());
    assert!(!project_registry_dir.exists());
}
