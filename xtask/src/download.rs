//! Checksum-verified downloads shared by `fetch-fixtures` and `fetch-models`.
//!
//! A download streams into `<dest>.part`, resumes an existing partial file with an HTTP
//! `Range` request, checks size and SHA-256, and only then renames the file to `dest`.
//! A file that fails verification never appears at `dest`.
//!
//! Redirects are followed here, not by ureq: every hop must pass the manifest's [`Policy`]
//! (https, and a host from `hosts` or, after a redirect, `redirect_hosts`). A transfer that
//! stops early (connection closed or lost, or the body deadline from [`BODY_DEADLINE_BASE`]
//! and [`MIN_BODY_RATE`] passed) keeps the `.part` file and fails with "run again to
//! resume".

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// What a verified file must look like.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    /// Lower-case hex SHA-256.
    pub sha256: &'a str,
    /// Size in bytes.
    pub size: u64,
}

/// Result of [`fetch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `dest` already existed and passed the check that was asked for.
    Present,
    /// The file was downloaded; `resumed_from` is the byte offset the transfer started at.
    Downloaded { resumed_from: u64 },
}

/// Joins a manifest-relative path onto `root`, refusing anything that could leave `root`
/// (absolute paths, drive or UNC prefixes, `..`, empty paths).
pub fn jail_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        bail!("empty relative path");
    }
    if rel.starts_with('/') || rel.starts_with('\\') || rel.contains(':') {
        bail!("path `{rel}` must be relative to the cache directory");
    }
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("path `{rel}` escapes the cache directory")
            }
        }
    }
    if out == root {
        bail!("path `{rel}` names the cache directory itself");
    }
    Ok(out)
}

/// Lower-case hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// Lower-case hex of a byte slice.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Checks size and SHA-256 of an existing file.
pub fn verify_file(path: &Path, exp: &Expected) -> Result<()> {
    let len = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if len != exp.size {
        bail!(
            "{}: size {len} bytes, manifest says {}",
            path.display(),
            exp.size
        );
    }
    let got = sha256_file(path)?;
    if got != exp.sha256 {
        bail!(
            "{}: sha256 {got}, manifest says {}",
            path.display(),
            exp.sha256
        );
    }
    Ok(())
}

/// Fixed part of the deadline for receiving one response body.
///
/// ureq offers only a total body deadline, no idle timeout (one would need the body read on
/// a helper thread; reads stay on the calling thread instead). The deadline is
/// [`BODY_DEADLINE_BASE`] plus the remaining bytes at [`MIN_BODY_RATE`], so a hung transfer
/// ends, with the `.part` kept for a resume, instead of blocking forever.
pub const BODY_DEADLINE_BASE: Duration = Duration::from_secs(120);
/// Slowest transfer rate the body deadline allows for, in bytes per second (128 KiB/s).
pub const MIN_BODY_RATE: u64 = 128 * 1024;
/// Most redirects followed for one request.
pub const MAX_REDIRECTS: usize = 10;

/// Where a download may go: the manifest's `hosts` for the first request, those or
/// `redirect_hosts` for every redirect hop, always over https on the default port.
#[derive(Debug, Clone)]
pub struct Policy {
    hosts: Vec<String>,
    redirect_hosts: Vec<String>,
    require_https: bool,
    deadline_base: Duration,
    min_rate: u64,
}

/// Whether `host` matches a `redirect_hosts` pattern: an exact host, or `*.<domain>` for
/// any host strictly below `<domain>`.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(domain) => host
            .strip_suffix(domain)
            .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
        None => pattern == host,
    }
}

/// Splits an absolute URL into `(scheme, authority, path-and-rest)`.
fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some((scheme, &rest[..end], &rest[end..]))
}

/// Resolves a `Location` header value against the URL that returned it (RFC 3986 5.2 for
/// the forms servers send: absolute, scheme-relative, absolute-path and relative-path).
pub fn resolve_location(base: &str, location: &str) -> Result<String> {
    if split_url(location).is_some() {
        return Ok(location.to_string());
    }
    let (scheme, authority, rest) =
        split_url(base).with_context(|| format!("cannot resolve against `{base}`"))?;
    if let Some(r) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{r}"));
    }
    if location.starts_with('/') {
        return Ok(format!("{scheme}://{authority}{location}"));
    }
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    Ok(format!("{scheme}://{authority}{dir}{location}"))
}

impl Policy {
    /// The policy for a manifest: https only, `hosts` exact, `redirect_hosts` exact or
    /// `*.<domain>`.
    pub fn new(hosts: &[String], redirect_hosts: &[String]) -> Self {
        Policy {
            hosts: hosts.to_vec(),
            redirect_hosts: redirect_hosts.to_vec(),
            require_https: true,
            deadline_base: BODY_DEADLINE_BASE,
            min_rate: MIN_BODY_RATE,
        }
    }

    /// Deadline for receiving a body of `bytes` bytes.
    pub fn body_deadline(&self, bytes: u64) -> Duration {
        self.deadline_base + Duration::from_secs(bytes.div_ceil(self.min_rate.max(1)))
    }

    /// Checks one request URL; `hop` 0 is the manifest URL, later hops are redirects.
    /// Returns the host, ASCII-lowercased (DNS names are case-insensitive; manifest hosts
    /// are lower-case).
    pub fn check(&self, url: &str, hop: usize) -> Result<String> {
        let Some((scheme, authority, _)) = split_url(url) else {
            bail!("`{}` is not an absolute URL", without_query(url));
        };
        let scheme_ok = scheme == "https" || (!self.require_https && scheme == "http");
        if !scheme_ok {
            bail!("refusing {scheme}:// for {authority}; downloads must use https");
        }
        if authority.contains('@') {
            bail!("refusing a URL with user info ({authority})");
        }
        let host = match authority.rsplit_once(':') {
            Some(_) if self.require_https => {
                bail!("refusing {authority}: no explicit port allowed")
            }
            Some((h, _)) => h,
            None => authority,
        }
        .to_ascii_lowercase();
        let listed = self.hosts.contains(&host);
        let redirect_ok = hop > 0 && self.redirect_hosts.iter().any(|p| host_matches(p, &host));
        if !(listed || redirect_ok) {
            if hop == 0 {
                bail!("host `{host}` is not listed in the manifest's `hosts`");
            }
            bail!(
                "redirect {hop} goes to `{host}`, which is in neither `hosts` nor `redirect_hosts` of the manifest"
            );
        }
        Ok(host)
    }
}

/// The URL up to (not including) its query or fragment, for messages: CDN redirect URLs
/// carry signed query strings.
fn without_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

fn agent(policy: &Policy, body_deadline: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .https_only(policy.require_https)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(body_deadline))
        // Redirects are followed by `request` so that every hop is checked.
        .max_redirects(0)
        .user_agent("earshot-xtask")
        .build()
        .into()
}

/// Makes sure `dest` holds the expected file, downloading it from `url` if needed.
///
/// If `dest` exists it is accepted when its size matches (`verify == false`) or when size
/// and SHA-256 match (`verify == true`); otherwise it is replaced. A leftover `<dest>.part`
/// is resumed with a `Range` request; a server that ignores the range restarts the file.
/// Every request and redirect hop must pass `policy`. A transfer that ends early keeps the
/// `.part` file and fails; on a checksum mismatch of a complete file the partial file is
/// deleted and an error returned.
pub fn fetch(
    url: &str,
    dest: &Path,
    exp: &Expected,
    verify: bool,
    policy: &Policy,
) -> Result<Outcome> {
    if dest.exists() {
        let ok = if verify {
            verify_file(dest, exp).is_ok()
        } else {
            fs::metadata(dest)?.len() == exp.size
        };
        if ok {
            return Ok(Outcome::Present);
        }
        eprintln!(
            "fetch: {} does not match the manifest; downloading again",
            dest.display()
        );
        fs::remove_file(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let part = part_path(dest);
    let mut have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > exp.size {
        fs::remove_file(&part)?;
        have = 0;
    }
    let resumed_from = have;
    if have < exp.size {
        transfer(url, &part, have, exp.size, policy)?;
    }
    if let Err(e) = verify_file(&part, exp) {
        let _ = fs::remove_file(&part);
        return Err(e.context(format!("download of {url} failed verification")));
    }
    fs::rename(&part, dest)
        .with_context(|| format!("rename {} -> {}", part.display(), dest.display()))?;
    Ok(Outcome::Downloaded { resumed_from })
}

/// The body deadline of a transfer that already holds `have` of `total` bytes. It is sized
/// for the whole file, not `total - have`: the deadline is fixed before the response status
/// is known, and a server may answer the range request with the whole file (200).
pub fn transfer_deadline(policy: &Policy, _have: u64, total: u64) -> Duration {
    policy.body_deadline(total)
}

/// Sends the GET (with `Range` when `have > 0`) and follows redirects, checking every hop
/// against `policy`. Returns the final (non-redirect) response, whose body must arrive
/// within `policy.body_deadline(total)`: the deadline is fixed before the status is known,
/// and a server may answer a range request with the whole file (200).
fn request(
    url: &str,
    have: u64,
    total: u64,
    policy: &Policy,
) -> Result<ureq::http::Response<ureq::Body>> {
    let agent = agent(policy, transfer_deadline(policy, have, total));
    let mut current = url.to_string();
    for hop in 0..=MAX_REDIRECTS {
        let host = policy.check(&current, hop)?;
        if hop > 0 {
            println!("fetch: redirect {hop} -> {host}");
        }
        let mut req = agent.get(&current);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let resp = req
            .call()
            .with_context(|| format!("GET {url} (hop {hop}, {host})"))?;
        if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(resp);
        }
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .with_context(|| {
                format!(
                    "GET {url}: HTTP {} from {host} without a Location",
                    resp.status()
                )
            })?;
        current = resolve_location(&current, location)?;
    }
    bail!("GET {url}: more than {MAX_REDIRECTS} redirects")
}

/// First byte offset of a `Content-Range: bytes <first>-<last>/<len>` value.
pub fn content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    rest.split_once('-')?.0.trim().parse().ok()
}

fn transfer(url: &str, part: &Path, have: u64, total: u64, policy: &Policy) -> Result<()> {
    let resp = request(url, have, total, policy)?;
    let status = resp.status().as_u16();
    if status == 206 {
        let range = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok());
        let first = range.and_then(content_range_start);
        if first != Some(have) {
            // Appending would corrupt the file; the .part is left untouched for a retry.
            bail!(
                "GET {url}: asked for bytes from {have}, got 206 with Content-Range {range:?}; .part kept, run again to resume"
            );
        }
    }
    let mut file = match (have, status) {
        (0, 200) => File::create(part)?,
        (_, 206) => OpenOptions::new().append(true).open(part)?,
        (_, 200) => {
            eprintln!("fetch: server ignored the range request; restarting {url}");
            File::create(part)?
        }
        (_, s) => bail!("GET {url}: HTTP {s}"),
    };
    let start = if status == 206 { have } else { 0 };
    if start > 0 {
        println!("fetch: resuming {url} at byte {start}");
    } else {
        println!("fetch: downloading {url} ({total} bytes)");
    }
    let mut reader = resp.into_body().into_reader();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = start;
    let mut next_report = done + (256 << 20);
    // Every early end keeps what was received in `.part` for the next run to resume.
    let keep_and_bail = |file: &mut File, done: u64, why: String| -> Result<()> {
        file.flush()?;
        file.sync_all()?;
        bail!("GET {url}: {why} at byte {done} of {total}; run again to resume")
    };
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return keep_and_bail(&mut file, done, format!("connection lost ({e})")),
        };
        file.write_all(&buf[..n])?;
        done += n as u64;
        if done > total {
            bail!("GET {url}: server sent more than the expected {total} bytes");
        }
        if done >= next_report {
            println!("fetch: {done} / {total} bytes");
            next_report += 256 << 20;
        }
    }
    if done < total {
        return keep_and_bail(&mut file, done, "connection closed".into());
    }
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_llm_replay::tcp;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    impl Policy {
        /// A policy for the loopback test servers: plain http and ports allowed.
        fn local(hosts: &[&str], redirect_hosts: &[&str]) -> Self {
            Policy {
                hosts: hosts.iter().map(|h| h.to_string()).collect(),
                redirect_hosts: redirect_hosts.iter().map(|h| h.to_string()).collect(),
                require_https: false,
                deadline_base: BODY_DEADLINE_BASE,
                min_rate: MIN_BODY_RATE,
            }
        }
    }

    fn loopback() -> Policy {
        Policy::local(&["127.0.0.1"], &[])
    }

    /// Minimal HTTP/1.1 server for one file. Handles one connection at a time on one
    /// thread, like the original server, and records the `Range` header of each request.
    struct Server {
        url: String,
        ranges: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[derive(Clone, Copy, Default)]
    struct Behaviour {
        ignore_range: bool,
        /// Close the connection after this many body bytes on the first request.
        cut_first_after: Option<usize>,
        /// Send no `Content-Length` (the body ends when the connection closes).
        close_delimited: bool,
        /// After this many body bytes on the first request, keep the connection open and
        /// silent until the client hangs up.
        stall_first_after: Option<usize>,
        /// Added to the start offset reported in `Content-Range` (the body is still the
        /// requested range): a server that lies about where its 206 starts.
        content_range_skew: usize,
        /// Send the body in 10 000-byte pieces with this pause after each.
        drip: Option<Duration>,
    }

    /// How long the stalling server holds a connection at most.
    const STALL_HOLD: Duration = Duration::from_secs(30);

    /// Reads the request head; returns the `Range` value. `None` if the client went away.
    fn read_request(stream: &TcpStream) -> Option<Option<String>> {
        let mut reader = BufReader::new(stream);
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                return Some(range);
            }
            if let Some((k, v)) = line.split_once(':')
                && k.eq_ignore_ascii_case("range")
            {
                range = Some(v.trim().to_string());
            }
        }
    }

    fn serve(body: Vec<u8>, b: Behaviour) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&ranges);
        thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { continue };
                let Some(range) = read_request(&stream) else {
                    tcp::lingering_close(stream, &bad_request());
                    continue;
                };
                seen.lock().unwrap().push(range.clone());
                let start = match (&range, b.ignore_range) {
                    (Some(r), false) => r
                        .trim_start_matches("bytes=")
                        .trim_end_matches('-')
                        .parse::<usize>()
                        .unwrap_or(0),
                    _ => 0,
                };
                let slice = &body[start.min(body.len())..];
                let length = if b.close_delimited {
                    String::new()
                } else {
                    format!("Content-Length: {}\r\n", slice.len())
                };
                let head = if start > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\n{length}Content-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        start + b.content_range_skew,
                        body.len() - 1,
                        body.len()
                    )
                } else {
                    format!("HTTP/1.1 200 OK\r\n{length}Connection: close\r\n\r\n")
                };
                let first = i == 0;
                let n = match (first, b.cut_first_after, b.stall_first_after) {
                    (true, Some(n), _) | (true, _, Some(n)) => n.min(slice.len()),
                    _ => slice.len(),
                };
                let rest = match b.drip {
                    None => [head.as_bytes(), &slice[..n]].concat(),
                    Some(pause) => {
                        let _ = stream.write_all(head.as_bytes());
                        for piece in slice[..n].chunks(10_000) {
                            let _ = stream.write_all(piece);
                            let _ = stream.flush();
                            thread::sleep(pause);
                        }
                        Vec::new()
                    }
                };
                if first && b.stall_first_after.is_some() {
                    let _ = stream.write_all(&rest);
                    let _ = stream.flush();
                    // Hold the connection open and silent until the client gives up and
                    // closes it (read returns 0 or an error), or STALL_HOLD passes so a
                    // client without a deadline fails instead of hanging the test.
                    if let Ok(clone) = stream.try_clone() {
                        thread::spawn(move || {
                            thread::sleep(STALL_HOLD);
                            let _ = clone.shutdown(std::net::Shutdown::Both);
                        });
                    }
                    let mut sink = [0u8; 64];
                    while matches!(stream.read(&mut sink), Ok(k) if k > 0) {}
                } else {
                    // A cut (`cut_first_after`) is a clean early close too: the client
                    // sees every byte sent, then EOF.
                    tcp::lingering_close(stream, &rest);
                }
            }
        });
        Server { url, ranges }
    }

    /// A server that answers every request with `status` and `Location: <location>`.
    /// Returns its URL and a request counter.
    fn serve_redirect(status: u16, location: String) -> (String, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/start.bin", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(0usize));
        let count = Arc::clone(&hits);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if read_request(&stream).is_none() {
                    tcp::lingering_close(stream, &bad_request());
                    continue;
                }
                *count.lock().unwrap() += 1;
                let head = format!(
                    "HTTP/1.1 {status} Moved\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                tcp::lingering_close(stream, head.as_bytes());
            }
        });
        (url, hits)
    }

    /// The reply to a request the test server could not read.
    fn bad_request() -> Vec<u8> {
        tcp::error_response(400, "Bad Request", "unreadable request")
    }

    fn body() -> Vec<u8> {
        (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("earshot-xtask-dl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    const PLAIN: Behaviour = Behaviour {
        ignore_range: false,
        cut_first_after: None,
        close_delimited: false,
        stall_first_after: None,
        content_range_skew: 0,
        drip: None,
    };

    #[test]
    fn downloads_verifies_and_renames() {
        let b = body();
        let s = serve(b.clone(), PLAIN);
        let dir = tmp("fresh");
        let dest = dir.join("sub").join("file.bin");
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        let out = fetch(&s.url, &dest, &exp, true, &loopback()).unwrap();
        assert_eq!(out, Outcome::Downloaded { resumed_from: 0 });
        assert_eq!(fs::read(&dest).unwrap(), b);
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn interrupted_download_resumes_with_range() {
        let b = body();
        let s = serve(
            b.clone(),
            Behaviour {
                cut_first_after: Some(70_000),
                ..PLAIN
            },
        );
        let dir = tmp("resume");
        let dest = dir.join("file.bin");
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        let err = format!(
            "{:#}",
            fetch(&s.url, &dest, &exp, true, &loopback()).unwrap_err()
        );
        assert!(err.contains("at byte 70000 of 200000; run again"), "{err}");
        assert!(!dest.exists(), "a cut download must not appear at dest");
        assert_eq!(fs::metadata(part_path(&dest)).unwrap().len(), 70_000);
        let out = fetch(&s.url, &dest, &exp, true, &loopback()).unwrap();
        assert_eq!(
            out,
            Outcome::Downloaded {
                resumed_from: 70_000
            }
        );
        assert_eq!(fs::read(&dest).unwrap(), b);
        let ranges = s.ranges.lock().unwrap().clone();
        assert_eq!(ranges, [None, Some("bytes=70000-".to_string())]);
    }

    #[test]
    fn cleanly_closed_truncated_body_keeps_the_part_and_resumes() {
        // No Content-Length: the server closes cleanly after 70 000 bytes, so the client
        // sees a normal end of body that is short of the manifest size.
        let b = body();
        let s = serve(
            b.clone(),
            Behaviour {
                cut_first_after: Some(70_000),
                close_delimited: true,
                ..PLAIN
            },
        );
        let dir = tmp("cleancut");
        let dest = dir.join("file.bin");
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        let err = format!(
            "{:#}",
            fetch(&s.url, &dest, &exp, true, &loopback()).unwrap_err()
        );
        assert!(
            err.contains("connection closed at byte 70000 of 200000; run again to resume"),
            "{err}"
        );
        assert_eq!(fs::metadata(part_path(&dest)).unwrap().len(), 70_000);
        let out = fetch(&s.url, &dest, &exp, true, &loopback()).unwrap();
        assert_eq!(
            out,
            Outcome::Downloaded {
                resumed_from: 70_000
            }
        );
        assert_eq!(fs::read(&dest).unwrap(), b);
    }

    #[test]
    fn body_deadline_scales_with_the_remaining_bytes() {
        let p = Policy::new(&[], &[]);
        assert_eq!(p.body_deadline(0), BODY_DEADLINE_BASE);
        assert_eq!(
            p.body_deadline(MIN_BODY_RATE * 100 + 1),
            BODY_DEADLINE_BASE + Duration::from_secs(101)
        );
        // 1.6 GB (the largest model) gets a few hours; nothing is unbounded.
        assert!(p.body_deadline(1_624_555_275) < Duration::from_secs(4 * 3600));
    }

    #[test]
    fn stalled_body_times_out_and_keeps_the_part() {
        let b = body();
        let s = serve(
            b.clone(),
            Behaviour {
                stall_first_after: Some(70_000),
                ..PLAIN
            },
        );
        let dir = tmp("stall");
        let dest = dir.join("file.bin");
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        // The server sends 70 000 bytes and then holds the connection open and silent.
        let mut policy = loopback();
        policy.deadline_base = Duration::from_secs(2);
        policy.min_rate = u64::MAX;
        let t0 = Instant::now();
        let err = format!(
            "{:#}",
            fetch(&s.url, &dest, &exp, true, &policy).unwrap_err()
        );
        let waited = t0.elapsed();
        assert!(
            waited < STALL_HOLD,
            "the deadline never fired; the server gave up first: {waited:?}"
        );
        assert!(
            err.contains("at byte 70000 of 200000; run again to resume"),
            "{err}"
        );
        assert!(err.to_lowercase().contains("timeout"), "{err}");
        assert_eq!(fs::metadata(part_path(&dest)).unwrap().len(), 70_000);
        let out = fetch(&s.url, &dest, &exp, true, &loopback()).unwrap();
        assert_eq!(
            out,
            Outcome::Downloaded {
                resumed_from: 70_000
            }
        );
        assert_eq!(fs::read(&dest).unwrap(), b);
    }

    #[test]
    fn partial_response_starting_elsewhere_is_refused_and_the_part_kept() {
        let b = body();
        let s = serve(
            b.clone(),
            Behaviour {
                cut_first_after: Some(70_000),
                content_range_skew: 1,
                ..PLAIN
            },
        );
        let dir = tmp("badrange");
        let dest = dir.join("file.bin");
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        assert!(fetch(&s.url, &dest, &exp, true, &loopback()).is_err());
        let err = format!(
            "{:#}",
            fetch(&s.url, &dest, &exp, true, &loopback()).unwrap_err()
        );
        assert!(
            err.contains("asked for bytes from 70000") && err.contains("70001-"),
            "{err}"
        );
        assert_eq!(fs::read(part_path(&dest)).unwrap(), b[..70_000]);
        assert!(!dest.exists());
    }

    #[test]
    fn content_range_start_parses_the_first_byte() {
        assert_eq!(
            content_range_start("bytes 70000-199999/200000"),
            Some(70_000)
        );
        assert_eq!(content_range_start(" bytes  5-9/*"), Some(5));
        assert_eq!(content_range_start("bytes */200000"), None);
        assert_eq!(content_range_start("items 1-2/3"), None);
        assert_eq!(content_range_start(""), None);
    }

    #[test]
    fn transfer_deadline_is_sized_for_the_whole_file() {
        let mut policy = loopback();
        policy.deadline_base = Duration::ZERO;
        policy.min_rate = 1;
        let total = 200_000u64;
        for have in [0, 1, total - 1, total] {
            assert_eq!(
                transfer_deadline(&policy, have, total),
                policy.body_deadline(total),
                "have {have}"
            );
            if have > 0 {
                assert_ne!(
                    transfer_deadline(&policy, have, total),
                    policy.body_deadline(total - have),
                    "have {have}: sized for the missing bytes only"
                );
            }
        }
    }

    #[test]
    fn range_request_answered_with_the_whole_file_completes() {
        // Smoke test with a wide margin: the server ignores the range and sends all
        // 200 000 bytes in about 0.2 s; the deadline is 4 s.
        let b = body();
        let s = serve(
            b.clone(),
            Behaviour {
                ignore_range: true,
                drip: Some(Duration::from_millis(10)),
                ..PLAIN
            },
        );
        let dir = tmp("wholedeadline");
        let dest = dir.join("file.bin");
        fs::write(part_path(&dest), &b[..150_000]).unwrap();
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        let mut policy = loopback();
        policy.deadline_base = Duration::ZERO;
        policy.min_rate = 50_000;
        fetch(&s.url, &dest, &exp, true, &policy).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b);
        assert_eq!(
            *s.ranges.lock().unwrap(),
            [Some("bytes=150000-".to_string())]
        );
    }

    #[test]
    fn server_ignoring_range_restarts_the_file() {
        let b = body();
        let s = serve(
            b.clone(),
            Behaviour {
                ignore_range: true,
                cut_first_after: Some(50_000),
                ..PLAIN
            },
        );
        let dir = tmp("norange");
        let dest = dir.join("file.bin");
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        assert!(fetch(&s.url, &dest, &exp, true, &loopback()).is_err());
        fetch(&s.url, &dest, &exp, true, &loopback()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b);
    }

    #[test]
    fn checksum_mismatch_leaves_nothing_behind() {
        let b = body();
        let s = serve(b.clone(), PLAIN);
        let dir = tmp("badsum");
        let dest = dir.join("file.bin");
        let wrong = sha(b"something else");
        let exp = Expected {
            sha256: &wrong,
            size: b.len() as u64,
        };
        let err = fetch(&s.url, &dest, &exp, true, &loopback()).unwrap_err();
        assert!(format!("{err:#}").contains("sha256"), "{err:#}");
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn verified_existing_file_needs_no_request() {
        let b = body();
        let s = serve(b.clone(), PLAIN);
        let dir = tmp("present");
        let dest = dir.join("file.bin");
        fs::write(&dest, &b).unwrap();
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        assert_eq!(
            fetch(&s.url, &dest, &exp, true, &loopback()).unwrap(),
            Outcome::Present
        );
        assert!(s.ranges.lock().unwrap().is_empty());
    }

    #[test]
    fn corrupt_existing_file_is_replaced_under_verify() {
        let b = body();
        let s = serve(b.clone(), PLAIN);
        let dir = tmp("corrupt");
        let dest = dir.join("file.bin");
        let mut bad = b.clone();
        bad[10] ^= 0xff;
        fs::write(&dest, &bad).unwrap();
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        assert_eq!(
            fetch(&s.url, &dest, &exp, true, &loopback()).unwrap(),
            Outcome::Downloaded { resumed_from: 0 }
        );
        assert_eq!(fs::read(&dest).unwrap(), b);
    }

    #[test]
    fn allowed_redirect_is_followed_with_the_range() {
        let b = body();
        let file = serve(
            b.clone(),
            Behaviour {
                cut_first_after: Some(70_000),
                ..PLAIN
            },
        );
        let (start, hits) = serve_redirect(302, file.url.clone());
        let dir = tmp("redirect");
        let dest = dir.join("file.bin");
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        assert!(fetch(&start, &dest, &exp, true, &loopback()).is_err());
        fetch(&start, &dest, &exp, true, &loopback()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b);
        assert_eq!(*hits.lock().unwrap(), 2);
        let ranges = file.ranges.lock().unwrap().clone();
        assert_eq!(ranges, [None, Some("bytes=70000-".to_string())]);
    }

    #[test]
    fn redirect_to_an_unlisted_host_is_refused_before_any_request() {
        let b = body();
        let file = serve(b.clone(), PLAIN);
        // Same server, but named `localhost`, which the policy does not list.
        let elsewhere = file.url.replace("127.0.0.1", "localhost");
        let (start, hits) = serve_redirect(307, elsewhere);
        let dir = tmp("badredirect");
        let dest = dir.join("file.bin");
        let sum = sha(&b);
        let exp = Expected {
            sha256: &sum,
            size: b.len() as u64,
        };
        let err = format!(
            "{:#}",
            fetch(&start, &dest, &exp, true, &loopback()).unwrap_err()
        );
        assert!(err.contains("redirect 1 goes to `localhost`"), "{err}");
        assert_eq!(*hits.lock().unwrap(), 1);
        assert!(
            file.ranges.lock().unwrap().is_empty(),
            "unlisted host was contacted"
        );
        assert!(!part_path(&dest).exists() && !dest.exists());

        // Listed as a redirect host, the same hop is followed.
        let policy = Policy::local(&["127.0.0.1"], &["localhost"]);
        fetch(&start, &dest, &exp, true, &policy).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b);
    }

    #[test]
    fn redirect_loops_are_bounded() {
        let (start, _) = serve_redirect(301, "/again".into());
        let dir = tmp("loop");
        let exp = Expected {
            sha256: &"0".repeat(64),
            size: 1,
        };
        let err = format!(
            "{:#}",
            fetch(&start, &dir.join("f"), &exp, true, &loopback()).unwrap_err()
        );
        assert!(err.contains("more than 10 redirects"), "{err}");
    }

    #[test]
    fn manifest_policy_checks_every_hop() {
        let p = Policy::new(
            &["huggingface.co".into(), "github.com".into()],
            &[
                "*.hf.co".into(),
                "release-assets.githubusercontent.com".into(),
            ],
        );
        p.check("https://huggingface.co/x/resolve/abc/m.bin", 0)
            .unwrap();
        p.check("https://us.aws.cdn.hf.co/xet-bridge-us/1/2?sig=3", 1)
            .unwrap();
        p.check("https://release-assets.githubusercontent.com/a/b", 1)
            .unwrap();
        p.check("https://github.com/a/b", 2).unwrap();
        // Host names are matched case-insensitively.
        assert_eq!(
            p.check("https://US.AWS.Cdn.HF.co/xet/1", 1).unwrap(),
            "us.aws.cdn.hf.co"
        );
        assert_eq!(
            p.check("https://HuggingFace.co/x", 0).unwrap(),
            "huggingface.co"
        );
        for (url, hop) in [
            // redirect hosts are not valid first hops
            ("https://us.aws.cdn.hf.co/x", 0),
            // https only, on the default port, without user info
            ("http://huggingface.co/x", 0),
            ("http://us.aws.cdn.hf.co/x", 1),
            ("https://huggingface.co:8443/x", 0),
            ("https://user@huggingface.co/x", 0),
            // unlisted hosts and wildcard look-alikes
            ("https://evil.example/x", 1),
            ("https://hf.co/x", 1),
            ("https://evilhf.co/x", 1),
            ("https://hf.co.evil.example/x", 1),
            ("https://objects.githubusercontent.com/x", 1),
            ("not a url", 0),
        ] {
            assert!(
                p.check(url, hop).is_err(),
                "{url} at hop {hop} must be refused"
            );
        }
    }

    #[test]
    fn wildcard_redirect_hosts_match_strict_subdomains_only() {
        assert!(host_matches("*.hf.co", "cdn.hf.co"));
        assert!(host_matches("*.hf.co", "us.aws.cdn.hf.co"));
        assert!(!host_matches("*.hf.co", "hf.co"));
        assert!(!host_matches("*.hf.co", "xhf.co"));
        assert!(!host_matches("*.hf.co", ".hf.co"));
        assert!(host_matches("a.b", "a.b"));
        assert!(!host_matches("a.b", "x.a.b"));
    }

    #[test]
    fn download_host_matches_follows_the_shared_case_table() {
        // The same table is run by earshot-net's
        // `allowlist::tests::host_matches_follows_the_shared_case_table`.
        let path = crate::repo_root().join("fixtures/net/host_match_cases.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table.len(), 1, "only [[cases]] in {}", path.display());
        let cases = table["cases"].as_array().expect("[[cases]]");
        assert!(cases.len() >= 10, "{} cases read", cases.len());
        let mut seen = [false, false];
        for case in cases {
            let case = case.as_table().expect("a table");
            assert_eq!(case.len(), 3, "exactly pattern, host, matches: {case:?}");
            let pattern = case["pattern"].as_str().expect("pattern string");
            let host = case["host"].as_str().expect("host string");
            let matches = case["matches"].as_bool().expect("matches bool");
            seen[usize::from(matches)] = true;
            assert_eq!(
                host_matches(pattern, host),
                matches,
                "host_matches({pattern:?}, {host:?})"
            );
        }
        assert_eq!(seen, [true, true], "cases of both outcomes");
    }

    #[test]
    fn locations_resolve_against_the_current_url() {
        let base = "https://h.example/a/b/c.bin?x=1";
        for (loc, want) in [
            ("https://cdn.example/z", "https://cdn.example/z"),
            ("//cdn.example/z", "https://cdn.example/z"),
            ("/z/y", "https://h.example/z/y"),
            ("d.bin", "https://h.example/a/b/d.bin"),
            ("d.bin?q=http://x", "https://h.example/a/b/d.bin?q=http://x"),
        ] {
            assert_eq!(resolve_location(base, loc).unwrap(), want, "{loc}");
        }
        assert_eq!(
            resolve_location("https://h.example", "z").unwrap(),
            "https://h.example/z"
        );
    }

    #[test]
    fn jail_join_accepts_nested_relative_paths() {
        let root = Path::new("cache");
        assert_eq!(
            jail_join(root, "a/b/c.flac").unwrap(),
            root.join("a").join("b").join("c.flac")
        );
        assert_eq!(jail_join(root, "./a").unwrap(), root.join("a"));
    }

    #[test]
    fn jail_join_rejects_escapes() {
        let root = Path::new("cache");
        for bad in [
            "",
            ".",
            "../x",
            "a/../../x",
            "a/..",
            "/etc/passwd",
            "\\x",
            "C:\\x",
            "C:x",
            "\\\\server\\share\\x",
        ] {
            assert!(jail_join(root, bad).is_err(), "{bad:?} must be rejected");
        }
    }
}
