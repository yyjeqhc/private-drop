//! Runner-owned structured process construction, without lifecycle ownership.
use std::path::Path;
use std::process::Command;

/// Build a command without changing the process owner, stdio or lifecycle.
/// Native argv remains native; batch conversion is exclusively Runner-owned.
pub(crate) fn structured_process_command(
    program: &std::ffi::OsStr,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<Command, String> {
    #[cfg(not(windows))]
    let _ = cwd;
    #[cfg(windows)]
    if Path::new(program).extension().is_some_and(|extension| {
        extension
            .to_str()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
    }) {
        use std::os::windows::process::CommandExt;
        let cwd = cwd
            .map(Path::to_path_buf)
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
            .map_err(|_| "invalid_arguments: batch working directory is unavailable")?;
        if matches!(cwd.components().next(), Some(std::path::Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..)))
        {
            return Err("invalid_arguments: Windows batch shims require a local drive working directory; cmd.exe cannot preserve UNC cwd, use a native runtime executable".to_string());
        }
        let program = program
            .to_str()
            .ok_or("invalid_arguments: batch path must be Unicode")?;
        let command_line = windows_batch_command_line(program, args)?;
        // Never resolve the command processor through the Project PATH or COMSPEC.
        let system_root =
            std::env::var_os("SystemRoot").ok_or("Windows SystemRoot is unavailable")?;
        let mut command = Command::new(Path::new(&system_root).join("System32/cmd.exe"));
        command.args(["/d", "/s", "/v:off", "/c"]);
        command.raw_arg(command_line);
        return Ok(command);
    }
    let mut command = Command::new(program);
    command.args(args);
    Ok(command)
}

/// Bounded cmd.exe contract, also tested on non-Windows hosts. Each value is
/// double quoted. Quotes, expansion characters, control characters and trailing
/// backslashes are rejected before spawn, since batch forwarding via %* cannot
/// preserve those values uniformly. Never interpolate an unquoted model value.
#[cfg(any(windows, test))]
fn windows_batch_command_line(program: &str, args: &[String]) -> Result<String, String> {
    let mut line = String::from("\"");
    for value in std::iter::once(program).chain(args.iter().map(String::as_str)) {
        if value
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '%' | '!' | '^'))
            || value.ends_with('\\')
        {
            return Err("invalid_arguments: Windows batch arguments cannot contain quotes, %, !, ^, control characters or trailing backslashes; use a native runtime executable for these arguments".to_string());
        }
        if line.len() > 1 {
            line.push(' ');
        }
        line.push('"');
        line.push_str(value);
        line.push('"');
        if line.encode_utf16().count() + 1 > 8000 {
            return Err(
                "invalid_arguments: Windows batch command exceeds 8000 UTF-16 units".to_string(),
            );
        }
    }
    line.push('"');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn batch_rejects_unc_cwd_before_command_processor_start() {
        for cwd in [r"\\server\share\repo", r"\\?\UNC\server\share\repo"] {
            assert!(structured_process_command(
                std::ffi::OsStr::new(r"C:\tools\pnpm.cmd"),
                &[],
                Some(Path::new(cwd))
            )
            .unwrap_err()
            .contains("cannot preserve UNC cwd"));
        }
    }

    #[test]
    fn windows_batch_quoting_has_a_bounded_literal_contract() {
        let args = ["space value", "&", "|", "(value)", "", "中文"].map(String::from);
        assert_eq!(
            windows_batch_command_line(r"C:\tools\pnpm.cmd", &args).unwrap(),
            "\"\"C:\\tools\\pnpm.cmd\" \"space value\" \"&\" \"|\" \"(value)\" \"\" \"中文\"\""
        );
        for value in [
            "a\"&whoami",
            "%PATH%",
            "!PATH!",
            "^&",
            "line\nnext",
            "\r",
            "\0",
            "tail\\",
        ] {
            assert!(windows_batch_command_line("pnpm.cmd", &[value.to_string()]).is_err());
            assert!(windows_batch_command_line(value, &[]).is_err());
        }
        assert_eq!(
            windows_batch_command_line("pnpm.cmd", &["x".repeat(7985)])
                .unwrap()
                .encode_utf16()
                .count(),
            8000
        );
        assert!(windows_batch_command_line("pnpm.cmd", &["x".repeat(7986)]).is_err());
    }
}
