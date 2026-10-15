//! The HTTP/1.1 framing the protocol allows: one request per connection, `Connection: close`,
//! `Content-Length` bodies and never a chunked body - the mirror of the client in PhotoCraft
//! (`crates/ai-client/src/http.rs`).
//!
//! Nothing here trusts the client: an over-long line, a missing or dishonest `Content-Length`, a
//! chunked transfer, a non-UTF-8 header or a body that stops short is an error with a plain
//! sentence, never a panic.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Fail, Result, Status};

/// Amendment 11: the whole request head is bounded, like the client bounds the reply.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;
/// One header line, and the request line, at most 8 KiB.
pub const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
/// More headers than this is a bad request, not a bigger array.
pub const MAX_HEADERS: usize = 64;
/// Amendment 11: request bodies at most 1 GiB.
pub const MAX_BODY_BYTES: u64 = 1024 * 1024 * 1024;
/// A path longer than this is not one of our endpoints.
const MAX_PATH_BYTES: usize = 4 * 1024;
/// C1: a body earns this much extra time for every [BODY_STEP_BYTES] it is long...
pub const BODY_PER_STEP: Duration = Duration::from_secs(1);
/// ...which is a slow disk's worth of loopback bytes.
pub const BODY_STEP_BYTES: u64 = 8 * 1024 * 1024;
/// ...and the whole body budget never goes past this, whatever the length says.
pub const BODY_TIMEOUT_CAP: Duration = Duration::from_secs(180);

/// Windows does not reliably interrupt a blocking read on another socket clone with shutdown.
/// Poll cancellation while preserving the caller's full per-operation timeout.
pub(crate) struct Cancellable<'a> {
    socket: &'a mut TcpStream,
    stop: &'a AtomicBool,
    read_wait: Duration,
    write_wait: Duration,
}

impl<'a> Cancellable<'a> {
    pub(crate) fn new(socket: &'a mut TcpStream, stop: &'a AtomicBool) -> Self {
        Self { socket, stop, read_wait: BODY_TIMEOUT_CAP, write_wait: BODY_TIMEOUT_CAP }
    }

    fn remaining(&self, until: Instant) -> std::io::Result<Duration> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(ErrorKind::ConnectionAborted, "Craft AI Manager is stopping."));
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(ErrorKind::TimedOut, "The socket operation timed out."));
        }
        Ok(left.min(Duration::from_millis(100)))
    }
}

impl Read for Cancellable<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let until = Instant::now().checked_add(self.read_wait).unwrap_or_else(Instant::now);
        loop {
            self.socket.set_read_timeout(Some(self.remaining(until)?))?;
            match self.socket.read(bytes) {
                Err(error) if timed_out(&error) || error.kind() == ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}

impl Write for Cancellable<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let until = Instant::now().checked_add(self.write_wait).unwrap_or_else(Instant::now);
        loop {
            self.socket.set_write_timeout(Some(self.remaining(until)?))?;
            match self.socket.write(bytes.get(..bytes.len().min(64 * 1024)).unwrap_or_default()) {
                Err(error) if timed_out(&error) || error.kind() == ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}

impl Timed for Cancellable<'_> {
    fn wait_at_most(&mut self, limit: Duration) -> std::io::Result<()> {
        self.read_wait = limit;
        Ok(())
    }
}

impl TimedWrite for Cancellable<'_> {
    fn write_wait_at_most(&mut self, limit: Duration) -> std::io::Result<()> {
        self.write_wait = limit;
        Ok(())
    }
}

/// A reader whose next read can be told how long it may wait: the socket in the server, a byte
/// slice in the tests.
pub trait Timed: Read {
    /// Let the next read wait at most `limit`.
    fn wait_at_most(&mut self, limit: Duration) -> std::io::Result<()>;
}

impl Timed for TcpStream {
    fn wait_at_most(&mut self, limit: Duration) -> std::io::Result<()> {
        // A zero timeout means "wait for ever" to the socket, so the smallest wait is 1 ms.
        self.set_read_timeout(Some(limit.max(Duration::from_millis(1))))
    }
}

impl<T: AsRef<[u8]>> Timed for std::io::Cursor<T> {
    fn wait_at_most(&mut self, _limit: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

/// One deadline across many reads (C1): the per-read timeout alone let a client that sends a byte
/// every 20 s hold a connection for ever.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    /// When the whole head, or the whole body, must have arrived.
    pub until: Instant,
    /// The longest any single read may wait (amendment 11: 30 s).
    pub per_read: Duration,
}

impl Clock {
    /// A deadline `budget` from now.
    #[must_use]
    pub fn starting_now(budget: Duration, per_read: Duration) -> Clock {
        let now = Instant::now();
        Clock { until: now.checked_add(budget).unwrap_or(now), per_read }
    }

    /// How long the next read may wait, or `None` once the deadline has passed.
    #[must_use]
    pub fn next_wait(&self) -> Option<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() { None } else { Some(left.min(self.per_read)) }
    }

    /// Has the deadline passed?
    #[must_use]
    pub fn expired(&self) -> bool {
        self.next_wait().is_none()
    }
}

/// The time a body of `length` bytes gets: `base`, plus [BODY_PER_STEP] for every whole
/// [BODY_STEP_BYTES], never more than [BODY_TIMEOUT_CAP] (or `base`, if that is bigger).
#[must_use]
pub fn body_budget(base: Duration, length: u64) -> Duration {
    let steps = u32::try_from(length / BODY_STEP_BYTES).unwrap_or(u32::MAX);
    base.saturating_add(BODY_PER_STEP.saturating_mul(steps)).min(BODY_TIMEOUT_CAP.max(base))
}

/// Was this read error the socket giving up waiting? Linux says `WouldBlock`, Windows `TimedOut`.
fn timed_out(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// One request header, with the name lowercased so lookups are case-insensitive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// The request line and headers of one request, plus the body bytes that arrived with them.
#[derive(Clone, Debug)]
pub struct Head {
    pub method: String,
    pub path: String,
    pub headers: Vec<Header>,
    pub leftover: Vec<u8>,
}

impl Head {
    /// The first value of a header, if the request has it.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|header| header.name == name).map(|header| header.value.as_str())
    }

    /// The `Content-Length` the body must have.
    ///
    /// Amendment 11 forbids chunked bodies on either side, and two `Content-Length` headers are a
    /// request we cannot answer honestly, so both are a bad request rather than a guess.
    pub fn content_length(&self) -> Result<Option<u64>> {
        let mut found: Option<u64> = None;
        for header in &self.headers {
            match header.name.as_str() {
                "transfer-encoding" => return Err(Fail::bad_request("Craft AI Manager does not accept a chunked request body.")),
                "content-length" => {
                    let Ok(value) = header.value.parse::<u64>() else {
                        return Err(Fail::bad_request("The Content-Length header is not a number."));
                    };
                    if found.is_some() {
                        return Err(Fail::bad_request("The request has more than one Content-Length header."));
                    }
                    found = Some(value);
                }
                _ => {}
            }
        }
        Ok(found)
    }
}

/// A reply: the status, the content type (none for a 204), and the body.
pub struct Response<'a> {
    pub status: Status,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
}

/// Read the request line and headers, all of them before `clock` runs out. The caller reads the
/// body once it knows the length.
pub fn read_head<R: Timed>(reader: &mut R, clock: Clock) -> Result<Head> {
    let mut buffer: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    let head_end = loop {
        if let Some(at) = find(&buffer, 0, b"\r\n\r\n") {
            break at;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(Fail::bad_request("The request headers are larger than the 64 KiB the protocol allows."));
        }
        if open_line_len(&buffer) > MAX_HEADER_LINE_BYTES + 2 {
            return Err(Fail::bad_request("One request header line is longer than the 8 KiB the protocol allows."));
        }
        let Some(wait) = clock.next_wait() else {
            return Err(Fail::bad_request(HEAD_TOO_SLOW));
        };
        reader.wait_at_most(wait)?;
        match reader.read(&mut chunk) {
            Ok(0) => return Err(Fail::bad_request("The connection closed before the request headers were complete.")),
            Ok(read) => buffer.extend_from_slice(chunk.get(..read).unwrap_or(&[])),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if timed_out(&error) => return Err(Fail::bad_request(HEAD_TOO_SLOW)),
            Err(error) => return Err(error.into()),
        }
    };

    if longest_line(buffer.get(..head_end).unwrap_or(&[])) > MAX_HEADER_LINE_BYTES + 2 {
        return Err(Fail::bad_request("One request header line is longer than the 8 KiB the protocol allows."));
    }
    let mut slot = [httparse::EMPTY_HEADER; MAX_HEADERS + 1];
    let mut parsed = httparse::Request::new(&mut slot);
    match parsed.parse(&buffer) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(Fail::bad_request("The request headers are not complete.")),
        Err(httparse::Error::TooManyHeaders) => return Err(Fail::bad_request("The request has more headers than the protocol allows.")),
        Err(_) => return Err(Fail::bad_request("The request line is not something Craft AI Manager can read.")),
    }
    if parsed.version != Some(1) {
        return Err(Fail::bad_request("Craft AI Manager speaks HTTP/1.1 only."));
    }
    let method = parsed.method.unwrap_or_default();
    let path = parsed.path.unwrap_or_default();
    if method.is_empty() || !method.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(Fail::bad_request("Craft AI Manager does not understand that request method."));
    }
    if path.is_empty() || path.len() > MAX_PATH_BYTES || !path.bytes().all(|byte| !byte.is_ascii_control()) {
        return Err(Fail::bad_request("The request path is not something Craft AI Manager can read."));
    }
    let mut headers: Vec<Header> = Vec::with_capacity(parsed.headers.len());
    for header in parsed.headers {
        let Ok(value) = std::str::from_utf8(header.value) else {
            return Err(Fail::bad_request("A request header is not text."));
        };
        headers.push(Header { name: header.name.to_ascii_lowercase(), value: value.trim().to_string() });
    }
    Ok(Head { method: method.to_string(), path: path.to_string(), headers, leftover: buffer.get(head_end.saturating_add(4)..).unwrap_or(&[]).to_vec() })
}

/// The sentence for a head that did not arrive in time.
const HEAD_TOO_SLOW: &str = "The request headers took too long to arrive.";
/// The sentence for a body that did not arrive in time.
const BODY_TOO_SLOW: &str = "The request body took too long to arrive.";

/// Read exactly `length` body bytes, with the bytes that came with the headers already counted, all
/// of them before `clock` runs out.
///
/// The cap is checked before anything is read, so an honest `Content-Length` over the limit is a
/// 413 without waiting for a gigabyte that never arrives.
pub fn read_body<R: Timed>(reader: &mut R, leftover: &[u8], length: u64, clock: Clock) -> Result<Vec<u8>> {
    if length > MAX_BODY_BYTES {
        return Err(Fail::too_large("The request body is larger than the 1 GiB the protocol allows."));
    }
    let Ok(want) = usize::try_from(length) else {
        return Err(Fail::too_large("The request body is larger than the 1 GiB the protocol allows."));
    };
    if leftover.len() > want {
        return Err(Fail::bad_request("The request body is longer than its Content-Length says."));
    }
    let mut body: Vec<u8> = Vec::new();
    body.try_reserve_exact(want).map_err(|_| Fail::busy("Craft AI Manager cannot allocate this request body right now."))?;
    body.extend_from_slice(leftover);
    let mut chunk = vec![0u8; 64 * 1024];
    while body.len() < want {
        let Some(wait) = clock.next_wait() else {
            return Err(Fail::bad_request(BODY_TOO_SLOW));
        };
        reader.wait_at_most(wait)?;
        let remaining = want.saturating_sub(body.len()).min(chunk.len());
        match reader.read(chunk.get_mut(..remaining).unwrap_or(&mut [])) {
            Ok(0) => return Err(Fail::bad_request("The request stopped before its body was complete.")),
            Ok(read) => body.extend_from_slice(chunk.get(..read).unwrap_or(&[])),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if timed_out(&error) => return Err(Fail::bad_request(BODY_TOO_SLOW)),
            Err(error) => return Err(error.into()),
        }
    }
    if body.len() > want {
        return Err(Fail::bad_request("The request body is longer than its Content-Length says."));
    }
    Ok(body)
}

/// Write one reply: the status line, the two headers the protocol promises, and the body.
pub fn write_response<W: Write>(writer: &mut W, response: &Response<'_>) -> Result<()> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", response.status.code(), response.status.reason()).as_bytes());
    if let Some(content_type) = response.content_type {
        out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\nConnection: close\r\n\r\n", response.body.len()).as_bytes());
    writer.write_all(&out)?;
    writer.write_all(response.body)?;
    writer.flush().map_err(Fail::from)
}

/// Every write shares one deadline, so a slow reader cannot renew its timeout forever.
pub(crate) fn write_socket_response(stream: &mut TcpStream, response: &Response<'_>, budget: Duration, stop: &AtomicBool) -> Result<()> {
    write_deadlined_response(&mut Cancellable::new(stream, stop), response, budget)
}

trait TimedWrite: Write {
    fn write_wait_at_most(&mut self, limit: Duration) -> std::io::Result<()>;
}

impl TimedWrite for TcpStream {
    fn write_wait_at_most(&mut self, limit: Duration) -> std::io::Result<()> {
        self.set_write_timeout(Some(limit))
    }
}

fn write_deadlined_response<W: TimedWrite>(stream: &mut W, response: &Response<'_>, budget: Duration) -> Result<()> {
    struct DeadlineWriter<'a, W> {
        stream: &'a mut W,
        clock: Clock,
    }
    impl<W: TimedWrite> Write for DeadlineWriter<'_, W> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let wait = self.clock.next_wait().ok_or_else(|| std::io::Error::new(ErrorKind::TimedOut, "The response write deadline expired."))?;
            self.stream.write_wait_at_most(wait)?;
            self.stream.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.clock.expired() {
                return Err(std::io::Error::new(ErrorKind::TimedOut, "The response write deadline expired."));
            }
            self.stream.flush()
        }
    }
    write_response(&mut DeadlineWriter { stream, clock: Clock::starting_now(budget, budget) }, response)
}

/// The `{"error": "<plain sentence>"}` body amendment 11 defines.
#[must_use]
pub fn error_body(sentence: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "error": sentence })).unwrap_or_else(|_| br#"{"error": "Craft AI Manager could not read that request."}"#.to_vec())
}

/// Answer with an error, which is how every failure the protocol describes reaches the client.
pub fn write_error<W: Write>(writer: &mut W, status: Status, sentence: &str) -> Result<()> {
    let body = error_body(sentence);
    write_response(writer, &Response { status, content_type: Some("application/json"), body: &body })
}

/// Where `needle` first appears in `haystack` at or after `from`.
#[must_use]
pub fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let tail = haystack.get(from..)?;
    tail.windows(needle.len()).position(|window| window == needle).map(|at| at.saturating_add(from))
}

/// How long the last, still unfinished line in the buffer is.
#[must_use]
fn open_line_len(buffer: &[u8]) -> usize {
    let line_start = buffer.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at.saturating_add(1));
    buffer.len().saturating_sub(line_start)
}

/// The longest line in a finished head, counting the line break.
#[must_use]
fn longest_line(head: &[u8]) -> usize {
    let mut longest = 0usize;
    let mut start = 0usize;
    for (at, byte) in head.iter().enumerate() {
        if *byte == b'\n' {
            longest = longest.max(at.saturating_add(1).saturating_sub(start));
            start = at.saturating_add(1);
        }
    }
    longest.max(head.len().saturating_sub(start))
}

#[cfg(test)]
mod tests {
    #[test]
    fn partial_writes_cannot_renew_the_response_deadline() {
        use std::{
            io::Write,
            time::{Duration, Instant},
        };
        #[derive(Default)]
        struct SlowWriter {
            waits: Vec<Duration>,
            bytes: usize,
        }
        impl Write for SlowWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(25));
                let n = bytes.len().min(1);
                self.bytes += n;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl super::TimedWrite for SlowWriter {
            fn write_wait_at_most(&mut self, wait: Duration) -> std::io::Result<()> {
                self.waits.push(wait);
                Ok(())
            }
        }
        let mut writer = SlowWriter::default();
        let response = super::Response { status: super::Status::Ok, content_type: None, body: &[7; 1024] };
        let start = Instant::now();
        assert!(super::write_deadlined_response(&mut writer, &response, Duration::from_millis(100)).is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(writer.bytes < 1024);
        assert!(writer.waits.windows(2).all(|w| w[1] < w[0]), "one shared deadline, not a fresh timeout per write");
    }

    #[test]
    fn an_expired_response_deadline_stops_even_a_writable_socket() {
        use std::{
            net::{TcpListener, TcpStream},
            time::Duration,
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let _client = TcpStream::connect(listener.local_addr().expect("address")).expect("client");
        let (mut socket, _) = listener.accept().expect("socket");
        let response = super::Response { status: super::Status::Ok, content_type: None, body: b"result" };
        assert!(super::write_socket_response(&mut socket, &response, Duration::ZERO, &std::sync::atomic::AtomicBool::new(false)).is_err());
    }

    use super::{
        BODY_STEP_BYTES, BODY_TIMEOUT_CAP, Clock, Head, Header, MAX_HEADER_LINE_BYTES, Response, Status, body_budget, error_body, find, read_body, read_head,
        write_response,
    };
    use std::io::Cursor;
    use std::time::{Duration, Instant};

    /// A clock that will not run out during a test.
    fn later() -> Clock {
        Clock::starting_now(Duration::from_secs(60), Duration::from_secs(30))
    }

    fn head(pairs: &[(&str, &str)]) -> Head {
        Head {
            method: "POST".to_string(),
            path: "/v1/jobs".to_string(),
            headers: pairs.iter().map(|(name, value)| Header { name: (*name).to_string(), value: (*value).to_string() }).collect(),
            leftover: Vec::new(),
        }
    }

    #[test]
    fn a_request_head_is_parsed_with_its_headers_lowercased() {
        let raw = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:9\r\nAuthorization: Bearer abc\r\nContent-Length: 0\r\n\r\n";
        let parsed = read_head(&mut Cursor::new(raw), later()).expect("a well formed head parses");
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/v1/health");
        assert_eq!(parsed.header("authorization"), Some("Bearer abc"));
        assert_eq!(parsed.header("host"), Some("127.0.0.1:9"));
        assert_eq!(parsed.content_length().expect("one content length"), Some(0));
    }

    #[test]
    fn a_garbage_request_line_is_a_bad_request() {
        assert!(read_head(&mut Cursor::new(b"not a request\r\n\r\n"), later()).is_err());
        assert!(read_head(&mut Cursor::new(b"GET /v1/health HTTP/2.0\r\n\r\n"), later()).is_err());
        assert!(read_head(&mut Cursor::new(b""), later()).is_err());
    }

    #[test]
    fn one_header_line_over_the_cap_is_caught_before_it_is_parsed() {
        let mut raw = b"GET /v1/health HTTP/1.1\r\nX-Long: ".to_vec();
        raw.extend_from_slice(&vec![b'a'; MAX_HEADER_LINE_BYTES.saturating_add(16)]);
        raw.extend_from_slice(b"\r\n\r\n");
        let fail = read_head(&mut Cursor::new(raw), later()).expect_err("an over-long header line is refused");
        assert!(fail.sentence.contains("8 KiB"), "the sentence says why: {fail}");
    }

    #[test]
    fn chunked_and_double_content_lengths_are_refused() {
        assert!(head(&[("transfer-encoding", "chunked")]).content_length().is_err());
        assert!(head(&[("content-length", "4"), ("content-length", "4")]).content_length().is_err());
        assert!(head(&[("content-length", "four")]).content_length().is_err());
        assert_eq!(head(&[]).content_length().expect("no content length is fine"), None);
    }

    #[test]
    fn a_body_is_read_to_its_length_and_no_further() {
        let mut stream = Cursor::new(b"remoter junk");
        let body = read_body(&mut stream, b"lo", 2, later()).expect("the body arrives");
        assert_eq!(body, b"lo");
        assert_eq!(stream.position(), 0, "bytes past the body are left in the socket");
        let short = read_body(&mut Cursor::new(b"ab"), b"", 8, later()).expect_err("a short body is bad input");
        assert!(short.sentence.contains("stopped before"), "{short}");
    }

    #[test]
    fn a_honest_length_over_the_cap_is_413_before_anything_is_read() {
        let fail = read_body(&mut Cursor::new(b""), b"", 1024 * 1024 * 1024 + 1, later()).expect_err("over the cap is refused");
        assert_eq!(fail.status, Status::TooLarge);
    }

    #[test]
    fn every_reply_carries_a_content_length_and_promises_to_close() {
        let mut out = Vec::new();
        write_response(&mut out, &Response { status: Status::NotFound, content_type: Some("application/json"), body: b"{\"error\": \"no\"}" })
            .expect("writing works");
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"), "{text}");
        assert!(text.contains("Content-Length: 15\r\n"), "{text}");
        assert!(text.contains("Connection: close\r\n"), "{text}");
    }

    #[test]
    fn the_error_body_is_the_shape_the_protocol_asks_for() {
        let value: serde_json::Value = serde_json::from_slice(&error_body("Nothing here.")).expect("the body is JSON");
        assert_eq!(value.get("error").and_then(serde_json::Value::as_str), Some("Nothing here."));
    }

    #[test]
    fn a_clock_that_has_run_out_stops_the_read() {
        let gone = Clock { until: Instant::now(), per_read: Duration::from_secs(30) };
        let fail = read_head(&mut Cursor::new(b"GET /v1/health HTTP/1.1\r\n\r\n"), gone).expect_err("no time is left");
        assert!(fail.sentence.contains("too long"), "{fail}");
        let fail = read_body(&mut Cursor::new(b"abcd"), b"", 4, gone).expect_err("no time is left");
        assert!(fail.sentence.contains("too long"), "{fail}");
        assert!(gone.expired());
        assert_eq!(later().next_wait(), Some(Duration::from_secs(30)), "a single read still waits at most the per-read timeout");
    }

    #[test]
    fn a_body_gets_a_second_more_for_every_8_mib_up_to_the_cap() {
        let base = Duration::from_secs(30);
        assert_eq!(body_budget(base, 0), base);
        assert_eq!(body_budget(base, BODY_STEP_BYTES - 1), base, "less than 8 MiB earns nothing extra");
        assert_eq!(body_budget(base, 4 * BODY_STEP_BYTES), base + Duration::from_secs(4));
        assert_eq!(body_budget(base, 1024 * 1024 * 1024), Duration::from_secs(158), "a whole GiB gets 30 s plus 128 s");
        assert_eq!(body_budget(base, u64::MAX), BODY_TIMEOUT_CAP, "and a lie about the length cannot buy more than the cap");
    }

    #[test]
    fn find_skips_bytes_before_the_start() {
        assert_eq!(find(b"ab\r\ncd", 0, b"\r\n"), Some(2));
        assert_eq!(find(b"ab\r\ncd", 3, b"\r\n"), None);
    }
}
