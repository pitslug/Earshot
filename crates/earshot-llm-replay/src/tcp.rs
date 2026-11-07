//! Closing a test server's TCP connection without racing the response.
//!
//! Dropping a socket right after writing can reset the connection instead of closing it:
//! on Windows `closesocket` sends an RST when unread bytes sit in the receive buffer, and
//! the client may then lose response bytes it has not read yet (`os error 10054`). A
//! lingering close sends FIN after the response (`shutdown(Write)`), then reads until the
//! client closes its side, so the socket is dropped with an empty receive buffer and the
//! client has already seen the whole response.
//!
//! Used by the mock endpoint and the xtask download tests; `earshot-net`'s own tests keep
//! a copy because this crate depends on `earshot-net`.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

/// How long a lingering close waits for the client to close its side.
pub const LINGER_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one write of the response may block (a client that never reads must not hold
/// the server thread forever).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How the drain after `shutdown(Write)` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainEnd {
    /// The client closed its side (read returned 0).
    Eof,
    /// The client sent nothing more and did not close within [`LINGER_TIMEOUT`].
    TimedOut,
    /// A read failed (for example a reset by the client).
    Error(io::ErrorKind),
}

/// What a [`lingering_close`] did, for diagnostics in a failing test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReport {
    /// Response bytes handed to the socket.
    pub response_bytes: usize,
    /// Result of writing and flushing the response (`WouldBlock` or `TimedOut` when the
    /// client stopped reading for longer than the write timeout).
    pub write: Result<(), io::ErrorKind>,
    /// Result of `shutdown(Shutdown::Write)`.
    pub shutdown: Result<(), io::ErrorKind>,
    /// Bytes the client sent after the response (read and discarded).
    pub drained_bytes: usize,
    /// How the drain ended.
    pub drain_end: DrainEnd,
}

impl CloseReport {
    /// True when the whole response was written and the write side shut down cleanly.
    pub fn response_complete(&self) -> bool {
        self.write.is_ok() && self.shutdown.is_ok()
    }
}

impl fmt::Display for CloseReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "response {} bytes, write {:?}, shutdown {:?}, drained {} bytes, drain end {:?}",
            self.response_bytes, self.write, self.shutdown, self.drained_bytes, self.drain_end
        )
    }
}

/// Writes `response` (each write blocking at most [`WRITE_TIMEOUT`]), flushes, shuts down
/// the write side, then reads and discards until the client closes, a read fails or
/// [`LINGER_TIMEOUT`] passes, and drops the socket.
pub fn lingering_close(mut stream: TcpStream, response: &[u8]) -> CloseReport {
    close_with(&mut stream, response, WRITE_TIMEOUT, LINGER_TIMEOUT)
}

/// [`lingering_close`] with `timeout` as both the write and the drain timeout.
pub fn lingering_close_within(
    mut stream: TcpStream,
    response: &[u8],
    timeout: Duration,
) -> CloseReport {
    close_with(&mut stream, response, timeout, timeout)
}

/// The close without the final drop, so tests can inspect the socket afterwards.
fn close_with(
    stream: &mut TcpStream,
    response: &[u8],
    write_timeout: Duration,
    drain_timeout: Duration,
) -> CloseReport {
    let write = stream
        .set_write_timeout(Some(write_timeout))
        .and_then(|()| stream.write_all(response))
        .and_then(|()| stream.flush())
        .map_err(|e| e.kind());
    // After a failed write the client may never read: shut down and drain anyway, so the
    // socket still ends with FIN rather than an RST where possible.
    let shutdown = stream.shutdown(Shutdown::Write).map_err(|e| e.kind());
    let (drained_bytes, drain_end) = drain(stream, drain_timeout);
    CloseReport {
        response_bytes: response.len(),
        write,
        shutdown,
        drained_bytes,
        drain_end,
    }
}

fn drain(stream: &mut TcpStream, timeout: Duration) -> (usize, DrainEnd) {
    let deadline = Instant::now() + timeout;
    let mut total = 0usize;
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return (total, DrainEnd::TimedOut);
        }
        if let Err(e) = stream.set_read_timeout(Some(left)) {
            return (total, DrainEnd::Error(e.kind()));
        }
        match stream.read(&mut buf) {
            Ok(0) => return (total, DrainEnd::Eof),
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return (total, DrainEnd::TimedOut);
            }
            Err(e) => return (total, DrainEnd::Error(e.kind())),
        }
    }
}

/// A complete `Connection: close` response with a plain-text body, for refusing a request
/// the server could not read (`400`) or would not accept (`413`).
pub fn error_response(status: u16, reason: &str, message: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
        message.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use std::thread;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (server, _) = l.accept().unwrap();
        (client, server)
    }

    #[test]
    fn tcp_lingering_close_delivers_the_response_and_waits_for_the_client() {
        let (mut client, server) = pair();
        // Unread request bytes in the server's receive buffer: a plain drop would reset.
        client.write_all(b"unread request bytes").unwrap();
        let body = vec![b'x'; 256 * 1024];
        let expected = body.clone();
        let t = thread::spawn(move || lingering_close(server, &body));
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got, expected, "whole response then EOF");
        drop(client);
        let report = t.join().unwrap();
        assert!(report.response_complete(), "{report}");
        assert_eq!(report.response_bytes, expected.len());
        assert_eq!(report.drain_end, DrainEnd::Eof, "{report}");
        assert_eq!(
            report.drained_bytes,
            b"unread request bytes".len(),
            "{report}"
        );
    }

    #[test]
    fn tcp_lingering_close_gives_up_when_the_client_never_closes() {
        let (mut client, server) = pair();
        let report = lingering_close_within(server, b"hello", Duration::from_millis(200));
        assert!(report.response_complete(), "{report}");
        assert_eq!(report.drain_end, DrainEnd::TimedOut, "{report}");
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"hello");
    }

    /// The server socket's write timeout after `close_with(write, drain)`. (A write that
    /// blocks on a client that never reads cannot be provoked portably: on the Windows
    /// desktop a single 256 MiB loopback write completed with no reader at all.)
    fn write_timeout_after(write: Duration, drain: Duration) -> Option<Duration> {
        let (mut client, mut server) = pair();
        let reader = thread::spawn(move || {
            let mut got = Vec::new();
            client.read_to_end(&mut got).unwrap();
            got
        });
        let report = close_with(&mut server, b"hi", write, drain);
        assert!(report.response_complete(), "{report}");
        assert_eq!(reader.join().unwrap(), b"hi");
        server.write_timeout().unwrap()
    }

    #[test]
    fn tcp_lingering_close_bounds_every_write() {
        assert_eq!(
            write_timeout_after(WRITE_TIMEOUT, LINGER_TIMEOUT),
            Some(WRITE_TIMEOUT)
        );
        let t = Duration::from_millis(300);
        assert_eq!(write_timeout_after(t, t), Some(t));
    }
    #[test]
    fn tcp_error_response_is_a_complete_close_delimited_reply() {
        let r = String::from_utf8(error_response(413, "Payload Too Large", "too big")).unwrap();
        assert_eq!(
            r,
            "HTTP/1.1 413 Payload Too Large\r\nContent-Type: text/plain\r\nContent-Length: 7\r\nConnection: close\r\n\r\ntoo big"
        );
    }
}
