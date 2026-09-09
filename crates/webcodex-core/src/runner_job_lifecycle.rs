//! Canonical semantics for the Runner Job wire lifecycle.
//!
//! Server recovery is an orthogonal overlay and is deliberately not represented
//! here. In particular, `recovering` is a Server observation state, not a Runner
//! lifecycle value.

/// Accepted Runner Job lifecycle values.
///
/// Variant names preserve distinctions that are semantically relevant even when
/// some consumers project multiple variants to the same higher-level state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerJobLifecycle {
    Queued,
    RunnerQueued,
    StartedLegacy,
    Running,
    StopRequested,
    Completed,
    Failed,
    Stopped,
    Timeout,
    TimedOut,
    Lost,
    Cancelled,
}

impl RunnerJobLifecycle {
    /// Parse one exact Runner wire lifecycle value.
    pub fn from_wire(status: &str) -> Result<Self, String> {
        match status {
            "queued" => Ok(Self::Queued),
            "agent_queued" => Ok(Self::RunnerQueued),
            "started" => Ok(Self::StartedLegacy),
            "running" => Ok(Self::Running),
            "stop_requested" => Ok(Self::StopRequested),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "stopped" => Ok(Self::Stopped),
            "timeout" => Ok(Self::Timeout),
            "timed_out" => Ok(Self::TimedOut),
            "lost" => Ok(Self::Lost),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(format!("unknown Runner Job lifecycle status: {status}")),
        }
    }

    /// Preserve the exact accepted wire spelling for this lifecycle value.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::RunnerQueued => "agent_queued",
            Self::StartedLegacy => "started",
            Self::Running => "running",
            Self::StopRequested => "stop_requested",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Timeout => "timeout",
            Self::TimedOut => "timed_out",
            Self::Lost => "lost",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Stopped
                | Self::Timeout
                | Self::TimedOut
                | Self::Lost
                | Self::Cancelled
        )
    }

    /// Runner-owned active execution states used by recovery, reconciliation,
    /// inventory retention, and stop delivery.
    ///
    /// `StartedLegacy` is deliberately excluded: it is historical broadly-active
    /// vocabulary but was never Runner recovery-active. Server-side `Queued` is
    /// likewise broadly active without yet being Runner-owned.
    pub const fn is_runner_active(self) -> bool {
        matches!(
            self,
            Self::RunnerQueued | Self::Running | Self::StopRequested
        )
    }

    /// Broad lifecycle activity, independent of any Server recovery overlay.
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::StartedLegacy) || self.is_runner_active()
    }

    /// Both accepted timeout spellings have the same timeout semantics while
    /// retaining their exact wire representation.
    pub const fn is_timed_out(self) -> bool {
        matches!(self, Self::Timeout | Self::TimedOut)
    }
}

#[cfg(test)]
mod tests {
    use super::RunnerJobLifecycle;

    #[test]
    fn runner_job_lifecycle_wire_semantics_are_canonical() {
        let cases = [
            ("queued", RunnerJobLifecycle::Queued, false, true, false),
            (
                "agent_queued",
                RunnerJobLifecycle::RunnerQueued,
                false,
                true,
                true,
            ),
            (
                "started",
                RunnerJobLifecycle::StartedLegacy,
                false,
                true,
                false,
            ),
            ("running", RunnerJobLifecycle::Running, false, true, true),
            (
                "stop_requested",
                RunnerJobLifecycle::StopRequested,
                false,
                true,
                true,
            ),
            (
                "completed",
                RunnerJobLifecycle::Completed,
                true,
                false,
                false,
            ),
            ("failed", RunnerJobLifecycle::Failed, true, false, false),
            ("stopped", RunnerJobLifecycle::Stopped, true, false, false),
            ("timeout", RunnerJobLifecycle::Timeout, true, false, false),
            (
                "timed_out",
                RunnerJobLifecycle::TimedOut,
                true,
                false,
                false,
            ),
            ("lost", RunnerJobLifecycle::Lost, true, false, false),
            (
                "cancelled",
                RunnerJobLifecycle::Cancelled,
                true,
                false,
                false,
            ),
        ];

        for (wire, expected, terminal, active, runner_active) in cases {
            let parsed = RunnerJobLifecycle::from_wire(wire).unwrap();
            assert_eq!(parsed, expected, "{wire}");
            assert_eq!(parsed.as_wire(), wire, "{wire}");
            assert_eq!(parsed.is_terminal(), terminal, "{wire}");
            assert_eq!(parsed.is_active(), active, "{wire}");
            assert_eq!(parsed.is_runner_active(), runner_active, "{wire}");
        }

        assert!(RunnerJobLifecycle::Timeout.is_timed_out());
        assert!(RunnerJobLifecycle::TimedOut.is_timed_out());
        assert!(!RunnerJobLifecycle::Failed.is_timed_out());
        assert!(RunnerJobLifecycle::from_wire("unknown").is_err());
        assert!(RunnerJobLifecycle::from_wire("recovering").is_err());
    }
}
