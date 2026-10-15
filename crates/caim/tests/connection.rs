// What happens on the socket itself (BF-01): an early error reply still reaches a client that is
// busy writing its body (X2), and a client that trickles bytes cannot hold a connection slot for
// ever (C1). Real TCP clients against a real server, with short deadlines so the tests are quick.

mod support;

use craft_ai_manager::server::ServeOptions;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

/// A body big enough that the kernel buffers cannot hide an unread one: the reviewer saw the
/// client's write fail after about 4 MB.
const BIG_BODY: usize = 16 * 1024 * 1024;

/// What a client that writes its whole request before reading saw.
struct Outcome {
    /// The error the write failed with, if it did.
    write_error: Option<std::io::Error>,
    /// The status line's number, or 0 when nothing could be read.
    status: u16,
    /// The reply as text, for the assertion messages.
    text: String,
}

/// Send a head and then `body_len` bytes in 64 KiB writes, exactly like a client that writes the
/// whole request before it reads, then read whatever reply there is.
fn write_then_read(port: u16, head: &str, body_len: usize) -> Outcome {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the server takes a connection");
    stream.set_read_timeout(Some(Duration::from_secs(20))).expect("a read timeout");
    stream.set_write_timeout(Some(Duration::from_secs(20))).expect("a write timeout");
    let mut write_error = None;
    if let Err(error) = stream.write_all(head.as_bytes()) {
        write_error = Some(error);
    }
    let chunk = vec![b'x'; 64 * 1024];
    let mut sent = 0usize;
    while write_error.is_none() && sent < body_len {
        let take = chunk.len().min(body_len - sent);
        match stream.write_all(&chunk[..take]) {
            Ok(()) => sent += take,
            Err(error) => write_error = Some(error),
        }
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    Outcome { write_error, status, text }
}

fn post_head(host: &str, token: &str, length: usize) -> String {
    format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    )
}

#[test]
fn a_wrong_token_with_a_big_body_still_gets_its_401() {
    // X2: the common case is CAIM restarted with a new token while PhotoCraft sends a job.
    let server = support::TestServer::fake("early-401");
    let host = format!("127.0.0.1:{}", server.port());
    let outcome = write_then_read(server.port(), &post_head(&host, &"0".repeat(64), BIG_BODY), BIG_BODY);
    assert!(outcome.write_error.is_none(), "the whole body could be written: {:?}", outcome.write_error);
    assert_eq!(outcome.status, 401, "and the 401 arrived instead of a lost connection: {}", outcome.text);
    assert!(outcome.text.contains("token"), "with its sentence: {}", outcome.text);
    assert_eq!(server.get("/v1/health").status, 200, "the server is still serving");
}

#[test]
fn a_wrong_host_with_a_big_body_still_gets_its_400() {
    let server = support::TestServer::fake("early-host");
    let outcome = write_then_read(server.port(), &post_head("example.com", server.token(), BIG_BODY), BIG_BODY);
    assert!(outcome.write_error.is_none(), "the whole body could be written: {:?}", outcome.write_error);
    assert_eq!(outcome.status, 400, "the Host refusal arrived: {}", outcome.text);
}

#[test]
fn too_many_connections_with_a_big_body_still_gets_its_503() {
    let server = support::TestServer::options("early-503", ServeOptions { fake_backend: true, max_connections: 1, ..ServeOptions::default() });
    // One connection that says nothing holds the only slot.
    let holder = TcpStream::connect(("127.0.0.1", server.port())).expect("the first connection");
    std::thread::sleep(Duration::from_millis(100));
    let host = format!("127.0.0.1:{}", server.port());
    let outcome = write_then_read(server.port(), &post_head(&host, server.token(), BIG_BODY), BIG_BODY);
    assert!(outcome.write_error.is_none(), "the whole body could be written: {:?}", outcome.write_error);
    assert_eq!(outcome.status, 503, "the busy answer arrived: {}", outcome.text);
    drop(holder);
}

/// Send `first`, then one byte of `rest` every `gap`, until the server answers or closes, and say
/// how long that took. `None` means the server was still listening after `give_up`.
fn trickle(port: u16, first: &[u8], rest: &[u8], gap: Duration, give_up: Duration) -> Option<(Duration, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the server takes a connection");
    stream.set_read_timeout(Some(gap)).expect("a read timeout");
    let started = Instant::now();
    let _ = stream.write_all(first);
    let mut next = rest.iter().cycle();
    let mut raw = Vec::new();
    let mut buffer = [0u8; 1024];
    while started.elapsed() < give_up {
        match stream.read(&mut buffer) {
            Ok(0) => return Some((started.elapsed(), String::from_utf8_lossy(&raw).to_string())),
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if let Some(byte) = next.next()
                    && stream.write_all(&[*byte]).is_err()
                {
                    return Some((started.elapsed(), String::from_utf8_lossy(&raw).to_string()));
                }
            }
            Err(_) => return Some((started.elapsed(), String::from_utf8_lossy(&raw).to_string())),
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
    None
}

/// Short deadlines, so a slow client is cut off in well under a second.
fn quick(who: &str) -> support::TestServer {
    support::TestServer::options(
        who,
        ServeOptions { fake_backend: true, head_timeout: Duration::from_millis(400), body_timeout: Duration::from_millis(600), ..ServeOptions::default() },
    )
}

#[test]
fn a_head_that_trickles_in_is_cut_off_at_the_deadline() {
    // C1: one byte every 100 ms is well inside the 30 s per-read timeout, so it used to last for ever.
    let server = quick("slow-head");
    let ended = trickle(
        server.port(),
        b"G",
        b"ET /v1/health HTTP/1.1\r\nX-Slow: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        Duration::from_millis(100),
        Duration::from_secs(5),
    );
    let (after, reply) = ended.expect("the server closed a head that never finished");
    assert!(after < Duration::from_secs(3), "it was cut off near the 400 ms deadline, not after {after:?}");
    assert!(reply.starts_with("HTTP/1.1 400"), "with a 400 that says why: {reply}");
}

#[test]
fn a_body_that_trickles_in_is_cut_off_at_its_budget() {
    let server = quick("slow-body");
    let head = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n",
        server.port(),
        server.token()
    );
    let ended = trickle(server.port(), head.as_bytes(), b"x", Duration::from_millis(100), Duration::from_secs(5));
    let (after, reply) = ended.expect("the server closed a body that never finished");
    assert!(after < Duration::from_secs(3), "it was cut off near the body budget, not after {after:?}");
    assert!(reply.starts_with("HTTP/1.1 400"), "with a 400 that says why: {reply}");
}

#[test]
fn slow_clients_cannot_lock_the_manager_out() {
    // C1 as the reviewer ran it: every slot taken by a client that sends a byte now and then.
    let server = support::TestServer::options(
        "slowloris",
        ServeOptions { fake_backend: true, max_connections: 4, head_timeout: Duration::from_millis(400), ..ServeOptions::default() },
    );
    let port = server.port();
    let trickles: Vec<_> = (0..4).map(|_| std::thread::spawn(move || trickle(port, b"G", b"E", Duration::from_millis(100), Duration::from_secs(5)))).collect();
    std::thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    let mut status = 0;
    while started.elapsed() < Duration::from_secs(4) {
        status = server.get("/v1/health").status;
        if status == 200 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(status, 200, "a real request gets through once the slow ones hit their deadline");
    for handle in trickles {
        assert!(handle.join().expect("the trickle thread").is_some(), "every slow client was cut off");
    }
}
