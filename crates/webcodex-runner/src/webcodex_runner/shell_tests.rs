use super::*;
use crate::runner_protocol::ShellCommandExecutionState;
use std::sync::{Arc, OnceLock};

#[cfg(windows)]
#[test]
fn inherited_environment_drops_windows_drive_current_directory_entries() {
    assert!(!should_inherit_env_key("=E:"));
    assert!(should_inherit_env_key("PATH"));
    assert!(!should_inherit_env_key("WebCodex_Token"));
}

struct ProcessArgvHelper {
    _temp: tempfile::TempDir,
    path: PathBuf,
}

static PROCESS_ARGV_HELPER: OnceLock<Arc<ProcessArgvHelper>> = OnceLock::new();

fn process_argv_helper() -> PathBuf {
    PROCESS_ARGV_HELPER
        .get_or_init(|| {
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/process_argv_helper.rs");
            let temp = tempfile::tempdir().unwrap();
            let output = temp.path().join(format!(
                "process-argv-helper{}",
                std::env::consts::EXE_SUFFIX
            ));
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let result = Command::new(rustc)
                .arg("--edition=2021")
                .arg("--crate-name=webcodex_process_argv_helper")
                .arg(source)
                .arg("-o")
                .arg(&output)
                .output()
                .expect("run rustc for process argv helper");
            assert!(
                result.status.success(),
                "process argv helper compilation failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            Arc::new(ProcessArgvHelper {
                _temp: temp,
                path: output,
            })
        })
        .path
        .clone()
}

fn create_fake_native_executable(path: &Path) {
    #[cfg(windows)]
    std::fs::copy(process_argv_helper(), path).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn run_direct_process(
    cwd: &Path,
    executable: &Path,
    args: &[String],
    stdin: Option<&str>,
    timeout_secs: u64,
) -> ShellCommandResult {
    let project_registry_dir = tempfile::tempdir().unwrap();
    run_process_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &ShellConfig::default(),
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.to_string_lossy().as_ref()),
        &executable.to_string_lossy(),
        args,
        stdin,
        timeout_secs,
        None,
    )
}

fn run_direct_script(
    cwd: &Path,
    language: ShellScriptLanguage,
    script: String,
    args: Vec<String>,
    stdin: Option<&str>,
    timeout_secs: u64,
) -> ShellCommandResult {
    let project_registry_dir = tempfile::tempdir().unwrap();
    run_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &ShellConfig::default(),
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.to_string_lossy().as_ref()),
        &ShellScriptPayload {
            language,
            script,
            args,
        },
        stdin,
        timeout_secs,
        None,
    )
}

fn unrestricted_policy() -> RunnerPolicy {
    RunnerPolicy {
        allow_cwd_anywhere: true,
        ..RunnerPolicy::default()
    }
}

#[cfg(windows)]
fn compile_internal_posix_test_executable(path: &Path) {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/internal_posix_bash_helper.rs");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let compiled = Command::new(rustc)
        .arg("--edition=2021")
        .arg("--crate-name=webcodex_internal_posix_bash_helper")
        .arg(source)
        .arg("-o")
        .arg(path)
        .output()
        .expect("compile internal POSIX test executable");
    assert!(
        compiled.status.success(),
        "internal POSIX test executable compilation failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
}

#[cfg(windows)]
#[test]
fn internal_posix_interpreter_prefers_git_toolchain_over_wsl_bash() {
    let root = tempfile::tempdir().unwrap();
    let wsl_dir = root.path().join("Windows").join("System32");
    let git_root = root.path().join("Git");
    let git_cmd = git_root.join("cmd");
    let git_bin = git_root.join("bin");
    std::fs::create_dir_all(&wsl_dir).unwrap();
    std::fs::create_dir_all(&git_cmd).unwrap();
    std::fs::create_dir_all(&git_bin).unwrap();
    compile_internal_posix_test_executable(&wsl_dir.join("bash.exe"));
    compile_internal_posix_test_executable(&git_cmd.join("git.exe"));
    let git_bash = git_bin.join("bash.exe");
    compile_internal_posix_test_executable(&git_bash);

    let mut shell = ShellConfig {
        program: "powershell.exe".to_string(),
        dialect: Some(ShellDialect::PowerShell),
        ..Default::default()
    };
    shell.env.insert(
        "PATH".to_string(),
        std::env::join_paths([&wsl_dir, &git_cmd])
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );

    assert_eq!(
        resolve_windows_internal_posix_interpreter(&shell, None).unwrap(),
        git_bash.into_os_string()
    );
}

#[cfg(windows)]
#[test]
fn internal_posix_interpreter_rejects_wsl_only_bash() {
    let root = tempfile::tempdir().unwrap();
    let wsl_dir = root.path().join("Windows").join("System32");
    std::fs::create_dir_all(&wsl_dir).unwrap();
    compile_internal_posix_test_executable(&wsl_dir.join("bash.exe"));
    let mut shell = ShellConfig::default();
    shell
        .env
        .insert("PATH".to_string(), wsl_dir.to_string_lossy().into_owned());
    let error = resolve_windows_internal_posix_interpreter(&shell, None).unwrap_err();
    assert!(
        error.contains("WSL bash launchers are not valid"),
        "{error}"
    );
}

#[cfg(windows)]
#[test]
fn internal_posix_runtime_uses_git_bash_stdin_with_powershell_configured() {
    let cwd = tempfile::tempdir().unwrap();
    let project_registry_dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let git_root = root.path().join("Git");
    let git_cmd = git_root.join("cmd");
    let git_bin = git_root.join("bin");
    std::fs::create_dir_all(&git_cmd).unwrap();
    std::fs::create_dir_all(&git_bin).unwrap();
    compile_internal_posix_test_executable(&git_cmd.join("git.exe"));
    compile_internal_posix_test_executable(&git_bin.join("bash.exe"));

    let mut shell = ShellConfig {
        program: "powershell.exe".to_string(),
        dialect: Some(ShellDialect::PowerShell),
        ..Default::default()
    };
    shell
        .env
        .insert("PATH".to_string(), git_cmd.to_string_lossy().into_owned());
    let script =
        "n=0\nwhile [ \"$n\" -lt 1 ]; do n=$((n + 1)); done\nprintf 'internal-posix-ok\\n'\n";
    let result = run_internal_posix_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &shell,
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        script,
        10,
        None,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(0));
    assert_eq!(result.result.stdout.as_deref(), Some(script));
    assert!(result.result.error.is_none());
}

#[cfg(unix)]
#[test]
fn internal_posix_runtime_ignores_configured_shell_on_posix_hosts() {
    use std::os::unix::fs::PermissionsExt;

    let cwd = tempfile::tempdir().unwrap();
    let project_registry_dir = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("configured-shell-ran");
    let configured_shell = cwd.path().join("configured-shell");
    std::fs::write(
        &configured_shell,
        format!("#!/bin/sh\nprintf ran > '{}'\nexit 99\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&configured_shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    let shell = ShellConfig {
        program: configured_shell.to_string_lossy().into_owned(),
        dialect: Some(ShellDialect::PowerShell),
        ..Default::default()
    };
    let script = "printf 'internal-posix-ok\\n'\n";
    let result = run_internal_posix_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &shell,
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        script,
        10,
        None,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(0));
    assert_eq!(result.result.stdout.as_deref(), Some("internal-posix-ok\n"));
    assert!(result.result.error.is_none());
    assert!(
        !marker.exists(),
        "generated internal script must not execute the configured shell"
    );
}

#[test]
fn phase_f_powershell_utf8_preamble_precedes_requested_shell_command() {
    let requested = "Write-Output '中文 🙂'";
    let command = powershell_command_text(requested);
    assert!(command.starts_with(POWERSHELL_UTF8_PREAMBLE));
    let preamble_end = command.find('\n').expect("preamble line ending");
    let requested_start = command.find(requested).expect("requested command");
    assert!(preamble_end < requested_start);
    assert!(command.contains("$LASTEXITCODE = 0"));
    assert!(command.ends_with("exit 0"));

    let init = Path::new(r"C:\runner profile\init.ps1");
    let initialized = powershell_init_command_text(init, requested);
    assert!(initialized.starts_with(POWERSHELL_UTF8_PREAMBLE));
    assert!(
        initialized
            .find(". 'C:\\runner profile\\init.ps1'")
            .unwrap()
            < initialized.find(requested).unwrap()
    );

    let prepared = powershell_profile_prepare_script("Write-Output init", "MARKER");
    assert!(prepared.starts_with(POWERSHELL_UTF8_PREAMBLE));
}

#[test]
fn phase_f_bounded_raw_tail_aligns_complete_utf8_before_windows_decode() {
    assert_eq!(RAW_TAIL_CAPTURE_ALLOWANCE, 4);
    let text = "中🙂".repeat(64);
    let bytes = text.as_bytes();
    let max = 64;
    let raw_tail_offset = bytes.len() - (max + RAW_TAIL_CAPTURE_ALLOWANCE);
    assert_eq!(raw_tail_offset, 380);
    assert!(
        !text.is_char_boundary(raw_tail_offset),
        "test tail must begin inside a UTF-8 scalar"
    );
    let aligned_offset = (raw_tail_offset..bytes.len())
        .find(|offset| text.is_char_boundary(*offset))
        .unwrap();
    assert_eq!(aligned_offset, 381);

    let captured = read_bounded_pipe_tail(std::io::Cursor::new(bytes), max, "stdout").unwrap();
    assert!(captured.raw_truncated);
    assert_eq!(
        captured.encoding.full_stream_utf8,
        FullStreamUtf8Validity::Valid
    );
    assert_eq!(captured.encoding.leading_bom, LeadingBom::None);
    assert_eq!(captured.bytes, &bytes[aligned_offset..]);
    assert!(captured.bytes.len() <= max + RAW_TAIL_CAPTURE_ALLOWANCE);
    assert!(
        !crate::webcodex_runner::output_text::captured_windows_output_uses_oem_for_test(
            &captured.bytes,
            captured.encoding,
        )
    );

    let normalized = captured.normalize_as_windows_for_test(max);
    assert!(normalized.len() <= max);
    assert!(std::str::from_utf8(normalized.as_bytes()).is_ok());
    assert!(normalized.starts_with("[output truncated]\n"));
    assert!(!normalized.contains('\u{fffd}'));
    let retained = normalized.strip_prefix("[output truncated]\n").unwrap();
    assert!(!retained.is_empty());
    assert!(text.ends_with(retained), "{retained:?}");
    assert!(retained.contains('中'));
    assert!(retained.contains('🙂'));
}

#[test]
fn phase_f_bounded_raw_tail_restores_utf8_bom_after_scalar_alignment() {
    let text = "中🙂".repeat(64);
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(text.as_bytes());
    let max = 64;
    let raw_tail_offset = bytes.len() - (max + RAW_TAIL_CAPTURE_ALLOWANCE);
    let content_offset = raw_tail_offset - 3;
    assert_eq!(raw_tail_offset, 383);
    assert_eq!(content_offset, 380);
    assert!(
        !text.is_char_boundary(content_offset),
        "test tail must begin inside a UTF-8 scalar"
    );
    let capacity_offset = content_offset + 3;
    assert!(!text.is_char_boundary(capacity_offset));
    let aligned_content_offset = (capacity_offset..text.len())
        .find(|offset| text.is_char_boundary(*offset))
        .unwrap();
    assert_eq!(aligned_content_offset, 385);

    let captured = read_bounded_pipe_tail(std::io::Cursor::new(&bytes), max, "stdout").unwrap();
    assert!(captured.raw_truncated);
    assert_eq!(
        captured.encoding,
        CapturedOutputEncoding {
            full_stream_utf8: FullStreamUtf8Validity::Valid,
            leading_bom: LeadingBom::Utf8,
        }
    );
    let mut expected = vec![0xEF, 0xBB, 0xBF];
    expected.extend_from_slice(&bytes[3 + aligned_content_offset..]);
    assert_eq!(captured.bytes, expected);
    assert!(captured.bytes.len() <= max + RAW_TAIL_CAPTURE_ALLOWANCE);

    let normalized = captured.normalize_as_windows_for_test(max);
    assert!(normalized.len() <= max);
    assert!(normalized.starts_with("[output truncated]\n"));
    assert!(!normalized.contains('\u{feff}'));
    assert!(!normalized.contains('\u{fffd}'));
    let retained = normalized.strip_prefix("[output truncated]\n").unwrap();
    assert!(!retained.is_empty());
    assert!(text.ends_with(retained), "{retained:?}");
    assert!(retained.contains('中'));
    assert!(retained.contains('🙂'));
}

#[test]
fn phase_f_invalid_full_stream_keeps_oem_classification_when_suffix_is_utf8() {
    let max = 32;
    let mut bytes = vec![0xFF];
    bytes.extend(std::iter::repeat_n(b'x', 100));
    let captured = read_bounded_pipe_tail(std::io::Cursor::new(bytes), max, "stderr").unwrap();
    assert!(captured.raw_truncated);
    assert_eq!(
        captured.encoding,
        CapturedOutputEncoding {
            full_stream_utf8: FullStreamUtf8Validity::Invalid,
            leading_bom: LeadingBom::None,
        }
    );
    assert!(std::str::from_utf8(&captured.bytes).is_ok());
    assert!(
        crate::webcodex_runner::output_text::captured_windows_output_uses_oem_for_test(
            &captured.bytes,
            captured.encoding,
        )
    );
    assert!(captured.bytes.len() <= max + RAW_TAIL_CAPTURE_ALLOWANCE);
    assert!(captured.normalize_as_windows_for_test(max).len() <= max);
}

#[test]
fn phase_f_utf8_validator_handles_split_scalars_and_latches_invalidity() {
    let scalar = "🙂".as_bytes();
    let mut validator = IncrementalUtf8Validator::new();
    validator.push(&scalar[..1]);
    assert!(validator.valid_so_far);
    assert_eq!(validator.pending.len(), 1);
    validator.push(&scalar[1..3]);
    assert!(validator.valid_so_far);
    assert_eq!(validator.pending.len(), 3);
    validator.push(&scalar[3..]);
    assert!(validator.valid_so_far);
    assert!(validator.pending.is_empty());
    assert_eq!(validator.finish(), FullStreamUtf8Validity::Valid);

    let mut across_capture_reads = vec![b'x'; 8191];
    across_capture_reads.extend_from_slice(scalar);
    let captured = read_bounded_pipe_tail(
        std::io::Cursor::new(&across_capture_reads),
        across_capture_reads.len(),
        "stdout",
    )
    .unwrap();
    assert!(!captured.raw_truncated);
    assert_eq!(
        captured.encoding.full_stream_utf8,
        FullStreamUtf8Validity::Valid
    );

    let mut invalid = IncrementalUtf8Validator::new();
    invalid.push(&[0xE2]);
    assert!(invalid.valid_so_far);
    assert_eq!(invalid.pending.len(), 1);
    invalid.push(b"A");
    assert!(!invalid.valid_so_far);
    assert!(invalid.pending.is_empty());
    invalid.push("valid later 🙂".as_bytes());
    assert!(!invalid.valid_so_far);
    assert!(invalid.pending.len() <= 3);
    assert_eq!(invalid.finish(), FullStreamUtf8Validity::Invalid);

    let mut incomplete_at_eof = IncrementalUtf8Validator::new();
    incomplete_at_eof.push(&scalar[..3]);
    assert_eq!(incomplete_at_eof.pending.len(), 3);
    assert_eq!(incomplete_at_eof.finish(), FullStreamUtf8Validity::Invalid);
}

#[test]
fn phase_f_bounded_raw_tail_preserves_utf16_bom_and_unit_alignment() {
    let max = 32;

    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "中".repeat(1000).encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let captured = read_bounded_pipe_tail(std::io::Cursor::new(utf16), max, "stderr").unwrap();
    assert!(captured.raw_truncated);
    assert!(captured.bytes.len() <= max + RAW_TAIL_CAPTURE_ALLOWANCE);
    assert!(captured.bytes.starts_with(&[0xFF, 0xFE]));
    assert_eq!((captured.bytes.len() - 2) % 2, 0);

    let mut surrogate_utf16 = vec![0xFF, 0xFE];
    for unit in "🙂".repeat(1000).encode_utf16() {
        surrogate_utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let captured =
        read_bounded_pipe_tail(std::io::Cursor::new(surrogate_utf16), max, "stderr").unwrap();
    let first_retained_unit = u16::from_le_bytes([captured.bytes[2], captured.bytes[3]]);
    assert!((0xD800..=0xDBFF).contains(&first_retained_unit));
    let normalized = captured.normalize_as_windows_for_test(max);
    assert!(!normalized.contains('\u{fffd}'));
    assert!(normalized.ends_with("🙂🙂🙂"));
}

#[test]
fn pre_spawn_rejection_is_not_started() {
    let mut policy = unrestricted_policy();
    policy.allow_raw_shell = false;
    let result = run_shell_impl(
        &policy,
        &ShellConfig::default(),
        None,
        None,
        "exit 0",
        None,
        10,
        None,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    assert!(result.result.exit_code.is_none());
}

#[test]
fn terminal_process_result_is_completed() {
    let result = run_shell_impl(
        &unrestricted_policy(),
        &ShellConfig::default(),
        None,
        None,
        "exit 7",
        None,
        10,
        None,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(7));
}

#[cfg(unix)]
#[test]
fn known_process_timeout_is_timed_out() {
    let result = run_shell_impl(
        &unrestricted_policy(),
        &ShellConfig::default(),
        None,
        None,
        "sleep 2",
        None,
        1,
        None,
    );

    assert_eq!(result.execution_state, ShellCommandExecutionState::TimedOut);
    assert_eq!(result.result.exit_code, Some(-1));
}

#[test]
fn post_spawn_missing_output_pipe_is_outcome_unknown() {
    let mut command = configured_shell_command(&ShellConfig::default(), "exit 0").unwrap();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    drop(child.child_mut().stdout.take());

    let error = terminate_and_read_pipes(child, 1024).unwrap_err();
    assert!(error.contains("stdout pipe missing"), "{error}");
    let result = spawned_output_failure(Instant::now(), error);
    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::OutcomeUnknown
    );
}

#[test]
fn structured_process_preserves_literal_argv_and_empty_boundaries() {
    let cwd = tempfile::tempdir().unwrap();
    let helper = process_argv_helper();
    let values = vec![
        String::new(),
        "two words".to_string(),
        "\"double\" and 'single'".to_string(),
        "; semicolon".to_string(),
        "$(not a command)".to_string(),
        "a&b|c".to_string(),
        r"C:\path with spaces\trailing\\".to_string(),
        "雪だるま☃".to_string(),
    ];
    let mut args = vec!["argv".to_string()];
    args.extend(values.clone());

    let result = run_direct_process(cwd.path(), &helper, &args, None, 10);

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(0));
    let expected = values
        .iter()
        .map(|value| format!("{}:{value}\n", value.len()))
        .collect::<String>();
    assert_eq!(result.result.stdout.as_deref(), Some(expected.as_str()));
}

#[test]
fn structured_process_does_not_interpret_shell_injection_arguments() {
    let cwd = tempfile::tempdir().unwrap();
    let helper = process_argv_helper();
    let marker = cwd.path().join("marker");
    let values = vec!["$(touch marker)".to_string(), "; touch marker".to_string()];
    let mut args = vec!["argv".to_string()];
    args.extend(values.clone());

    let result = run_direct_process(cwd.path(), &helper, &args, None, 10);

    assert_eq!(result.result.exit_code, Some(0));
    assert!(!marker.exists());
    let stdout = result.result.stdout.unwrap();
    for value in values {
        assert!(stdout.contains(&format!("{}:{value}", value.len())));
    }
}

#[test]
fn structured_process_supports_empty_args_and_bounded_stdin() {
    let cwd = tempfile::tempdir().unwrap();
    let helper = process_argv_helper();
    let empty = run_direct_process(cwd.path(), &helper, &[], None, 10);
    assert_eq!(empty.result.exit_code, Some(0));
    assert_eq!(empty.result.stdout.as_deref(), Some(""));

    let stdin = "line one\nUnicode 雪\n";
    let with_stdin =
        run_direct_process(cwd.path(), &helper, &["stdin".to_string()], Some(stdin), 10);
    assert_eq!(with_stdin.result.exit_code, Some(0));
    assert_eq!(with_stdin.result.stdout.as_deref(), Some(stdin));
}

#[cfg(windows)]
#[test]
fn structured_process_without_stdin_receives_eof_instead_of_runner_parent_lease() {
    let cwd = tempfile::tempdir().unwrap();
    let helper = process_argv_helper();
    let result = run_direct_process(cwd.path(), &helper, &["stdin".to_string()], None, 2);
    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed,
        "{:?}",
        result.result
    );
    assert_eq!(result.result.exit_code, Some(0));
    assert_eq!(result.result.stdout.as_deref(), Some(""));
}

#[test]
fn structured_process_preserves_large_literal_argv_without_shell_parsing() {
    let cwd = tempfile::tempdir().unwrap();
    let helper = process_argv_helper();
    let args = vec!["argv".to_string(), "a".repeat(4_500), "b".repeat(4_500)];
    assert!(args.iter().map(String::len).sum::<usize>() > 8_000);
    assert!(
        args.iter().map(String::len).sum::<usize>()
            < crate::runner_protocol::PROCESS_ARGV_MAX_BYTES
    );

    let result = run_direct_process(cwd.path(), &helper, &args, None, 10);

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(0));
    let stdout = result.result.stdout.unwrap();
    assert!(stdout.starts_with("4500:"));
    assert!(stdout.contains("\n4500:"));
}

#[test]
fn structured_process_continuously_drains_large_stdout_and_stderr() {
    let cwd = tempfile::tempdir().unwrap();
    let project_registry_dir = tempfile::tempdir().unwrap();
    let mut policy = unrestricted_policy();
    policy.max_output_bytes = 16 * 1024;
    let result = run_process_with_profiles_and_execution_state(
        1,
        &policy,
        &ShellConfig::default(),
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &process_argv_helper().to_string_lossy(),
        &["chatty".to_string(), "512".to_string()],
        None,
        10,
        None,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed,
        "{:?}",
        result.result
    );
    assert_eq!(result.result.exit_code, Some(0));
    for (name, output, tail_byte) in [
        ("stdout", result.result.stdout.as_deref().unwrap(), b'x'),
        ("stderr", result.result.stderr.as_deref().unwrap(), b'y'),
    ] {
        assert!(
            output.len() <= policy.max_output_bytes,
            "{name} was unbounded"
        );
        assert!(
            output.starts_with("[output truncated]\n"),
            "{name}: {output:?}"
        );
        assert_eq!(output.as_bytes().last().copied(), Some(b'\n'));
        assert!(
            output.as_bytes().iter().any(|byte| *byte == tail_byte),
            "{name} lost its retained tail"
        );
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }
}

#[test]
fn structured_script_continuously_drains_large_stdout_and_stderr() {
    let cwd = tempfile::tempdir().unwrap();
    let project_registry_dir = tempfile::tempdir().unwrap();
    let mut policy = unrestricted_policy();
    policy.max_output_bytes = 16 * 1024;
    let stdout_payload = "x".repeat(4096);
    let stderr_payload = "y".repeat(4096);
    #[cfg(windows)]
    let (language, script) = (
        ShellScriptLanguage::Powershell,
        format!(
            "$out = '{stdout_payload}'\n$err = '{stderr_payload}'\nfor ($i = 0; $i -lt 512; $i++) {{ [Console]::Out.Write($out); [Console]::Error.Write($err) }}\n"
        ),
    );
    #[cfg(not(windows))]
    let (language, script) = (
        ShellScriptLanguage::Sh,
        format!(
            "out='{stdout_payload}'\nerr='{stderr_payload}'\ni=0\nwhile [ \"$i\" -lt 512 ]; do\n  printf '%s' \"$out\"\n  printf '%s' \"$err\" >&2\n  i=$((i + 1))\ndone\n"
        ),
    );
    let result = run_script_with_profiles_and_execution_state(
        1,
        &policy,
        &ShellConfig::default(),
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &ShellScriptPayload {
            language,
            script,
            args: Vec::new(),
        },
        None,
        30,
        None,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed,
        "{:?}",
        result.result
    );
    assert_eq!(result.result.exit_code, Some(0));
    for (name, output, tail_byte) in [
        ("stdout", result.result.stdout.as_deref().unwrap(), b'x'),
        ("stderr", result.result.stderr.as_deref().unwrap(), b'y'),
    ] {
        assert!(
            output.len() <= policy.max_output_bytes,
            "{name} was unbounded"
        );
        assert!(
            output.starts_with("[output truncated]\n"),
            "{name}: {output:?}"
        );
        assert!(
            output.as_bytes().iter().any(|byte| *byte == tail_byte),
            "{name} lost its retained tail"
        );
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }
}

#[test]
fn structured_process_parent_exit_closes_descendant_held_pipes_via_tree_cleanup() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("pipe-descendant-started");
    let result = run_direct_process(
        cwd.path(),
        &process_argv_helper(),
        &[
            "spawn-pipe-descendant".to_string(),
            marker.to_string_lossy().into_owned(),
            "60000".to_string(),
        ],
        None,
        10,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed,
        "{:?}",
        result.result
    );
    assert_eq!(result.result.exit_code, Some(0));
    assert!(
        result
            .result
            .stdout
            .as_deref()
            .unwrap_or_default()
            .contains("DESCENDANT_PID="),
        "the parent did not prove that a pipe-inheriting descendant was spawned"
    );
    // The descendant inherits the parent's stdout/stderr handles and sleeps for
    // 60 seconds. Returning terminal here proves direct-child exit still drives
    // whole-tree cleanup before the continuously draining readers wait for EOF.
    assert!(result.result.duration_ms.unwrap_or_default() < 10_000);
}

#[test]
fn structured_process_reports_prestart_completion_and_nonzero_truthfully() {
    let cwd = tempfile::tempdir().unwrap();
    let missing = cwd.path().join("definitely-missing-process");
    let not_started = run_direct_process(cwd.path(), &missing, &[], None, 10);
    assert_eq!(
        not_started.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    assert_eq!(not_started.result.exit_code, None);

    let helper = process_argv_helper();
    let completed = run_direct_process(
        cwd.path(),
        &helper,
        &["exit".to_string(), "0".to_string()],
        None,
        10,
    );
    assert_eq!(
        completed.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(completed.result.exit_code, Some(0));

    let nonzero = run_direct_process(
        cwd.path(),
        &helper,
        &["exit".to_string(), "23".to_string()],
        None,
        10,
    );
    assert_eq!(
        nonzero.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(nonzero.result.exit_code, Some(23));
}

#[test]
fn structured_process_known_timeout_is_timed_out() {
    let cwd = tempfile::tempdir().unwrap();
    let result = run_direct_process(
        cwd.path(),
        &process_argv_helper(),
        &["sleep".to_string(), "2000".to_string()],
        None,
        1,
    );

    assert_eq!(result.execution_state, ShellCommandExecutionState::TimedOut);
    assert_eq!(result.result.exit_code, Some(-1));
}

#[cfg(unix)]
#[test]
fn structured_sh_script_exceeds_legacy_limit_uses_temp_file_and_cleans_it() {
    let cwd = tempfile::tempdir().unwrap();
    let observed_path = cwd.path().join("observed-script-path");
    let mut script = "# payload padding\n".repeat(2_400);
    script.push_str(&format!(
        "printf '%s' \"$0\" > '{}'\nprintf 'path=%s\\nhello\\n' \"$0\"\n",
        observed_path.display()
    ));
    assert!(script.len() > 32 * 1024);

    let result = run_direct_script(
        cwd.path(),
        ShellScriptLanguage::Sh,
        script,
        Vec::new(),
        None,
        10,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(0));
    let temporary_path =
        PathBuf::from(std::fs::read_to_string(&observed_path).expect("script path evidence"));
    assert_eq!(
        temporary_path.extension().and_then(|value| value.to_str()),
        Some("sh")
    );
    assert!(!temporary_path.starts_with(cwd.path()));
    assert!(
        !temporary_path.exists(),
        "temporary script must be removed after terminal execution"
    );
    let stdout = result.result.stdout.unwrap();
    assert!(stdout.contains("path=<temporary-script>"), "{stdout}");
    assert!(stdout.ends_with("hello\n"), "{stdout}");
    assert!(
        !stdout.contains(&temporary_path.to_string_lossy().to_string()),
        "absolute temporary path must be redacted"
    );
}

#[cfg(unix)]
#[test]
fn structured_bash_script_preserves_content_and_literal_argument_boundaries() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("marker");
    let observed_path = cwd.path().join("bash-script-path");
    let args = vec![
        String::new(),
        "two words".to_string(),
        "$(touch marker)".to_string(),
        "; touch marker".to_string(),
        r"C:\path with spaces\trailing\\".to_string(),
        "雪だるま☃".to_string(),
    ];
    let script = format!(
        r#"printf '%s' "$0" > '{}'
literal=$(cat <<'WEBCODEX_LITERAL'
quotes: "'" $()
semicolons: ;;; pipes: |||
backslashes: C:\one\two\\
Unicode: 雪だるま☃
WEBCODEX_LITERAL
)
printf '%s\n' "$literal"
for value in "$@"; do
  printf '%s:%s\n' "${{#value}}" "$value"
done
"#,
        observed_path.display()
    );

    let result = run_direct_script(
        cwd.path(),
        ShellScriptLanguage::Bash,
        script,
        args.clone(),
        None,
        10,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(0));
    assert!(!marker.exists(), "shell-looking args must remain data");
    let stdout = result.result.stdout.unwrap();
    assert!(stdout.contains(r#"quotes: "'" $()"#), "{stdout}");
    assert!(stdout.contains("semicolons: ;;; pipes: |||"), "{stdout}");
    assert!(stdout.contains(r"backslashes: C:\one\two\\"), "{stdout}");
    assert!(stdout.contains("Unicode: 雪だるま☃"), "{stdout}");
    for value in &args {
        assert!(
            stdout.contains(&format!("{}:{value}", value.chars().count())),
            "missing literal arg {value:?} in {stdout:?}"
        );
    }
    let temporary_path = PathBuf::from(std::fs::read_to_string(observed_path).unwrap());
    assert_eq!(
        temporary_path.extension().and_then(|value| value.to_str()),
        Some("sh")
    );
    assert!(!temporary_path.exists());
}

#[cfg(unix)]
#[test]
fn structured_script_stdin_nonzero_and_timeout_preserve_lifecycle() {
    let cwd = tempfile::tempdir().unwrap();
    let input = "line one\nUnicode 雪\n";
    let stdin_result = run_direct_script(
        cwd.path(),
        ShellScriptLanguage::Sh,
        "cat".to_string(),
        Vec::new(),
        Some(input),
        10,
    );
    assert_eq!(stdin_result.result.exit_code, Some(0));
    assert_eq!(stdin_result.result.stdout.as_deref(), Some(input));

    let language_semantics = run_direct_script(
        cwd.path(),
        ShellScriptLanguage::Sh,
        "false\nprintf 'continued\\n'".to_string(),
        Vec::new(),
        None,
        10,
    );
    assert_eq!(language_semantics.result.exit_code, Some(0));
    assert_eq!(
        language_semantics.result.stdout.as_deref(),
        Some("continued\n"),
        "Runner must not inject set -e"
    );

    let nonzero = run_direct_script(
        cwd.path(),
        ShellScriptLanguage::Sh,
        "exit 23".to_string(),
        Vec::new(),
        None,
        10,
    );
    assert_eq!(
        nonzero.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(nonzero.result.exit_code, Some(23));

    let timed_out = run_direct_script(
        cwd.path(),
        ShellScriptLanguage::Sh,
        "sleep 2".to_string(),
        Vec::new(),
        None,
        1,
    );
    assert_eq!(
        timed_out.execution_state,
        ShellCommandExecutionState::TimedOut
    );
    assert_eq!(timed_out.result.exit_code, Some(-1));
}

#[test]
fn missing_script_interpreter_is_prestart_and_does_not_run_script() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("marker");
    let project_registry_dir = tempfile::tempdir().unwrap();
    let mut shell = ShellConfig {
        program: "custom-shell".to_string(),
        ..Default::default()
    };
    shell.env.insert("PATH".to_string(), String::new());
    let result = run_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &shell,
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &ShellScriptPayload {
            language: ShellScriptLanguage::Bash,
            script: "touch marker".to_string(),
            args: Vec::new(),
        },
        None,
        10,
        None,
    );
    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    assert!(result.result.exit_code.is_none());
    assert!(result
        .result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("interpreter_unavailable"));
    assert!(!marker.exists());
}

#[test]
fn missing_javascript_interpreter_is_prestart_and_does_not_run_script() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("marker");
    let project_registry_dir = tempfile::tempdir().unwrap();
    let mut shell = ShellConfig {
        program: "custom-shell".to_string(),
        ..Default::default()
    };
    shell.env.insert("PATH".to_string(), String::new());
    let result = run_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &shell,
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &ShellScriptPayload {
            language: ShellScriptLanguage::Javascript,
            script: "import { writeFileSync } from 'node:fs'; writeFileSync('marker', 'ran');"
                .to_string(),
            args: Vec::new(),
        },
        None,
        10,
        None,
    );
    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    assert!(result.result.exit_code.is_none());
    let error = result.result.error.as_deref().unwrap_or_default();
    assert!(error.contains("interpreter_unavailable"), "{error}");
    assert!(error.contains("JavaScript/Node"), "{error}");
    assert!(!marker.exists());
}

#[test]
fn missing_typescript_interpreter_is_prestart_and_does_not_run_script() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("marker");
    let project_registry_dir = tempfile::tempdir().unwrap();
    let mut shell = ShellConfig {
        program: "custom-shell".to_string(),
        ..Default::default()
    };
    shell.env.insert("PATH".to_string(), String::new());
    let result = run_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &shell,
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &ShellScriptPayload {
            language: ShellScriptLanguage::Typescript,
            script: "const marker: string = 'ran';\nvoid marker;".to_string(),
            args: Vec::new(),
        },
        None,
        10,
        None,
    );
    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    assert!(result.result.exit_code.is_none());
    let error = result.result.error.as_deref().unwrap_or_default();
    assert!(error.contains("interpreter_unavailable"), "{error}");
    assert!(error.contains("TypeScript/Node"), "{error}");
    assert!(!marker.exists());
}

#[test]
fn javascript_interpreter_resolves_node_from_prepared_profile_path() {
    let temp = tempfile::tempdir().unwrap();
    let node = temp
        .path()
        .join(format!("node{}", std::env::consts::EXE_SUFFIX));
    create_fake_native_executable(&node);
    let profile = PreparedShellProfile {
        profile_name: "js-test".to_string(),
        program: "bash".to_string(),
        args: Vec::new(),
        dialect: ShellDialect::Posix,
        env_snapshot: std::collections::HashMap::from([(
            "PATH".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]),
    };

    let resolved = configured_script_interpreter(
        &ShellConfig::default(),
        Some(&profile),
        ShellScriptLanguage::Javascript,
    )
    .unwrap();
    assert_eq!(PathBuf::from(resolved), node);
}

#[test]
fn typescript_interpreter_resolves_the_same_node_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let node = temp
        .path()
        .join(format!("node{}", std::env::consts::EXE_SUFFIX));
    create_fake_native_executable(&node);
    let profile = PreparedShellProfile {
        profile_name: "ts-test".to_string(),
        program: "bash".to_string(),
        args: Vec::new(),
        dialect: ShellDialect::Posix,
        env_snapshot: std::collections::HashMap::from([(
            "PATH".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]),
    };
    let resolved = configured_script_interpreter(
        &ShellConfig::default(),
        Some(&profile),
        ShellScriptLanguage::Typescript,
    )
    .unwrap();
    assert_eq!(PathBuf::from(resolved), node);
}

#[test]
fn javascript_interpreter_accepts_configured_node_executable() {
    let temp = tempfile::tempdir().unwrap();
    let node = temp
        .path()
        .join(format!("node{}", std::env::consts::EXE_SUFFIX));
    create_fake_native_executable(&node);
    let mut shell = ShellConfig {
        program: node.to_string_lossy().into_owned(),
        ..Default::default()
    };
    shell.env.insert("PATH".to_string(), String::new());

    let resolved =
        configured_script_interpreter(&shell, None, ShellScriptLanguage::Javascript).unwrap();
    assert_eq!(PathBuf::from(resolved), node);
}

#[test]
fn node_script_languages_do_not_fallback_to_alternate_runtimes() {
    let temp = tempfile::tempdir().unwrap();
    for runtime in ["bun", "deno", "tsx", "npx", "npm"] {
        create_fake_native_executable(
            &temp
                .path()
                .join(format!("{runtime}{}", std::env::consts::EXE_SUFFIX)),
        );
    }
    let mut shell = ShellConfig {
        program: "custom-shell".to_string(),
        ..Default::default()
    };
    shell.env.insert(
        "PATH".to_string(),
        temp.path().to_string_lossy().into_owned(),
    );

    for (language, runtime_name) in [
        (ShellScriptLanguage::Javascript, "JavaScript/Node"),
        (ShellScriptLanguage::Typescript, "TypeScript/Node"),
    ] {
        let error = configured_script_interpreter(&shell, None, language).unwrap_err();
        assert!(error.contains("interpreter_unavailable"), "{error}");
        assert!(error.contains(runtime_name), "{error}");
        assert!(!error.contains(temp.path().to_string_lossy().as_ref()));
    }
}

#[cfg(unix)]
#[test]
fn arbitrary_configured_shell_is_not_treated_as_a_script_language() {
    use std::os::unix::fs::PermissionsExt;

    let cwd = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("custom-shell-ran");
    let custom_shell = cwd.path().join("custom-shell");
    std::fs::write(
        &custom_shell,
        format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&custom_shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    let project_registry_dir = tempfile::tempdir().unwrap();
    let mut shell = ShellConfig {
        program: custom_shell.to_string_lossy().into_owned(),
        ..Default::default()
    };
    shell.env.insert("PATH".to_string(), String::new());
    let result = run_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &shell,
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &ShellScriptPayload {
            language: ShellScriptLanguage::Bash,
            script: "true".to_string(),
            args: Vec::new(),
        },
        None,
        10,
        None,
    );

    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    assert!(result
        .result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("interpreter_unavailable"));
    assert!(!marker.exists());
}

#[test]
fn sh_and_bash_plans_pass_a_script_file_without_command_text_mode() {
    use std::ffi::OsStr;

    let script_path = Path::new("/runner/scratch/payload.sh");
    let script_args = vec!["two words".to_string(), "$(literal)".to_string()];
    for (language, interpreter) in [
        (ShellScriptLanguage::Sh, "sh"),
        (ShellScriptLanguage::Bash, "bash"),
    ] {
        let plan = ScriptRuntimePlan {
            program: OsString::from(interpreter),
            prefix_args: fixed_script_prefix_args(language),
        };
        let command = build_script_command(&plan, script_path, &script_args);
        assert_eq!(command.get_program(), OsStr::new(interpreter));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                script_path.as_os_str(),
                OsStr::new("two words"),
                OsStr::new("$(literal)")
            ]
        );
        assert!(!command
            .get_args()
            .any(|argument| argument == OsStr::new("-c")));
    }
}

#[test]
fn javascript_plan_uses_mjs_file_and_native_literal_argv() {
    use std::ffi::OsStr;

    let script_path = Path::new("/runner/scratch/payload.mjs");
    let args = vec![
        "two words".to_string(),
        "$(literal)".to_string(),
        "; literal".to_string(),
    ];
    let plan = ScriptRuntimePlan {
        program: OsString::from("node"),
        prefix_args: fixed_script_prefix_args(ShellScriptLanguage::Javascript),
    };
    let command = build_script_command(&plan, script_path, &args);
    assert_eq!(command.get_program(), OsStr::new("node"));
    let actual = command.get_args().collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            script_path.as_os_str(),
            OsStr::new("two words"),
            OsStr::new("$(literal)"),
            OsStr::new("; literal")
        ]
    );
    for forbidden in ["-e", "-p", "-c", "-Command"] {
        assert!(!actual.iter().any(|arg| *arg == OsStr::new(forbidden)));
    }
}

#[test]
fn typescript_node_versions_select_only_runner_owned_strip_flag_when_required() {
    assert!(parse_node_version(b"v22.12.0\n").is_some());
    assert_eq!(
        typescript_node_probe_error("profile prepare stopped during runner shutdown".to_string()),
        "TypeScript runtime probe stopped during runner shutdown; command was not started"
    );
    assert!(
        typescript_node_probe_error("profile prepare timed out after 2 seconds".to_string())
            .starts_with("interpreter_unavailable:"),
        "a bounded capability-probe failure must remain a pre-start runtime availability failure"
    );
    for version in [
        NodeVersion {
            major: 22,
            minor: 6,
            patch: 0,
        },
        NodeVersion {
            major: 22,
            minor: 12,
            patch: 0,
        },
        NodeVersion {
            major: 22,
            minor: 17,
            patch: 9,
        },
        NodeVersion {
            major: 23,
            minor: 5,
            patch: 0,
        },
    ] {
        assert_eq!(
            typescript_node_prefix_args(version).unwrap(),
            vec![OsString::from("--experimental-strip-types")]
        );
    }
    for version in [
        NodeVersion {
            major: 22,
            minor: 18,
            patch: 0,
        },
        NodeVersion {
            major: 23,
            minor: 6,
            patch: 0,
        },
        NodeVersion {
            major: 24,
            minor: 0,
            patch: 0,
        },
        NodeVersion {
            major: 26,
            minor: 0,
            patch: 0,
        },
    ] {
        assert!(typescript_node_prefix_args(version).unwrap().is_empty());
    }
    let error = typescript_node_prefix_args(NodeVersion {
        major: 22,
        minor: 5,
        patch: 9,
    })
    .unwrap_err();
    assert!(error.contains("Node.js 22.6.0 or newer"), "{error}");
    assert!(error.contains("command was not started"), "{error}");
    assert!(parse_node_version(b"not-node").is_none());
}

#[test]
fn typescript_plan_uses_mts_native_argv_and_runner_owned_prefix() {
    use std::ffi::OsStr;

    let script_path = Path::new("/runner/scratch/payload.mts");
    let user_args = vec![
        "two words".to_string(),
        "--experimental-transform-types".to_string(),
    ];
    let plan = ScriptRuntimePlan {
        program: OsString::from("node"),
        prefix_args: vec![OsString::from("--experimental-strip-types")],
    };
    let command = build_script_command(&plan, script_path, &user_args);
    assert_eq!(command.get_program(), OsStr::new("node"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            OsStr::new("--experimental-strip-types"),
            script_path.as_os_str(),
            OsStr::new("two words"),
            OsStr::new("--experimental-transform-types")
        ]
    );
    assert!(!command.get_args().any(|arg| arg == OsStr::new("-e")));
}

#[test]
fn powershell_plan_uses_ps1_file_and_never_command_text_mode() {
    use std::ffi::OsStr;

    let script_path = Path::new("/runner/scratch/payload.ps1");
    let args = vec![
        String::new(),
        "two words".to_string(),
        "$(literal)".to_string(),
        "; literal".to_string(),
    ];
    let plan = ScriptRuntimePlan {
        program: OsString::from("pwsh"),
        prefix_args: fixed_script_prefix_args(ShellScriptLanguage::Powershell),
    };
    let command = build_script_command(&plan, script_path, &args);
    assert_eq!(command.get_program(), OsStr::new("pwsh"));
    let actual = command.get_args().collect::<Vec<_>>();
    let mut expected = vec![OsStr::new("-NoProfile"), OsStr::new("-NonInteractive")];
    if cfg!(windows) {
        expected.extend([OsStr::new("-ExecutionPolicy"), OsStr::new("Bypass")]);
    }
    expected.extend([
        OsStr::new("-File"),
        script_path.as_os_str(),
        OsStr::new(""),
        OsStr::new("two words"),
        OsStr::new("$(literal)"),
        OsStr::new("; literal"),
    ]);
    assert_eq!(actual, expected);
    assert!(!actual.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg.eq_ignore_ascii_case("-Command") || arg.eq_ignore_ascii_case("-c")
    }));
}

#[test]
fn javascript_temp_file_uses_mjs_and_exact_script_bytes() {
    let payload = ShellScriptPayload {
        language: ShellScriptLanguage::Javascript,
        script: "import { readFile } from 'node:fs/promises';\nawait Promise.resolve(readFile);"
            .to_string(),
        args: Vec::new(),
    };
    let (temporary_path, _original, absolute) = create_temporary_script(&payload).unwrap();
    assert_eq!(
        absolute.extension().and_then(|ext| ext.to_str()),
        Some("mjs")
    );
    assert_eq!(std::fs::read(&absolute).unwrap(), payload.script.as_bytes());
    temporary_path.close().unwrap();
}

#[test]
fn typescript_temp_file_uses_mts_and_exact_script_bytes() {
    let payload = ShellScriptPayload {
        language: ShellScriptLanguage::Typescript,
        script: "interface Item { value: string }\nconst item: Item = { value: 'ok' };\nconsole.log(item.value);\n".to_string(),
        args: Vec::new(),
    };
    let (temporary_path, _original, absolute) = create_temporary_script(&payload).unwrap();
    assert_eq!(
        absolute.extension().and_then(|ext| ext.to_str()),
        Some("mts")
    );
    assert_eq!(std::fs::read(&absolute).unwrap(), payload.script.as_bytes());
    temporary_path.close().unwrap();
}

#[test]
#[ignore = "manual real-process smoke: requires compatible Node.js on PATH"]
fn typescript_runtime_executes_erasable_mts_with_argv_stdin_and_cwd_when_available() {
    let cwd = tempfile::tempdir().unwrap();
    let project_registry_dir = tempfile::tempdir().unwrap();
    let result = run_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &ShellConfig::default(),
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &ShellScriptPayload {
            language: ShellScriptLanguage::Typescript,
            script: r#"import { readFileSync } from 'node:fs';
interface Payload { value: string }
function identity<T>(value: T): T { return value; }
const payload: Payload = identity<Payload>({ value: process.argv[2] ?? '' });
const input: string = readFileSync(0, 'utf8').trim();
console.log(JSON.stringify({ value: payload.value, input, cwd: process.cwd() }));
"#
            .to_string(),
            args: vec!["two words".to_string()],
        },
        Some("stdin-value\n"),
        30,
        None,
    );
    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(0), "{:?}", result.result);
    let stdout = result.result.stdout.as_deref().unwrap_or_default();
    assert!(stdout.contains("\"value\":\"two words\""), "{stdout}");
    assert!(stdout.contains("\"input\":\"stdin-value\""), "{stdout}");
    assert!(
        stdout.contains(cwd.path().to_string_lossy().as_ref()),
        "{stdout}"
    );
}

#[test]
fn phase_f_powershell_temp_file_uses_utf8_bom_without_script_preamble() {
    let payload = ShellScriptPayload {
        language: ShellScriptLanguage::Powershell,
        script: "param([string]$Value)\nWrite-Output $Value".to_string(),
        args: Vec::new(),
    };
    let (temporary_path, _original, absolute) = create_temporary_script(&payload).unwrap();
    let bytes = std::fs::read(&absolute).unwrap();
    assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF]);
    assert_eq!(&bytes[3..], payload.script.as_bytes());
    temporary_path.close().unwrap();
}
#[test]
fn powershell_runtime_executes_from_file_when_available() {
    let cwd = tempfile::tempdir().unwrap();
    let project_registry_dir = tempfile::tempdir().unwrap();
    if configured_script_interpreter(
        &ShellConfig::default(),
        None,
        ShellScriptLanguage::Powershell,
    )
    .is_err()
    {
        return;
    }
    let result = run_script_with_profiles_and_execution_state(
        1,
        &unrestricted_policy(),
        &ShellConfig::default(),
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &ShellScriptPayload {
            language: ShellScriptLanguage::Powershell,
            script: "param([string]$Value)\nWrite-Output $Value".to_string(),
            args: vec!["two words".to_string()],
        },
        None,
        // This smoke validates external PowerShell file execution, not timeout
        // behavior. Allow cold pwsh startup under a loaded CI runner.
        30,
        None,
    );
    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(0), "{:?}", result.result);
    assert!(result
        .result
        .stdout
        .as_deref()
        .unwrap_or_default()
        .contains("two words"));
}

#[cfg(windows)]
#[test]
fn phase_f_windows_native_utf8_and_utf16_stdout_stderr_normalize() {
    let cwd = tempfile::tempdir().unwrap();
    let helper = process_argv_helper();

    let utf8 = run_direct_process(
        cwd.path(),
        &helper,
        &["windows-utf8-output".to_string()],
        None,
        10,
    );
    assert_eq!(utf8.execution_state, ShellCommandExecutionState::Completed);
    assert_eq!(utf8.result.exit_code, Some(17));
    assert_eq!(utf8.result.stdout.as_deref(), Some("UTF8 中文 🙂\n"));
    assert_eq!(utf8.result.stderr.as_deref(), Some("UTF8 中文 🙂\n"));

    let utf16 = run_direct_process(
        cwd.path(),
        &helper,
        &["windows-utf16-output".to_string()],
        None,
        10,
    );
    assert_eq!(utf16.execution_state, ShellCommandExecutionState::Completed);
    assert_eq!(utf16.result.exit_code, Some(0));
    assert_eq!(utf16.result.stdout.as_deref(), Some("UTF16 中文 🙂\n"));
    assert_eq!(utf16.result.stderr.as_deref(), Some("UTF16 中文 🙂\n"));
}

#[cfg(windows)]
#[test]
fn phase_f_windows_native_oem_stdout_stderr_use_active_oem_page() {
    let cwd = tempfile::tempdir().unwrap();
    let expected_path = cwd.path().join("expected.txt");
    let result = run_direct_process(
        cwd.path(),
        &process_argv_helper(),
        &[
            "windows-oem-output".to_string(),
            expected_path.to_string_lossy().into_owned(),
        ],
        None,
        10,
    );
    let expected = std::fs::read_to_string(expected_path).unwrap();
    assert!(!expected.is_ascii());
    assert_eq!(
        result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(result.result.exit_code, Some(23));
    assert_eq!(result.result.stdout.as_deref(), Some(expected.as_str()));
    assert_eq!(result.result.stderr.as_deref(), Some(expected.as_str()));
    assert!(!result
        .result
        .stdout
        .as_deref()
        .unwrap_or_default()
        .contains('\u{fffd}'));

    let bounded_expected_path = cwd.path().join("bounded-expected.txt");
    let project_registry_dir = tempfile::tempdir().unwrap();
    let mut policy = unrestricted_policy();
    policy.max_output_bytes = 64;
    let bounded = run_process_with_profiles_and_execution_state(
        1,
        &policy,
        &ShellConfig::default(),
        project_registry_dir.path(),
        &PreparedShellProfileCache::default(),
        Some(cwd.path().to_string_lossy().as_ref()),
        &process_argv_helper().to_string_lossy(),
        &[
            "windows-oem-output".to_string(),
            bounded_expected_path.to_string_lossy().into_owned(),
            "1000".to_string(),
        ],
        None,
        10,
        None,
    );
    for output in [bounded.result.stdout, bounded.result.stderr] {
        let output = output.unwrap();
        assert!(output.len() <= policy.max_output_bytes);
        assert!(output.starts_with("[output truncated]\n"), "{output:?}");
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }
}

#[cfg(windows)]
#[test]
fn phase_f_windows_powershell_shell_and_param_script_keep_semantics() {
    let cwd = tempfile::tempdir().unwrap();
    let shell = ShellConfig::default();
    let shell_result = run_shell_impl(
        &unrestricted_policy(),
        &shell,
        None,
        Some(cwd.path().to_string_lossy().as_ref()),
        "[Console]::Out.WriteLine('shell 中文 🙂'); [Console]::Error.WriteLine('error 中文 🙂'); exit 19",
        None,
        10,
        None,);
    assert_eq!(
        shell_result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(shell_result.result.exit_code, Some(19));
    assert_eq!(
        shell_result.result.stdout.as_deref(),
        Some("shell 中文 🙂\n")
    );
    assert_eq!(
        shell_result.result.stderr.as_deref(),
        Some("error 中文 🙂\n")
    );

    let script_result = run_direct_script(
        cwd.path(),
        ShellScriptLanguage::Powershell,
        "param([string]$Value)\n\
         $out = [Text.Encoding]::UTF8.GetBytes('script ' + $Value + \"`n\")\n\
         [Console]::OpenStandardOutput().Write($out, 0, $out.Length)\n\
         $err = [Text.Encoding]::UTF8.GetBytes('script-error ' + $Value + \"`n\")\n\
         [Console]::OpenStandardError().Write($err, 0, $err.Length)"
            .to_string(),
        vec!["中文 🙂".to_string()],
        None,
        10,
    );
    assert_eq!(
        script_result.execution_state,
        ShellCommandExecutionState::Completed
    );
    assert_eq!(script_result.result.exit_code, Some(0));
    assert_eq!(
        script_result.result.stdout.as_deref(),
        Some("script 中文 🙂\n")
    );
    assert_eq!(
        script_result.result.stderr.as_deref(),
        Some("script-error 中文 🙂\n")
    );
}

#[cfg(windows)]
#[test]
fn phase_f_windows_timeout_retains_unicode_and_runs_child_once() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("execution-marker");
    let result = run_direct_process(
        cwd.path(),
        &process_argv_helper(),
        &[
            "windows-mark-output-sleep".to_string(),
            marker.to_string_lossy().into_owned(),
            "10000".to_string(),
        ],
        None,
        1,
    );
    assert_eq!(result.execution_state, ShellCommandExecutionState::TimedOut);
    assert_eq!(result.result.exit_code, Some(-1));
    assert!(result
        .result
        .stdout
        .as_deref()
        .unwrap_or_default()
        .contains("partial 中文 🙂\n"));
    assert!(result
        .result
        .stderr
        .as_deref()
        .unwrap_or_default()
        .contains("partial 中文 🙂\n"));
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 1);
}

#[cfg(windows)]
#[test]
fn windows_run_process_preserves_native_argv_and_rejects_unsafe_batch_argv() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join("native.exe");
    std::fs::write(&native, b"MZ").unwrap();
    let args = vec![
        "space value".to_string(),
        "\"quotes\"".to_string(),
        r"backslash\\".to_string(),
        "&|;".to_string(),
    ];

    let command = configured_process_command(
        &ShellConfig::default(),
        None,
        &native.to_string_lossy(),
        &args,
        Some(temp.path()),
    )
    .unwrap();
    assert_eq!(command.get_program(), native.as_os_str());
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        args.iter().map(OsStr::new).collect::<Vec<_>>()
    );

    let marker = temp.path().join("batch-started");
    let batch = temp.path().join("script.cmd");
    std::fs::write(
        &batch,
        format!("@echo off\r\ncopy nul \"{}\"\r\n", marker.display()),
    )
    .unwrap();
    let shell = ShellConfig {
        env: HashMap::from([("PATH".into(), temp.path().to_string_lossy().into_owned())]),
        ..ShellConfig::default()
    };
    let resolved =
        configured_process_command(&shell, None, "script", &["test".into()], Some(temp.path()))
            .unwrap();
    assert!(Path::new(resolved.get_program()).ends_with("cmd.exe"));
    let batch_result = run_direct_process(temp.path(), &batch, &args, None, 10);
    assert_eq!(
        batch_result.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    let batch_error = batch_result.result.error.as_deref().unwrap_or_default();
    assert!(batch_error.contains("invalid_arguments"), "{batch_error}");
    assert!(batch_error.contains("native runtime"), "{batch_error}");
    assert!(
        !marker.exists(),
        "run_process must reject Batch before child spawn"
    );

    let unsupported = temp.path().join("script.vbs");
    std::fs::write(&unsupported, b"WScript.Echo 1\r\n").unwrap();
    let unsupported_result = run_direct_process(temp.path(), &unsupported, &[], None, 10);
    assert_eq!(
        unsupported_result.execution_state,
        ShellCommandExecutionState::NotStarted
    );
    assert!(unsupported_result
        .result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("unsupported Windows extension"));
}

#[cfg(windows)]
#[test]
fn windows_batch_shim_forwards_supported_argv_to_native_child() {
    let temp = tempfile::tempdir().unwrap();
    let batch = temp.path().join("package shim.cmd");
    std::fs::write(
        &batch,
        format!(
            "@echo off\r\n\"{}\" %*\r\n",
            process_argv_helper().display()
        ),
    )
    .unwrap();
    let args = ["argv", "space value", "&", "|", "(value)", ""].map(String::from);
    let result = run_direct_process(temp.path(), &batch, &args, None, 10);
    assert_eq!(result.result.exit_code, Some(0), "{:?}", result.result);
    let expected = args[1..]
        .iter()
        .map(|arg| format!("{}:{arg}\n", arg.len()))
        .collect::<String>();
    assert_eq!(result.result.stdout.as_deref(), Some(expected.as_str()));
}

#[test]
fn execution_environment_inherits_path_filters_credentials_and_honors_overrides() {
    let _lock = crate::tests::test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("runner-toolchain");
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(marker.clone()).chain(std::env::split_paths(&inherited)),
    )
    .unwrap();
    let _env = crate::tests::EnvGuard::new()
        .set("PATH", path)
        .set("WEBCODEX_TOKEN", "test-secret");
    let shell = ShellConfig::default();
    let env = base_shell_env(&shell, &ShellProfileConfig::default()).unwrap();
    assert_eq!(
        env_lookup(&env, "PATH").cloned(),
        std::env::var("PATH").ok()
    );
    assert!(std::env::split_paths(env_lookup(&env, "PATH").unwrap()).any(|path| path == marker));
    assert!(!env.contains_key("WEBCODEX_TOKEN"));
    let shell = ShellConfig {
        environment_mode: ShellEnvironmentMode::Isolated,
        env: HashMap::from([
            ("PATH".into(), "shell-path".into()),
            ("WEBCODEX_TOKEN".into(), "test-secret".into()),
        ]),
        ..ShellConfig::default()
    };
    let profile = ShellProfileConfig {
        env: std::collections::BTreeMap::from([("PATH".into(), "profile-path".into())]),
        ..ShellProfileConfig::default()
    };
    let env = base_shell_env(&shell, &profile).unwrap();
    assert_eq!(
        env_lookup(&env, "PATH").map(String::as_str),
        Some("profile-path")
    );
    assert!(!env.contains_key("WEBCODEX_TOKEN"));
    assert!(!env.contains_key("HOME"));
    let provider = PreparedExecutionEnvironment::prepare(
        1,
        &shell,
        None,
        Path::new("."),
        &PreparedShellProfileCache::default(),
        None,
    )
    .unwrap();
    assert!(!provider.env_snapshot.contains_key("WEBCODEX_TOKEN"));
}

#[test]
fn isolated_environment_is_explicit_and_does_not_inherit_user_path() {
    let shell: ShellConfig = toml::from_str("environment_mode = 'isolated'").unwrap();
    assert_eq!(shell.environment_mode, ShellEnvironmentMode::Isolated);
    let env = base_shell_env(&shell, &ShellProfileConfig::default()).unwrap();
    assert!(!env.contains_key("HOME"));
    assert!(!env.contains_key("USERPROFILE"));
    #[cfg(not(windows))]
    assert_eq!(
        env_lookup(&env, "PATH").map(String::as_str),
        Some("/usr/bin:/bin")
    );
    assert!(toml::from_str::<ShellConfig>("environment_mode = 'unknown'").is_err());
}

#[cfg(unix)]
#[test]
fn default_shell_preserves_non_unicode_environment_without_panicking() {
    use std::os::unix::ffi::OsStringExt;
    let _lock = crate::tests::test_env_lock();
    let _env = crate::tests::EnvGuard::new().set(
        "WEBCODEX_OPAQUE_TOOLCHAIN_ENV",
        OsString::from_vec(vec![0xff]),
    );
    configured_process_command(&ShellConfig::default(), None, "true", &[], None).unwrap();
}
