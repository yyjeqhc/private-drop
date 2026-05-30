use std::path::Path;
use std::time::Instant;

pub(super) fn sanitize_tail(s: &str, max_len: usize) -> (String, bool) {
    let bytes = s.as_bytes();
    if bytes.len() <= max_len {
        (s.to_string(), false)
    } else {
        // Find a valid UTF-8 boundary near max_len
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        (s[end..].to_string(), true)
    }
}

pub(super) fn run_command(cmd: &str, cwd: &Path, _timeout_secs: u64) -> (i32, String, String, u64) {
    let start = Instant::now();
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .output();

    match result {
        Ok(output) => {
            let elapsed = start.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);
            (code, stdout, stderr, elapsed)
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis() as u64;
            (
                -1,
                String::new(),
                format!("Failed to execute command: {}", e),
                elapsed,
            )
        }
    }
}

/// Escape a string for safe use as a shell argument via `ssh -- arg`.
/// Uses single-quote wrapping with proper escaping.
pub(super) fn shell_escape(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

pub(super) fn shell_join_paths(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| shell_escape(p))
        .collect::<Vec<_>>()
        .join(" ")
}
