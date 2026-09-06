use super::*;

fn apply_patch_request(cwd: &Path, patch: &str, dry_run: bool) -> RunnerRequest {
    apply_patch_request_with_mode(cwd, patch, dry_run, "unique")
}

fn apply_patch_request_with_mode(
    cwd: &Path,
    patch: &str,
    dry_run: bool,
    matching_mode: &str,
) -> RunnerRequest {
    let payload = serde_json::json!({
        "patch": patch,
        "dry_run": dry_run,
        "matching_mode": matching_mode,
    });
    RunnerRequest {
        request_id: "req-apply-patch".to_string(),
        client_id: "agent-1".to_string(),
        kind: "file_apply_patch".to_string(),
        job_id: None,
        cwd: Some(cwd.to_string_lossy().to_string()),
        path: Some("routing-placeholder".to_string()),
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

fn apply_patch_request_legacy_strict(cwd: &Path, patch: &str, dry_run: bool) -> RunnerRequest {
    let payload = serde_json::json!({
        "patch": patch,
        "dry_run": dry_run,
        "strict_matching": true,
    });
    let mut request = apply_patch_request_with_mode(cwd, patch, dry_run, "unique");
    request.content = Some(payload.to_string());
    request
}

#[test]
fn file_apply_patch_exact_unique_accepts_exact_unique_and_append() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("exact.txt"), "old\n").unwrap();
    std::fs::write(tmp.path().join("append.txt"), "head\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: exact.txt\n-old\n+new\n*** Update File: append.txt\n+tail\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request_with_mode(tmp.path(), patch, false, "exact_unique"),
    ));

    assert_eq!(out["changed"], true);
    assert_eq!(out["execution_state"], "completed");
    assert_eq!(out["requested_matching_mode"], "exact_unique");
    assert_eq!(out["files"][0]["edits"][0]["unique_match"], true);
    assert_eq!(out["files"][0]["edits"][0]["strict_match"], true);
    assert_eq!(out["files"][1]["edits"][0]["strict_match"], true);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("exact.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("append.txt")).unwrap(),
        "head\ntail\n"
    );
}

#[test]
fn file_apply_patch_exact_unique_rejects_fuzzy_and_ambiguous_before_write() {
    for (original, patch, expected_mode, expected_candidates) in [
        (
            " old \n",
            "*** Begin Patch\n*** Update File: target.txt\n-old\n+new\n*** End Patch",
            "trim",
            1,
        ),
        (
            "dup\nmiddle\ndup\n",
            "*** Begin Patch\n*** Update File: target.txt\n-dup\n+new\n*** End Patch",
            "exact",
            2,
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("target.txt"), original).unwrap();
        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_patch_request_with_mode(tmp.path(), patch, false, "exact_unique"),
        ));
        assert_eq!(out["error_kind"], "matching_mode_rejected");
        assert_eq!(out["state_changed"], false);
        assert_eq!(out["execution_state"], "not_started");
        assert_eq!(out["requested_matching_mode"], "exact_unique");
        assert_eq!(out["match_mode"], expected_mode);
        assert_eq!(out["candidate_count"], expected_candidates);
        assert_eq!(out["search_start_line"], 1);
        assert!(out["source_line_count"].as_u64().unwrap() >= expected_candidates);
        assert_eq!(out["matching_mode_satisfied"], false);
        assert_eq!(out["recovery_action"], "refine_patch_context");
        assert!(out["retry_guidance"]
            .as_str()
            .unwrap()
            .contains("matching_mode=exact_unique"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
            original
        );
    }
}

#[test]
fn file_apply_patch_unique_accepts_repeated_context_with_one_mutation_target() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let original = "ctx\n foo \nctx\nother\n";
    std::fs::write(tmp.path().join("target.txt"), original).unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n@@ ctx\n-foo\n+new\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request_with_mode(tmp.path(), patch, false, "unique"),
    ));

    assert_eq!(out["changed"], true);
    assert_eq!(out["execution_state"], "completed");
    assert_eq!(out["requested_matching_mode"], "unique");
    let edit = &out["files"][0]["edits"][0];
    assert_eq!(edit["match_source"], "old_lines");
    assert_eq!(edit["match_mode"], "trim");
    assert_eq!(edit["matched_start_line"], 2);
    assert_eq!(edit["candidate_count"], 1);
    assert_eq!(edit["unique_match"], true);
    assert_eq!(edit["strict_match"], false);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        "ctx\nnew\nctx\nother\n"
    );
}

#[test]
fn file_apply_patch_unique_rejects_repeated_context_for_pure_addition() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let original = "ctx\nfirst\nctx\nsecond\n";
    std::fs::write(tmp.path().join("target.txt"), original).unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n@@ ctx\n+inserted\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request_with_mode(tmp.path(), patch, false, "unique"),
    ));

    assert_eq!(out["error_kind"], "matching_mode_rejected");
    assert_eq!(out["requested_matching_mode"], "unique");
    assert_eq!(out["match_source"], "change_context");
    assert_eq!(out["candidate_count"], 2);
    assert_eq!(out["candidate_start_lines"], serde_json::json!([1, 3]));
    assert!(out["matched_start_line"].is_null());
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        original
    );
}

#[test]
fn file_apply_patch_exact_unique_rejects_later_risk_before_any_batch_write() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("safe.txt"), "old\n").unwrap();
    std::fs::write(tmp.path().join("risky.txt"), " fuzzy \n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: safe.txt\n-old\n+new\n*** Update File: risky.txt\n-fuzzy\n+changed\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request_with_mode(tmp.path(), patch, false, "exact_unique"),
    ));

    assert_eq!(out["error_kind"], "matching_mode_rejected");
    assert_eq!(out["change_index"], 1);
    assert_eq!(out["path"], "risky.txt");
    assert_eq!(out["state_changed"], false);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("safe.txt")).unwrap(),
        "old\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("risky.txt")).unwrap(),
        " fuzzy \n"
    );
}

#[test]
fn file_apply_patch_applies_add_update_delete_and_update_move_transactionally() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("update.txt"), "header\nbefore\nafter\n").unwrap();
    std::fs::write(tmp.path().join("delete.txt"), "remove\n").unwrap();
    std::fs::write(tmp.path().join("move.txt"), "head\nold\ntail\n").unwrap();

    let patch = "*** Begin Patch\n*** Add File: added.txt\n+added\n*** Update File: update.txt\n@@ header\n-before\n+updated\n*** Delete File: delete.txt\n*** Update File: move.txt\n*** Move to: nested/moved.txt\n@@ head\n-old\n+new\n*** End Patch";
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request(tmp.path(), patch, false),
    ));

    assert_eq!(out["changed"], true);
    assert_eq!(out["state_changed"], true);
    assert_eq!(out["execution_state"], "completed");
    assert_eq!(out["applied_count"], 4);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("added.txt")).unwrap(),
        "added\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("update.txt")).unwrap(),
        "header\nupdated\nafter\n"
    );
    assert!(!tmp.path().join("delete.txt").exists());
    assert!(!tmp.path().join("move.txt").exists());
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("nested/moved.txt")).unwrap(),
        "head\nnew\ntail\n"
    );
    assert_eq!(out["changed_paths"].as_array().unwrap().len(), 5);
}

#[test]
fn file_apply_patch_dry_run_reports_plan_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("target.txt"), "old\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n-old\n+new\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request(tmp.path(), patch, true),
    ));

    assert_eq!(out["dry_run"], true);
    assert_eq!(out["requested_matching_mode"], "unique");
    assert_eq!(out["changed"], false);
    assert_eq!(out["state_changed"], false);
    assert_eq!(out["would_change"], true);
    let edit = &out["files"][0]["edits"][0];
    assert_eq!(edit["match_mode"], "exact");
    assert_eq!(edit["match_source"], "old_lines");
    assert_eq!(edit["matched_start_line"], 1);
    assert_eq!(edit["candidate_count"], 1);
    assert_eq!(edit["unique_match"], true);
    assert_eq!(edit["strict_match"], true);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn file_apply_patch_reports_fuzzy_and_append_match_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("trim-end.txt"), "alpha  \n").unwrap();
    std::fs::write(tmp.path().join("trim.txt"), "  beta  \n").unwrap();
    std::fs::write(tmp.path().join("append.txt"), "head\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: trim-end.txt\n-alpha\n+ALPHA\n*** Update File: trim.txt\n-beta\n+BETA\n*** Update File: append.txt\n+tail\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request(tmp.path(), patch, true),
    ));

    let trim_end = &out["files"][0]["edits"][0];
    assert_eq!(trim_end["match_mode"], "trim_end");
    assert_eq!(trim_end["match_source"], "old_lines");
    assert_eq!(trim_end["matched_start_line"], 1);
    assert_eq!(trim_end["candidate_count"], 1);
    assert_eq!(trim_end["unique_match"], true);
    assert_eq!(trim_end["strict_match"], false);

    let trim = &out["files"][1]["edits"][0];
    assert_eq!(trim["match_mode"], "trim");
    assert_eq!(trim["match_source"], "old_lines");
    assert_eq!(trim["matched_start_line"], 1);
    assert_eq!(trim["candidate_count"], 1);
    assert_eq!(trim["unique_match"], true);
    assert_eq!(trim["strict_match"], false);

    let append = &out["files"][2]["edits"][0];
    assert!(append["match_mode"].is_null());
    assert_eq!(append["match_source"], "append");
    assert_eq!(append["matched_start_line"], 2);
    assert!(append["candidate_count"].is_null());
    assert_eq!(append["unique_match"], true);
    assert_eq!(append["strict_match"], true);
}

#[test]
fn file_apply_patch_unique_rejects_ambiguous_candidate_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("target.txt"), "dup\nmiddle\ndup\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n-dup\n+changed\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request(tmp.path(), patch, true),
    ));

    assert_eq!(out["error_kind"], "matching_mode_rejected");
    assert_eq!(out["requested_matching_mode"], "unique");
    assert_eq!(out["match_mode"], "exact");
    assert_eq!(out["match_source"], "old_lines");
    assert!(out["matched_start_line"].is_null());
    assert_eq!(out["candidate_count"], 2);
    assert_eq!(out["candidate_start_lines"], serde_json::json!([1, 3]));
    assert_eq!(out["state_changed"], false);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        "dup\nmiddle\ndup\n"
    );
}

#[test]
fn file_apply_patch_context_conflict_keeps_whole_batch_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("first.txt"), "one\n").unwrap();
    std::fs::write(tmp.path().join("second.txt"), "actual\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: first.txt\n-one\n+ONE\n*** Update File: second.txt\n-missing\n+changed\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request(tmp.path(), patch, false),
    ));

    assert_eq!(out["error_kind"], "context_mismatch");
    assert_eq!(out["state_changed"], false);
    assert_eq!(out["execution_state"], "not_started");
    assert_eq!(out["change_index"], 1);
    assert_eq!(out["match_diagnostic"]["chunk_index"], 0);
    assert_eq!(out["match_diagnostic"]["match_source"], "old_lines");
    assert_eq!(out["match_diagnostic"]["search_start_line"], 1);
    assert_eq!(out["match_diagnostic"]["expected_line_count"], 1);
    assert_eq!(out["match_diagnostic"]["available_line_count"], 1);
    assert_eq!(out["match_diagnostic"]["closest_start_line"], 1);
    assert_eq!(out["match_diagnostic"]["closest_exact_line_matches"], 0);
    assert_eq!(out["match_diagnostic"]["first_exact_mismatch_offset"], 1);
    assert!(
        out.get("recovery").is_none(),
        "Runner must return canonical structural facts only; Server derives model-facing recovery"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("first.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("second.txt")).unwrap(),
        "actual\n"
    );
}

#[test]
fn file_apply_patch_context_conflict_does_not_echo_patch_or_source_body() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("target.txt"), "SOURCE_PRIVATE_TOKEN\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n@@ PATCH_PRIVATE_TOKEN\n-old\n+new\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request(tmp.path(), patch, false),
    ));
    let serialized = serde_json::to_string(&out).unwrap();

    assert_eq!(out["error_kind"], "context_mismatch");
    assert_eq!(out["match_diagnostic"]["match_source"], "change_context");
    assert!(!serialized.contains("PATCH_PRIVATE_TOKEN"));
    assert!(!serialized.contains("SOURCE_PRIVATE_TOKEN"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        "SOURCE_PRIVATE_TOKEN\n"
    );
}

#[test]
fn file_apply_patch_unique_accepts_normalized_unicode_and_reports_the_tier() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("target.txt"), "alpha—beta\n").unwrap();
    let patch =
        "*** Begin Patch\n*** Update File: target.txt\n-alpha-beta\n+changed\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request_with_mode(tmp.path(), patch, false, "unique"),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(out["requested_matching_mode"], "unique");
    assert_eq!(out["files"][0]["edits"][0]["match_mode"], "normalized");
    assert_eq!(out["files"][0]["edits"][0]["candidate_count"], 1);
    assert_eq!(out["files"][0]["edits"][0]["unique_match"], true);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        "changed\n"
    );
}

#[test]
fn file_apply_patch_unique_eof_constraint_ignores_earlier_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("target.txt"), "same\nmid\nsame\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n-same\n+last\n*** End of File\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request_with_mode(tmp.path(), patch, false, "unique"),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(out["files"][0]["edits"][0]["matched_start_line"], 3);
    assert_eq!(out["files"][0]["edits"][0]["candidate_count"], 1);
    assert_eq!(out["files"][0]["edits"][0]["unique_match"], true);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        "same\nmid\nlast\n"
    );
}

#[test]
fn file_apply_patch_first_match_is_deterministic_for_repeated_candidates() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("target.txt"), "dup\nmid\ndup\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n-dup\n+first\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request_with_mode(tmp.path(), patch, false, "first_match"),
    ));
    assert_eq!(out["changed"], true);
    assert_eq!(out["requested_matching_mode"], "first_match");
    assert_eq!(out["files"][0]["edits"][0]["candidate_count"], 2);
    assert_eq!(out["files"][0]["edits"][0]["unique_match"], false);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        "first\nmid\ndup\n"
    );
}

#[test]
fn file_apply_patch_rejects_contradictory_enum_and_legacy_bool() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("target.txt"), "old\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n-old\n+new\n*** End Patch";
    let mut request = apply_patch_request_with_mode(tmp.path(), patch, false, "unique");
    request.content = Some(
        serde_json::json!({
            "patch": patch,
            "dry_run": false,
            "matching_mode": "unique",
            "strict_matching": true,
        })
        .to_string(),
    );

    let out = line_edit_json(handle_file_request(&policy, &request));
    assert_eq!(out["error_kind"], "invalid_payload");
    assert_eq!(out["state_changed"], false);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("target.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn file_apply_patch_legacy_strict_true_maps_to_exact_unique() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    std::fs::write(tmp.path().join("target.txt"), " old \n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: target.txt\n-old\n+new\n*** End Patch";
    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request_legacy_strict(tmp.path(), patch, false),
    ));
    assert_eq!(out["error_kind"], "matching_mode_rejected");
    assert_eq!(out["requested_matching_mode"], "exact_unique");
    assert_eq!(out["match_mode"], "trim");
    assert_eq!(out["candidate_count"], 1);
    assert_eq!(out["state_changed"], false);
}

#[test]
fn file_apply_patch_rejects_sensitive_paths_before_write() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = project_policy(tmp.path());
    let patch = "*** Begin Patch\n*** Add File: .env\n+SECRET=value\n*** End Patch";

    let out = line_edit_json(handle_file_request(
        &policy,
        &apply_patch_request(tmp.path(), patch, false),
    ));

    assert_eq!(out["error_kind"], "invalid_path");
    assert_eq!(out["state_changed"], false);
    assert!(!tmp.path().join(".env").exists());
    assert!(!serde_json::to_string(&out)
        .unwrap()
        .contains("SECRET=value"));
}
