//! Audit declarations only. These policies neither authorize execution nor shape model results.
//! The runtime interprets field rules; Session owns final redaction, bounds and evidence reduction.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolAuditPolicy {
    pub request: AuditRequestPolicy,
    pub result: AuditResultPolicy,
    /// Final ledger context reduction, including restore-time re-projection.
    pub context: AuditContextPolicy,
    /// Whether the existing bounded stdout/stderr excerpt contract applies.
    pub execution: AuditExecutionPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditContextPolicy {
    Omit,
    Fields(&'static [AuditField]),
    WorkingTreeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditExecutionPolicy {
    Omit,
    Text,
    TestCounts,
    TestAssertions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditRequestPolicy {
    pub fields: &'static [AuditField],
    pub transform: AuditTransform,
    /// The historical typed recording stage is sometimes narrower than the raw
    /// request boundary. This controls recording only, never execution arguments.
    pub typed: AuditTypedPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditTypedPolicy {
    Same,
    /// Preserve an intentionally empty typed summary even when raw fields evolve.
    Omit,
    Overrides {
        fields: &'static [AuditField],
        omit: &'static [&'static str],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditResultPolicy {
    Omit,
    Fields(&'static [AuditField]),
    CodingEvents(&'static [AuditField]),
    /// Ephemeral canonical evidence for Session's existing bounded outcome, path,
    /// execution and context reducers. Not permission to persist this value directly.
    /// Must be explicitly selected, and is never a lookup/error fallback.
    SessionEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditTransform {
    Fields,
    ProcessExecution,
    DetachedExecution,
    ScriptExecution,
    JobObservation,
    Checkpoint,
    Edits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditField {
    pub destination: &'static str,
    /// A top-level key or a JSON pointer; ValidationIdentity uses the recipe identity.
    pub source: &'static str,
    pub value: AuditValue,
}

impl AuditField {
    pub const fn new(destination: &'static str, source: &'static str, value: AuditValue) -> Self {
        Self {
            destination,
            source,
            value,
        }
    }
    pub fn is_valid(&self) -> bool {
        !self.destination.is_empty()
            && !self.source.is_empty()
            && !self.destination.contains('/')
            && (!self.source.starts_with('/')
                || self
                    .source
                    .split('~')
                    .skip(1)
                    .all(|s| s.starts_with('0') || s.starts_with('1')))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditValue {
    Copy,
    Nullable,
    KeyPresent,
    Present,
    StringPresent,
    NonemptyString,
    Bytes,
    Chars,
    Count,
    OptionalCount,
    ObjectCount,
    NullableCount,
    NullableBytes,
    Preview,
    /// Bounded activity preview only; the Session ledger must omit it.
    EphemeralPreview,
    ExecutionContext,
    CompletionFingerprint,
    ConsumeTokenPresent,
    RecipientMode,
    AnyPattern,
    ExactCommit,
    ValidationIdentity,
}

impl ToolAuditPolicy {
    pub fn is_valid(&self) -> bool {
        self.request.fields.iter().all(AuditField::is_valid)
            && match self.request.typed {
                AuditTypedPolicy::Same | AuditTypedPolicy::Omit => true,
                AuditTypedPolicy::Overrides { fields, omit } => {
                    fields.iter().all(AuditField::is_valid)
                        && omit.iter().all(|field| !field.is_empty())
                }
            }
            && match self.result {
                AuditResultPolicy::Fields(fields) | AuditResultPolicy::CodingEvents(fields) => {
                    fields.iter().all(AuditField::is_valid)
                }
                AuditResultPolicy::Omit | AuditResultPolicy::SessionEvidence => true,
            }
            && match self.context {
                AuditContextPolicy::Fields(fields) => fields.iter().all(|field| {
                    field.is_valid()
                        && matches!(
                            field.value,
                            AuditValue::Copy
                                | AuditValue::Nullable
                                | AuditValue::NullableCount
                                | AuditValue::NullableBytes
                        )
                }),
                AuditContextPolicy::Omit | AuditContextPolicy::WorkingTreeStatus => true,
            }
    }
}
