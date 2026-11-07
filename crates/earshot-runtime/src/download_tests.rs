//! Tests of in-app model downloads against loopback HTTP servers (plain http to
//! 127.0.0.1 is allowed by `earshot-net`). Every connection ends with a lingering close.

use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use earshot_core::identity::ManualClock;
use earshot_llm_replay::tcp;
use earshot_net::{AuditOutcome, DenyReason};

use super::*;
use crate::testutil::{T0, TempDir};

/// Minimal HTTP/1.1 server for one file, one connection at a time; records the `Range`
/// header of each request.
struct Server {
    port: u16,
    ranges: Arc<Mutex<Vec<Option<String>>>>,
}

impl Server {
    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}{path}", self.port)).unwrap()
    }

    fn ranges(&self) -> Vec<Option<String>> {
        self.ranges.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy, Default)]
struct Behaviour {
    /// Answer range requests with the whole file (200).
    ignore_range: bool,
    /// Close the connection after this many body bytes on the first request (the
    /// `Content-Length` still announces the whole body).
    cut_first_after: Option<usize>,
    /// Added to the start offset reported in `Content-Range` (the body is still the
    /// requested range): a server that lies about where its 206 starts.
    content_range_skew: usize,
}

/// Reads the request head; returns the `Range` value. `None` if unreadable.
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

fn bad_request() -> Vec<u8> {
    tcp::error_response(400, "Bad Request", "unreadable request")
}

fn serve(body: Vec<u8>, b: Behaviour) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&ranges);
    thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { continue };
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
            let head = if start > 0 {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    start + b.content_range_skew,
                    body.len() - 1,
                    body.len()
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                )
            };
            let n = match (i, b.cut_first_after) {
                (0, Some(n)) => n.min(slice.len()),
                _ => slice.len(),
            };
            tcp::lingering_close(stream, &[head.as_bytes(), &slice[..n]].concat());
        }
    });
    Server { port, ranges }
}

/// A server that answers every request with a raw `head` (no body). Returns its port and
/// a request counter.
fn serve_raw(head: String) -> (u16, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
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
            tcp::lingering_close(stream, head.as_bytes());
        }
    });
    (port, hits)
}

fn serve_redirect(location: &Url) -> (u16, Arc<Mutex<usize>>) {
    serve_raw(format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    ))
}

fn body() -> Vec<u8> {
    (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect()
}

fn sha(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn local(ports: &[u16]) -> Allowlist {
    let mut list = Allowlist::new();
    for p in ports {
        list.allow_url(&Url::parse(&format!("http://127.0.0.1:{p}/")).unwrap())
            .unwrap();
    }
    list
}

fn downloader(list: Allowlist) -> Downloader {
    Downloader::new(list, Arc::new(ManualClock::new(T0))).unwrap()
}

/// Runs a fetch with a fresh token and returns the outcome and the last progress call.
fn fetch(
    d: &Downloader,
    url: &Url,
    dest: &Path,
    exp: &Expected,
) -> (Result<Outcome, DownloadError>, Option<(u64, u64)>) {
    let mut last = None;
    let out = d.fetch(url, dest, exp, &CancelToken::new(), &mut |done, total| {
        last = Some((done, total));
    });
    (out, last)
}

#[test]
fn download_streams_verifies_and_renames() {
    let b = body();
    let s = serve(b.clone(), Behaviour::default());
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-fresh");
    let dest = dir.0.join("sub").join("file.bin");
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let (out, last) = fetch(&d, &s.url("/file.bin"), &dest, &exp);
    assert_eq!(out.unwrap(), Outcome::Downloaded { resumed_from: 0 });
    assert_eq!(last, Some((200_000, 200_000)));
    assert_eq!(fs::read(&dest).unwrap(), b);
    assert!(!part_path(&dest).exists());
    assert_eq!(s.ranges(), [None]);
    let log = d.audit_log().entries();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].outcome, AuditOutcome::Completed);
    assert_eq!(log[0].response_bytes, 200_000);
}

#[test]
fn download_cut_transfer_keeps_the_part_and_resumes_with_a_range() {
    let b = body();
    let s = serve(
        b.clone(),
        Behaviour {
            cut_first_after: Some(70_000),
            ..Behaviour::default()
        },
    );
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-resume");
    let dest = dir.0.join("file.bin");
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let url = s.url("/file.bin");
    let err = fetch(&d, &url, &dest, &exp).0.unwrap_err();
    assert!(err.resumable(), "{err}");
    let msg = err.to_string();
    assert!(
        msg.contains("at byte 70000 of 200000; run again to resume"),
        "{msg}"
    );
    assert!(!dest.exists(), "a cut download must not appear at dest");
    assert_eq!(fs::read(part_path(&dest)).unwrap(), b[..70_000]);

    let (out, last) = fetch(&d, &url, &dest, &exp);
    assert_eq!(
        out.unwrap(),
        Outcome::Downloaded {
            resumed_from: 70_000
        }
    );
    assert_eq!(last, Some((200_000, 200_000)));
    assert_eq!(fs::read(&dest).unwrap(), b);
    assert!(!part_path(&dest).exists());
    assert_eq!(s.ranges(), [None, Some("bytes=70000-".to_string())]);
}

#[test]
fn download_206_starting_elsewhere_is_refused_and_the_part_kept() {
    let b = body();
    let s = serve(
        b.clone(),
        Behaviour {
            cut_first_after: Some(70_000),
            content_range_skew: 1,
            ..Behaviour::default()
        },
    );
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-badrange");
    let dest = dir.0.join("file.bin");
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let url = s.url("/file.bin");
    assert!(fetch(&d, &url, &dest, &exp).0.is_err());
    let err = fetch(&d, &url, &dest, &exp).0.unwrap_err();
    assert!(
        matches!(err, DownloadError::ContentRange { have: 70_000, .. }),
        "{err:?}"
    );
    assert!(err.resumable());
    let msg = err.to_string();
    assert!(
        msg.contains("70001-") && msg.contains(".part kept"),
        "{msg}"
    );
    assert_eq!(fs::read(part_path(&dest)).unwrap(), b[..70_000]);
    assert!(!dest.exists());
}

#[test]
fn download_200_answer_to_a_range_request_restarts_the_file() {
    let b = body();
    let s = serve(
        b.clone(),
        Behaviour {
            ignore_range: true,
            ..Behaviour::default()
        },
    );
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-norange");
    let dest = dir.0.join("file.bin");
    // A wrong prefix: appending to it would fail verification, a restart does not.
    fs::write(part_path(&dest), vec![0xAAu8; 150_000]).unwrap();
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let (out, _) = fetch(&d, &s.url("/file.bin"), &dest, &exp);
    assert_eq!(out.unwrap(), Outcome::Downloaded { resumed_from: 0 });
    assert_eq!(fs::read(&dest).unwrap(), b);
    assert_eq!(s.ranges(), [Some("bytes=150000-".to_string())]);
}

#[test]
fn download_part_larger_than_the_file_is_deleted_and_restarted() {
    let b = body();
    let s = serve(b.clone(), Behaviour::default());
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-bigpart");
    let dest = dir.0.join("file.bin");
    fs::write(part_path(&dest), vec![0u8; 200_001]).unwrap();
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let (out, _) = fetch(&d, &s.url("/file.bin"), &dest, &exp);
    assert_eq!(out.unwrap(), Outcome::Downloaded { resumed_from: 0 });
    assert_eq!(fs::read(&dest).unwrap(), b);
    assert_eq!(s.ranges(), [None]);
}

#[test]
fn download_with_a_wrong_sha256_deletes_the_part() {
    let b = body();
    let s = serve(b.clone(), Behaviour::default());
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-badsum");
    let dest = dir.0.join("file.bin");
    let wrong = sha(b"something else");
    let exp = Expected {
        sha256: &wrong,
        size: b.len() as u64,
    };
    let err = fetch(&d, &s.url("/file.bin"), &dest, &exp).0.unwrap_err();
    assert!(matches!(err, DownloadError::Verification { .. }), "{err:?}");
    assert!(!err.resumable());
    assert!(err.to_string().contains("sha256"), "{err}");
    assert!(!dest.exists());
    assert!(!part_path(&dest).exists());
}

#[test]
fn download_more_bytes_than_expected_fails() {
    let b = body();
    let s = serve(b.clone(), Behaviour::default());
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-toolong");
    let dest = dir.0.join("file.bin");
    let sum = sha(&b[..100_000]);
    let exp = Expected {
        sha256: &sum,
        size: 100_000,
    };
    let err = fetch(&d, &s.url("/file.bin"), &dest, &exp).0.unwrap_err();
    assert!(
        matches!(err, DownloadError::TooLong { total: 100_000, .. }),
        "{err:?}"
    );
    assert!(!err.resumable());
    assert!(
        err.to_string().contains("discarded on the next run"),
        "{err}"
    );
    assert!(!dest.exists());
    // The .part holds more than the expected size (what arrived up to the overflow) ...
    let held = fs::read(part_path(&dest)).unwrap();
    assert!(
        held.len() > 100_000 && held.len() <= b.len(),
        "{}",
        held.len()
    );
    assert_eq!(held, b[..held.len()]);
    // ... and the next run discards it and starts again without a range.
    assert!(matches!(
        fetch(&d, &s.url("/file.bin"), &dest, &exp).0.unwrap_err(),
        DownloadError::TooLong { .. }
    ));
    assert_eq!(s.ranges(), [None, None]);
}

#[test]
fn download_follows_a_redirect_to_a_redirect_only_host() {
    let b = body();
    let file = serve(
        b.clone(),
        Behaviour {
            cut_first_after: Some(70_000),
            ..Behaviour::default()
        },
    );
    let (origin, hits) = serve_redirect(&file.url("/cdn/file.bin?sig=1"));
    let mut list = local(&[origin]);
    list.allow_redirect_host(&format!("127.0.0.1:{}", file.port))
        .unwrap();
    let d = downloader(list);
    let dir = TempDir::new("dl-redirect");
    let dest = dir.0.join("file.bin");
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let url = Url::parse(&format!("http://127.0.0.1:{origin}/file.bin")).unwrap();
    assert!(fetch(&d, &url, &dest, &exp).0.unwrap_err().resumable());
    let (out, _) = fetch(&d, &url, &dest, &exp);
    assert_eq!(
        out.unwrap(),
        Outcome::Downloaded {
            resumed_from: 70_000
        }
    );
    assert_eq!(fs::read(&dest).unwrap(), b);
    assert_eq!(*hits.lock().unwrap(), 2);
    // The range survives the redirect.
    assert_eq!(file.ranges(), [None, Some("bytes=70000-".to_string())]);
    let log = d.audit_log().entries();
    assert_eq!(log.len(), 4, "{log:?}");
    let ports: Vec<_> = log.iter().map(|e| e.port).collect();
    assert_eq!(ports, [origin, file.port, origin, file.port]);
    assert_eq!(log[3].outcome, AuditOutcome::Completed);
    assert_eq!(log[3].path, "/cdn/file.bin");
}

#[test]
fn download_redirect_to_an_unlisted_host_is_refused() {
    let b = body();
    let file = serve(b.clone(), Behaviour::default());
    let (origin, hits) = serve_redirect(&file.url("/file.bin?sig=secret"));
    let d = downloader(local(&[origin]));
    let dir = TempDir::new("dl-badredirect");
    let dest = dir.0.join("file.bin");
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let url = Url::parse(&format!("http://127.0.0.1:{origin}/file.bin?token=secret")).unwrap();
    let err = fetch(&d, &url, &dest, &exp).0.unwrap_err();
    assert!(
        matches!(
            err,
            DownloadError::Request {
                error: NetError::NotAllowed {
                    reason: DenyReason::HostNotAllowed,
                    ..
                },
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.resumable());
    let msg = format!("{err} {err:?}");
    assert!(!msg.contains("secret"), "{msg}");
    assert_eq!(*hits.lock().unwrap(), 1);
    assert!(file.ranges().is_empty(), "the unlisted host was contacted");
    assert!(!part_path(&dest).exists() && !dest.exists());
    let log = d.audit_log().entries();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(
        log[1].outcome,
        AuditOutcome::Blocked(DenyReason::HostNotAllowed)
    );
}

#[test]
fn download_from_a_redirect_only_host_as_the_first_request_is_refused() {
    let b = body();
    let file = serve(b.clone(), Behaviour::default());
    let mut list = Allowlist::new();
    list.allow_redirect_host(&format!("127.0.0.1:{}", file.port))
        .unwrap();
    let d = downloader(list);
    let dir = TempDir::new("dl-redirectfirst");
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let err = fetch(&d, &file.url("/file.bin"), &dir.0.join("f"), &exp)
        .0
        .unwrap_err();
    assert!(
        matches!(
            err,
            DownloadError::Request {
                error: NetError::NotAllowed {
                    reason: DenyReason::HostNotAllowed,
                    ..
                },
                ..
            }
        ),
        "{err:?}"
    );
    assert!(file.ranges().is_empty());
    let log = d.audit_log().entries();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(
        log[0].outcome,
        AuditOutcome::Blocked(DenyReason::HostNotAllowed)
    );
}

#[test]
fn download_other_status_fails_without_touching_the_part() {
    let (port, hits) = serve_raw(
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    );
    let d = downloader(local(&[port]));
    let dir = TempDir::new("dl-404");
    let dest = dir.0.join("file.bin");
    fs::write(part_path(&dest), b"held").unwrap();
    let exp = Expected {
        sha256: &"0".repeat(64),
        size: 10,
    };
    let url = Url::parse(&format!("http://127.0.0.1:{port}/f?sig=secret")).unwrap();
    let err = fetch(&d, &url, &dest, &exp).0.unwrap_err();
    assert!(
        matches!(err, DownloadError::Status { status: 404, .. }),
        "{err:?}"
    );
    assert!(!err.to_string().contains("secret"), "{err}");
    assert_eq!(*hits.lock().unwrap(), 1);
    assert_eq!(fs::read(part_path(&dest)).unwrap(), b"held");
}

#[test]
fn download_cancelled_keeps_the_part_for_a_resume() {
    let b = body();
    let s = serve(b.clone(), Behaviour::default());
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-cancel");
    let dest = dir.0.join("file.bin");
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let url = s.url("/file.bin");
    let cancel = CancelToken::new();
    let err = d
        .fetch(&url, &dest, &exp, &cancel, &mut |done, _| {
            if done > 0 {
                cancel.cancel();
            }
        })
        .unwrap_err();
    let DownloadError::Cancelled { done, total } = err else {
        panic!("{err:?}")
    };
    assert_eq!(total, 200_000);
    assert!(done > 0 && done < total, "{done}");
    assert!(err.to_string().contains("run again to resume"), "{err}");
    assert_eq!(fs::read(part_path(&dest)).unwrap(), b[..done as usize]);
    assert!(!dest.exists());

    let (out, _) = fetch(&d, &url, &dest, &exp);
    assert_eq!(out.unwrap(), Outcome::Downloaded { resumed_from: done });
    assert_eq!(fs::read(&dest).unwrap(), b);
}

#[test]
fn download_of_a_present_file_makes_no_request() {
    let b = body();
    let s = serve(b.clone(), Behaviour::default());
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-present");
    let dest = dir.0.join("file.bin");
    fs::write(&dest, &b).unwrap();
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let (out, _) = fetch(&d, &s.url("/file.bin"), &dest, &exp);
    assert_eq!(out.unwrap(), Outcome::Present);
    assert!(s.ranges().is_empty());
    assert!(d.audit_log().is_empty());

    // A corrupt file at dest is replaced.
    let mut bad = b.clone();
    bad[10] ^= 0xff;
    fs::write(&dest, &bad).unwrap();
    let (out, _) = fetch(&d, &s.url("/file.bin"), &dest, &exp);
    assert_eq!(out.unwrap(), Outcome::Downloaded { resumed_from: 0 });
    assert_eq!(fs::read(&dest).unwrap(), b);
}

#[test]
fn download_of_a_complete_part_makes_no_request_and_says_so() {
    let b = body();
    let s = serve(b.clone(), Behaviour::default());
    let d = downloader(local(&[s.port]));
    let dir = TempDir::new("dl-complete-part");
    let dest = dir.0.join("file.bin");
    fs::write(part_path(&dest), &b).unwrap();
    let sum = sha(&b);
    let exp = Expected {
        sha256: &sum,
        size: b.len() as u64,
    };
    let (out, _) = fetch(&d, &s.url("/file.bin"), &dest, &exp);
    assert_eq!(out.unwrap(), Outcome::CompletedPart);
    assert!(s.ranges().is_empty());
    assert!(d.audit_log().is_empty());
    assert_eq!(fs::read(&dest).unwrap(), b);
    assert!(!part_path(&dest).exists());

    // A complete-size .part that does not verify is deleted, not renamed.
    fs::remove_file(&dest).unwrap();
    let mut bad = b.clone();
    bad[10] ^= 0xff;
    fs::write(part_path(&dest), &bad).unwrap();
    let (out, _) = fetch(&d, &s.url("/file.bin"), &dest, &exp);
    assert!(
        matches!(out, Err(DownloadError::Verification { .. })),
        "{out:?}"
    );
    assert!(!dest.exists() && !part_path(&dest).exists());
    assert!(s.ranges().is_empty());
}

#[test]
fn download_manifest_allowlist_has_the_manifest_hosts() {
    let list = manifest_allowlist().unwrap();
    // Read independently of `allowlist_of`.
    let manifest: toml::Table = MODELS_MANIFEST.parse().unwrap();
    let strings = |key: &str| -> Vec<String> {
        manifest[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    let hosts = strings("hosts");
    let redirects = strings("redirect_hosts");
    assert!(!hosts.is_empty() && !redirects.is_empty());
    let exact: Vec<(String, u16)> = list.entries().map(|(h, p)| (h.to_string(), p)).collect();
    let mut want: Vec<(String, u16)> = hosts.iter().map(|h| (h.clone(), 443)).collect();
    want.sort();
    assert_eq!(exact, want);
    let got: Vec<(String, u16)> = list
        .redirect_entries()
        .map(|(h, p)| (h.to_string(), p))
        .collect();
    let mut want: Vec<(String, u16)> = redirects.iter().map(|h| (h.clone(), 443)).collect();
    want.sort();
    assert_eq!(got, want);

    let u = |s: &str| Url::parse(s).unwrap();
    assert!(list.check_hop(&u("https://huggingface.co/a/b"), 0).is_ok());
    assert!(list.check_hop(&u("https://us.aws.cdn.hf.co/x"), 1).is_ok());
    assert_eq!(
        list.check_hop(&u("https://us.aws.cdn.hf.co/x"), 0),
        Err(DenyReason::HostNotAllowed)
    );
    assert_eq!(
        list.check_hop(&u("http://huggingface.co/a"), 0),
        Err(DenyReason::InsecureScheme)
    );
    assert!(Downloader::for_manifest(Arc::new(ManualClock::new(T0))).is_ok());
}

#[test]
fn download_manifest_hosts_that_cannot_be_allowlisted_fail() {
    let err = allowlist_of("hosts = [\"a.example/path\"]").unwrap_err();
    assert!(matches!(err, DownloadError::Host { .. }), "{err:?}");
    let err = allowlist_of("hosts = []\nredirect_hosts = [\"*.127.0.0.1\"]").unwrap_err();
    assert!(matches!(err, DownloadError::Host { .. }), "{err:?}");
    assert!(matches!(
        allowlist_of("nothing = 1").unwrap_err(),
        DownloadError::Manifest(_)
    ));
}

#[test]
fn download_deadline_is_sized_for_the_whole_file() {
    // xtask's `download::BODY_DEADLINE_BASE` and `MIN_BODY_RATE` (120 s, 128 KiB/s).
    assert_eq!(BODY_DEADLINE_BASE, Duration::from_secs(120));
    assert_eq!(MIN_BODY_RATE, 128 * 1024);
    assert_eq!(transfer_deadline(0), BODY_DEADLINE_BASE);
    assert_eq!(
        transfer_deadline(MIN_BODY_RATE * 100 + 1),
        BODY_DEADLINE_BASE + Duration::from_secs(101)
    );
    // The largest model gets a few hours; nothing is unbounded.
    assert!(transfer_deadline(1_624_555_275) < Duration::from_secs(4 * 3600));
}

#[test]
fn download_content_range_start_parses_the_first_byte() {
    assert_eq!(
        content_range_start("bytes 70000-199999/200000"),
        Some(70_000)
    );
    assert_eq!(content_range_start(" bytes  5-9/*"), Some(5));
    assert_eq!(content_range_start("bytes */200000"), None);
    assert_eq!(content_range_start("items 1-2/3"), None);
    assert_eq!(content_range_start(""), None);
}
