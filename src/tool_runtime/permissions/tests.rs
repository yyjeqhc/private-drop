//! Authority unit tests: modes, evaluator, execution gate, hard-safety independence.

use super::policy::{self, EffectiveAuthorityConfig};
use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const WRITE_TOOL: &str = "write_project_file";
const READ_TOOL: &str = "read_files";

#[test]
fn trusted_agent_auto_authorizes_permission_bearing_tools() {
    let evaluator = PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent);
    let decision = evaluator
        .evaluate(WRITE_TOOL, Some("agent:oe:private-drop"))
        .expect("write tools require permission");
    assert!(decision.required);
    assert_eq!(decision.policy, "trusted_agent");
    assert_eq!(decision.status, "auto_approved");
    assert_eq!(decision.reason, TRUSTED_AGENT_AUTO_REASON);
    assert_eq!(decision.risk, "write");
    assert_eq!(decision.tool_name, WRITE_TOOL);
    assert_eq!(decision.project.as_deref(), Some("agent:oe:private-drop"));
    assert!(decision.request_id.starts_with("wc_perm_"));
    assert_eq!(decision.outcome(), Some(PermissionOutcome::AutoApproved));
}

#[test]
fn unset_authority_mode_matches_trusted_agent_behavior() {
    // resolve: unset / empty → default
    assert_eq!(
        resolve_authority_mode(None).unwrap(),
        AuthorityMode::TrustedAgent
    );
    assert_eq!(
        resolve_authority_mode(Some("")).unwrap(),
        AuthorityMode::TrustedAgent
    );

    let from_unset = PermissionEvaluator::with_config(EffectiveAuthorityConfig::from_raw(None));
    let from_default = PermissionEvaluator::with_mode(AuthorityMode::DEFAULT);
    let a = from_unset.evaluate(WRITE_TOOL, None).unwrap();
    let b = from_default.evaluate(WRITE_TOOL, None).unwrap();
    assert_eq!(a.policy, b.policy);
    assert_eq!(a.status, b.status);
    assert_eq!(a.reason, b.reason);
    assert_eq!(a.risk, b.risk);
    assert_eq!(a.required, b.required);
}

#[test]
fn read_only_tools_do_not_emit_permission_decision() {
    let evaluator = PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent);
    assert!(evaluator.evaluate(READ_TOOL, None).is_none());
    // Even under restricted, non-permission tools stay not_required.
    let strict = PermissionEvaluator::with_mode(AuthorityMode::Restricted);
    assert!(strict.evaluate(READ_TOOL, None).is_none());
}

#[test]
fn illegal_mode_has_explicit_handling_and_does_not_auto_authorize() {
    let err = resolve_authority_mode(Some("totally_bogus")).unwrap_err();
    assert_eq!(err.value, "totally_bogus");
    let msg = err.to_string();
    assert!(msg.contains(AUTHORITY_MODE_ENV), "{msg}");
    assert!(msg.contains("totally_bogus"), "{msg}");

    let config = EffectiveAuthorityConfig::from_raw(Some("totally_bogus"));
    assert!(matches!(
        config,
        EffectiveAuthorityConfig::InvalidMode { .. }
    ));
    assert!(!config.auto_authorize());
    assert!(config.human_approval_required());

    let decision = PermissionEvaluator::with_config(config)
        .evaluate(WRITE_TOOL, None)
        .expect("still emits a decision for permission-bearing tools");
    assert_ne!(decision.status, "auto_approved");
    assert_eq!(decision.outcome(), Some(PermissionOutcome::Denied));
    assert!(
        decision.reason.contains("invalid_authority_mode"),
        "{}",
        decision.reason
    );
}

#[test]
fn ambiguous_legacy_permission_mode_is_rejected() {
    // audit_only has no equivalent in the current authority model.
    for legacy in ["audit_only"] {
        let config = EffectiveAuthorityConfig::from_raw(Some(legacy));
        assert!(
            matches!(config, EffectiveAuthorityConfig::InvalidMode { .. }),
            "{legacy} must not parse as an authority mode"
        );
        let decision = PermissionEvaluator::with_config(config)
            .evaluate(WRITE_TOOL, None)
            .unwrap();
        assert!(!decision.allows_execution());
    }
}

#[test]
fn restricted_denies_consequential_tools_without_pretending_to_approve() {
    let evaluator = PermissionEvaluator::with_mode(AuthorityMode::Restricted);
    let decision = evaluator.evaluate(WRITE_TOOL, None).unwrap();
    assert_eq!(decision.policy, "restricted");
    assert_ne!(decision.status, "auto_approved");
    assert_ne!(decision.status, "approved");
    assert_eq!(decision.outcome(), Some(PermissionOutcome::Denied));
    assert_eq!(decision.reason, RESTRICTED_DENY_REASON);
    assert!(!evaluator.config().auto_authorize());
    assert!(evaluator.config().human_approval_required());
}

#[test]
fn hard_security_rules_are_not_bypassed_by_authority_mode() {
    // Hard-deny detection is independent of soft authority mode.
    let hard_kinds = [
        "policy_rejected",
        "session_guard_denied",
        "unknown_session_id",
        "session_project_mismatch",
    ];
    for kind in hard_kinds {
        let output = json!({ "error_kind": kind, "failure_kind": kind });
        assert!(
            is_hard_denied_output(&output, None),
            "expected hard deny for {kind}"
        );
    }
    assert!(is_hard_denied_output(
        &json!({}),
        Some("sensitive path blocked")
    ));
    assert!(is_hard_denied_output(
        &json!({}),
        Some("path cannot contain parent traversal")
    ));

    // Auto-authorized decision exists for the tool class, but hard-deny filter
    // still drops attachment — mode never overrides hard safety signals.
    let decision = PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent)
        .evaluate(WRITE_TOOL, None)
        .unwrap();
    assert_eq!(decision.status, "auto_approved");
    let hard = json!({
        "error_kind": "policy_rejected",
        "failure_kind": "policy_rejected",
    });
    assert!(is_hard_denied_output(&hard, None));
    let filtered = (!is_hard_denied_output(&hard, None)).then_some(decision);
    assert!(
        filtered.is_none(),
        "hard deny must suppress permission attach even under trusted_agent"
    );

    // edit_path helper still produces policy_rejected hard-deny shape.
    let rejected = edit_path_policy_rejected_result("secret.env", "sensitive path".into());
    assert!(!rejected.success);
    assert!(is_hard_denied_output(
        &rejected.output,
        rejected.error.as_deref()
    ));
    assert_eq!(rejected.output["state_changed"], false);
}

#[test]
fn authority_profile_payload_projects_canonical_fields() {
    let trusted = policy::authority_profile_payload_for(&EffectiveAuthorityConfig::with_mode(
        AuthorityMode::TrustedAgent,
    ));
    assert_eq!(trusted["mode"], "trusted_agent");
    assert_eq!(trusted["source"], "default");
    assert_eq!(trusted["project_write"], true);
    assert_eq!(trusted["shell"], true);
    assert_eq!(trusted["git"], true);
    assert_eq!(trusted["network"], true);
    assert_eq!(trusted["package_install"], true);
    assert_eq!(trusted["service_control"], true);
    assert_eq!(trusted["release"], "user_task_scoped");
    assert_eq!(trusted["human_approval_required"], false);

    let restricted = policy::authority_profile_payload_for(&EffectiveAuthorityConfig::from_raw(
        Some("restricted"),
    ));
    assert_eq!(restricted["mode"], "restricted");
    assert_eq!(restricted["source"], "env:WEBCODEX_AUTHORITY_MODE");
    assert_eq!(restricted["project_write"], false);
    assert_eq!(restricted["shell"], false);
    assert_eq!(restricted["release"], "human_approval");
    assert_eq!(restricted["human_approval_required"], true);

    // No token / secret / internal policy dump in the projection.
    for payload in [&trusted, &restricted] {
        let text = payload.to_string().to_lowercase();
        assert!(!text.contains("token"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("authorization"), "{text}");
    }
}

#[test]
fn permission_decision_for_tool_wrapper_uses_evaluator() {
    // When env is unset (typical test process), default equals explicit mode.
    let via_wrapper = permission_decision_for_tool(WRITE_TOOL, None);
    let via_evaluator =
        PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent).evaluate(WRITE_TOOL, None);
    let a = via_wrapper.expect("wrapper");
    let b = via_evaluator.expect("evaluator");
    assert_eq!(a.policy, b.policy);
    assert_eq!(a.status, b.status);
    assert_eq!(a.reason, b.reason);
}

#[test]
fn allows_execution_is_centralized_by_outcome() {
    assert!(PermissionOutcome::AutoApproved.allows_execution());
    assert!(PermissionOutcome::Approved.allows_execution());
    assert!(!PermissionOutcome::Denied.allows_execution());
    assert!(!PermissionOutcome::Pending.allows_execution());
    assert!(!PermissionOutcome::HardDenied.allows_execution());

    let auto = PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent)
        .evaluate(WRITE_TOOL, None)
        .unwrap();
    assert!(auto.allows_execution());

    let restricted = PermissionEvaluator::with_mode(AuthorityMode::Restricted)
        .evaluate(WRITE_TOOL, None)
        .unwrap();
    assert!(!restricted.allows_execution());

    let invalid = PermissionEvaluator::with_config(EffectiveAuthorityConfig::from_raw(Some(
        "not_a_real_mode",
    )))
    .evaluate(WRITE_TOOL, None)
    .unwrap();
    assert!(!invalid.allows_execution());

    // Unparsable status fails closed.
    let mut bogus = auto.clone();
    bogus.status = "totally_unknown_status".to_string();
    assert!(!bogus.allows_execution());
}

#[test]
fn permission_execution_denied_result_is_stable_and_non_approving() {
    let decision = PermissionEvaluator::with_mode(AuthorityMode::Restricted)
        .evaluate(WRITE_TOOL, None)
        .unwrap();
    let result = permission_execution_denied_result(&decision);
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "permission_denied");
    assert_eq!(result.output["failure_kind"], "permission_denied");
    assert_eq!(result.output["permission_reason"], RESTRICTED_DENY_REASON);
    let err = result.error.as_deref().unwrap();
    assert!(err.contains("restricted"), "{err}");
    assert!(err.contains("human authorization"), "{err}");
    // Must not look like hard-deny (permission attach must remain).
    assert!(!is_hard_denied_output(
        &result.output,
        result.error.as_deref()
    ));

    let invalid =
        PermissionEvaluator::with_config(EffectiveAuthorityConfig::from_raw(Some("weird_mode")))
            .evaluate(WRITE_TOOL, None)
            .unwrap();
    let invalid_result = permission_execution_denied_result(&invalid);
    assert!(!invalid_result.success);
    let msg = invalid_result.error.as_deref().unwrap();
    assert!(msg.contains(AUTHORITY_MODE_ENV), "{msg}");
    assert!(
        !msg.contains("auto_approved"),
        "must not pretend auto-authorization: {msg}"
    );
}

#[test]
fn evaluate_counter_increments_once_per_call() {
    let counter = Arc::new(AtomicUsize::new(0));
    let evaluator = PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent)
        .with_eval_counter(counter.clone());
    let _ = evaluator.evaluate(WRITE_TOOL, None);
    let _ = evaluator.evaluate(READ_TOOL, None);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn permission_decision_from_output_roundtrips() {
    let decision = PermissionEvaluator::with_mode(AuthorityMode::TrustedAgent)
        .evaluate(WRITE_TOOL, Some("agent:oe:private-drop"))
        .unwrap();
    let mut result = ToolResult::ok(json!({"ok": true}));
    add_permission_to_result(&mut result, &decision);
    let restored = permission_decision_from_output(&result.output).expect("permission present");
    assert_eq!(restored.request_id, decision.request_id);
    assert_eq!(restored.status, decision.status);
    assert_eq!(restored.policy, decision.policy);
}
