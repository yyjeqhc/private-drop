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

pub(crate) trait RuntimeMetrics: std::fmt::Debug + Send + Sync {
    fn observe_tool_call(&self, record: &ModelErgonomicsRecord);
    fn observe_mcp_call(&self, observation: McpCallMetricObservation);
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
        observe_window_transition(&sink, WindowLoopTransition::Overlap);
    }
}
