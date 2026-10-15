//! The loopback server: bind `127.0.0.1:0`, publish the endpoint file, and answer the protocol of
//! SPEC amendment 11. One thread per connection, at most sixteen at once, a 30 s read timeout,
//! `Connection: close` and a `Content-Length` on every reply - no async runtime, no web framework.
//!
//! Order on every request: the token first (401), then the `Host` header (400, which is what stops
//! a browser on another machine from reaching us through a DNS rebinding), then the endpoint.
//!
//! BF-01 added two things around that. One deadline per request (C1): the whole head within 5 s
//! and the body within a budget that grows with its length, however the bytes are split, so a
//! client trickling a byte at a time cannot hold a slot for ever. And a reply sent before the body
//! was read (X2: 401, 400, 413, 503) is followed by closing our side for writing and reading the
//! rest of the body - up to its `Content-Length` and a few seconds - before the socket closes, so a
//! client that writes its whole request before it reads still gets the reply instead of a reset.

use std::io::{ErrorKind, Read};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::VERSION;
use crate::endpoint::{self, ENDPOINT_ENV};
use crate::error::{Fail, Result, Status, quoted};
use crate::http::{self, Clock, Head, Response, read_head, write_error, write_response};
use crate::image;
use crate::job::{self, Backend, Runner, State};
use crate::lama;
use crate::multipart;
use crate::protocol::{self, JobRequest};
use crate::token::{self, bearer_matches};

/// Amendment 11: loopback only, and a port the operating system picks.
pub const LOCAL_HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
/// Amendment 11: at most sixteen connections open at once.
pub const MAX_CONNECTIONS: usize = 16;
/// Amendment 11: 30 s per read.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// C1: the whole request head within 5 s, however slowly the bytes come.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// C1: the body gets 30 s, plus a second for every 8 MiB, up to three minutes (`http::body_budget`).
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// X2: how long an early error reply waits for the rest of a body before it closes.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the accept loop sleeps when there is nothing to accept.
const ACCEPT_POLL: Duration = Duration::from_millis(2);
/// How long the accept loop waits after an accept that failed, before it tries again (C7).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(20);
/// What an early reply reads at most when the request never said how long its body is.
const DRAIN_UNKNOWN_BYTES: u64 = http::MAX_BODY_BYTES + http::MAX_HEAD_BYTES as u64;
/// The endpoints of amendment 11, and nothing else.
const PATH_HEALTH: &str = "/v1/health";
const PATH_TASKS: &str = "/v1/tasks";
const PATH_JOBS: &str = "/v1/jobs";
const PATH_JOB_PREFIX: &str = "/v1/jobs/";
/// The content types this server sends.
const JSON: &str = "application/json";
const PNG: &str = "image/png";

/// How to start a server. The defaults are what `caim serve` uses; the tests shorten the timeouts and limits.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// Run jobs on the built-in fake backend.
    pub fake_backend: bool,
    /// Where to look for installed models. `None` means `CRAFT_AI_MODELS`, else the platform path.
    pub models_root: Option<PathBuf>,
    /// Where to write the endpoint file. `None` means `CRAFT_AI_ENDPOINT`, else the platform path.
    pub endpoint_file: Option<PathBuf>,
    /// Read timeout per connection.
    pub read_timeout: Duration,
    /// Connections at once before a 503.
    pub max_connections: usize,
    /// Jobs queued or running before a 503.
    pub max_queue: usize,
    /// The fake backend's delay between its ten progress steps.
    pub step_delay: Duration,
    /// The whole request head has to arrive within this, however it is split into reads (C1).
    pub head_timeout: Duration,
    /// The body's budget before the per-size allowance is added (C1).
    pub body_timeout: Duration,
    /// How long an early error reply waits for the rest of the request body before closing (X2).
    pub drain_timeout: Duration,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            fake_backend: false,
            models_root: None,
            endpoint_file: None,
            read_timeout: READ_TIMEOUT,
            max_connections: MAX_CONNECTIONS,
            max_queue: job::MAX_QUEUE,
            step_delay: Duration::ZERO,
            head_timeout: HEAD_TIMEOUT,
            body_timeout: BODY_TIMEOUT,
            drain_timeout: DRAIN_TIMEOUT,
        }
    }
}

/// What a connection thread needs: the port we are on, the token, the deadlines, and the job queue.
struct Context {
    port: u16,
    token: String,
    /// Whether a backend is running, which is what `/v1/tasks` and `POST /v1/jobs` answer with.
    fill_offered: bool,
    read_timeout: Duration,
    head_timeout: Duration,
    body_timeout: Duration,
    drain_timeout: Duration,
    runner: Arc<Runner>,
}

/// A running server. Dropping it, or calling `Server::shutdown`, stops the threads and removes the
/// endpoint file, so the next start replaces it.
pub struct Server {
    port: u16,
    token: String,
    endpoint_path: Option<PathBuf>,
    /// Which backend this start picked, for the line `caim serve` prints.
    backend_note: &'static str,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    runner: Arc<Runner>,
}

impl Server {
    /// Bind the socket, make the token, write the endpoint file, and start the threads.
    pub fn start(options: ServeOptions) -> Result<Server> {
        let token = token::random_token()?;
        let listener =
            TcpListener::bind((LOCAL_HOST, 0)).map_err(|error| Fail::internal(format!("Craft AI Manager could not listen on 127.0.0.1: {error}")))?;
        let port = listener.local_addr().map_err(|error| Fail::internal(format!("Craft AI Manager could not find its own port: {error}")))?.port();
        listener.set_nonblocking(true).map_err(|error| Fail::internal(format!("Craft AI Manager could not set up its listener: {error}")))?;
        // MG-04 item 2: without `--fake-backend`, LaMa is offered only when MG-02 says it is
        // installed and acknowledged. Nothing is re-hashed at start; `verified.json` is the proof.
        let backend = if options.fake_backend {
            Backend::Fake { step_delay: options.step_delay }
        } else {
            lama::ready_pipeline_in(options.models_root.as_deref()).map_or(Backend::NoModel, Backend::Lama)
        };
        let fill_offered = !matches!(backend, Backend::NoModel);
        let backend_note = backend.note();
        let runner = Arc::new(Runner::new(backend, options.max_queue)?);
        let endpoint_path = options.endpoint_file.clone().or_else(endpoint::endpoint_file_from_env);
        if let Some(path) = endpoint_path.as_ref()
            && let Err(error) = endpoint::write(path, port, &token)
        {
            runner.stop();
            return Err(error);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let context = Arc::new(Context {
            port,
            token: token.clone(),
            fill_offered,
            read_timeout: options.read_timeout,
            head_timeout: options.head_timeout,
            body_timeout: options.body_timeout,
            drain_timeout: options.drain_timeout,
            runner: Arc::clone(&runner),
        });
        let accept_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("caim-accept".to_string())
            .spawn(move || accept_loop(listener, context, accept_stop, options.max_connections))
            .map_err(|error| Fail::internal(format!("Craft AI Manager could not start its listener thread: {error}")))?;
        Ok(Server { port, token, endpoint_path, backend_note, stop, accept: Some(handle), runner })
    }

    /// The port the operating system gave us.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The token for this start, which the endpoint file carries.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Which backend this start is running, for the terminal.
    #[must_use]
    pub const fn backend_note(&self) -> &'static str {
        self.backend_note
    }

    /// Where the endpoint file is, if this start wrote one.
    #[must_use]
    pub fn endpoint_path(&self) -> Option<&std::path::Path> {
        self.endpoint_path.as_deref()
    }

    /// The flag that asks this server to stop, so the binary can watch stdin without owning the
    /// server itself.
    #[must_use]
    pub fn stop_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Ask the server to stop; `Server::wait` then joins the threads.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Wait for the accept loop and the job worker to finish.
    pub fn wait(&mut self) {
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        self.runner.stop();
    }

    /// Stop cleanly: the endpoint file goes away, so the next start replaces it.
    pub fn shutdown(&mut self) -> Result<()> {
        self.request_stop();
        self.wait();
        if let Some(path) = self.endpoint_path.as_ref() {
            endpoint::remove(path, &self.token)?;
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        self.runner.stop();
        if let Some(path) = self.endpoint_path.as_ref() {
            let _ = endpoint::remove(path, &self.token);
        }
    }
}

/// Where the accept loop gets its connections from: the listener, or a stand-in in a test.
trait Accept: Send + 'static {
    fn accept_one(&self) -> std::io::Result<TcpStream>;
}

impl Accept for TcpListener {
    fn accept_one(&self) -> std::io::Result<TcpStream> {
        self.accept().map(|(stream, _peer)| stream)
    }
}

/// Accept until asked to stop, one thread per connection.
///
/// An accept that fails is about that one connection (it was reset or aborted before we took it,
/// or the process is briefly out of file handles), so the loop waits a moment and carries on; it
/// never stops serving because of one (C7).
fn accept_loop<L: Accept>(listener: L, context: Arc<Context>, stop: Arc<AtomicBool>, max_connections: usize) {
    let mut children: Vec<JoinHandle<()>> = Vec::new();
    // Threads that are answering a 503 and reading the rest of that request (X2), counted so they
    // are bounded too.
    let refusing = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        children.retain(|child| !child.is_finished());
        match listener.accept_one() {
            Ok(stream) => {
                if children.len() >= max_connections {
                    refuse_busy(stream, max_connections, &refusing, context.drain_timeout);
                    continue;
                }
                // The thread owns its own socket, so a thread that cannot start can still answer.
                match stream.try_clone() {
                    Ok(owned) => {
                        let connection_context = Arc::clone(&context);
                        match std::thread::Builder::new().name("caim-connection".to_string()).spawn(move || handle_connection(owned, &connection_context)) {
                            Ok(handle) => children.push(handle),
                            Err(_) => refuse(stream, Status::Internal, "Craft AI Manager could not take another connection."),
                        }
                    }
                    Err(_) => refuse(stream, Status::Internal, "Craft AI Manager could not take another connection."),
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => std::thread::sleep(ACCEPT_BACKOFF),
        }
    }
    for child in children {
        let _ = child.join();
    }
}

/// Answer a connection that could not be served at all, and close it.
fn refuse(mut stream: TcpStream, status: Status, sentence: &str) {
    let _ = write_error(&mut stream, status, sentence);
    let _ = stream.shutdown(Shutdown::Both);
}

/// Answer a connection that arrived while every worker slot was taken. The 503 goes out at once;
/// then, on a short-lived thread of its own, the rest of the request is read and thrown away, so a
/// client still writing its body can read the 503 (X2). Those threads are capped at the
/// connection limit as well; past that the 503 is sent and the socket simply closed.
fn refuse_busy(mut stream: TcpStream, max_connections: usize, refusing: &Arc<AtomicUsize>, drain_timeout: Duration) {
    let sentence = format!("Craft AI Manager already has {max_connections} requests open, so it cannot take another one yet.");
    let _ = write_error(&mut stream, Status::Busy, &sentence);
    if refusing.fetch_add(1, Ordering::SeqCst) >= max_connections {
        refusing.fetch_sub(1, Ordering::SeqCst);
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let counter = Arc::clone(refusing);
    let spawned = std::thread::Builder::new().name("caim-refuse".to_string()).spawn(move || {
        if stream.set_nonblocking(false).is_ok() {
            close_after_reply(&mut stream, Unread::Unknown, drain_timeout);
        } else {
            let _ = stream.shutdown(Shutdown::Both);
        }
        counter.fetch_sub(1, Ordering::SeqCst);
    });
    if spawned.is_err() {
        // The socket went with the closure, after its 503; only the count is left to undo.
        refusing.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How much of the request is still on the socket once the reply has gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unread {
    /// Nothing worth waiting for: the body was read, there was none, or the client was too slow.
    Nothing,
    /// The rest of a body whose length the request gave.
    Bytes(u64),
    /// A body of a length we do not know.
    Unknown,
}

/// One connection, one request, then the socket is closed.
fn handle_connection(mut stream: TcpStream, context: &Context) {
    if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(context.read_timeout)).is_err() {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let mut unread = Unread::Unknown;
    if let Err(fail) = serve_request(&mut stream, context, &mut unread) {
        let _ = write_error(&mut stream, fail.status, &fail.sentence);
    }
    close_after_reply(&mut stream, unread, context.drain_timeout);
}

/// Close a connection whose reply has been written. When part of the request is still unread, our
/// side is closed for writing first - the client sees the end of the reply - and the rest is read
/// and dropped, up to its length and `drain_timeout`, so closing does not reset a client that is
/// still writing and make it lose the reply (X2).
fn close_after_reply(stream: &mut TcpStream, unread: Unread, drain_timeout: Duration) {
    let limit = match unread {
        Unread::Nothing | Unread::Bytes(0) => {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        Unread::Bytes(left) => left,
        Unread::Unknown => DRAIN_UNKNOWN_BYTES,
    };
    let _ = stream.shutdown(Shutdown::Write);
    let until = Instant::now().checked_add(drain_timeout).unwrap_or_else(Instant::now);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut drained: u64 = 0;
    while drained < limit {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left.max(Duration::from_millis(1)))).is_err() {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => drained = drained.saturating_add(read as u64),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// The order amendment 11 implies: token, then `Host`, then the endpoint. `unread` is kept up to
/// date with how much of the request is still on the socket, for [close_after_reply].
fn serve_request(stream: &mut TcpStream, context: &Context, unread: &mut Unread) -> Result<()> {
    let clock = Clock::starting_now(context.head_timeout, context.read_timeout);
    let head = match read_head(stream, clock) {
        Ok(head) => head,
        Err(fail) => {
            // A head that never finished in time is a slow client: answer and close, no waiting.
            if clock.expired() {
                *unread = Unread::Nothing;
            }
            return Err(fail);
        }
    };
    *unread = match head.content_length() {
        Ok(Some(length)) => Unread::Bytes(length.saturating_sub(u64::try_from(head.leftover.len()).unwrap_or(u64::MAX))),
        Ok(None) => Unread::Nothing,
        Err(_) => Unread::Unknown,
    };
    if !bearer_matches(&context.token, head.header("authorization").unwrap_or_default()) {
        return write_error(stream, Status::Unauthorized, "Craft AI Manager needs its token in the Authorization header.");
    }
    let expected = format!("127.0.0.1:{}", context.port);
    if head.header("host") != Some(expected.as_str()) {
        return write_error(
            stream,
            Status::BadRequest,
            &format!("Craft AI Manager only answers requests addressed to {} (this one says {}).", expected, quoted(head.header("host").unwrap_or_default())),
        );
    }
    route(stream, context, &head, unread)
}

/// Send one reply and close.
fn send(stream: &mut TcpStream, status: Status, content_type: Option<&str>, body: &[u8]) -> Result<()> {
    write_response(stream, &Response { status, content_type, body })
}

/// The 404 for a path that is not part of the protocol.
fn unknown_path(stream: &mut TcpStream, path: &str) -> Result<()> {
    write_error(stream, Status::NotFound, &format!("Craft AI Manager has no endpoint at {}.", quoted(path)))
}

fn route(stream: &mut TcpStream, context: &Context, head: &Head, unread: &mut Unread) -> Result<()> {
    let (method, path) = (head.method.as_str(), head.path.as_str());
    if path == PATH_HEALTH {
        return if method == "GET" { send(stream, Status::Ok, Some(JSON), &protocol::health(&format!("caim/{VERSION}"))) } else { unknown_path(stream, path) };
    }
    if path == PATH_TASKS {
        return if method == "GET" { send(stream, Status::Ok, Some(JSON), &protocol::tasks(context.fill_offered)) } else { unknown_path(stream, path) };
    }
    if path == PATH_JOBS {
        return if method == "POST" { submit(stream, context, head, unread) } else { unknown_path(stream, path) };
    }
    let Some(rest) = path.strip_prefix(PATH_JOB_PREFIX) else {
        return unknown_path(stream, path);
    };
    let (id, tail) = match rest.split_once('/') {
        Some((id, tail)) => (id, tail),
        None => (rest, ""),
    };
    if !protocol::is_safe_id(id) {
        return write_error(stream, Status::NotFound, "Craft AI Manager does not recognise that job id.");
    }
    match (method, tail) {
        ("GET", "") => {
            let (state, progress, message) = context.runner.state(id)?;
            send(stream, Status::Ok, Some(JSON), &protocol::job_state(state.wire(), progress, &message))
        }
        ("DELETE", "") => {
            context.runner.cancel(id)?;
            send(stream, Status::NoContent, None, &[])
        }
        ("GET", "result") => {
            let found = context.runner.provenance(id)?;
            send(stream, Status::Ok, Some(JSON), &protocol::result_provenance(&found.model, &found.device, &found.provider, found.elapsed_ms, found.image_size))
        }
        ("GET", "result.png") => {
            let png = context.runner.take_png(id)?;
            send(stream, Status::Ok, Some(PNG), &png)
        }
        _ => unknown_path(stream, path),
    }
}

/// `POST /v1/jobs`: read the body, split the three parts, check them against each other, queue.
fn submit(stream: &mut TcpStream, context: &Context, head: &Head, unread: &mut Unread) -> Result<()> {
    let length =
        head.content_length()?.ok_or_else(|| Fail::bad_request("The job request has no Content-Length, so Craft AI Manager cannot read its parts."))?;
    let clock = Clock::starting_now(http::body_budget(context.body_timeout, length), context.read_timeout);
    let body = match http::read_body(stream, &head.leftover, length, clock) {
        Ok(body) => body,
        Err(fail) => {
            // A body over the cap is refused before it is read, so the rest of it is still there;
            // anything else (too slow, cut short) has nothing left worth waiting for.
            if fail.status != Status::TooLarge {
                *unread = Unread::Nothing;
            }
            return Err(fail);
        }
    };
    *unread = Unread::Nothing;
    let parts = multipart::parse(head.header("content-type"), &body)?;
    let request = JobRequest::parse(parts.job)?;
    request.validate(context.fill_offered)?;
    let [width, height] = request.image_size;
    let frame = image::decode_image(parts.image, width, height, usize::from(request.channels), request.bit_depth)?;
    let mask = image::decode_mask(parts.mask, width, height)?;
    let id = context.runner.submit(frame, mask)?;
    send(stream, Status::Accepted, Some(JSON), &protocol::job_id(&id))
}

/// The name a poll reports for a state, which the tests and the binary both read.
#[must_use]
pub const fn state_name(state: State) -> &'static str {
    state.wire()
}

/// The environment variable that moves the endpoint file, re-exported for the binary's help text.
pub const ENDPOINT_VARIABLE: &str = ENDPOINT_ENV;

#[cfg(test)]
mod tests {
    use super::{Accept, Context, MAX_CONNECTIONS, READ_TIMEOUT, ServeOptions, Server, accept_loop, state_name};
    use crate::job::{Backend, MAX_QUEUE, Runner, State};
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A listener whose next few accepts fail the way a reset or aborted connection does, or the way
    /// running out of file handles does, before it hands over real connections.
    struct Flaky {
        listener: TcpListener,
        failures: Mutex<Vec<std::io::Error>>,
    }

    impl Accept for Flaky {
        fn accept_one(&self) -> std::io::Result<TcpStream> {
            if let Some(error) = self.failures.lock().expect("the failure list").pop() {
                return Err(error);
            }
            self.listener.accept_one()
        }
    }

    #[test]
    fn an_accept_that_fails_does_not_stop_the_server() {
        // C7: any accept error other than WouldBlock or Interrupted used to end the accept loop.
        let listener = TcpListener::bind("127.0.0.1:0").expect("a listener");
        listener.set_nonblocking(true).expect("non-blocking");
        let port = listener.local_addr().expect("its address").port();
        let failures = vec![
            std::io::Error::from(ErrorKind::ConnectionAborted),
            std::io::Error::from(ErrorKind::ConnectionReset),
            std::io::Error::other("Too many open files"),
        ];
        let flaky = Flaky { listener, failures: Mutex::new(failures) };
        let runner = Arc::new(Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("the worker starts"));
        let options = ServeOptions::default();
        let context = Arc::new(Context {
            port,
            token: "t".repeat(64),
            fill_offered: true,
            read_timeout: options.read_timeout,
            head_timeout: options.head_timeout,
            body_timeout: options.body_timeout,
            drain_timeout: options.drain_timeout,
            runner: Arc::clone(&runner),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || accept_loop(flaky, context, loop_stop, MAX_CONNECTIONS));
        std::thread::sleep(Duration::from_millis(200));
        let mut client = TcpStream::connect(("127.0.0.1", port)).expect("the server still takes connections");
        client.set_read_timeout(Some(Duration::from_secs(5))).expect("a read timeout");
        let request = format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", "t".repeat(64));
        client.write_all(request.as_bytes()).expect("the request goes out");
        let mut reply = String::new();
        let _ = client.read_to_string(&mut reply);
        assert!(reply.starts_with("HTTP/1.1 200"), "the request after three failed accepts is answered: {reply:?}");
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("the accept loop stops when asked");
        runner.stop();
    }

    fn temp_endpoint(who: &str) -> PathBuf {
        std::env::temp_dir().join(format!("caim-server-{}-{who}.json", std::process::id()))
    }

    #[test]
    fn the_defaults_are_the_protocol_numbers() {
        let options = ServeOptions::default();
        assert_eq!(MAX_CONNECTIONS, 16);
        assert_eq!(READ_TIMEOUT, Duration::from_secs(30));
        assert!(!options.fake_backend);
        assert_eq!(options.max_connections, MAX_CONNECTIONS);
        assert_eq!(options.max_queue, 8);
    }

    #[test]
    fn a_server_binds_a_free_port_and_removes_its_file_on_shutdown() {
        let path = temp_endpoint("start");
        let _ = std::fs::remove_file(&path);
        let mut server =
            Server::start(ServeOptions { fake_backend: true, endpoint_file: Some(path.clone()), ..ServeOptions::default() }).expect("the server starts");
        assert!(server.port() > 0);
        assert_eq!(server.token().len(), 64);
        assert!(std::fs::File::open(&path).is_ok(), "the endpoint file is there while it runs");
        server.shutdown().expect("shutdown is clean");
        assert!(std::fs::metadata(&path).is_err(), "a clean shutdown removes the endpoint file");
        assert_eq!(state_name(State::Done), "done");
    }
}
