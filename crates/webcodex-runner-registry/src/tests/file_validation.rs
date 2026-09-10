use super::*;

#[test]
fn validate_file_request_rejects_invalid_read_requests() {
    let cases: Vec<(&str, fn(&mut ShellFileOpRequest), &str)> = vec![
        (
            "only start_line",
            |req| req.start_line = Some(10),
            "end_line is required when start_line is set for op=read",
        ),
        (
            "only end_line",
            |req| req.end_line = Some(20),
            "start_line is required when end_line is set for op=read",
        ),
        (
            "inverted line range",
            |req| {
                req.start_line = Some(20);
                req.end_line = Some(10);
            },
            "invalid line range",
        ),
        (
            "zero start_line",
            |req| {
                req.start_line = Some(0);
                req.end_line = Some(10);
            },
            "invalid line range",
        ),
        (
            "old_text field on read",
            |req| req.old_text = Some("old".to_string()),
            "old_text is not supported for any file op",
        ),
        (
            "pattern field on read",
            |req| req.pattern = Some("needle".to_string()),
            "pattern is not supported for any file op",
        ),
        (
            "line field on read",
            |req| req.line = Some(10),
            "line is not supported for any file op",
        ),
        (
            "expected_prefix on read",
            |req| req.expected_prefix = Some("pub fn".to_string()),
            "expected_prefix is only allowed for op=write",
        ),
    ];

    for (case, mutate, expected) in cases {
        let mut req = file_request("read");
        mutate(&mut req);
        let err = validate_file_request(&req).unwrap_err();
        assert_eq!(err, expected, "case: {case}");
    }
}

#[test]
fn generic_file_request_cannot_inject_runner_skill_store_operations() {
    for op in [
        "skill_store",
        "configured_skill_roots",
        "skill_install",
        "skill_activate",
        "skill_versions",
        "skill_remove_revision",
    ] {
        let request = file_request(op);
        let error = validate_file_request(&request).unwrap_err();
        assert!(error.contains("op must be one of"), "{op}: {error}");
    }
}

#[test]
fn validate_file_request_allows_structured_edit_payload_ops() {
    for op in ["write_project_file", "apply_text_edits"] {
        let mut req = file_request(op);
        req.content = Some(r#"{"path":"src/lib.rs"}"#.to_string());

        validate_file_request(&req).unwrap();
    }
}

#[test]
fn validate_file_request_accepts_only_bounded_skill_internal_shapes() {
    let mut list = file_request("skill_list_packages");
    list.path = ".agents/skills".to_string();
    list.content = Some(r#"{"limit":257}"#.to_string());
    validate_file_request(&list).unwrap();

    let mut read = file_request("skill_read_file");
    read.path = ".agents/skills/foo/SKILL.md".to_string();
    read.content =
        Some(r#"{"package_root":".agents/skills/foo","max_file_bytes":65536}"#.to_string());
    read.start_line = Some(1);
    read.end_line = Some(20);
    read.max_bytes = Some(48 * 1024);
    validate_file_request(&read).unwrap();

    let mut missing_range = read.clone();
    missing_range.end_line = None;
    assert!(validate_file_request(&missing_range)
        .unwrap_err()
        .contains("start_line/end_line"));

    let mut list_with_range = list.clone();
    list_with_range.start_line = Some(1);
    list_with_range.end_line = Some(2);
    assert!(validate_file_request(&list_with_range)
        .unwrap_err()
        .contains("only accepts path/cwd/content"));
}

#[test]
fn validate_file_request_rejects_structured_edit_extra_fields() {
    let mut req = file_request("write_project_file");
    req.content = Some(r#"{"path":"src/lib.rs"}"#.to_string());
    req.expected_sha256 =
        Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string());

    let err = validate_file_request(&req).unwrap_err();
    assert!(err.contains("expected_sha256 is only allowed"), "{err}");
}
