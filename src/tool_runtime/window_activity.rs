use crate::client_window::ClientWindow;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub(crate) const MAX_ACTIVE_WINDOW_REQUESTS: usize = 64;
pub(crate) const MAX_ACTIVE_REQUESTS_PER_WINDOW: usize = 8;

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
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WindowActivityRegistry {
    inner: Arc<Mutex<WindowActivityRegistryInner>>,
}

impl WindowActivityRegistry {
    pub(crate) fn start(
        &self,
        window: &ClientWindow,
        server_trace_id: &str,
        method: &str,
        principal: Option<(&str, &str)>,
    ) -> WindowActivityGuard {
        let record = ActiveWindowRequest {
            client_window_key: window.key().to_string(),
            client_window_source: window.source().to_string(),
            server_trace_id: server_trace_id.to_string(),
            method: method.to_string(),
            tool_name: None,
            project: None,
            principal_correlation_kind: principal.map(|(kind, _)| kind.to_string()),
            principal_correlation_id: principal.map(|(_, id)| id.to_string()),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
        };
        let mut inner = self.inner.lock().expect("Window activity mutex poisoned");
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
        records.truncate(MAX_ACTIVE_REQUESTS_PER_WINDOW);
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

    fn finish(&self, server_trace_id: &str) {
        self.inner
            .lock()
            .expect("Window activity mutex poisoned")
            .by_trace
            .remove(server_trace_id);
    }
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
    active: bool,
}

impl WindowActivityGuard {
    pub(crate) fn update(&self, tool_name: Option<&str>, project: Option<&str>) {
        self.registry
            .update(&self.server_trace_id, tool_name, project);
    }
}

impl Drop for WindowActivityGuard {
    fn drop(&mut self) {
        if self.active {
            self.registry.finish(&self.server_trace_id);
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
}
