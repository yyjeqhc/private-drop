use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::path::Path;
use std::time::Duration;
use webcodex_workspace::file_read_normalize::MODEL_RESULT_ENVELOPE_RESERVE_BYTES;
use webcodex_workspace::file_read_range::MAX_SERIALIZED_OUTPUT_BYTES;

use super::git_committed::{
    committed_git_discovery_prefix, committed_git_isolated_view_setup, normalize_exact_commit_id,
    CommittedGitScope,
};
use super::helpers::{
    decode_git_quoted_path, shell_escape_simple, validate_limited_cleanup_paths,
    validate_project_relative_path,
};
use super::shell::{dispatch_uncertainty_lifecycle, runner_command_lifecycle};
use super::tool_result::{RecoveryKind, ToolResult};
use super::ToolRuntime;
use crate::runner_protocol::{ShellCommandExecutionState, ShellRunRequest};
use crate::tool_runtime::sessions::{SessionEvent, SessionSummary};

/// Sentinel separating `git status --porcelain` from `git diff --stat` in the
/// combined `git_diff_summary` command output. Chosen to be extremely unlikely
/// to appear in real git output.
pub(crate) const DIFF_SUMMARY_SENTINEL: &str = "@@WEBCODEX_DIFF_SUMMARY_SEP@@";
#[cfg(test)]
pub(crate) const SHOW_CHANGES_SENTINEL: &str = "@@WEBCODEX_SHOW_CHANGES_SEP@@";
const SHOW_CHANGES_BLOCK_TRAILER_BYTES: usize = 30;
const SHOW_CHANGES_BLOCK_MAGIC: &[u8; 6] = b"WCSF1:";
const DEFAULT_MAX_HUNKS: usize = 30;
const MAX_MAX_HUNKS: usize = 100;
const DEFAULT_MAX_HUNK_LINES: usize = 160;
const MAX_MAX_HUNK_LINES: usize = 400;
pub(crate) use webcodex_core::runtime_contract::GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES;
const GIT_DIFF_HUNKS_CONTINUATION_PREFIX: &str = "wcdh1.";
const GIT_DIFF_HUNKS_CONTINUATION_VERSION: u8 = 1;
const GIT_DIFF_HUNKS_COMMITTED_CONTINUATION_VERSION: u8 = 2;
pub(crate) const GIT_DIFF_HUNKS_PAGE_BYTES: usize = 32 * 1024;
const GIT_DIFF_HUNKS_STDERR_BYTES: usize = 8 * 1024;
const GIT_DIFF_HUNKS_BLOCK_TRAILER_BYTES: usize = 30;
const GIT_DIFF_HUNKS_BLOCK_MAGIC: &[u8; 6] = b"WCDH1:";
const SHOW_CHANGES_DEFAULT_MAX_HUNKS: usize = 20;
const SHOW_CHANGES_MAX_HUNKS: usize = 100;
const GIT_COMMIT_PATHS_MAX_PATHS: usize = 32;
const GIT_COMMIT_PATH_MAX_CHARS: usize = 512;
const GIT_COMMIT_MESSAGE_MAX_CHARS: usize = 1000;
pub(crate) const GIT_COMMIT_RESULT_PREFIX: &str = "@@WEBCODEX_GIT_COMMIT@@";
const SHOW_CHANGES_DEFAULT_MAX_HUNK_LINES: usize = 80;
const SHOW_CHANGES_MAX_HUNK_LINES: usize = 240;
const SHOW_CHANGES_DEFAULT_SESSION_EVENT_LIMIT: usize = 30;
const SHOW_CHANGES_MAX_SESSION_EVENT_LIMIT: usize = 200;
/// Maximum number of changed-file records `show_changes` emits on the
/// production side. The total count stays exact (all entries are counted); only
/// the returned records are bounded so a multi-thousand-file status never
/// overflows the transport tail-retention window.
pub(crate) const SHOW_CHANGES_MAX_STATUS_FILES: usize = 200;
/// Production-side stdout budget for the whole `show_changes` command. The
/// command is constructed so its worst-case raw stdout stays under this value,
/// which is itself well under the Runner/Shell transport default of 256 KiB
/// with room for protocol framing and error text. Bounding happens in the
/// command itself, never by relying on the transport tail.
pub(crate) const SHOW_CHANGES_OUTPUT_BUDGET_BYTES: usize = 192 * 1024;
/// Reserved protocol space inside the output budget. The observation/result
/// metadata frames and the diff metadata frame must always remain complete even
/// when an individual segment overflows its own budget, so the script carves
/// this many bytes out of [`SHOW_CHANGES_OUTPUT_BUDGET_BYTES`] before splitting
/// the rest across the status records, HEAD metadata, diff stat, and diff
/// hunks. Sized to comfortably cover all metadata bodies, fixed-length trailers,
/// and `printf` overhead in the worst case.
const SHOW_CHANGES_PROTOCOL_RESERVE_BYTES: usize = 8 * 1024;
/// Independent byte budget for the emitted status records (file entries only;
/// the branch header and the status result frame are always preserved). The
/// status byte budget is enforced *in addition to* [`SHOW_CHANGES_MAX_STATUS_FILES`]:
/// a record is only returned when both the count limit and the byte budget have
/// room, and an overlong path skips its record and reports a structured reason.
pub(crate) const SHOW_CHANGES_STATUS_BYTES: usize = 96 * 1024;
/// Byte budget for the `git diff --stat` segment.
pub(crate) const SHOW_CHANGES_DIFF_STAT_BYTES: usize = 24 * 1024;
/// Byte budget for the HEAD metadata segment (`git log -1`).
pub(crate) const SHOW_CHANGES_HEAD_BYTES: usize = 8 * 1024;
/// Byte budget for the emitted diff hunks segment (hunk bodies, file headers,
/// and preambles). Hunks are additionally bounded by `max_hunks` and
/// `max_hunk_lines`; this byte budget guarantees a single pathological diff
/// line cannot overflow the global budget.
pub(crate) const SHOW_CHANGES_DIFF_BYTES: usize = 48 * 1024;
const DEFAULT_GIT_LOG_LIMIT: usize = 20;
const MAX_GIT_LOG_LIMIT: usize = 100;
const MAX_GIT_LOG_SKIP: usize = 10_000;
const GIT_LOG_RECORD_SEP: char = '\u{1e}';
const GIT_LOG_UNIT_SEP: char = '\u{1f}';
const SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_FILES: usize = 5;
const SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_BYTES: u64 = 8192;
const SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_LINES: usize = 40;

// The per-segment byte budgets are each independently bounded in the
// production script; their sum plus the fixed protocol reserve must fit within
// the transport output budget, so the script's raw stdout is provably at or
// under `SHOW_CHANGES_OUTPUT_BUDGET_BYTES` whenever every segment's metadata
// frame is present. This compile-time check pins that invariant: changing any
// segment budget (or the reserve) without shrinking the sum below the budget
// fails to build, rather than silently overflowing transport.
const _: () = assert!(
    SHOW_CHANGES_STATUS_BYTES
        + SHOW_CHANGES_HEAD_BYTES
        + SHOW_CHANGES_DIFF_STAT_BYTES
        + SHOW_CHANGES_DIFF_BYTES
        + SHOW_CHANGES_PROTOCOL_RESERVE_BYTES
        <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES
);

/// Build the read-only `git_diff_summary` command. Runs `git status
/// --porcelain` and `git diff --stat` separated by a unique sentinel. No
/// mutating git subcommand is emitted.
pub(crate) fn git_diff_summary_command() -> String {
    format!(
        "git status --porcelain; printf '\\n{sentinel}\\n'; git diff --stat",
        sentinel = DIFF_SUMMARY_SENTINEL,
    )
}

pub(crate) fn normalize_git_log_limit(limit: Option<usize>) -> usize {
    limit
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_GIT_LOG_LIMIT)
        .min(MAX_GIT_LOG_LIMIT)
}

pub(crate) fn normalize_git_log_skip(skip: Option<usize>) -> usize {
    skip.unwrap_or(0).min(MAX_GIT_LOG_SKIP)
}

pub(crate) fn git_log_command(limit: usize, skip: usize) -> String {
    let limit_plus_one = limit.saturating_add(1);
    format!(
        "git log --decorate=short --date=iso-strict --pretty=format:'%H%x1f%h%x1f%D%x1f%an%x1f%ae%x1f%aI%x1f%s%x1e' -n {limit_plus_one} --skip {skip}",
    )
}

fn parse_git_log_refs(decorations: &str) -> Vec<String> {
    decorations
        .split(',')
        .flat_map(|part| {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else if let Some((head, branch)) = trimmed.split_once(" -> ") {
                vec![head.trim().to_string(), branch.trim().to_string()]
            } else if let Some(tag) = trimmed.strip_prefix("tag: ") {
                vec![tag.trim().to_string()]
            } else {
                vec![trimmed.to_string()]
            }
        })
        .collect()
}

pub(crate) fn parse_git_log_commits(stdout: &str, limit: usize) -> (Vec<Value>, bool) {
    let mut commits = Vec::new();
    let mut truncated = false;
    for record in stdout.split(GIT_LOG_RECORD_SEP) {
        let record = record.trim_matches(['\n', '\r']);
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.splitn(7, GIT_LOG_UNIT_SEP).collect();
        if fields.len() != 7 {
            continue;
        }
        if commits.len() >= limit {
            truncated = true;
            break;
        }
        commits.push(json!({
            "hash": fields[0],
            "short_hash": fields[1],
            "subject": fields[6],
            "author_name": fields[3],
            "author_email": fields[4],
            "author_date": fields[5],
            "refs": parse_git_log_refs(fields[2]),
        }));
    }
    (commits, truncated)
}

fn git_log_empty_repo(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("does not have any commits") || lower.contains("no commits yet")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShowChangesStatusObservationKind {
    Observed,
    NonGit,
    CommandFailed,
    OutputUnavailable,
}

impl ShowChangesStatusObservationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::NonGit => "non_git",
            Self::CommandFailed => "command_failed",
            Self::OutputUnavailable => "output_unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShowChangesStatusObservation {
    kind: ShowChangesStatusObservationKind,
    reason_code: Option<&'static str>,
    pub(crate) exit_code: Option<i32>,
    repository_probe: &'static str,
    repository_probe_exit_code: Option<i32>,
    git_available: bool,
}

impl ShowChangesStatusObservation {
    fn observed(exit_code: Option<i32>) -> Self {
        Self {
            kind: ShowChangesStatusObservationKind::Observed,
            reason_code: None,
            exit_code,
            repository_probe: "inside_worktree",
            repository_probe_exit_code: Some(0),
            git_available: true,
        }
    }

    pub(crate) fn status_observed(&self) -> bool {
        self.kind == ShowChangesStatusObservationKind::Observed
    }

    fn non_git(&self) -> bool {
        self.kind == ShowChangesStatusObservationKind::NonGit
    }

    pub(crate) fn as_json(&self) -> Value {
        json!({
            "status": self.kind.as_str(),
            "reason_code": self.reason_code,
            "exit_code": self.exit_code,
            "repository_probe": self.repository_probe,
            "repository_probe_exit_code": self.repository_probe_exit_code,
        })
    }
}

fn parse_status_result_field<'a>(result: &'a str, key: &str) -> Option<&'a str> {
    result.lines().find_map(|line| {
        let (field, value) = line.split_once('=')?;
        (field == key).then_some(value.trim())
    })
}

fn git_status_failure_reason(stderr: &str) -> &'static str {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("bad config")
        || lower.contains("invalid untracked files mode")
        || lower.contains("invalid value for") && lower.contains("config")
    {
        "git_status_config_error"
    } else if lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("access is denied")
    {
        "git_status_permission_denied"
    } else {
        "git_status_command_failed"
    }
}

pub(crate) fn parse_show_changes_status_observation(
    status_stdout: &str,
    status_result_stdout: &str,
    stderr: &str,
) -> ShowChangesStatusObservation {
    let exit_code = parse_status_result_field(status_result_stdout, "status_exit")
        .and_then(|value| value.parse::<i32>().ok());
    let repository_probe = parse_status_result_field(status_result_stdout, "repository_probe");
    let repository_probe_exit_code =
        parse_status_result_field(status_result_stdout, "repository_probe_exit")
            .and_then(|value| value.parse::<i32>().ok());
    let has_reliable_header = status_stdout
        .lines()
        .any(|line| parse_status_header(line).is_some());
    let command_failure_reason = git_status_failure_reason(stderr);

    match (exit_code, repository_probe) {
        (Some(0), Some("inside_worktree")) if has_reliable_header => {
            ShowChangesStatusObservation::observed(Some(0))
        }
        (Some(0), Some("inside_worktree")) => ShowChangesStatusObservation {
            kind: ShowChangesStatusObservationKind::OutputUnavailable,
            reason_code: Some("git_status_header_unavailable"),
            exit_code: Some(0),
            repository_probe: "inside_worktree",
            repository_probe_exit_code,
            git_available: true,
        },
        (Some(code), Some("outside_worktree")) if code != 0 => ShowChangesStatusObservation {
            kind: ShowChangesStatusObservationKind::NonGit,
            reason_code: Some("not_a_git_repository"),
            exit_code: Some(code),
            repository_probe: "outside_worktree",
            repository_probe_exit_code,
            git_available: false,
        },
        (Some(code), Some("inside_worktree")) if code != 0 => ShowChangesStatusObservation {
            kind: ShowChangesStatusObservationKind::CommandFailed,
            reason_code: Some(command_failure_reason),
            exit_code: Some(code),
            repository_probe: "inside_worktree",
            repository_probe_exit_code,
            git_available: true,
        },
        (Some(code), Some("unavailable")) if code != 0 => ShowChangesStatusObservation {
            kind: ShowChangesStatusObservationKind::CommandFailed,
            reason_code: Some(command_failure_reason),
            exit_code: Some(code),
            repository_probe: "unavailable",
            repository_probe_exit_code,
            git_available: false,
        },
        _ => ShowChangesStatusObservation {
            kind: ShowChangesStatusObservationKind::OutputUnavailable,
            reason_code: Some("git_status_result_unavailable"),
            exit_code,
            repository_probe: "unavailable",
            repository_probe_exit_code,
            git_available: false,
        },
    }
}

/// Build the graceful-degradation payload returned by `show_changes` when the
/// project is not a git repository. Git-backed status/diff is reported as
/// unavailable without dumping git's noisy stderr/usage; the session
/// sub-summary is still layered on by the caller via
/// `apply_show_changes_session`.
#[cfg(test)]
pub(crate) fn non_git_show_changes_payload(
    project: &str,
    exit_code: Option<i32>,
    include_diff: bool,
) -> serde_json::Value {
    non_git_show_changes_payload_with_observation(
        project,
        include_diff,
        ShowChangesStatusObservation {
            kind: ShowChangesStatusObservationKind::NonGit,
            reason_code: Some("not_a_git_repository"),
            exit_code,
            repository_probe: "outside_worktree",
            repository_probe_exit_code: exit_code,
            git_available: false,
        },
    )
}

fn non_git_show_changes_payload_with_observation(
    project: &str,
    include_diff: bool,
    observation: ShowChangesStatusObservation,
) -> serde_json::Value {
    let mut payload = json!({
        "project": project,
        "git_available": false,
        "non_git_project": true,
        "git_error": "not a git repository; git-backed diff unavailable",
        "branch": null,
        "upstream_status": "unobserved",
        "upstream_reason_code": "git_unavailable",
        "upstream": null,
        "ahead": null,
        "behind": null,
        "head": {
            "commit": null,
            "short": null,
            "summary": null,
        },
        "status_observation": observation.as_json(),
        "clean": null,
        "counts": {
            "modified": 0,
            "added": 0,
            "deleted": 0,
            "renamed": 0,
            "copied": 0,
            "untracked": 0,
            "conflicted": null,
            "staged": 0,
            "unstaged": 0,
        },
        "files": [],
        "files_total": null,
        "files_returned": 0,
        "files_truncated": false,
        "files_limit": SHOW_CHANGES_MAX_STATUS_FILES,
        "transport_safe": false,
        "output_budget_bytes": SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "output_truncated": false,
        "truncation_reasons": [],
        "diff_stat": "",
        "diff_exit": null,
        "diff_status": diff_status_json(None),
        "diff_stat_exit": null,
        "diff_stat_status": {
            "status": "output_unavailable", "exit_code": null, "reason_code": "git_unavailable",
        },
        "head_exit": null,
        "warnings": [],
        "suggested_next_actions": [
            "git-backed status/diff unavailable; project is not a git repository",
        ],
        "session": null,
        "exit_code": observation.exit_code,
        "stderr": "",
    });
    if include_diff {
        payload["untracked_previews"] = json!([]);
        payload["untracked_previews_truncated"] = json!(false);
    }
    set_show_changes_verdict(&mut payload);
    payload
}

/// Build the read-only, production-bounded `show_changes` command.
///
/// Unlike an unbounded `git status` capture, the status is *streamed* line by
/// line through a POSIX `while read` loop that:
///   * passes the branch header (`## ...`) through verbatim,
///   * emits at most `status_files_limit` changed-file records,
///   * counts every entry and classifies every entry by porcelain XY code so
///     `files_total` and all per-category counts stay exact even when the
///     returned records are truncated,
///   * captures the real `git status` exit code from the final producer line,
///   * runs the explicit repository probe only when status failed.
///
/// Each data/metadata pair ends in a fixed 30-byte `WCSF1` trailer containing
/// the pair kind and exact wire lengths. The parser walks backward by those
/// lengths, so frame bodies are never scanned for delimiter text. The status
/// metadata carries `status_exit`, `repository_probe`, `repository_probe_exit`, the
/// `files_total/files_returned/files_truncated/files_limit` metadata, and the
/// exact per-category counts
/// (`modified/added/deleted/renamed/copied/untracked/conflicted/staged/unstaged`).
///
/// When `include_diff` is set, the `git diff` is likewise bounded in the command
/// by `max_hunks` hunks and `max_hunk_lines` lines per hunk before it ever
/// reaches the transport, so transport tail-retention cannot drop the first
/// selected hunks. A trailing diff metadata frame reports the returned count,
/// emitted bytes, and independent count/line/byte truncation flags.
///
/// The shell script is held in a raw string with literal placeholders and
/// interpolated with `str::replace`, so nested `${...}` expansions stay
/// literal and never collide with Rust `format!` brace escaping.
///
/// No Python/Node helper or external temp script is used; only POSIX `sh`,
/// `git`, `printf`, and `grep`. No project worktree is mutated.
pub(crate) fn show_changes_command(
    include_diff: bool,
    max_hunks: usize,
    max_hunk_lines: usize,
) -> String {
    let status_files_limit = SHOW_CHANGES_MAX_STATUS_FILES;
    let diff_part = if include_diff {
        r#"; {
               git diff --unified=80; printf 'diff_exit=%s\n' "$?";
             } | {
               hc=0; in_hunk=0; lc=0; diff_exit_raw=; diff_bytes=0; file_buf=; stop_emit=0;
               trunc_count=0; trunc_lines=0; trunc_bytes=0; trunc_bytes_in_hunk=0; have=0; pending=;
               while IFS= read -r dl; do
                 next=$dl; dl=$pending; pending=$next;
                 if [ "$have" = 0 ]; then have=1; continue; fi;
                 case "$dl" in
                   '@@ '*)
                     if [ "$stop_emit" = 1 ]; then in_hunk=0; file_buf=; continue; fi;
                     if [ "$hc" -ge __HUNK_LIMIT__ ]; then trunc_count=1; stop_emit=1; in_hunk=0; file_buf=; continue; fi;
                     preamble_len=${#file_buf}; header_len=$((${#dl}+1)); candidate_len=$((preamble_len+header_len));
                     if [ "$((diff_bytes + candidate_len))" -gt __DIFF_BYTE_BUDGET__ ]; then trunc_bytes=1; stop_emit=1; in_hunk=0; file_buf=; continue; fi;
                     if [ -n "$file_buf" ]; then printf '%s' "$file_buf"; file_buf=; fi;
                     hc=$((hc+1)); lc=1; in_hunk=1; printf '%s\n' "$dl"; diff_bytes=$((diff_bytes+candidate_len)) ;;
                   *)
                     case "$dl" in
                       'diff --git '*)
                         in_hunk=0; file_buf=;
                         if [ "$stop_emit" = 0 ]; then
                           line_len=$((${#dl}+1));
                           if [ "$((diff_bytes + line_len))" -gt __DIFF_BYTE_BUDGET__ ]; then trunc_bytes=1; stop_emit=1; else file_buf="$dl
"; fi;
                         fi;
                         continue ;;
                     esac;
                     if [ "$in_hunk" = 1 ]; then
                       if [ "$stop_emit" = 1 ]; then continue; fi;
                       if [ "$lc" -ge __LINE_LIMIT__ ]; then trunc_lines=1;
                       else
                         line_len=$((${#dl}+1));
                         if [ "$((diff_bytes + line_len))" -gt __DIFF_BYTE_BUDGET__ ]; then trunc_bytes=1; trunc_bytes_in_hunk=1; stop_emit=1;
                         else printf '%s\n' "$dl"; lc=$((lc+1)); diff_bytes=$((diff_bytes+line_len)); fi;
                       fi;
                     elif [ "$stop_emit" = 0 ]; then
                       line_len=$((${#dl}+1)); file_len=${#file_buf};
                       if [ "$((diff_bytes + file_len + line_len))" -gt __DIFF_BYTE_BUDGET__ ]; then trunc_bytes=1; stop_emit=1; file_buf=;
                       else file_buf="$file_buf$dl
"; fi;
                     fi ;;
                 esac;
               done;
               case "$pending" in diff_exit=*) diff_exit_raw=${pending#diff_exit=} ;; *) diff_exit_raw= ;; esac;
               if [ "$stop_emit" = 0 ] && [ -n "$file_buf" ]; then printf '%s' "$file_buf"; diff_bytes=$((diff_bytes+${#file_buf})); fi;
               diff_wire_bytes=$diff_bytes; diff_frame_bytes=$diff_wire_bytes;
               if [ "$diff_frame_bytes" -gt 0 ]; then diff_frame_bytes=$((diff_frame_bytes-1)); fi;
               dm=$(printf 'diff_exit=%s\ndiff_hunks_returned=%s\ndiff_hunks_truncated=%s\ndiff_trunc_hunk_count=%s\ndiff_trunc_hunk_lines=%s\ndiff_trunc_bytes=%s\ndiff_trunc_bytes_in_hunk=%s\ndiff_bytes=%s' \
                 "$diff_exit_raw" "$hc" "$((trunc_count || trunc_lines || trunc_bytes))" "$trunc_count" "$trunc_lines" "$trunc_bytes" "$trunc_bytes_in_hunk" "$diff_frame_bytes");
               printf '%s\n' "$dm"; printf 'WCSF1:D:%010d:%010d\n' "$diff_wire_bytes" "$(( ${#dm}+1 ))";
             }"#
        .replace("__HUNK_LIMIT__", &max_hunks.to_string())
        .replace("__LINE_LIMIT__", &max_hunk_lines.to_string())
        .replace("__DIFF_BYTE_BUDGET__", &SHOW_CHANGES_DIFF_BYTES.to_string())
    } else {
        String::new()
    };

    let script = r#"{ LC_ALL=C; export LC_ALL;
         { git status --porcelain=v1 -b; printf 'status_exit=%s\n' "$?"; } | {
           n=0; total=0; trunc=0; trunc_count=0; trunc_bytes=0; trunc_path=0; exit_raw=; status_bytes=0;
           c_mod=0; c_add=0; c_del=0; c_ren=0; c_cop=0; c_unt=0; c_conf=0; c_stg=0; c_uns=0; have=0; pending=;
           while IFS= read -r line; do
             next=$line; line=$pending; pending=$next;
             if [ "$have" = 0 ]; then have=1; continue; fi;
             case "$line" in
               '## '*) printf '%s\n' "$line"; status_bytes=$((status_bytes+${#line}+1)) ;;
               *)
                 total=$((total+1)); xc=${line%"${line#?}"}; yc=${line#?}; yc=${yc%"${yc#?}"};
                 case "$xc$yc" in
                   '??') c_unt=$((c_unt+1)); label=untracked ;;
                   UU|AA|DD|AU|UA|DU|UD) c_conf=$((c_conf+1)); label=conflicted ;;
                   *R*) c_ren=$((c_ren+1)); label=renamed ;;
                   *C*) c_cop=$((c_cop+1)); label=copied ;;
                   *D*) c_del=$((c_del+1)); label=deleted ;;
                   *A*) c_add=$((c_add+1)); label=added ;;
                   *) c_mod=$((c_mod+1)); label=modified ;;
                 esac;
                 if [ "$label" != untracked ] && [ "$label" != conflicted ]; then
                   if [ "$xc" != ' ' ] && [ "$xc" != '?' ]; then c_stg=$((c_stg+1)); fi;
                   if [ "$yc" != ' ' ] && [ "$yc" != '?' ]; then c_uns=$((c_uns+1)); fi;
                 fi;
                 rec_len=$((${#line}+1));
                 if [ "$n" -ge __STATUS_LIMIT__ ]; then trunc=1; trunc_count=1;
                 elif [ "$((status_bytes + rec_len))" -gt __STATUS_BYTE_BUDGET__ ]; then trunc=1; trunc_bytes=1;
                 elif [ "$rec_len" -gt __STATUS_MAX_RECORD_BYTES__ ]; then trunc=1; trunc_path=1;
                 else printf '%s\n' "$line"; n=$((n+1)); status_bytes=$((status_bytes+rec_len)); fi ;;
             esac;
           done;
           case "$pending" in status_exit=*) exit_raw=${pending#status_exit=} ;; *) exit_raw= ;; esac;
           status_wire_bytes=$status_bytes; if [ "$status_bytes" -gt 0 ]; then status_bytes=$((status_bytes-1)); fi;
           if [ -z "$exit_raw" ]; then exit_raw=0; fi;
           if [ "$exit_raw" -eq 0 ] 2>/dev/null; then repo_probe=inside_worktree; repo_probe_exit=0;
           else
             repo_probe_stdout=$(LC_ALL=C git rev-parse --is-inside-work-tree 2>&1); repo_probe_exit=$?;
             if [ "$repo_probe_exit" -eq 0 ] && [ "$repo_probe_stdout" = true ]; then repo_probe=inside_worktree;
             elif printf '%s\n' "$repo_probe_stdout" | grep -Fq 'not a git repository'; then repo_probe=outside_worktree;
             else repo_probe=unavailable; fi;
           fi;
           sm=$(printf 'status_exit=%s\nrepository_probe=%s\nrepository_probe_exit=%s\nfiles_total=%s\nfiles_returned=%s\nfiles_truncated=%s\nfiles_limit=%s\nstatus_bytes=%s\nstatus_trunc_count=%s\nstatus_trunc_bytes=%s\nstatus_trunc_path=%s\nmodified=%s\nadded=%s\ndeleted=%s\nrenamed=%s\ncopied=%s\nuntracked=%s\nconflicted=%s\nstaged=%s\nunstaged=%s' \
             "$exit_raw" "$repo_probe" "$repo_probe_exit" "$total" "$n" "$trunc" __STATUS_LIMIT__ "$status_bytes" "$trunc_count" "$trunc_bytes" "$trunc_path" \
             "$c_mod" "$c_add" "$c_del" "$c_ren" "$c_cop" "$c_unt" "$c_conf" "$c_stg" "$c_uns");
           printf '%s\n' "$sm"; printf 'WCSF1:S:%010d:%010d\n' "$status_wire_bytes" "$(( ${#sm}+1 ))";
           head_exit_raw=; he=0; head_frame_bytes=0; head_commit=; head_short=; head_subject=;
           git log -1 --format=%s >/dev/null 2>&1; head_exit_raw=$?;
           if [ "$head_exit_raw" -eq 0 ] 2>/dev/null; then
             head_commit=$(git rev-parse --verify HEAD 2>/dev/null); head_short=$(git rev-parse --short "$head_commit" 2>/dev/null);
             if [ -n "$head_commit" ] && [ -n "$head_short" ]; then
               head_prefix=$(printf 'commit=%s\nshort=%s\nsummary=' "$head_commit" "$head_short"); head_prefix_bytes=${#head_prefix};
               head_subject_limit=$((__HEAD_BYTE_BUDGET__-head_prefix_bytes));
               if [ "$head_subject_limit" -lt 0 ]; then he=1;
               else
                 head_subject=$(git log -1 --format=%s "$head_commit" 2>/dev/null | dd bs=1 count=$((head_subject_limit+1)) 2>/dev/null);
                 git log -1 --format=%s "$head_commit" >/dev/null 2>&1; head_exit_raw=$?; head_subject_bytes=${#head_subject};
                 if [ "$head_subject_bytes" -gt "$head_subject_limit" ]; then he=1;
                 elif [ "$head_exit_raw" -eq 0 ] 2>/dev/null; then printf '%s%s' "$head_prefix" "$head_subject"; head_frame_bytes=$((head_prefix_bytes+head_subject_bytes)); fi;
               fi;
             fi;
           fi;
           hm=$(printf 'head_exit=%s\nhead_truncated=%s\nhead_bytes=%s' "$head_exit_raw" "$he" "$head_frame_bytes");
           printf '%s\n' "$hm"; printf 'WCSF1:H:%010d:%010d\n' "$head_frame_bytes" "$(( ${#hm}+1 ))";
           { git diff --stat 2>/dev/null; printf '__WEBCODEX_STAT_EXIT__=%s\n' "$?"; } | {
             sb=0; se=0; stat_exit_raw=; have=0; pending=;
             while IFS= read -r sline; do
               next=$sline; sline=$pending; pending=$next;
               if [ "$have" = 0 ]; then have=1; continue; fi;
               ll=$((${#sline}+1));
               if [ "$((sb + ll))" -gt __DIFF_STAT_BUDGET__ ]; then se=1; else printf '%s\n' "$sline"; sb=$((sb+ll)); fi;
             done;
             case "$pending" in __WEBCODEX_STAT_EXIT__=*) stat_exit_raw=${pending#__WEBCODEX_STAT_EXIT__=} ;; *) stat_exit_raw= ;; esac;
             stat_wire_bytes=$sb; if [ "$sb" -gt 0 ]; then sb=$((sb-1)); fi;
             tm=$(printf 'diff_stat_exit=%s\ndiff_stat_truncated=%s\ndiff_stat_bytes=%s' "$stat_exit_raw" "$se" "$sb");
             printf '%s\n' "$tm"; printf 'WCSF1:T:%010d:%010d\n' "$stat_wire_bytes" "$(( ${#tm}+1 ))";
           }__DIFF_PART__;
           final_exit=$?;
           if [ "$exit_raw" -ne 0 ] && [ "$exit_raw" -ge 0 ] 2>/dev/null; then exit "$exit_raw"; else exit "$final_exit"; fi;
         }; }"#;

    let command = script
        .replace("__STATUS_LIMIT__", &status_files_limit.to_string())
        .replace(
            "__STATUS_BYTE_BUDGET__",
            &SHOW_CHANGES_STATUS_BYTES.to_string(),
        )
        .replace("__STATUS_MAX_RECORD_BYTES__", &(4 * 1024).to_string())
        .replace(
            "__DIFF_STAT_BUDGET__",
            &SHOW_CHANGES_DIFF_STAT_BYTES.to_string(),
        )
        .replace("__HEAD_BYTE_BUDGET__", &SHOW_CHANGES_HEAD_BYTES.to_string())
        .replace("__DIFF_PART__", &diff_part);
    command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parsed, production-bounded `show_changes` stdout frames. The status result
/// frame carries the authoritative `files_*` and per-category counts reported
/// by the command's streaming loop, so totals stay exact even when the
/// returned file records were truncated by the production-side limit.
#[derive(Debug, Clone, Default)]
pub(crate) struct ShowChangesStdout {
    pub(crate) framing_valid: bool,
    pub(crate) status: String,
    pub(crate) status_result: String,
    pub(crate) head: String,
    pub(crate) stat: String,
    pub(crate) diff: String,
    /// Authoritative status metadata parsed from the current bounded frame.
    /// `None` means the frame was absent or invalid.
    pub(crate) files_total: Option<usize>,
    pub(crate) files_returned: Option<usize>,
    pub(crate) files_truncated: Option<bool>,
    pub(crate) files_limit: Option<usize>,
    pub(crate) counts_modified: Option<usize>,
    pub(crate) counts_added: Option<usize>,
    pub(crate) counts_deleted: Option<usize>,
    pub(crate) counts_renamed: Option<usize>,
    pub(crate) counts_copied: Option<usize>,
    pub(crate) counts_untracked: Option<usize>,
    pub(crate) counts_conflicted: Option<usize>,
    pub(crate) counts_staged: Option<usize>,
    pub(crate) counts_unstaged: Option<usize>,
    /// Per-segment byte-budget truncation flags parsed from the status result
    /// frame. `None` when the current frame is absent or invalid.
    pub(crate) status_trunc_count: Option<bool>,
    pub(crate) status_trunc_bytes: Option<bool>,
    pub(crate) status_trunc_path: Option<bool>,
    /// Exact bytes in the parsed status frame.
    pub(crate) status_bytes: Option<usize>,
    /// HEAD metadata frame: real `git log -1` exit code and whether HEAD data
    /// was emitted. `None` when the frame is absent.
    pub(crate) head_exit: Option<i32>,
    /// Whether the HEAD metadata segment was dropped for overflowing its byte
    /// budget (a pathological commit subject). `None` when the frame is absent.
    pub(crate) head_truncated: Option<bool>,
    /// Exact bytes in the parsed HEAD frame.
    pub(crate) head_bytes: Option<usize>,
    /// `git diff --stat` metadata frame. `None` when the frame is absent.
    pub(crate) diff_stat_exit: Option<i32>,
    pub(crate) diff_stat_truncated: Option<bool>,
    /// Exact bytes in the parsed diff-stat frame.
    pub(crate) diff_stat_bytes: Option<usize>,
    /// Number of diff hunks the production-side loop returned, parsed from the
    /// trailing diff metadata frame. `None` when `include_diff` is false or the
    /// frame is absent.
    pub(crate) diff_hunks_returned: Option<usize>,
    /// Whether the production-side loop dropped any diff data.
    pub(crate) diff_hunks_truncated: Option<bool>,
    /// Independent production-side diff truncation flags.
    pub(crate) diff_trunc_hunk_count: Option<bool>,
    pub(crate) diff_trunc_hunk_lines: Option<bool>,
    pub(crate) diff_trunc_bytes: Option<bool>,
    /// Whether the diff byte budget fired while emitting the body of an
    /// already-returned hunk. This distinguishes omitted current-hunk lines
    /// from a page boundary that only omitted later file/hunk records.
    pub(crate) diff_trunc_bytes_in_hunk: Option<bool>,
    /// Real full `git diff` exit code parsed from the diff metadata frame.
    pub(crate) diff_exit: Option<i32>,
    /// Bytes emitted in the bounded diff segment, parsed from the diff metadata
    /// frame.
    pub(crate) diff_bytes: Option<usize>,
}

fn parse_optional_usize(result: &str, key: &str) -> Option<usize> {
    parse_status_result_field(result, key).and_then(|v| v.parse::<usize>().ok())
}

fn parse_optional_bool(result: &str, key: &str) -> Option<bool> {
    parse_status_result_field(result, key).and_then(|v| match v {
        "0" | "false" => Some(false),
        "1" | "true" => Some(true),
        _ => None,
    })
}

fn parse_fixed_decimal(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn parse_show_changes_wire_block(
    stdout: &str,
    end: usize,
    kind: u8,
) -> Option<(&str, &str, usize)> {
    let bytes = stdout.as_bytes();
    let trailer_start = end.checked_sub(SHOW_CHANGES_BLOCK_TRAILER_BYTES)?;
    let trailer = bytes.get(trailer_start..end)?;
    if trailer.get(..6)? != SHOW_CHANGES_BLOCK_MAGIC
        || trailer.get(6).copied()? != kind
        || trailer.get(7).copied()? != b':'
        || trailer.get(18).copied()? != b':'
        || trailer.get(29).copied()? != b'\n'
    {
        return None;
    }
    let data_wire_bytes = parse_fixed_decimal(trailer.get(8..18)?)?;
    let meta_wire_bytes = parse_fixed_decimal(trailer.get(19..29)?)?;
    let meta_start = trailer_start.checked_sub(meta_wire_bytes)?;
    let data_start = meta_start.checked_sub(data_wire_bytes)?;
    let data = std::str::from_utf8(bytes.get(data_start..meta_start)?).ok()?;
    let meta = std::str::from_utf8(bytes.get(meta_start..trailer_start)?).ok()?;
    Some((data, meta, data_start))
}

fn strip_wire_lf(value: &str) -> Option<String> {
    if value.is_empty() {
        Some(String::new())
    } else {
        value.strip_suffix('\n').map(ToOwned::to_owned)
    }
}

#[cfg(test)]
pub(crate) fn framed_show_changes_test_block(kind: char, body: &str, metadata: &str) -> String {
    format!(
        "{body}{metadata}WCSF1:{kind}:{:010}:{:010}\n",
        body.len(),
        metadata.len()
    )
}

#[cfg(test)]
pub(crate) fn framed_clean_show_changes_test_stdout(subject: &str, include_diff: bool) -> String {
    let head = format!("commit=abc123\nshort=abc123\nsummary={subject}\n");
    let head_bytes = head.strip_suffix('\n').unwrap_or(&head).len();
    let mut stdout = format!(
        "{}{}{}",
        framed_show_changes_test_block(
            'S',
            "## main\n",
            "status_exit=0\nrepository_probe=inside_worktree\nrepository_probe_exit=0\nfiles_total=0\nfiles_returned=0\nfiles_truncated=0\nfiles_limit=200\nmodified=0\nadded=0\ndeleted=0\nrenamed=0\ncopied=0\nuntracked=0\nconflicted=0\nstaged=0\nunstaged=0\nstatus_trunc_count=0\nstatus_trunc_bytes=0\nstatus_trunc_path=0\nstatus_bytes=7\n"
        ),
        framed_show_changes_test_block(
            'H',
            &head,
            &format!("head_exit=0\nhead_truncated=0\nhead_bytes={head_bytes}\n")
        ),
        framed_show_changes_test_block(
            'T',
            "",
            "diff_stat_exit=0\ndiff_stat_truncated=0\ndiff_stat_bytes=0\n"
        )
    );
    if include_diff {
        stdout.push_str(&framed_show_changes_test_block(
            'D',
            "",
            "diff_exit=0\ndiff_hunks_returned=0\ndiff_hunks_truncated=0\ndiff_trunc_hunk_count=0\ndiff_trunc_hunk_lines=0\ndiff_trunc_bytes=0\ndiff_trunc_bytes_in_hunk=0\ndiff_bytes=0\n",
        ));
    }
    stdout
}

fn parse_framed_show_changes_stdout(stdout: &str, include_diff: bool) -> Option<ShowChangesStdout> {
    let mut cursor = stdout.len();
    let (diff, diff_meta) = if include_diff {
        let (body, meta, start) = parse_show_changes_wire_block(stdout, cursor, b'D')?;
        cursor = start;
        (strip_wire_lf(body)?, strip_wire_lf(meta)?)
    } else {
        (String::new(), String::new())
    };
    let (stat, stat_meta, start) = parse_show_changes_wire_block(stdout, cursor, b'T')?;
    cursor = start;
    let (head, head_meta, start) = parse_show_changes_wire_block(stdout, cursor, b'H')?;
    cursor = start;
    let (status, status_result, start) = parse_show_changes_wire_block(stdout, cursor, b'S')?;
    if start != 0 {
        return None;
    }
    let status = strip_wire_lf(status)?;
    let status_result = strip_wire_lf(status_result)?;
    let stat = strip_wire_lf(stat)?;
    let stat_meta = strip_wire_lf(stat_meta)?;
    let head_meta = strip_wire_lf(head_meta)?;

    Some(ShowChangesStdout {
        framing_valid: true,
        files_total: parse_optional_usize(&status_result, "files_total"),
        files_returned: parse_optional_usize(&status_result, "files_returned"),
        files_truncated: parse_optional_bool(&status_result, "files_truncated"),
        files_limit: parse_optional_usize(&status_result, "files_limit"),
        counts_modified: parse_optional_usize(&status_result, "modified"),
        counts_added: parse_optional_usize(&status_result, "added"),
        counts_deleted: parse_optional_usize(&status_result, "deleted"),
        counts_renamed: parse_optional_usize(&status_result, "renamed"),
        counts_copied: parse_optional_usize(&status_result, "copied"),
        counts_untracked: parse_optional_usize(&status_result, "untracked"),
        counts_conflicted: parse_optional_usize(&status_result, "conflicted"),
        counts_staged: parse_optional_usize(&status_result, "staged"),
        counts_unstaged: parse_optional_usize(&status_result, "unstaged"),
        status_trunc_count: parse_optional_bool(&status_result, "status_trunc_count"),
        status_trunc_bytes: parse_optional_bool(&status_result, "status_trunc_bytes"),
        status_trunc_path: parse_optional_bool(&status_result, "status_trunc_path"),
        status_bytes: parse_optional_usize(&status_result, "status_bytes"),
        head_exit: parse_status_result_field(&head_meta, "head_exit")
            .and_then(|value| value.parse().ok()),
        head_truncated: parse_optional_bool(&head_meta, "head_truncated"),
        head_bytes: parse_optional_usize(&head_meta, "head_bytes"),
        diff_stat_exit: parse_status_result_field(&stat_meta, "diff_stat_exit")
            .and_then(|value| value.parse().ok()),
        diff_stat_truncated: parse_optional_bool(&stat_meta, "diff_stat_truncated"),
        diff_stat_bytes: parse_optional_usize(&stat_meta, "diff_stat_bytes"),
        diff_hunks_returned: parse_optional_usize(&diff_meta, "diff_hunks_returned"),
        diff_hunks_truncated: parse_optional_bool(&diff_meta, "diff_hunks_truncated"),
        diff_trunc_hunk_count: parse_optional_bool(&diff_meta, "diff_trunc_hunk_count"),
        diff_trunc_hunk_lines: parse_optional_bool(&diff_meta, "diff_trunc_hunk_lines"),
        diff_trunc_bytes: parse_optional_bool(&diff_meta, "diff_trunc_bytes"),
        diff_trunc_bytes_in_hunk: parse_optional_bool(&diff_meta, "diff_trunc_bytes_in_hunk"),
        diff_exit: parse_status_result_field(&diff_meta, "diff_exit")
            .and_then(|value| value.parse().ok()),
        diff_bytes: parse_optional_usize(&diff_meta, "diff_bytes"),
        status,
        status_result,
        head: head.to_string(),
        stat,
        diff,
    })
}

pub(crate) fn split_show_changes_stdout(stdout: &str, include_diff: bool) -> ShowChangesStdout {
    parse_framed_show_changes_stdout(stdout, include_diff).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedStatusHeader {
    branch: Option<String>,
    upstream_status: &'static str,
    upstream_reason_code: Option<&'static str>,
    upstream: Option<String>,
    ahead: Option<u64>,
    behind: Option<u64>,
}

fn parsed_branch_name(raw: &str) -> Option<String> {
    let branch = raw.trim().trim_matches('"');
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

pub(crate) fn parse_status_header(line: &str) -> Option<ParsedStatusHeader> {
    let rest = line.strip_prefix("## ")?;

    for prefix in ["No commits yet on ", "Initial commit on "] {
        if let Some(branch) = rest.strip_prefix(prefix) {
            return Some(ParsedStatusHeader {
                branch: parsed_branch_name(branch),
                upstream_status: "absent",
                upstream_reason_code: None,
                upstream: None,
                ahead: None,
                behind: None,
            });
        }
    }

    if rest == "HEAD" || rest == "HEAD (no branch)" || rest.starts_with("HEAD (detached ") {
        return Some(ParsedStatusHeader {
            branch: None,
            upstream_status: "absent",
            upstream_reason_code: None,
            upstream: None,
            ahead: None,
            behind: None,
        });
    }

    let Some((local, tracking)) = rest.split_once("...") else {
        return Some(ParsedStatusHeader {
            branch: parsed_branch_name(rest.split(" [").next().unwrap_or(rest)),
            upstream_status: "absent",
            upstream_reason_code: None,
            upstream: None,
            ahead: None,
            behind: None,
        });
    };

    let (upstream, counts) = tracking
        .split_once(" [")
        .map_or((tracking, None), |(upstream, counts)| {
            (upstream, Some(counts))
        });
    let upstream = parsed_branch_name(upstream);
    let mut ahead = None;
    let mut behind = None;
    let mut gone = false;
    if let Some(counts) = counts {
        for item in counts.trim_end_matches(']').split(',') {
            let item = item.trim();
            if item == "gone" {
                gone = true;
            } else if let Some(value) = item.strip_prefix("ahead ") {
                ahead = value.parse().ok();
            } else if let Some(value) = item.strip_prefix("behind ") {
                behind = value.parse().ok();
            }
        }
    }

    if gone {
        return Some(ParsedStatusHeader {
            branch: parsed_branch_name(local),
            upstream_status: "gone",
            upstream_reason_code: Some("upstream_gone"),
            upstream,
            ahead: None,
            behind: None,
        });
    }

    if upstream.is_some() {
        ahead.get_or_insert(0);
        behind.get_or_insert(0);
    }
    Some(ParsedStatusHeader {
        branch: parsed_branch_name(local),
        upstream_status: "available",
        upstream_reason_code: None,
        upstream,
        ahead,
        behind,
    })
}

fn parse_show_changes_head_frame(head: &str) -> Option<(&str, &str, &str)> {
    let mut lines = head.split('\n');
    let commit = lines.next()?.strip_prefix("commit=")?;
    let short = lines.next()?.strip_prefix("short=")?;
    let summary = lines.next()?.strip_prefix("summary=")?;
    if commit.is_empty() || short.is_empty() || lines.next().is_some() {
        return None;
    }
    Some((commit, short, summary))
}

fn parse_show_changes_head(head: &str) -> serde_json::Value {
    if let Some((commit, short, summary)) = parse_show_changes_head_frame(head) {
        return json!({
            "commit": commit,
            "short": short,
            "summary": summary,
        });
    }
    json!({
        "commit": null,
        "short": null,
        "summary": null,
    })
}

fn frame_bytes_match(value: &str, reported: Option<usize>, budget: usize) -> bool {
    let actual = value.len();
    actual <= budget && reported == Some(actual)
}

fn show_changes_transport_safe(
    frames: &ShowChangesStdout,
    include_diff: bool,
    max_hunks: usize,
) -> bool {
    if !frames.framing_valid {
        return false;
    }
    let status_exit = parse_status_result_field(&frames.status_result, "status_exit")
        .and_then(|value| value.parse::<i32>().ok());
    let repository_probe = parse_status_result_field(&frames.status_result, "repository_probe");
    let repository_probe_exit =
        parse_status_result_field(&frames.status_result, "repository_probe_exit")
            .and_then(|value| value.parse::<i32>().ok());
    let (
        Some(_),
        Some("inside_worktree" | "outside_worktree" | "unavailable"),
        Some(_),
        Some(files_total),
        Some(files_returned),
        Some(files_truncated),
        Some(files_limit),
        Some(status_trunc_count),
        Some(status_trunc_bytes),
        Some(status_trunc_path),
    ) = (
        status_exit,
        repository_probe,
        repository_probe_exit,
        frames.files_total,
        frames.files_returned,
        frames.files_truncated,
        frames.files_limit,
        frames.status_trunc_count,
        frames.status_trunc_bytes,
        frames.status_trunc_path,
    )
    else {
        return false;
    };

    let category_total = [
        frames.counts_modified,
        frames.counts_added,
        frames.counts_deleted,
        frames.counts_renamed,
        frames.counts_copied,
        frames.counts_untracked,
        frames.counts_conflicted,
    ]
    .into_iter()
    .try_fold(0usize, |sum, value| sum.checked_add(value?));
    let status_records = frames
        .status
        .lines()
        .filter(|line| !line.starts_with("## "))
        .count();
    let status_truncated_by_reason = status_trunc_count || status_trunc_bytes || status_trunc_path;
    let status_valid = frame_bytes_match(
        &frames.status,
        frames.status_bytes,
        SHOW_CHANGES_STATUS_BYTES,
    ) && frames.counts_staged.is_some()
        && frames.counts_unstaged.is_some()
        && files_limit == SHOW_CHANGES_MAX_STATUS_FILES
        && files_returned == status_records
        && files_total >= files_returned
        && files_truncated == (files_total > files_returned)
        && files_truncated == status_truncated_by_reason
        && category_total == Some(files_total);

    let (Some(head_exit), Some(head_truncated)) = (frames.head_exit, frames.head_truncated) else {
        return false;
    };
    let head_shape_valid = if head_truncated {
        frames.head.is_empty() && frames.head_bytes == Some(0)
    } else if head_exit == 0 {
        parse_show_changes_head_frame(&frames.head).is_some()
    } else {
        frames.head.is_empty() && frames.head_bytes == Some(0)
    };
    let head_valid = frame_bytes_match(&frames.head, frames.head_bytes, SHOW_CHANGES_HEAD_BYTES)
        && head_shape_valid;

    let stat_valid = frames.diff_stat_exit.is_some()
        && frames.diff_stat_truncated.is_some()
        && frame_bytes_match(
            &frames.stat,
            frames.diff_stat_bytes,
            SHOW_CHANGES_DIFF_STAT_BYTES,
        );

    let diff_valid = if include_diff {
        let (
            Some(_),
            Some(hunks_returned),
            Some(hunks_truncated),
            Some(trunc_count),
            Some(trunc_lines),
            Some(trunc_bytes),
            Some(trunc_bytes_in_hunk),
        ) = (
            frames.diff_exit,
            frames.diff_hunks_returned,
            frames.diff_hunks_truncated,
            frames.diff_trunc_hunk_count,
            frames.diff_trunc_hunk_lines,
            frames.diff_trunc_bytes,
            frames.diff_trunc_bytes_in_hunk,
        )
        else {
            return false;
        };
        let actual_hunks = frames
            .diff
            .lines()
            .filter(|line| line.starts_with("@@ "))
            .count();
        frame_bytes_match(&frames.diff, frames.diff_bytes, SHOW_CHANGES_DIFF_BYTES)
            && hunks_returned == actual_hunks
            && hunks_returned <= max_hunks
            && hunks_truncated == (trunc_count || trunc_lines || trunc_bytes)
            && (!trunc_bytes_in_hunk || trunc_bytes)
    } else {
        true
    };

    status_valid && head_valid && stat_valid && diff_valid
}

fn porcelain_path(path_part: &str) -> (String, Option<String>) {
    let path_part = path_part.trim().trim_matches('"');
    if let Some((old, new)) = path_part.split_once(" -> ") {
        (
            new.trim().trim_matches('"').to_string(),
            Some(old.trim().trim_matches('"').to_string()),
        )
    } else {
        (path_part.to_string(), None)
    }
}

fn is_unmerged_status(x: char, y: char) -> bool {
    // Porcelain v1 unmerged/conflict pairs (UU, AA, DD, AU, UA, DU, UD, ...).
    x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D')
}

fn status_label(x: char, y: char) -> &'static str {
    if x == '?' && y == '?' {
        return "untracked";
    }
    if is_unmerged_status(x, y) {
        return "conflicted";
    }
    if x == 'R' || y == 'R' {
        "renamed"
    } else if x == 'C' || y == 'C' {
        "copied"
    } else if x == 'D' || y == 'D' {
        "deleted"
    } else if x == 'A' || y == 'A' {
        "added"
    } else {
        "modified"
    }
}

fn looks_like_smoke_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("smoke")
        || lower.contains("tmp")
        || lower.contains("test")
        || lower.contains("anchor")
}

#[cfg(test)]
pub(crate) fn parse_show_changes_output(
    project: &str,
    status_stdout: &str,
    head_stdout: &str,
    diff_stat: &str,
    diff_stdout: Option<&str>,
    max_hunks: usize,
    max_hunk_lines: usize,
    exit_code: Option<i32>,
    stderr: &str,
) -> serde_json::Value {
    let has_reliable_header = status_stdout
        .lines()
        .any(|line| parse_status_header(line).is_some());
    let observation = if exit_code == Some(0) && has_reliable_header {
        ShowChangesStatusObservation::observed(exit_code)
    } else if exit_code == Some(0) {
        ShowChangesStatusObservation {
            kind: ShowChangesStatusObservationKind::OutputUnavailable,
            reason_code: Some("git_status_header_unavailable"),
            exit_code,
            repository_probe: "inside_worktree",
            repository_probe_exit_code: Some(0),
            git_available: true,
        }
    } else {
        ShowChangesStatusObservation {
            kind: ShowChangesStatusObservationKind::CommandFailed,
            reason_code: Some("git_status_command_failed"),
            exit_code,
            repository_probe: if has_reliable_header {
                "inside_worktree"
            } else {
                "unavailable"
            },
            repository_probe_exit_code: None,
            git_available: has_reliable_header,
        }
    };
    let frames = ShowChangesStdout {
        status: status_stdout.to_string(),
        head: head_stdout.to_string(),
        stat: diff_stat.to_string(),
        diff: diff_stdout.unwrap_or_default().to_string(),
        ..ShowChangesStdout::default()
    };
    let diff_for_parse = if diff_stdout.is_some() {
        Some(frames.diff.as_str())
    } else {
        None
    };
    parse_show_changes_output_with_observation(
        project,
        &frames.status,
        &frames.head,
        &frames.stat,
        diff_for_parse,
        max_hunks,
        max_hunk_lines,
        exit_code,
        stderr,
        observation,
        &frames,
    )
}

/// Map a full `git diff` exit code to the structured `diff_status` object
/// reported by `show_changes`. `observed` means the diff exit code was
/// captured (0 = clean diff, non-zero = command failure); `command_failed`
/// means a non-zero exit was observed; `output_unavailable` means the exit
/// code could not be captured by the production-side loop.
fn diff_status_json(diff_exit: Option<i32>) -> serde_json::Value {
    match diff_exit {
        Some(0) => json!({"status": "observed", "exit_code": 0}),
        Some(code) => json!({"status": "command_failed", "exit_code": code}),
        None => json!({"status": "output_unavailable", "exit_code": null}),
    }
}

/// Map the independently captured `git diff --stat` exit code to a strict
/// observation object. Unlike `transport_safe`, this status describes whether
/// the inspection itself succeeded and therefore participates in ToolResult
/// success for confirmed Git worktrees.
fn diff_stat_status_json(diff_stat_exit: Option<i32>) -> serde_json::Value {
    match diff_stat_exit {
        Some(0) => json!({
            "status": "observed", "exit_code": 0, "reason_code": null,
        }),
        Some(code) => json!({
            "status": "command_failed", "exit_code": code,
            "reason_code": "git_diff_stat_command_failed",
        }),
        None => json!({
            "status": "output_unavailable", "exit_code": null,
            "reason_code": "git_diff_stat_result_unavailable",
        }),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn parse_show_changes_output_with_observation(
    project: &str,
    status_stdout: &str,
    head_stdout: &str,
    diff_stat: &str,
    diff_stdout: Option<&str>,
    max_hunks: usize,
    max_hunk_lines: usize,
    exit_code: Option<i32>,
    stderr: &str,
    observation: ShowChangesStatusObservation,
    frames: &ShowChangesStdout,
) -> serde_json::Value {
    let mut branch = None;
    let mut upstream_status = "unobserved";
    let mut upstream_reason_code = observation.reason_code.or(Some("git_status_unobserved"));
    let mut upstream = None;
    let mut ahead = None;
    let mut behind = None;
    let mut files = Vec::new();
    let mut modified = 0usize;
    let mut added = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    let mut copied = 0usize;
    let mut untracked = 0usize;
    let mut conflicted = 0usize;
    let mut staged_count = 0usize;
    let mut unstaged_count = 0usize;
    let status_observed = observation.status_observed();
    let observation_reason_code = observation.reason_code;

    if status_observed {
        for line in status_stdout.lines() {
            if let Some(parsed) = parse_status_header(line) {
                branch = parsed.branch;
                upstream_status = parsed.upstream_status;
                upstream_reason_code = parsed.upstream_reason_code;
                upstream = parsed.upstream;
                ahead = parsed.ahead;
                behind = parsed.behind;
                continue;
            }
            if line.len() < 3 {
                continue;
            }
            let mut chars = line.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            if x == '!' && y == '!' {
                continue;
            }
            let path_part = line.get(3..).unwrap_or_default();
            let (path, old_path) = porcelain_path(path_part);
            if path.is_empty() {
                continue;
            }
            let status = status_label(x, y);
            let is_untracked = status == "untracked";
            let is_conflicted = status == "conflicted";
            let staged = !is_untracked && !is_conflicted && x != ' ' && x != '?';
            let unstaged = !is_untracked && !is_conflicted && y != ' ' && y != '?';
            match status {
                "modified" => modified += 1,
                "added" => added += 1,
                "deleted" => deleted += 1,
                "renamed" => renamed += 1,
                "copied" => copied += 1,
                "untracked" => untracked += 1,
                "conflicted" => conflicted += 1,
                _ => {}
            }
            if staged {
                staged_count += 1;
            }
            if unstaged {
                unstaged_count += 1;
            }
            let mut file = serde_json::Map::new();
            file.insert("path".to_string(), json!(path));
            file.insert("status".to_string(), json!(status));
            file.insert("staged".to_string(), json!(staged));
            file.insert("unstaged".to_string(), json!(unstaged));
            file.insert(
                "kind".to_string(),
                json!(if is_untracked {
                    "untracked"
                } else if is_conflicted {
                    "conflicted"
                } else {
                    "tracked"
                }),
            );
            if let Some(old_path) = old_path {
                file.insert("old_path".to_string(), json!(old_path));
            }
            files.push(json!(file));
        }
    }

    // When the production-side status loop truncated the returned file
    // records, the per-category counts parsed above only cover the returned
    // subset. The command's streaming loop counted and classified *every*
    // entry, so prefer those authoritative counts whenever they are present.
    // `files_truncated` is true when the production-side limit dropped records
    // by either the count cap *or* the independent status byte budget; the
    // exact totals stay authoritative either way.
    let status_trunc_count = frames.status_trunc_count.unwrap_or(false);
    let status_trunc_bytes = frames.status_trunc_bytes.unwrap_or(false);
    let status_trunc_path = frames.status_trunc_path.unwrap_or(false);
    let files_truncated =
        frames.files_truncated.unwrap_or(false) || status_trunc_count || status_trunc_bytes;
    let files_total = frames.files_total;
    let files_returned = frames.files_returned.unwrap_or(files.len());
    let files_limit = frames.files_limit.unwrap_or(SHOW_CHANGES_MAX_STATUS_FILES);
    if files_truncated {
        if let Some(value) = frames.counts_modified {
            modified = value;
        }
        if let Some(value) = frames.counts_added {
            added = value;
        }
        if let Some(value) = frames.counts_deleted {
            deleted = value;
        }
        if let Some(value) = frames.counts_renamed {
            renamed = value;
        }
        if let Some(value) = frames.counts_copied {
            copied = value;
        }
        if let Some(value) = frames.counts_untracked {
            untracked = value;
        }
        if let Some(value) = frames.counts_conflicted {
            conflicted = value;
        }
        if let Some(value) = frames.counts_staged {
            staged_count = value;
        }
        if let Some(value) = frames.counts_unstaged {
            unstaged_count = value;
        }
    }

    let clean = status_observed.then_some(files_total.map_or(files.is_empty(), |total| total == 0));
    let mut warnings = Vec::new();
    if !status_observed {
        let reason_code = observation_reason_code.unwrap_or("git_status_result_unavailable");
        let message = match observation.kind {
            ShowChangesStatusObservationKind::CommandFailed => {
                "git status command failed; worktree state is unavailable"
            }
            ShowChangesStatusObservationKind::OutputUnavailable => {
                "git status did not produce a reliable porcelain branch header"
            }
            ShowChangesStatusObservationKind::NonGit => "project is not a git repository",
            ShowChangesStatusObservationKind::Observed => "git status was observed",
        };
        warnings.push(json!({
            "kind": reason_code,
            "reason_code": reason_code,
            "message": message,
        }));
    }
    if upstream_status == "gone" {
        warnings.push(json!({
            "kind": "upstream_gone",
            "reason_code": "upstream_gone",
            "message": "configured upstream tracking branch is gone",
        }));
    }
    if conflicted > 0 {
        warnings.push(json!({
            "kind": "workspace_conflicts",
            "conflicted": conflicted,
            "message": "workspace has unresolved merge/rebase conflicts; inspect carefully and preserve conflict markers until intentionally resolved",
        }));
    }
    for file in &files {
        if file["kind"] != "untracked" {
            continue;
        }
        let path = file["path"].as_str().unwrap_or_default();
        if looks_like_smoke_path(path) {
            warnings.push(json!({
                "kind": "untracked_smoke_file",
                "path": path,
                "message": "untracked smoke/tmp/test/anchor file should be reviewed before commit",
            }));
        } else {
            warnings.push(json!({
                "kind": "untracked_file",
                "path": path,
                "message": "untracked file should be reviewed before commit",
            }));
        }
    }

    let suggested_next_actions = if status_observed {
        suggested_next_actions_for(
            clean.unwrap_or(false),
            untracked > 0,
            has_smoke_warning(&warnings),
            None,
        )
    } else {
        vec!["inspect git status failure before relying on worktree cleanliness".to_string()]
    };

    // Parse the (already production-bounded) diff hunks with the Rust-side
    // parser. The parser's `truncated` flag captures a line-level bound on the
    // final returned hunk, distinct from the production-side hunk-count bound,
    // so both reasons can be surfaced.
    let (diff_hunks, parser_hunk_count, parser_truncated) = match diff_stdout {
        Some(diff) => parse_git_diff_hunks(diff, max_hunks, max_hunk_lines),
        None => (Vec::new(), 0, false),
    };

    // Collect stable truncation reason codes from the per-segment metadata
    // frames the production-side command emits. Each reason corresponds to a
    // concrete production-side bound that fired, never a transport tail marker.
    let mut truncation_reasons: Vec<&'static str> = Vec::new();
    if status_trunc_count {
        truncation_reasons.push("status_file_count_limit");
    }
    if status_trunc_bytes {
        truncation_reasons.push("status_byte_budget");
    }
    if status_trunc_path {
        truncation_reasons.push("status_path_overlong");
    }
    if frames.head_truncated == Some(true) {
        truncation_reasons.push("head_metadata_byte_budget");
    }
    if frames.diff_stat_truncated == Some(true) {
        truncation_reasons.push("diff_stat_byte_budget");
    }
    if frames.diff_trunc_hunk_count == Some(true) {
        truncation_reasons.push("diff_hunk_count_limit");
    }
    if frames.diff_trunc_hunk_lines == Some(true) || parser_truncated {
        truncation_reasons.push("diff_hunk_line_limit");
    }
    if frames.diff_trunc_bytes == Some(true) {
        truncation_reasons.push("diff_byte_budget");
    }
    if frames.diff_trunc_bytes_in_hunk == Some(true) {
        truncation_reasons.push("diff_hunk_byte_budget");
    }
    let output_truncated = !truncation_reasons.is_empty();
    // Every segment must fit its independent production budget and match its
    // exact byte metadata. Missing, malformed, or internally inconsistent
    // metadata cannot prove transport safety.
    let transport_safe = show_changes_transport_safe(frames, diff_stdout.is_some(), max_hunks);

    let mut output = json!({
        "project": project,
        "git_available": observation.git_available,
        "non_git_project": observation.non_git(),
        "git_error": observation_reason_code.map(|reason| match reason {
            "git_status_config_error" => "git status configuration is invalid",
            "git_status_permission_denied" => "git status permission denied",
            "git_status_command_failed" => "git status command failed",
            "git_status_header_unavailable" => "git status output unavailable",
            "git_status_result_unavailable" => "git status execution result unavailable",
            "not_a_git_repository" => "not a git repository",
            _ => "git status unavailable",
        }),
        "branch": branch,
        "upstream_status": upstream_status,
        "upstream_reason_code": upstream_reason_code,
        "upstream": upstream,
        "ahead": ahead,
        "behind": behind,
        "head": parse_show_changes_head(head_stdout),
        "status_observation": observation.as_json(),
        "clean": clean,
        "counts": {
            "modified": modified,
            "added": added,
            "deleted": deleted,
            "renamed": renamed,
            "copied": copied,
            "untracked": untracked,
            "conflicted": status_observed.then_some(conflicted),
            "staged": staged_count,
            "unstaged": unstaged_count,
        },
        "files": files,
        "files_total": files_total,
        "files_returned": files_returned,
        "files_truncated": files_truncated,
        "files_limit": files_limit,
        "transport_safe": transport_safe,
        "output_budget_bytes": SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "output_truncated": output_truncated,
        "truncation_reasons": truncation_reasons,
        "diff_stat": diff_stat,
        "diff_exit": frames.diff_exit,
        "diff_status": diff_status_json(frames.diff_exit),
        "diff_stat_exit": frames.diff_stat_exit,
        "diff_stat_status": diff_stat_status_json(frames.diff_stat_exit),
        "head_exit": frames.head_exit,
        "warnings": warnings,
        "suggested_next_actions": suggested_next_actions,
        "session": null,
        "exit_code": exit_code,
        "stderr": stderr,
    });

    if diff_stdout.is_some() {
        // The production-side diff loop already bounded the raw diff to at
        // most `max_hunks` hunks and `max_hunk_lines` lines per hunk before
        // transport. Its reported counts are authoritative for dropped hunks;
        // the parser's `truncated` still captures the final returned hunk being
        // line-bounded. Combine both.
        let hunk_count = frames.diff_hunks_returned.unwrap_or(parser_hunk_count);
        let hunks_truncated =
            frames.diff_hunks_truncated.unwrap_or(parser_truncated) || parser_truncated;
        output["hunks"] = json!(diff_hunks);
        output["hunk_count"] = json!(hunk_count);
        output["hunks_truncated"] = json!(hunks_truncated);
    }

    set_show_changes_verdict(&mut output);
    output
}

fn skipped_untracked_preview(path: &str, reason: &str, byte_count: Option<u64>) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("path".to_string(), json!(path));
    object.insert("kind".to_string(), json!("skipped"));
    object.insert("reason".to_string(), json!(reason));
    if let Some(byte_count) = byte_count {
        object.insert("byte_count".to_string(), json!(byte_count));
    }
    Value::Object(object)
}

fn untracked_preview_path_is_invalid(path: &str) -> bool {
    let trimmed = path.trim();
    trimmed.is_empty()
        || trimmed == "."
        || validate_project_relative_path(trimmed).is_err()
        || trimmed.split('/').any(|part| part.is_empty())
}

fn untracked_preview_path_is_sensitive(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .any(|part| {
            matches!(
                part,
                ".git"
                    | "target"
                    | "node_modules"
                    | "project-registry"
                    | "projects.d"
                    | "runner.toml"
                    | "agent.toml"
                    | "webcodex.env"
                    | ".env"
                    | "secrets"
                    | "tokens"
                    | "id_rsa"
                    | "id_ed25519"
            ) || part.starts_with(".env")
                || part.starts_with("runner.toml")
                || part.starts_with("agent.toml")
                || part.starts_with("webcodex.env")
                || part.ends_with(".pem")
                || part.ends_with(".key")
        })
}

fn untracked_preview_from_bytes(
    path: &str,
    data: &[u8],
    declared_byte_count: Option<u64>,
) -> Value {
    let byte_count = declared_byte_count.unwrap_or(data.len() as u64);
    if data.len() as u64 > SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_BYTES {
        return skipped_untracked_preview(
            path,
            "too_large",
            Some(byte_count.max(data.len() as u64)),
        );
    }
    if data
        .iter()
        .any(|byte| *byte == 0 || (*byte < 32 && !matches!(*byte, b'\t' | b'\n' | b'\r')))
    {
        return skipped_untracked_preview(path, "binary_or_non_utf8", Some(data.len() as u64));
    }
    let text = match std::str::from_utf8(data) {
        Ok(text) => text,
        Err(_) => {
            return skipped_untracked_preview(path, "binary_or_non_utf8", Some(data.len() as u64))
        }
    };
    let all_lines: Vec<&str> = text.lines().collect();
    let shown_lines: Vec<Value> = all_lines
        .iter()
        .take(SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_LINES)
        .enumerate()
        .map(|(index, line)| {
            json!({
                "line": index + 1,
                "text": line,
            })
        })
        .collect();
    json!({
        "path": path,
        "kind": "text",
        "line_count": all_lines.len(),
        "byte_count": byte_count,
        "truncated": all_lines.len() > SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_LINES,
        "lines": shown_lines,
    })
}

#[cfg(test)]
pub(crate) fn collect_show_changes_untracked_previews_for_root(
    root: &Path,
    untracked_paths: &[String],
) -> (Vec<Value>, bool) {
    let truncated = untracked_paths.len() > SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_FILES;
    let canonical_root = match root.canonicalize() {
        Ok(root) => root,
        Err(_) => return (Vec::new(), truncated),
    };
    let mut previews = Vec::new();
    for path in untracked_paths
        .iter()
        .take(SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_FILES)
    {
        if untracked_preview_path_is_invalid(path) || untracked_preview_path_is_sensitive(path) {
            previews.push(skipped_untracked_preview(
                path,
                "sensitive_or_excluded_path",
                None,
            ));
            continue;
        }
        let full_path = root.join(path);
        let metadata = match std::fs::symlink_metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                previews.push(skipped_untracked_preview(path, "not_found", None));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            previews.push(skipped_untracked_preview(
                path,
                "sensitive_or_excluded_path",
                None,
            ));
            continue;
        }
        if !metadata.is_file() {
            previews.push(skipped_untracked_preview(path, "not_regular_file", None));
            continue;
        }
        let canonical = match full_path.canonicalize() {
            Ok(canonical) => canonical,
            Err(_) => {
                previews.push(skipped_untracked_preview(
                    path,
                    "sensitive_or_excluded_path",
                    None,
                ));
                continue;
            }
        };
        if !canonical.starts_with(&canonical_root) {
            previews.push(skipped_untracked_preview(
                path,
                "sensitive_or_excluded_path",
                None,
            ));
            continue;
        }
        let byte_count = metadata.len();
        if byte_count > SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_BYTES {
            previews.push(skipped_untracked_preview(
                path,
                "too_large",
                Some(byte_count),
            ));
            continue;
        }
        match std::fs::read(&full_path) {
            Ok(data) => previews.push(untracked_preview_from_bytes(path, &data, Some(byte_count))),
            Err(_) => previews.push(skipped_untracked_preview(path, "read_error", None)),
        }
    }
    (previews, truncated)
}

pub(crate) fn show_changes_untracked_paths(output: &Value) -> Vec<String> {
    output["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|file| file["kind"] == "untracked")
        .filter_map(|file| file["path"].as_str().map(str::to_string))
        .collect()
}

fn show_changes_untracked_preview_probe_command(path: &str) -> String {
    format!(
        "p={path}; \
         if [ -L \"$p\" ]; then printf 'SKIP\\tsensitive_or_excluded_path\\n'; \
         elif [ ! -e \"$p\" ]; then printf 'SKIP\\tnot_found\\n'; \
         elif [ ! -f \"$p\" ]; then printf 'SKIP\\tnot_regular_file\\n'; \
         else bytes=$(wc -c < \"$p\" 2>/dev/null | tr -d '[:space:]'); \
           case \"$bytes\" in \
             ''|*[!0-9]*) printf 'SKIP\\tread_error\\n' ;; \
             *) if [ \"$bytes\" -gt {max_bytes} ]; then printf 'SKIP\\ttoo_large\\t%s\\n' \"$bytes\"; \
                else printf 'DATA\\t%s\\n' \"$bytes\"; base64 < \"$p\" 2>/dev/null; fi ;; \
           esac; \
         fi",
        path = shell_escape_simple(path),
        max_bytes = SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_BYTES,
    )
}

fn parse_show_changes_agent_preview_probe(
    path: &str,
    stdout: &str,
    exit_code: Option<i32>,
) -> Value {
    let mut lines = stdout.lines();
    let Some(header) = lines.next() else {
        return skipped_untracked_preview(path, "read_error", None);
    };
    let parts: Vec<&str> = header.split('\t').collect();
    match parts.as_slice() {
        ["SKIP", reason] => skipped_untracked_preview(path, reason, None),
        ["SKIP", reason, byte_count] => {
            skipped_untracked_preview(path, reason, byte_count.parse::<u64>().ok())
        }
        ["DATA", byte_count] => {
            if exit_code != Some(0) {
                return skipped_untracked_preview(
                    path,
                    "read_error",
                    byte_count.parse::<u64>().ok(),
                );
            }
            let encoded = lines.collect::<Vec<_>>().join("");
            let data = match general_purpose::STANDARD.decode(encoded.as_bytes()) {
                Ok(data) => data,
                Err(_) => return skipped_untracked_preview(path, "read_error", None),
            };
            untracked_preview_from_bytes(path, &data, byte_count.parse::<u64>().ok())
        }
        _ => skipped_untracked_preview(path, "read_error", None),
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SessionActionSignals {
    failed: bool,
    write_like: bool,
    shell_like: bool,
}

fn has_smoke_warning(warnings: &[Value]) -> bool {
    warnings
        .iter()
        .any(|warning| warning["kind"] == "untracked_smoke_file")
}

fn suggested_next_actions_for(
    clean: bool,
    has_untracked: bool,
    has_smoke_warning: bool,
    session: Option<SessionActionSignals>,
) -> Vec<String> {
    let mut actions = Vec::new();
    let session = session.unwrap_or_default();
    if clean && !session.failed {
        push_unique_action(&mut actions, "no changes detected");
    }
    if !clean {
        push_unique_action(&mut actions, "review diff");
        push_unique_action(&mut actions, "run focused tests");
        if has_untracked {
            push_unique_action(&mut actions, "review untracked files before commit");
        }
        if has_smoke_warning {
            push_unique_action(
                &mut actions,
                "clean untracked smoke/tmp files or intentionally commit them",
            );
        }
        push_unique_action(&mut actions, "commit or revert changes after review");
    }
    if session.failed {
        push_unique_action(&mut actions, "review failed tool calls in session_summary");
    }
    if session.write_like {
        push_unique_action(&mut actions, "review changed paths from this session");
    }
    if session.shell_like {
        push_unique_action(&mut actions, "check command/test results before commit");
    }
    actions
}

fn push_unique_action(actions: &mut Vec<String>, action: &str) {
    if !actions.iter().any(|existing| existing == action) {
        actions.push(action.to_string());
    }
}

pub(crate) fn apply_show_changes_session(
    output: &mut Value,
    session_id: Option<&str>,
    summary: Option<SessionSummary>,
) {
    let Some(session_id) = session_id else {
        output["session"] = Value::Null;
        set_show_changes_verdict(output);
        return;
    };
    let session_signals = match summary {
        Some(summary) => {
            let changed_paths = session_changed_paths(&summary.events);
            let recent_events: Vec<Value> = summary
                .events
                .iter()
                .map(show_changes_session_event)
                .collect();
            let signals = SessionActionSignals {
                failed: summary.counts.failed > 0,
                write_like: summary.counts.write_like > 0,
                shell_like: summary.counts.shell_like > 0,
            };
            output["session"] = json!({
                "found": true,
                "session_id": summary.session_id,
                "project": summary.project,
                "title": summary.title,
                "created_at": summary.created_at,
                "updated_at": summary.updated_at,
                "counts": summary.counts,
                "changed_paths": changed_paths,
                "recent_events": recent_events,
            });
            Some(signals)
        }
        None => {
            output["session"] = json!({
                "found": false,
                "session_id": session_id,
                "message": "session not found",
            });
            if let Some(warnings) = output["warnings"].as_array_mut() {
                warnings.push(json!({
                    "kind": "session_not_found",
                    "session_id": session_id,
                    "message": "session not found",
                }));
            }
            None
        }
    };
    refresh_show_changes_suggestions(output, session_signals);
}

fn refresh_show_changes_suggestions(output: &mut Value, session: Option<SessionActionSignals>) {
    // Any unobserved status keeps an unavailable message as the primary
    // suggestion. Clean/dirty review actions only apply to a reliable
    // porcelain branch header.
    let status_observation = output
        .pointer("/status_observation/status")
        .and_then(Value::as_str)
        .unwrap_or("output_unavailable");
    if status_observation != "observed" {
        let session = session.unwrap_or_default();
        let mut actions = vec![if status_observation == "non_git" {
            "git-backed status/diff unavailable; project is not a git repository".to_string()
        } else {
            "git status unavailable; inspect the status failure before relying on worktree cleanliness".to_string()
        }];
        if session.failed {
            push_unique_action(&mut actions, "review failed tool calls in session_summary");
        }
        if session.write_like {
            push_unique_action(&mut actions, "review changed paths from this session");
        }
        if session.shell_like {
            push_unique_action(&mut actions, "check command/test results before commit");
        }
        output["suggested_next_actions"] = json!(actions);
        set_show_changes_verdict(output);
        return;
    }
    let clean = output["clean"].as_bool().unwrap_or(false);
    let has_untracked = output["counts"]["untracked"].as_u64().unwrap_or(0) > 0;
    let has_smoke_warning = output["warnings"]
        .as_array()
        .is_some_and(|warnings| has_smoke_warning(warnings));
    output["suggested_next_actions"] = json!(suggested_next_actions_for(
        clean,
        has_untracked,
        has_smoke_warning,
        session,
    ));
    set_show_changes_verdict(output);
}

fn set_show_changes_verdict(output: &mut Value) {
    let mut blocking_reasons: Vec<&'static str> = Vec::new();
    let mut warning_reasons: Vec<&'static str> = Vec::new();
    let mut actions = string_array(output.get("suggested_next_actions"));

    let git_available = output
        .get("git_available")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let non_git_project = output
        .get("non_git_project")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status_observation = output
        .pointer("/status_observation/status")
        .and_then(Value::as_str)
        .unwrap_or("output_unavailable");
    let status_reason_code = output
        .pointer("/status_observation/reason_code")
        .and_then(Value::as_str);
    match status_observation {
        "command_failed" => {
            push_unique_reason(
                &mut warning_reasons,
                match status_reason_code {
                    Some("git_status_config_error") => "git_status_config_error",
                    Some("git_status_permission_denied") => "git_status_permission_denied",
                    _ => "git_status_command_failed",
                },
            );
        }
        "output_unavailable" => {
            push_unique_reason(&mut warning_reasons, "git_status_output_unavailable");
        }
        _ => {}
    }
    if !git_available || non_git_project {
        push_unique_reason(&mut warning_reasons, "git_unavailable");
        push_unique_action(
            &mut actions,
            "git-backed status/diff unavailable; continue with non-git review evidence",
        );
    }

    if output
        .get("clean")
        .and_then(Value::as_bool)
        .is_some_and(|clean| !clean)
    {
        push_unique_reason(&mut warning_reasons, "workspace_dirty");
        push_unique_action(&mut actions, "review workspace changes with show_changes");
    }
    if output
        .pointer("/counts/conflicted")
        .and_then(Value::as_u64)
        .is_some_and(|count| count > 0)
    {
        push_unique_reason(&mut blocking_reasons, "workspace_conflicts");
        push_unique_action(&mut actions, "resolve workspace conflicts before closeout");
    }

    if status_observation == "command_failed"
        || (git_available
            && output
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|exit_code| exit_code != 0))
    {
        push_unique_reason(&mut blocking_reasons, "git_inspection_failed");
        push_unique_action(
            &mut actions,
            "rerun show_changes or inspect git status directly",
        );
    }

    if output
        .get("warnings")
        .and_then(Value::as_array)
        .is_some_and(|warnings| !warnings.is_empty())
    {
        push_unique_reason(&mut warning_reasons, "review_warnings_present");
    }

    let hunks_truncated = output
        .get("hunks_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if hunks_truncated {
        actions.retain(|action| action != "review workspace changes with show_changes");
        let diff_truncation_reasons = output
            .get("truncation_reasons")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|reason| match reason.as_str() {
                Some("diff_hunk_count_limit") => Some("diff_hunk_count_limit"),
                Some("diff_hunk_line_limit") => Some("diff_hunk_line_limit"),
                Some("diff_byte_budget") => Some("diff_byte_budget"),
                Some("diff_hunk_byte_budget") => Some("diff_hunk_byte_budget"),
                _ => None,
            })
            .collect::<Vec<_>>();
        let page_truncated = diff_truncation_reasons
            .iter()
            .any(|reason| matches!(*reason, "diff_hunk_count_limit" | "diff_byte_budget"));
        let hunk_line_truncated = diff_truncation_reasons
            .iter()
            .any(|reason| *reason == "diff_hunk_line_limit");
        let current_hunk_omitted = hunk_line_truncated
            || diff_truncation_reasons
                .iter()
                .any(|reason| *reason == "diff_hunk_byte_budget");
        // show_changes currently reports line truncation at the aggregate diff
        // level, not as authoritative per-hunk provenance. Keep whole-worktree
        // scope rather than guessing which returned path owns the omitted lines.
        let suggested_paths: Vec<String> = Vec::new();
        let suggested_max_hunk_lines = if hunk_line_truncated {
            MAX_MAX_HUNK_LINES
        } else {
            DEFAULT_MAX_HUNK_LINES
        };
        let project = output
            .get("project")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let recovery_kind = match (page_truncated, current_hunk_omitted) {
            (true, true) => "mixed",
            (true, false) => "page",
            (false, true) => "hunk_lines",
            (false, false) => unreachable!("truncated show_changes diff needs a recovery kind"),
        };
        let suggested_call = json!({
            "project": project,
            "cached": false,
            "paths": suggested_paths,
            "max_hunks": DEFAULT_MAX_HUNKS,
            "max_hunk_lines": suggested_max_hunk_lines,
        });
        output["diff_review_handoff"] = json!({
            "tool": "git_diff_hunks",
            "scope": "worktree",
            "reason": "show_changes_diff_truncated",
            "truncation_reasons": diff_truncation_reasons,
            "recovery": {
                "kind": recovery_kind,
                "tool": "git_diff_hunks",
                "arguments": suggested_call.clone(),
                "safe_continuation_for_omitted_lines": if current_hunk_omitted {
                    Value::Bool(false)
                } else {
                    Value::Null
                },
            },
            "suggested_call": suggested_call,
        });
        push_unique_reason(&mut warning_reasons, "truncated_by_limit");
        push_unique_action(
            &mut actions,
            "continue the diff review with git_diff_hunks; use paths to narrow scope when useful",
        );
        if page_truncated {
            push_unique_action(
                &mut actions,
                "follow git_diff_hunks.next_continuation while has_more=true",
            );
        }
        if hunk_line_truncated {
            push_unique_action(
                &mut actions,
                "increase git_diff_hunks.max_hunk_lines and/or narrow paths; continuation alone does not recover omitted lines from the same hunk",
            );
        }
    } else if let Some(object) = output.as_object_mut() {
        object.remove("diff_review_handoff");
    }

    let untracked_previews_truncated = output
        .get("untracked_previews_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if untracked_previews_truncated {
        actions.retain(|action| action != "review workspace changes with show_changes");
        push_unique_reason(&mut warning_reasons, "truncated_by_limit");
        push_unique_action(
            &mut actions,
            "inspect the relevant untracked files separately",
        );
    }

    if actions.is_empty() {
        actions.push("no action needed".to_string());
    }
    if hunks_truncated || untracked_previews_truncated {
        output["suggested_next_actions"] = json!(actions.clone());
    }
    let status = if blocking_reasons.is_empty() {
        if warning_reasons.is_empty() {
            "pass"
        } else {
            "warn"
        }
    } else {
        "fail"
    };

    output["verdict"] = json!({
        "status": status,
        "blocking": !blocking_reasons.is_empty(),
        "blocking_reasons": blocking_reasons,
        "warning_reasons": warning_reasons,
        "suggested_next_actions": actions,
    });
}

fn git_diff_file_hunk_count(files: &[Value]) -> Option<usize> {
    files.iter().try_fold(0usize, |count, file| {
        file.get("hunks")
            .and_then(Value::as_array)
            .map(|hunks| count + hunks.len())
    })
}

fn git_diff_files_have_omitted_lines(files: &[Value]) -> bool {
    files.iter().any(|file| {
        file.get("hunks")
            .and_then(Value::as_array)
            .is_some_and(|hunks| {
                hunks
                    .iter()
                    .any(|hunk| hunk.get("truncated").and_then(Value::as_bool) == Some(true))
            })
    })
}

fn sparsify_complete_diff_files(files: &mut [Value]) {
    for file in files {
        let Some(file) = file.as_object_mut() else {
            continue;
        };
        let duplicate_old_path = match (
            file.get("path").and_then(Value::as_str),
            file.get("old_path").and_then(Value::as_str),
        ) {
            (Some(path), Some(old_path)) => path == old_path,
            _ => false,
        };
        if duplicate_old_path {
            file.remove("old_path");
        }
        let Some(hunks) = file.get_mut("hunks").and_then(Value::as_array_mut) else {
            continue;
        };
        for hunk in hunks {
            let Some(hunk) = hunk.as_object_mut() else {
                continue;
            };
            if hunk.get("truncated").and_then(Value::as_bool) == Some(false) {
                hunk.remove("truncated");
            }
            let line_count_is_derived = match (
                hunk.get("line_count").and_then(Value::as_u64),
                hunk.get("diff").and_then(Value::as_str),
            ) {
                (Some(line_count), Some(diff)) => line_count == diff.lines().count() as u64,
                _ => false,
            };
            if line_count_is_derived {
                hunk.remove("line_count");
            }
        }
    }
}

fn sparsify_complete_git_diff_hunks_output(output: &mut serde_json::Map<String, Value>) {
    let Some(files) = output.get("files").and_then(Value::as_array) else {
        return;
    };
    let Some(computed_hunks) = git_diff_file_hunk_count(files) else {
        return;
    };
    let ordinary_complete = output.get("hunk_count").and_then(Value::as_u64)
        == Some(computed_hunks as u64)
        && output.get("truncated").and_then(Value::as_bool) == Some(false)
        && output
            .get("truncation_reasons")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        && output.get("has_more").and_then(Value::as_bool) == Some(false)
        && output.get("next_continuation").is_some_and(Value::is_null)
        && output.get("recovery").is_none()
        && output.get("exit_code").and_then(Value::as_i64) == Some(0)
        && output.get("stderr").and_then(Value::as_str) == Some("")
        && !git_diff_files_have_omitted_lines(files);
    if !ordinary_complete {
        return;
    }

    if let Some(files) = output.get_mut("files").and_then(Value::as_array_mut) {
        sparsify_complete_diff_files(files);
    }
    for key in [
        "hunk_count",
        "truncated",
        "truncation_reasons",
        "has_more",
        "next_continuation",
        "exit_code",
        "stderr",
    ] {
        output.remove(key);
    }
}

fn sparsify_complete_show_changes_output(output: &mut serde_json::Map<String, Value>) {
    let Some(files_len) = output.get("files").and_then(Value::as_array).map(Vec::len) else {
        return;
    };
    let status_observed = output.get("status_observation").is_some_and(|status| {
        status.get("status").and_then(Value::as_str) == Some("observed")
            && status.get("repository_probe").and_then(Value::as_str) == Some("inside_worktree")
    });
    let diff_stat_observed = output.get("diff_stat_status").is_some_and(|status| {
        status.get("status").and_then(Value::as_str) == Some("observed")
            && status.get("exit_code").and_then(Value::as_i64) == Some(0)
            && status.get("reason_code").is_some_and(Value::is_null)
    });
    let has_hunks = output.get("hunks").and_then(Value::as_array).is_some();
    let diff_observed = if has_hunks {
        output.get("diff_exit").and_then(Value::as_i64) == Some(0)
            && output.get("diff_status").is_some_and(|status| {
                status.get("status").and_then(Value::as_str) == Some("observed")
                    && status.get("exit_code").and_then(Value::as_i64) == Some(0)
            })
    } else {
        output.get("diff_exit").is_some_and(Value::is_null)
    };
    let hunk_metadata_complete = match output.get("hunks").and_then(Value::as_array) {
        Some(hunks) => {
            git_diff_file_hunk_count(hunks).is_some_and(|count| {
                output.get("hunk_count").and_then(Value::as_u64) == Some(count as u64)
            }) && output.get("hunks_truncated").and_then(Value::as_bool) == Some(false)
                && !git_diff_files_have_omitted_lines(hunks)
        }
        None => output.get("hunk_count").is_none() && output.get("hunks_truncated").is_none(),
    };
    let ordinary_complete = output.get("git_available").and_then(Value::as_bool) == Some(true)
        && output.get("non_git_project").and_then(Value::as_bool) == Some(false)
        && output.get("git_error").is_some_and(Value::is_null)
        && status_observed
        && output.get("files_total").and_then(Value::as_u64) == Some(files_len as u64)
        && output.get("files_returned").and_then(Value::as_u64) == Some(files_len as u64)
        && output.get("files_truncated").and_then(Value::as_bool) == Some(false)
        && output.get("transport_safe").and_then(Value::as_bool) == Some(true)
        && output.get("output_truncated").and_then(Value::as_bool) == Some(false)
        && output
            .get("truncation_reasons")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        && output.get("diff_review_handoff").is_none()
        && output
            .get("untracked_previews_truncated")
            .is_none_or(|value| value.as_bool() == Some(false))
        && output
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        && output.get("exit_code").and_then(Value::as_i64) == Some(0)
        && output.get("stderr").and_then(Value::as_str) == Some("")
        && output.get("head_exit").and_then(Value::as_i64) == Some(0)
        && diff_stat_observed
        && diff_observed
        && hunk_metadata_complete;
    if !ordinary_complete {
        return;
    }

    if let Some(hunks) = output.get_mut("hunks").and_then(Value::as_array_mut) {
        sparsify_complete_diff_files(hunks);
    }
    for key in [
        "git_available",
        "non_git_project",
        "git_error",
        "status_observation",
        "files_total",
        "files_returned",
        "files_truncated",
        "files_limit",
        "transport_safe",
        "output_budget_bytes",
        "output_truncated",
        "truncation_reasons",
        "diff_exit",
        "diff_status",
        "diff_stat_exit",
        "diff_stat_status",
        "head_exit",
        "hunk_count",
        "hunks_truncated",
        "untracked_previews_truncated",
        "warnings",
        "exit_code",
        "stderr",
    ] {
        output.remove(key);
    }
}

pub(crate) fn sparsify_complete_git_review_success(tool_name: &str, result: &mut ToolResult) {
    if !result.success {
        return;
    }
    let Some(output) = result.output.as_object_mut() else {
        return;
    };
    match tool_name {
        "git_diff_hunks" => sparsify_complete_git_diff_hunks_output(output),
        "show_changes" => sparsify_complete_show_changes_output(output),
        _ => {}
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn push_unique_reason(reasons: &mut Vec<&'static str>, reason: &'static str) {
    if !reasons.iter().any(|existing| existing == &reason) {
        reasons.push(reason);
    }
}

fn session_changed_paths(events: &[SessionEvent]) -> Vec<String> {
    let mut paths = Vec::new();
    for event in events {
        for path in &event.changed_paths {
            let path = path.trim();
            if !path.is_empty() && !paths.iter().any(|existing| existing == path) {
                paths.push(path.to_string());
            }
        }
    }
    paths
}

fn show_changes_session_event(event: &SessionEvent) -> Value {
    json!({
        "event_id": event.event_id,
        "kind": event.kind,
        "timestamp": event.timestamp,
        "transport": event.transport,
        "tool_name": event.tool_name,
        "project": event.project,
        "resolved_project": event.resolved_project,
        "risk_class": event.risk_class,
        "read_like": event.read_like,
        "write_like": event.write_like,
        "shell_like": event.shell_like,
        "git_like": event.git_like,
        "change_summary_like": event.change_summary_like,
        "started_at": event.started_at,
        "finished_at": event.finished_at,
        "status": event.status,
        "exit_code": event.exit_code,
        "failure_kind": event.failure_kind,
        "duration_ms": event.duration_ms,
        "error_kind": event.error_kind,
        "error_message_summary": event.error_message_summary,
        "changed_paths": event.changed_paths,
        "job_id": event.job_id,
    })
}

/// Split the combined `git_diff_summary` stdout into the porcelain section and
/// the `diff --stat` section. If the sentinel is absent, everything is treated
/// as porcelain (defensive; should not happen in practice).
pub(crate) fn split_diff_summary(stdout: &str) -> (String, String) {
    if let Some((before, after)) = stdout.split_once(DIFF_SUMMARY_SENTINEL) {
        (
            before.trim_end_matches(['\n', '\r']).to_string(),
            after
                .trim_start_matches(['\n', '\r'])
                .trim_end()
                .to_string(),
        )
    } else {
        (stdout.trim_end().to_string(), String::new())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct GitDiffHunksContinuationV1 {
    v: u8,
    scope: String,
    fence: String,
    next: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitDiffHunksContinuationError {
    Invalid,
    ScopeMismatch,
}

fn is_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_git_object_hex(value: &str) -> bool {
    is_lower_hex(value, 40) || is_lower_hex(value, 64)
}

fn git_diff_hunks_scope_digest(resolved_project: &str, paths: &[String], cached: bool) -> String {
    let mut normalized_paths = paths.to_vec();
    normalized_paths.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.git-diff-hunks.scope.v1\0");
    hasher.update(resolved_project.as_bytes());
    hasher.update([0]);
    if cached {
        hasher.update(b"cached");
    } else {
        hasher.update(b"worktree");
    }
    for path in normalized_paths {
        hasher.update([0]);
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn git_diff_hunks_committed_scope_digest(
    resolved_project: &str,
    paths: &[String],
    scope: &CommittedGitScope,
    max_hunks: usize,
    max_hunk_lines: usize,
) -> String {
    let mut normalized_paths = paths.to_vec();
    normalized_paths.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.git-diff-hunks.scope.committed.v1\0");
    for value in [
        resolved_project,
        scope.requested_base.as_str(),
        scope.requested_head.as_str(),
        scope.merge_base.as_str(),
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hasher.update((max_hunks as u64).to_be_bytes());
    hasher.update((max_hunk_lines as u64).to_be_bytes());
    hasher.update((GIT_DIFF_HUNKS_PAGE_BYTES as u64).to_be_bytes());
    for path in normalized_paths {
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn git_diff_hunks_committed_token_mac(
    key: &[u8; 32],
    scope: &str,
    fence: &str,
    next: u64,
) -> String {
    const BLOCK_BYTES: usize = 64;
    let mut ipad = [0x36u8; BLOCK_BYTES];
    let mut opad = [0x5cu8; BLOCK_BYTES];
    for (index, byte) in key.iter().enumerate() {
        ipad[index] ^= byte;
        opad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(b"webcodex.git-diff-hunks.continuation.committed.v2\0");
    inner.update((scope.len() as u64).to_be_bytes());
    inner.update(scope.as_bytes());
    inner.update((fence.len() as u64).to_be_bytes());
    inner.update(fence.as_bytes());
    inner.update(next.to_be_bytes());
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    format!("{:x}", outer.finalize())
}

fn fixed_time_ascii_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn encode_git_diff_hunks_continuation(
    scope: &str,
    fence: &str,
    next: usize,
) -> Result<String, String> {
    let token = GitDiffHunksContinuationV1 {
        v: GIT_DIFF_HUNKS_CONTINUATION_VERSION,
        scope: scope.to_string(),
        fence: fence.to_string(),
        next: next as u64,
        mode: None,
        mac: None,
    };
    let encoded = general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&token).map_err(|_| "failed to encode git diff continuation")?);
    let value = format!("{GIT_DIFF_HUNKS_CONTINUATION_PREFIX}{encoded}");
    if value.len() > GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES {
        return Err("git diff continuation exceeded its size bound".to_string());
    }
    Ok(value)
}

fn encode_git_diff_hunks_committed_continuation(
    key: &[u8; 32],
    scope: &str,
    fence: &str,
    next: usize,
) -> Result<String, String> {
    let next = next as u64;
    let token = GitDiffHunksContinuationV1 {
        v: GIT_DIFF_HUNKS_COMMITTED_CONTINUATION_VERSION,
        scope: scope.to_string(),
        fence: fence.to_string(),
        next,
        mode: Some("committed".to_string()),
        mac: Some(git_diff_hunks_committed_token_mac(key, scope, fence, next)),
    };
    let encoded = general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&token).map_err(|_| "failed to encode git diff continuation")?);
    let value = format!("{GIT_DIFF_HUNKS_CONTINUATION_PREFIX}{encoded}");
    if value.len() > GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES {
        return Err("git diff continuation exceeded its size bound".to_string());
    }
    Ok(value)
}

fn decode_git_diff_hunks_continuation(
    raw: &str,
    expected_scope: &str,
    committed_mac_key: Option<&[u8; 32]>,
) -> Result<GitDiffHunksContinuationV1, GitDiffHunksContinuationError> {
    if raw.is_empty() || raw.len() > GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES {
        return Err(GitDiffHunksContinuationError::Invalid);
    }
    let encoded = raw
        .strip_prefix(GIT_DIFF_HUNKS_CONTINUATION_PREFIX)
        .ok_or(GitDiffHunksContinuationError::Invalid)?;
    let decoded = general_purpose::URL_SAFE_NO_PAD
        .decode(encoded.as_bytes())
        .map_err(|_| GitDiffHunksContinuationError::Invalid)?;
    if decoded.len() > GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES {
        return Err(GitDiffHunksContinuationError::Invalid);
    }
    let token: GitDiffHunksContinuationV1 =
        serde_json::from_slice(&decoded).map_err(|_| GitDiffHunksContinuationError::Invalid)?;
    if !is_lower_hex(&token.scope, 64)
        || !is_git_object_hex(&token.fence)
        || token.next == 0
        || usize::try_from(token.next).is_err()
    {
        return Err(GitDiffHunksContinuationError::Invalid);
    }
    if let Some(key) = committed_mac_key {
        if token.v != GIT_DIFF_HUNKS_COMMITTED_CONTINUATION_VERSION
            || token.mode.as_deref() != Some("committed")
        {
            return Err(GitDiffHunksContinuationError::ScopeMismatch);
        }
        let mac = token
            .mac
            .as_deref()
            .filter(|value| is_lower_hex(value, 64))
            .ok_or(GitDiffHunksContinuationError::Invalid)?;
        let expected_mac =
            git_diff_hunks_committed_token_mac(key, &token.scope, &token.fence, token.next);
        if !fixed_time_ascii_eq(mac, &expected_mac) {
            return Err(GitDiffHunksContinuationError::Invalid);
        }
    } else if token.v != GIT_DIFF_HUNKS_CONTINUATION_VERSION
        || token.mode.is_some()
        || token.mac.is_some()
    {
        return Err(GitDiffHunksContinuationError::ScopeMismatch);
    }
    if token.scope != expected_scope {
        return Err(GitDiffHunksContinuationError::ScopeMismatch);
    }
    Ok(token)
}

fn bounded_git_diff_hunks_stderr(stderr: &str) -> String {
    if stderr.len() <= GIT_DIFF_HUNKS_STDERR_BYTES {
        return stderr.to_string();
    }
    let mut end = GIT_DIFF_HUNKS_STDERR_BYTES;
    while !stderr.is_char_boundary(end) {
        end -= 1;
    }
    stderr[..end].to_string()
}

fn git_diff_hunks_failure(
    project: &str,
    paths: &[String],
    cached: bool,
    reason_code: &'static str,
    exit_code: Option<i32>,
    stderr: &str,
) -> ToolResult {
    ToolResult::err_with_output(
        format!("git_diff_hunks failed: {reason_code}"),
        json!({
            "project": project,
            "paths": paths,
            "cached": cached,
            "files": [],
            "hunk_count": 0,
            "truncated": false,
            "truncation_reasons": [],
            "has_more": false,
            "next_continuation": null,
            "exit_code": exit_code,
            "stderr": bounded_git_diff_hunks_stderr(stderr),
            "error_kind": "git_diff_hunks_failed",
            "reason_code": reason_code,
            "state_changed": false,
        }),
    )
}

fn committed_git_diff_hunks_scope_value(scope: &CommittedGitScope) -> Value {
    json!({
        "mode": "committed",
        "requested_base": scope.requested_base,
        "requested_head": scope.requested_head,
        "merge_base": scope.merge_base,
        "base_is_ancestor": scope.base_is_ancestor,
        "diff_range": format!("{}..{}", scope.merge_base, scope.requested_head),
    })
}

fn git_diff_hunks_call_arguments(
    project: &str,
    paths: &[String],
    cached: bool,
    committed_scope: Option<&CommittedGitScope>,
    max_hunks: usize,
    max_hunk_lines: usize,
    continuation: Option<&str>,
) -> Value {
    let mut arguments = serde_json::Map::new();
    arguments.insert("project".to_string(), json!(project));
    arguments.insert("paths".to_string(), json!(paths));
    arguments.insert("max_hunks".to_string(), json!(max_hunks));
    arguments.insert("max_hunk_lines".to_string(), json!(max_hunk_lines));
    if let Some(scope) = committed_scope {
        arguments.insert("base_commit".to_string(), json!(scope.requested_base));
        arguments.insert("head_commit".to_string(), json!(scope.requested_head));
    } else {
        arguments.insert("cached".to_string(), json!(cached));
    }
    if let Some(continuation) = continuation {
        arguments.insert("continuation".to_string(), json!(continuation));
    }
    Value::Object(arguments)
}

fn git_diff_hunks_exact_truncated_paths(files: &[Value]) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    let mut saw_truncated_hunk = false;
    for file in files {
        let has_omitted_lines = file
            .get("hunks")
            .and_then(Value::as_array)
            .is_some_and(|hunks| {
                hunks
                    .iter()
                    .any(|hunk| hunk.get("truncated").and_then(Value::as_bool) == Some(true))
            });
        if !has_omitted_lines {
            continue;
        }
        saw_truncated_hunk = true;
        let Some(path) = file
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .or_else(|| {
                file.get("old_path")
                    .and_then(Value::as_str)
                    .filter(|path| !path.is_empty())
            })
        else {
            return None;
        };
        if !paths.iter().any(|existing| existing == path) {
            paths.push(path.to_string());
        }
    }
    saw_truncated_hunk.then_some(paths)
}

fn git_diff_hunks_recovery_value(
    project: &str,
    paths: &[String],
    cached: bool,
    committed_scope: Option<&CommittedGitScope>,
    max_hunks: usize,
    max_hunk_lines: usize,
    files: &[Value],
    page_hunk_limit: bool,
    hunk_line_limit: bool,
    page_byte_budget: bool,
    truncated_hunks: &[usize],
    line_ceiling_hunks: &[usize],
    line_recoverable_hunks: &[usize],
    next_continuation: Option<&str>,
) -> Option<Value> {
    let page_truncated = page_hunk_limit || page_byte_budget;
    let omitted_lines_present = hunk_line_limit || git_diff_files_have_omitted_lines(files);
    if !page_truncated && !omitted_lines_present {
        return None;
    }

    let kind = match (page_truncated, omitted_lines_present) {
        (true, true) => "mixed",
        (true, false) => "page",
        (false, true) => "hunk_lines",
        (false, false) => unreachable!(),
    };
    let continuation_call = next_continuation.map(|continuation| {
        json!({
            "tool": "git_diff_hunks",
            "arguments": git_diff_hunks_call_arguments(
                project,
                paths,
                cached,
                committed_scope,
                max_hunks,
                max_hunk_lines,
                Some(continuation),
            ),
        })
    });

    let (omitted_line_paths, path_provenance) = if omitted_lines_present {
        match git_diff_hunks_exact_truncated_paths(files) {
            Some(paths) => (paths, "exact"),
            None => (paths.to_vec(), "scope"),
        }
    } else {
        (Vec::new(), "none")
    };
    // The producer drains each returned hunk to its boundary even when its
    // model-facing body is line-bounded. These bounded index lists therefore
    // prove whether *all* omitted returned hunks fit both the 400-line ceiling
    // and a fresh path-scoped 32 KiB page. Never infer future byte fit from the
    // already-emitted prefix alone.
    let omitted_lines_fit_line_ceiling = !truncated_hunks.is_empty()
        && truncated_hunks
            .iter()
            .all(|index| line_ceiling_hunks.contains(index));
    let omitted_lines_recovery_proven = !truncated_hunks.is_empty()
        && truncated_hunks
            .iter()
            .all(|index| line_recoverable_hunks.contains(index));
    let single_returned_hunk = git_diff_file_hunk_count(files) == Some(1);
    let exact_single_hunk_recovery = path_provenance == "exact"
        && omitted_line_paths.len() == 1
        && truncated_hunks.len() == 1
        && single_returned_hunk
        && !page_truncated;
    let omitted_lines_recoverable = omitted_lines_present
        && hunk_line_limit
        && max_hunk_lines < MAX_MAX_HUNK_LINES
        && omitted_lines_recovery_proven
        && exact_single_hunk_recovery;
    let omitted_lines_reason = if !omitted_lines_present {
        Value::Null
    } else if !hunk_line_limit {
        json!("page_byte_budget_prevents_proven_recovery")
    } else if max_hunk_lines >= MAX_MAX_HUNK_LINES && !omitted_lines_fit_line_ceiling {
        json!("max_hunk_lines_ceiling_reached")
    } else if !omitted_lines_fit_line_ceiling {
        json!("max_hunk_lines_ceiling_insufficient")
    } else if !omitted_lines_recovery_proven {
        json!("page_byte_budget_prevents_proven_recovery")
    } else if exact_single_hunk_recovery && max_hunk_lines < MAX_MAX_HUNK_LINES {
        json!("larger_max_hunk_lines_available")
    } else {
        json!("bounded_recovery_unavailable")
    };
    let omitted_lines_call = omitted_lines_recoverable.then(|| {
        json!({
            "tool": "git_diff_hunks",
            "arguments": git_diff_hunks_call_arguments(
                project,
                &omitted_line_paths,
                cached,
                committed_scope,
                max_hunks,
                MAX_MAX_HUNK_LINES,
                None,
            ),
        })
    });
    let primary_call = omitted_lines_call.as_ref().or(continuation_call.as_ref());

    Some(json!({
        "kind": kind,
        "tool": "git_diff_hunks",
        "arguments": primary_call
            .map(|call| call["arguments"].clone())
            .unwrap_or(Value::Null),
        "safe_continuation_for_omitted_lines": if omitted_lines_present {
            Value::Bool(false)
        } else {
            Value::Null
        },
        "continuation": {
            "available": continuation_call.is_some(),
            "recovers_later_hunks": continuation_call.is_some(),
            "recovers_omitted_lines": false,
            "next_call": continuation_call,
        },
        "omitted_lines": {
            "present": omitted_lines_present,
            "recoverable": omitted_lines_recoverable,
            "reason_code": omitted_lines_reason,
            "path_provenance": path_provenance,
            "paths": omitted_line_paths,
            "next_call": omitted_lines_call,
        },
    }))
}

fn git_diff_hunks_committed_failure(
    project: &str,
    paths: &[String],
    requested_base: Option<&str>,
    requested_head: Option<&str>,
    reason_code: &'static str,
    exit_code: Option<i32>,
    stderr: &str,
) -> ToolResult {
    let mut result = git_diff_hunks_failure(project, paths, false, reason_code, exit_code, stderr);
    let requested_base = requested_base.and_then(|value| normalize_exact_commit_id(value).ok());
    let requested_head = requested_head.and_then(|value| normalize_exact_commit_id(value).ok());
    if let Some(output) = result.output.as_object_mut() {
        output.insert(
            "scope".to_string(),
            json!({
                "mode": "committed",
                "requested_base": requested_base,
                "requested_head": requested_head,
                "merge_base": null,
                "base_is_ancestor": null,
                "diff_range": null,
            }),
        );
    }
    result
}

fn normalize_git_diff_hunks_committed_range(
    base_commit: Option<&str>,
    head_commit: Option<&str>,
    cached_present: bool,
) -> Result<Option<(String, String)>, &'static str> {
    match (base_commit, head_commit) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err("committed_range_requires_base_and_head"),
        (Some(base), Some(head)) => {
            if cached_present {
                return Err("committed_range_conflicts_with_cached");
            }
            Ok(Some((
                normalize_exact_commit_id(&base)?,
                normalize_exact_commit_id(&head)?,
            )))
        }
    }
}

fn clean_optional_paths(paths: Option<Vec<String>>) -> Result<Vec<String>, String> {
    let mut clean = Vec::new();
    for raw in paths.unwrap_or_default() {
        validate_project_relative_path(&raw)?;
        let path = raw.trim().trim_start_matches("./").trim_end_matches('/');
        if path.is_empty() || path == "." {
            return Err(
                "diff path must name a file or directory, not the project root".to_string(),
            );
        }
        if !clean.iter().any(|p: &String| p == path) {
            clean.push(path.to_string());
        }
    }
    Ok(clean)
}

fn git_diff_hunks_paths_include_secret(paths: &[String]) -> bool {
    paths
        .iter()
        .any(|path| crate::sensitive_paths::is_secret_path(path))
}

fn git_diff_hunks_files_include_secret(files: &[Value]) -> bool {
    files.iter().any(|file| {
        ["path", "old_path"].into_iter().any(|field| {
            file.get(field)
                .and_then(Value::as_str)
                .is_some_and(crate::sensitive_paths::is_secret_path)
        })
    })
}

pub(crate) fn git_diff_hunks_command(paths: &[String], cached: bool) -> Result<String, String> {
    let mut parts = vec!["git".to_string(), "diff".to_string()];
    if cached {
        parts.push("--cached".to_string());
    }
    parts.push("--no-ext-diff".to_string());
    parts.push("--no-textconv".to_string());
    parts.push("--unified=80".to_string());
    if !paths.is_empty() {
        parts.push("--".to_string());
        parts.extend(paths.iter().map(|path| shell_escape_simple(path)));
    }
    Ok(parts.join(" "))
}

fn git_diff_hunks_fingerprint_command(paths: &[String], cached: bool) -> String {
    let mut parts = vec!["git".to_string(), "diff".to_string()];
    if cached {
        parts.push("--cached".to_string());
    }
    parts.extend([
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--binary".to_string(),
        "--full-index".to_string(),
        "--unified=80".to_string(),
    ]);
    if !paths.is_empty() {
        parts.push("--".to_string());
        parts.extend(paths.iter().map(|path| shell_escape_simple(path)));
    }
    parts.join(" ")
}

fn git_diff_hunks_committed_command(
    scope: &CommittedGitScope,
    paths: &[String],
    fingerprint: bool,
) -> String {
    let head_q = shell_escape_simple(&scope.requested_head);
    let mut parts = vec![
        "git".to_string(),
        "--no-pager".to_string(),
        "-c".to_string(),
        "core.quotePath=false".to_string(),
        "-c".to_string(),
        format!("attr.tree={head_q}"),
        "diff".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--find-renames".to_string(),
    ];
    if fingerprint {
        parts.push("--binary".to_string());
        parts.push("--full-index".to_string());
    }
    parts.push("--unified=80".to_string());
    parts.push(shell_escape_simple(&scope.merge_base));
    parts.push(head_q);
    if !paths.is_empty() {
        parts.push("--".to_string());
        parts.extend(paths.iter().map(|path| shell_escape_simple(path)));
    }
    parts.join(" ")
}

fn git_diff_hunks_page_command(
    paths: &[String],
    cached: bool,
    start_position: usize,
    max_hunks: usize,
    max_hunk_lines: usize,
    expected_fence: Option<&str>,
    committed_scope: Option<&CommittedGitScope>,
) -> Result<String, String> {
    let (diff_command, fingerprint_command, prelude) = match committed_scope {
        Some(scope) => (
            git_diff_hunks_committed_command(scope, paths, false),
            git_diff_hunks_committed_command(scope, paths, true),
            format!(
                "{}{}",
                committed_git_discovery_prefix(),
                committed_git_isolated_view_setup(&scope.requested_head, "exit 91")
            ),
        ),
        None => (
            git_diff_hunks_command(paths, cached)?,
            git_diff_hunks_fingerprint_command(paths, cached),
            String::new(),
        ),
    };
    let expected_fence = shell_escape_simple(expected_fence.unwrap_or(""));
    let script = r#"__PRELUDE__
LC_ALL=C; export LC_ALL
page_budget=__PAGE_BUDGET__
max_hunks=__MAX_HUNKS__
max_hunk_lines=__MAX_HUNK_LINES__
max_recovery_hunk_lines=__MAX_RECOVERY_HUNK_LINES__
start_position=__START_POSITION__
expected_fence=__EXPECTED_FENCE__
pre_fence=$(__FINGERPRINT_COMMAND__ | git hash-object --stdin)
pre_hash_exit=$?
__FINGERPRINT_COMMAND__ >/dev/null
pre_diff_exit=$?
stale=0
if [ -n "$expected_fence" ] && [ "$pre_fence" != "$expected_fence" ]; then stale=1; fi
if [ "$pre_hash_exit" -eq 0 ] && [ "$pre_diff_exit" -eq 0 ] && [ "$stale" -eq 0 ]; then
  { __DIFF_COMMAND__; diff_exit=$?; printf '__WCDH_DIFF_EXIT__=%s\n' "$diff_exit"; } |
  awk -v page_budget="$page_budget" -v max_hunks="$max_hunks" -v max_hunk_lines="$max_hunk_lines" -v max_recovery_hunk_lines="$max_recovery_hunk_lines" -v start_position="$start_position" '
function reset_record() {
  record_kind=""; record_buf=""; record_bytes=0; record_byte_trunc=0; record_unreturnable=0;
  hunk_line_count=0; hunk_full_bytes=0; hunk_line_trunc=0;
}
function append_record(line,    line_bytes) {
  if (stopped) return;
  line_bytes=length(line)+1;
  if (record_bytes==0 && line_bytes>page_budget) {
    record_unreturnable=1; record_byte_trunc=1; return;
  }
  if (record_bytes+line_bytes<=page_budget) {
    record_buf=record_buf line "\n"; record_bytes+=line_bytes;
  } else {
    record_byte_trunc=1;
  }
}
function append_hunk_line(line,    line_bytes) {
  line_bytes=length(line)+1;
  hunk_full_bytes+=line_bytes;
  if (hunk_line_count<max_hunk_lines) append_record(line); else hunk_line_trunc=1;
  hunk_line_count++;
}
function note_truncated_hunk(idx) {
  if (truncated_hunks=="") truncated_hunks=idx; else truncated_hunks=truncated_hunks "," idx;
}
function note_line_ceiling_hunk(idx) {
  if (line_ceiling_hunks=="") line_ceiling_hunks=idx; else line_ceiling_hunks=line_ceiling_hunks "," idx;
}
function note_line_recoverable_hunk(idx) {
  if (line_recoverable_hunks=="") line_recoverable_hunks=idx; else line_recoverable_hunks=line_recoverable_hunks "," idx;
}
function flush_record(    need_context, combined_bytes, context_truncated, hunk_index, line_ceiling_fit, line_recovery_safe) {
  if (record_kind=="") return;
  if (record_kind=="file") {
    file_ctx=record_buf; file_ctx_bytes=record_bytes; file_ctx_truncated=record_byte_trunc;
    file_ctx_record=record_index;
  }
  if (record_index<start_position) { reset_record(); return; }
  if (stopped) { has_more=1; reset_record(); return; }
  if (record_unreturnable) {
    page_byte_budget=1; has_more=1; stopped=1; reset_record(); return;
  }
  if (record_kind=="hunk" && returned_hunks>=max_hunks) {
    page_hunk_limit=1; has_more=1; stopped=1; reset_record(); return;
  }
  need_context=(record_kind=="hunk" && !current_file_context_emitted);
  combined_bytes=record_bytes + (need_context ? file_ctx_bytes : 0);
  if (page_bytes+combined_bytes>page_budget) {
    page_byte_budget=1; has_more=1; stopped=1; reset_record(); return;
  }
  context_truncated=0;
  if (need_context) {
    if (file_ctx_bytes>0) { printf "%s", file_ctx; page_bytes+=file_ctx_bytes; }
    current_file_context_emitted=1;
    if (file_ctx_record<start_position && returned_file_records==0 && returned_hunks==0)
      first_file_context_only=1;
    if (file_ctx_truncated) { page_byte_budget=1; context_truncated=1; }
  }
  if (record_bytes>0) { printf "%s", record_buf; page_bytes+=record_bytes; }
  next_position=record_index+1;
  if (record_kind=="file") {
    returned_file_records++;
    current_file_context_emitted=1;
  } else {
    hunk_index=returned_hunks;
    returned_hunks++;
    if (hunk_line_trunc || record_byte_trunc) note_truncated_hunk(hunk_index);
    line_ceiling_fit=(hunk_line_count<=max_recovery_hunk_lines);
    if ((hunk_line_trunc || record_byte_trunc) && line_ceiling_fit)
      note_line_ceiling_hunk(hunk_index);
    line_recovery_safe=(hunk_line_trunc && !record_byte_trunc && line_ceiling_fit && !file_ctx_truncated && file_ctx_bytes+hunk_full_bytes<=page_budget);
    if (line_recovery_safe) note_line_recoverable_hunk(hunk_index);
    if (hunk_line_trunc) hunk_line_limit=1;
    if (record_byte_trunc) page_byte_budget=1;
  }
  if (record_byte_trunc || context_truncated) stopped=1;
  reset_record();
}
function start_file(line) {
  flush_record();
  record_kind="file"; record_index=record_count; record_count++;
  current_file_context_emitted=0; file_ctx=""; file_ctx_bytes=0; file_ctx_truncated=0;
  append_record(line);
}
function start_hunk(line) {
  flush_record();
  record_kind="hunk"; record_index=record_count; record_count++;
  hunk_line_count=0; append_hunk_line(line);
}
function process_line(line) {
  if (substr(line,1,11)=="diff --git ") { start_file(line); return; }
  if (substr(line,1,3)=="@@ ") { start_hunk(line); return; }
  if (record_kind=="hunk") append_hunk_line(line); else if (record_kind=="file") append_record(line);
}
BEGIN {
  record_count=0; next_position=start_position; page_bytes=0; returned_hunks=0; returned_file_records=0;
  has_more=0; stopped=0; page_hunk_limit=0; hunk_line_limit=0; page_byte_budget=0;
  first_file_context_only=0; truncated_hunks=""; line_ceiling_hunks=""; line_recoverable_hunks=""; current_file_context_emitted=0; file_ctx_record=-1;
  reset_record(); have_pending=0; diff_exit=-1;
}
{
  if (have_pending) process_line(pending);
  pending=$0; have_pending=1;
}
END {
  if (have_pending && index(pending,"__WCDH_DIFF_EXIT__=")==1) {
    diff_exit=substr(pending,20)+0;
  } else if (have_pending) {
    process_line(pending);
  }
  flush_record();
  meta=sprintf("diff_exit=%d\nnext_position=%d\ntotal_records=%d\nhas_more=%d\nreturned_hunks=%d\nreturned_file_records=%d\nfirst_file_context_only=%d\npage_hunk_limit=%d\nhunk_line_limit=%d\npage_byte_budget=%d\npage_bytes=%d\ntruncated_hunks=%s\nline_ceiling_hunks=%s\nline_recoverable_hunks=%s", diff_exit, next_position, record_count, has_more, returned_hunks, returned_file_records, first_file_context_only, page_hunk_limit, hunk_line_limit, page_byte_budget, page_bytes, truncated_hunks, line_ceiling_hunks, line_recoverable_hunks);
  printf "%s\n", meta;
  printf "WCDH1:P:%010d:%010d\n", page_bytes, length(meta)+1;
}
'
  page_filter_exit=$?
else
  page_meta=$(printf 'diff_exit=-1\nnext_position=%s\ntotal_records=0\nhas_more=0\nreturned_hunks=0\nreturned_file_records=0\nfirst_file_context_only=0\npage_hunk_limit=0\nhunk_line_limit=0\npage_byte_budget=0\npage_bytes=0\ntruncated_hunks=\nline_ceiling_hunks=\nline_recoverable_hunks=' "$start_position")
  page_meta_bytes=${#page_meta}
  printf '%s\n' "$page_meta"
  printf 'WCDH1:P:%010d:%010d\n' 0 "$((page_meta_bytes+1))"
  page_filter_exit=0
fi
post_fence=$(__FINGERPRINT_COMMAND__ | git hash-object --stdin)
post_hash_exit=$?
__FINGERPRINT_COMMAND__ >/dev/null
post_diff_exit=$?
obs_meta=$(printf 'pre_fence=%s\npost_fence=%s\npre_hash_exit=%s\npost_hash_exit=%s\npre_diff_exit=%s\npost_diff_exit=%s\nstale=%s\npage_filter_exit=%s' "$pre_fence" "$post_fence" "$pre_hash_exit" "$post_hash_exit" "$pre_diff_exit" "$post_diff_exit" "$stale" "$page_filter_exit")
obs_meta_bytes=${#obs_meta}
printf '%s\n' "$obs_meta"
printf 'WCDH1:O:%010d:%010d\n' 0 "$((obs_meta_bytes+1))"
if [ "$pre_hash_exit" -eq 0 ] && [ "$post_hash_exit" -eq 0 ] && [ "$pre_diff_exit" -eq 0 ] && [ "$post_diff_exit" -eq 0 ] && [ "$stale" -eq 0 ] && [ "$page_filter_exit" -eq 0 ] && [ "$pre_fence" = "$post_fence" ]; then
  exit 0
fi
exit 1
"#;
    Ok(script
        .replace("__PRELUDE__", &prelude)
        .replace("__PAGE_BUDGET__", &GIT_DIFF_HUNKS_PAGE_BYTES.to_string())
        .replace("__MAX_HUNKS__", &max_hunks.to_string())
        .replace("__MAX_HUNK_LINES__", &max_hunk_lines.to_string())
        .replace(
            "__MAX_RECOVERY_HUNK_LINES__",
            &MAX_MAX_HUNK_LINES.to_string(),
        )
        .replace("__START_POSITION__", &start_position.to_string())
        .replace("__EXPECTED_FENCE__", &expected_fence)
        .replace("__FINGERPRINT_COMMAND__", &fingerprint_command)
        .replace("__DIFF_COMMAND__", &diff_command))
}

#[derive(Debug)]
struct GitDiffHunksPageWire {
    diff: String,
    diff_exit: i32,
    next_position: usize,
    total_records: usize,
    has_more: bool,
    returned_hunks: usize,
    returned_file_records: usize,
    first_file_context_only: bool,
    page_hunk_limit: bool,
    hunk_line_limit: bool,
    page_byte_budget: bool,
    page_bytes: usize,
    truncated_hunks: Vec<usize>,
    line_ceiling_hunks: Vec<usize>,
    line_recoverable_hunks: Vec<usize>,
    pre_fence: String,
    post_fence: String,
    pre_hash_exit: i32,
    post_hash_exit: i32,
    pre_diff_exit: i32,
    post_diff_exit: i32,
    stale: bool,
    page_filter_exit: i32,
}

fn parse_git_diff_hunks_wire_block(
    stdout: &str,
    end: usize,
    kind: u8,
) -> Option<(&str, &str, usize)> {
    let bytes = stdout.as_bytes();
    let trailer_start = end.checked_sub(GIT_DIFF_HUNKS_BLOCK_TRAILER_BYTES)?;
    let trailer = bytes.get(trailer_start..end)?;
    if trailer.get(..6)? != GIT_DIFF_HUNKS_BLOCK_MAGIC
        || trailer.get(6).copied()? != kind
        || trailer.get(7).copied()? != b':'
        || trailer.get(18).copied()? != b':'
        || trailer.get(29).copied()? != b'\n'
    {
        return None;
    }
    let data_wire_bytes = parse_fixed_decimal(trailer.get(8..18)?)?;
    let meta_wire_bytes = parse_fixed_decimal(trailer.get(19..29)?)?;
    let meta_start = trailer_start.checked_sub(meta_wire_bytes)?;
    let data_start = meta_start.checked_sub(data_wire_bytes)?;
    let data = std::str::from_utf8(bytes.get(data_start..meta_start)?).ok()?;
    let meta = std::str::from_utf8(bytes.get(meta_start..trailer_start)?).ok()?;
    Some((data, meta, data_start))
}

fn parse_required_i32(meta: &str, key: &str) -> Option<i32> {
    parse_status_result_field(meta, key)?.parse().ok()
}

fn parse_hunk_indices(meta: &str, key: &str, returned_hunks: usize) -> Option<Vec<usize>> {
    let raw = parse_status_result_field(meta, key)?;
    if raw.is_empty() {
        return Some(Vec::new());
    }
    let mut indices = Vec::new();
    for value in raw.split(',') {
        let index = value.parse::<usize>().ok()?;
        if index >= returned_hunks || indices.contains(&index) {
            return None;
        }
        indices.push(index);
    }
    Some(indices)
}

fn parse_framed_git_diff_hunks_stdout(stdout: &str) -> Option<GitDiffHunksPageWire> {
    let mut cursor = stdout.len();
    let (obs_data, obs_meta, start) = parse_git_diff_hunks_wire_block(stdout, cursor, b'O')?;
    if !obs_data.is_empty() {
        return None;
    }
    cursor = start;
    let (diff, page_meta, start) = parse_git_diff_hunks_wire_block(stdout, cursor, b'P')?;
    if start != 0 || diff.len() > GIT_DIFF_HUNKS_PAGE_BYTES {
        return None;
    }
    let obs_meta = strip_wire_lf(obs_meta)?;
    let page_meta = strip_wire_lf(page_meta)?;
    let page_bytes = parse_optional_usize(&page_meta, "page_bytes")?;
    if page_bytes != diff.len() {
        return None;
    }
    let returned_hunks = parse_optional_usize(&page_meta, "returned_hunks")?;
    let returned_file_records = parse_optional_usize(&page_meta, "returned_file_records")?;
    let truncated_hunks = parse_hunk_indices(&page_meta, "truncated_hunks", returned_hunks)?;
    let line_ceiling_hunks = parse_hunk_indices(&page_meta, "line_ceiling_hunks", returned_hunks)?;
    let line_recoverable_hunks =
        parse_hunk_indices(&page_meta, "line_recoverable_hunks", returned_hunks)?;
    Some(GitDiffHunksPageWire {
        diff: strip_wire_lf(diff)?,
        diff_exit: parse_required_i32(&page_meta, "diff_exit")?,
        next_position: parse_optional_usize(&page_meta, "next_position")?,
        total_records: parse_optional_usize(&page_meta, "total_records")?,
        has_more: parse_optional_bool(&page_meta, "has_more")?,
        returned_hunks,
        returned_file_records,
        first_file_context_only: parse_optional_bool(&page_meta, "first_file_context_only")?,
        page_hunk_limit: parse_optional_bool(&page_meta, "page_hunk_limit")?,
        hunk_line_limit: parse_optional_bool(&page_meta, "hunk_line_limit")?,
        page_byte_budget: parse_optional_bool(&page_meta, "page_byte_budget")?,
        page_bytes,
        truncated_hunks,
        line_ceiling_hunks,
        line_recoverable_hunks,
        pre_fence: parse_status_result_field(&obs_meta, "pre_fence")?.to_string(),
        post_fence: parse_status_result_field(&obs_meta, "post_fence")?.to_string(),
        pre_hash_exit: parse_required_i32(&obs_meta, "pre_hash_exit")?,
        post_hash_exit: parse_required_i32(&obs_meta, "post_hash_exit")?,
        pre_diff_exit: parse_required_i32(&obs_meta, "pre_diff_exit")?,
        post_diff_exit: parse_required_i32(&obs_meta, "post_diff_exit")?,
        stale: parse_optional_bool(&obs_meta, "stale")?,
        page_filter_exit: parse_required_i32(&obs_meta, "page_filter_exit")?,
    })
}

fn mark_git_diff_hunks_page_metadata(
    files: &mut [Value],
    truncated_hunks: &[usize],
    first_file_context_only: bool,
) -> usize {
    if first_file_context_only {
        if let Some(file) = files.first_mut().and_then(Value::as_object_mut) {
            file.insert("continued".to_string(), json!(true));
        }
    }
    let mut index = 0usize;
    for file in files {
        let Some(hunks) = file.get_mut("hunks").and_then(Value::as_array_mut) else {
            continue;
        };
        for hunk in hunks {
            if truncated_hunks.contains(&index) {
                if let Some(hunk) = hunk.as_object_mut() {
                    hunk.insert("truncated".to_string(), json!(true));
                }
            }
            index += 1;
        }
    }
    index
}

fn strip_diff_prefix(path: &str) -> String {
    let decoded = decode_git_quoted_path(path).unwrap_or_else(|| path.to_string());
    decoded
        .strip_prefix("a/")
        .or_else(|| decoded.strip_prefix("b/"))
        .unwrap_or(&decoded)
        .to_string()
}

fn parse_binary_diff_paths(line: &str) -> Option<(Option<String>, Option<String>)> {
    let body = line
        .strip_prefix("Binary files ")?
        .strip_suffix(" differ")?;
    for (index, _) in body.match_indices(" and ") {
        let old_raw = &body[..index];
        let new_raw = &body[index + " and ".len()..];
        let old_path = (old_raw != "/dev/null").then(|| strip_diff_prefix(old_raw));
        let path = (new_raw != "/dev/null").then(|| strip_diff_prefix(new_raw));
        let plausible = match (&old_path, &path) {
            (None, Some(_)) | (Some(_), None) => true,
            (Some(old_path), Some(path)) => old_path == path,
            (None, None) => false,
        };
        if plausible {
            return Some((old_path, path));
        }
    }
    None
}

fn parse_hunk_header(header: &str) -> (i64, i64, i64, i64) {
    fn parse_range(raw: &str) -> (i64, i64) {
        let raw = raw.trim_start_matches(['-', '+']);
        let mut parts = raw.splitn(2, ',');
        let start = parts.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
        let lines = parts.next().unwrap_or("1").parse::<i64>().unwrap_or(1);
        (start, lines)
    }
    let mut parts = header.split_whitespace();
    let _at = parts.next();
    let old = parts.next().unwrap_or("-0,0");
    let new = parts.next().unwrap_or("+0,0");
    let (old_start, old_lines) = parse_range(old);
    let (new_start, new_lines) = parse_range(new);
    (old_start, old_lines, new_start, new_lines)
}

fn finish_hunk(
    file: &mut serde_json::Map<String, serde_json::Value>,
    current_hunk: &mut Option<serde_json::Map<String, serde_json::Value>>,
    hunk_lines: &mut Vec<String>,
) {
    let Some(mut hunk) = current_hunk.take() else {
        return;
    };
    hunk.insert("diff".to_string(), json!(hunk_lines.join("\n")));
    hunk.insert("line_count".to_string(), json!(hunk_lines.len()));
    file.entry("hunks".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("hunks array")
        .push(json!(hunk));
    hunk_lines.clear();
}

fn finish_file(
    files: &mut Vec<serde_json::Value>,
    current_file: &mut Option<serde_json::Map<String, serde_json::Value>>,
    current_hunk: &mut Option<serde_json::Map<String, serde_json::Value>>,
    hunk_lines: &mut Vec<String>,
) {
    let Some(mut file) = current_file.take() else {
        return;
    };
    finish_hunk(&mut file, current_hunk, hunk_lines);
    if file.get("hunks").is_none() {
        file.insert("hunks".to_string(), json!([]));
    }
    files.push(json!(file));
}

pub(crate) fn parse_git_diff_hunks(
    diff: &str,
    max_hunks: usize,
    max_hunk_lines: usize,
) -> (Vec<serde_json::Value>, usize, bool) {
    let mut files = Vec::new();
    let mut current_file: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut current_hunk: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut hunk_lines = Vec::new();
    let mut hunk_count = 0usize;
    let mut truncated = false;
    let mut skip_current_hunk = false;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            finish_file(
                &mut files,
                &mut current_file,
                &mut current_hunk,
                &mut hunk_lines,
            );
            let mut parts = rest.split_whitespace();
            let old_path = parts.next().map(strip_diff_prefix).unwrap_or_default();
            let path = parts.next().map(strip_diff_prefix).unwrap_or_default();
            let mut file = serde_json::Map::new();
            file.insert("path".to_string(), json!(path));
            file.insert("old_path".to_string(), json!(old_path));
            file.insert("status".to_string(), json!("modified"));
            file.insert("hunks".to_string(), json!([]));
            current_file = Some(file);
            skip_current_hunk = false;
            continue;
        }

        let Some(file) = current_file.as_mut() else {
            continue;
        };

        if line.starts_with("new file mode ") {
            file.insert("status".to_string(), json!("added"));
        } else if line.starts_with("deleted file mode ") {
            file.insert("status".to_string(), json!("deleted"));
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.insert("old_path".to_string(), json!(strip_diff_prefix(path)));
            file.insert("status".to_string(), json!("renamed"));
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.insert("path".to_string(), json!(strip_diff_prefix(path)));
            file.insert("status".to_string(), json!("renamed"));
        } else if line.starts_with("Binary files ") {
            file.insert("binary".to_string(), json!(true));
            if file.get("status").and_then(Value::as_str) != Some("renamed") {
                if let Some((old_path, path)) = parse_binary_diff_paths(line) {
                    file.insert("old_path".to_string(), json!(old_path));
                    file.insert("path".to_string(), json!(path));
                    if file.get("old_path").is_some_and(Value::is_null) {
                        file.insert("status".to_string(), json!("added"));
                    } else if file.get("path").is_some_and(Value::is_null) {
                        file.insert("status".to_string(), json!("deleted"));
                    }
                }
            }
        } else if let Some(path) = line.strip_prefix("--- ") {
            if path == "/dev/null" {
                file.insert("old_path".to_string(), json!(null));
                file.insert("status".to_string(), json!("added"));
            } else {
                file.insert("old_path".to_string(), json!(strip_diff_prefix(path)));
            }
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if path == "/dev/null" {
                file.insert("path".to_string(), json!(null));
                file.insert("status".to_string(), json!("deleted"));
            } else {
                file.insert("path".to_string(), json!(strip_diff_prefix(path)));
            }
        }

        if line.starts_with("@@ ") {
            finish_hunk(file, &mut current_hunk, &mut hunk_lines);
            if hunk_count >= max_hunks {
                truncated = true;
                skip_current_hunk = true;
                continue;
            }
            let (old_start, old_lines, new_start, new_lines) = parse_hunk_header(line);
            let mut hunk = serde_json::Map::new();
            hunk.insert("old_start".to_string(), json!(old_start));
            hunk.insert("old_lines".to_string(), json!(old_lines));
            hunk.insert("new_start".to_string(), json!(new_start));
            hunk.insert("new_lines".to_string(), json!(new_lines));
            hunk.insert("header".to_string(), json!(line));
            hunk.insert("truncated".to_string(), json!(false));
            current_hunk = Some(hunk);
            hunk_lines.push(line.to_string());
            hunk_count += 1;
            skip_current_hunk = false;
            continue;
        }

        if current_hunk.is_some() && !skip_current_hunk {
            if hunk_lines.len() < max_hunk_lines {
                hunk_lines.push(line.to_string());
            } else {
                truncated = true;
                if let Some(hunk) = current_hunk.as_mut() {
                    hunk.insert("truncated".to_string(), json!(true));
                }
            }
        }
    }
    finish_file(
        &mut files,
        &mut current_file,
        &mut current_hunk,
        &mut hunk_lines,
    );
    (files, hunk_count, truncated)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PorcelainSummary {
    pub(crate) changed_files: Vec<String>,
    pub(crate) tracked_changed_files: Vec<String>,
    pub(crate) untracked_files: Vec<String>,
    pub(crate) ignored_files: Vec<String>,
    pub(crate) changed_files_count: usize,
}

/// Parse `git status --porcelain` output into tracked/untracked buckets.
/// Handles renames (`R  old -> new` -> `new`) and quoted paths.
pub(crate) fn parse_porcelain_summary(porcelain: &str) -> PorcelainSummary {
    let mut summary = PorcelainSummary::default();
    for line in porcelain.lines() {
        if line.len() < 4 {
            continue;
        }
        let status = &line[..2];
        let path_part = &line[3..];
        let path = if let Some((_, dst)) = path_part.split_once(" -> ") {
            dst
        } else {
            path_part
        };
        let path = path.trim().trim_matches('"');
        if path.is_empty() {
            continue;
        }
        match status {
            "??" => summary.untracked_files.push(path.to_string()),
            "!!" => summary.ignored_files.push(path.to_string()),
            _ => summary.tracked_changed_files.push(path.to_string()),
        }
        summary.changed_files.push(path.to_string());
    }
    summary.changed_files_count = summary.changed_files.len();
    summary
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitCommitMarker {
    pub(crate) status: String,
    pub(crate) previous_head: Option<String>,
    pub(crate) actual_head: Option<String>,
    pub(crate) new_head: Option<String>,
}

fn validate_git_commit_paths_input(
    expected_head: &str,
    paths: &[String],
    message: &str,
) -> Result<(String, Vec<String>, String), String> {
    let expected_head = normalize_exact_commit_id(expected_head)
        .map_err(|_| "expected_head must be one exact 40-hex commit id".to_string())?;
    if paths.len() > GIT_COMMIT_PATHS_MAX_PATHS {
        return Err(format!(
            "paths may contain at most {GIT_COMMIT_PATHS_MAX_PATHS} entries"
        ));
    }
    let paths = validate_limited_cleanup_paths(paths, true)?;
    for path in &paths {
        if path.chars().count() > GIT_COMMIT_PATH_MAX_CHARS {
            return Err(format!(
                "commit path exceeds {GIT_COMMIT_PATH_MAX_CHARS} characters"
            ));
        }
        if path.chars().any(char::is_control) {
            return Err("commit paths must not contain control characters".to_string());
        }
    }
    if message.trim().is_empty() {
        return Err("message must not be empty or whitespace-only".to_string());
    }
    if message.chars().count() > GIT_COMMIT_MESSAGE_MAX_CHARS {
        return Err(format!(
            "message may contain at most {GIT_COMMIT_MESSAGE_MAX_CHARS} characters"
        ));
    }
    if message.contains('\0') {
        return Err("message must not contain NUL".to_string());
    }
    Ok((expected_head, paths, message.to_string()))
}

fn git_commit_paths_script(expected_head: &str, paths: &[String], message: &str) -> String {
    let expected = shell_escape_simple(expected_head);
    let message = shell_escape_simple(message);
    let mut path_checks = String::new();
    let mut index_adds = String::new();
    let mut requested_lines = String::new();
    let mut reset_args = String::new();
    for path in paths {
        let quoted = shell_escape_simple(path);
        path_checks.push_str(&format!(
            "if [ -d {quoted} ]; then printf '{GIT_COMMIT_RESULT_PREFIX} status=directory_path\\n'; exit 23; fi\n\
             if [ ! -e {quoted} ] && [ ! -L {quoted} ] && ! git ls-files --error-unmatch -- {quoted} >/dev/null 2>&1; then printf '{GIT_COMMIT_RESULT_PREFIX} status=missing_path\\n'; exit 24; fi\n\
             if [ -z \"$(git status --porcelain=v1 --untracked-files=all -- {quoted})\" ]; then printf '{GIT_COMMIT_RESULT_PREFIX} status=unchanged_path\\n'; exit 25; fi\n"
        ));
        index_adds.push_str(&format!(
            "if ! GIT_INDEX_FILE=\"$index_file\" git add -A -- {quoted}; then printf '{GIT_COMMIT_RESULT_PREFIX} status=stage_failed\\n'; exit 26; fi\n"
        ));
        requested_lines.push_str(&format!("printf '%s\\n' {quoted} >> \"$req_file\"\n"));
        reset_args.push(' ');
        reset_args.push_str(&quoted);
    }

    format!(
        "set -eu\n\
         export LC_ALL=C GIT_PAGER=cat GIT_TERMINAL_PROMPT=0\n\
         unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE\n\
         expected={expected}\n\
         index_file=$(mktemp \"${{TMPDIR:-/tmp}}/webcodex-index.XXXXXX\")\n\
         req_file=$(mktemp \"${{TMPDIR:-/tmp}}/webcodex-req.XXXXXX\")\n\
         req_sorted=$(mktemp \"${{TMPDIR:-/tmp}}/webcodex-req-sorted.XXXXXX\")\n\
         staged_file=$(mktemp \"${{TMPDIR:-/tmp}}/webcodex-staged.XXXXXX\")\n\
         msg_file=$(mktemp \"${{TMPDIR:-/tmp}}/webcodex-msg.XXXXXX\")\n\
         rm -f \"$index_file\"\n\
         cleanup() {{ rm -f \"$index_file\" \"$req_file\" \"$req_sorted\" \"$staged_file\" \"$msg_file\"; }}\n\
         trap cleanup EXIT HUP INT TERM\n\
         actual=$(git rev-parse --verify HEAD 2>/dev/null || true)\n\
         if [ \"$actual\" != \"$expected\" ]; then printf '{GIT_COMMIT_RESULT_PREFIX} status=head_mismatch actual=%s\\n' \"$actual\"; exit 20; fi\n\
         if [ -n \"$(git diff --name-only --diff-filter=U)\" ]; then printf '{GIT_COMMIT_RESULT_PREFIX} status=conflicts\\n'; exit 21; fi\n\
         if ! git diff --cached --quiet --exit-code; then printf '{GIT_COMMIT_RESULT_PREFIX} status=existing_staged\\n'; exit 22; fi\n\
         {path_checks}\
         {requested_lines}\
         if ! GIT_INDEX_FILE=\"$index_file\" git read-tree \"$expected\"; then printf '{GIT_COMMIT_RESULT_PREFIX} status=index_init_failed\\n'; exit 26; fi\n\
         {index_adds}\
         LC_ALL=C sort -u \"$req_file\" > \"$req_sorted\"\n\
         GIT_INDEX_FILE=\"$index_file\" git -c core.quotepath=false diff --cached --name-only --no-renames \"$expected\" | LC_ALL=C sort -u > \"$staged_file\"\n\
         if ! cmp -s \"$req_sorted\" \"$staged_file\"; then printf '{GIT_COMMIT_RESULT_PREFIX} status=staged_set_mismatch\\n'; exit 27; fi\n\
         if GIT_INDEX_FILE=\"$index_file\" git diff --cached --quiet --exit-code \"$expected\"; then printf '{GIT_COMMIT_RESULT_PREFIX} status=no_changes\\n'; exit 25; fi\n\
         if [ -n \"$(git diff --name-only --diff-filter=U)\" ] || ! git diff --cached --quiet --exit-code; then printf '{GIT_COMMIT_RESULT_PREFIX} status=staged_state_changed\\n'; exit 28; fi\n\
         actual=$(git rev-parse --verify HEAD 2>/dev/null || true)\n\
         if [ \"$actual\" != \"$expected\" ]; then printf '{GIT_COMMIT_RESULT_PREFIX} status=head_changed actual=%s\\n' \"$actual\"; exit 29; fi\n\
         tree=$(GIT_INDEX_FILE=\"$index_file\" git write-tree) || {{ printf '{GIT_COMMIT_RESULT_PREFIX} status=tree_failed\\n'; exit 30; }}\n\
         printf '%s' {message} > \"$msg_file\"\n\
         new=$(git commit-tree \"$tree\" -p \"$expected\" < \"$msg_file\") || {{ printf '{GIT_COMMIT_RESULT_PREFIX} status=commit_create_failed\\n'; exit 31; }}\n\
         if ! git update-ref -m 'webcodex git_commit_paths' HEAD \"$new\" \"$expected\"; then actual=$(git rev-parse --verify HEAD 2>/dev/null || true); printf '{GIT_COMMIT_RESULT_PREFIX} status=head_update_failed actual=%s\\n' \"$actual\"; exit 32; fi\n\
         if ! git reset -q \"$new\" --{reset_args}; then printf '{GIT_COMMIT_RESULT_PREFIX} status=index_cleanup_failed previous=%s new=%s\\n' \"$expected\" \"$new\"; exit 33; fi\n\
         printf '{GIT_COMMIT_RESULT_PREFIX} status=success previous=%s new=%s\\n' \"$expected\" \"$new\"\n"
    )
}

pub(crate) fn parse_git_commit_marker(stdout: &str) -> Option<GitCommitMarker> {
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with(GIT_COMMIT_RESULT_PREFIX))?;
    let mut status = None;
    let mut previous_head = None;
    let mut actual_head = None;
    let mut new_head = None;
    for field in line[GIT_COMMIT_RESULT_PREFIX.len()..].split_whitespace() {
        let (key, value) = field.split_once('=')?;
        match key {
            "status" => status = Some(value.to_string()),
            "previous" => previous_head = normalize_exact_commit_id(value).ok(),
            "actual" => actual_head = normalize_exact_commit_id(value).ok(),
            "new" => new_head = normalize_exact_commit_id(value).ok(),
            _ => {}
        }
    }
    Some(GitCommitMarker {
        status: status?,
        previous_head,
        actual_head,
        new_head,
    })
}

fn git_commit_paths_failure_output(
    expected_head: &str,
    failure_kind: &str,
    actual_head: Option<String>,
) -> Value {
    json!({
        "committed": false,
        "expected_head": expected_head,
        "previous_head": null,
        "actual_head": actual_head,
        "new_head": null,
        "committed_paths": [],
        "state_changed": false,
        "outcome_unknown": false,
        "failure_kind": failure_kind,
        "hook_policy": "bypassed_exact_tree",
    })
}

fn git_commit_paths_outcome_unknown(expected_head: &str, reason: &str) -> ToolResult {
    ToolResult::err_with_output(
        format!(
            "git_commit_paths outcome is unknown: {reason}. Do not retry the commit blindly; observe HEAD/status first."
        ),
        json!({
            "committed": null,
            "expected_head": expected_head,
            "previous_head": null,
            "actual_head": null,
            "new_head": null,
            "committed_paths": [],
            "state_changed": null,
            "outcome_unknown": true,
            "failure_kind": "outcome_unknown",
            "hook_policy": "bypassed_exact_tree",
        }),
    )
    .with_recovery(RecoveryKind::Reobserve, None)
}

impl ToolRuntime {
    async fn collect_show_changes_untracked_previews(
        &self,
        project: &str,
        untracked_paths: &[String],
    ) -> (Vec<Value>, bool) {
        let truncated = untracked_paths.len() > SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_FILES;
        if self.resolve_project(project).await.is_err() {
            return (Vec::new(), truncated);
        }
        let mut previews = Vec::new();
        for path in untracked_paths
            .iter()
            .take(SHOW_CHANGES_UNTRACKED_PREVIEW_MAX_FILES)
        {
            if untracked_preview_path_is_invalid(path) || untracked_preview_path_is_sensitive(path)
            {
                previews.push(skipped_untracked_preview(
                    path,
                    "sensitive_or_excluded_path",
                    None,
                ));
                continue;
            }
            let command = show_changes_untracked_preview_probe_command(path);
            let preview = match self
                .run_project_internal_posix_script_capture(project, command, 10, None)
                .await
            {
                Ok(output) => {
                    parse_show_changes_agent_preview_probe(path, &output.stdout, output.exit_code)
                }
                Err(_) => skipped_untracked_preview(path, "read_error", None),
            };
            previews.push(preview);
        }
        (previews, truncated)
    }

    pub(crate) async fn git_restore_paths(
        &self,
        project: String,
        paths: Vec<String>,
    ) -> ToolResult {
        let paths = match validate_limited_cleanup_paths(&paths, true) {
            Ok(paths) => paths,
            Err(e) => return ToolResult::err(e),
        };
        let mut args = vec!["restore".to_string(), "--".to_string()];
        args.extend(paths.iter().cloned());
        let result = self
            .run_internal_process_sync(project, "git".to_string(), args, 30)
            .await;
        if result.success {
            ToolResult::ok(json!({
                "restored_paths": paths,
                "command_result": result.output,
            }))
        } else {
            result
        }
    }

    pub(crate) async fn discard_untracked(
        &self,
        project: String,
        paths: Vec<String>,
    ) -> ToolResult {
        let paths = match validate_limited_cleanup_paths(&paths, true) {
            Ok(paths) => paths,
            Err(e) => return ToolResult::err(e),
        };
        let mut args = vec!["clean".to_string(), "-f".to_string(), "--".to_string()];
        args.extend(paths.iter().cloned());
        let result = self
            .run_internal_process_sync(project, "git".to_string(), args, 30)
            .await;
        if result.success {
            ToolResult::ok(json!({
                "discarded_untracked_paths": paths,
                "command_result": result.output,
            }))
        } else {
            result
        }
    }

    pub(crate) async fn git_status(&self, project: String) -> ToolResult {
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let client_id = proj.client_id.clone();
        let (req_id, rx) = match self
            .runner_registry
            .enqueue_run(
                ShellRunRequest {
                    client_id,
                    cwd: Some(proj.path.clone()),
                    command: "git status --porcelain".to_string(),
                    stdin: None,
                    timeout_secs: 30,
                    wait_timeout_secs: 32,
                },
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(e),
        };
        match tokio::time::timeout(Duration::from_secs(34), rx).await {
            Ok(Ok(resp)) => ToolResult::ok(json!({
                "stdout": resp.stdout,
                "stderr": resp.stderr,
                "exit_code": resp.exit_code,
            })),
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&req_id).await;
                ToolResult::err("request dropped")
            }
            Err(_) => {
                self.runner_registry.cancel_request(&req_id).await;
                ToolResult::err("timed out")
            }
        }
    }

    pub(crate) async fn git_diff(&self, project: String, args: Option<Vec<String>>) -> ToolResult {
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let diff_args = args.unwrap_or_default();
        let cmd = if diff_args.is_empty() {
            "git diff".to_string()
        } else {
            // `args` is the existing pathspec list contract. Keep the `--`
            // fence and POSIX word quoting exactly as before, but execute the
            // generated command through the typed internal POSIX path so the
            // quoting can never be parsed by the user's configured shell.
            let escaped: Vec<String> = diff_args.iter().map(|a| shell_escape_simple(a)).collect();
            format!("git diff -- {}", escaped.join(" "))
        };
        let client_id = proj.client_id.clone();
        let (req_id, rx) = match self
            .runner_registry
            .enqueue_internal_posix_script(
                client_id,
                Some(proj.path.clone()),
                cmd,
                30,
                32,
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(e),
        };
        match tokio::time::timeout(Duration::from_secs(34), rx).await {
            Ok(Ok(resp)) => ToolResult::ok(json!({
                "stdout": resp.stdout,
                "stderr": resp.stderr,
                "exit_code": resp.exit_code,
            })),
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&req_id).await;
                ToolResult::err("request dropped")
            }
            Err(_) => {
                self.runner_registry.cancel_request(&req_id).await;
                ToolResult::err("timed out")
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn git_diff_hunks(
        &self,
        project: String,
        paths: Option<Vec<String>>,
        max_hunks: Option<usize>,
        max_hunk_lines: Option<usize>,
        cached: Option<bool>,
    ) -> ToolResult {
        self.git_diff_hunks_continued(project, paths, max_hunks, max_hunk_lines, cached, None)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn git_diff_hunks_continued(
        &self,
        project: String,
        paths: Option<Vec<String>>,
        max_hunks: Option<usize>,
        max_hunk_lines: Option<usize>,
        cached: Option<bool>,
        continuation: Option<String>,
    ) -> ToolResult {
        self.git_diff_hunks_continued_with_range(
            project,
            paths,
            max_hunks,
            max_hunk_lines,
            cached,
            None,
            None,
            continuation,
        )
        .await
    }

    pub(crate) async fn git_diff_hunks_continued_with_range(
        &self,
        project: String,
        paths: Option<Vec<String>>,
        max_hunks: Option<usize>,
        max_hunk_lines: Option<usize>,
        cached: Option<bool>,
        base_commit: Option<String>,
        head_commit: Option<String>,
        continuation: Option<String>,
    ) -> ToolResult {
        let paths = match clean_optional_paths(paths) {
            Ok(paths) => paths,
            Err(e) => return ToolResult::err(e),
        };
        if git_diff_hunks_paths_include_secret(&paths) {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached.unwrap_or(false),
                "sensitive_path",
                None,
                "",
            );
        }
        let committed_range = match normalize_git_diff_hunks_committed_range(
            base_commit.as_deref(),
            head_commit.as_deref(),
            cached.is_some(),
        ) {
            Ok(range) => range,
            Err(reason) => {
                return git_diff_hunks_committed_failure(
                    &project,
                    &paths,
                    base_commit.as_deref(),
                    head_commit.as_deref(),
                    reason,
                    None,
                    "",
                )
            }
        };
        let max_hunks = max_hunks
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_HUNKS)
            .min(MAX_MAX_HUNKS);
        let max_hunk_lines = max_hunk_lines
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_HUNK_LINES)
            .min(MAX_MAX_HUNK_LINES);
        let cached = cached.unwrap_or(false);
        if continuation
            .as_ref()
            .is_some_and(|value| value.len() > GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES)
        {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "invalid_continuation",
                None,
                "",
            );
        }
        let resolved = match self.resolve_project_input(&project).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_tool_result(),
        };
        let committed_scope = match committed_range.as_ref() {
            Some((base, head)) => match self
                .resolve_committed_git_scope(&resolved.resolved_id, base, head)
                .await
            {
                Ok(scope) => Some(scope),
                Err(reason) => {
                    return git_diff_hunks_committed_failure(
                        &project,
                        &paths,
                        Some(base),
                        Some(head),
                        reason,
                        None,
                        "",
                    )
                }
            },
            None => None,
        };
        let scope = match committed_scope.as_ref() {
            Some(committed_scope) => git_diff_hunks_committed_scope_digest(
                &resolved.resolved_id,
                &paths,
                committed_scope,
                max_hunks,
                max_hunk_lines,
            ),
            None => git_diff_hunks_scope_digest(&resolved.resolved_id, &paths, cached),
        };
        let mut command_paths = paths.clone();
        command_paths.sort();
        let decoded = match continuation.as_deref() {
            Some(raw) => {
                match decode_git_diff_hunks_continuation(
                    raw,
                    &scope,
                    committed_scope
                        .as_ref()
                        .map(|_| self.git_diff_hunks_continuation_mac_key.as_ref()),
                ) {
                    Ok(token) => Some(token),
                    Err(GitDiffHunksContinuationError::Invalid) => {
                        return git_diff_hunks_failure(
                            &project,
                            &paths,
                            cached,
                            "invalid_continuation",
                            None,
                            "",
                        )
                    }
                    Err(GitDiffHunksContinuationError::ScopeMismatch) => {
                        return git_diff_hunks_failure(
                            &project,
                            &paths,
                            cached,
                            "continuation_mismatch",
                            None,
                            "",
                        )
                    }
                }
            }
            None => None,
        };
        let start_position = decoded
            .as_ref()
            .and_then(|token| usize::try_from(token.next).ok())
            .unwrap_or(0);
        let expected_fence = decoded.as_ref().map(|token| token.fence.as_str());
        let command = match git_diff_hunks_page_command(
            &command_paths,
            cached,
            start_position,
            max_hunks,
            max_hunk_lines,
            expected_fence,
            committed_scope.as_ref(),
        ) {
            Ok(command) => command,
            Err(_) => {
                return git_diff_hunks_failure(
                    &project,
                    &paths,
                    cached,
                    "source_observation_unavailable",
                    None,
                    "",
                )
            }
        };
        let output = match self
            .run_project_internal_posix_script_capture(&resolved.resolved_id, command, 30, None)
            .await
        {
            Ok(output) => output,
            Err(_) => {
                return git_diff_hunks_failure(
                    &project,
                    &paths,
                    cached,
                    "source_observation_unavailable",
                    None,
                    "",
                )
            }
        };
        let stderr = bounded_git_diff_hunks_stderr(&output.stderr);
        let Some(wire) = parse_framed_git_diff_hunks_stdout(&output.stdout) else {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "source_observation_unavailable",
                output.exit_code,
                &stderr,
            );
        };
        if wire.stale
            || decoded
                .as_ref()
                .is_some_and(|token| token.fence != wire.pre_fence)
        {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "stale_continuation",
                output.exit_code,
                &stderr,
            );
        }
        if wire.pre_hash_exit != 0
            || wire.post_hash_exit != 0
            || !is_git_object_hex(&wire.pre_fence)
            || !is_git_object_hex(&wire.post_fence)
        {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "source_observation_unavailable",
                output.exit_code,
                &stderr,
            );
        }
        if wire.pre_diff_exit != 0 || wire.post_diff_exit != 0 || wire.diff_exit != 0 {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "git_diff_failed",
                output.exit_code,
                &stderr,
            );
        }
        if wire.pre_fence != wire.post_fence {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                if decoded.is_some() {
                    "stale_continuation"
                } else {
                    "source_changed_during_observation"
                },
                output.exit_code,
                &stderr,
            );
        }
        if output.error.is_some()
            || output.exit_code != Some(0)
            || wire.page_filter_exit != 0
            || wire.page_bytes > GIT_DIFF_HUNKS_PAGE_BYTES
        {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "source_observation_unavailable",
                output.exit_code,
                &stderr,
            );
        }
        if start_position > wire.total_records
            || wire.next_position < start_position
            || wire.next_position > wire.total_records
            || (wire.has_more && wire.next_position <= start_position)
            || wire.returned_hunks > max_hunks
        {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                if wire.has_more && wire.next_position <= start_position {
                    "page_record_too_large"
                } else {
                    "source_observation_unavailable"
                },
                output.exit_code,
                &stderr,
            );
        }
        let (mut files, parsed_hunks, parser_truncated) =
            parse_git_diff_hunks(&wire.diff, MAX_MAX_HUNKS, MAX_MAX_HUNK_LINES);
        if git_diff_hunks_files_include_secret(&files) {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "sensitive_path",
                output.exit_code,
                "",
            );
        }
        let marked_hunks = mark_git_diff_hunks_page_metadata(
            &mut files,
            &wire.truncated_hunks,
            wire.first_file_context_only,
        );
        let expected_files = wire.returned_file_records
            + usize::from(wire.first_file_context_only && wire.returned_hunks > 0);
        if parser_truncated
            || parsed_hunks != wire.returned_hunks
            || marked_hunks != wire.returned_hunks
            || files.len() != expected_files
            || wire
                .line_ceiling_hunks
                .iter()
                .any(|index| !wire.truncated_hunks.contains(index))
            || wire
                .line_recoverable_hunks
                .iter()
                .any(|index| !wire.line_ceiling_hunks.contains(index))
            || (!wire.hunk_line_limit && !wire.line_recoverable_hunks.is_empty())
        {
            return git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "source_observation_unavailable",
                output.exit_code,
                &stderr,
            );
        }
        let mut truncation_reasons = Vec::new();
        if wire.page_hunk_limit {
            truncation_reasons.push("page_hunk_limit");
        }
        if wire.hunk_line_limit {
            truncation_reasons.push("hunk_line_limit");
        }
        if wire.page_byte_budget {
            truncation_reasons.push("page_byte_budget");
        }
        let next_continuation = if wire.has_more {
            let encoded = if committed_scope.is_some() {
                encode_git_diff_hunks_committed_continuation(
                    self.git_diff_hunks_continuation_mac_key.as_ref(),
                    &scope,
                    &wire.pre_fence,
                    wire.next_position,
                )
            } else {
                encode_git_diff_hunks_continuation(&scope, &wire.pre_fence, wire.next_position)
            };
            match encoded {
                Ok(value) => Some(value),
                Err(_) => {
                    return git_diff_hunks_failure(
                        &project,
                        &paths,
                        cached,
                        "source_observation_unavailable",
                        output.exit_code,
                        &stderr,
                    )
                }
            }
        } else {
            None
        };
        let recovery = git_diff_hunks_recovery_value(
            &project,
            &paths,
            cached,
            committed_scope.as_ref(),
            max_hunks,
            max_hunk_lines,
            &files,
            wire.page_hunk_limit,
            wire.hunk_line_limit,
            wire.page_byte_budget,
            &wire.truncated_hunks,
            &wire.line_ceiling_hunks,
            &wire.line_recoverable_hunks,
            next_continuation.as_deref(),
        );
        let mut payload = json!({
            "project": project,
            "paths": paths,
            "cached": cached,
            "files": files,
            "hunk_count": parsed_hunks,
            "truncated": !truncation_reasons.is_empty(),
            "truncation_reasons": truncation_reasons,
            "has_more": wire.has_more,
            "next_continuation": next_continuation,
            "exit_code": wire.diff_exit,
            "stderr": stderr,
        });
        if let Some(recovery) = recovery {
            payload["recovery"] = recovery;
        }
        if let Some(committed_scope) = committed_scope.as_ref() {
            if let Some(payload) = payload.as_object_mut() {
                payload.insert(
                    "scope".to_string(),
                    committed_git_diff_hunks_scope_value(committed_scope),
                );
            }
        }
        let result = ToolResult::ok(payload);
        if serde_json::to_vec(&result)
            .map(|bytes| {
                bytes.len()
                    <= MAX_SERIALIZED_OUTPUT_BYTES
                        .saturating_sub(MODEL_RESULT_ENVELOPE_RESERVE_BYTES)
            })
            .unwrap_or(false)
        {
            result
        } else {
            git_diff_hunks_failure(
                &project,
                &paths,
                cached,
                "output_budget_exceeded",
                Some(wire.diff_exit),
                "",
            )
        }
    }

    pub(crate) async fn git_log(
        &self,
        project: String,
        limit: Option<usize>,
        skip: Option<usize>,
    ) -> ToolResult {
        let limit = normalize_git_log_limit(limit);
        let skip = normalize_git_log_skip(skip);
        let command = git_log_command(limit, skip);
        let output = match self
            .run_project_command_capture(&project, command, 30, None)
            .await
        {
            Ok(output) => output,
            Err(e) => return ToolResult::err(e),
        };
        let (commits, truncated) = parse_git_log_commits(&output.stdout, limit);
        let payload = json!({
            "project": project,
            "limit": limit,
            "skip": skip,
            "count": commits.len(),
            "truncated": truncated,
            "commits": commits,
        });
        if output.exit_code == Some(0) || git_log_empty_repo(&output.stderr) {
            ToolResult::ok(payload)
        } else {
            ToolResult {
                success: false,
                output: json!({
                    "project": payload["project"],
                    "limit": payload["limit"],
                    "skip": payload["skip"],
                    "count": payload["count"],
                    "truncated": payload["truncated"],
                    "commits": payload["commits"],
                    "exit_code": output.exit_code,
                    "stderr": output.stderr,
                }),
                error: Some("git log failed".to_string()),
            }
        }
    }

    pub(crate) async fn git_commit_paths(
        &self,
        project: String,
        expected_head: String,
        paths: Vec<String>,
        message: String,
    ) -> ToolResult {
        let (expected_head, paths, message) =
            match validate_git_commit_paths_input(&expected_head, &paths, &message) {
                Ok(values) => values,
                Err(error) => return ToolResult::err(error),
            };
        let proj = match self.resolve_project(&project).await {
            Ok(project) => project,
            Err(error) => return ToolResult::err(error),
        };
        let script = git_commit_paths_script(&expected_head, &paths, &message);
        let client_id = proj.client_id.clone();
        let (request_id, rx) = match self
            .runner_registry
            .enqueue_internal_posix_script(
                client_id,
                Some(proj.path.clone()),
                script,
                60,
                60,
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(request) => request,
            Err(error) => {
                return ToolResult::err_with_output(
                    error,
                    git_commit_paths_failure_output(&expected_head, "runner_rejected", None),
                )
            }
        };

        match tokio::time::timeout(Duration::from_secs(64), rx).await {
            Ok(Ok(response)) => {
                let lifecycle = runner_command_lifecycle(&response, 60);
                if lifecycle == ShellCommandExecutionState::NotStarted {
                    return ToolResult::err_with_output(
                        response
                            .error
                            .unwrap_or_else(|| "git commit request was not started".to_string()),
                        git_commit_paths_failure_output(&expected_head, "not_started", None),
                    );
                }
                if matches!(
                    lifecycle,
                    ShellCommandExecutionState::OutcomeUnknown
                        | ShellCommandExecutionState::TimedOut
                ) {
                    return git_commit_paths_outcome_unknown(
                        &expected_head,
                        response.error.as_deref().unwrap_or(
                            "Runner did not provide a trustworthy terminal commit result",
                        ),
                    );
                }

                let stdout = response.stdout.unwrap_or_default();
                let Some(marker) = parse_git_commit_marker(&stdout) else {
                    return git_commit_paths_outcome_unknown(
                        &expected_head,
                        "completed Runner response omitted the structured commit marker",
                    );
                };
                match marker.status.as_str() {
                    "success"
                        if response.exit_code == Some(0)
                            && marker.previous_head.as_deref() == Some(expected_head.as_str())
                            && marker.new_head.is_some() =>
                    {
                        let new_head = marker.new_head.expect("checked above");
                        ToolResult::ok(json!({
                            "committed": true,
                            "expected_head": expected_head,
                            "previous_head": expected_head,
                            "actual_head": new_head,
                            "new_head": new_head,
                            "committed_paths": paths,
                            "state_changed": true,
                            "outcome_unknown": false,
                            "failure_kind": null,
                            "hook_policy": "bypassed_exact_tree",
                        }))
                    }
                    "index_cleanup_failed"
                        if marker.previous_head.as_deref() == Some(expected_head.as_str())
                            && marker.new_head.is_some() =>
                    {
                        let new_head = marker.new_head.expect("checked above");
                        ToolResult::err_with_output(
                            "commit was created and HEAD advanced, but the real index could not be aligned to the new commit; do not retry the commit",
                            json!({
                                "committed": true,
                                "expected_head": expected_head,
                                "previous_head": expected_head,
                                "actual_head": new_head,
                                "new_head": new_head,
                                "committed_paths": paths,
                                "state_changed": true,
                                "outcome_unknown": false,
                                "failure_kind": "index_cleanup_failed",
                                "hook_policy": "bypassed_exact_tree",
                            }),
                        )
                        .with_recovery(RecoveryKind::Reobserve, None)
                    }
                    "success" | "index_cleanup_failed" => git_commit_paths_outcome_unknown(
                        &expected_head,
                        "Runner returned a mutation-like commit marker with inconsistent SHA or exit-code evidence",
                    ),
                    status => ToolResult::err_with_output(
                        format!("git_commit_paths rejected or failed: {status}"),
                        git_commit_paths_failure_output(&expected_head, status, marker.actual_head),
                    ),
                }
            }
            Ok(Err(_)) => {
                let dispatch = self
                    .runner_registry
                    .cancel_request_dispatch_state(&request_id)
                    .await;
                if dispatch_uncertainty_lifecycle(dispatch)
                    == ShellCommandExecutionState::NotStarted
                {
                    ToolResult::err_with_output(
                        "git commit request waiter was dropped before Runner dispatch",
                        git_commit_paths_failure_output(&expected_head, "not_started", None),
                    )
                } else {
                    git_commit_paths_outcome_unknown(
                        &expected_head,
                        "git commit request waiter was dropped after dispatch may have occurred",
                    )
                }
            }
            Err(_) => {
                let dispatch = self
                    .runner_registry
                    .cancel_request_dispatch_state(&request_id)
                    .await;
                if dispatch_uncertainty_lifecycle(dispatch)
                    == ShellCommandExecutionState::NotStarted
                {
                    ToolResult::err_with_output(
                        "timed out before git commit request reached the Runner",
                        git_commit_paths_failure_output(&expected_head, "not_started", None),
                    )
                } else {
                    git_commit_paths_outcome_unknown(
                        &expected_head,
                        "timed out after Runner dispatch may have occurred",
                    )
                }
            }
        }
    }

    pub(crate) async fn git_diff_summary(&self, project: String) -> ToolResult {
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let cmd = git_diff_summary_command();
        let client_id = proj.client_id.clone();
        let (req_id, rx) = match self
            .runner_registry
            .enqueue_internal_posix_script(
                client_id,
                Some(proj.path.clone()),
                cmd,
                30,
                32,
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(e),
        };
        match tokio::time::timeout(Duration::from_secs(34), rx).await {
            Ok(Ok(resp)) => {
                let stdout = resp.stdout.unwrap_or_default();
                let (porcelain, diff_stat) = split_diff_summary(&stdout);
                let porcelain_summary = parse_porcelain_summary(&porcelain);
                ToolResult::ok(json!({
                    "porcelain": porcelain,
                    "diff_stat": diff_stat,
                    "changed_files": porcelain_summary.changed_files,
                    "changed_files_count": porcelain_summary.changed_files_count,
                    "tracked_changed_files": porcelain_summary.tracked_changed_files,
                    "untracked_files": porcelain_summary.untracked_files,
                    "ignored_files": porcelain_summary.ignored_files,
                    "exit_code": resp.exit_code,
                }))
            }
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&req_id).await;
                ToolResult::err("request dropped")
            }
            Err(_) => {
                self.runner_registry.cancel_request(&req_id).await;
                ToolResult::err("timed out")
            }
        }
    }

    pub(crate) async fn show_changes(
        &self,
        project: String,
        session_id: Option<String>,
        include_diff: Option<bool>,
        max_hunks: Option<usize>,
        max_hunk_lines: Option<usize>,
        session_event_limit: Option<usize>,
    ) -> ToolResult {
        let include_diff = include_diff.unwrap_or(false);
        let max_hunks = max_hunks
            .filter(|n| *n > 0)
            .unwrap_or(SHOW_CHANGES_DEFAULT_MAX_HUNKS)
            .min(SHOW_CHANGES_MAX_HUNKS);
        let max_hunk_lines = max_hunk_lines
            .filter(|n| *n > 0)
            .unwrap_or(SHOW_CHANGES_DEFAULT_MAX_HUNK_LINES)
            .min(SHOW_CHANGES_MAX_HUNK_LINES);
        let session_event_limit = session_event_limit
            .filter(|n| *n > 0)
            .unwrap_or(SHOW_CHANGES_DEFAULT_SESSION_EVENT_LIMIT)
            .min(SHOW_CHANGES_MAX_SESSION_EVENT_LIMIT);
        let command = show_changes_command(include_diff, max_hunks, max_hunk_lines);
        let output = match self
            .run_project_internal_posix_script_capture(&project, command, 30, None)
            .await
        {
            Ok(output) => output,
            Err(e) => return ToolResult::err(e),
        };
        let frames = split_show_changes_stdout(&output.stdout, include_diff);
        let status_observation = parse_show_changes_status_observation(
            &frames.status,
            &frames.status_result,
            &output.stderr,
        );
        let effective_exit_code = if status_observation.exit_code != Some(0) {
            status_observation.exit_code
        } else {
            output.exit_code
        };
        // Graceful degradation is reserved for a dedicated repository probe
        // that explicitly reports an outside-worktree directory. Status
        // configuration, permission, or execution failures remain distinct.
        if status_observation.non_git() {
            let mut payload = non_git_show_changes_payload_with_observation(
                &project,
                include_diff,
                status_observation,
            );
            let session_summary = session_id
                .as_deref()
                .and_then(|id| self.sessions.summary(id, Some(session_event_limit)));
            apply_show_changes_session(&mut payload, session_id.as_deref(), session_summary);
            return ToolResult::ok(payload);
        }
        let status_observed = status_observation.status_observed();
        let mut payload = parse_show_changes_output_with_observation(
            &project,
            &frames.status,
            &frames.head,
            &frames.stat,
            include_diff.then_some(frames.diff.as_str()),
            max_hunks,
            max_hunk_lines,
            effective_exit_code,
            &output.stderr,
            status_observation,
            &frames,
        );
        if include_diff {
            let untracked_paths = show_changes_untracked_paths(&payload);
            let (previews, truncated) = self
                .collect_show_changes_untracked_previews(&project, &untracked_paths)
                .await;
            payload["untracked_previews"] = json!(previews);
            payload["untracked_previews_truncated"] = json!(truncated);
        }
        let session_summary = session_id
            .as_deref()
            .and_then(|id| self.sessions.summary(id, Some(session_event_limit)));
        apply_show_changes_session(&mut payload, session_id.as_deref(), session_summary);
        // Success requires the command/result envelope, status, diff-stat, and
        // (when requested) full diff inspections all to be proven successful.
        // `transport_safe` only describes bounded transport integrity and must
        // never mask an observed or unavailable inspection failure.
        let diff_stat_ok = frames.diff_stat_exit == Some(0);
        let diff_ok = if include_diff {
            // `diff_exit == None` means the exit code could not be captured
            // (e.g. legacy/transport-truncated stdout); treat as not proven-ok.
            frames.diff_exit == Some(0)
        } else {
            true
        };
        if status_observed && output.exit_code == Some(0) && diff_stat_ok && diff_ok {
            ToolResult::ok(payload)
        } else {
            let error = if !status_observed {
                "show_changes git status unavailable".to_string()
            } else if !diff_stat_ok {
                match frames.diff_stat_exit {
                    Some(code) => format!(
                        "show_changes git diff-stat inspection failed with exit code {code}"
                    ),
                    None => "show_changes git diff-stat inspection unavailable".to_string(),
                }
            } else if include_diff && !diff_ok {
                match frames.diff_exit {
                    Some(code) => {
                        format!("show_changes git diff inspection failed with exit code {code}")
                    }
                    None => "show_changes git diff inspection unavailable".to_string(),
                }
            } else {
                "show_changes git inspection failed".to_string()
            };
            ToolResult {
                success: false,
                output: payload,
                error: Some(error),
            }
        }
    }
}
