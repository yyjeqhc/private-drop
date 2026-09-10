use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const ACTIVITY_LIMIT: usize = 200;
const ACTIVITY_MESSAGE_BYTES: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityEventKind {
    ProcessStarted,
    ProcessExited,
    ProcessObservationFailed,
    ProcessStopping,
    ProcessStopped,
    LocalSetupPreparing,
    LocalRuntimeReady,
    RemoteConnecting,
    RemoteConnected,
    QuickShareStarting,
    QuickShareReady,
    QuickShareStopped,
    RegularTunnelStarting,
    RegularTunnelReady,
    RegularTunnelStopped,
    RuntimeStopped,
    StateRecovered,
    ProjectActivated,
    OperationStarted,
    OperationCancelRequested,
    OperationCancelled,
    OperationFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityEntry {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub source: String,
    pub level: ActivityLevel,
    pub event_kind: ActivityEventKind,
    pub message: String,
}

#[derive(Clone, Default)]
pub struct ActivityLog {
    inner: Arc<Mutex<ActivityInner>>,
}

#[derive(Default)]
struct ActivityInner {
    next_sequence: u64,
    entries: VecDeque<ActivityEntry>,
}

impl ActivityLog {
    pub fn push(
        &self,
        event_kind: ActivityEventKind,
        source: impl Into<String>,
        level: ActivityLevel,
        message: impl AsRef<str>,
    ) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        let entry = ActivityEntry {
            sequence: inner.next_sequence,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            source: source.into(),
            level,
            event_kind,
            message: sanitize_message(message.as_ref()),
        };
        inner.entries.push_back(entry);
        while inner.entries.len() > ACTIVITY_LIMIT {
            inner.entries.pop_front();
        }
    }

    pub fn snapshot(&self) -> Vec<ActivityEntry> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .iter()
            .cloned()
            .collect()
    }

    pub fn latest_sequence(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .next_sequence
    }
}

pub fn sanitize_message(message: &str) -> String {
    let mut safe = message
        .replace("Authorization: Bearer ", "Authorization: [redacted]")
        .replace("authorization: bearer ", "authorization: [redacted]");
    for prefix in ["wc_pair_", "wc_pat_", "wc_agent_", "webcodex_temporary_"] {
        safe = redact_prefixed_token(&safe, prefix);
    }
    truncate_utf8(&safe, ACTIVITY_MESSAGE_BYTES)
}

fn redact_prefixed_token(value: &str, prefix: &str) -> String {
    let mut rest = value;
    let mut out = String::with_capacity(value.len());
    while let Some(index) = rest.find(prefix) {
        out.push_str(&rest[..index]);
        out.push_str("[redacted]");
        let tail = &rest[index + prefix.len()..];
        let consumed = tail
            .char_indices()
            .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
            .map(|(offset, ch)| offset + ch.len_utf8())
            .last()
            .unwrap_or(0);
        rest = &tail[consumed..];
    }
    out.push_str(rest);
    out
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_redacts_runtime_credentials() {
        let safe = sanitize_message(
            "Authorization: Bearer abc wc_pair_secret wc_pat_secret wc_agent_secret webcodex_temporary_secret",
        );
        assert!(!safe.contains("secret"));
        assert!(!safe.contains("Bearer abc"));
        assert!(safe.matches("[redacted]").count() >= 5);
    }

    #[test]
    fn activity_history_is_bounded() {
        let log = ActivityLog::default();
        for index in 0..250 {
            log.push(
                ActivityEventKind::ProcessStarted,
                "test",
                ActivityLevel::Info,
                format!("message {index}"),
            );
        }
        let entries = log.snapshot();
        assert_eq!(entries.len(), ACTIVITY_LIMIT);
        assert_eq!(entries.first().unwrap().sequence, 51);
        assert_eq!(entries.last().unwrap().sequence, 250);
    }
}
