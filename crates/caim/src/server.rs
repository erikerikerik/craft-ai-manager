//! The loopback server: bind `127.0.0.1:0`, publish the endpoint file, and answer the protocol of
//! SPEC amendment 11. One thread per connection, at most sixteen at once, a 30 s read timeout,
//! `Connection: close` and a `Content-Length` on every reply - no async runtime, no web framework.
//!
//! Order on every request: the token first (401), then the `Host` header (400, which is what stops
//! a browser on another machine from reaching us through a DNS rebinding), then the endpoint.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::VERSION;
use crate::endpoint::{self, ENDPOINT_ENV};
use crate::error::{Fail, Result, Status, quoted};
use crate::http::{self, Head, Response, read_head, write_error, write_response};
use crate::image;
use crate::job::{self, Runner, State};
use crate::multipart;
use crate::protocol::{self, JobRequest};
use crate::token::{self, bearer_matches};

/// Amendment 11: loopback only, and a port the operating system picks.
pub const LOCAL_HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
/// Amendment 11: at most sixteen connections open at once.
pub const MAX_CONNECTIONS: usize = 16;
/// Amendment 11: 30 s per read.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the accept loop sleeps when there is nothing to accept.
const ACCEPT_POLL: Duration = Duration::from_millis(2);
/// The endpoints of amendment 11, and nothing else.
const PATH_HEALTH: &str = "/v1/health";
const PATH_TASKS: &str = "/v1/tasks";
const PATH_JOBS: &str = "/v1/jobs";
const PATH_JOB_PREFIX: &str = "/v1/jobs/";
/// The content types this server sends.
const JSON: &str = "application/json";
const PNG: &str = "image/png";

/// How to start a server. The defaults are what `caim serve` uses; the tests change the last three.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// Run jobs on the built-in fake backend.
    pub fake_backend: bool,
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
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            fake_backend: false,
            endpoint_file: None,
            read_timeout: READ_TIMEOUT,
            max_connections: MAX_CONNECTIONS,
            max_queue: job::MAX_QUEUE,
            step_delay: Duration::ZERO,
        }
    }
}

/// What a connection thread needs: the port we are on, the token, and the job queue.
struct Context {
    port: u16,
    token: String,
    fake_backend: bool,
    read_timeout: Duration,
    runner: Arc<Runner>,
}

/// A running server. Dropping it, or calling `Server::shutdown`, stops the threads and removes the
/// endpoint file, so the next start replaces it.
pub struct Server {
    port: u16,
    token: String,
    endpoint_path: Option<PathBuf>,
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
        let runner = Arc::new(Runner::new(options.fake_backend, options.max_queue, options.step_delay)?);
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
            fake_backend: options.fake_backend,
            read_timeout: options.read_timeout,
            runner: Arc::clone(&runner),
        });
        let accept_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("caim-accept".to_string())
            .spawn(move || accept_loop(listener, context, accept_stop, options.max_connections))
            .map_err(|error| Fail::internal(format!("Craft AI Manager could not start its listener thread: {error}")))?;
        Ok(Server { port, token, endpoint_path, stop, accept: Some(handle), runner })
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

/// Accept until asked to stop, one thread per connection.
fn accept_loop(listener: TcpListener, context: Arc<Context>, stop: Arc<AtomicBool>, max_connections: usize) {
    let mut children: Vec<JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        children.retain(|child| !child.is_finished());
        match listener.accept() {
            Ok((mut stream, _peer)) => {
                if children.len() >= max_connections {
                    refuse(
                        &mut stream,
                        Status::Busy,
                        &format!("Craft AI Manager already has {} requests open, so it cannot take another one yet.", max_connections),
                    );
                    continue;
                }
                // The thread owns its own socket, so a thread that cannot start can still answer.
                match stream.try_clone() {
                    Ok(owned) => {
                        let connection_context = Arc::clone(&context);
                        match std::thread::Builder::new().name("caim-connection".to_string()).spawn(move || handle_connection(owned, &connection_context)) {
                            Ok(handle) => children.push(handle),
                            Err(_) => refuse(&mut stream, Status::Internal, "Craft AI Manager could not take another connection."),
                        }
                    }
                    Err(_) => refuse(&mut stream, Status::Internal, "Craft AI Manager could not take another connection."),
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => std::thread::sleep(ACCEPT_POLL),
            Err(_) => break,
        }
    }
    for child in children {
        let _ = child.join();
    }
}

/// Answer a connection that arrived while every worker slot was taken.
fn refuse(stream: &mut TcpStream, status: Status, sentence: &str) {
    let _ = write_error(stream, status, sentence);
    let _ = stream.shutdown(Shutdown::Both);
}

/// One connection, one request, then the socket is closed.
fn handle_connection(mut stream: TcpStream, context: &Context) {
    if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(context.read_timeout)).is_err() {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    if let Err(fail) = serve_request(&mut stream, context) {
        let _ = write_error(&mut stream, fail.status, &fail.sentence);
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// The order amendment 11 implies: token, then `Host`, then the endpoint.
fn serve_request(stream: &mut TcpStream, context: &Context) -> Result<()> {
    let head = read_head(stream)?;
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
    route(stream, context, &head)
}

/// Send one reply and close.
fn send(stream: &mut TcpStream, status: Status, content_type: Option<&str>, body: &[u8]) -> Result<()> {
    write_response(stream, &Response { status, content_type, body })
}

/// The 404 for a path that is not part of the protocol.
fn unknown_path(stream: &mut TcpStream, path: &str) -> Result<()> {
    write_error(stream, Status::NotFound, &format!("Craft AI Manager has no endpoint at {}.", quoted(path)))
}

fn route(stream: &mut TcpStream, context: &Context, head: &Head) -> Result<()> {
    let (method, path) = (head.method.as_str(), head.path.as_str());
    if path == PATH_HEALTH {
        return if method == "GET" { send(stream, Status::Ok, Some(JSON), &protocol::health(&format!("caim/{VERSION}"))) } else { unknown_path(stream, path) };
    }
    if path == PATH_TASKS {
        return if method == "GET" { send(stream, Status::Ok, Some(JSON), &protocol::tasks(context.fake_backend)) } else { unknown_path(stream, path) };
    }
    if path == PATH_JOBS {
        return if method == "POST" { submit(stream, context, head) } else { unknown_path(stream, path) };
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
fn submit(stream: &mut TcpStream, context: &Context, head: &Head) -> Result<()> {
    let length =
        head.content_length()?.ok_or_else(|| Fail::bad_request("The job request has no Content-Length, so Craft AI Manager cannot read its parts."))?;
    let body = http::read_body(stream, &head.leftover, length)?;
    let parts = multipart::parse(head.header("content-type"), &body)?;
    let request = JobRequest::parse(parts.job)?;
    request.validate(context.fake_backend)?;
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
    use super::{MAX_CONNECTIONS, READ_TIMEOUT, ServeOptions, Server, state_name};
    use crate::job::State;
    use std::path::PathBuf;
    use std::time::Duration;

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
