use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;

/// Broad activity for the Registry's public Job status projection.
///
/// `recovering` is a Server recovery overlay, not a Runner lifecycle value.
/// `stop_requested` remains active until authoritative terminal truth arrives.
pub fn job_status_is_active(status: &str) -> bool {
    status == "recovering"
        || RunnerJobLifecycle::from_wire(status).is_ok_and(RunnerJobLifecycle::is_active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_status_vocabulary_is_stable() {
        for status in [
            "running",
            "queued",
            "started",
            "agent_queued",
            "stop_requested",
            "recovering",
        ] {
            assert!(job_status_is_active(status), "{status}");
        }
        for status in [
            "completed",
            "failed",
            "stopped",
            "lost",
            "timeout",
            "timed_out",
            "cancelled",
        ] {
            assert!(!job_status_is_active(status), "{status}");
        }
        assert!(!job_status_is_active("unknown"));
        assert!(RunnerJobLifecycle::from_wire("recovering").is_err());
    }
}
