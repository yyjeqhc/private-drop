use super::*;
use webcodex_core::runner_operation::RunnerFilePayload;

fn payload(root: &Path, content: serde_json::Value) -> RunnerFilePayload {
    RunnerFilePayload {
        cwd: Some(root.to_string_lossy().into_owned()),
        path: ".".to_string(),
        content: Some(content.to_string()),
        max_bytes: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: None,
        end_line: None,
        create_dirs: false,
    }
}

#[cfg(not(feature = "workspace-checkpoints"))]
#[test]
fn workspace_checkpoints_disabled_runner_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let policy = project_policy(root.path());
    std::fs::write(root.path().join("keep.txt"), "unchanged").unwrap();
    for operation in [
        RunnerFileOperation::CheckpointCreate(payload(root.path(), serde_json::json!({}))),
        RunnerFileOperation::CheckpointRestore(payload(root.path(), serde_json::json!({}))),
    ] {
        let result = handle_file_operation(&policy, &operation);
        assert!(
            result.error.as_deref().unwrap().contains("unsupported"),
            "{result:?}"
        );
        assert!(result.stdout.is_none());
        assert!(!is_file_request_kind(operation.wire_kind()));
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("keep.txt")).unwrap(),
        "unchanged"
    );
}

#[cfg(feature = "workspace-checkpoints")]
#[test]
fn workspace_checkpoints_runner_dispatch_create_restore() {
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "git failed");
    };
    git(&["init", "-q"]);
    std::fs::write(root.path().join("file.txt"), "base\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "base",
    ]);
    std::fs::write(root.path().join("file.txt"), "snapshot\n").unwrap();
    let policy = project_policy(root.path());
    let operation =
        RunnerFileOperation::CheckpointCreate(payload(root.path(), serde_json::json!({})));
    assert!(is_file_request_kind(operation.wire_kind()));
    let result = handle_file_operation(&policy, &operation);
    assert!(result.error.is_none(), "{result:?}");
    let mut checkpoint: serde_json::Value =
        serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
    assert!(checkpoint.get("error").is_none(), "{checkpoint}");
    checkpoint["checkpoint_id"] = serde_json::json!("wc_ckpt_test");
    std::fs::write(root.path().join("file.txt"), "later\n").unwrap();
    let operation = RunnerFileOperation::CheckpointRestore(payload(
        root.path(),
        serde_json::json!({"checkpoint": checkpoint}),
    ));
    let result = handle_file_operation(&policy, &operation);
    assert!(result.error.is_none(), "{result:?}");
    let restored: serde_json::Value =
        serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
    assert_eq!(restored["restored"], true, "{restored}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "snapshot\n"
    );
}
