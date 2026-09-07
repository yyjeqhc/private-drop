use std::ffi::OsString;

use super::output::RuntimeBuildMetadata;
use crate::{cli_action, CliAction};

/// Serializes every test that mutates process environment variables.
///
/// Env mutation is process-global: a panic in one test must not corrupt the
/// next one, so all env-mutating tests in this binary hold this lock for
/// their whole body. There is deliberately a single lock for the whole CLI
/// test binary — separate per-crate locks (e.g. `runner_config::TEST_ENV_LOCK`)
/// would let two env-mutating tests run concurrently.
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the shared env-test lock, tolerating poisoning: a prior test may
/// have panicked while holding it, but RAII `EnvGuard`s have already restored
/// the environment, so the lock is safe to reuse.
pub(crate) fn env_test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// RAII restore for environment variables: snapshots the current value (or
/// absence) of each variable and restores it on drop, even when the test
/// panics. Never leak env state into later tests.
pub(crate) struct EnvGuard {
    restored: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    pub(crate) fn new() -> Self {
        EnvGuard {
            restored: Vec::new(),
        }
    }

    pub(crate) fn set(mut self, name: &'static str, value: &str) -> Self {
        self.capture(name);
        std::env::set_var(name, value);
        self
    }

    pub(crate) fn set_os(mut self, name: &'static str, value: OsString) -> Self {
        self.capture(name);
        std::env::set_var(name, value);
        self
    }

    pub(crate) fn remove(mut self, name: &'static str) -> Self {
        self.capture(name);
        std::env::remove_var(name);
        self
    }

    fn capture(&mut self, name: &'static str) {
        self.restored.push((name, std::env::var_os(name)));
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.restored.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// Create security-sensitive CLI fixtures under a canonical scratch root
/// rather than the platform's ambient temp alias. On macOS, `tempfile` normally
/// starts under `/var/folders`, while `/var` is a system symlink to
/// `/private/var`; credential tests intentionally reject symlink ancestors and
/// project tests intentionally reject `/private/var` as a dangerous root.
/// Canonical `/tmp` becomes `/private/tmp` on macOS and stays outside those
/// policies. Windows keeps its canonical ambient temp directory.
pub(crate) fn canonical_test_tempdir() -> tempfile::TempDir {
    #[cfg(unix)]
    let root = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let root = std::env::temp_dir();
    let root = root
        .canonicalize()
        .expect("canonicalize CLI test temp root");
    tempfile::Builder::new()
        .prefix("webcodex-cli-case-")
        .tempdir_in(root)
        .expect("create canonical CLI test tempdir")
}

pub(crate) fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

pub(crate) fn build_metadata(commit: Option<&str>) -> RuntimeBuildMetadata {
    RuntimeBuildMetadata {
        version: Some("0.1.0".to_string()),
        git_commit: commit.map(str::to_string),
        git_dirty: Some(false),
        built_at: Some("1782739890".to_string()),
    }
}

pub(crate) fn cli_exit<I, S>(args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    match cli_action(args) {
        CliAction::Exit {
            code: 0, stdout, ..
        } => Ok(stdout),
        CliAction::Exit { stderr, .. } => Err(stderr),
        other => Err(format!("expected exit, got {other:?}")),
    }
}
