//! Trusted raw command execution: multi-line shell scripts with safety guardrails.
//!
//! This module provides:
//! - `validate_trusted_script`: validates multi-line script content
//! - `check_denylist`: checks for blocked dangerous commands
//! - `check_secret_read`: checks for attempts to read sensitive files
//! - `check_background_escape`: checks for nohup/&/disown
//! - `build_trusted_wrapper`: wraps user script with set -euo pipefail and timing
//! - `TrustedRawCommandResult`: structured result type

use super::shell::sanitize_tail;
use super::types::TrustedRawCommandResult;
use std::path::Path;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub(super) const TRUSTED_MAX_SCRIPT_LEN: usize = 32_000;
pub(super) const TRUSTED_DEFAULT_TIMEOUT_SECS: u64 = 120;
pub(super) const TRUSTED_MAX_TIMEOUT_SECS: u64 = 1800;
pub(super) const TRUSTED_STDOUT_MAX: usize = 40_000;
pub(super) const TRUSTED_STDERR_MAX: usize = 20_000;

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that a trusted script is non-empty, within length limits, and
/// doesn't contain NUL bytes.
pub fn validate_trusted_script(script: &str) -> Result<(), String> {
    let trimmed = script.trim();
    if trimmed.is_empty() {
        return Err("trusted script cannot be empty".to_string());
    }
    if script.contains('\0') {
        return Err("trusted script cannot contain NUL bytes".to_string());
    }
    if script.len() > TRUSTED_MAX_SCRIPT_LEN {
        return Err(format!(
            "trusted script is too long; maximum is {} bytes, got {}",
            TRUSTED_MAX_SCRIPT_LEN,
            script.len()
        ));
    }
    Ok(())
}

/// Validate timeout_secs for trusted raw commands.
pub fn validate_trusted_timeout(timeout_secs: Option<u64>) -> Result<u64, String> {
    let secs = timeout_secs.unwrap_or(TRUSTED_DEFAULT_TIMEOUT_SECS);
    if secs == 0 || secs > TRUSTED_MAX_TIMEOUT_SECS {
        return Err(format!(
            "timeout_secs must be between 1 and {}",
            TRUSTED_MAX_TIMEOUT_SECS
        ));
    }
    Ok(secs)
}

/// Validate response_mode for trusted raw commands.
pub fn validate_response_mode(mode: &Option<String>) -> Result<String, String> {
    match mode.as_deref() {
        None | Some("summary") => Ok("summary".to_string()),
        Some("full") => Ok("full".to_string()),
        Some("minimal") => Ok("minimal".to_string()),
        Some(other) => Err(format!(
            "response_mode must be 'summary', 'full', or 'minimal'; got '{}'",
            other
        )),
    }
}

/// Validate reason field (strongly recommended for trusted commands).
pub fn validate_trusted_reason(reason: &Option<String>) -> Result<(), String> {
    match reason {
        Some(r) if r.trim().is_empty() => {
            Err("reason is required for trusted raw commands".to_string())
        }
        None => Err("reason is required for trusted raw commands".to_string()),
        Some(r) if r.len() > 4000 => Err(format!(
            "reason is too long; maximum is 4000 characters, got {}",
            r.len()
        )),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Denylist check
// ---------------------------------------------------------------------------

/// Dangerous command patterns that are always blocked in trusted mode.
const DANGEROUS_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "rm -rf ~",
    "rm -rf ~/*",
    "mkfs",
    "dd if=",
    "dd of=/dev",
    ":(){ :|:& };:",
    "chmod -R 777 /",
    "chown -R ",
    "systemctl",
    "service nginx",
    "service apache",
    "docker system prune",
    "git push",
    "git fetch",
    // Prevent modifying system daemons
    "nginx -s",
    "docker rm",
    "docker rmi",
];

/// Check if script contains dangerous patterns. Returns Some(error_message) if blocked.
pub fn check_denylist(script: &str) -> Option<String> {
    let lower = script.to_ascii_lowercase();
    for pattern in DANGEROUS_PATTERNS {
        if lower.contains(pattern) {
            return Some(format!(
                "blocked by denylist: command contains dangerous pattern '{}'",
                pattern
            ));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Secret read check
// ---------------------------------------------------------------------------

/// Patterns that indicate reading sensitive file content.
const SECRET_READ_PATTERNS: &[&str] = &[
    ".env",
    ".pem",
    "id_rsa",
    "id_ed25519",
    ".key",
    "secrets.",
    "secrets/",
    "token",
];

/// File extensions that are always sensitive.
const SENSITIVE_EXTENSIONS: &[&str] = &[".pem", ".key"];

/// Check if a script attempts to read sensitive file content.
/// Returns Some(error_message) if blocked.
pub fn check_secret_read(script: &str) -> Option<String> {
    let lower = script.to_ascii_lowercase();
    // Look for cat/grep/head/tail/less/more followed by sensitive file references
    let read_commands = [
        "cat ",
        "grep ",
        "head ",
        "tail ",
        "less ",
        "more ",
        "jq ",
        "python -c",
        "python3 -c",
    ];
    for cmd in &read_commands {
        for line in lower.lines() {
            if line.contains(cmd) {
                // Check if the line references sensitive files
                for secret in SECRET_READ_PATTERNS {
                    if line.contains(secret) {
                        // Allow ls to see filenames but not content
                        if line.trim().starts_with("ls ") || line.trim().starts_with("find ") {
                            continue;
                        }
                        return Some(format!(
                            "blocked: appears to read sensitive file content (pattern '{}'). Use ls to see filenames but not content.",
                            secret
                        ));
                    }
                }
                // Check sensitive extensions
                for ext in SENSITIVE_EXTENSIONS {
                    if line.contains(ext) {
                        if line.trim().starts_with("ls ") || line.trim().starts_with("find ") {
                            continue;
                        }
                        return Some(format!(
                            "blocked: appears to read sensitive file content (extension '{}'). Use ls to see filenames but not content.",
                            ext
                        ));
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Background escape check
// ---------------------------------------------------------------------------

/// Check if a script tries to escape job management via nohup/disown/background &.
/// Returns Some(error_message) if blocked.
pub fn check_background_escape(script: &str) -> Option<String> {
    for line in script.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        // nohup anywhere in line
        if lower.contains("nohup") {
            return Some(
                "blocked: 'nohup' is not allowed; use runJobOp for long-running tasks".to_string(),
            );
        }
        // disown anywhere in line
        if lower.contains("disown") {
            return Some(
                "blocked: 'disown' is not allowed; use runJobOp for long-running tasks".to_string(),
            );
        }
        // trailing & that is not && or part of a valid construct
        // Simple heuristic: if line ends with & and not &&, and not inside a comment
        if !trimmed.starts_with('#') && trimmed.ends_with('&') && !trimmed.ends_with("&&") {
            return Some(
                "blocked: background '&' is not allowed; use runJobOp for async execution"
                    .to_string(),
            );
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Script wrapper
// ---------------------------------------------------------------------------

/// Wrap user script with shell best practices:
/// - `set -euo pipefail`
/// - Print working directory
/// - Print start/end timestamps
pub fn build_trusted_wrapper(script: &str) -> String {
    // Use bash explicitly since sh does not support pipefail.
    // Escape single quotes in the script for safe embedding.
    let escaped = script.trim().replace("'", "'\\''");
    format!(
        "bash -c 'set -euo pipefail; echo \"[trusted] cwd: $(pwd)\"; echo \"[trusted] start: $(date -Iseconds)\"; {}; echo \"[trusted] end: $(date -Iseconds)\"'",
        escaped
    )
}

// ---------------------------------------------------------------------------
// Audit logging
// ---------------------------------------------------------------------------

/// Write an audit record for a trusted raw command execution.
pub fn write_trusted_audit(
    audit_dir: &Path,
    project: &str,
    cwd: &str,
    reason: &str,
    script: &str,
    start_time: i64,
    end_time: i64,
    exit_code: i32,
    duration_ms: u64,
    stdout_truncated: bool,
    stderr_truncated: bool,
    blocked_by_denylist: bool,
) -> Result<(), String> {
    std::fs::create_dir_all(audit_dir).map_err(|e| format!("Failed to create audit dir: {}", e))?;
    let record = serde_json::json!({
        "type": "trusted_raw_command",
        "project": project,
        "cwd": cwd,
        "reason": reason,
        "script": script,
        "start_time": start_time,
        "end_time": end_time,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
        "blocked_by_denylist": blocked_by_denylist,
    });
    let filename = format!("trusted_{}.json", start_time);
    std::fs::write(
        audit_dir.join(&filename),
        serde_json::to_string_pretty(&record).unwrap(),
    )
    .map_err(|e| format!("Failed to write audit record: {}", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Execution and result building
// ---------------------------------------------------------------------------

/// Build a TrustedRawCommandResult from execution output.
pub fn build_trusted_result(
    exit_code: i32,
    duration_ms: u64,
    cwd: &str,
    stdout: &str,
    stderr: &str,
    response_mode: &str,
    audit_log_path: Option<String>,
    blocked_by_denylist: bool,
) -> TrustedRawCommandResult {
    let (stdout_tail, stdout_truncated) = if response_mode == "minimal" {
        (None, false)
    } else {
        let max = if response_mode == "full" {
            TRUSTED_STDOUT_MAX
        } else {
            // summary: smaller tail
            8_000
        };
        let (tail, trunc) = sanitize_tail(stdout, max);
        (Some(tail), trunc)
    };

    let (stderr_tail, stderr_truncated) = if response_mode == "minimal" {
        (None, false)
    } else {
        let max = if response_mode == "full" {
            TRUSTED_STDERR_MAX
        } else {
            // summary: smaller tail
            4_000
        };
        let (tail, trunc) = sanitize_tail(stderr, max);
        (Some(tail), trunc)
    };

    TrustedRawCommandResult {
        exit_code,
        duration_ms,
        cwd: cwd.to_string(),
        stdout_tail,
        stderr_tail,
        stdout_truncated,
        stderr_truncated,
        audit_log_path,
        blocked_by_denylist,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_trusted_script_accepts_normal() {
        assert!(validate_trusted_script("echo hello").is_ok());
        assert!(validate_trusted_script("line1\nline2\nline3").is_ok());
    }

    #[test]
    fn validate_trusted_script_rejects_empty() {
        assert!(validate_trusted_script("").is_err());
        assert!(validate_trusted_script("   ").is_err());
    }

    #[test]
    fn validate_trusted_script_rejects_nul() {
        assert!(validate_trusted_script("echo\0hello").is_err());
    }

    #[test]
    fn validate_trusted_script_rejects_too_long() {
        let long = "a".repeat(TRUSTED_MAX_SCRIPT_LEN + 1);
        assert!(validate_trusted_script(&long).is_err());
    }

    #[test]
    fn validate_trusted_timeout_defaults() {
        assert_eq!(
            validate_trusted_timeout(None).unwrap(),
            TRUSTED_DEFAULT_TIMEOUT_SECS
        );
    }

    #[test]
    fn validate_trusted_timeout_range() {
        assert!(validate_trusted_timeout(Some(0)).is_err());
        assert!(validate_trusted_timeout(Some(TRUSTED_MAX_TIMEOUT_SECS + 1)).is_err());
        assert_eq!(validate_trusted_timeout(Some(60)).unwrap(), 60);
        assert_eq!(
            validate_trusted_timeout(Some(TRUSTED_MAX_TIMEOUT_SECS)).unwrap(),
            TRUSTED_MAX_TIMEOUT_SECS
        );
    }

    #[test]
    fn validate_response_mode_accepts_valid() {
        assert_eq!(validate_response_mode(&None).unwrap(), "summary");
        assert_eq!(
            validate_response_mode(&Some("summary".into())).unwrap(),
            "summary"
        );
        assert_eq!(
            validate_response_mode(&Some("full".into())).unwrap(),
            "full"
        );
        assert_eq!(
            validate_response_mode(&Some("minimal".into())).unwrap(),
            "minimal"
        );
    }

    #[test]
    fn validate_response_mode_rejects_invalid() {
        assert!(validate_response_mode(&Some("verbose".into())).is_err());
    }

    #[test]
    fn validate_trusted_reason_requires_reason() {
        assert!(validate_trusted_reason(&None).is_err());
        assert!(validate_trusted_reason(&Some("".into())).is_err());
        assert!(validate_trusted_reason(&Some("   ".into())).is_err());
        assert!(validate_trusted_reason(&Some("check logs".into())).is_ok());
    }

    #[test]
    fn check_denylist_blocks_dangerous() {
        assert!(check_denylist("rm -rf /").is_some());
        assert!(check_denylist("rm -rf /*").is_some());
        assert!(check_denylist("mkfs.ext4 /dev/sda1").is_some());
        assert!(check_denylist("dd if=/dev/zero of=/dev/sda").is_some());
        assert!(check_denylist("systemctl restart nginx").is_some());
        assert!(check_denylist("git push origin main").is_some());
        assert!(check_denylist("docker system prune -a").is_some());
    }

    #[test]
    fn check_denylist_allows_safe() {
        assert!(check_denylist("echo hello").is_none());
        assert!(check_denylist("cargo test").is_none());
        assert!(check_denylist("rm -rf target/").is_none());
        assert!(check_denylist("git status").is_none());
        assert!(check_denylist("grep -RIn foo src/").is_none());
    }

    #[test]
    fn check_secret_read_blocks_env() {
        assert!(check_secret_read("cat .env").is_some());
        assert!(check_secret_read("cat config/.env.local").is_some());
        assert!(check_secret_read("grep secret .env").is_some());
        assert!(check_secret_read("cat id_rsa").is_some());
        assert!(check_secret_read("cat server.pem").is_some());
        assert!(check_secret_read("head -5 secrets.json").is_some());
    }

    #[test]
    fn check_secret_read_allows_ls() {
        assert!(check_secret_read("ls .env").is_none());
        assert!(check_secret_read("find . -name '*.pem'").is_none());
    }

    #[test]
    fn check_secret_read_allows_normal() {
        assert!(check_secret_read("cat src/main.rs").is_none());
        assert!(check_secret_read("grep foo README.md").is_none());
    }

    #[test]
    fn check_background_escape_blocks_nohup() {
        assert!(check_background_escape("nohup python train.py").is_some());
    }

    #[test]
    fn check_background_escape_blocks_disown() {
        assert!(check_background_escape("disown %1").is_some());
    }

    #[test]
    fn check_background_escape_blocks_bg_ampersand() {
        assert!(check_background_escape("sleep 100 &").is_some());
    }

    #[test]
    fn check_background_escape_allows_logical_and() {
        assert!(check_background_escape("cargo fmt && cargo test").is_none());
    }

    #[test]
    fn check_background_escape_allows_normal() {
        assert!(check_background_escape("echo hello").is_none());
        assert!(check_background_escape("grep -RIn foo src/").is_none());
    }

    #[test]
    fn build_trusted_wrapper_adds_safety() {
        let wrapped = build_trusted_wrapper("echo hello");
        assert!(wrapped.contains("set -euo pipefail"));
        assert!(wrapped.contains("[trusted] cwd:"));
        assert!(wrapped.contains("[trusted] start:"));
        assert!(wrapped.contains("[trusted] end:"));
        assert!(wrapped.contains("echo hello"));
    }

    #[test]
    fn build_trusted_result_summary_mode() {
        let result = build_trusted_result(
            0,
            100,
            "/tmp/project",
            "hello\n",
            "warning\n",
            "summary",
            None,
            false,
        );
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.duration_ms, 100);
        assert_eq!(result.cwd, "/tmp/project");
        assert!(result.stdout_tail.is_some());
        assert!(!result.stdout_truncated);
    }

    #[test]
    fn build_trusted_result_minimal_mode() {
        let result = build_trusted_result(
            0,
            100,
            "/tmp/project",
            "hello\n",
            "warning\n",
            "minimal",
            None,
            false,
        );
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout_tail.is_none());
        assert!(result.stderr_tail.is_none());
    }

    #[test]
    fn build_trusted_result_truncates_large_output() {
        let big_stdout = "a".repeat(100_000);
        let big_stderr = "b".repeat(50_000);
        let result = build_trusted_result(
            0,
            100,
            "/tmp/project",
            &big_stdout,
            &big_stderr,
            "full",
            None,
            false,
        );
        assert!(result.stdout_truncated);
        assert!(result.stderr_truncated);
        assert!(result.stdout_tail.is_some());
        assert!(result.stderr_tail.is_some());
    }

    #[test]
    fn write_trusted_audit_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().join("audit");
        write_trusted_audit(
            &audit_dir,
            "myproj",
            "/tmp/project",
            "check logs",
            "echo hello",
            1000,
            1100,
            0,
            100,
            false,
            false,
            false,
        )
        .unwrap();
        let entries: Vec<_> = std::fs::read_dir(&audit_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let entry = entries[0].as_ref().unwrap();
        let content = std::fs::read_to_string(entry.path()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["type"], "trusted_raw_command");
        assert_eq!(json["project"], "myproj");
        assert_eq!(json["exit_code"], 0);
    }
}
