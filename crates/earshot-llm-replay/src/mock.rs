//! A scripted LLM endpoint on 127.0.0.1 for tests and acceptance, speaking the
//! OpenAI-compatible chat completions shape ([`MockResponse::completion`]) or the
//! Anthropic Messages shape ([`MockResponse::anthropic_text`],
//! [`MockResponse::anthropic_tool`], [`MockResponse::anthropic_error`]). It answers
//! requests in order with the scripted responses and records what it received; it does
//! not route by path, so tests assert the path of each recorded request.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use earshot_net::Url;

use crate::tcp::{self, CloseReport};

/// Largest request the mock accepts (headers plus body).
const MAX_REQUEST: usize = 16 * 1024 * 1024;

/// One scripted HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockResponse {
    /// HTTP status.
    pub status: u16,
    /// Body, sent as `application/json`.
    pub body: String,
    /// Wait this long before answering.
    pub delay: Duration,
}

impl MockResponse {
    /// A `200 OK` with `body`.
    pub fn ok(body: impl Into<String>) -> Self {
        Self::status(200, body)
    }

    /// A response with `status` and `body`.
    pub fn status(status: u16, body: impl Into<String>) -> Self {
        MockResponse {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    /// The same response after `delay`.
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// A chat completion whose message content is `content`.
    pub fn completion(content: &str, finish_reason: &str) -> Self {
        Self::ok(
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "created": 0,
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": content },
                    "finish_reason": finish_reason,
                }],
            })
            .to_string(),
        )
    }

    /// An Anthropic Messages reply: an empty `thinking` block (as adaptive thinking
    /// returns by default), then one `text` block holding `text`.
    pub fn anthropic_text(text: &str, stop_reason: &str) -> Self {
        Self::anthropic_message(
            serde_json::json!([
                { "type": "thinking", "thinking": "", "signature": "mock" },
                { "type": "text", "text": text },
            ]),
            stop_reason,
        )
    }

    /// An Anthropic Messages reply calling the tool `name` with `input`.
    pub fn anthropic_tool(name: &str, input: serde_json::Value, stop_reason: &str) -> Self {
        Self::anthropic_message(
            serde_json::json!([
                { "type": "tool_use", "id": "toolu_mock", "name": name, "input": input },
            ]),
            stop_reason,
        )
    }

    /// An Anthropic error reply: `{"type": "error", "error": {"type": .., "message": ..}}`.
    pub fn anthropic_error(status: u16, error_type: &str, message: &str) -> Self {
        Self::status(
            status,
            serde_json::json!({
                "type": "error",
                "error": { "type": error_type, "message": message },
            })
            .to_string(),
        )
    }

    fn anthropic_message(content: serde_json::Value, stop_reason: &str) -> Self {
        Self::ok(
            serde_json::json!({
                "id": "msg_mock",
                "type": "message",
                "role": "assistant",
                "model": "mock",
                "content": content,
                "stop_reason": stop_reason,
                "stop_sequence": null,
                "usage": { "input_tokens": 1, "output_tokens": 1 },
            })
            .to_string(),
        )
    }
}

/// A request as the mock received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// HTTP method.
    pub method: String,
    /// Request target (path and query).
    pub path: String,
    /// Headers, names lower-cased.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body as JSON (`Null` if it is not JSON).
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// Computes the response to a request (a stand-in model that reads the prompt).
pub type Responder = Box<dyn Fn(&RecordedRequest) -> MockResponse + Send>;

#[derive(Default)]
struct State {
    script: Vec<MockResponse>,
    next: usize,
    received: Vec<RecordedRequest>,
    unscripted: usize,
    responder: Option<Responder>,
    connections: Vec<ConnectionLog>,
}

/// The mock server. It listens on `127.0.0.1:0` (a free port, loopback only) and stops
/// when dropped.
pub struct MockServer {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl MockServer {
    /// Starts a server answering with `script`, in order. A request after the script is
    /// used up gets a 500 and is counted in [`MockServer::unscripted`].
    pub fn start(script: Vec<MockResponse>) -> io::Result<Self> {
        Self::spawn(State {
            script,
            ..State::default()
        })
    }

    /// Starts a server that answers every request with `responder(request)` (for example
    /// a summary that cites the segment ids found in the prompt). Requests are recorded as
    /// with [`MockServer::start`]; none is ever unscripted.
    pub fn start_with(
        responder: impl Fn(&RecordedRequest) -> MockResponse + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn(State {
            responder: Some(Box::new(responder)),
            ..State::default()
        })
    }

    fn spawn(state: State) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (Arc::clone(&state), Arc::clone(&stop));
        thread::Builder::new()
            .name("earshot-llm-mock".into())
            .spawn(move || {
                for (index, stream) in listener.incoming().enumerate() {
                    if sp.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        serve(stream, index, &st);
                    }
                }
            })?;
        Ok(MockServer { addr, state, stop })
    }

    /// The OpenAI-compatible endpoint base URL to configure: `http://127.0.0.1:<port>/v1`.
    pub fn endpoint(&self) -> Url {
        Url::parse(&format!("http://{}/v1", self.addr)).expect("valid loopback URL")
    }

    /// The Anthropic API root to configure: `http://127.0.0.1:<port>` (requests go to
    /// `/v1/messages`).
    pub fn base_url(&self) -> Url {
        Url::parse(&format!("http://{}", self.addr)).expect("valid loopback URL")
    }

    /// Requests received so far, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.lock().received.clone()
    }

    /// Scripted responses not yet served.
    pub fn remaining(&self) -> usize {
        let s = self.lock();
        s.script.len() - s.next
    }

    /// Requests that arrived after the script was used up.
    pub fn unscripted(&self) -> usize {
        self.lock().unscripted
    }

    /// What happened on each connection served so far, in accept order.
    pub fn connections(&self) -> Vec<ConnectionLog> {
        self.lock().connections.clone()
    }

    /// One line per connection served so far (bytes read, status, whether the response
    /// was fully written, shutdown and drain results), for a failing test's message.
    /// A server dropped while its thread panics prints this to stderr.
    pub fn diagnostics(&self) -> String {
        let s = self.lock();
        let mut out = format!(
            "mock {}: {} connection(s), {} request(s), {} unscripted",
            self.addr,
            s.connections.len(),
            s.received.len(),
            s.unscripted
        );
        for c in &s.connections {
            out.push_str(&format!("\n  {c}"));
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("{}", self.diagnostics());
        }
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop; it sees the flag and exits.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

/// How the mock's read of one connection's request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// A complete request was read and answered from the script or responder.
    Request,
    /// The request could not be read or parsed; answered with a 400.
    Malformed(String),
    /// The declared body exceeds the mock's limit; answered with a 413.
    TooLarge,
}

/// What happened on one accepted connection, for a failing test's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionLog {
    /// Accept order, from 0.
    pub index: usize,
    /// Bytes read from the socket before answering (may include bytes past the request).
    pub bytes_read: usize,
    /// How reading the request ended.
    pub outcome: ReadOutcome,
    /// Status sent; `None` until the reply is chosen.
    pub status: Option<u16>,
    /// The write, shutdown and drain of the lingering close; `None` while it is running
    /// (the drain waits for the client to close, up to [`tcp::LINGER_TIMEOUT`]).
    pub close: Option<CloseReport>,
}

impl fmt::Display for ConnectionLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "connection {}: read {} bytes ({:?}), ",
            self.index, self.bytes_read, self.outcome
        )?;
        match self.status {
            Some(status) => write!(f, "status {status}, ")?,
            None => write!(f, "reply not chosen yet, ")?,
        }
        match &self.close {
            Some(close) => write!(f, "{close}"),
            None => write!(f, "close still running"),
        }
    }
}

fn serve(stream: TcpStream, index: usize, state: &Mutex<State>) {
    let mut counted = Counting {
        stream: &stream,
        bytes: 0,
    };
    let read = read_request(&mut counted);
    let outcome = match &read {
        Ok(_) => ReadOutcome::Request,
        Err(outcome) => outcome.clone(),
    };
    // Logged before the reply is chosen, so a test failing during a scripted delay or the
    // lingering close still sees this connection.
    let slot = {
        let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
        s.connections.push(ConnectionLog {
            index,
            bytes_read: counted.bytes,
            outcome,
            status: None,
            close: None,
        });
        s.connections.len() - 1
    };
    let update = |edit: &dyn Fn(&mut ConnectionLog)| {
        edit(
            &mut state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .connections[slot],
        );
    };
    let (delay, status, response) = match read {
        Ok(req) => {
            let resp = respond(req, state);
            let head = format!(
                "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                resp.status,
                resp.body.len()
            );
            let mut bytes = head.into_bytes();
            bytes.extend_from_slice(resp.body.as_bytes());
            (resp.delay, resp.status, bytes)
        }
        Err(ReadOutcome::TooLarge) => (
            Duration::ZERO,
            413,
            tcp::error_response(413, "Payload Too Large", "mock: request too large"),
        ),
        Err(_) => (
            Duration::ZERO,
            400,
            tcp::error_response(400, "Bad Request", "mock: malformed request"),
        ),
    };
    update(&|c| c.status = Some(status));
    if !delay.is_zero() {
        thread::sleep(delay);
    }
    let close = tcp::lingering_close(stream, &response);
    update(&|c| c.close = Some(close.clone()));
}
/// Counts the bytes read through it.
struct Counting<'a> {
    stream: &'a TcpStream,
    bytes: usize,
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stream.read(buf)?;
        self.bytes += n;
        Ok(n)
    }
}

fn respond(req: RecordedRequest, state: &Mutex<State>) -> MockResponse {
    {
        let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
        let computed = s.responder.as_ref().map(|r| r(&req));
        s.received.push(req);
        if let Some(resp) = computed {
            resp
        } else if s.next < s.script.len() {
            s.next += 1;
            s.script[s.next - 1].clone()
        } else {
            s.unscripted += 1;
            MockResponse::status(
                500,
                r#"{"error":{"message":"mock: no scripted response left"}}"#,
            )
        }
    }
}

fn malformed(why: impl fmt::Display) -> ReadOutcome {
    ReadOutcome::Malformed(why.to_string())
}

/// Reads one line; a connection that ends before the line does is malformed.
fn read_line(r: &mut impl BufRead, what: &str) -> Result<String, ReadOutcome> {
    let mut line = String::new();
    match r.read_line(&mut line) {
        Ok(0) => Err(malformed(format!("connection closed before the {what}"))),
        Ok(_) => Ok(line),
        Err(e) => Err(malformed(format!("reading the {what}: {:?}", e.kind()))),
    }
}

fn read_request(stream: &mut Counting<'_>) -> Result<RecordedRequest, ReadOutcome> {
    let _ = stream
        .stream
        .set_read_timeout(Some(Duration::from_secs(10)));
    let mut r = BufReader::new(stream);
    let line = read_line(&mut r, "request line")?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(malformed(format!("bad request line {:?}", line.trim_end())));
    };
    let (method, path) = (method.to_string(), path.to_string());
    let mut headers = Vec::new();
    loop {
        let h = read_line(&mut r, "headers")?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let Some((k, v)) = h.split_once(':') else {
            return Err(malformed(format!("bad header line {h:?}")));
        };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let len = match headers.iter().find(|(k, _)| k == "content-length") {
        None => 0,
        Some((_, v)) => v
            .parse::<usize>()
            .map_err(|_| malformed(format!("bad Content-Length {v:?}")))?,
    };
    if len > MAX_REQUEST {
        return Err(ReadOutcome::TooLarge);
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .map_err(|e| malformed(format!("reading the body: {:?}", e.kind())))?;
    Ok(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn post(server: &MockServer, body: &str) -> String {
        let addr = server.addr;
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        // Read the head and exactly Content-Length body bytes, as an HTTP client does
        // (waiting for EOF can end in a reset on Windows loopback).
        let mut r = BufReader::new(s);
        let mut head = String::new();
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            head.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).unwrap();
        head + &String::from_utf8(body).unwrap()
    }

    #[test]
    fn mock_responder_answers_from_the_request_and_records_it() {
        let server = MockServer::start_with(|req| {
            let n = req.json()["n"].as_u64().unwrap_or(0);
            MockResponse::ok(format!("{{\"twice\":{}}}", n * 2))
        })
        .unwrap();
        let first = post(&server, r#"{"n":3}"#);
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(first.ends_with(r#"{"twice":6}"#), "{first}");
        let second = post(&server, r#"{"n":5}"#);
        assert!(second.ends_with(r#"{"twice":10}"#), "{second}");
        let got = server.requests();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].path, "/v1/chat/completions");
        assert_eq!(got[1].json()["n"], 5);
        assert_eq!(server.unscripted(), 0);
        let conns = server.connections();
        assert_eq!(conns.len(), 2, "{}", server.diagnostics());
        for c in settled(&server, 2) {
            assert_eq!(c.outcome, ReadOutcome::Request, "{c}");
            let close = c.close.as_ref().unwrap();
            assert!(close.response_complete(), "{c}");
        }
    }

    /// The first `n` connections once each one's lingering close has finished.
    fn settled(server: &MockServer, n: usize) -> Vec<ConnectionLog> {
        let deadline = std::time::Instant::now() + tcp::LINGER_TIMEOUT * 3;
        loop {
            let conns = server.connections();
            if conns.len() >= n && conns.iter().all(|c| c.close.is_some()) {
                return conns;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{}",
                server.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Sends `bytes` and reads the whole reply until the server closes. The write side is
    /// left open, as HTTP clients do (a filtering proxy on loopback, with localhost
    /// filtering on, was seen to drop the reply after a client half-close).
    fn exchange(server: &MockServer, bytes: &[u8]) -> String {
        let mut s = TcpStream::connect(server.addr).unwrap();
        s.write_all(bytes).unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).unwrap_or_else(|e| {
            panic!("reply lost ({e}); {}", server.diagnostics());
        });
        String::from_utf8_lossy(&got).into_owned()
    }

    #[test]
    fn mock_malformed_request_gets_a_400_not_a_silent_drop() {
        let server = MockServer::start(vec![MockResponse::ok("{}")]).unwrap();
        let reply = exchange(&server, b"garbage\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 400 "), "{reply}");
        assert!(reply.contains("Connection: close\r\n"), "{reply}");
        assert!(server.requests().is_empty());
        assert_eq!(
            server.remaining(),
            1,
            "a bad request uses no scripted response"
        );
        let conns = settled(&server, 1);
        assert!(
            matches!(conns[0].outcome, ReadOutcome::Malformed(_)),
            "{}",
            server.diagnostics()
        );
        assert_eq!(conns[0].status, Some(400));
        assert!(conns[0].close.as_ref().unwrap().response_complete());
    }

    #[test]
    fn mock_logs_the_connection_while_a_delayed_reply_waits() {
        let delay = Duration::from_secs(3);
        let server = MockServer::start(vec![MockResponse::ok("{}").delayed(delay)]).unwrap();
        let addr = server.addr;
        let start = std::time::Instant::now();
        let client = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"POST /v1/x HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
            let mut got = Vec::new();
            s.read_to_end(&mut got).unwrap();
            String::from_utf8_lossy(&got).into_owned()
        });
        // The connection is visible before the scripted delay has passed (the reply can
        // only be written after it), so a test failing during the delay sees it.
        let during = loop {
            if let Some(c) = server.connections().first() {
                break c.clone();
            }
            assert!(
                start.elapsed() < delay,
                "not logged during the delay; {}",
                server.diagnostics()
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            start.elapsed() < delay,
            "logged only after the delay: {during}"
        );
        assert_eq!(during.outcome, ReadOutcome::Request, "{during}");
        assert!(during.close.is_none(), "{during}");
        let reply = client.join().unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        let conns = settled(&server, 1);
        assert_eq!(conns[0].status, Some(200), "{}", server.diagnostics());
    }

    #[test]
    fn mock_oversized_request_gets_a_413() {
        let server = MockServer::start(vec![MockResponse::ok("{}")]).unwrap();
        let head = format!(
            "POST /v1/x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST + 1
        );
        let reply = exchange(&server, head.as_bytes());
        assert!(reply.starts_with("HTTP/1.1 413 "), "{reply}");
        assert!(server.requests().is_empty());
        let conns = settled(&server, 1);
        assert_eq!(
            conns[0].outcome,
            ReadOutcome::TooLarge,
            "{}",
            server.diagnostics()
        );
        assert_eq!(conns[0].status, Some(413));
    }
}
