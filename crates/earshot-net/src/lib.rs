//! The only crate allowed to make HTTP requests (PLAN.md 3.1, 3.9, decision D14).
//!
//! - [`NetClient`]: one reqwest + rustls (ring, bundled Mozilla roots) client; no
//!   native-tls/OpenSSL, no system proxy, no automatic retries.
//! - [`Allowlist`]: exact `(host, port)` pairs from the manifest hosts, the configured LLM
//!   URL and (later) the sync server, plus redirect-only entries (the manifests'
//!   `redirect_hosts`, valid from the first redirect hop on). Checked on every request and
//!   every redirect hop; plain `http` only to loopback or private LAN hosts (PLAN.md 3.7).
//! - [`NetStream`]: a response body read piece by piece ([`NetClient::open`]), for model
//!   downloads that must not be buffered.
//! - [`AuditLog`]: every attempt (method, scheme, host, port, path, byte counts, status,
//!   injected-clock timestamp), readable by tests.
//! - [`validate_url`]: endpoint URL validation (http/https only, host present).
//!
//! No other shipped crate may depend on an HTTP client; `cargo xtask ci` enforces this
//! with its `net` step (`xtask/src/net_guard.rs`).

#![forbid(unsafe_code)]

mod allowlist;
mod audit;
mod client;
mod url_policy;

pub use allowlist::{Allowlist, DenyReason};
pub use audit::{AuditEntry, AuditLog, AuditOutcome};
pub use client::{
    CANCELLED_BEFORE_RESPONSE, DEFAULT_CONNECT_TIMEOUT, DEFAULT_MAX_BODY, DEFAULT_TIMEOUT,
    MAX_REDIRECTS, NetClient, NetConfig, NetError, NetRequest, NetResponse, NetStream,
    STREAM_DROPPED, SystemClock, is_secret_header, redact,
};
pub use reqwest::Method;
pub use reqwest::header;
pub use url::Url;
pub use url_policy::{UrlError, check_url_shape, is_local_host, is_local_url, validate_url};
