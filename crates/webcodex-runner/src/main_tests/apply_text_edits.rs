use super::*;

fn apply_text_edits_request(
    cwd: &Path,
    path: &str,
    mut payload: serde_json::Value,
) -> RunnerRequest {
    if payload.get("changes").is_none() {
        let expected_sha256 = payload
            .get("expected_file_sha256")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                sha256_hex_bytes(&std::fs::read(cwd.join(path)).unwrap_or_default())
            });
        payload = serde_json::json!({
            "dry_run": payload.get("dry_run").cloned().unwrap_or(serde_json::Value::Bool(false)),
            "changes": [{
                "kind": "edit",
                "path": path,
                "expected_sha256": expected_sha256,
                "edits": payload.get("edits").cloned().unwrap_or_else(|| serde_json::json!([]))
            }]
        });
    }
    RunnerRequest {
        request_id: "req-apply-text-edits".to_string(),
        client_id: "agent-1".to_string(),
        kind: "file_apply_text_edits".to_string(),
        job_id: None,
        cwd: Some(cwd.to_string_lossy().to_string()),
        path: Some(path.to_string()),
        content: Some(payload.to_string()),
        max_bytes: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: None,
        end_line: None,
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

#[test]
fn file_apply_text_edits_applies_multi_file_transaction() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "beta\n").unwrap();
    std::fs::write(tmp.path().join("c.txt"), "gamma\n").unwrap();
    let hash = |path: &str| sha256_hex_bytes(&std::fs::read(tmp.path().join(path)).unwrap());

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "a.txt",
            serde_json::json!({
                "changes": [
                    {
                        "kind": "edit",
                        "path": "a.txt",
                        "expected_sha256": hash("a.txt"),
                        "edits": [{"kind": "replace_exact", "old_text": "alpha", "new_text": "ALPHA"}]
                    },
                    {"kind": "create", "path": "nested/new.txt", "content": "new\n"},
                    {"kind": "delete", "path": "b.txt", "expected_sha256": hash("b.txt")},
                    {"kind": "rename", "path": "c.txt", "to_path": "moved/c.txt", "expected_sha256": hash("c.txt")}
                ]
            }),
        ),
    ));

    assert_eq!(out["changed"], true);
    assert_eq!(out["applied_count"], 4);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
        "ALPHA\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("nested/new.txt")).unwrap(),
        "new\n"
    );
    assert!(!tmp.path().join("b.txt").exists());
    assert!(!tmp.path().join("c.txt").exists());
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("moved/c.txt")).unwrap(),
        "gamma\n"
    );
    assert_eq!(out["files"].as_array().unwrap().len(), 4);
}

#[test]
fn file_apply_text_edits_hash_conflict_keeps_every_file_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "beta\n").unwrap();
    let a_hash = sha256_hex_bytes(&std::fs::read(tmp.path().join("a.txt")).unwrap());
    let b_hash = sha256_hex_bytes(&std::fs::read(tmp.path().join("b.txt")).unwrap());

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "a.txt",
            serde_json::json!({
                "recovery_metadata_version": 1,
                "changes": [
                    {
                        "kind": "edit",
                        "path": "a.txt",
                        "expected_sha256": a_hash,
                        "edits": [{"kind": "replace_exact", "old_text": "alpha", "new_text": "ALPHA"}]
                    },
                    {
                        "kind": "delete",
                        "path": "b.txt",
                        "expected_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    }
                ]
            }),
        ),
    ));

    assert_eq!(out["error_kind"], "sha256_conflict");
    assert_eq!(out["change_index"], 1);
    assert_eq!(out["state_changed"], false);
    assert_eq!(out["conflict_recovery"]["conflict_kind"], "sha256_mismatch");
    assert_eq!(out["conflict_recovery"]["direct_retry_safe"], false);
    assert_eq!(out["conflict_recovery"]["reread_required"], true);
    assert_eq!(
        out["conflict_recovery"]["occurrence_selector_supported"],
        false
    );
    assert_eq!(
        out["conflict_recovery"]["expected_sha256"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(out["conflict_recovery"]["current_sha256"], b_hash);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(),
        "beta\n"
    );
}

#[test]
fn file_apply_text_edits_rejects_resolved_path_aliases() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/a.txt"), "alpha\n").unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(tmp.path().join("src/a.txt")).unwrap());

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "src/a.txt",
            serde_json::json!({
                "changes": [
                    {
                        "kind": "edit",
                        "path": "src/a.txt",
                        "expected_sha256": hash,
                        "edits": [{"kind": "replace_exact", "old_text": "alpha", "new_text": "ALPHA"}]
                    },
                    {
                        "kind": "delete",
                        "path": "src//a.txt",
                        "expected_sha256": hash
                    }
                ]
            }),
        ),
    ));

    assert_eq!(out["error_kind"], "path_overlap");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src/a.txt")).unwrap(),
        "alpha\n"
    );
}

#[test]
fn file_apply_text_edits_replace_exact_writes_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "old\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "replace_exact", "old_text": "old", "new_text": "new"}
                ]
            }),
        ),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(out["would_change"], true);
    assert_eq!(out["changed_paths"][0], "target.txt");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
}

#[test]
fn file_apply_text_edits_ignores_empty_insert_noop_and_applies_remaining_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "old\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "insert_before", "anchor_text": "missing", "new_text": ""},
                    {"kind": "replace_exact", "old_text": "old", "new_text": "new"}
                ]
            }),
        ),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(out["ignored_noop_count"], 1);
    assert_eq!(out["files"][0]["edits"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
}

#[test]
fn file_apply_text_edits_dry_run_does_not_write() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "old\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "dry_run": true,
                "edits": [
                    {"kind": "replace_exact", "old_text": "old", "new_text": "new"}
                ]
            }),
        ),
    ));
    assert_eq!(out["dry_run"], true);
    assert_eq!(out["changed"], false);
    assert_eq!(out["would_change"], true);
    assert_eq!(out["changed_paths"][0], "target.txt");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
}

#[test]
fn file_apply_text_edits_rejects_missing_match_without_write() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "alpha\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "replace_exact", "old_text": "missing", "new_text": "x"}
                ]
            }),
        ),
    ));
    let msg = out["error"].as_str().unwrap();
    assert!(msg.contains("match text was not found"));
    assert!(msg.contains("No files were modified"));
    assert_eq!(out["changed"], false);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
}

#[test]
fn file_apply_text_edits_rejects_ambiguous_match_without_write() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "dup-dup\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "replace_exact", "old_text": "dup", "new_text": "x"}
                ]
            }),
        ),
    ));
    let msg = out["error"].as_str().unwrap();
    assert!(msg.contains("matched 2 times"));
    assert!(msg.contains("No files were modified"));
    assert_eq!(out["changed"], false);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "dup-dup\n");
}

#[test]
fn file_apply_text_edits_expected_file_sha256_mismatch_without_write() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "alpha\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "expected_file_sha256": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdead",
                "edits": [
                    {"kind": "replace_exact", "old_text": "alpha", "new_text": "beta"}
                ]
            }),
        ),
    ));
    let err = out["error"].as_str().unwrap();
    assert!(err.contains("expected_sha256 does not match"));
    assert!(err.contains("No files were modified"));
    assert_eq!(out["changed"], false);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
}

#[test]
fn file_apply_text_edits_insert_before_after_and_delete_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "insert_after", "anchor_text": "alpha\n", "new_text": "ALPHA-AFTER\n"},
                    {"kind": "delete_exact", "old_text": "beta\n"},
                    {"kind": "insert_before", "anchor_text": "gamma\n", "new_text": "GAMMA-BEFORE\n"}
                ]
            }),
        ),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(out["applied_count"], 1);
    assert_eq!(out["files"][0]["edits"].as_array().unwrap().len(), 3);
    assert_eq!(out["changed_paths"][0], "target.txt");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "alpha\nALPHA-AFTER\nGAMMA-BEFORE\ngamma\n"
    );
}

#[test]
fn file_apply_text_edits_crlf_accepts_lf_edits_and_preserves_crlf() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "replace_exact", "old_text": "one\n", "new_text": "ONE\n"},
                    {"kind": "insert_after", "anchor_text": "two\n", "new_text": "AFTER-TWO\n"},
                    {"kind": "delete_exact", "old_text": "three\n"},
                    {"kind": "insert_before", "anchor_text": "four\n", "new_text": "BEFORE-FOUR\n"}
                ]
            }),
        ),
    ));

    assert_eq!(out["changed"], true);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"ONE\r\ntwo\r\nAFTER-TWO\r\nBEFORE-FOUR\r\nfour\r\nfive\r\n"
    );
}

#[test]
fn file_apply_text_edits_lf_accepts_crlf_edits_and_preserves_lf() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, b"one\ntwo\nthree\nfour\nfive\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "replace_exact", "old_text": "one\r\n", "new_text": "ONE\r\n"},
                    {"kind": "insert_after", "anchor_text": "two\r\n", "new_text": "AFTER-TWO\r\n"},
                    {"kind": "delete_exact", "old_text": "three\r\n"},
                    {"kind": "insert_before", "anchor_text": "four\r\n", "new_text": "BEFORE-FOUR\r\n"}
                ]
            }),
        ),
    ));

    assert_eq!(out["changed"], true);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"ONE\ntwo\nAFTER-TWO\nBEFORE-FOUR\nfour\nfive\n"
    );
}

#[test]
fn file_apply_text_edits_mixed_line_endings_abort_entire_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let first = tmp.path().join("first.txt");
    let mixed = tmp.path().join("mixed.txt");
    std::fs::write(&first, b"alpha\r\n").unwrap();
    std::fs::write(&mixed, b"beta\r\ngamma\n").unwrap();
    let hash = |path: &Path| sha256_hex_bytes(&std::fs::read(path).unwrap());

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "first.txt",
            serde_json::json!({
                "changes": [
                    {
                        "kind": "edit",
                        "path": "first.txt",
                        "expected_sha256": hash(&first),
                        "edits": [{"kind": "replace_exact", "old_text": "alpha\n", "new_text": "ALPHA\n"}]
                    },
                    {
                        "kind": "edit",
                        "path": "mixed.txt",
                        "expected_sha256": hash(&mixed),
                        "edits": [{"kind": "replace_exact", "old_text": "beta\n", "new_text": "BETA\n"}]
                    }
                ]
            }),
        ),
    ));

    assert_eq!(out["error_kind"], "edit_conflict");
    assert_eq!(out["change_index"], 1);
    assert!(out["error"].as_str().unwrap().contains("mixed LF and CRLF"));
    assert_eq!(std::fs::read(&first).unwrap(), b"alpha\r\n");
    assert_eq!(std::fs::read(&mixed).unwrap(), b"beta\r\ngamma\n");
}

#[test]
fn file_apply_text_edits_line_ending_normalization_is_not_fuzzy_matching() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, b"alpha beta\r\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "replace_exact", "old_text": "alpha  beta\n", "new_text": "x\n"}
                ]
            }),
        ),
    ));

    assert_eq!(out["error_kind"], "edit_conflict");
    assert!(out["error"]
        .as_str()
        .unwrap()
        .contains("match text was not found"));
    assert_eq!(std::fs::read(&file).unwrap(), b"alpha beta\r\n");
}

#[test]
fn file_apply_text_edits_rejects_bare_cr_replacement_without_file_line_endings() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, b"one").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "replace_exact", "old_text": "one", "new_text": "ONE\r"}
                ]
            }),
        ),
    ));

    assert_eq!(out["error_kind"], "edit_conflict");
    assert!(out["error"].as_str().unwrap().contains("bare CR"));
    assert_eq!(std::fs::read(&file).unwrap(), b"one");
}

#[test]
fn file_apply_text_edits_structured_multiple_match_recovery_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(
        &file,
        (1..=10)
            .map(|n| format!("dup-{n}\ndup\n"))
            .collect::<String>(),
    )
    .unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version": 1,
                "changes": [{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"dup\n","new_text":"x\n"}]}]
            }),
        ),
    ));
    assert_eq!(out["error_kind"], "edit_conflict");
    let recovery = &out["conflict_recovery"];
    assert_eq!(recovery["schema_version"], 1);
    assert_eq!(recovery["conflict_kind"], "multiple_matches");
    assert_eq!(recovery["match_count"], 10);
    assert_eq!(recovery["occurrence_selector_supported"], true);
    assert_eq!(recovery["direct_retry_safe"], true);
    assert_eq!(recovery["reread_required"], false);
    assert_eq!(recovery["candidate_ranges"].as_array().unwrap().len(), 8);
    assert_eq!(recovery["candidate_ranges"][0]["occurrence"], 1);
    assert_eq!(recovery["candidate_ranges"][0]["start_line"], 2);
    assert_eq!(recovery["candidate_ranges"][7]["occurrence"], 8);
    assert_eq!(recovery["candidates_truncated"], true);
    let error = out["error"].as_str().unwrap();
    assert!(error.contains("choose an advertised occurrence"));
    assert!(error.contains("reuse the same expected_sha256"));
    assert!(!error.contains("read this file again"));
    let serialized = serde_json::to_string(&out).unwrap();
    assert!(!serialized.contains("x\\n"));
    assert_eq!(
        std::fs::read_to_string(&file)
            .unwrap()
            .matches("dup\n")
            .count(),
        10
    );
}

#[test]
fn file_apply_text_edits_structured_not_found_disables_selector() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"missing","new_text":"SECRET_NEW"}]}]
            }),
        ),
    ));
    let recovery = &out["conflict_recovery"];
    assert_eq!(recovery["conflict_kind"], "match_not_found");
    assert_eq!(recovery["match_count"], 0);
    assert_eq!(recovery["occurrence_selector_supported"], false);
    assert_eq!(recovery["direct_retry_safe"], false);
    assert_eq!(recovery["reread_required"], true);
    assert_eq!(recovery["recovery_action"], "reread_or_refine_match");
    assert!(out["retry_guidance"]
        .as_str()
        .unwrap()
        .contains("prefer apply_patch"));
    assert_eq!(recovery["candidate_ranges"].as_array().unwrap().len(), 0);
    assert!(!serde_json::to_string(recovery)
        .unwrap()
        .contains("SECRET_NEW"));
}

#[test]
fn file_apply_text_edits_structured_overlap_is_atomic_and_body_free() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "abcdef abcdef\n").unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[
                        {"kind":"replace_exact","old_text":"abc","new_text":"SECRET_A","occurrence":2},
                        {"kind":"replace_exact","old_text":"cde","new_text":"SECRET_B","occurrence":2}
                    ]}]
            }),
        ),
    ));
    let recovery = &out["conflict_recovery"];
    assert_eq!(recovery["conflict_kind"], "overlapping_edits");
    assert_eq!(
        recovery["conflicting_edit_indices"],
        serde_json::json!([0, 1])
    );
    assert_eq!(recovery["recovery_action"], "refine_edit_batch");
    assert_eq!(recovery["direct_retry_safe"], true);
    assert_eq!(recovery["reread_required"], false);
    let serialized = serde_json::to_string(recovery).unwrap();
    assert!(!serialized.contains("SECRET_A"));
    assert!(!serialized.contains("SECRET_B"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "abcdef abcdef\n");
}

#[test]
fn file_apply_text_edits_occurrence_selects_second_exact_match() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "dup\nkeep\ndup\n").unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version": 1,
                "changes": [{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"dup","new_text":"SECOND","occurrence":2}]}]
            }),
        ),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "dup\nkeep\nSECOND\n"
    );
}

#[test]
fn file_apply_text_edits_occurrence_out_of_range_is_actionable_and_atomic() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "dup\ndup\n").unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version": 1,
                "changes": [{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"delete_exact","old_text":"dup","occurrence":3}]}]
            }),
        ),
    ));
    assert_eq!(out["error_kind"], "edit_conflict");
    assert_eq!(
        out["conflict_recovery"]["conflict_kind"],
        "occurrence_out_of_range"
    );
    assert_eq!(out["conflict_recovery"]["match_count"], 2);
    assert_eq!(out["conflict_recovery"]["requested_occurrence"], 3);
    assert_eq!(out["conflict_recovery"]["direct_retry_safe"], true);
    assert_eq!(out["conflict_recovery"]["reread_required"], false);
    assert!(out["error"]
        .as_str()
        .unwrap()
        .contains("choose a valid advertised occurrence"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "dup\ndup\n");
}

#[test]
fn file_apply_text_edits_sha_conflict_precedes_occurrence_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "dup\ndup\n").unwrap();
    let current_sha256 = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version": 1,
                "changes": [{"kind":"edit","path":"target.txt","expected_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "edits":[{"kind":"replace_exact","old_text":"dup","new_text":"SECOND","occurrence":2,"line_scope":{"start_line":2,"end_line":2}}]}]
            }),
        ),
    ));
    assert_eq!(out["error_kind"], "sha256_conflict");
    assert_eq!(out["conflict_recovery"]["conflict_kind"], "sha256_mismatch");
    assert_eq!(out["conflict_recovery"]["recovery_action"], "reread_file");
    assert_eq!(out["conflict_recovery"]["direct_retry_safe"], false);
    assert_eq!(out["conflict_recovery"]["reread_required"], true);
    assert_eq!(
        out["conflict_recovery"]["occurrence_selector_supported"],
        false
    );
    assert_eq!(
        out["conflict_recovery"]["expected_sha256"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(out["conflict_recovery"]["current_sha256"], current_sha256);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "dup\ndup\n");
}

#[test]
fn file_apply_text_edits_multiline_and_crlf_candidates_use_source_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, b"head\r\na\r\nb\r\nmid\r\na\r\nb\r\n").unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let conflict = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"a\nb\n","new_text":"X\n"}]}]
            }),
        ),
    ));
    assert_eq!(
        conflict["conflict_recovery"]["candidate_ranges"][0]["start_line"],
        2
    );
    assert_eq!(
        conflict["conflict_recovery"]["candidate_ranges"][0]["end_line"],
        3
    );
    assert_eq!(
        conflict["conflict_recovery"]["candidate_ranges"][1]["start_line"],
        5
    );
    let success = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"a\nb\n","new_text":"X\n","occurrence":2}]}]
            }),
        ),
    ));
    assert_eq!(success["changed"], true);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"head\r\na\r\nb\r\nmid\r\nX\r\n"
    );
}

#[test]
fn file_apply_text_edits_old_server_payload_keeps_legacy_conflict_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "dup\ndup\n").unwrap();
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits":[{"kind":"replace_exact","old_text":"dup","new_text":"x"}]
            }),
        ),
    ));
    assert_eq!(out["error_kind"], "edit_conflict");
    assert!(out.get("conflict_recovery").is_none());
}

#[test]
fn file_apply_text_edits_new_payload_is_legacy_deserializable_and_fail_closed() {
    #[derive(serde::Deserialize)]
    struct LegacyEdit {
        kind: crate::apply_edits_shared::ApplyTextEditKind,
        #[serde(default)]
        old_text: Option<String>,
        #[serde(default)]
        new_text: Option<String>,
        #[serde(default)]
        anchor_text: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct LegacyChange {
        kind: crate::apply_edits_shared::ApplyFileChangeKind,
        path: String,
        #[serde(default)]
        to_path: Option<String>,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        edits: Vec<LegacyEdit>,
        #[serde(default)]
        expected_sha256: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct LegacyPayload {
        changes: Vec<LegacyChange>,
        #[serde(default)]
        dry_run: Option<bool>,
    }
    let value = serde_json::json!({
        "recovery_metadata_version":1,
        "changes":[{"kind":"edit","path":"target.txt","expected_sha256":"a".repeat(64),
            "edits":[{"kind":"replace_exact","old_text":"dup","new_text":"x","occurrence":2,"line_scope":{"start_line":2,"end_line":2}}]}],
        "dry_run":false
    });
    let legacy: LegacyPayload = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.changes.len(), 1);
    let change = &legacy.changes[0];
    assert_eq!(
        change.kind,
        crate::apply_edits_shared::ApplyFileChangeKind::Edit
    );
    assert_eq!(change.path, "target.txt");
    assert_eq!(
        change.expected_sha256.as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert!(change.to_path.is_none() && change.content.is_none());
    assert_eq!(legacy.dry_run, Some(false));
    let edit = &change.edits[0];
    assert_eq!(
        edit.kind,
        crate::apply_edits_shared::ApplyTextEditKind::ReplaceExact
    );
    assert_eq!(edit.new_text.as_deref(), Some("x"));
    assert!(edit.anchor_text.is_none());
    let needle = edit.old_text.as_deref().unwrap();
    assert_eq!(
        "dup\ndup\n".matches(needle).count(),
        2,
        "pre-feature Runner sees no occurrence and its unique-only semantics still fail closed"
    );
}

#[test]
fn file_apply_text_edits_rejects_overlapping_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "abcdef\n").unwrap();

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "edits": [
                    {"kind": "replace_exact", "old_text": "abc", "new_text": "ABC"},
                    {"kind": "replace_exact", "old_text": "cde", "new_text": "CDE"}
                ]
            }),
        ),
    ));
    let err = out["error"].as_str().unwrap();
    assert!(err.contains("edits overlap"));
    assert!(err.contains("No files were modified"));
    assert_eq!(out["changed"], false);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "abcdef\n");
}

#[test]
fn file_apply_text_edits_line_scope_filters_candidates_and_occurrence_stays_global() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    let original = "head\ndup\nmid\ndup\ntail\n";
    std::fs::write(&file, original).unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());

    let ambiguity = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"dup","new_text":"x","line_scope":{"start_line":1,"end_line":5}}]}]
            }),
        ),
    ));
    let recovery = &ambiguity["conflict_recovery"];
    assert_eq!(recovery["conflict_kind"], "multiple_matches");
    assert_eq!(recovery["match_count"], 2);
    assert_eq!(recovery["line_scope_match_count"], 2);
    assert_eq!(recovery["candidate_ranges"][0]["occurrence"], 1);
    assert_eq!(recovery["candidate_ranges"][1]["occurrence"], 2);
    assert_eq!(
        recovery["recovery_action"],
        "narrow_line_scope_or_select_occurrence"
    );
    assert_eq!(recovery["direct_retry_safe"], true);
    assert_eq!(recovery["reread_required"], false);

    let no_match = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"dup","new_text":"x","line_scope":{"start_line":5,"end_line":5}}]}]
            }),
        ),
    ));
    let recovery = &no_match["conflict_recovery"];
    assert_eq!(recovery["conflict_kind"], "match_not_found");
    assert_eq!(recovery["match_count"], 2);
    assert_eq!(recovery["line_scope_match_count"], 0);
    assert_eq!(recovery["candidate_ranges"], serde_json::json!([]));
    assert_eq!(recovery["direct_retry_safe"], true);
    assert_eq!(recovery["reread_required"], false);

    let mismatch = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"dup","new_text":"x","occurrence":1,"line_scope":{"start_line":4,"end_line":4}}]}]
            }),
        ),
    ));
    let recovery = &mismatch["conflict_recovery"];
    assert_eq!(recovery["conflict_kind"], "occurrence_outside_line_scope");
    assert_eq!(recovery["requested_occurrence"], 1);
    assert_eq!(
        recovery["line_scope"],
        serde_json::json!({"start_line":4,"end_line":4})
    );
    assert_eq!(recovery["candidate_ranges"][0]["occurrence"], 2);
    assert_eq!(
        recovery["recovery_action"],
        "align_occurrence_with_line_scope"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);

    let success = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"dup","new_text":"SECOND","line_scope":{"start_line":4,"end_line":4}}]}]
            }),
        ),
    ));
    assert_eq!(success["changed"], true);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "head\ndup\nmid\nSECOND\ntail\n"
    );
}

#[test]
fn file_apply_text_edits_multiline_crlf_scope_requires_full_containment() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    let original = b"head\r\na\r\nb\r\nmid\r\na\r\nb\r\ntail\r\n";
    std::fs::write(&file, original).unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());

    for line_scope in [
        serde_json::json!({"start_line":5,"end_line":5}),
        serde_json::json!({"start_line":4,"end_line":5}),
    ] {
        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "target.txt",
                serde_json::json!({
                    "recovery_metadata_version":1,
                    "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                        "edits":[{"kind":"replace_exact","old_text":"a\nb\n","new_text":"X\n","line_scope":line_scope}]}]
                }),
            ),
        ));
        assert_eq!(out["conflict_recovery"]["conflict_kind"], "match_not_found");
        assert_eq!(out["conflict_recovery"]["line_scope_match_count"], 0);
        assert_eq!(std::fs::read(&file).unwrap(), original);
    }

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,
                    "edits":[{"kind":"replace_exact","old_text":"a\nb\n","new_text":"X\n","line_scope":{"start_line":5,"end_line":6}}]}]
            }),
        ),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"head\r\na\r\nb\r\nmid\r\nX\r\ntail\r\n"
    );
}

#[test]
fn file_apply_text_edits_line_scope_supports_all_exact_edit_kinds() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    std::fs::write(&file, "replace\ndelete\nbefore\nafter\n").unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,"edits":[
                    {"kind":"replace_exact","old_text":"replace\n","new_text":"REPLACE\n","line_scope":{"start_line":1,"end_line":1}},
                    {"kind":"delete_exact","old_text":"delete\n","line_scope":{"start_line":2,"end_line":2}},
                    {"kind":"insert_before","anchor_text":"before\n","new_text":"BEFORE+\n","line_scope":{"start_line":3,"end_line":3}},
                    {"kind":"insert_after","anchor_text":"after\n","new_text":"AFTER+\n","line_scope":{"start_line":4,"end_line":4}}
                ]}]
            }),
        ),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "REPLACE\nBEFORE+\nbefore\nafter\nAFTER+\n"
    );
}

#[test]
fn file_apply_text_edits_scoped_failures_and_overlap_remain_transactional() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    let original = "abcdef\nsecond\n";
    std::fs::write(&file, original).unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());

    let failed = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,"edits":[
                    {"kind":"replace_exact","old_text":"second","new_text":"SECOND","line_scope":{"start_line":2,"end_line":2}},
                    {"kind":"replace_exact","old_text":"missing","new_text":"x","line_scope":{"start_line":2,"end_line":2}}
                ]}]
            }),
        ),
    ));
    assert_eq!(failed["error_kind"], "edit_conflict");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);

    let overlap = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "recovery_metadata_version":1,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,"edits":[
                    {"kind":"replace_exact","old_text":"abc","new_text":"ABC","line_scope":{"start_line":1,"end_line":1}},
                    {"kind":"replace_exact","old_text":"cde","new_text":"CDE","line_scope":{"start_line":1,"end_line":1}}
                ]}]
            }),
        ),
    ));
    assert_eq!(
        overlap["conflict_recovery"]["conflict_kind"],
        "overlapping_edits"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);

    let reversed = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,"edits":[
                    {"kind":"replace_exact","old_text":"second","new_text":"SECOND","line_scope":{"start_line":2,"end_line":1}}
                ]}]
            }),
        ),
    ));
    assert_eq!(reversed["error_kind"], "edit_conflict");
    assert!(reversed["error"].as_str().unwrap().contains("end_line"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
}

#[test]
fn file_apply_text_edits_scoped_dry_run_uses_same_resolution_without_write() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let file = tmp.path().join("target.txt");
    let original = "dup\ndup\n";
    std::fs::write(&file, original).unwrap();
    let hash = sha256_hex_bytes(&std::fs::read(&file).unwrap());
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_text_edits_request(
            tmp.path(),
            "target.txt",
            serde_json::json!({
                "dry_run":true,
                "changes":[{"kind":"edit","path":"target.txt","expected_sha256":hash,"edits":[
                    {"kind":"replace_exact","old_text":"dup","new_text":"SECOND","line_scope":{"start_line":2,"end_line":2}}
                ]}]
            }),
        ),
    ));
    assert_eq!(out["dry_run"], true);
    assert_eq!(out["changed"], false);
    assert_eq!(out["would_change"], true);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
}
