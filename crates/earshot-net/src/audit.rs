//! Audit log of every request attempt (PLAN.md 3.9): what tests use to prove what left
//! the machine.

use std::sync::{Arc, Mutex, PoisonError};

use crate::allowlist::DenyReason;

/// How a request attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOutcome {
    /// A response was received and its body read (any status, including redirects).
    Completed,
    /// Refused by the allowlist before any connection was made; nothing was sent.
    Blocked(DenyReason),
    /// The transport failed (connect, TLS, timeout, body read). The request may have been
    /// partly sent.
    Failed(String),
}

/// One request attempt: one entry per hop, so a followed redirect produces several.
///
/// The query string and fragment are never recorded (they may carry secrets). Byte counts
/// are body bytes only: `request_bytes` is the body handed to the transport (0 when
/// blocked), `response_bytes` the body bytes read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Injected clock reading when the attempt started (Unix milliseconds).
    pub timestamp_ms: u64,
    /// HTTP method, upper case.
    pub method: String,
    /// URL scheme (`http`/`https`, or whatever a refused URL had).
    pub scheme: String,
    /// Canonical host (lower-case domain, IPv4, or IPv6 without brackets); empty if none.
    pub host: String,
    /// Port, explicit or the scheme default (0 if unknown).
    pub port: u16,
    /// URL path only.
    pub path: String,
    /// Request body bytes handed to the transport.
    pub request_bytes: u64,
    /// Response body bytes read.
    pub response_bytes: u64,
    /// HTTP status, when a response arrived.
    pub status: Option<u16>,
    /// How the attempt ended.
    pub outcome: AuditOutcome,
}

/// Shared, append-only, in-memory audit log. Cloning gives another handle to the same log.
///
/// It lives for the lifetime of the client(s) that write to it; persisting or trimming it
/// is the runtime's job.
#[derive(Debug, Clone, Default)]
pub struct AuditLog {
    entries: Arc<Mutex<Vec<AuditEntry>>>,
}

impl AuditLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry.
    pub fn record(&self, entry: AuditEntry) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }

    /// A snapshot of all entries, oldest first.
    pub fn entries(&self) -> Vec<AuditEntry> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
