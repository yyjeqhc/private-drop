use super::*;

#[test]
fn file_structured_edits_reject_canonical_boundary_escapes() {
    for (alias, expected_error) in [("external", "escapes project"), ("protected", "sensitive")] {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("existing.txt"), "original\n").unwrap();
        std::fs::write(project.join(".git/existing.txt"), "original\n").unwrap();
        std::fs::write(project.join("source.txt"), "original\n").unwrap();
        std::os::unix::fs::symlink(&outside, project.join("external")).unwrap();
        std::os::unix::fs::symlink(".git", project.join("protected")).unwrap();
        // Both projects are allowed to the Runner; each request must still stay
        // inside its own project and outside that project's protected paths.
        let policy = project_policy(temp.path());
        let target = format!("{alias}/nested/new.txt");
        let existing = format!("{alias}/existing.txt");
        let hash = sha256_hex_bytes(b"original\n");
        let cases = vec![
            (
                "file_write_project_file",
                target.clone(),
                serde_json::json!({"content": "replacement\n"}),
            ),
            (
                "file_write_project_file",
                existing.clone(),
                serde_json::json!({"content": "replacement\n", "overwrite": true, "expected_sha256": hash}),
            ),
            (
                "file_apply_text_edits",
                "routing-placeholder".to_string(),
                serde_json::json!({"changes": [
                    {"kind": "create", "path": "would-create.txt", "content": "first\n"},
                    {"kind": "create", "path": target, "content": "replacement\n"}
                ]}),
            ),
            (
                "file_apply_text_edits",
                "routing-placeholder".to_string(),
                serde_json::json!({"changes": [{"kind": "edit", "path": existing, "expected_sha256": hash,
                    "edits": [{"kind": "replace_exact", "old_text": "original", "new_text": "replacement"}]}]}),
            ),
            (
                "file_apply_text_edits",
                "routing-placeholder".to_string(),
                serde_json::json!({"changes": [{"kind": "delete", "path": existing, "expected_sha256": hash}]}),
            ),
            (
                "file_apply_text_edits",
                "routing-placeholder".to_string(),
                serde_json::json!({"changes": [{"kind": "rename", "path": "source.txt", "to_path": target, "expected_sha256": hash}]}),
            ),
            (
                "file_apply_patch",
                "routing-placeholder".to_string(),
                serde_json::json!({"patch": format!("*** Begin Patch\n*** Add File: would-create.txt\n+first\n*** Add File: {target}\n+replacement\n*** End Patch")}),
            ),
            (
                "file_apply_patch",
                "routing-placeholder".to_string(),
                serde_json::json!({"patch": format!("*** Begin Patch\n*** Update File: {existing}\n-original\n+replacement\n*** End Patch")}),
            ),
            (
                "file_apply_patch",
                "routing-placeholder".to_string(),
                serde_json::json!({"patch": format!("*** Begin Patch\n*** Delete File: {existing}\n*** End Patch")}),
            ),
            (
                "file_apply_patch",
                "routing-placeholder".to_string(),
                serde_json::json!({"patch": format!("*** Begin Patch\n*** Update File: source.txt\n*** Move to: {target}\n-original\n+replacement\n*** End Patch")}),
            ),
        ];

        for (kind, path, payload) in cases {
            let output = line_edit_json(handle_file_request(
                &policy,
                &json_file_op_request(&project, kind, &path, payload),
            ));
            assert!(
                output["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(expected_error),
                "{kind} accepted {alias}: {output}"
            );
            assert_eq!(output["state_changed"], false);
            assert_eq!(
                std::fs::read(outside.join("existing.txt")).unwrap(),
                b"original\n"
            );
            assert_eq!(
                std::fs::read(project.join(".git/existing.txt")).unwrap(),
                b"original\n"
            );
            assert_eq!(
                std::fs::read(project.join("source.txt")).unwrap(),
                b"original\n"
            );
            assert!(!outside.join("nested").exists());
            assert!(!project.join(".git/nested").exists());
            assert!(!project.join("would-create.txt").exists());
        }
    }
}

#[test]
fn file_structured_edits_allow_internal_directory_aliases() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::os::unix::fs::symlink("src", root.path().join("alias")).unwrap();
    let output = line_edit_json(handle_file_request(
        &project_policy(root.path()),
        &json_file_op_request(
            root.path(),
            "file_write_project_file",
            "alias/nested/new.txt",
            serde_json::json!({"content": "created\n"}),
        ),
    ));
    assert_eq!(output["created"], true, "{output}");
    assert_eq!(
        std::fs::read(root.path().join("src/nested/new.txt")).unwrap(),
        b"created\n"
    );
}
