//! In-app model downloads (T7.4a): the rules of xtask's `download::fetch`, through
//! `earshot-net` (PLAN.md 3.9, D14).
//!
//! - Every request goes through a [`NetClient`] whose allowlist comes from the embedded
//!   `models/manifest.toml` ([`manifest_allowlist`]): its `hosts` for the first request,
//!   those or its `redirect_hosts` (the hosts' CDNs) on every redirect hop.
//! - The body streams into `<dest>.part` ([`NetClient::open`], no `max_body` buffering).
//!   The file is renamed to `dest` only after its size and SHA-256 match.
//! - A leftover `.part` is resumed with `Range: bytes=<have>-`. A 206 is trusted only when
//!   its `Content-Range` starts at exactly `have`; a 200 restarts the file; any other
//!   status fails. A `.part` larger than the expected size is deleted first.
//! - A transfer that ends early (connection closed or lost, the deadline, cancellation)
//!   keeps the `.part` and says "run again to resume"; only a complete file that fails
//!   verification deletes it.
//! - Errors and messages show URLs only through [`earshot_net::redact`].

use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
/// What [`Downloader::new`] and [`Downloader::fetch`] take.
pub use earshot_net::{Allowlist, Url};
use earshot_net::{AuditLog, NetClient, NetConfig, NetError, NetRequest, UrlError, redact};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::SharedClock;

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;

/// `models/manifest.toml`, embedded so the download hosts cannot drift from it.
pub(crate) const MODELS_MANIFEST: &str = include_str!("../../../models/manifest.toml");

/// Fixed part of a transfer's deadline. Same value and rule as xtask's
/// `download::BODY_DEADLINE_BASE` and `MIN_BODY_RATE`: the deadline is this plus the whole
/// file at [`MIN_BODY_RATE`], so a hung transfer ends with the `.part` kept for a resume
/// instead of blocking forever.
pub const BODY_DEADLINE_BASE: Duration = Duration::from_secs(120);

/// Slowest transfer rate the deadline allows for, in bytes per second (128 KiB/s).
pub const MIN_BODY_RATE: u64 = 128 * 1024;

/// How often a pending request or read checks the [`CancelToken`].
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// The deadline of one download request of a `total`-byte file (each hop, connect to the
/// last body byte). It is sized for the whole file, not the bytes still missing: it is
/// fixed before the response status is known, and a server may answer a range request
/// with the whole file (200). xtask's `download::transfer_deadline` follows the same rule.
#[must_use]
pub fn transfer_deadline(total: u64) -> Duration {
    BODY_DEADLINE_BASE + Duration::from_secs(total.div_ceil(MIN_BODY_RATE))
}

/// Least time between two progress reports of one download (T9.24a): at most 4 a second,
/// so a fast connection does not send the window an event per network chunk.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Thins one download's `progress(done, total)` reports to at most one per
/// [`PROGRESS_INTERVAL`]. The first report and a complete one (`done >= total`) are sent at
/// once, a report equal to the last one sent is dropped, and the last report held back is
/// handed out by [`ProgressThrottle::finish`], so whatever the download ends with is shown.
/// Time comes in as an argument, so the rule is tested without a clock.
#[derive(Debug, Default)]
pub struct ProgressThrottle {
    /// When the last report was sent, and what it said.
    sent: Option<(Instant, u64, u64)>,
    /// The latest report not sent.
    held: Option<(u64, u64)>,
}

impl ProgressThrottle {
    /// A throttle that has sent nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The report `(done, total)` made at `now`: `Some` when it is to be sent now, `None`
    /// when it is held back (or repeats the last one sent).
    pub fn offer(&mut self, now: Instant, done: u64, total: u64) -> Option<(u64, u64)> {
        let send = match self.sent {
            None => true,
            Some((_, d, t)) if (d, t) == (done, total) => {
                self.held = None;
                return None;
            }
            Some((at, _, _)) => {
                done >= total || now.saturating_duration_since(at) >= PROGRESS_INTERVAL
            }
        };
        if send {
            self.sent = Some((now, done, total));
            self.held = None;
            Some((done, total))
        } else {
            self.held = Some((done, total));
            None
        }
    }

    /// The last report held back, once: sent when the download ends.
    pub fn finish(&mut self) -> Option<(u64, u64)> {
        let held = self.held.take()?;
        if let Some((_, d, t)) = &mut self.sent {
            (*d, *t) = held;
        }
        Some(held)
    }
}

/// Runs `run` with a progress callback that passes `progress` at most one report per
/// [`PROGRESS_INTERVAL`] ([`ProgressThrottle`], time from `now`), then sends the last report
/// held back before returning `run`'s result, whatever it is (done, failed or cancelled).
pub fn with_throttled_progress<T>(
    progress: &mut dyn FnMut(u64, u64),
    now: &dyn Fn() -> Instant,
    run: impl FnOnce(&mut dyn FnMut(u64, u64)) -> T,
) -> T {
    let mut throttle = ProgressThrottle::new();
    let out = run(&mut |done, total| {
        if let Some((done, total)) = throttle.offer(now(), done, total) {
            progress(done, total);
        }
    });
    if let Some((done, total)) = throttle.finish() {
        progress(done, total);
    }
    out
}

/// What a verified file must look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected<'a> {
    /// Hex SHA-256 (compared case-insensitively).
    pub sha256: &'a str,
    /// Size in bytes.
    pub size: u64,
}

/// Result of [`download`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `dest` already held the expected file; nothing was requested.
    Present,
    /// `dest` was missing but its `.part` already held the whole file (a transfer cut
    /// after its last byte, before the rename); it was verified and renamed, and nothing
    /// was requested.
    CompletedPart,
    /// The file was downloaded; `resumed_from` is the byte offset this transfer started
    /// at (0 when the server answered a range request with the whole file).
    Downloaded {
        /// First byte this transfer received.
        resumed_from: u64,
    },
}

/// Why a download failed. Messages never contain a URL query, fragment or credentials.
#[derive(Debug)]
pub enum DownloadError {
    /// The embedded manifest's hosts could not be read.
    Manifest(String),
    /// A manifest host cannot be allowlisted.
    Host {
        /// The manifest entry.
        host: String,
        /// Why.
        error: UrlError,
    },
    /// The network client or its runtime could not be built.
    Setup(String),
    /// The request (or a redirect hop) was refused or failed before a response arrived;
    /// any `.part` is untouched.
    Request {
        /// The requested URL ([`redact`]ed).
        url: String,
        /// The network error.
        error: NetError,
    },
    /// The server answered with a status other than 200 or 206.
    Status {
        /// The requested URL ([`redact`]ed).
        url: String,
        /// The HTTP status.
        status: u16,
    },
    /// A 206 whose `Content-Range` does not start at the bytes held; the `.part` is kept
    /// unchanged.
    ContentRange {
        /// The requested URL ([`redact`]ed).
        url: String,
        /// Bytes held (the range asked for starts here).
        have: u64,
        /// The `Content-Range` received, if any.
        got: Option<String>,
    },
    /// The transfer ended early (connection closed or lost, deadline); the `.part` keeps
    /// what arrived.
    Interrupted {
        /// The requested URL ([`redact`]ed).
        url: String,
        /// Bytes in the `.part`.
        done: u64,
        /// Expected size.
        total: u64,
        /// What ended it.
        reason: String,
    },
    /// Cancelled through the [`CancelToken`]; the `.part` keeps what arrived.
    Cancelled {
        /// Bytes in the `.part`.
        done: u64,
        /// Expected size.
        total: u64,
    },
    /// The server sent more than the expected size. The `.part` is left longer than the
    /// expected size, so the next run deletes it and starts again from byte 0.
    TooLong {
        /// The requested URL ([`redact`]ed).
        url: String,
        /// Expected size.
        total: u64,
    },
    /// The complete file has the wrong size or SHA-256; the `.part` was deleted.
    Verification {
        /// The requested URL ([`redact`]ed).
        url: String,
        /// What differed.
        detail: String,
    },
    /// A local file operation failed.
    Io {
        /// What was being done, with the path.
        what: String,
        /// The error.
        error: io::Error,
    },
}

impl DownloadError {
    /// Whether the `.part` file was kept so that running the download again resumes it.
    #[must_use]
    pub fn resumable(&self) -> bool {
        matches!(
            self,
            Self::ContentRange { .. } | Self::Interrupted { .. } | Self::Cancelled { .. }
        )
    }

    /// Whether no server answered at all: the connection, its TLS or the wait for a response
    /// failed before any response arrived (T9.23). On a network that only lets traffic out
    /// through a proxy every download ends here, because `earshot-net` never uses one.
    #[must_use]
    pub fn reached_no_server(&self) -> bool {
        matches!(
            self,
            Self::Request {
                error: NetError::Transport(_),
                ..
            }
        )
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(e) => write!(f, "the embedded models/manifest.toml: {e}"),
            Self::Host { host, error } => write!(f, "manifest host `{host}`: {error}"),
            Self::Setup(e) => write!(f, "cannot set up downloads: {e}"),
            Self::Request { url, error } => write!(f, "GET {url}: {error}"),
            Self::Status { url, status } => write!(f, "GET {url}: HTTP {status}"),
            Self::ContentRange { url, have, got } => write!(
                f,
                "GET {url}: asked for bytes from {have}, got 206 with Content-Range {got:?}; .part kept, run again to resume"
            ),
            Self::Interrupted {
                url,
                done,
                total,
                reason,
            } => write!(
                f,
                "GET {url}: {reason} at byte {done} of {total}; run again to resume"
            ),
            Self::Cancelled { done, total } => write!(
                f,
                "download cancelled at byte {done} of {total}; run again to resume"
            ),
            Self::TooLong { url, total } => write!(
                f,
                "GET {url}: server sent more than the expected {total} bytes; the over-long .part is discarded on the next run"
            ),
            Self::Verification { url, detail } => {
                write!(f, "download of {url} failed verification: {detail}")
            }
            Self::Io { what, error } => write!(f, "{what}: {error}"),
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Request { error, .. } => Some(error),
            Self::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

fn io_err(what: impl FnOnce() -> String) -> impl FnOnce(io::Error) -> DownloadError {
    move |error| DownloadError::Io {
        what: what(),
        error,
    }
}

/// The download hosts of a manifest text.
#[derive(Deserialize)]
struct ManifestHosts {
    hosts: Vec<String>,
    #[serde(default)]
    redirect_hosts: Vec<String>,
}

/// The allowlist of a manifest: its `hosts` as exact entries (any hop), its
/// `redirect_hosts` as redirect-only entries ([`Allowlist::allow_redirect_host`]).
fn allowlist_of(manifest: &str) -> Result<Allowlist, DownloadError> {
    let m: ManifestHosts =
        toml::from_str(manifest).map_err(|e| DownloadError::Manifest(e.to_string()))?;
    let mut list = Allowlist::new();
    for host in &m.hosts {
        list.allow_https_host(host)
            .map_err(|error| DownloadError::Host {
                host: host.clone(),
                error,
            })?;
    }
    for host in &m.redirect_hosts {
        list.allow_redirect_host(host)
            .map_err(|error| DownloadError::Host {
                host: host.clone(),
                error,
            })?;
    }
    Ok(list)
}

/// The allowlist of model downloads, from the embedded `models/manifest.toml`: `hosts`
/// for every hop, `redirect_hosts` from the first redirect on, https on port 443.
///
/// # Errors
/// [`DownloadError::Manifest`] or [`DownloadError::Host`] for a broken manifest (a bug).
pub fn manifest_allowlist() -> Result<Allowlist, DownloadError> {
    allowlist_of(MODELS_MANIFEST)
}

/// `<dest>.part`.
#[must_use]
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// Lower-case hex SHA-256 of a file.
pub(crate) fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

/// Checks size and SHA-256 of `path`: `Ok(None)` when they match, else what differs.
fn verify_file(path: &Path, exp: &Expected) -> Result<Option<String>, DownloadError> {
    let len = fs::metadata(path)
        .map_err(io_err(|| format!("stat {}", path.display())))?
        .len();
    if len != exp.size {
        return Ok(Some(format!(
            "size {len} bytes, manifest says {}",
            exp.size
        )));
    }
    let got = sha256_file(path).map_err(io_err(|| format!("read {}", path.display())))?;
    if !got.eq_ignore_ascii_case(exp.sha256) {
        return Ok(Some(format!("sha256 {got}, manifest says {}", exp.sha256)));
    }
    Ok(None)
}

/// First byte offset of a `Content-Range: bytes <first>-<last>/<len>` value.
fn content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    rest.split_once('-')?.0.trim().parse().ok()
}

/// Awaits `fut`, checking `cancel` every [`CANCEL_POLL`]; `None` once cancelled (the
/// future is dropped). The same future is polled throughout, so no progress is lost.
async fn or_cancel<F: Future>(fut: F, cancel: &CancelToken) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        if let Ok(out) = tokio::time::timeout(CANCEL_POLL, &mut fut).await {
            return Some(out);
        }
    }
}

/// Makes sure `dest` holds the expected file, downloading it from `url` through `client`
/// if needed (module docs for the rules). `progress(done, total)` is called as bytes
/// arrive. Needs a tokio runtime with the time driver; [`Downloader`] provides one.
///
/// # Errors
/// [`DownloadError`]; [`DownloadError::resumable`] says whether the `.part` was kept.
pub async fn download(
    client: &NetClient,
    url: &Url,
    dest: &Path,
    exp: &Expected<'_>,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Outcome, DownloadError> {
    if dest.exists() {
        if verify_file(dest, exp)?.is_none() {
            return Ok(Outcome::Present);
        }
        fs::remove_file(dest).map_err(io_err(|| format!("remove {}", dest.display())))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(io_err(|| format!("create {}", parent.display())))?;
    }
    let part = part_path(dest);
    let mut have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > exp.size {
        fs::remove_file(&part).map_err(io_err(|| format!("remove {}", part.display())))?;
        have = 0;
    }
    let requested = have < exp.size;
    let mut resumed_from = have;
    if requested {
        resumed_from = transfer(client, url, &part, have, exp.size, cancel, progress).await?;
    }
    if let Some(detail) = verify_file(&part, exp)? {
        let _ = fs::remove_file(&part);
        return Err(DownloadError::Verification {
            url: redact(url),
            detail,
        });
    }
    fs::rename(&part, dest).map_err(io_err(|| {
        format!("rename {} -> {}", part.display(), dest.display())
    }))?;
    Ok(if requested {
        Outcome::Downloaded { resumed_from }
    } else {
        Outcome::CompletedPart
    })
}

/// One request into `part`, which holds `have` of `total` bytes. Returns the offset the
/// body started at.
async fn transfer(
    client: &NetClient,
    url: &Url,
    part: &Path,
    have: u64,
    total: u64,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<u64, DownloadError> {
    let shown = redact(url);
    let mut req = NetRequest::get(url.clone()).timeout(transfer_deadline(total));
    if have > 0 {
        // Sent on every hop: a caller header would be dropped at the CDN redirect.
        req = req.range_from(have);
    }
    let cancelled = || DownloadError::Cancelled { done: have, total };
    let mut stream = or_cancel(client.open(req), cancel)
        .await
        .ok_or_else(cancelled)?
        .map_err(|error| DownloadError::Request {
            url: shown.clone(),
            error,
        })?;
    let status = stream.status();
    if status == 206 {
        let got = stream
            .headers()
            .get(earshot_net::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if got.as_deref().and_then(content_range_start) != Some(have) {
            // Appending would corrupt the file; the .part is left untouched.
            return Err(DownloadError::ContentRange {
                url: shown,
                have,
                got,
            });
        }
    }
    let start = match status {
        206 => have,
        200 => 0,
        _ => return Err(DownloadError::Status { url: shown, status }),
    };
    let file = if start > 0 {
        OpenOptions::new().append(true).open(part)
    } else {
        File::create(part)
    }
    .map_err(io_err(|| format!("open {}", part.display())))?;
    let mut file = BufWriter::with_capacity(1 << 20, file);
    let mut done = start;
    progress(done, total);
    // Every early end keeps what was received in `.part` for the next run to resume.
    let keep = |file: &mut BufWriter<File>| -> Result<(), DownloadError> {
        file.flush()
            .and_then(|()| file.get_ref().sync_all())
            .map_err(io_err(|| format!("write {}", part.display())))
    };
    loop {
        let piece = match or_cancel(stream.chunk(), cancel).await {
            None => {
                keep(&mut file)?;
                return Err(DownloadError::Cancelled { done, total });
            }
            Some(Ok(Some(piece))) => piece,
            Some(Ok(None)) => break,
            Some(Err(e)) => {
                keep(&mut file)?;
                return Err(DownloadError::Interrupted {
                    url: shown,
                    done,
                    total,
                    reason: format!("connection lost ({e})"),
                });
            }
        };
        file.write_all(&piece)
            .map_err(io_err(|| format!("write {}", part.display())))?;
        done += piece.len() as u64;
        if done > total {
            keep(&mut file)?;
            return Err(DownloadError::TooLong { url: shown, total });
        }
        progress(done, total);
    }
    keep(&mut file)?;
    if done < total {
        return Err(DownloadError::Interrupted {
            url: shown,
            done,
            total,
            reason: "connection closed".into(),
        });
    }
    Ok(start)
}

/// A blocking downloader: an `earshot-net` client over an allowlist and a current-thread
/// tokio runtime to drive it (as the summary service does). Create one and reuse it; do
/// not call it from inside another tokio runtime (use [`download`] there).
pub struct Downloader {
    client: NetClient,
    runtime: tokio::runtime::Runtime,
}

impl std::fmt::Debug for Downloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Downloader")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Downloader {
    /// A downloader for the manifest's hosts ([`manifest_allowlist`]), timestamping its
    /// audit log with `clock`.
    ///
    /// # Errors
    /// A broken manifest, or the client or runtime cannot be built.
    pub fn for_manifest(clock: SharedClock) -> Result<Self, DownloadError> {
        Self::new(manifest_allowlist()?, clock)
    }

    /// A downloader that may contact only `allowlist`.
    ///
    /// # Errors
    /// [`DownloadError::Setup`] if the client or runtime cannot be built.
    pub fn new(allowlist: Allowlist, clock: SharedClock) -> Result<Self, DownloadError> {
        let client = NetClient::new(NetConfig::new(allowlist, clock))
            .map_err(|e| DownloadError::Setup(e.to_string()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DownloadError::Setup(e.to_string()))?;
        Ok(Self { client, runtime })
    }

    /// Every request attempt this downloader made.
    #[must_use]
    pub fn audit_log(&self) -> AuditLog {
        self.client.audit_log()
    }

    /// [`download`] on this downloader's runtime, blocking until it ends.
    ///
    /// # Errors
    /// As [`download`].
    pub fn fetch(
        &self,
        url: &Url,
        dest: &Path,
        exp: &Expected<'_>,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<Outcome, DownloadError> {
        self.runtime
            .block_on(download(&self.client, url, dest, exp, cancel, progress))
    }
}
