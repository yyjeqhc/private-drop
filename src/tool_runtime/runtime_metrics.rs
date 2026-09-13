//! Fail-open, low-cardinality runtime metrics boundary.
//!
//! Metrics consume canonical runtime/transport facts only. They own no
//! authorization, admission, retry, Job, Session, or Window truth and must
//! never affect a tool result.

use super::model_ergonomics_telemetry::ModelErgonomicsRecord;
use super::window_activity::WindowLoopTransition;

#[derive(Debug, Clone, Copy)]
pub(crate) struct McpCallMetricObservation {
    pub(crate) elapsed_ms: u64,
    pub(crate) outcome_class: &'static str,
    pub(crate) meaningful: bool,
    pub(crate) streaming: bool,
}

impl McpCallMetricObservation {
    fn ordinary_completed_response(self) -> bool {
        !self.streaming
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillSourceMetricSource {
    Project,
    RunnerConfigured,
    RunnerManaged,
}

impl SkillSourceMetricSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::RunnerConfigured => "runner_configured",
            Self::RunnerManaged => "runner_managed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillSourceMetricOperation {
    CatalogList,
    CatalogDefinitionRead,
    ExactResolve,
    ResourceRead,
    DefinitionRecheck,
}

impl SkillSourceMetricOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CatalogList => "catalog_list",
            Self::CatalogDefinitionRead => "catalog_definition_read",
            Self::ExactResolve => "exact_resolve",
            Self::ResourceRead => "resource_read",
            Self::DefinitionRecheck => "definition_recheck",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillSourceMetricOutcomeClass {
    Success,
    RunnerError,
    Unavailable,
    InvalidResponse,
}

impl SkillSourceMetricOutcomeClass {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::RunnerError => "runner_error",
            Self::Unavailable => "unavailable",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

/// One issued Skill source request. All dimensions are closed enums; identities,
/// paths, content, queries, and dynamic error strings deliberately have no slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SkillSourceMetricObservation {
    pub(crate) source: SkillSourceMetricSource,
    pub(crate) operation: SkillSourceMetricOperation,
    pub(crate) outcome_class: SkillSourceMetricOutcomeClass,
    pub(crate) elapsed_ms: u64,
    pub(crate) runner_duration_ms: Option<u64>,
    pub(crate) response_bytes: Option<u64>,
    pub(crate) item_count: Option<u64>,
}

pub(crate) trait RuntimeMetrics: std::fmt::Debug + Send + Sync {
    fn observe_tool_call(&self, record: &ModelErgonomicsRecord);
    fn observe_mcp_call(&self, observation: McpCallMetricObservation);
    fn observe_skill_source(&self, observation: SkillSourceMetricObservation);
    fn observe_window_transition(&self, transition: WindowLoopTransition);
}

fn observe_fail_open(operation: &'static str, observe: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(observe)).is_err() {
        tracing::warn!(
            event = "runtime_metrics_observer_failed",
            operation,
            "runtime_metrics_observer_failed"
        );
    }
}

pub(crate) fn observe_tool_call(metrics: &dyn RuntimeMetrics, record: &ModelErgonomicsRecord) {
    observe_fail_open("tool_call", || metrics.observe_tool_call(record));
}

pub(crate) fn observe_mcp_call(
    metrics: &dyn RuntimeMetrics,
    observation: McpCallMetricObservation,
) {
    observe_fail_open("mcp_call", || metrics.observe_mcp_call(observation));
}

pub(crate) fn observe_skill_source(
    metrics: &dyn RuntimeMetrics,
    observation: SkillSourceMetricObservation,
) {
    observe_fail_open("skill_source", || metrics.observe_skill_source(observation));
}

pub(crate) fn observe_window_transition(
    metrics: &dyn RuntimeMetrics,
    transition: WindowLoopTransition,
) {
    observe_fail_open("window_transition", || {
        metrics.observe_window_transition(transition)
    });
}

#[derive(Debug, Default)]
pub(crate) struct TracingRuntimeMetrics;

impl RuntimeMetrics for TracingRuntimeMetrics {
    fn observe_tool_call(&self, record: &ModelErgonomicsRecord) {
        let outcome_class = record.outcome_class();
        tracing::info!(
            metric = "tool_runtime_duration_seconds",
            value = record.duration_ms as f64 / 1000.0,
            tool = record.tool_name,
            tool_category = record.tool_category,
            surface = "runtime",
            outcome_class,
            "runtime_metric"
        );
        tracing::info!(
            metric = "tool_outcomes_total",
            value = 1_u64,
            tool = record.tool_name,
            tool_category = record.tool_category,
            surface = "runtime",
            outcome_class,
            "runtime_metric"
        );
        if let Some(bytes) = record.serialized_result_bytes {
            tracing::info!(
                metric = "tool_result_bytes",
                value = bytes,
                tool = record.tool_name,
                tool_category = record.tool_category,
                surface = "runtime",
                outcome_class,
                "runtime_metric"
            );
        }
    }

    fn observe_mcp_call(&self, observation: McpCallMetricObservation) {
        let response_kind = if observation.streaming {
            "streaming"
        } else {
            "nonstreaming"
        };
        if observation.ordinary_completed_response() {
            tracing::info!(
                metric = "mcp_call_duration_seconds",
                value = observation.elapsed_ms as f64 / 1000.0,
                transport = "mcp",
                outcome_class = observation.outcome_class,
                meaningful = observation.meaningful,
                response_kind,
                "runtime_metric"
            );
            if observation.meaningful {
                tracing::info!(
                    metric = "window_meaningful_calls_total",
                    value = 1_u64,
                    transport = "mcp",
                    outcome_class = observation.outcome_class,
                    response_kind,
                    "runtime_metric"
                );
            }
        }
    }

    fn observe_skill_source(&self, observation: SkillSourceMetricObservation) {
        let source = observation.source.as_str();
        let operation = observation.operation.as_str();
        let outcome_class = observation.outcome_class.as_str();
        tracing::info!(
            metric = "skill_source_requests_total",
            value = 1_u64,
            source,
            operation,
            outcome_class,
            "runtime_metric"
        );
        tracing::info!(
            metric = "skill_source_request_duration_seconds",
            value = observation.elapsed_ms as f64 / 1000.0,
            source,
            operation,
            outcome_class,
            "runtime_metric"
        );
        if let Some(duration_ms) = observation.runner_duration_ms {
            tracing::info!(
                metric = "skill_source_runner_duration_seconds",
                value = duration_ms as f64 / 1000.0,
                source,
                operation,
                outcome_class,
                "runtime_metric"
            );
        }
        if let Some(response_bytes) = observation.response_bytes {
            tracing::info!(
                metric = "skill_source_response_bytes",
                value = response_bytes,
                source,
                operation,
                outcome_class,
                "runtime_metric"
            );
        }
        if let Some(item_count) = observation.item_count {
            tracing::info!(
                metric = "skill_source_returned_items",
                value = item_count,
                source,
                operation,
                outcome_class,
                "runtime_metric"
            );
        }
    }

    fn observe_window_transition(&self, transition: WindowLoopTransition) {
        match transition {
            WindowLoopTransition::Serial { gap_ms } => tracing::info!(
                metric = "window_inter_call_gap_seconds",
                value = gap_ms as f64 / 1000.0,
                transport = "mcp",
                cadence = "meaningful",
                relation = "serial",
                "runtime_metric"
            ),
            WindowLoopTransition::Overlap => tracing::info!(
                metric = "window_overlapping_calls_total",
                value = 1_u64,
                transport = "mcp",
                cadence = "meaningful",
                relation = "overlap",
                "runtime_metric"
            ),
            WindowLoopTransition::Unavailable => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct PanicMetrics;

    impl RuntimeMetrics for PanicMetrics {
        fn observe_tool_call(&self, _record: &ModelErgonomicsRecord) {
            panic!("test metrics sink failure");
        }

        fn observe_mcp_call(&self, _observation: McpCallMetricObservation) {
            panic!("test metrics sink failure");
        }

        fn observe_skill_source(&self, _observation: SkillSourceMetricObservation) {
            panic!("test metrics sink failure");
        }

        fn observe_window_transition(&self, _transition: WindowLoopTransition) {
            panic!("test metrics sink failure");
        }
    }

    #[test]
    fn streaming_mcp_call_is_not_an_ordinary_completed_response_metric() {
        let observation = McpCallMetricObservation {
            elapsed_ms: 10,
            outcome_class: "success",
            meaningful: true,
            streaming: true,
        };
        assert!(!observation.ordinary_completed_response());
    }

    #[test]
    fn skill_source_metric_dimensions_are_closed_and_identity_free() {
        let sources = [
            SkillSourceMetricSource::Project.as_str(),
            SkillSourceMetricSource::RunnerConfigured.as_str(),
            SkillSourceMetricSource::RunnerManaged.as_str(),
        ];
        assert_eq!(sources, ["project", "runner_configured", "runner_managed"]);

        let operations = [
            SkillSourceMetricOperation::CatalogList.as_str(),
            SkillSourceMetricOperation::CatalogDefinitionRead.as_str(),
            SkillSourceMetricOperation::ExactResolve.as_str(),
            SkillSourceMetricOperation::ResourceRead.as_str(),
            SkillSourceMetricOperation::DefinitionRecheck.as_str(),
        ];
        assert_eq!(
            operations,
            [
                "catalog_list",
                "catalog_definition_read",
                "exact_resolve",
                "resource_read",
                "definition_recheck",
            ]
        );

        let outcomes = [
            SkillSourceMetricOutcomeClass::Success.as_str(),
            SkillSourceMetricOutcomeClass::RunnerError.as_str(),
            SkillSourceMetricOutcomeClass::Unavailable.as_str(),
            SkillSourceMetricOutcomeClass::InvalidResponse.as_str(),
        ];
        assert_eq!(
            outcomes,
            ["success", "runner_error", "unavailable", "invalid_response"]
        );

        for label in sources.into_iter().chain(operations).chain(outcomes) {
            assert!(!label.contains("wc_skill_"));
            assert!(!label.contains('/'));
            assert!(!label.contains('\\'));
        }
    }

    #[test]
    fn metrics_sink_panics_are_fail_open() {
        let sink = PanicMetrics;
        observe_mcp_call(
            &sink,
            McpCallMetricObservation {
                elapsed_ms: 10,
                outcome_class: "success",
                meaningful: true,
                streaming: false,
            },
        );
        observe_skill_source(
            &sink,
            SkillSourceMetricObservation {
                source: SkillSourceMetricSource::Project,
                operation: SkillSourceMetricOperation::ExactResolve,
                outcome_class: SkillSourceMetricOutcomeClass::RunnerError,
                elapsed_ms: 10,
                runner_duration_ms: Some(8),
                response_bytes: Some(32),
                item_count: None,
            },
        );
        observe_window_transition(&sink, WindowLoopTransition::Overlap);
    }
}
