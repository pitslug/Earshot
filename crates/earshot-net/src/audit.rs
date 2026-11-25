//! Audit log of every request attempt (PLAN.md 3.9): what tests use to prove what left
//! the machine.

use std::collections::VecDeque;
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

/// The most entries an [`AuditLog`] keeps (T14.32): the newest are kept and older ones are
/// dropped and counted ([`AuditLog::dropped`]), so a client that lives as long as the app (the
/// sync client: roughly 1 MB of entries a day before this bound) cannot grow without limit.
///
/// Sized above the most one run of any reader can write, so every reader that prints or checks
/// a run's entries still sees them all:
/// - one summary: each step (map or merge) is at most 1 request + 2 format fallbacks +
///   `MAX_REPAIRS` (2) repairs + 1 grown retry = 6 requests; a ten-hour meeting on the smallest
///   endpoint budget (8,192 tokens) is about 30 steps, so about 180 requests, each at most
///   `MAX_REDIRECTS + 1` = 11 hops: under 2,000 entries (a summary service is built per run);
/// - one session's downloads: every model and runtime artefact of the catalogue, each at most
///   11 hops, with resumes, stays well under that too;
/// - a `sum_eval` run (one log for every meeting of a run, at most ten meetings of a few
///   requests each) and a replay case are far smaller.
///
/// At about 200 bytes an entry, a full log holds under 1 MB.
pub const MAX_AUDIT_ENTRIES: usize = 4096;

#[derive(Debug)]
struct Kept {
    entries: VecDeque<AuditEntry>,
    dropped: u64,
    limit: usize,
}

/// Shared in-memory audit log of the newest [`MAX_AUDIT_ENTRIES`] entries (oldest dropped and
/// counted). Cloning gives another handle to the same log.
///
/// It lives for the lifetime of the client(s) that write to it; persisting it is the
/// runtime's job.
#[derive(Debug, Clone)]
pub struct AuditLog {
    kept: Arc<Mutex<Kept>>,
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::with_limit(MAX_AUDIT_ENTRIES)
    }
}

impl AuditLog {
    /// An empty log keeping at most [`MAX_AUDIT_ENTRIES`] entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty log keeping at most `limit` entries (at least one).
    pub fn with_limit(limit: usize) -> Self {
        Self {
            kept: Arc::new(Mutex::new(Kept {
                entries: VecDeque::new(),
                dropped: 0,
                limit: limit.max(1),
            })),
        }
    }

    /// Appends an entry; when the log is full the oldest entry is dropped and counted.
    pub fn record(&self, entry: AuditEntry) {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.entries.len() >= kept.limit {
            kept.entries.pop_front();
            kept.dropped = kept.dropped.saturating_add(1);
        }
        kept.entries.push_back(entry);
    }

    /// A snapshot of the kept entries, oldest first.
    pub fn entries(&self) -> Vec<AuditEntry> {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .iter()
            .cloned()
            .collect()
    }

    /// Number of kept entries.
    pub fn len(&self) -> usize {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .len()
    }

    /// Whether no entry is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many older entries were dropped to keep the log within its limit.
    pub fn dropped(&self) -> u64 {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u64) -> AuditEntry {
        AuditEntry {
            timestamp_ms: n,
            method: "GET".into(),
            scheme: "https".into(),
            host: "relay.example.invalid".into(),
            port: 443,
            path: "/v1/pull".into(),
            request_bytes: 0,
            response_bytes: 0,
            status: Some(200),
            outcome: AuditOutcome::Completed,
        }
    }

    #[test]
    fn audit_log_keeps_the_newest_entries_and_counts_the_dropped() {
        let log = AuditLog::with_limit(3);
        let other = log.clone();
        for n in 0..3 {
            log.record(entry(n));
        }
        assert_eq!(log.dropped(), 0);
        assert_eq!(log.len(), 3);
        for n in 3..8 {
            other.record(entry(n));
        }
        let kept: Vec<u64> = log.entries().iter().map(|e| e.timestamp_ms).collect();
        assert_eq!(kept, [5, 6, 7], "the newest, oldest first");
        assert_eq!(log.dropped(), 5);
        assert_eq!(other.dropped(), 5, "clones share the log");
        assert!(!log.is_empty());
        // A zero limit still keeps the latest entry.
        let one = AuditLog::with_limit(0);
        one.record(entry(1));
        one.record(entry(2));
        assert_eq!(one.entries(), [entry(2)]);
        assert_eq!(one.dropped(), 1);
    }

    #[test]
    fn audit_log_default_limit_is_the_named_constant() {
        let log = AuditLog::new();
        for n in 0..=MAX_AUDIT_ENTRIES as u64 {
            log.record(entry(n));
        }
        assert_eq!(log.len(), MAX_AUDIT_ENTRIES);
        assert_eq!(log.dropped(), 1);
        assert_eq!(log.entries()[0].timestamp_ms, 1);
    }

    /// A client built without a log of its own (the sync relay's, downloads', updates') gets
    /// the bounded one.
    #[test]
    fn audit_log_of_a_client_without_its_own_is_bounded() {
        let net = crate::NetClient::new(crate::NetConfig::new(
            crate::Allowlist::new(),
            Arc::new(crate::SystemClock),
        ))
        .unwrap();
        let log = net.audit_log();
        for n in 0..(MAX_AUDIT_ENTRIES as u64 + 10) {
            log.record(entry(n));
        }
        assert_eq!(net.audit_log().len(), MAX_AUDIT_ENTRIES);
        assert_eq!(net.audit_log().dropped(), 10);
    }
}
