//! The HTTP/1.1 framing the protocol allows: one request per connection, `Connection: close`,
//! `Content-Length` bodies and never a chunked body - the mirror of the client in PhotoCraft
//! (`crates/ai-client/src/http.rs`).
//!
//! Nothing here trusts the client: an over-long line, a missing or dishonest `Content-Length`, a
//! chunked transfer, a non-UTF-8 header or a body that stops short is an error with a plain
//! sentence, never a panic.

use std::io::{ErrorKind, Read, Write};

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

/// Read the request line and headers. The caller reads the body once it knows the length.
pub fn read_head<R: Read>(reader: &mut R) -> Result<Head> {
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
        match reader.read(&mut chunk) {
            Ok(0) => return Err(Fail::bad_request("The connection closed before the request headers were complete.")),
            Ok(read) => buffer.extend_from_slice(chunk.get(..read).unwrap_or(&[])),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Err(Fail::bad_request("The request headers took too long to arrive.")),
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

/// Read exactly `length` body bytes, with the bytes that came with the headers already counted.
///
/// The cap is checked before anything is read, so an honest `Content-Length` over the limit is a
/// 413 without waiting for a gigabyte that never arrives.
pub fn read_body<R: Read>(reader: &mut R, leftover: &[u8], length: u64) -> Result<Vec<u8>> {
    if length > MAX_BODY_BYTES {
        return Err(Fail::too_large("The request body is larger than the 1 GiB the protocol allows."));
    }
    let Ok(want) = usize::try_from(length) else {
        return Err(Fail::too_large("The request body is larger than the 1 GiB the protocol allows."));
    };
    let mut body: Vec<u8> = Vec::with_capacity(want.min(64 * 1024));
    body.extend_from_slice(leftover);
    let mut chunk = vec![0u8; 64 * 1024];
    while body.len() < want {
        match reader.read(&mut chunk) {
            Ok(0) => return Err(Fail::bad_request("The request stopped before its body was complete.")),
            Ok(read) => body.extend_from_slice(chunk.get(..read).unwrap_or(&[])),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Err(Fail::bad_request("The request body took too long to arrive.")),
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
    let mut out = Vec::with_capacity(response.body.len().saturating_add(128));
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", response.status.code(), response.status.reason()).as_bytes());
    if let Some(content_type) = response.content_type {
        out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\nConnection: close\r\n\r\n", response.body.len()).as_bytes());
    out.extend_from_slice(response.body);
    writer.write_all(&out)?;
    writer.flush().map_err(Fail::from)
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
    use super::{Head, Header, MAX_HEADER_LINE_BYTES, Response, Status, error_body, find, read_body, read_head, write_response};
    use std::io::Cursor;

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
        let parsed = read_head(&mut Cursor::new(raw)).expect("a well formed head parses");
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/v1/health");
        assert_eq!(parsed.header("authorization"), Some("Bearer abc"));
        assert_eq!(parsed.header("host"), Some("127.0.0.1:9"));
        assert_eq!(parsed.content_length().expect("one content length"), Some(0));
    }

    #[test]
    fn a_garbage_request_line_is_a_bad_request() {
        assert!(read_head(&mut Cursor::new(b"not a request\r\n\r\n")).is_err());
        assert!(read_head(&mut Cursor::new(b"GET /v1/health HTTP/2.0\r\n\r\n")).is_err());
        assert!(read_head(&mut Cursor::new(b"")).is_err());
    }

    #[test]
    fn one_header_line_over_the_cap_is_caught_before_it_is_parsed() {
        let mut raw = b"GET /v1/health HTTP/1.1\r\nX-Long: ".to_vec();
        raw.extend_from_slice(&vec![b'a'; MAX_HEADER_LINE_BYTES.saturating_add(16)]);
        raw.extend_from_slice(b"\r\n\r\n");
        let fail = read_head(&mut Cursor::new(raw)).expect_err("an over-long header line is refused");
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
        let body = read_body(&mut stream, b"lo", 2).expect("the body arrives");
        assert_eq!(body, b"lo");
        assert_eq!(stream.position(), 0, "bytes past the body are left in the socket");
        let short = read_body(&mut Cursor::new(b"ab"), b"", 8).expect_err("a short body is bad input");
        assert!(short.sentence.contains("stopped before"), "{short}");
    }

    #[test]
    fn a_honest_length_over_the_cap_is_413_before_anything_is_read() {
        let fail = read_body(&mut Cursor::new(b""), b"", 1024 * 1024 * 1024 + 1).expect_err("over the cap is refused");
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
    fn find_skips_bytes_before_the_start() {
        assert_eq!(find(b"ab\r\ncd", 0, b"\r\n"), Some(2));
        assert_eq!(find(b"ab\r\ncd", 3, b"\r\n"), None);
    }
}
