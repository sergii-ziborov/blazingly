use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Redacted result class stored for one MCP request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuditOutcome {
    Success,
    Error { code: i32 },
}

/// One metadata-only MCP audit event.
///
/// Arguments, prompt contents, resource contents, and tool results are never
/// captured by this type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditEvent {
    pub method: String,
    pub subject: Option<String>,
    pub session_id: Option<String>,
    pub outcome: AuditOutcome,
}

/// Pluggable sink for redacted MCP audit events.
pub trait AuditSink: Send + Sync + 'static {
    fn record(&self, event: AuditEvent);
}

/// Thread-safe bounded audit log useful for tests and embedded deployments.
#[derive(Clone)]
pub struct BoundedAuditLog {
    capacity: usize,
    events: Arc<Mutex<VecDeque<AuditEvent>>>,
}

impl BoundedAuditLog {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            events: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
        }
    }

    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

impl AuditSink for BoundedAuditLog {
    fn record(&self, event: AuditEvent) {
        if self.capacity == 0 {
            return;
        }
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if events.len() == self.capacity {
            events.pop_front();
        }
        events.push_back(event);
    }
}
