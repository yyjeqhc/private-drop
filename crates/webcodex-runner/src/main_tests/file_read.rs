use super::*;

fn file_read_request(
    cwd: &Path,
    path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_bytes: Option<usize>,
) -> RunnerRequest {
    RunnerRequest {
        request_id: "req-file-read".to_string(),
        client_id: "agent-1".to_string(),
        kind: "file_read".to_string(),
        job_id: None,
        cwd: Some(cwd.to_string_lossy().to_string()),
        path: Some(path.to_string()),
        content: None,
        max_bytes,
        expected_sha256: None,
        expected_prefix: None,
        start_line,
        end_line,
        create_dirs: false,
        command: String::new(),
        process: None,
        script: None,
        stdin: None,
        timeout_secs: 30,
        requested_by: "tester".to_string(),
        created_at: 0,
        validation: None,
        lsp: None,
        job_context: None,
        mcp_gateway: None,
        plugin_gateway: None,
        coding_agent: None,
        persistent_shell: None,
    }
}

fn file_read_json(result: CommandResult) -> serde_json::Value {
    assert_eq!(result.exit_code, Some(0), "unexpected result: {:?}", result);
    assert!(
        result.error.is_none(),
        "unexpected error: {:?}",
        result.error
    );
    serde_json::from_str(result.stdout.as_deref().expect("stdout json")).unwrap()
}

#[test]
fn runner_file_read_without_range_preserves_plain_text_output() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("small.txt"), "one\ntwo\n").unwrap();

    let out = handle_file_request(
        &policy,
        &file_read_request(tmp.path(), "small.txt", None, None, Some(1024)),
    );

    assert_eq!(out.exit_code, Some(0), "unexpected result: {out:?}");
    assert_eq!(out.stdout.as_deref(), Some("one\ntwo\n"));
}

#[cfg(unix)]
#[test]
fn runner_file_read_rejects_symlink_escape_even_when_policy_allows_target() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "outside-secret").unwrap();
    std::os::unix::fs::symlink(&secret, project.path().join("leak.txt")).unwrap();

    let mut policy = project_policy(project.path());
    policy.allowed_roots.push(outside.path().to_path_buf());
    let out = handle_file_request(
        &policy,
        &file_read_request(project.path(), "leak.txt", None, None, Some(1024)),
    );

    assert_eq!(out.exit_code, None);
    assert_eq!(out.error.as_deref(), Some("read_file failed: invalid_path"));
    assert!(!out.stdout.unwrap_or_default().contains("outside-secret"));
}

#[test]
fn runner_file_read_range_reads_large_file_subset_under_max_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let mut content = String::new();
    for n in 1..=500 {
        content.push_str(&format!("line-{n:04}\n"));
    }
    let expected_sha256 = sha256_hex_bytes(content.as_bytes());
    std::fs::write(tmp.path().join("large.txt"), content).unwrap();

    let out = file_read_json(handle_file_request(
        &policy,
        &file_read_request(tmp.path(), "large.txt", Some(250), Some(252), Some(1024)),
    ));

    assert_eq!(out["format"], "webcodex.file_read_range.v1");
    assert_eq!(out["content"], "line-0250\nline-0251\nline-0252");
    assert_eq!(out["total_lines"], 500);
    assert_eq!(out["start_line"], 250);
    assert_eq!(out["limit"], 3);
    assert_eq!(out["sha256"], expected_sha256);
}

#[test]
fn runner_file_read_range_output_obeys_max_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("limited.txt"), "alpha\nbeta\n").unwrap();

    let out = handle_file_request(
        &policy,
        &file_read_request(tmp.path(), "limited.txt", Some(1), Some(1), Some(4)),
    );

    assert!(out.exit_code.is_none(), "unexpected success: {out:?}");
    assert_eq!(
        out.error.as_deref(),
        Some("read_file failed: range_too_large")
    );
    assert!(out.stdout.is_none());
}

#[test]
fn runner_file_read_range_rejects_serialized_envelope_expansion_before_stdout() {
    for (name, byte, len) in [
        ("nul.txt", 0x00, 48 * 1024),
        ("quote.txt", b'\"', 140 * 1024),
        ("backslash.txt", b'\\', 140 * 1024),
        ("control.txt", 0x01, 48 * 1024),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join(name), vec![byte; len]).unwrap();
        let out = handle_file_request(
            &policy,
            &file_read_request(tmp.path(), name, Some(1), Some(1), Some(512 * 1024)),
        );
        assert!(
            out.exit_code.is_none(),
            "unexpected success for {name}: {out:?}"
        );
        assert_eq!(out.error.as_deref(), Some("range output too large"));
        assert!(
            out.stdout.is_none(),
            "oversized envelope reached stdout for {name}"
        );
    }
}

#[test]
fn runner_file_read_precheck_distinguishes_missing_and_non_file() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::create_dir(tmp.path().join("directory")).unwrap();

    let missing = handle_file_request(
        &policy,
        &file_read_request(tmp.path(), "missing.txt", Some(1), Some(1), Some(1024)),
    );
    assert_eq!(
        missing.error.as_deref(),
        Some("read_file failed: not_found")
    );
    assert!(missing.stdout.is_none());

    let directory = handle_file_request(
        &policy,
        &file_read_request(tmp.path(), "directory", Some(1), Some(1), Some(1024)),
    );
    assert_eq!(
        directory.error.as_deref(),
        Some("read_file failed: not_file")
    );
    assert!(directory.stdout.is_none());
}

#[test]
fn runner_file_read_range_errors_never_include_absolute_path() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());

    // Missing target: the error must be a stable message, not the resolved
    // absolute path (`resolved.display()`).
    let missing = handle_file_request(
        &policy,
        &file_read_request(tmp.path(), "absent.txt", Some(1), Some(1), Some(128)),
    );
    assert!(missing.exit_code.is_none());
    assert_eq!(
        missing.error.as_deref(),
        Some("read_file failed: not_found")
    );
    let err = missing.error.expect("error");
    assert!(
        !err.contains(tmp.path().to_string_lossy().as_ref()),
        "error leaked absolute path: {err}"
    );
}

#[test]
fn runner_file_read_allows_exact_generated_files_but_protects_secrets_and_git() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    for (path, allowed) in [
        ("node_modules/foo/package.json", true),
        ("target/result.txt", true),
        (".env", false),
        (".git/config", false),
    ] {
        let target = tmp.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "fixture").unwrap();
        let out = handle_file_request(
            &policy,
            &file_read_request(tmp.path(), path, None, None, Some(1024)),
        );
        if allowed {
            assert_eq!(out.stdout.as_deref(), Some("fixture"));
        } else {
            assert_eq!(
                out.error.as_deref(),
                Some("read_file failed: sensitive_path")
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn runner_file_read_rejects_alias_to_secret_inside_project() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(".env"), "fixture").unwrap();
    std::os::unix::fs::symlink(".env", tmp.path().join("alias.txt")).unwrap();
    let out = handle_file_request(
        &project_policy(tmp.path()),
        &file_read_request(tmp.path(), "alias.txt", None, None, Some(1024)),
    );
    assert_eq!(
        out.error.as_deref(),
        Some("read_file failed: sensitive_path")
    );
}
