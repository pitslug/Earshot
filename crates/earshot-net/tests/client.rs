//! End-to-end tests of `NetClient` against local HTTP/1.1 servers bound to 127.0.0.1:0.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use earshot_core::identity::ManualClock;
use earshot_net::{
    Allowlist, AuditLog, AuditOutcome, DenyReason, Method, NetClient, NetConfig, NetError,
    NetRequest,
};
use url::Url;

/// One request as the test server saw it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// A minimal HTTP/1.1 server: answers every connection with `respond(&Seen)` (a full raw
/// response) and records what it received. Lives until the test process exits. Every
/// connection ends with [`lingering_close`]; one it cannot read gets a 400.
struct TestServer {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// One line per connection (status line sent, write, shutdown and drain results),
    /// printed when a test panics.
    closes: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    fn start(respond: impl Fn(&Seen) -> String + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let (log, close_log) = (Arc::clone(&seen), Arc::clone(&closes));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let resp = match read_request(&stream) {
                    Some(req) => {
                        let resp = respond(&req);
                        log.lock().unwrap().push(req);
                        resp
                    }
                    None => {
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                };
                let status = resp.lines().next().unwrap_or_default().to_string();
                let report = lingering_close(stream, resp.as_bytes());
                close_log
                    .lock()
                    .unwrap()
                    .push(format!("{status:?}: {report}"));
            }
        });
        TestServer { port, seen, closes }
    }

    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}{path}", self.port)).unwrap()
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if thread::panicking() {
            let closes = self.closes.lock().unwrap_or_else(|e| e.into_inner());
            eprintln!("test server :{}: {} connection(s)", self.port, closes.len());
            for c in closes.iter() {
                eprintln!("  {c}");
            }
        }
    }
}

/// How long [`lingering_close`] waits for the client to close its side.
const LINGER_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one write of a response may block on a client that stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Writes `response`, shuts down the write side (FIN), then reads until the client closes
/// (or a read fails, or [`LINGER_TIMEOUT`] passes) before dropping the socket. Dropping
/// right after the write can reset the connection (on Windows, `closesocket` with unread
/// bytes sends an RST) and the client loses the response with `os error 10054`. A copy of
/// `earshot_llm_replay::tcp::lingering_close`, which this crate cannot depend on (that
/// crate depends on `earshot-net`). Returns a one-line report for diagnostics.
fn lingering_close(mut stream: TcpStream, response: &[u8]) -> String {
    let write = stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .and_then(|()| stream.write_all(response))
        .and_then(|()| stream.flush())
        .map_err(|e| e.kind());
    let shutdown = stream.shutdown(Shutdown::Write).map_err(|e| e.kind());
    let deadline = Instant::now() + LINGER_TIMEOUT;
    let mut drained = 0usize;
    let mut buf = [0u8; 4096];
    let end = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break "timed out".to_string();
        }
        if let Err(e) = stream.set_read_timeout(Some(left)) {
            break format!("{:?}", e.kind());
        }
        match stream.read(&mut buf) {
            Ok(0) => break "eof".to_string(),
            Ok(n) => drained += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                break "timed out".to_string();
            }
            Err(e) => break format!("{:?}", e.kind()),
        }
    };
    format!(
        "response {} bytes, write {write:?}, shutdown {shutdown:?}, drained {drained} bytes, drain end {end}",
        response.len()
    )
}

fn read_request(stream: &TcpStream) -> Option<Seen> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        path,
        headers,
        body,
    })
}

fn ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn redirect(code: u16, location: &str) -> String {
    format!(
        "HTTP/1.1 {code} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

const T0: u64 = 1_790_000_000_000;

fn client(allow: &[&TestServer]) -> (NetClient, AuditLog, Arc<ManualClock>) {
    let mut list = Allowlist::new();
    for s in allow {
        list.allow_url(&s.url("/")).unwrap();
    }
    let clock = Arc::new(ManualClock::new(T0));
    let audit = AuditLog::new();
    let c = NetClient::new(NetConfig::new(list, clock.clone()).audit(audit.clone()))
        .expect("client builds");
    (c, audit, clock)
}

#[tokio::test]
async fn allowed_get_is_sent_and_audited() {
    let server = TestServer::start(|_| ok("hello"));
    let (c, audit, _) = client(&[&server]);

    let resp = c
        .send(NetRequest::get(server.url("/ok?secret=1")))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"hello");

    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");

    let log = audit.entries();
    assert_eq!(log.len(), 1, "{log:?}");
    let e = &log[0];
    assert_eq!(e.timestamp_ms, T0);
    assert_eq!(e.method, "GET");
    assert_eq!(e.scheme, "http");
    assert_eq!(e.host, "127.0.0.1");
    assert_eq!(e.port, server.port);
    assert_eq!(e.path, "/ok", "the query string is never logged");
    assert_eq!(e.request_bytes, 0);
    assert_eq!(e.response_bytes, 5);
    assert_eq!(e.status, Some(200));
    assert_eq!(e.outcome, AuditOutcome::Completed);
}

#[tokio::test]
async fn post_body_bytes_and_clock_are_recorded() {
    let server = TestServer::start(|req| ok(&format!("{}", req.body.len())));
    let (c, audit, clock) = client(&[&server]);

    clock.set_ms(T0 + 42);
    let body = br#"{"model":"x"}"#.to_vec();
    let resp = c
        .send(
            NetRequest::new(Method::POST, server.url("/v1/chat/completions"))
                .header("content-type", "application/json")
                .body(body.clone()),
        )
        .await
        .unwrap();
    assert_eq!(resp.body, b"13");
    assert_eq!(server.seen()[0].body, body);

    let e = &audit.entries()[0];
    assert_eq!(e.timestamp_ms, T0 + 42);
    assert_eq!(e.method, "POST");
    assert_eq!(e.path, "/v1/chat/completions");
    assert_eq!(e.request_bytes, 13);
    assert_eq!(e.response_bytes, 2);
}

#[tokio::test]
async fn host_not_on_the_allowlist_is_refused_before_any_connection() {
    let allowed = TestServer::start(|_| ok("a"));
    let other = TestServer::start(|_| ok("b"));
    let (c, audit, _) = client(&[&allowed]);

    // Same host, different port: exact host + port matching.
    let err = c.send(NetRequest::get(other.url("/x"))).await.unwrap_err();
    assert!(
        matches!(
            err,
            NetError::NotAllowed {
                reason: DenyReason::HostNotAllowed,
                ..
            }
        ),
        "{err:?}"
    );
    // Different host spelling for the same address is a different host.
    let localhost = Url::parse(&format!("http://localhost:{}/x", allowed.port)).unwrap();
    let err = c.send(NetRequest::get(localhost)).await.unwrap_err();
    assert!(matches!(err, NetError::NotAllowed { .. }), "{err:?}");

    assert!(other.seen().is_empty(), "nothing reached the other server");
    assert!(allowed.seen().is_empty());

    let log = audit.entries();
    assert_eq!(log.len(), 2);
    for e in &log {
        assert_eq!(e.outcome, AuditOutcome::Blocked(DenyReason::HostNotAllowed));
        assert_eq!(e.request_bytes, 0);
        assert_eq!(e.response_bytes, 0);
        assert_eq!(e.status, None);
    }
    assert_eq!(log[0].port, other.port);
    assert_eq!(log[1].host, "localhost");
}

#[tokio::test]
async fn redirect_to_a_host_not_on_the_allowlist_is_refused() {
    let target = TestServer::start(|_| ok("leaked"));
    let target_url = target.url("/steal");
    let origin = TestServer::start(move |_| redirect(302, target_url.as_str()));
    let (c, audit, _) = client(&[&origin]);

    let err = c
        .send(NetRequest::get(origin.url("/start")))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            NetError::NotAllowed {
                reason: DenyReason::HostNotAllowed,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(origin.seen().len(), 1);
    assert!(target.seen().is_empty(), "the redirect was not followed");

    let log = audit.entries();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0].port, origin.port);
    assert_eq!(log[0].status, Some(302));
    assert_eq!(log[0].outcome, AuditOutcome::Completed);
    assert_eq!(log[1].port, target.port);
    assert_eq!(log[1].path, "/steal");
    assert_eq!(
        log[1].outcome,
        AuditOutcome::Blocked(DenyReason::HostNotAllowed)
    );
}

#[tokio::test]
async fn redirect_to_an_allowlisted_host_is_followed_hop_by_hop() {
    let target = TestServer::start(|req| {
        let auth = req.headers.iter().any(|(k, _)| k == "authorization");
        ok(if auth { "auth" } else { "no-auth" })
    });
    let target_url = target.url("/final");
    let origin = TestServer::start(move |_| redirect(307, target_url.as_str()));
    let (c, audit, _) = client(&[&origin, &target]);

    let resp = c
        .send(
            NetRequest::new(Method::POST, origin.url("/start"))
                .header("authorization", "Bearer k")
                .body(b"abc".to_vec()),
        )
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.url.as_str(), target.url("/final").as_str());
    // Cross-origin hop: credentials are dropped; 307 keeps method and body.
    assert_eq!(resp.body, b"no-auth");
    let seen = target.seen();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].body, b"abc");

    let log = audit.entries();
    assert_eq!(log.len(), 2);
    assert_eq!((log[0].status, log[0].request_bytes), (Some(307), 3));
    assert_eq!((log[1].status, log[1].request_bytes), (Some(200), 3));
}

#[tokio::test]
async fn see_other_turns_post_into_get_without_body() {
    let server = TestServer::start(|req| {
        if req.path == "/start" {
            redirect(303, "/next")
        } else {
            ok(&req.method)
        }
    });
    let (c, audit, _) = client(&[&server]);
    let resp = c
        .send(NetRequest::new(Method::POST, server.url("/start")).body(b"payload".to_vec()))
        .await
        .unwrap();
    assert_eq!(resp.body, b"GET");
    let seen = server.seen();
    assert_eq!(seen[1].path, "/next");
    assert!(seen[1].body.is_empty());
    let log = audit.entries();
    assert_eq!(log[1].method, "GET");
    assert_eq!(log[1].request_bytes, 0);
}

#[tokio::test]
async fn redirect_loops_stop_at_the_hop_limit() {
    let server = TestServer::start(|_| redirect(302, "/again"));
    let (c, audit, _) = client(&[&server]);
    let err = c.send(NetRequest::get(server.url("/"))).await.unwrap_err();
    assert!(matches!(err, NetError::TooManyRedirects), "{err:?}");
    assert_eq!(audit.entries().len(), earshot_net::MAX_REDIRECTS + 1);
}

#[tokio::test]
async fn connection_failures_are_audited() {
    // Bind then drop a listener so the port is (very likely) closed.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = Url::parse(&format!("http://127.0.0.1:{port}/gone")).unwrap();
    let mut list = Allowlist::new();
    list.allow_url(&url).unwrap();
    let audit = AuditLog::new();
    let c =
        NetClient::new(NetConfig::new(list, Arc::new(ManualClock::new(T0))).audit(audit.clone()))
            .unwrap();
    let err = c.send(NetRequest::get(url)).await.unwrap_err();
    assert!(matches!(err, NetError::Transport(_)), "{err:?}");
    let log = audit.entries();
    assert_eq!(log.len(), 1);
    assert!(matches!(log[0].outcome, AuditOutcome::Failed(_)));
    assert_eq!(log[0].status, None);
}

#[tokio::test]
async fn public_hosts_need_https_even_when_allowlisted() {
    let mut list = Allowlist::new();
    list.allow_https_host("example.com").unwrap();
    let audit = AuditLog::new();
    let c =
        NetClient::new(NetConfig::new(list, Arc::new(ManualClock::new(T0))).audit(audit.clone()))
            .unwrap();
    let err = c
        .send(NetRequest::get(
            Url::parse("http://example.com:443/").unwrap(),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            NetError::NotAllowed {
                reason: DenyReason::InsecureScheme,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        audit.entries()[0].outcome,
        AuditOutcome::Blocked(DenyReason::InsecureScheme)
    );
}

fn has_header(req: &Seen, name: &str) -> bool {
    req.headers.iter().any(|(k, _)| k == name)
}

fn client_with(
    list: Allowlist,
    config: impl FnOnce(NetConfig) -> NetConfig,
) -> (NetClient, AuditLog) {
    let audit = AuditLog::new();
    let c = NetClient::new(config(
        NetConfig::new(list, Arc::new(ManualClock::new(T0))).audit(audit.clone()),
    ))
    .unwrap();
    (c, audit)
}

fn dead_port() -> u16 {
    // Bind then drop a listener so the port is (very likely) closed.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test]
async fn transport_errors_never_contain_the_query() {
    let url = Url::parse(&format!(
        "http://127.0.0.1:{}/gone?secret=1&X-Amz-Signature=abc",
        dead_port()
    ))
    .unwrap();
    let mut list = Allowlist::new();
    list.allow_url(&url).unwrap();
    let (c, audit) = client_with(list, |c| c);
    let err = c.send(NetRequest::get(url)).await.unwrap_err();
    assert!(matches!(err, NetError::Transport(_)), "{err:?}");
    let AuditOutcome::Failed(why) = &audit.entries()[0].outcome else {
        panic!("{:?}", audit.entries())
    };
    for text in [
        err.to_string(),
        format!("{err:?}"),
        why.clone(),
        format!("{:?}", audit.entries()),
    ] {
        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("Signature"), "{text}");
    }
}

#[tokio::test]
async fn same_origin_redirect_keeps_caller_headers() {
    let server = TestServer::start(|req| {
        if req.path == "/start" {
            redirect(302, "/next")
        } else if has_header(req, "authorization") && has_header(req, "x-api-key") {
            ok("kept")
        } else {
            ok("dropped")
        }
    });
    let (c, _, _) = client(&[&server]);
    let resp = c
        .send(
            NetRequest::get(server.url("/start"))
                .header("authorization", "Bearer k")
                .header("x-api-key", "k"),
        )
        .await
        .unwrap();
    assert_eq!(resp.body, b"kept");
}

#[tokio::test]
async fn cross_origin_redirect_drops_every_caller_header_except_content_type() {
    let target = TestServer::start(|_| ok("done"));
    let target_url = target.url("/final");
    let origin = TestServer::start(move |_| redirect(307, target_url.as_str()));
    let (c, _, _) = client(&[&origin, &target]);
    c.send(
        NetRequest::new(Method::POST, origin.url("/start"))
            .header("authorization", "Bearer k")
            .header("x-api-key", "k")
            .header("api-key", "k")
            .header("cookie", "c=1")
            .header("x-custom", "1")
            .header("content-type", "application/json")
            .body(b"{}".to_vec()),
    )
    .await
    .unwrap();
    let seen = &origin.seen()[0];
    assert!(has_header(seen, "x-api-key") && has_header(seen, "authorization"));
    let seen = &target.seen()[0];
    for h in [
        "authorization",
        "x-api-key",
        "api-key",
        "cookie",
        "x-custom",
    ] {
        assert!(!has_header(seen, h), "{h} reached another origin: {seen:?}");
    }
    assert!(has_header(seen, "content-type"));
    assert_eq!(seen.body, b"{}");
}

#[tokio::test]
async fn redirect_with_credentials_in_location_is_refused() {
    let target = TestServer::start(|_| ok("x"));
    let loc = format!("http://user:pw@127.0.0.1:{}/x", target.port);
    let origin = TestServer::start(move |_| redirect(302, &loc));
    let (c, audit, _) = client(&[&origin, &target]);
    let err = c.send(NetRequest::get(origin.url("/"))).await.unwrap_err();
    assert!(
        matches!(
            err,
            NetError::NotAllowed {
                reason: DenyReason::BadUrl,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.to_string().contains("pw"), "{err}");
    assert!(target.seen().is_empty());
    assert_eq!(
        audit.entries()[1].outcome,
        AuditOutcome::Blocked(DenyReason::BadUrl)
    );
}

#[tokio::test]
async fn redirect_downgrade_to_public_http_is_refused() {
    let origin = TestServer::start(|_| redirect(302, "http://example.com:443/x"));
    let mut list = Allowlist::new();
    list.allow_url(&origin.url("/")).unwrap();
    list.allow_https_host("example.com").unwrap();
    let (c, audit) = client_with(list, |c| c);
    let err = c.send(NetRequest::get(origin.url("/"))).await.unwrap_err();
    assert!(
        matches!(
            err,
            NetError::NotAllowed {
                reason: DenyReason::InsecureScheme,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(audit.entries()[1].host, "example.com");
}

#[tokio::test]
async fn bodies_over_max_body_fail_and_are_audited() {
    let big = "x".repeat(100);
    let with_len = TestServer::start(move |_| ok(&big));
    // No Content-Length: the limit must hold while streaming, not only on the header.
    let streamed = TestServer::start(|_| {
        format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}",
            "y".repeat(5000)
        )
    });
    let small = TestServer::start(|_| ok("0123456789"));
    let mut list = Allowlist::new();
    for s in [&with_len, &streamed, &small] {
        list.allow_url(&s.url("/")).unwrap();
    }
    let (c, audit) = client_with(list, |c| c.max_body(10));

    let resp = c.send(NetRequest::get(small.url("/"))).await.unwrap();
    assert_eq!(resp.body.len(), 10, "exactly the limit is fine");
    for s in [&with_len, &streamed] {
        let err = c.send(NetRequest::get(s.url("/"))).await.unwrap_err();
        assert!(
            matches!(err, NetError::BodyTooLarge { limit: 10 }),
            "{err:?}"
        );
    }
    let log = audit.entries();
    assert_eq!(log.len(), 3);
    for e in &log[1..] {
        assert!(matches!(e.outcome, AuditOutcome::Failed(_)), "{e:?}");
        assert_eq!(e.status, Some(200));
    }
}

/// Reads a stream to its end.
async fn read_all(stream: &mut earshot_net::NetStream) -> Result<Vec<u8>, NetError> {
    let mut out = Vec::new();
    while let Some(piece) = stream.chunk().await? {
        out.extend_from_slice(&piece);
    }
    Ok(out)
}

#[tokio::test]
async fn open_streams_a_body_larger_than_max_body_and_audits_it_once() {
    let body = "z".repeat(300_000);
    let expected = body.clone();
    let server = TestServer::start(move |_| ok(&body));
    let mut list = Allowlist::new();
    list.allow_url(&server.url("/")).unwrap();
    let (c, audit) = client_with(list, |c| c.max_body(10));

    // `send` still refuses it.
    let err = c.send(NetRequest::get(server.url("/"))).await.unwrap_err();
    assert!(matches!(err, NetError::BodyTooLarge { .. }), "{err:?}");

    let mut stream = c
        .open(NetRequest::get(server.url("/big?secret=1")))
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    assert_eq!(stream.content_length(), Some(300_000));
    assert!(!format!("{stream:?}").contains("secret"));
    assert_eq!(audit.len(), 1, "nothing recorded before the body ends");
    let got = read_all(&mut stream).await.unwrap();
    assert_eq!(got.len(), expected.len());
    assert!(got == expected.as_bytes());
    // The end is sticky and recorded once.
    assert!(stream.chunk().await.unwrap().is_none());
    drop(stream);

    let log = audit.entries();
    assert_eq!(log.len(), 2, "{log:?}");
    let e = &log[1];
    assert_eq!(e.path, "/big");
    assert_eq!(e.status, Some(200));
    assert_eq!(e.response_bytes, 300_000);
    assert_eq!(e.outcome, AuditOutcome::Completed);
}

#[tokio::test]
async fn open_dropped_before_the_end_is_audited_as_failed() {
    let body = "z".repeat(300_000);
    let server = TestServer::start(move |_| ok(&body));
    let (c, audit, _) = client(&[&server]);
    let mut stream = c.open(NetRequest::get(server.url("/f"))).await.unwrap();
    let first = stream.chunk().await.unwrap().expect("a first piece");
    assert!(!first.is_empty());
    drop(stream);
    let log = audit.entries();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(
        log[0].outcome,
        AuditOutcome::Failed(earshot_net::STREAM_DROPPED.to_string())
    );
    assert_eq!(log[0].status, Some(200));
    assert_eq!(log[0].response_bytes, first.len() as u64);
}

#[tokio::test]
async fn open_follows_a_redirect_to_a_redirect_only_host_on_a_later_hop() {
    let target = TestServer::start(|req| {
        let auth = req.headers.iter().any(|(k, _)| k == "authorization");
        ok(if auth { "auth" } else { "file" })
    });
    let target_url = target.url("/cdn/file.bin");
    let origin = TestServer::start(move |_| redirect(302, target_url.as_str()));
    let mut list = Allowlist::new();
    list.allow_url(&origin.url("/")).unwrap();
    list.allow_redirect_host(&format!("127.0.0.1:{}", target.port))
        .unwrap();
    let (c, audit) = client_with(list, |c| c);

    let mut stream = c
        .open(NetRequest::get(origin.url("/file.bin")).header("authorization", "Bearer k"))
        .await
        .unwrap();
    assert_eq!(stream.url().as_str(), target.url("/cdn/file.bin").as_str());
    assert_eq!(read_all(&mut stream).await.unwrap(), b"file");
    drop(stream);
    let log = audit.entries();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!((log[0].port, log[0].status), (origin.port, Some(302)));
    assert_eq!(log[0].outcome, AuditOutcome::Completed);
    assert_eq!((log[1].port, log[1].status), (target.port, Some(200)));
    assert_eq!(log[1].response_bytes, 4);
    assert_eq!(log[1].outcome, AuditOutcome::Completed);
}

#[tokio::test]
async fn range_from_is_sent_on_every_hop_while_caller_headers_are_dropped() {
    let target = TestServer::start(|_| ok("rest"));
    let target_url = target.url("/cdn");
    let origin = TestServer::start(move |_| redirect(302, target_url.as_str()));
    let (c, _, _) = client(&[&origin, &target]);
    let mut stream = c
        .open(
            NetRequest::get(origin.url("/f"))
                .range_from(70_000)
                .header("authorization", "Bearer k")
                .header("x-custom", "1"),
        )
        .await
        .unwrap();
    assert_eq!(read_all(&mut stream).await.unwrap(), b"rest");
    let range = |s: &Seen| {
        s.headers
            .iter()
            .find(|(k, _)| k == "range")
            .map(|(_, v)| v.clone())
    };
    let first = &origin.seen()[0];
    assert_eq!(range(first).as_deref(), Some("bytes=70000-"));
    assert!(has_header(first, "authorization"));
    let cdn = &target.seen()[0];
    assert_eq!(range(cdn).as_deref(), Some("bytes=70000-"), "{cdn:?}");
    assert!(!has_header(cdn, "authorization") && !has_header(cdn, "x-custom"));
}

#[tokio::test]
async fn send_follows_a_redirect_to_a_redirect_only_host_too() {
    let target = TestServer::start(|_| ok("cdn"));
    let target_url = target.url("/x");
    let origin = TestServer::start(move |_| redirect(307, target_url.as_str()));
    let mut list = Allowlist::new();
    list.allow_url(&origin.url("/")).unwrap();
    list.allow_redirect_host(&format!("127.0.0.1:{}", target.port))
        .unwrap();
    let (c, audit) = client_with(list, |c| c);
    let resp = c.send(NetRequest::get(origin.url("/"))).await.unwrap();
    assert_eq!(resp.body, b"cdn");
    assert_eq!(audit.len(), 2);
}

#[tokio::test]
async fn open_refuses_a_redirect_to_an_unlisted_host() {
    let target = TestServer::start(|_| ok("leaked"));
    let target_url = target.url("/steal");
    let origin = TestServer::start(move |_| redirect(302, target_url.as_str()));
    let mut list = Allowlist::new();
    list.allow_url(&origin.url("/")).unwrap();
    // A redirect entry for another host spelling does not cover the target.
    list.allow_redirect_host(&format!("localhost:{}", target.port))
        .unwrap();
    let (c, audit) = client_with(list, |c| c);
    let err = c
        .open(NetRequest::get(origin.url("/start")))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            NetError::NotAllowed {
                reason: DenyReason::HostNotAllowed,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(target.seen().is_empty(), "the redirect was not followed");
    let log = audit.entries();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0].outcome, AuditOutcome::Completed);
    assert_eq!(log[1].port, target.port);
    assert_eq!(
        log[1].outcome,
        AuditOutcome::Blocked(DenyReason::HostNotAllowed)
    );
}

#[tokio::test]
async fn a_redirect_only_host_is_refused_as_the_first_hop() {
    let server = TestServer::start(|_| ok("x"));
    let mut list = Allowlist::new();
    list.allow_redirect_host(&format!("127.0.0.1:{}", server.port))
        .unwrap();
    let (c, audit) = client_with(list, |c| c);
    for result in [
        c.send(NetRequest::get(server.url("/"))).await.map(|_| ()),
        c.open(NetRequest::get(server.url("/"))).await.map(|_| ()),
    ] {
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                NetError::NotAllowed {
                    reason: DenyReason::HostNotAllowed,
                    ..
                }
            ),
            "{err:?}"
        );
    }
    assert!(server.seen().is_empty());
    let log = audit.entries();
    assert_eq!(log.len(), 2);
    for e in &log {
        assert_eq!(e.outcome, AuditOutcome::Blocked(DenyReason::HostNotAllowed));
    }
}

#[tokio::test]
async fn open_read_failure_is_audited_once_and_sticky() {
    // Announces 1000 bytes, sends 10, then closes: the read fails.
    let server = TestServer::start(|_| {
        "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n0123456789".to_string()
    });
    let (c, audit, _) = client(&[&server]);
    let mut stream = c
        .open(NetRequest::get(server.url("/?secret=1")))
        .await
        .unwrap();
    let err = read_all(&mut stream).await.unwrap_err();
    assert!(matches!(err, NetError::Transport(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("secret"));
    let again = stream.chunk().await.unwrap_err();
    assert!(matches!(again, NetError::StreamFailed), "{again:?}");
    drop(stream);
    let log = audit.entries();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(matches!(log[0].outcome, AuditOutcome::Failed(_)));
    assert_ne!(
        log[0].outcome,
        AuditOutcome::Failed(earshot_net::STREAM_DROPPED.to_string())
    );
    assert_eq!(log[0].response_bytes, 10);
}

/// How long [`stalling_server`] waits for the client to go away at most.
const STALL_HOLD: Duration = Duration::from_secs(30);

/// A server that reads each request, sends `prefix` (possibly nothing) and then stays
/// silent until the client closes the connection (or [`STALL_HOLD`] passes), then ends it
/// through [`lingering_close`]. Returns its port.
fn stalling_server(prefix: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                if read_request(&stream).is_none() {
                    lingering_close(stream, b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return;
                }
                let _ = stream.write_all(prefix.as_bytes());
                let _ = stream.flush();
                let deadline = Instant::now() + STALL_HOLD;
                let mut sink = [0u8; 64];
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
                        break;
                    }
                    match stream.read(&mut sink) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                lingering_close(stream, b"");
            });
        }
    });
    port
}

#[tokio::test]
async fn dropped_request_futures_record_the_hop_in_flight_once() {
    let port = stalling_server("");
    let url = Url::parse(&format!("http://127.0.0.1:{port}/slow?secret=1")).unwrap();
    let mut list = Allowlist::new();
    list.allow_url(&url).unwrap();
    let (c, audit) = client_with(list, |c| c);
    let wait = Duration::from_millis(300);

    let open = tokio::time::timeout(wait, c.open(NetRequest::get(url.clone()))).await;
    assert!(open.is_err(), "the server never answers");
    let send = tokio::time::timeout(wait, c.send(NetRequest::get(url))).await;
    assert!(send.is_err(), "the server never answers");

    let log = audit.entries();
    assert_eq!(log.len(), 2, "{log:?}");
    for e in &log {
        assert_eq!(
            e.outcome,
            AuditOutcome::Failed(earshot_net::CANCELLED_BEFORE_RESPONSE.to_string())
        );
        assert_eq!((e.status, e.port, e.path.as_str()), (None, port, "/slow"));
    }
}

#[tokio::test]
async fn a_send_dropped_while_reading_the_body_records_the_hop_once() {
    let port = stalling_server(
        "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n0123456789",
    );
    let url = Url::parse(&format!("http://127.0.0.1:{port}/body")).unwrap();
    let mut list = Allowlist::new();
    list.allow_url(&url).unwrap();
    let (c, audit) = client_with(list, |c| c);
    let send = tokio::time::timeout(Duration::from_millis(300), c.send(NetRequest::get(url))).await;
    assert!(send.is_err(), "the body never ends");
    let log = audit.entries();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(
        log[0].outcome,
        AuditOutcome::Failed(earshot_net::STREAM_DROPPED.to_string())
    );
    assert_eq!(log[0].status, Some(200));
}

#[tokio::test]
async fn a_request_dropped_while_reading_a_redirect_body_records_that_hop_once() {
    // The redirect hop's own body never ends: the future is dropped inside the hop loop's
    // `read_body`, after the 302 arrived, so the hop is a STREAM_DROPPED entry (not
    // CANCELLED_BEFORE_RESPONSE) and the Location is never followed.
    let port = stalling_server(
        "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n0123456789",
    );
    let url = Url::parse(&format!("http://127.0.0.1:{port}/hop")).unwrap();
    let mut list = Allowlist::new();
    list.allow_url(&url).unwrap();
    let (c, audit) = client_with(list, |c| c);
    let wait = Duration::from_millis(300);

    let open = tokio::time::timeout(wait, c.open(NetRequest::get(url.clone()))).await;
    assert!(open.is_err(), "the redirect body never ends");
    let send = tokio::time::timeout(wait, c.send(NetRequest::get(url))).await;
    assert!(send.is_err(), "the redirect body never ends");

    let log = audit.entries();
    assert_eq!(log.len(), 2, "one entry per dropped future: {log:?}");
    for e in &log {
        assert_eq!(
            e.outcome,
            AuditOutcome::Failed(earshot_net::STREAM_DROPPED.to_string())
        );
        assert_eq!(
            (e.status, e.port, e.path.as_str(), e.response_bytes),
            (Some(302), port, "/hop", 10)
        );
    }
}

#[tokio::test]
async fn default_timeout_applies_when_the_request_sets_none() {
    use std::time::{Duration, Instant};
    assert!(earshot_net::DEFAULT_TIMEOUT >= Duration::from_secs(60));
    let slow = TestServer::start(|_| {
        thread::sleep(Duration::from_secs(3));
        ok("late")
    });
    let mut list = Allowlist::new();
    list.allow_url(&slow.url("/")).unwrap();
    let (c, audit) = client_with(list, |c| c.timeout(Duration::from_millis(300)));
    let started = Instant::now();
    let err = c
        .send(NetRequest::get(slow.url("/?secret=1")))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let NetError::Transport(e) = &err else {
        panic!("{err:?}")
    };
    assert!(e.is_timeout(), "{e:?}");
    assert!(!format!("{err} {err:?}").contains("secret"));
    assert!(matches!(
        audit.entries()[0].outcome,
        AuditOutcome::Failed(_)
    ));
}
