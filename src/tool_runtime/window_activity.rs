use crate::client_window::ClientWindow;
use crate::tool_request_trace::RequestCompletionTiming;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub(crate) const MAX_ACTIVE_WINDOW_REQUESTS: usize = 64;
pub(crate) const MAX_ACTIVE_REQUESTS_PER_WINDOW: usize = 8;
pub(crate) const MAX_WINDOW_LOOP_CONTINUITIES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowLoopTransition {
    Unavailable,
    Serial { gap_ms: u64 },
    Overlap,
}

impl WindowLoopTransition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Serial { .. } => "serial",
            Self::Overlap => "overlap",
        }
    }

    #[cfg(test)]
    pub(crate) fn gap_ms(self) -> Option<u64> {
        match self {
            Self::Serial { gap_ms } => Some(gap_ms),
            Self::Unavailable | Self::Overlap => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowSessionCorrelationRelation {
    Recording,
    WorkOnProject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowSessionCorrelation {
    pub(crate) session_id: String,
    pub(crate) project: Option<String>,
    pub(crate) relation: WorkflowSessionCorrelationRelation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolCallCorrelation {
    pub(crate) resolved_project: Option<String>,
    pub(crate) workflow_sessions: Vec<WorkflowSessionCorrelation>,
    pub(crate) recorder_gap_session_id: Option<String>,
}

impl ToolCallCorrelation {
    pub(crate) fn add_workflow_session(&mut self, link: WorkflowSessionCorrelation) {
        if self
            .workflow_sessions
            .iter()
            .any(|existing| existing == &link)
        {
            return;
        }
        self.workflow_sessions.push(link);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ActiveWindowRequest {
    pub(crate) client_window_key: String,
    pub(crate) client_window_source: String,
    pub(crate) server_trace_id: String,
    pub(crate) method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project: Option<String>,
    #[serde(skip)]
    principal_correlation_kind: Option<String>,
    #[serde(skip)]
    principal_correlation_id: Option<String>,
    #[serde(skip)]
    meaningful: bool,
    #[serde(skip)]
    overlapped: bool,
    pub(crate) started_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveWindowSummary {
    pub(crate) client_window_key: String,
    pub(crate) client_window_source: String,
    pub(crate) active_count: usize,
    pub(crate) last_started_at_ms: i64,
}

#[derive(Debug, Default)]
struct WindowActivityRegistryInner {
    by_trace: BTreeMap<String, ActiveWindowRequest>,
    previous_meaningful: BTreeMap<WindowContinuityKey, CompletedMeaningfulCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WindowContinuityKey {
    client_window_key: String,
    principal_kind: String,
    principal_id: String,
}

#[derive(Debug, Clone, Copy)]
struct CompletedMeaningfulCall {
    request_observed_at_ms: i64,
    response_handed_at_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WindowActivityRegistry {
    inner: Arc<Mutex<WindowActivityRegistryInner>>,
}

impl WindowActivityRegistry {
    #[cfg(test)]
    pub(crate) fn start(
        &self,
        window: &ClientWindow,
        server_trace_id: &str,
        method: &str,
        principal: Option<(&str, &str)>,
    ) -> WindowActivityGuard {
        self.start_observed(
            window,
            server_trace_id,
            method,
            None,
            principal,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    pub(crate) fn start_observed(
        &self,
        window: &ClientWindow,
        server_trace_id: &str,
        method: &str,
        tool_name: Option<&str>,
        principal: Option<(&str, &str)>,
        request_observed_at_ms: i64,
    ) -> WindowActivityGuard {
        let meaningful = method == "tools/call"
            && tool_name.is_some_and(crate::tool_runtime::is_meaningful_activity_tool);
        let mut inner = self.inner.lock().expect("Window activity mutex poisoned");
        let continuity_key = principal.map(|(kind, id)| WindowContinuityKey {
            client_window_key: window.key().to_string(),
            principal_kind: kind.to_string(),
            principal_id: id.to_string(),
        });
        let (transition, overlapped) = if meaningful {
            continuity_key
                .as_ref()
                .map(|key| {
                    classify_transition_and_mark_overlap(&mut inner, key, request_observed_at_ms)
                })
                .unwrap_or((WindowLoopTransition::Unavailable, false))
        } else {
            (WindowLoopTransition::Unavailable, false)
        };
        let record = ActiveWindowRequest {
            client_window_key: window.key().to_string(),
            client_window_source: window.source().to_string(),
            server_trace_id: server_trace_id.to_string(),
            method: method.to_string(),
            tool_name: tool_name.map(str::to_string),
            project: None,
            principal_correlation_kind: principal.map(|(kind, _)| kind.to_string()),
            principal_correlation_id: principal.map(|(_, id)| id.to_string()),
            meaningful,
            overlapped,
            started_at_ms: request_observed_at_ms,
        };
        if inner.by_trace.len() >= MAX_ACTIVE_WINDOW_REQUESTS {
            if let Some(oldest) = inner
                .by_trace
                .values()
                .min_by_key(|request| request.started_at_ms)
                .map(|request| request.server_trace_id.clone())
            {
                inner.by_trace.remove(&oldest);
            }
        }
        inner.by_trace.insert(server_trace_id.to_string(), record);
        WindowActivityGuard {
            registry: self.clone(),
            server_trace_id: server_trace_id.to_string(),
            continuity_key,
            meaningful,
            request_observed_at_ms,
            transition,
            active: true,
        }
    }

    pub(crate) fn update(
        &self,
        server_trace_id: &str,
        tool_name: Option<&str>,
        project: Option<&str>,
    ) {
        let mut inner = self.inner.lock().expect("Window activity mutex poisoned");
        let Some(request) = inner.by_trace.get_mut(server_trace_id) else {
            return;
        };
        if let Some(tool_name) = tool_name {
            request.tool_name = Some(tool_name.to_string());
        }
        if let Some(project) = project {
            request.project = Some(project.to_string());
        }
    }

    pub(crate) fn list_for_window(
        &self,
        window_key: &str,
        principal: Option<(&str, &str)>,
    ) -> Vec<ActiveWindowRequest> {
        let inner = self.inner.lock().expect("Window activity mutex poisoned");
        let mut records = inner
            .by_trace
            .values()
            .filter(|request| {
                request.client_window_key == window_key && principal_visible(request, principal)
            })
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|a, b| {
            b.started_at_ms
                .cmp(&a.started_at_ms)
                .then_with(|| a.server_trace_id.cmp(&b.server_trace_id))
        });
        records
    }

    pub(crate) fn active_windows(
        &self,
        principal: Option<(&str, &str)>,
    ) -> Vec<ActiveWindowSummary> {
        let inner = self.inner.lock().expect("Window activity mutex poisoned");
        let mut summaries = BTreeMap::<String, ActiveWindowSummary>::new();
        for request in inner
            .by_trace
            .values()
            .filter(|request| principal_visible(request, principal))
        {
            summaries
                .entry(request.client_window_key.clone())
                .and_modify(|summary| {
                    summary.active_count = summary.active_count.saturating_add(1);
                    summary.last_started_at_ms =
                        summary.last_started_at_ms.max(request.started_at_ms);
                })
                .or_insert_with(|| ActiveWindowSummary {
                    client_window_key: request.client_window_key.clone(),
                    client_window_source: request.client_window_source.clone(),
                    active_count: 1,
                    last_started_at_ms: request.started_at_ms,
                });
        }
        let mut values = summaries.into_values().collect::<Vec<_>>();
        values.sort_by(|a, b| {
            b.last_started_at_ms
                .cmp(&a.last_started_at_ms)
                .then_with(|| a.client_window_key.cmp(&b.client_window_key))
        });
        values
    }

    #[cfg(test)]
    pub(crate) fn counts_by_window(
        &self,
        principal: Option<(&str, &str)>,
    ) -> BTreeMap<String, usize> {
        let inner = self.inner.lock().expect("Window activity mutex poisoned");
        let mut counts = BTreeMap::new();
        for request in inner
            .by_trace
            .values()
            .filter(|request| principal_visible(request, principal))
        {
            *counts.entry(request.client_window_key.clone()).or_insert(0) += 1;
        }
        counts
    }

    fn finish(
        &self,
        server_trace_id: &str,
        continuity_key: Option<&WindowContinuityKey>,
        meaningful: bool,
        request_observed_at_ms: i64,
        completion: Option<RequestCompletionTiming>,
        continuity_eligible: bool,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            tracing::warn!(
                event = "window_activity_lock_poisoned",
                "window_activity_lock_poisoned"
            );
            return;
        };
        let finished_request = inner.by_trace.remove(server_trace_id);
        let overlapped = finished_request
            .as_ref()
            .is_some_and(|request| request.overlapped);
        if meaningful && continuity_eligible && finished_request.is_some() && !overlapped {
            if let (Some(key), Some(completion)) = (continuity_key, completion) {
                let should_replace = inner.previous_meaningful.get(key).is_none_or(|previous| {
                    request_observed_at_ms >= previous.request_observed_at_ms
                });
                if should_replace {
                    inner.previous_meaningful.insert(
                        key.clone(),
                        CompletedMeaningfulCall {
                            request_observed_at_ms,
                            response_handed_at_ms: completion.response_handed_at_ms,
                        },
                    );
                }
                while inner.previous_meaningful.len() > MAX_WINDOW_LOOP_CONTINUITIES {
                    let Some(oldest) = inner
                        .previous_meaningful
                        .iter()
                        .min_by_key(|(_, previous)| previous.response_handed_at_ms)
                        .map(|(key, _)| key.clone())
                    else {
                        break;
                    };
                    inner.previous_meaningful.remove(&oldest);
                }
            }
        }
    }
}

fn classify_transition_and_mark_overlap(
    inner: &mut WindowActivityRegistryInner,
    key: &WindowContinuityKey,
    request_observed_at_ms: i64,
) -> (WindowLoopTransition, bool) {
    let active_overlap = inner.by_trace.values().any(|request| {
        request.meaningful
            && request.client_window_key == key.client_window_key
            && request.principal_correlation_kind.as_deref() == Some(key.principal_kind.as_str())
            && request.principal_correlation_id.as_deref() == Some(key.principal_id.as_str())
    });
    let previous_overlap = inner
        .previous_meaningful
        .get(key)
        .is_some_and(|previous| request_observed_at_ms < previous.response_handed_at_ms);
    if active_overlap || previous_overlap {
        // Once a meaningful sequence overlaps, there is no unambiguous adjacent
        // serial predecessor. Clear the prior anchor and mark every in-flight
        // member of this Window+principal overlap group so none can later
        // manufacture an outside-WebCodex gap. A later clean completion will
        // establish a fresh anchor for the following call.
        inner.previous_meaningful.remove(key);
        for request in inner.by_trace.values_mut() {
            if request.meaningful
                && request.client_window_key == key.client_window_key
                && request.principal_correlation_kind.as_deref()
                    == Some(key.principal_kind.as_str())
                && request.principal_correlation_id.as_deref() == Some(key.principal_id.as_str())
            {
                request.overlapped = true;
            }
        }
        return (WindowLoopTransition::Overlap, true);
    }
    // Consume the predecessor at arrival. Only this request's eligible
    // completion may establish the next anchor; cancellation, streaming,
    // timeout, or active-record eviction must not leave an older call behind.
    let Some(previous) = inner.previous_meaningful.remove(key) else {
        return (WindowLoopTransition::Unavailable, false);
    };
    (
        WindowLoopTransition::Serial {
            gap_ms: u64::try_from(request_observed_at_ms - previous.response_handed_at_ms)
                .unwrap_or(u64::MAX),
        },
        false,
    )
}

fn principal_visible(request: &ActiveWindowRequest, principal: Option<(&str, &str)>) -> bool {
    match principal {
        None => true,
        Some((kind, id)) => {
            request.principal_correlation_kind.as_deref() == Some(kind)
                && request.principal_correlation_id.as_deref() == Some(id)
        }
    }
}

pub(crate) struct WindowActivityGuard {
    registry: WindowActivityRegistry,
    server_trace_id: String,
    continuity_key: Option<WindowContinuityKey>,
    meaningful: bool,
    request_observed_at_ms: i64,
    transition: WindowLoopTransition,
    active: bool,
}

impl WindowActivityGuard {
    pub(crate) fn update(&self, tool_name: Option<&str>, project: Option<&str>) {
        self.registry
            .update(&self.server_trace_id, tool_name, project);
    }

    pub(crate) fn transition(&self) -> WindowLoopTransition {
        self.transition
    }

    pub(crate) fn complete(mut self, timing: RequestCompletionTiming, continuity_eligible: bool) {
        if self.active {
            self.registry.finish(
                &self.server_trace_id,
                self.continuity_key.as_ref(),
                self.meaningful,
                self.request_observed_at_ms,
                Some(timing),
                continuity_eligible,
            );
            self.active = false;
        }
    }
}

impl Drop for WindowActivityGuard {
    fn drop(&mut self) {
        if self.active {
            self.registry.finish(
                &self.server_trace_id,
                self.continuity_key.as_ref(),
                self.meaningful,
                self.request_observed_at_ms,
                None,
                false,
            );
            self.active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(key: &str) -> ClientWindow {
        ClientWindow::for_test(key)
    }

    fn window_key(value: &str) -> String {
        window(value).key().to_string()
    }

    #[test]
    fn live_window_registry_tracks_concurrent_windows_and_raii_cleanup() {
        let registry = WindowActivityRegistry::default();
        let first = registry.start(
            &window("w1"),
            "trace-1",
            "tools/call",
            Some(("username", "alice")),
        );
        first.update(Some("read_files"), Some("agent:r:p"));
        let second = registry.start(
            &window("w2"),
            "trace-2",
            "tools/call",
            Some(("username", "bob")),
        );
        assert_eq!(
            registry
                .list_for_window(&window_key("w1"), Some(("username", "alice")))
                .len(),
            1
        );
        assert_eq!(
            registry
                .list_for_window(&window_key("w2"), Some(("username", "bob")))
                .len(),
            1
        );
        assert_eq!(
            registry
                .counts_by_window(Some(("username", "alice")))
                .get(window_key("w1").as_str()),
            Some(&1)
        );
        drop(first);
        assert!(registry
            .list_for_window(&window_key("w1"), Some(("username", "alice")))
            .is_empty());
        assert_eq!(
            registry
                .list_for_window(&window_key("w2"), Some(("username", "bob")))
                .len(),
            1
        );
        drop(second);
        assert!(registry.counts_by_window(None).is_empty());
    }

    #[test]
    fn live_window_registry_filters_principals_before_key_lookup() {
        let registry = WindowActivityRegistry::default();
        let _alice = registry.start(
            &window("wa"),
            "trace-alice",
            "tools/call",
            Some(("username", "alice")),
        );
        let _bob = registry.start(
            &window("wb"),
            "trace-bob",
            "tools/call",
            Some(("username", "bob")),
        );
        assert_eq!(
            registry
                .list_for_window(&window_key("wa"), Some(("username", "alice")))
                .len(),
            1
        );
        assert!(registry
            .list_for_window(&window_key("wb"), Some(("username", "alice")))
            .is_empty());
        assert_eq!(
            registry
                .counts_by_window(Some(("username", "alice")))
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec![window_key("wa")]
        );
    }

    #[test]
    fn sequential_requests_do_not_leave_phantom_active_state() {
        let registry = WindowActivityRegistry::default();
        {
            let _first = registry.start(
                &window("w"),
                "trace-a",
                "tools/list",
                Some(("username", "alice")),
            );
            assert_eq!(
                registry
                    .list_for_window(&window_key("w"), Some(("username", "alice")))
                    .len(),
                1
            );
        }
        assert!(registry
            .list_for_window(&window_key("w"), Some(("username", "alice")))
            .is_empty());
        {
            let _second = registry.start(
                &window("w"),
                "trace-b",
                "tools/call",
                Some(("username", "alice")),
            );
            assert_eq!(
                registry
                    .list_for_window(&window_key("w"), Some(("username", "alice")))
                    .len(),
                1
            );
        }
        assert!(registry
            .list_for_window(&window_key("w"), Some(("username", "alice")))
            .is_empty());
    }

    fn completion(started_at_ms: i64, response_handed_at_ms: i64) -> RequestCompletionTiming {
        RequestCompletionTiming {
            request_observed_at_ms: started_at_ms,
            response_handed_at_ms,
            elapsed_ms: u64::try_from(response_handed_at_ms - started_at_ms).unwrap(),
        }
    }

    fn meaningful_start(
        registry: &WindowActivityRegistry,
        window: &ClientWindow,
        trace: &str,
        principal: (&str, &str),
        at_ms: i64,
    ) -> WindowActivityGuard {
        registry.start_observed(
            window,
            trace,
            "tools/call",
            Some("read_files"),
            Some(principal),
            at_ms,
        )
    }

    #[test]
    fn meaningful_sequential_gap_uses_previous_response_handoff() {
        let registry = WindowActivityRegistry::default();
        let window = window("sequential");
        let first = meaningful_start(
            &registry,
            &window,
            "trace-first",
            ("username", "alice"),
            1_000,
        );
        assert_eq!(first.transition(), WindowLoopTransition::Unavailable);
        first.complete(completion(1_000, 1_125), true);

        let second = meaningful_start(
            &registry,
            &window,
            "trace-second",
            ("username", "alice"),
            1_500,
        );
        assert_eq!(second.transition().gap_ms(), Some(375));
    }

    #[test]
    fn meaningful_continuity_requires_same_window_and_principal() {
        let registry = WindowActivityRegistry::default();
        let first_window = window("identity-a");
        let second_window = window("identity-b");
        meaningful_start(
            &registry,
            &first_window,
            "trace-first",
            ("username", "alice"),
            1_000,
        )
        .complete(completion(1_000, 1_050), true);

        let different_principal = meaningful_start(
            &registry,
            &first_window,
            "trace-bob",
            ("username", "bob"),
            1_200,
        );
        assert_eq!(
            different_principal.transition(),
            WindowLoopTransition::Unavailable
        );
        drop(different_principal);

        let different_window = meaningful_start(
            &registry,
            &second_window,
            "trace-other-window",
            ("username", "alice"),
            1_300,
        );
        assert_eq!(
            different_window.transition(),
            WindowLoopTransition::Unavailable
        );
    }

    #[test]
    fn discovery_call_does_not_break_meaningful_cadence() {
        let registry = WindowActivityRegistry::default();
        let window = window("meaningful-cadence");
        meaningful_start(
            &registry,
            &window,
            "trace-first",
            ("username", "alice"),
            1_000,
        )
        .complete(completion(1_000, 1_100), true);

        let discovery = registry.start_observed(
            &window,
            "trace-status",
            "tools/call",
            Some("runtime_status"),
            Some(("username", "alice")),
            1_200,
        );
        assert_eq!(discovery.transition(), WindowLoopTransition::Unavailable);
        discovery.complete(completion(1_200, 1_225), true);

        let second = meaningful_start(
            &registry,
            &window,
            "trace-second",
            ("username", "alice"),
            1_500,
        );
        assert_eq!(second.transition().gap_ms(), Some(400));
    }

    #[test]
    fn overlapping_meaningful_calls_never_emit_negative_serial_gap() {
        let registry = WindowActivityRegistry::default();
        let window = window("overlap");
        let first = meaningful_start(
            &registry,
            &window,
            "trace-first",
            ("username", "alice"),
            1_000,
        );
        let second = meaningful_start(
            &registry,
            &window,
            "trace-second",
            ("username", "alice"),
            1_050,
        );
        assert_eq!(second.transition(), WindowLoopTransition::Overlap);
        assert_eq!(second.transition().gap_ms(), None);
        first.complete(completion(1_000, 1_200), true);
        second.complete(completion(1_050, 1_250), true);

        let after_overlap = meaningful_start(
            &registry,
            &window,
            "trace-after-overlap",
            ("username", "alice"),
            1_500,
        );
        assert_eq!(
            after_overlap.transition(),
            WindowLoopTransition::Unavailable,
            "an overlap group must invalidate the serial anchor instead of leaking WebCodex overlap time into an outside gap"
        );
        after_overlap.complete(completion(1_500, 1_550), true);

        let clean_followup = meaningful_start(
            &registry,
            &window,
            "trace-clean-followup",
            ("username", "alice"),
            1_700,
        );
        assert_eq!(clean_followup.transition().gap_ms(), Some(150));
    }

    #[test]
    fn interrupted_meaningful_call_consumes_existing_anchor() {
        for complete_ineligible in [false, true] {
            let registry = WindowActivityRegistry::default();
            let window = window("interrupted");
            let principal = ("username", "alice");
            meaningful_start(&registry, &window, "first", principal, 1_000)
                .complete(completion(1_000, 1_100), true);
            let interrupted = meaningful_start(&registry, &window, "interrupted", principal, 1_200);
            assert_eq!(interrupted.transition().gap_ms(), Some(100));
            if complete_ineligible {
                interrupted.complete(completion(1_200, 1_400), false);
            } else {
                drop(interrupted);
            }
            let next = meaningful_start(&registry, &window, "next", principal, 1_500);
            assert_eq!(next.transition(), WindowLoopTransition::Unavailable);
            next.complete(completion(1_500, 1_600), true);
            let recovered = meaningful_start(&registry, &window, "recovered", principal, 1_700);
            assert_eq!(recovered.transition().gap_ms(), Some(100));
        }
    }

    #[test]
    fn restart_or_ineligible_completion_does_not_invent_continuity() {
        let window = window("restart");
        let registry = WindowActivityRegistry::default();
        meaningful_start(
            &registry,
            &window,
            "trace-stream",
            ("username", "alice"),
            1_000,
        )
        .complete(completion(1_000, 1_100), false);
        let after_stream = meaningful_start(
            &registry,
            &window,
            "trace-after-stream",
            ("username", "alice"),
            1_300,
        );
        assert_eq!(after_stream.transition(), WindowLoopTransition::Unavailable);
        after_stream.complete(completion(1_300, 1_350), true);

        let restarted_registry = WindowActivityRegistry::default();
        let after_restart = meaningful_start(
            &restarted_registry,
            &window,
            "trace-after-restart",
            ("username", "alice"),
            1_600,
        );
        assert_eq!(
            after_restart.transition(),
            WindowLoopTransition::Unavailable
        );
    }
}
