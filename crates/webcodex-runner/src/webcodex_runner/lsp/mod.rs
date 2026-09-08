mod language;
mod navigation;
mod position;
mod protocol;
mod supervisor;

pub(crate) use navigation::handle_lsp_operation;
pub(crate) use supervisor::LspSupervisor;

// The documented Windows ManagedChild spawn path uses a system-wide Toolhelp
// thread snapshot. Fake LSP tests use deliberately tight protocol deadlines,
// so serialize those test functions on Windows to avoid cross-test resource
// contention. Production builds and Linux test concurrency are unaffected.
#[cfg(all(test, windows))]
static FAKE_LSP_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, windows))]
fn serialize_fake_lsp_test() -> std::sync::MutexGuard<'static, ()> {
    FAKE_LSP_TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(all(test, not(windows)))]
struct FakeLspTestSerialGuard;

#[cfg(all(test, not(windows)))]
fn serialize_fake_lsp_test() -> FakeLspTestSerialGuard {
    FakeLspTestSerialGuard
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "navigation_tests.rs"]
mod navigation_tests;
