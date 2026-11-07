//! The one HTTP client (PLAN.md 3.9, D14).

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use earshot_core::identity::Clock;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use url::Url;

use crate::allowlist::{Allowlist, DenyReason};
use crate::audit::{AuditEntry, AuditLog, AuditOutcome};
use crate::url_policy::host_key;

/// Maximum redirect hops followed per request. Each hop is checked against the allowlist
/// and audited.
pub const MAX_REDIRECTS: usize = 10;

/// Default connect timeout.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default overall timeout per hop when the request sets none. Generous because a local
/// LLM may take minutes; callers can tighten it per request with [`NetRequest::timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Default limit on a response body (64 MiB): enough for LLM JSON, far below anything
/// that should be buffered in memory. Model downloads need a streaming API instead.
pub const DEFAULT_MAX_BODY: u64 = 64 * 1024 * 1024;

/// Wall clock over the OS time, for adapters that have no other [`Clock`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Configuration of a [`NetClient`].
pub struct NetConfig {
    allowlist: Allowlist,
    clock: Arc<dyn Clock + Send + Sync>,
    audit: AuditLog,
    connect_timeout: Duration,
    timeout: Duration,
    max_body: u64,
}

impl NetConfig {
    /// A config with `allowlist`, audit timestamps from `clock`, a fresh audit log and the
    /// default connect timeout.
    pub fn new(allowlist: Allowlist, clock: Arc<dyn Clock + Send + Sync>) -> Self {
        NetConfig {
            allowlist,
            clock,
            audit: AuditLog::new(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            timeout: DEFAULT_TIMEOUT,
            max_body: DEFAULT_MAX_BODY,
        }
    }

    /// Records into `audit` (a shared handle) instead of a fresh log.
    pub fn audit(mut self, audit: AuditLog) -> Self {
        self.audit = audit;
        self
    }

    /// Sets the connect timeout.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets the overall per-hop timeout used when a request sets none.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the response body limit in bytes; a longer body fails with
    /// [`NetError::BodyTooLarge`].
    pub fn max_body(mut self, bytes: u64) -> Self {
        self.max_body = bytes;
        self
    }
}

/// A request to send through [`NetClient::send`]. Its `Debug` output redacts secret
/// headers ([`is_secret_header`]) and shows the URL without credentials, query or
/// fragment.
#[derive(Clone)]
pub struct NetRequest {
    method: Method,
    url: Url,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Option<Duration>,
    range_from: Option<u64>,
}

impl NetRequest {
    /// A request with no headers and an empty body.
    pub fn new(method: Method, url: Url) -> Self {
        NetRequest {
            method,
            url,
            headers: Vec::new(),
            body: Vec::new(),
            timeout: None,
            range_from: None,
        }
    }

    /// Asks for the body from byte `offset` on (`Range: bytes=<offset>-`, a download
    /// resume). Unlike a caller header ([`NetRequest::header`]), which a cross-origin
    /// redirect drops, this one is built here, carries no secret and is sent on every hop
    /// (a model host redirects to its CDN, another origin).
    pub fn range_from(mut self, offset: u64) -> Self {
        self.range_from = Some(offset);
        self
    }

    /// A `GET` request.
    pub fn get(url: Url) -> Self {
        Self::new(Method::GET, url)
    }

    /// Adds a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Sets the body.
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Overall timeout for each hop (connect, send, read the whole body).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

impl fmt::Debug for NetRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let v = if is_secret_header(k) {
                    "<redacted>"
                } else {
                    v.as_str()
                };
                (k.as_str(), v)
            })
            .collect();
        f.debug_struct("NetRequest")
            .field("method", &self.method)
            .field("url", &redact(&self.url))
            .field("headers", &headers)
            .field("body_bytes", &self.body.len())
            .field("timeout", &self.timeout)
            .field("range_from", &self.range_from)
            .finish()
    }
}

/// Whether a header carries a secret and must never be printed: `authorization`,
/// `proxy-authorization`, `cookie`, `set-cookie` and any name ending in `api-key` or
/// `api_key` (`x-api-key`, `api-key`, ...), compared case-insensitively.
pub fn is_secret_header(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(
        n.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) || n.ends_with("api-key")
        || n.ends_with("api_key")
}

/// A fully read response.
#[derive(Debug, Clone)]
pub struct NetResponse {
    /// HTTP status of the final hop.
    pub status: u16,
    /// Headers of the final hop.
    pub headers: HeaderMap,
    /// Whole body of the final hop.
    pub body: Vec<u8>,
    /// URL of the final hop (after redirects).
    pub url: Url,
}

/// Audit message of a response body not read to its end: a [`NetStream`] dropped early, or
/// a [`NetClient::send`] / [`NetClient::open`] future dropped while reading a body.
pub const STREAM_DROPPED: &str = "body not read to the end";

/// Audit message of a hop whose request future was dropped after the allowlist let it go
/// and before a response arrived (the request may have been sent).
pub const CANCELLED_BEFORE_RESPONSE: &str = "cancelled before a response";

/// The audit entry of a hop in flight. It is recorded exactly once: by
/// [`PendingEntry::complete`] or [`PendingEntry::fail`], handed on by
/// [`PendingEntry::disarm`], or, when the future holding it is dropped, as
/// [`AuditOutcome::Failed`] with `on_drop`.
struct PendingEntry {
    /// `Some` until it is recorded or handed on.
    entry: Option<AuditEntry>,
    audit: AuditLog,
    on_drop: &'static str,
}

impl PendingEntry {
    fn new(entry: AuditEntry, audit: AuditLog, on_drop: &'static str) -> Self {
        Self {
            entry: Some(entry),
            audit,
            on_drop,
        }
    }

    fn entry_mut(&mut self) -> &mut AuditEntry {
        // Only `Drop` sees `None`: every other way out consumes `self`.
        self.entry
            .as_mut()
            .expect("a pending entry is held until consumed")
    }

    fn record(&mut self, outcome: AuditOutcome) {
        if let Some(mut entry) = self.entry.take() {
            entry.outcome = outcome;
            self.audit.record(entry);
        }
    }

    fn complete(mut self) {
        self.record(AuditOutcome::Completed);
    }

    /// Records the hop as failed with `err`'s message and returns `err`.
    fn fail(mut self, err: NetError) -> NetError {
        self.record(AuditOutcome::Failed(err.to_string()));
        err
    }

    /// The entry, unrecorded, for a new owner that records it ([`NetStream`] or `send`).
    fn disarm(mut self) -> AuditEntry {
        self.entry
            .take()
            .expect("a pending entry is held until consumed")
    }
}

impl Drop for PendingEntry {
    fn drop(&mut self) {
        self.record(AuditOutcome::Failed(self.on_drop.to_string()));
    }
}

/// A response whose body is read piece by piece ([`NetClient::open`]), with no limit on
/// its size. Its `Debug` output shows status and the URL without credentials, query or
/// fragment, never headers.
pub struct NetStream {
    status: u16,
    headers: HeaderMap,
    content_length: Option<u64>,
    url: Url,
    /// `None` once the body ended or a read failed.
    response: Option<reqwest::Response>,
    /// The final hop's entry, recorded exactly once (end, failure or drop).
    entry: Option<AuditEntry>,
    failed: bool,
    audit: AuditLog,
}

impl NetStream {
    /// HTTP status of the final hop.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Headers of the final hop.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The body length the final hop announced (`Content-Length`), if any.
    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// URL of the final hop (after redirects).
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The next piece of the body; `Ok(None)` at its end. After a failed read the stream
    /// is finished and every later call returns [`NetError::StreamFailed`]. Dropping the
    /// future cancels only that read.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        let Some(resp) = self.response.as_mut() else {
            return if self.failed {
                Err(NetError::StreamFailed)
            } else {
                Ok(None)
            };
        };
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if let Some(entry) = self.entry.as_mut() {
                    entry.response_bytes += chunk.len() as u64;
                }
                Ok(Some(Vec::from(chunk)))
            }
            Ok(None) => {
                self.response = None;
                self.finish(AuditOutcome::Completed);
                Ok(None)
            }
            Err(e) => {
                let err = transport(e);
                self.response = None;
                self.failed = true;
                self.finish(AuditOutcome::Failed(err.to_string()));
                Err(err)
            }
        }
    }

    fn finish(&mut self, outcome: AuditOutcome) {
        if let Some(mut entry) = self.entry.take() {
            entry.outcome = outcome;
            self.audit.record(entry);
        }
    }
}

impl Drop for NetStream {
    fn drop(&mut self) {
        self.finish(AuditOutcome::Failed(STREAM_DROPPED.to_string()));
    }
}

impl fmt::Debug for NetStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetStream")
            .field("status", &self.status)
            .field("url", &redact(&self.url))
            .field("content_length", &self.content_length)
            .finish_non_exhaustive()
    }
}

/// Errors from [`NetClient`].
#[derive(Debug)]
pub enum NetError {
    /// The client could not be built (TLS configuration).
    Build(String),
    /// A header name or value is invalid.
    InvalidHeader(String),
    /// The request (or a redirect hop) was refused by the allowlist; nothing was sent to
    /// `url`.
    NotAllowed {
        /// The refused URL, without query or fragment.
        url: String,
        /// Why it was refused.
        reason: DenyReason,
    },
    /// More than [`MAX_REDIRECTS`] redirects.
    TooManyRedirects,
    /// A redirect without a usable `Location`.
    BadRedirect(String),
    /// The response body exceeded the configured limit ([`NetConfig::max_body`]).
    BodyTooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// Connect, TLS, timeout or body read failure. The URL is stripped from the error
    /// (`reqwest::Error::without_url`) so queries never reach logs or messages.
    Transport(reqwest::Error),
    /// A [`NetStream`] read after an earlier read of it failed.
    StreamFailed,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Build(e) => write!(f, "cannot build the HTTP client: {e}"),
            NetError::InvalidHeader(e) => write!(f, "invalid header: {e}"),
            NetError::NotAllowed { url, reason } => write!(f, "request to {url} refused: {reason}"),
            NetError::TooManyRedirects => write!(f, "more than {MAX_REDIRECTS} redirects"),
            NetError::BadRedirect(e) => write!(f, "bad redirect: {e}"),
            NetError::BodyTooLarge { limit } => {
                write!(f, "response body is larger than {limit} bytes")
            }
            NetError::Transport(e) => write!(f, "request failed: {e}"),
            NetError::StreamFailed => write!(f, "the response body already failed to read"),
        }
    }
}

impl std::error::Error for NetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NetError::Transport(e) => Some(e),
            _ => None,
        }
    }
}

/// The only HTTP client shipped crates may use. Async (reqwest on tokio): call it from a
/// tokio runtime, which Tauri provides and which the runtime crate can run on a worker
/// thread. Dropping the future of [`NetClient::send`] cancels the request.
///
/// Every request and every redirect hop is checked against the [`Allowlist`] and recorded
/// in the [`AuditLog`]. Redirects are followed here, not by reqwest, so each hop is
/// visible. System proxies are ignored (a proxy would be an unlisted destination), and
/// automatic retries are off (each attempt must be audited).
#[derive(Clone)]
pub struct NetClient {
    inner: reqwest::Client,
    allowlist: Arc<Allowlist>,
    clock: Arc<dyn Clock + Send + Sync>,
    audit: AuditLog,
    timeout: Duration,
    max_body: u64,
}

impl fmt::Debug for NetClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetClient")
            .field("allowlist", &self.allowlist)
            .finish_non_exhaustive()
    }
}

fn tls_config() -> Result<rustls::ClientConfig, NetError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| NetError::Build(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

impl NetClient {
    /// Builds the client.
    pub fn new(config: NetConfig) -> Result<Self, NetError> {
        let inner = reqwest::Client::builder()
            .tls_backend_preconfigured(tls_config()?)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .referer(false)
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(|e| NetError::Build(e.to_string()))?;
        Ok(NetClient {
            inner,
            allowlist: Arc::new(config.allowlist),
            clock: config.clock,
            audit: config.audit,
            timeout: config.timeout,
            max_body: config.max_body,
        })
    }

    /// The allowlist this client enforces.
    pub fn allowlist(&self) -> &Allowlist {
        &self.allowlist
    }

    /// A handle to this client's audit log.
    pub fn audit_log(&self) -> AuditLog {
        self.audit.clone()
    }

    /// Sends `req`, following up to [`MAX_REDIRECTS`] redirects, each hop checked against
    /// the allowlist, and reads the whole response body (at most [`NetConfig::max_body`]).
    pub async fn send(&self, req: NetRequest) -> Result<NetResponse, NetError> {
        let (mut resp, entry, url) = self.follow(req).await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let mut pending = PendingEntry::new(entry, self.audit.clone(), STREAM_DROPPED);
        let body = match self.read_body(&mut resp, pending.entry_mut()).await {
            Ok(body) => body,
            Err(e) => return Err(pending.fail(e)),
        };
        pending.complete();
        Ok(NetResponse {
            status,
            headers,
            body,
            url,
        })
    }

    /// Sends `req` like [`NetClient::send`] (same redirects, allowlist checks and audit
    /// entries; redirect bodies are read and discarded within [`NetConfig::max_body`]),
    /// but returns the final hop with its body unread, to be read piece by piece with
    /// [`NetStream::chunk`] and without the `max_body` limit (model downloads). The
    /// request's timeout ([`NetRequest::timeout`], else [`NetConfig::timeout`]) still
    /// covers each hop from connect until its whole body has been read, so size it for
    /// the whole transfer.
    ///
    /// The final hop's [`AuditEntry`] is recorded once, with the body bytes read: when the
    /// body ends ([`AuditOutcome::Completed`]), when a read fails, or when the stream is
    /// dropped before its end ([`AuditOutcome::Failed`] with [`STREAM_DROPPED`]).
    pub async fn open(&self, req: NetRequest) -> Result<NetStream, NetError> {
        let (resp, entry, url) = self.follow(req).await?;
        Ok(NetStream {
            status: resp.status().as_u16(),
            headers: resp.headers().clone(),
            content_length: resp.content_length(),
            url,
            response: Some(resp),
            entry: Some(entry),
            failed: false,
            audit: self.audit.clone(),
        })
    }

    /// The hop loop shared by [`NetClient::send`] and [`NetClient::open`]: checks every
    /// hop with [`Allowlist::check_hop`], records each redirect hop (its body read within
    /// `max_body` and discarded) and returns the first non-redirect response with its body
    /// unread and its audit entry not yet recorded.
    async fn follow(
        &self,
        req: NetRequest,
    ) -> Result<(reqwest::Response, AuditEntry, Url), NetError> {
        let mut headers = HeaderMap::new();
        for (k, v) in &req.headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| NetError::InvalidHeader(format!("{k}: {e}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| NetError::InvalidHeader(format!("{k}: {e}")))?;
            headers.append(name, value);
        }
        let mut method = req.method;
        let mut url = req.url;
        let mut body = req.body;

        for hop in 0..=MAX_REDIRECTS {
            let mut entry = self.entry(&method, &url);
            if let Err(reason) = self.allowlist.check_hop(&url, hop) {
                entry.outcome = AuditOutcome::Blocked(reason);
                self.audit.record(entry);
                return Err(NetError::NotAllowed {
                    url: redact(&url),
                    reason,
                });
            }

            let mut rb = self
                .inner
                .request(method.clone(), url.clone())
                .headers(headers.clone());
            if !body.is_empty() {
                rb = rb.body(body.clone());
            }
            if let Some(from) = req.range_from {
                rb = rb.header(header::RANGE, format!("bytes={from}-"));
            }
            rb = rb.timeout(req.timeout.unwrap_or(self.timeout));
            entry.request_bytes = body.len() as u64;

            // From here the request may be on the wire: a dropped future still records
            // this hop, exactly once.
            let mut pending =
                PendingEntry::new(entry, self.audit.clone(), CANCELLED_BEFORE_RESPONSE);
            let mut resp = match rb.send().await {
                Ok(r) => r,
                Err(e) => return Err(pending.fail(transport(e))),
            };
            let status = resp.status();
            pending.entry_mut().status = Some(status.as_u16());
            if !is_followed_redirect(status) {
                return Ok((resp, pending.disarm(), url));
            }
            pending.on_drop = STREAM_DROPPED;
            let resp_headers = resp.headers().clone();
            if let Err(e) = self.read_body(&mut resp, pending.entry_mut()).await {
                return Err(pending.fail(e));
            }
            pending.complete();

            let location = resp_headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| NetError::BadRedirect(format!("{status} without Location")))?;
            let next = url
                .join(location)
                // The raw Location may carry a query; it is never echoed.
                .map_err(|e| {
                    NetError::BadRedirect(format!("{status} with an invalid Location: {e}"))
                })?;

            // RFC 9110 15.4: 303 always becomes GET; 301/302 turn POST into GET (what
            // browsers and reqwest do); 307/308 keep method and body.
            let to_get = status == StatusCode::SEE_OTHER
                || (method == Method::POST
                    && matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND));
            if to_get {
                if method != Method::HEAD {
                    method = Method::GET;
                }
                body.clear();
                for h in [
                    header::CONTENT_TYPE,
                    header::CONTENT_LENGTH,
                    header::CONTENT_ENCODING,
                ] {
                    headers.remove(h);
                }
            }
            if next.origin() != url.origin() {
                // Another origin never sees caller headers (API keys come under many
                // names: authorization, api-key, x-api-key). Only Content-Type survives,
                // and only while the body is still forwarded (307/308).
                let content_type = headers.get(header::CONTENT_TYPE).cloned();
                headers.clear();
                if let Some(ct) = content_type.filter(|_| !body.is_empty()) {
                    headers.insert(header::CONTENT_TYPE, ct);
                }
            }
            url = next;
        }
        Err(NetError::TooManyRedirects)
    }

    /// Reads the whole body of `resp` within `max_body`, counting it in `entry`. The
    /// caller records `entry` (through its [`PendingEntry`]).
    async fn read_body(
        &self,
        resp: &mut reqwest::Response,
        entry: &mut AuditEntry,
    ) -> Result<Vec<u8>, NetError> {
        if resp.content_length().is_some_and(|n| n > self.max_body) {
            return Err(self.too_large());
        }
        let mut bytes = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    entry.response_bytes += chunk.len() as u64;
                    if entry.response_bytes > self.max_body {
                        return Err(self.too_large());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(bytes),
                Err(e) => return Err(transport(e)),
            }
        }
    }

    fn too_large(&self) -> NetError {
        NetError::BodyTooLarge {
            limit: self.max_body,
        }
    }

    fn entry(&self, method: &Method, url: &Url) -> AuditEntry {
        AuditEntry {
            timestamp_ms: self.clock.now_unix_ms(),
            method: method.as_str().to_string(),
            scheme: url.scheme().to_string(),
            host: url.host().map(|h| host_key(&h)).unwrap_or_default(),
            port: url.port_or_known_default().unwrap_or(0),
            path: url.path().to_string(),
            request_bytes: 0,
            response_bytes: 0,
            status: None,
            outcome: AuditOutcome::Completed,
        }
    }
}

/// The only way to build [`NetError::Transport`]: reqwest's Display and Debug include the
/// full URL (query too), so it is stripped first.
fn transport(e: reqwest::Error) -> NetError {
    NetError::Transport(e.without_url())
}

fn is_followed_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

/// `url` without credentials, query or fragment: the only form in which a URL may appear
/// in an error, a log line or a message (CDN redirect URLs carry signed queries).
pub fn redact(url: &Url) -> String {
    let mut u = url.clone();
    u.set_query(None);
    u.set_fragment(None);
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_reads_a_plausible_time() {
        // 2020-01-01 .. 2200-01-01
        let now = SystemClock.now_unix_ms();
        assert!(
            (1_577_836_800_000..7_258_118_400_000).contains(&now),
            "{now}"
        );
    }

    #[test]
    fn redact_drops_secrets() {
        let u = Url::parse("https://u:p@h.example/p?key=1#f").unwrap();
        assert_eq!(redact(&u), "https://h.example/p");
    }

    #[test]
    fn only_the_five_redirect_codes_are_followed() {
        for c in [301, 302, 303, 307, 308] {
            assert!(
                is_followed_redirect(StatusCode::from_u16(c).unwrap()),
                "{c}"
            );
        }
        for c in [200, 300, 304, 305, 306, 400] {
            assert!(
                !is_followed_redirect(StatusCode::from_u16(c).unwrap()),
                "{c}"
            );
        }
    }

    #[test]
    fn request_debug_redacts_secret_headers_and_the_query() {
        let req = NetRequest::get(Url::parse("https://u:p@h.example/v1?key=q1#f").unwrap())
            .header("Authorization", "Bearer sk-SECRET1")
            .header("proxy-authorization", "Basic SECRET2")
            .header("Cookie", "session=SECRET3")
            .header("X-Api-Key", "SECRET4")
            .header("api-key", "SECRET5")
            .header("content-type", "application/json")
            .body(b"{\"x\":\"SECRET6\"}".to_vec());
        let dbg = format!("{req:?}");
        for secret in ["SECRET", "q1", "u:p"] {
            assert!(!dbg.contains(secret), "{secret} in {dbg}");
        }
        assert!(dbg.contains("application/json"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert!(is_secret_header(" X-API-KEY "));
        assert!(is_secret_header("openai_api_key"));
        assert!(!is_secret_header("content-type"));
    }

    #[test]
    fn client_builds_with_rustls_and_ring() {
        let c = NetClient::new(NetConfig::new(Allowlist::new(), Arc::new(SystemClock)));
        assert!(c.is_ok(), "{c:?}");
    }
}
