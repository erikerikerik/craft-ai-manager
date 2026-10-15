mod support;

use craft_ai_manager::server::ServeOptions;
use std::{
    io::Write,
    net::TcpStream,
    time::{Duration, Instant},
};

#[test]
fn shutdown_interrupts_idle_and_incomplete_body_connections() {
    let server = support::TestServer::options(
        "bf03-shutdown",
        ServeOptions { fake_backend: true, head_timeout: Duration::from_secs(60), body_timeout: Duration::from_secs(60), ..ServeOptions::default() },
    );
    let idle = TcpStream::connect(("127.0.0.1", server.port())).expect("idle socket");
    let mut body = TcpStream::connect(("127.0.0.1", server.port())).expect("body socket");
    write!(body, "POST /v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 100\r\n\r\nx", server.port(), server.token())
        .expect("head and partial body");
    std::thread::sleep(Duration::from_millis(100));
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let start = Instant::now();
        let endpoint = server.shutdown();
        let _ = tx.send((start.elapsed(), endpoint));
    });
    let observed = rx.recv_timeout(Duration::from_secs(3));
    drop(idle);
    drop(body);
    thread.join().expect("shutdown thread");
    let (elapsed, endpoint) = observed.expect("shutdown does not wait for the read deadlines");
    assert!(elapsed < Duration::from_secs(3));
    assert!(!endpoint.exists());
}

#[test]
fn over_budget_upload_is_refused_before_its_body_arrives() {
    let server = support::TestServer::options(
        "bf03-memory",
        ServeOptions { fake_backend: true, memory_limit: 1024, drain_timeout: Duration::from_millis(10), ..ServeOptions::default() },
    );
    let request =
        format!("POST /v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 2048\r\n\r\n", server.port(), server.token());
    assert_eq!(server.send_raw(request.as_bytes()).status, 503);
    assert_eq!(server.get("/v1/health").status, 200);
}

#[test]
fn cancelled_uploads_release_admission_and_invalid_pngs_are_not_decoded_without_memory() {
    let server = support::TestServer::options(
        "bf03-upload-release",
        ServeOptions { fake_backend: true, max_queue: 1, memory_limit: 4096, drain_timeout: Duration::from_millis(10), ..ServeOptions::default() },
    );
    // The body fits, but the declared decoded working space does not. The PNG bytes are also
    // invalid: 503 rather than a PNG error proves the reservation preceded decoding.
    let job = support::job_json(support::TASK, 2, 2, [0, 0, 2, 2], 8, 3);
    for _ in 0..3 {
        let reply = server.submit(&job, b"invalid PNG", b"invalid mask");
        assert_eq!(reply.status, 503, "{}", reply.text());
        assert!(reply.sentence().contains("memory budget"));
    }
    assert_eq!(server.get("/v1/health").status, 200);
}
