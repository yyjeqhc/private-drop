use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_SHUTDOWN_BUDGET: Duration = Duration::from_secs(12);
pub(crate) const JOB_DRAIN_BUDGET: Duration = Duration::from_secs(3);
pub(crate) const PROVIDER_SHUTDOWN_BUDGET: Duration = Duration::from_secs(3);
pub(crate) const LSP_SHUTDOWN_BUDGET: Duration = Duration::from_secs(3);
pub(crate) const BACKGROUND_JOIN_BUDGET: Duration = Duration::from_secs(2);
pub(crate) const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShutdownDeadline {
    started_at: Instant,
    deadline: Instant,
}

impl ShutdownDeadline {
    fn new(started_at: Instant, budget: Duration) -> Self {
        Self {
            started_at,
            deadline: started_at + budget,
        }
    }

    pub(crate) fn started_at(self) -> Instant {
        self.started_at
    }

    pub(crate) fn instant(self) -> Instant {
        self.deadline
    }

    pub(crate) fn phase_deadline(self, cap: Duration) -> Instant {
        self.deadline.min(Instant::now() + cap)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownPhaseStatus {
    Completed,
    TimedOut,
    Failed,
    Skipped,
}

impl ShutdownPhaseStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::TimedOut => "timed_out",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShutdownPhaseResult {
    pub(crate) phase: &'static str,
    pub(crate) status: ShutdownPhaseStatus,
    pub(crate) elapsed_ms: u64,
    pub(crate) resources: usize,
    pub(crate) error_code: Option<&'static str>,
}

impl ShutdownPhaseResult {
    pub(crate) fn completed(phase: &'static str, started: Instant, resources: usize) -> Self {
        Self::new(
            phase,
            ShutdownPhaseStatus::Completed,
            started,
            resources,
            None,
        )
    }

    pub(crate) fn timed_out(phase: &'static str, started: Instant, resources: usize) -> Self {
        Self::new(
            phase,
            ShutdownPhaseStatus::TimedOut,
            started,
            resources,
            None,
        )
    }

    pub(crate) fn failed(
        phase: &'static str,
        started: Instant,
        resources: usize,
        error_code: &'static str,
    ) -> Self {
        Self::new(
            phase,
            ShutdownPhaseStatus::Failed,
            started,
            resources,
            Some(error_code),
        )
    }

    pub(crate) fn skipped(phase: &'static str, started: Instant) -> Self {
        Self::new(phase, ShutdownPhaseStatus::Skipped, started, 0, None)
    }

    fn new(
        phase: &'static str,
        status: ShutdownPhaseStatus,
        started: Instant,
        resources: usize,
        error_code: Option<&'static str>,
    ) -> Self {
        Self {
            phase,
            status,
            elapsed_ms: elapsed_ms(started),
            resources,
            error_code,
        }
    }

    fn log_line(&self) -> String {
        let mut line = format!(
            "webcodex-runner shutdown phase {} phase={} elapsed_ms={} resources={}",
            self.status.label(),
            self.phase,
            self.elapsed_ms,
            self.resources
        );
        if let Some(error_code) = self.error_code {
            line.push_str(" error_code=");
            line.push_str(error_code);
        }
        line
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ShutdownReport {
    pub(crate) phases: Vec<ShutdownPhaseResult>,
    pub(crate) elapsed_ms: u64,
    pub(crate) timed_out_phases: Vec<&'static str>,
    pub(crate) failed_phases: Vec<&'static str>,
}

impl ShutdownReport {
    fn new(started_at: Instant, phases: Vec<ShutdownPhaseResult>) -> Self {
        let timed_out_phases = phases
            .iter()
            .filter(|result| result.status == ShutdownPhaseStatus::TimedOut)
            .map(|result| result.phase)
            .collect();
        let failed_phases = phases
            .iter()
            .filter(|result| result.status == ShutdownPhaseStatus::Failed)
            .map(|result| result.phase)
            .collect();
        Self {
            phases,
            elapsed_ms: elapsed_ms(started_at),
            timed_out_phases,
            failed_phases,
        }
    }

    pub(crate) fn log_lines(&self) -> Vec<String> {
        let mut lines = self
            .phases
            .iter()
            .filter(|result| {
                matches!(
                    result.status,
                    ShutdownPhaseStatus::TimedOut | ShutdownPhaseStatus::Failed
                ) || (result.resources > 0
                    && matches!(
                        result.phase,
                        "queued_jobs_cancel"
                            | "active_jobs_signal"
                            | "active_jobs_drain"
                            | "external_providers_stop"
                            | "lsp_servers_stop"
                    ))
            })
            .map(ShutdownPhaseResult::log_line)
            .collect::<Vec<_>>();
        lines.push(format!(
            "webcodex-runner shutdown complete elapsed_ms={} timed_out_phases={} failed_phases={}",
            self.elapsed_ms,
            phase_list(&self.timed_out_phases),
            phase_list(&self.failed_phases)
        ));
        lines
    }
}

fn phase_list(phases: &[&str]) -> String {
    if phases.is_empty() {
        "none".to_string()
    } else {
        phases.join(",")
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

pub(crate) struct ShutdownCoordinator {
    budget: Duration,
    requested: Arc<AtomicBool>,
    requested_watch: tokio::sync::watch::Sender<bool>,
    signal_received: AtomicBool,
    started: Mutex<Option<ShutdownDeadline>>,
    report: OnceLock<ShutdownReport>,
    runs: AtomicUsize,
}

impl ShutdownCoordinator {
    pub(crate) fn new(budget: Duration) -> Self {
        let (requested_watch, _) = tokio::sync::watch::channel(false);
        Self {
            budget,
            requested: Arc::new(AtomicBool::new(false)),
            requested_watch,
            signal_received: AtomicBool::new(false),
            started: Mutex::new(None),
            report: OnceLock::new(),
            runs: AtomicUsize::new(0),
        }
    }

    pub(crate) fn request_signal(&self) -> bool {
        self.signal_received.store(true, Ordering::SeqCst);
        let first = !self.requested.swap(true, Ordering::SeqCst);
        self.ensure_deadline();
        self.requested_watch.send_replace(true);
        if first {
            eprintln!("webcodex-runner shutdown signal received");
        }
        first
    }

    pub(crate) fn request_cleanup(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.ensure_deadline();
        self.requested_watch.send_replace(true);
    }

    pub(crate) async fn wait_requested(&self) {
        let mut requested = self.requested_watch.subscribe();
        loop {
            if *requested.borrow_and_update() {
                return;
            }
            requested
                .changed()
                .await
                .expect("ShutdownCoordinator retains its watch sender");
        }
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    pub(crate) fn requested_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.requested)
    }

    pub(crate) fn signal_received(&self) -> bool {
        self.signal_received.load(Ordering::SeqCst)
    }

    pub(crate) fn deadline(&self) -> ShutdownDeadline {
        self.ensure_deadline()
    }

    pub(crate) fn run_once(
        &self,
        cleanup: impl FnOnce(ShutdownDeadline) -> Vec<ShutdownPhaseResult>,
    ) -> ShutdownReport {
        self.request_cleanup();
        self.report
            .get_or_init(|| {
                self.runs.fetch_add(1, Ordering::SeqCst);
                let deadline = self.deadline();
                let report = ShutdownReport::new(deadline.started_at(), cleanup(deadline));
                for line in report.log_lines() {
                    eprintln!("{line}");
                }
                report
            })
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn run_count(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    fn ensure_deadline(&self) -> ShutdownDeadline {
        let mut started = lock_unpoison(&self.started);
        *started.get_or_insert_with(|| ShutdownDeadline::new(Instant::now(), self.budget))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ActivityTracker {
    inner: Arc<(Mutex<usize>, Condvar)>,
}

impl ActivityTracker {
    pub(crate) fn enter(&self) -> ActivityGuard {
        let (count, _) = &*self.inner;
        *lock_unpoison(count) += 1;
        ActivityGuard {
            tracker: self.clone(),
        }
    }

    pub(crate) fn active(&self) -> usize {
        let (count, _) = &*self.inner;
        *lock_unpoison(count)
    }

    pub(crate) fn wait_until(&self, deadline: Instant) -> bool {
        let (count, changed) = &*self.inner;
        let mut count = lock_unpoison(count);
        while *count > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timed_out) = changed
                .wait_timeout(count, remaining.min(Duration::from_millis(50)))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            count = next;
            if timed_out.timed_out() && Instant::now() >= deadline {
                return false;
            }
        }
        true
    }
}

pub(crate) struct ActivityGuard {
    tracker: ActivityTracker,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let (count, changed) = &*self.tracker.inner;
        let mut count = lock_unpoison(count);
        *count = count.saturating_sub(1);
        changed.notify_all();
    }
}

#[derive(Debug, Default)]
pub(crate) struct BackgroundThreads {
    handles: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BackgroundJoinResult {
    pub(crate) joined: usize,
    pub(crate) panicked: usize,
    pub(crate) timed_out: usize,
}

impl BackgroundThreads {
    pub(crate) fn register(&self, handle: JoinHandle<()>) {
        lock_unpoison(&self.handles).push(handle);
    }

    /// Join every worker that has already finished without waiting for active
    /// workers. Long-lived transports use this to keep a process-local worker
    /// registry from growing with completed polling dispatches; shutdown still
    /// owns and bounds every handle that remains active.
    pub(crate) fn reap_finished(&self) -> BackgroundJoinResult {
        let finished = {
            let mut handles = lock_unpoison(&self.handles);
            let mut finished = Vec::new();
            let mut index = 0;
            while index < handles.len() {
                if handles[index].is_finished() {
                    finished.push(handles.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            finished
        };
        let mut result = BackgroundJoinResult::default();
        for handle in finished {
            if handle.join().is_err() {
                result.panicked += 1;
            } else {
                result.joined += 1;
            }
        }
        result
    }

    pub(crate) fn join_until(&self, deadline: Instant) -> BackgroundJoinResult {
        let mut result = BackgroundJoinResult::default();
        loop {
            let reaped = self.reap_finished();
            result.joined += reaped.joined;
            result.panicked += reaped.panicked;
            let pending = lock_unpoison(&self.handles).len();
            if pending == 0 {
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                result.timed_out = pending;
                return result;
            }
            thread::sleep(SHUTDOWN_POLL_INTERVAL.min(remaining));
        }
    }

    pub(crate) fn pending(&self) -> usize {
        lock_unpoison(&self.handles).len()
    }
}

pub(crate) fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_coordinator_runs_once_and_completion_log_is_last() {
        let coordinator = ShutdownCoordinator::new(Duration::from_millis(100));
        coordinator.request_signal();
        let first = coordinator.run_once(|_| {
            vec![ShutdownPhaseResult::completed(
                "stop_accepting_work",
                Instant::now(),
                1,
            )]
        });
        let second = coordinator.run_once(|_| {
            panic!("shutdown cleanup ran more than once");
        });
        assert_eq!(coordinator.run_count(), 1);
        assert_eq!(first.phases, second.phases);
        let lines = first.log_lines();
        assert!(lines
            .last()
            .unwrap()
            .starts_with("webcodex-runner shutdown complete elapsed_ms="));
        assert!(lines[..lines.len() - 1]
            .iter()
            .all(|line| !line.contains("shutdown complete")));
    }

    #[tokio::test]
    async fn shutdown_wait_observes_signal_without_polling() {
        let coordinator = Arc::new(ShutdownCoordinator::new(Duration::from_secs(1)));
        let waiting = Arc::clone(&coordinator);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            waiting.wait_requested().await;
        });
        started_rx.await.unwrap();
        coordinator.request_signal();
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("shutdown waiter was not notified")
            .unwrap();
    }

    #[test]
    fn shutdown_logs_are_single_line_bounded_and_do_not_accept_payloads() {
        let report = ShutdownReport::new(
            Instant::now(),
            vec![
                ShutdownPhaseResult::timed_out("external_providers_stop", Instant::now(), 2),
                ShutdownPhaseResult::failed(
                    "background_threads_join",
                    Instant::now(),
                    1,
                    "thread_panicked",
                ),
            ],
        );
        for line in report.log_lines() {
            assert!(!line.contains('\n'));
            assert!(line.len() < 512);
            for forbidden in [
                "Authorization",
                "Bearer ",
                "token=",
                "payload",
                "private command",
            ] {
                assert!(!line.contains(forbidden));
            }
        }
    }
}
