use super::*;

#[cfg(test)]
pub(crate) fn read_file_content_result(
    content: String,
    start_line: Option<usize>,
    limit: Option<usize>,
) -> ToolResult {
    read_file_content_result_with_options(content, start_line, limit, false)
}

#[cfg(test)]
pub(crate) fn read_file_content_result_with_options(
    content: String,
    start_line: Option<usize>,
    limit: Option<usize>,
    with_line_numbers: bool,
) -> ToolResult {
    // The local path previously kept the entire file body in `content` and then
    // sliced it. It now feeds the bytes through the shared streaming range
    // reader so only the selected window is retained, and the model output is
    // built by the shared normalizer shared with the Runner path.
    let range = EffectiveRange::new(start_line, limit);
    match file_read_range::read_range_from(content.as_bytes(), range) {
        Ok(result) => build_read_file_success(&result, with_line_numbers, None),
        Err(error) => read_file_failure(error.reason, None),
    }
}

#[cfg(test)]
pub(crate) fn read_file_runner_stdout_result(
    stdout: String,
    start_line: Option<usize>,
    limit: Option<usize>,
) -> ToolResult {
    read_file_runner_stdout_result_with_options(stdout, start_line, limit, false)
}

pub(crate) fn read_file_runner_stdout_result_with_options(
    stdout: String,
    start_line: Option<usize>,
    limit: Option<usize>,
    with_line_numbers: bool,
) -> ToolResult {
    // The runner stdout is untrusted input. The shared v1 envelope is accepted,
    // but every formal field is strictly validated and the model output is
    // reconstructed from those fields alone — no envelope extras, padding, or
    // absolute paths are passed through.
    match parse_runner_file_read_range(&stdout, start_line, limit) {
        Ok(result) => build_read_file_success(&result, with_line_numbers, None),
        Err(reason) => read_file_failure(reason, None),
    }
}

pub(crate) fn effective_read_file_range(
    start_line: Option<usize>,
    limit: Option<usize>,
) -> (usize, usize, usize) {
    let range = EffectiveRange::new(start_line, limit);
    (range.start_line, range.limit, range.end_line())
}

/// Build the unified `read_file` success [`ToolResult`] from a shared range
/// result, enforcing the final serialized-output hard limit after JSON
/// escaping and (optional) line numbering. The model output is reconstructed
/// from the canonical fields only; no Runner envelope extras survive.
///
/// `path` is the project-relative input path to attach for model navigation.
fn build_read_file_success(
    result: &FileReadRange,
    with_line_numbers: bool,
    path: Option<&str>,
) -> ToolResult {
    let mut output =
        webcodex_workspace::file_read_normalize::success_output(result, with_line_numbers);
    if let Some(path) = path {
        output["path"] = json!(path);
    }
    // Re-check the hard serialized limit after numbering and JSON escaping.
    // The content budget already bounds the raw range, but a heavily escaped or
    // numbered payload could still grow; fail closed rather than truncate.
    if !webcodex_workspace::file_read_normalize::serialized_fits(&output) {
        return read_file_failure(ReadFileReason::RangeTooLarge, path);
    }
    ToolResult::ok(output)
}

/// Stable, schema-backed failure output for `read_file`. Carries only the
/// project-relative input path and a stable reason code — never absolute paths,
/// raw OS error text, or runner stdout/stderr.
fn read_file_failure(reason: ReadFileReason, path: Option<&str>) -> ToolResult {
    let output = json!({
        "error_kind": "read_file_failed",
        "reason_code": reason.as_str(),
        "path": path.unwrap_or(""),
        "state_changed": false,
    });
    ToolResult::err_with_output(format!("read_file failed: {}", reason.as_str()), output)
}

fn validate_read_file_path(path: &str) -> Option<ToolResult> {
    if validate_project_relative_path(path).is_err() {
        return Some(read_file_failure(ReadFileReason::InvalidPath, Some(path)));
    }
    if crate::sensitive_paths::is_secret_path(path) {
        return Some(read_file_failure(ReadFileReason::SensitivePath, Some(path)));
    }
    None
}

#[cfg(test)]
fn io_error_reason(error: &std::io::Error) -> ReadFileReason {
    match error.kind() {
        std::io::ErrorKind::NotFound => ReadFileReason::NotFound,
        std::io::ErrorKind::PermissionDenied => ReadFileReason::PermissionDenied,
        _ => ReadFileReason::IoError,
    }
}

/// Map a non-zero Runner execution response to a stable reason code. Current
/// Runners emit `read_file failed: <reason_code>`; only a narrow set of legacy
/// path-free phrases is retained for rolling upgrades. Unrecognized text fails
/// closed as `io_error` and is never returned to the model.
fn map_runner_read_error(resp: &ShellRunResponse) -> ReadFileReason {
    for text in [resp.error.as_deref(), resp.stderr.as_deref()]
        .into_iter()
        .flatten()
    {
        for line in text.lines() {
            if let Some(code) = line.trim().strip_prefix("read_file failed: ") {
                if let Some(reason) = ReadFileReason::from_code(code.trim()) {
                    return reason;
                }
            }
        }
    }

    let text = resp
        .error
        .as_deref()
        .or(resp.stderr.as_deref())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match text.as_str() {
        "file_read target not found" | "file_read target is unavailable" => {
            ReadFileReason::NotFound
        }
        "file_read permission denied" | "permission denied" => ReadFileReason::PermissionDenied,
        "file is not valid utf-8" | "file is not valid utf8" => ReadFileReason::InvalidUtf8,
        "range output too large" => ReadFileReason::RangeTooLarge,
        "file_read path escapes project root" | "invalid line range for file_read" => {
            ReadFileReason::InvalidPath
        }
        "file_read target is not a file" | "not a regular file" => ReadFileReason::NotFile,
        "file_read io error" | "file_read project root is unavailable" => ReadFileReason::IoError,
        _ if text.starts_with("range output too large:") => ReadFileReason::RangeTooLarge,
        _ => ReadFileReason::IoError,
    }
}

/// Strictly validate a Runner `webcodex.file_read_range.v1` stdout envelope and
/// return a shared [`FileReadRange`] reconstructed from its formal fields alone.
/// Returns a stable [`ReadFileReason`] for any malformed, mistyped, inconsistent,
/// or oversized response so the caller can fail closed without leaking runner
/// internals.
fn parse_runner_file_read_range(
    stdout: &str,
    request_start_line: Option<usize>,
    request_limit: Option<usize>,
) -> Result<FileReadRange, ReadFileReason> {
    let effective = EffectiveRange::new(request_start_line, request_limit);
    let trimmed = stdout.trim();
    let value = serde_json::from_str::<Value>(trimmed)
        .map_err(|_| ReadFileReason::MalformedRunnerResponse)?;
    if value.get("format").and_then(|f| f.as_str()) != Some("webcodex.file_read_range.v1") {
        return Err(ReadFileReason::MalformedRunnerResponse);
    }

    let content = value
        .get("content")
        .and_then(|c| c.as_str())
        .ok_or(ReadFileReason::MalformedRunnerResponse)?
        .to_string();
    let sha256 = value
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|s| file_read_range::is_valid_sha256_hex(s))
        .ok_or(ReadFileReason::MalformedRunnerResponse)?
        .to_string();
    let total_lines = value
        .get("total_lines")
        .and_then(Value::as_u64)
        .filter(|t| *t <= usize::MAX as u64)
        .map(|t| t as usize)
        .ok_or(ReadFileReason::MalformedRunnerResponse)?;
    let resp_start_line = value
        .get("start_line")
        .and_then(Value::as_u64)
        .filter(|l| *l >= 1 && *l <= usize::MAX as u64)
        .map(|l| l as usize)
        .ok_or(ReadFileReason::MalformedRunnerResponse)?;
    let resp_limit = value
        .get("limit")
        .and_then(Value::as_u64)
        .filter(|l| *l >= 1 && *l <= usize::MAX as u64)
        .map(|l| l as usize)
        .ok_or(ReadFileReason::MalformedRunnerResponse)?;

    // The response must match the effective request range the server sent.
    if resp_start_line != effective.start_line || resp_limit != effective.limit {
        return Err(ReadFileReason::MalformedRunnerResponse);
    }

    // Compute the expected returned line count from the range and total, then
    // verify the content's segment count matches exactly. `split('\n')`
    // preserves empty segments so a single blank line is one segment, not zero.
    let expected_returned = if effective.start_line > total_lines {
        0
    } else {
        total_lines
            .saturating_sub(effective.start_line)
            .saturating_add(1)
            .min(effective.limit)
    };
    let content_segments = content.split('\n').count();
    let content_returned = if content.is_empty() {
        // Disambiguate: empty content is 0 segments for an empty file or
        // overflow, but 1 segment for a single selected blank line.
        if expected_returned == 1 {
            1
        } else {
            0
        }
    } else {
        content_segments
    };
    if content_returned != expected_returned {
        return Err(ReadFileReason::MalformedRunnerResponse);
    }

    // Reject oversized Runner content before it can reach the model output.
    if content.len() > file_read_range::MAX_RANGE_CONTENT_BYTES {
        return Err(ReadFileReason::RangeTooLarge);
    }

    let end_line = if content_returned > 0 {
        Some(effective.start_line + content_returned - 1)
    } else {
        None
    };
    let has_more = end_line.is_some_and(|end| end < total_lines);
    let next_start_line = if has_more {
        end_line.map(|e| e + 1)
    } else {
        None
    };

    Ok(FileReadRange {
        content,
        sha256,
        total_lines,
        start_line: effective.start_line,
        limit: effective.limit,
        returned_lines: content_returned,
        end_line,
        has_more,
        next_start_line,
    })
}

/// Parse the stdout of a best-effort Runner `file_read` for an instruction
/// candidate. Only the canonical `webcodex.file_read_range.v1` JSON envelope
/// is accepted. Empty content is a successfully observed absent rule body;
/// malformed or obsolete output is conservatively unavailable.
fn parse_instruction_runner_stdout(
    stdout: String,
) -> Result<Option<(String, usize, Option<String>)>, ()> {
    let trimmed = stdout.trim();
    if !trimmed.is_empty() {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if value.get("format").and_then(|format| format.as_str())
                == Some("webcodex.file_read_range.v1")
            {
                let content = value
                    .get("content")
                    .and_then(|c| c.as_str())
                    .ok_or(())?
                    .to_string();
                let total_lines = value
                    .get("total_lines")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0) as usize;
                if content_is_empty_instruction(&content) {
                    return Ok(None);
                }
                let full_sha256 = value
                    .get("sha256")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                return Ok(Some((content, total_lines, full_sha256)));
            }
        }
    }
    Err(())
}

/// True when an instruction body carries no meaningful content (empty or
/// whitespace-only). Empty instruction files are skipped so a later candidate
/// can win.
fn content_is_empty_instruction(content: &str) -> bool {
    content.trim().is_empty()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstructionAgentsAliasResolution {
    KeepBoth,
    KeepUppercase,
    KeepLowercase,
}

fn instruction_agents_alias_resolution(root_listing: &str) -> InstructionAgentsAliasResolution {
    let mut uppercase = false;
    let mut lowercase = false;
    for line in root_listing.lines() {
        match line.trim_end_matches('\r') {
            "AGENTS.md" => uppercase = true,
            "agents.md" => lowercase = true,
            _ => {}
        }
    }
    match (uppercase, lowercase) {
        (true, false) => InstructionAgentsAliasResolution::KeepUppercase,
        (false, true) => InstructionAgentsAliasResolution::KeepLowercase,
        _ => InstructionAgentsAliasResolution::KeepBoth,
    }
}

enum InstructionCandidateRead {
    Found(super::project_instructions::LoadedInstructionCandidate),
    Missing,
    Unavailable,
}

fn instruction_candidate_missing(error: &Option<String>, stderr: &Option<String>) -> bool {
    error
        .iter()
        .chain(stderr.iter())
        .map(|value| value.to_ascii_lowercase())
        .any(|value| {
            value.contains("no such file")
                || value.contains("not found")
                || value.contains("not_found")
                || value.contains("cannot find the file")
        })
}

// =============================================================================
// Phase A read-only console helpers
// =============================================================================

/// Build the project-relative path for a single entry returned by a Runner
/// `file_list` op. `rel_path` is the project-relative directory the caller
/// requested (`"."` for the project root); `name` is the bare entry name.
pub(crate) fn relative_entry_path(rel_path: &str, name: &str) -> String {
    let trimmed = rel_path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        name.to_string()
    } else {
        format!("{}/{}", trimmed, name)
    }
}

/// Parse Runner `file_list` stdout (one entry per line, dirs suffixed with
/// `/`) into bounded project-relative entries with a file/dir kind. Returns
/// the entries and whether the source exceeded `max_entries`.
pub(crate) fn parse_file_list_entries(
    stdout: &str,
    rel_path: &str,
    max_entries: usize,
) -> (Vec<Value>, bool) {
    let mut all: Vec<Value> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (name, is_dir) = if let Some(stripped) = line.strip_suffix('/') {
            (stripped.to_string(), true)
        } else {
            (line.to_string(), false)
        };
        if name.is_empty() {
            continue;
        }
        all.push(json!({
            "path": relative_entry_path(rel_path, &name),
            "kind": if is_dir { "dir" } else { "file" },
        }));
    }
    all.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap_or("")
            .cmp(b["path"].as_str().unwrap_or(""))
    });
    let truncated = all.len() > max_entries;
    all.truncate(max_entries);
    (all, truncated)
}

/// Wall-clock budget for one tracked-file listing. `git ls-files` reads the
/// index and does no tree walk, so a project that needs longer than this is
/// reporting a sick repository, not a big one.
const LIST_TRACKED_TIMEOUT_SECS: u64 = 20;

/// Transport cap on raw `git ls-files -z` output. Roughly 25k paths at typical
/// lengths; beyond it the listing reports `list_truncated` rather than
/// pretending the index ended there.
const LIST_TRACKED_MAX_BYTES: usize = 1024 * 1024;

/// Structured failure shaped like the other file-tool errors, so a caller can
/// branch on `code` instead of matching prose.
fn list_tracked_error(code: &str, message: String) -> ToolResult {
    ToolResult::err_with_output(message.clone(), json!({ "code": code, "message": message }))
}

/// Bounded multi-line command stderr for tracked-file diagnostics. Keep the
/// tail because shell parsers commonly put the actionable diagnostic after a
/// location header. `bounded_tail` is UTF-8 safe and caps retained stderr even
/// when a subprocess emits arbitrarily large diagnostics.
pub(crate) const LIST_TRACKED_STDERR_MAX_CHARS: usize = 4 * 1024;

fn list_tracked_stderr_excerpt(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return "(no stderr)".to_string();
    }
    bounded_tail(stderr, LIST_TRACKED_STDERR_MAX_CHARS).0
}

/// Build the tracked-file listing command.
///
/// `git rev-parse` guards the pipeline so "not a repository" arrives as a
/// distinct exit code rather than as an empty listing that looks like an empty
/// project. Exit contract: 2 = no usable `head`, 3 = not a Git repository.
pub(crate) fn list_tracked_files_command(scope: &str) -> String {
    list_tracked_files_command_with_head_fallbacks(scope, DEFAULT_SEARCH_HEAD_ABSOLUTE_CANDIDATES)
}

pub(crate) fn list_tracked_files_command_with_head_fallbacks(
    scope: &str,
    absolute_head_candidates: &[&str],
) -> String {
    let head_setup = search_head_resolution_shell(absolute_head_candidates);
    // `:(literal)` disables pathspec globbing, so a directory named `*` or
    // `[a]` scopes to itself instead of matching siblings.
    let pathspec = if scope.is_empty() {
        String::new()
    } else {
        format!(
            " -- {}",
            shell_escape_simple(&format!(":(literal){}", scope.trim_end_matches('/')))
        )
    };
    format!(
        r#"{head_setup}if git rev-parse --git-dir >/dev/null 2>&1; then
  git ls-files -z --cached{pathspec} | "$head_cmd" -c {LIST_TRACKED_MAX_BYTES}
else
  exit 3
fi"#
    )
}

impl ToolRuntime {
    pub(crate) async fn read_file(
        &self,
        project: String,
        path: String,
        start_line: Option<usize>,
        limit: Option<usize>,
        with_line_numbers: Option<bool>,
    ) -> ToolResult {
        let with_line_numbers = with_line_numbers.unwrap_or(false);
        // Bound the request to the project before it can reach an executor.
        // The Runner branch below forwards `path` to a remote host that scopes
        // file ops to `allowed_roots` — which is broader than the project — so
        // the project boundary has to be enforced here, as `list_project_files`
        // and `project_overview` already do.
        if let Some(failure) = validate_read_file_path(&path) {
            return failure;
        }
        // Every other surface already refuses credentials: search excludes
        // them, artifacts and edits reject them. Reading was the one way left
        // to get a `.env` or a private key back verbatim. Only the narrow
        // secret policy applies here — reading `.git/HEAD` or a file under
        // `target/` by explicit path stays allowed.
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        self.read_one_validated_project_file(
            &proj,
            path,
            start_line,
            limit,
            with_line_numbers,
            None,
        )
        .await
    }

    pub(crate) async fn read_file_resolved(
        &self,
        resolved: &ResolvedProject,
        path: String,
        start_line: Option<usize>,
        limit: Option<usize>,
        with_line_numbers: Option<bool>,
    ) -> ToolResult {
        let with_line_numbers = with_line_numbers.unwrap_or(false);
        // Reuse the same path/sensitive checks as the legacy direct helper,
        // but consume the authoritative Project resolved by dispatch instead
        // of performing another registry lookup.
        if let Some(failure) = validate_read_file_path(&path) {
            return failure;
        }
        self.read_one_validated_project_file(
            &resolved.config,
            path,
            start_line,
            limit,
            with_line_numbers,
            None,
        )
        .await
    }

    pub(crate) async fn read_one_resolved_project_file(
        &self,
        project: &ProjectConfig,
        path: String,
        start_line: Option<usize>,
        limit: Option<usize>,
        with_line_numbers: bool,
        deadline: Instant,
    ) -> ToolResult {
        if Instant::now() >= deadline {
            return read_file_failure(ReadFileReason::Timeout, Some(&path));
        }
        if let Some(failure) = validate_read_file_path(&path) {
            return failure;
        }
        self.read_one_validated_project_file(
            project,
            path,
            start_line,
            limit,
            with_line_numbers,
            Some(deadline),
        )
        .await
    }

    async fn read_one_validated_project_file(
        &self,
        proj: &ProjectConfig,
        path: String,
        start_line: Option<usize>,
        limit: Option<usize>,
        with_line_numbers: bool,
        deadline: Option<Instant>,
    ) -> ToolResult {
        let client_id = proj.client_id.clone();
        let wait_timeout = 30;
        let (eff_start, _eff_limit, eff_end) = effective_read_file_range(start_line, limit);
        let (request_id, rx) = match self
            .runner_registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: "read".to_string(),
                    client_id,
                    path: path.clone(),
                    cwd: Some(proj.path.clone()),
                    content: None,
                    // The shared scanner bounds raw content. The Runner
                    // preflights the complete v1 envelope against this cap,
                    // and ToolRuntime separately checks the final model
                    // output after numbering and JSON escaping.
                    max_bytes: Some(512 * 1024),
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: Some(eff_start),
                    end_line: Some(eff_end),
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: wait_timeout,
                },
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(r) => r,
            Err(_) => return read_file_failure(ReadFileReason::RunnerUnavailable, Some(&path)),
        };
        let response = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, rx).await,
            None => tokio::time::timeout(Duration::from_secs(wait_timeout + 2), rx).await,
        };
        match response {
            Ok(Ok(resp)) if resp.exit_code == Some(0) && resp.error.is_none() => {
                let mut result = read_file_runner_stdout_result_with_options(
                    resp.stdout.unwrap_or_default(),
                    start_line,
                    limit,
                    with_line_numbers,
                );
                if result.success {
                    result.output["path"] = json!(path);
                    result
                } else {
                    // The Runner response failed validation; surface a stable
                    // structured error using the project-relative path.
                    result.output["path"] = json!(path);
                    result
                }
            }
            // Map Runner execution failures to stable reason codes. The
            // raw `resp.error`/`resp.stderr` (which may carry the runner's
            // absolute path) is never forwarded to the model.
            Ok(Ok(resp)) => {
                let reason = map_runner_read_error(&resp);
                read_file_failure(reason, Some(&path))
            }
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                read_file_failure(ReadFileReason::RunnerUnavailable, Some(&path))
            }
            Err(_) => {
                self.runner_registry.cancel_request(&request_id).await;
                read_file_failure(ReadFileReason::Timeout, Some(&path))
            }
        }
    }

    // -------------------------------------------------------------------------
    // Project instructions auto-load (best-effort, session-start guidance)
    // -------------------------------------------------------------------------

    /// Best-effort load of project-local instruction files
    /// (`project_instructions::INSTRUCTION_CANDIDATE_PATHS`) for a resolved
    /// project. Candidates are tried in fixed order; the first candidate that
    /// reads successfully wins, bounding Runner round-trips. Any read failure
    /// (Runner not connected, file missing, timeout, decode error) is swallowed
    /// and the next candidate is tried. Returns an empty (`loaded=false`)
    /// snapshot when no candidate could be read.
    ///
    /// This never records session events (the session does not exist yet) and
    /// never fails `start_session`.
    pub(crate) async fn load_project_instructions(
        &self,
        config: &ProjectConfig,
    ) -> super::project_instructions::ProjectInstructionsSnapshot {
        use super::project_instructions::{
            ProjectInstructionsSnapshot, INSTRUCTION_CANDIDATE_PATHS,
        };
        let mut scan_complete = true;
        for candidate in INSTRUCTION_CANDIDATE_PATHS {
            match self.read_instruction_candidate(config, candidate).await {
                InstructionCandidateRead::Found(candidate) => {
                    return ProjectInstructionsSnapshot::from_candidates(
                        vec![candidate],
                        scan_complete,
                    );
                }
                InstructionCandidateRead::Missing => {}
                InstructionCandidateRead::Unavailable => scan_complete = false,
            }
        }
        if scan_complete {
            ProjectInstructionsSnapshot::empty()
        } else {
            ProjectInstructionsSnapshot::unavailable()
        }
    }

    /// Observe every fixed project-instruction candidate for model-facing
    /// coding startup. Requests run concurrently under the existing per-read
    /// deadline, while the resulting sources retain the fixed candidate order.
    /// This is what lets a Workflow Session detect additions, removals,
    /// content changes, and truncation changes across continuations.
    pub(crate) async fn load_coding_project_instructions(
        &self,
        config: &ProjectConfig,
    ) -> super::project_instructions::ProjectInstructionsSnapshot {
        use super::project_instructions::{
            ProjectInstructionsSnapshot, INSTRUCTION_CANDIDATE_PATHS,
        };
        let reads = INSTRUCTION_CANDIDATE_PATHS
            .iter()
            .map(|candidate| self.read_instruction_candidate(config, candidate));
        let results = futures_util::future::join_all(reads).await;
        let both_agents_spellings_read = results
            .first()
            .is_some_and(|result| matches!(result, InstructionCandidateRead::Found(_)))
            && results
                .get(1)
                .is_some_and(|result| matches!(result, InstructionCandidateRead::Found(_)));
        // On a case-insensitive filesystem both case-variant reads can resolve
        // to one physical file. Only deduplicate when a complete root listing
        // proves which spelling actually exists. If listing fails, or both
        // entries really exist on a case-sensitive filesystem, preserve both
        // fixed sources rather than guessing from content or hashes.
        let agents_alias = if both_agents_spellings_read {
            self.instruction_agents_alias_resolution(config).await
        } else {
            None
        };
        let mut found = Vec::new();
        let mut scan_complete = true;
        for (index, result) in results.into_iter().enumerate() {
            let path = INSTRUCTION_CANDIDATE_PATHS[index];
            let skip_alias = matches!(
                (path, agents_alias),
                (
                    "AGENTS.md",
                    Some(InstructionAgentsAliasResolution::KeepLowercase)
                ) | (
                    "agents.md",
                    Some(InstructionAgentsAliasResolution::KeepUppercase)
                )
            );
            if skip_alias {
                continue;
            }
            match result {
                InstructionCandidateRead::Found(candidate) => found.push(candidate),
                InstructionCandidateRead::Missing => {}
                InstructionCandidateRead::Unavailable => scan_complete = false,
            }
        }
        if found.is_empty() && scan_complete {
            ProjectInstructionsSnapshot::empty()
        } else if found.is_empty() {
            ProjectInstructionsSnapshot::unavailable()
        } else {
            ProjectInstructionsSnapshot::from_candidates(found, scan_complete)
        }
    }

    async fn instruction_agents_alias_resolution(
        &self,
        config: &ProjectConfig,
    ) -> Option<InstructionAgentsAliasResolution> {
        const WAIT_TIMEOUT: u64 = 6;
        let client_id = config.client_id.as_str();
        let (request_id, rx) = self
            .runner_registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: "list".to_string(),
                    client_id: client_id.to_string(),
                    path: ".".to_string(),
                    cwd: Some(config.path.clone()),
                    content: None,
                    max_bytes: None,
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: None,
                    end_line: None,
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: WAIT_TIMEOUT,
                },
                "project_instructions".to_string(),
            )
            .await
            .ok()?;
        match tokio::time::timeout(Duration::from_secs(WAIT_TIMEOUT + 2), rx).await {
            Ok(Ok(resp)) if resp.exit_code == Some(0) && resp.error.is_none() => Some(
                instruction_agents_alias_resolution(resp.stdout.as_deref().unwrap_or_default()),
            ),
            _ => {
                self.runner_registry.cancel_request(&request_id).await;
                None
            }
        }
    }

    /// Read a single instruction candidate from a resolved project. Returns
    /// `(content, total_lines)` on success or `None` on any failure.
    ///
    /// Reads are routed to the owning Runner via the `file_read` op with a short
    /// best-effort timeout.
    async fn read_instruction_candidate(
        &self,
        config: &ProjectConfig,
        path: &str,
    ) -> InstructionCandidateRead {
        use super::project_instructions::{LoadedInstructionCandidate, MAX_LINES_PER_FILE};
        // Request one extra line so canonical envelope total/selection metadata
        // reliably signals truncation beyond the per-file cap.
        let read_limit = MAX_LINES_PER_FILE + 1;
        const WAIT_TIMEOUT: u64 = 6;

        let client_id = config.client_id.as_str();
        let (request_id, rx) = match self
            .runner_registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: "read".to_string(),
                    client_id: client_id.to_string(),
                    path: path.to_string(),
                    cwd: Some(config.path.clone()),
                    content: None,
                    max_bytes: Some(512 * 1024),
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: Some(1),
                    end_line: Some(read_limit),
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: WAIT_TIMEOUT,
                },
                "project_instructions".to_string(),
            )
            .await
        {
            Ok(enqueued) => enqueued,
            Err(_) => return InstructionCandidateRead::Unavailable,
        };
        match tokio::time::timeout(Duration::from_secs(WAIT_TIMEOUT + 2), rx).await {
            Ok(Ok(resp)) if resp.exit_code == Some(0) && resp.error.is_none() => {
                match parse_instruction_runner_stdout(resp.stdout.unwrap_or_default()) {
                    Ok(Some((content, total_lines, full_sha256))) => {
                        InstructionCandidateRead::Found(LoadedInstructionCandidate {
                            path: path.to_string(),
                            content,
                            total_lines,
                            full_sha256,
                        })
                    }
                    Ok(None) => InstructionCandidateRead::Missing,
                    Err(()) => InstructionCandidateRead::Unavailable,
                }
            }
            Ok(Ok(resp)) => {
                if instruction_candidate_missing(&resp.error, &resp.stderr) {
                    InstructionCandidateRead::Missing
                } else {
                    InstructionCandidateRead::Unavailable
                }
            }
            _ => {
                self.runner_registry.cancel_request(&request_id).await;
                InstructionCandidateRead::Unavailable
            }
        }
    }

    /// `list_project_files`: bounded, project-relative file listing routed to
    /// the owning registered Runner via the `file_list` op. The server never
    /// reads the Runner project path directly. Returns `path` + `kind`
    /// (file/dir); size/mtime are not exposed by the current file op protocol.
    pub(crate) async fn list_project_files(
        &self,
        project: String,
        path: Option<String>,
        limit: Option<usize>,
    ) -> ToolResult {
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let rel_path = path
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ".".to_string());
        if let Err(e) = validate_project_relative_path(&rel_path) {
            return ToolResult::err(e);
        }
        let max_entries = limit.unwrap_or(200).clamp(1, 500);
        let client_id = proj.client_id.clone();
        let wait_timeout = 30;
        let (request_id, rx) = match self
            .runner_registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: "list".to_string(),
                    client_id,
                    path: rel_path.clone(),
                    cwd: Some(proj.path.clone()),
                    content: None,
                    max_bytes: None,
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: None,
                    end_line: None,
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: wait_timeout,
                },
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(e),
        };
        match tokio::time::timeout(Duration::from_secs(wait_timeout + 2), rx).await {
            Ok(Ok(resp)) if resp.exit_code == Some(0) && resp.error.is_none() => {
                let stdout = resp.stdout.unwrap_or_default();
                let (entries, truncated) = parse_file_list_entries(&stdout, &rel_path, max_entries);
                ToolResult::ok(json!({
                    "project": project,
                    "path": rel_path,
                    "entries": entries,
                    "truncated": truncated,
                }))
            }
            Ok(Ok(resp)) => ToolResult::err(
                resp.error
                    .or(resp.stderr)
                    .unwrap_or_else(|| "Runner list_project_files failed".to_string()),
            ),
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                ToolResult::err("Runner list_project_files waiter was dropped")
            }
            Err(_) => {
                self.runner_registry.cancel_request(&request_id).await;
                ToolResult::err("timed out waiting for Runner list_project_files")
            }
        }
    }

    /// `list_project_tracked_files`: enumerate what the project actually
    /// contains, using the Git index as the definition of "contains".
    ///
    /// Deliberately not `read_dir` (which `list_project_files` uses): a
    /// filesystem walk of a real project descends into `.venv`, `target`, and
    /// datasets, and reports tool state such as `.opencode/` as project
    /// content. `git ls-files` is the same definition `files_search`, project
    /// detection, and workspace provenance already use, so all four agree on
    /// what belongs to the project.
    ///
    /// The Runner runs one deterministic command; scope, globs, rollup, and
    /// pagination are decided server-side in [`super::file_listing`].
    pub(crate) async fn list_project_tracked_files(
        &self,
        project: String,
        path: Option<String>,
        globs: Option<Vec<String>>,
        depth: Option<usize>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> ToolResult {
        let scope_input = path.as_deref().unwrap_or("").trim().to_string();
        if !scope_input.is_empty() && scope_input != "." {
            if let Err(error) = validate_project_relative_path(&scope_input) {
                return list_tracked_error("invalid_path", error);
            }
        }
        let globs = globs.unwrap_or_default();
        if globs.len() > 20 {
            return list_tracked_error(
                "invalid_globs",
                "at most 20 globs are accepted".to_string(),
            );
        }
        if let Some(bad) = globs
            .iter()
            .find(|glob| glob.is_empty() || glob.len() > 256 || glob.contains('\0'))
        {
            return list_tracked_error(
                "invalid_globs",
                format!("glob must be 1..=256 bytes without NUL: {bad:?}"),
            );
        }
        let scope = super::file_listing::normalize_scope(Some(&scope_input));
        let limit = limit.unwrap_or(200).clamp(1, 1000);
        let offset = offset.unwrap_or(0);
        let depth = depth.map(|depth| depth.clamp(1, 16));

        let proj = match self.resolve_project(&project).await {
            Ok(project) => project,
            Err(error) => return ToolResult::err(error),
        };
        let command = list_tracked_files_command(&scope);
        let client_id = proj.client_id.clone();
        let (raw, exit_code, stderr) = {
            let (request_id, rx) = match self
                .runner_registry
                .enqueue_internal_posix_script(
                    client_id,
                    Some(proj.path.clone()),
                    command,
                    LIST_TRACKED_TIMEOUT_SECS,
                    LIST_TRACKED_TIMEOUT_SECS + 5,
                    "tool_runtime".to_string(),
                )
                .await
            {
                Ok(pending) => pending,
                Err(error) => return ToolResult::err(error),
            };
            match tokio::time::timeout(Duration::from_secs(LIST_TRACKED_TIMEOUT_SECS + 10), rx)
                .await
            {
                Ok(Ok(response)) => (
                    response.stdout.unwrap_or_default(),
                    response.exit_code,
                    response.stderr.unwrap_or_default(),
                ),
                Ok(Err(_)) => {
                    self.runner_registry.cancel_request(&request_id).await;
                    return list_tracked_error(
                        "list_request_dropped",
                        "the Runner request was dropped before a listing arrived".to_string(),
                    );
                }
                Err(_) => {
                    self.runner_registry.cancel_request(&request_id).await;
                    return list_tracked_error(
                        "list_timeout",
                        "timed out waiting for the project file listing".to_string(),
                    );
                }
            }
        };

        // Exit codes are the command's own contract (see
        // `list_tracked_files_command`): 2 = no usable `head`, 3 = not a Git
        // repository, 141 = SIGPIPE once `head` closed a large stream early.
        match exit_code {
            Some(3) => return list_tracked_error(
                "not_a_git_repository",
                "this project is not a Git repository, so its tracked-file index cannot be read"
                    .to_string(),
            ),
            Some(2) => {
                return list_tracked_error(
                    "list_unavailable",
                    "the Runner host has no usable head command to bound the listing".to_string(),
                )
            }
            Some(0) | Some(141) | None => {}
            Some(code) => {
                return list_tracked_error(
                    "list_failed",
                    format!(
                        "listing command failed with exit {code}:\n{}",
                        list_tracked_stderr_excerpt(&stderr)
                    ),
                )
            }
        }

        let (paths, list_truncated) = super::file_listing::parse_nul_separated(&raw);
        let listing =
            super::file_listing::build_listing(&paths, &scope, &globs, depth, limit, offset);
        ToolResult::ok(listing.to_json(&project, &scope, list_truncated))
    }

    /// `project_overview`: deterministic, bounded project metadata routed to
    /// the owning Runner. The server validates inputs and parses the structured
    /// response but never reads the Runner host's project path.
    pub(crate) async fn project_overview(
        &self,
        project: String,
        path: Option<String>,
        max_depth: Option<usize>,
        limit: Option<usize>,
    ) -> ToolResult {
        let rel_path = match normalize_project_overview_path(path.as_deref().unwrap_or("")) {
            Ok(path) => path,
            Err(error) => return ToolResult::err(error),
        };
        let max_depth = effective_project_overview_max_depth(max_depth);
        let limit = effective_project_overview_limit(limit);
        let proj = match self.resolve_project(&project).await {
            Ok(project) => project,
            Err(error) => return ToolResult::err(error),
        };
        let client_id = proj.client_id.clone();
        let wait_timeout = 30;
        let runner_path = if rel_path.is_empty() {
            ".".to_string()
        } else {
            rel_path.clone()
        };
        let (request_id, receiver) = match self
            .runner_registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: "project_overview".to_string(),
                    client_id,
                    path: runner_path,
                    cwd: Some(proj.path.clone()),
                    content: Some(json!({"max_depth": max_depth, "limit": limit}).to_string()),
                    max_bytes: None,
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: None,
                    end_line: None,
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: wait_timeout,
                },
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(request) => request,
            Err(error) => return ToolResult::err(error),
        };
        match tokio::time::timeout(Duration::from_secs(wait_timeout + 2), receiver).await {
            Ok(Ok(response)) if response.exit_code == Some(0) && response.error.is_none() => {
                let mut output: Value =
                    match serde_json::from_str(response.stdout.as_deref().unwrap_or_default()) {
                        Ok(Value::Object(output)) => Value::Object(output),
                        Ok(_) => {
                            return ToolResult::err(
                                "Runner project_overview returned a non-object payload",
                            )
                        }
                        Err(error) => {
                            return ToolResult::err(format!(
                                "Runner project_overview returned invalid JSON: {error}"
                            ))
                        }
                    };
                if output["path"] != rel_path
                    || output["scan"]["max_depth"] != max_depth
                    || output["scan"]["limit"] != limit
                {
                    return ToolResult::err(
                        "Runner project_overview response did not match the requested bounds",
                    );
                }
                output["project"] = json!(project);
                ToolResult::ok(output)
            }
            Ok(Ok(response)) => ToolResult::err(
                response
                    .error
                    .or(response.stderr)
                    .unwrap_or_else(|| "Runner project_overview failed".to_string()),
            ),
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                ToolResult::err("Runner project_overview waiter was dropped")
            }
            Err(_) => {
                self.runner_registry.cancel_request(&request_id).await;
                ToolResult::err("timed out waiting for Runner project_overview")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_agents_alias_resolution_requires_one_actual_spelling() {
        assert_eq!(
            instruction_agents_alias_resolution("AGENTS.md\nCLAUDE.md\n"),
            InstructionAgentsAliasResolution::KeepUppercase
        );
        assert_eq!(
            instruction_agents_alias_resolution("agents.md\nCLAUDE.md\n"),
            InstructionAgentsAliasResolution::KeepLowercase
        );
        assert_eq!(
            instruction_agents_alias_resolution("AGENTS.md\nagents.md\nCLAUDE.md\n"),
            InstructionAgentsAliasResolution::KeepBoth,
            "case-sensitive filesystems may contain two distinct rule sources"
        );
        assert_eq!(
            instruction_agents_alias_resolution("CLAUDE.md\n"),
            InstructionAgentsAliasResolution::KeepBoth,
            "absence of both exact directory entries is not alias evidence"
        );
    }

    #[test]
    fn effective_read_file_range_defaults_and_clamps() {
        assert_eq!(effective_read_file_range(None, None), (1, 2000, 2000));
        assert_eq!(effective_read_file_range(Some(0), Some(0)), (1, 1, 1));
        assert_eq!(
            effective_read_file_range(Some(7), Some(5000)),
            (7, 2000, 2006)
        );
    }

    #[test]
    fn read_file_default_behavior_has_no_line_number_fields() {
        let result = read_file_content_result("one\ntwo\nthree".to_string(), Some(2), Some(1));

        assert!(result.success);
        assert_eq!(result.output["text"], "two");
        assert_eq!(result.output["format"], "plain");
        assert_eq!(result.output["total_lines"], 3);
        assert_eq!(result.output["start_line"], 2);
        assert_eq!(result.output["limit"], 1);
        assert_eq!(
            result.output["sha256"],
            sha256_hex_bytes(b"one\ntwo\nthree")
        );
        assert_eq!(result.output["returned_lines"], 1);
        assert_eq!(result.output["end_line"], 2);
        assert!(result.output["has_more"].as_bool().unwrap());
        assert_eq!(result.output["next_start_line"], 3);
        assert!(result.output.get("content").is_none());
        assert!(result.output.get("lines").is_none());
    }

    #[test]
    fn read_file_runner_stdout_json_is_returned_without_reslicing() {
        let result = read_file_runner_stdout_result(
            serde_json::json!({
                "format": "webcodex.file_read_range.v1",
                "content": "line-560\nline-561",
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "total_lines": 7348,
                "start_line": 560,
                "limit": 2,
            })
            .to_string(),
            Some(560),
            Some(2),
        );

        assert!(result.success);
        assert_eq!(
            result.output["sha256"],
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(result.output["text"], "line-560\nline-561");
        assert_eq!(result.output["total_lines"], 7348);
        assert_eq!(result.output["start_line"], 560);
        assert_eq!(result.output["limit"], 2);
        // Continuation cursor metadata is always present.
        assert_eq!(result.output["returned_lines"], 2);
        assert_eq!(result.output["end_line"], 561);
        assert!(result.output["has_more"].as_bool().unwrap());
        assert_eq!(result.output["next_start_line"], 562);
        // No envelope extras are passed through.
        assert!(result.output.get("content").is_none());
    }

    #[test]
    fn read_file_runner_stdout_json_without_canonical_envelope_is_rejected() {
        let result = read_file_runner_stdout_result(
            serde_json::json!({
                "content": "file-json-content",
                "total_lines": 7348,
                "start_line": 560,
                "limit": 2,
            })
            .to_string(),
            Some(560),
            Some(2),
        );

        assert!(!result.success);
        // Malformed envelope surfaces a stable reason code, never raw stdout.
        assert_eq!(result.output["reason_code"], "malformed_agent_response");
        assert!(result.error.unwrap().contains("malformed_agent_response"));
    }

    #[test]
    fn read_file_runner_stdout_plain_text_is_rejected() {
        let result =
            read_file_runner_stdout_result("one\ntwo\nthree\n".to_string(), Some(2), Some(1));

        assert!(!result.success);
        assert_eq!(result.output["reason_code"], "malformed_agent_response");
    }

    #[test]
    fn read_file_with_line_numbers_returns_one_numbered_representation() {
        let result = read_file_content_result_with_options(
            "alpha\nbeta\ngamma".to_string(),
            None,
            None,
            true,
        );

        assert!(result.success);
        assert_eq!(result.output["text"], "1 | alpha\n2 | beta\n3 | gamma");
        assert_eq!(result.output["format"], "numbered");
        assert!(result.output.get("content").is_none());
        assert!(result.output.get("numbered_text").is_none());
    }

    #[test]
    fn read_file_start_line_limit_with_line_numbers_uses_effective_range() {
        let result = read_file_content_result_with_options(
            "one\ntwo\nthree\nfour".to_string(),
            Some(2),
            Some(2),
            true,
        );

        assert!(result.success);
        assert_eq!(result.output["text"], "2 | two\n3 | three");
        assert_eq!(result.output["start_line"], 2);
        assert_eq!(result.output["limit"], 2);
        assert_eq!(result.output["format"], "numbered");
    }

    #[test]
    fn read_file_with_line_numbers_handles_eof_and_short_files() {
        let result =
            read_file_content_result_with_options("one\ntwo".to_string(), Some(5), Some(3), true);

        assert!(result.success);
        assert_eq!(result.output["text"], "");
        assert_eq!(result.output["total_lines"], 2);
        assert_eq!(result.output["start_line"], 5);
        assert_eq!(result.output["limit"], 3);
        assert_eq!(result.output["format"], "numbered");
    }

    #[test]
    fn read_file_runner_stdout_json_with_line_numbers_preserves_empty_lines() {
        let result = read_file_runner_stdout_result_with_options(
            serde_json::json!({
                "format": "webcodex.file_read_range.v1",
                "content": "\nsecond",
                "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "total_lines": 3,
                "start_line": 1,
                "limit": 2,
            })
            .to_string(),
            Some(1),
            Some(2),
            true,
        );

        assert!(result.success);
        assert_eq!(result.output["text"], "1 | \n2 | second");
        assert_eq!(result.output["format"], "numbered");
        // Numbering does not change range cursor metadata.
        assert_eq!(result.output["returned_lines"], 2);
        assert_eq!(result.output["end_line"], 2);
        assert!(result.output["has_more"].as_bool().unwrap());
        assert_eq!(result.output["next_start_line"], 3);
    }

    // -------------------------------------------------------------------------
    // Bounded source reads: reference/Runner parity, strict validation, budgets.
    // -------------------------------------------------------------------------

    /// Build a local result from full file content and a request range.
    fn local_read(
        content: &str,
        start: Option<usize>,
        limit: Option<usize>,
        numbered: bool,
    ) -> Value {
        read_file_content_result_with_options(content.to_string(), start, limit, numbered).output
    }

    /// Build a Runner result from a synthesized v1 envelope derived from the
    /// same full content and range, so local and Runner outputs can be compared
    /// field-by-field.
    fn runner_read(
        content: &str,
        start: Option<usize>,
        limit: Option<usize>,
        numbered: bool,
    ) -> Value {
        use webcodex_workspace::file_read_range::{self, EffectiveRange};
        let range = EffectiveRange::new(start, limit);
        let result = file_read_range::read_range_from(content.as_bytes(), range).unwrap();
        let envelope = serde_json::json!({
            "format": "webcodex.file_read_range.v1",
            "content": result.content,
            "sha256": result.sha256,
            "total_lines": result.total_lines,
            "start_line": result.start_line,
            "limit": result.limit,
            "padding": "x".repeat(8192),
        })
        .to_string();
        read_file_runner_stdout_result_with_options(envelope, start, limit, numbered).output
    }

    fn assert_parity(content: &str, start: Option<usize>, limit: Option<usize>, numbered: bool) {
        let local = local_read(content, start, limit, numbered);
        let runner = runner_read(content, start, limit, numbered);
        for field in [
            "text",
            "format",
            "sha256",
            "start_line",
            "limit",
            "total_lines",
            "returned_lines",
            "end_line",
            "has_more",
            "next_start_line",
        ] {
            assert_eq!(
                local.get(field),
                runner.get(field),
                "parity mismatch on {field} for content={content:?} start={start:?} limit={limit:?} numbered={numbered}"
            );
        }
        // Runner envelope padding never reaches the model output.
        assert!(runner.get("padding").is_none());
        assert!(runner.get("content").is_none());
    }

    #[test]
    fn read_file_local_runner_parity_across_ranges() {
        assert_parity("one\ntwo\nthree\nfour\nfive", Some(2), Some(2), false);
        assert_parity("one\ntwo\nthree\nfour\nfive", Some(2), Some(2), true);
        assert_parity("one\ntwo\nthree", Some(1), Some(100), false);
        assert_parity("a\n\nb\nc", Some(2), Some(2), false);
        assert_parity("\nsecond\nthird", Some(1), Some(2), true);
        assert_parity("only", None, None, false);
    }

    #[test]
    fn read_file_parity_empty_file_and_overflow() {
        assert_parity("", Some(1), Some(5), false);
        assert_parity("a\nb\nc", Some(10), Some(5), false);
    }

    #[test]
    fn read_file_parity_no_trailing_newline() {
        assert_parity("one\ntwo", Some(2), Some(5), false);
        assert_parity("one\ntwo", Some(2), Some(5), true);
    }

    #[test]
    fn read_file_parity_clamps() {
        assert_parity("x\ny\nz", Some(0), Some(0), false);
        assert_parity("x\ny\nz", Some(1), Some(5000), false);
    }

    #[test]
    fn read_file_runner_rejects_bad_sha256() {
        let envelope = serde_json::json!({
            "format": "webcodex.file_read_range.v1",
            "content": "x",
            "sha256": "too-short",
            "total_lines": 1,
            "start_line": 1,
            "limit": 2000,
        })
        .to_string();
        let result = read_file_runner_stdout_result_with_options(envelope, None, None, false);
        assert!(!result.success);
        assert_eq!(result.output["reason_code"], "malformed_agent_response");
    }

    #[test]
    fn read_file_runner_rejects_wrong_start_line() {
        let envelope = serde_json::json!({
            "format": "webcodex.file_read_range.v1",
            "content": "x\ny",
            "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "total_lines": 2,
            "start_line": 5,
            "limit": 2000,
        })
        .to_string();
        let result = read_file_runner_stdout_result_with_options(envelope, None, None, false);
        assert!(!result.success);
        assert_eq!(result.output["reason_code"], "malformed_agent_response");
    }

    #[test]
    fn read_file_runner_rejects_inconsistent_content_lines() {
        // content has 2 segments but total_lines/start/limit imply 3 returned.
        let envelope = serde_json::json!({
            "format": "webcodex.file_read_range.v1",
            "content": "a\nb",
            "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "total_lines": 4,
            "start_line": 1,
            "limit": 3,
        })
        .to_string();
        let result = read_file_runner_stdout_result_with_options(envelope, Some(1), Some(3), false);
        assert!(!result.success);
        assert_eq!(result.output["reason_code"], "malformed_agent_response");
    }

    #[test]
    fn read_file_runner_rejects_wrong_field_types() {
        let envelope = serde_json::json!({
            "format": "webcodex.file_read_range.v1",
            "content": 7,
            "sha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "total_lines": "1",
            "start_line": 1,
            "limit": 2000,
        })
        .to_string();
        let result = read_file_runner_stdout_result_with_options(envelope, None, None, false);
        assert!(!result.success);
        assert_eq!(result.output["reason_code"], "malformed_agent_response");
    }

    #[test]
    fn read_file_runner_rejects_malformed_json() {
        let result = read_file_runner_stdout_result_with_options(
            "{ not valid json ".to_string(),
            None,
            None,
            false,
        );
        assert!(!result.success);
        assert_eq!(result.output["reason_code"], "malformed_agent_response");
    }

    #[test]
    fn read_file_runner_rejects_oversized_formal_content() {
        let big = "x".repeat(webcodex_workspace::file_read_range::MAX_RANGE_CONTENT_BYTES + 1);
        let envelope = serde_json::json!({
            "format": "webcodex.file_read_range.v1",
            "content": big,
            "sha256": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "total_lines": 1,
            "start_line": 1,
            "limit": 2000,
        })
        .to_string();
        let result = read_file_runner_stdout_result_with_options(envelope, None, None, false);
        assert!(!result.success);
        assert_eq!(result.output["reason_code"], "range_too_large");
    }

    #[test]
    fn read_file_runner_strips_huge_padding() {
        let envelope = serde_json::json!({
            "format": "webcodex.file_read_range.v1",
            "content": "hello",
            "sha256": "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "total_lines": 1,
            "start_line": 1,
            "limit": 2000,
            "runner_secret": "S".repeat(64_000),
            "canonical_root": "/abs/leak/path",
            "cwd": "/abs/cwd/leak",
        })
        .to_string();
        let result = read_file_runner_stdout_result_with_options(envelope, None, None, false);
        assert!(result.success, "{:?}", result.error);
        // No envelope extras, absolute paths, or secrets leak.
        assert!(result.output.get("runner_secret").is_none());
        assert!(result.output.get("canonical_root").is_none());
        assert!(result.output.get("cwd").is_none());
        let serialized = serde_json::to_string(&result.output).unwrap();
        assert!(!serialized.contains("leak"));
        assert!(!serialized.contains("runner_secret"));
        assert!(
            serialized.len() < webcodex_workspace::file_read_range::MAX_SERIALIZED_OUTPUT_BYTES
        );
    }

    #[test]
    fn read_file_error_no_absolute_path_or_os_text() {
        let result = read_file_failure(
            webcodex_workspace::file_read_range::ReadFileReason::NotFound,
            Some("README.md"),
        );
        assert!(!result.success);
        assert_eq!(result.output["reason_code"], "not_found");
        assert_eq!(result.output["path"], "README.md");
        assert_eq!(result.output["state_changed"], false);
        let serialized = serde_json::to_string(&result.output).unwrap();
        assert!(!serialized.contains("/"));
        assert!(!serialized.contains("os error"));
    }

    #[test]
    fn read_file_large_file_small_range_small_output() {
        let line = "x".repeat(64);
        let body = (0..200_000)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let out = local_read(&body, Some(100_000), Some(3), false);
        assert_eq!(out["returned_lines"], 3);
        assert!(out["has_more"].as_bool().unwrap());
        assert_eq!(out["next_start_line"], 100_003);
        let text = out["text"].as_str().unwrap();
        assert!(text.len() < 256);
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            serialized.len() < webcodex_workspace::file_read_range::MAX_SERIALIZED_OUTPUT_BYTES
        );
    }

    #[test]
    fn read_file_range_exceeding_budget_fails_with_reason_code() {
        let line = "x".repeat(1024);
        let body = (0..512)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let out = local_read(&body, Some(1), Some(2000), false);
        assert_eq!(out["reason_code"], "range_too_large");
    }

    #[test]
    fn read_file_local_io_errors_keep_stable_reason_codes() {
        assert_eq!(
            io_error_reason(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            ReadFileReason::NotFound
        );
        assert_eq!(
            io_error_reason(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            ReadFileReason::PermissionDenied
        );
        assert_eq!(
            io_error_reason(&std::io::Error::from(std::io::ErrorKind::Other)),
            ReadFileReason::IoError
        );
    }

    fn runner_error_response(message: &str) -> ShellRunResponse {
        ShellRunResponse {
            success: false,
            request_id: "req-read".to_string(),
            client_id: "runner".to_string(),
            cwd: None,
            command_preview: String::new(),
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(1),
            error: Some(message.to_string()),
            request_dispatched: Some(true),
            command_execution_state: None,
        }
    }

    #[test]
    fn read_file_runner_error_mapping_prefers_formal_reason_codes() {
        for reason in [
            ReadFileReason::InvalidPath,
            ReadFileReason::SensitivePath,
            ReadFileReason::NotFound,
            ReadFileReason::NotFile,
            ReadFileReason::PermissionDenied,
            ReadFileReason::InvalidUtf8,
            ReadFileReason::RangeTooLarge,
            ReadFileReason::RunnerUnavailable,
            ReadFileReason::Timeout,
            ReadFileReason::MalformedRunnerResponse,
            ReadFileReason::IoError,
        ] {
            let response = runner_error_response(&format!("read_file failed: {}", reason.as_str()));
            assert_eq!(map_runner_read_error(&response), reason);
        }
        assert_eq!(
            map_runner_read_error(&runner_error_response("range output too large")),
            ReadFileReason::RangeTooLarge
        );
        assert_eq!(
            map_runner_read_error(&runner_error_response("file_read target is not a file")),
            ReadFileReason::NotFile
        );
        assert_eq!(
            map_runner_read_error(&runner_error_response("invalid unrelated runner detail")),
            ReadFileReason::IoError
        );
    }

    #[test]
    fn read_file_payload_reserves_final_result_and_session_metadata_budget() {
        fn result_for(content_len: usize) -> ToolResult {
            let range = FileReadRange {
                content: "\0".repeat(content_len),
                sha256: "a".repeat(64),
                total_lines: 1,
                start_line: 1,
                limit: 1,
                returned_lines: 1,
                end_line: Some(1),
                has_more: false,
                next_start_line: None,
            };
            build_read_file_success(&range, false, Some("src/lib.rs"))
        }

        let mut accepted = 0usize;
        let mut rejected = file_read_range::MAX_RANGE_CONTENT_BYTES.saturating_add(1);
        while accepted.saturating_add(1) < rejected {
            let candidate = accepted + (rejected - accepted) / 2;
            if result_for(candidate).success {
                accepted = candidate;
            } else {
                rejected = candidate;
            }
        }
        let rejected_result = result_for(rejected);
        assert!(!rejected_result.success);
        assert_eq!(rejected_result.output["reason_code"], "range_too_large");

        let mut result = result_for(accepted);
        assert!(result.success);
        result.output["session_hint"] = json!({
            "has_open_messages": true,
            "open_counts": {
                "guidance": u64::MAX,
                "question": u64::MAX,
                "todo": u64::MAX,
                "risk": u64::MAX
            },
            "highest_priority": "high",
            "suggested_next_tool": "session_discussion_summary"
        });
        let serialized = serde_json::to_vec(&result).unwrap();
        assert!(
            serialized.len() <= file_read_range::MAX_SERIALIZED_OUTPUT_BYTES,
            "final serialized result {} exceeds hard limit",
            serialized.len()
        );
    }

    #[test]
    fn read_file_json_escaping_stays_under_hard_limit() {
        // A range that fits the content budget but is full of quote/backslash
        // characters must not escape past the serialized hard limit.
        let line = "\\\"\\\n".repeat(8);
        let body = line.repeat(2000);
        let out = local_read(&body, Some(1), Some(2), true);
        if out.get("reason_code").and_then(|v| v.as_str()) == Some("range_too_large") {
            return;
        }
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            serialized.len() <= webcodex_workspace::file_read_range::MAX_SERIALIZED_OUTPUT_BYTES,
            "serialized output {} exceeds hard limit",
            serialized.len()
        );
    }
}
