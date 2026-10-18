//! Where the manager publishes its port and token (SPEC amendment 11 > Discovery), written
//! atomically so a client can never read half a file.
//!
//! On Windows the file lives under %LOCALAPPDATA%\CraftAI\, which already carries per-user
//! permissions, so the token inside it is readable only by this user. CRAFT_AI_ENDPOINT names a
//! different file, which is how the tests keep out of each other's way. On Linux and macOS that
//! folder has no such guarantee, so the file - and the temp file before it - are created with mode
//! 0600 (the MG-01 review follow-up).

use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::error::{Fail, Result};

/// The version an endpoint file declares.
pub const ENDPOINT_VERSION: u64 = 1;
/// The environment variable that names the endpoint file instead of the platform default.
pub const ENDPOINT_ENV: &str = "CRAFT_AI_ENDPOINT";
/// The folder inside the user's data directory.
const ENDPOINT_DIR: &str = "CraftAI";
/// The file inside that folder.
const ENDPOINT_NAME: &str = "endpoint.json";
/// The file that says where the manager program lives (amendment 18, "Starting CAIM on demand").
pub const APP_NAME: &str = "app.json";
/// The version an app file declares.
pub const APP_VERSION: u64 = 1;
/// How long to wait for a manager to answer `/v1/health`. Loopback answers at once, so this is
/// only a guard against a port somebody else has taken over.
const HEALTH_TIMEOUT: Duration = Duration::from_millis(250);
/// The words every Craft AI Manager error body carries (`crate::http::error_body`), which is what
/// tells a busy manager's 503 from another local service's.
const CAIM_ERROR_MARK: &str = "Craft AI Manager";
/// The words only a Craft AI Manager health answer carries (`crate::protocol::health`), which is
/// what tells a running manager's 200 from another local service that answers every path the same.
const CAIM_HEALTH_MARK: &str = "\"app\":\"caim/";
/// How much of a health reply is read before deciding. A real one is a status line, three headers
/// and one short JSON body; past this is somebody else's page.
const MAX_PROBE_BYTES: usize = 2048;
/// The endpoint file is JSON, and the protocol caps JSON at 64 KiB.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// The file beside `endpoint.json` that says one `caim open` is deciding whether to start a
/// manager right now. It is never deleted, so every run opens and locks the same inode.
const LAUNCH_LOCK_NAME: &str = "open.lock";
/// How long a second `caim open` waits for the first to finish starting before it gives up. The
/// lock is only held while a server binds its port and publishes the endpoint file, so a few
/// seconds is a wide margin, and giving up costs a sentence rather than a manager.
const LAUNCH_LOCK_WAIT: Duration = Duration::from_secs(5);
/// How long between polls of a launch lock somebody else is holding.
const LAUNCH_LOCK_POLL: Duration = Duration::from_millis(25);

static WRITE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// What the endpoint file holds: the port to connect to and the token to present.
#[derive(Serialize)]
struct EndpointFile {
    version: u64,
    port: u32,
    token: String,
}

/// What the app file holds: the shape's version and this executable, as an absolute path.
#[derive(Serialize)]
struct AppFile {
    version: u64,
    exe: String,
}

/// The file for this platform, or None when the platform has nowhere to put one.
#[must_use]
pub fn default_endpoint_file() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(ENDPOINT_NAME))
}

/// The folder CAIM keeps its files in: `%LOCALAPPDATA%\CraftAI` on Windows, and the platform
/// equivalent elsewhere. The model store hangs off here too, so there is one place to look.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    platform_data_dir().map(|dir| dir.join(ENDPOINT_DIR))
}

fn platform_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()).map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").filter(|value| !value.is_empty()).map(|home| PathBuf::from(home).join("Library").join("Application Support"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()).map(|home| PathBuf::from(home).join(".local").join("share")))
    }
}

/// CRAFT_AI_ENDPOINT when it names a file, otherwise the platform default.
#[must_use]
pub fn endpoint_file_from_env() -> Option<PathBuf> {
    match std::env::var_os(ENDPOINT_ENV) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => default_endpoint_file(),
    }
}

/// Write the endpoint file: a temp file in the same folder, then a rename over the old one. A
/// client that reads the file while we start sees either the previous start or this one, never a
/// mixture, and the rename replaces the previous file rather than failing because it exists.
pub fn write(path: &Path, port: u16, token: &str) -> Result<()> {
    let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
        return Err(Fail::internal(format!("Craft AI Manager has no folder to write {} in.", path.display())));
    };
    std::fs::create_dir_all(parent).map_err(|error| Fail::internal(format!("Craft AI Manager could not create {}: {error}", parent.display())))?;
    let body = serde_json::to_vec(&EndpointFile { version: ENDPOINT_VERSION, port: u32::from(port), token: token.to_string() })?;
    let temp = temp_path(path);
    let written = write_whole(&temp, &body);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(Fail::internal(format!("Craft AI Manager could not write its endpoint file at {}: {error}", path.display())));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(Fail::internal(format!("Craft AI Manager could not put its endpoint file at {}: {error}", path.display())));
    }
    Ok(())
}

/// The app file that belongs next to a given endpoint file.
#[must_use]
pub fn app_path_for(endpoint: &Path) -> PathBuf {
    endpoint.with_file_name(APP_NAME)
}

/// The launch lock that belongs next to a given endpoint file. It sits beside the file the winner
/// of the lock is about to write, so one folder holds everything about one manager.
#[must_use]
pub fn launch_lock_path_for(endpoint: &Path) -> PathBuf {
    endpoint.with_file_name(LAUNCH_LOCK_NAME)
}

/// Write the app file, atomically the way [write] writes the endpoint file, and never delete it on
/// the way out: it says where the program is, not that the program is running. PhotoCraft reads it
/// to start the manager when nothing is answering (amendment 18).
pub fn write_app(path: &Path, exe: &Path) -> Result<()> {
    let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
        return Err(Fail::internal(format!("Craft AI Manager has no folder to write {} in.", path.display())));
    };
    std::fs::create_dir_all(parent).map_err(|error| Fail::internal(format!("Craft AI Manager could not create {}: {error}", parent.display())))?;
    let body = serde_json::to_vec(&AppFile { version: APP_VERSION, exe: exe.display().to_string() })?;
    let temp = temp_path(path);
    if let Err(error) = write_whole(&temp, &body) {
        let _ = std::fs::remove_file(&temp);
        return Err(Fail::internal(format!("Craft AI Manager could not write its app file at {}: {error}", path.display())));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(Fail::internal(format!("Craft AI Manager could not put its app file at {}: {error}", path.display())));
    }
    Ok(())
}

/// Create, write and flush one file before it is renamed into place.
fn write_whole(temp: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut file = create_private(temp)?;
    file.write_all(body)?;
    file.flush()
}

/// Make a file that only this user can read. On Linux and macOS the mode is fixed when the file
/// is created, so the token is never world-readable, not even in the moment between writing the
/// temp file and renaming it; a umask can only clear bits, so 0600 is what we get. Windows needs
/// nothing: %LOCALAPPDATA% already carries a per-user ACL that a plain create inherits.
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
}

/// The non-Unix half of [create_private](write_whole), where the folder's permissions are enough.
#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

/// A temp name in the same folder as the target, so the rename stays inside one folder. The name
/// hangs off the target's own name, so two files in one folder never reach for the same temp.
fn temp_path(path: &Path) -> PathBuf {
    let counter = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| ENDPOINT_NAME.to_string());
    let name = format!("{stem}.tmp-{}-{counter}", std::process::id());
    path.with_file_name(name)
}

/// Remove the endpoint file on a clean shutdown - but only if it is still ours. A second manager
/// that started while this one was running has replaced it, and its file must stay.
pub fn remove(path: &Path, token: &str) -> Result<()> {
    if !is_ours(path, token) {
        return Ok(());
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Fail::internal(format!("Craft AI Manager could not remove its endpoint file: {error}"))),
    }
}

/// Does the file at this path still hold this token?
fn is_ours(path: &Path, token: &str) -> bool {
    read_value(path).is_some_and(|value| value.get("token").and_then(Value::as_str) == Some(token))
}

/// The port the manager published, when the file is there, within the size cap, and names a port.
/// A file left behind by a manager that died still names a port, so anyone asking whether a server
/// is running has to ask the port as well (MG-09).
#[must_use]
pub fn published_port(path: &Path) -> Option<u16> {
    let port = read_value(path)?.get("port").and_then(Value::as_u64)?;
    u16::try_from(port).ok()
}

/// The port and token an endpoint file publishes together, or `None` when it does not publish a
/// usable pair. A stale file naming a port nobody owns is still a pair; whether the port is alive
/// is a separate question, answered by [health_is_answering].
#[must_use]
pub fn published_credentials(path: &Path) -> Option<(u16, String)> {
    let value = read_value(path)?;
    let port = u16::try_from(value.get("port").and_then(Value::as_u64)?).ok()?;
    let token = value.get("token").and_then(Value::as_str).filter(|token| !token.is_empty())?;
    Some((port, token.to_string()))
}

/// What a health probe found on the port an endpoint file names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// A manager answered `/v1/health` with a 200 carrying this program's own health body. A 200
    /// from anything else is `Absent`.
    Ready,
    /// Something answered with a 503 that carries this program's own error sentence. That is a
    /// manager whose connection slots are all taken: the accept loop refuses a request before it
    /// even routes it (`server.rs`, `refuse_busy`). It is running, only busy, and starting a second
    /// manager over its endpoint file would be wrong. A 503 from anything else is `Absent`.
    Busy,
    /// Nothing answered the way a manager does: a shut port, a socket that died, a reply that is
    /// not a status line we know, or a stranger holding the port.
    Absent,
}

/// What the manager on loopback at `port` says about itself when asked with this token: `GET
/// /v1/health`, and nearly all of the answer is one status line.
///
/// This is a raw request rather than a client library on purpose. `caim open` has to answer a
/// simple question about a machine-local port. A 200 and a 503 both need more than the status line,
/// because any local service can answer both of them to a path it has never heard of.
#[must_use]
pub fn health_probe(port: u16, token: &str) -> Health {
    if token.chars().any(|ch| matches!(ch, '\r' | '\n' | '\0')) {
        // A token with a newline in it would be somebody writing headers for us.
        return Health::Absent;
    }
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, HEALTH_TIMEOUT) else { return Health::Absent };
    if stream.set_read_timeout(Some(HEALTH_TIMEOUT)).is_err() {
        return Health::Absent;
    }
    let request = format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).and_then(|()| stream.flush()).is_err() {
        return Health::Absent;
    }
    health_from(&read_reply(&mut stream, Instant::now() + HEALTH_TIMEOUT))
}

/// Read one reply and stop when the reply is over, never after a number of reads: this program's own
/// mark in the body, the whole body the headers promised, a hang-up, the cap, or the deadline. One
/// read can legally hand back half a status line, and the manager writes its headers and its body as
/// two writes (http.rs:334-335) that TCP splits again however it likes, so deciding on a fragment
/// would start a second manager on top of the one that is running and overwrite its endpoint file.
/// Split out of [health_probe] so the pieces can be handed over by hand.
fn read_reply<R: Read>(stream: &mut R, deadline: Instant) -> Vec<u8> {
    let mut head: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 256];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if let Some(chunk) = buffer.get(..read) {
                    head.extend_from_slice(chunk);
                }
            }
            // Out of time or a dead socket: decide on whatever arrived.
            Err(_) => break,
        }
        if head.len() >= MAX_PROBE_BYTES || Instant::now() >= deadline {
            break;
        }
        let so_far = String::from_utf8_lossy(&head);
        if !so_far.contains("\r\n") {
            continue;
        }
        // A 401 and anything else are decided by the status line alone: only this program answers
        // /v1/health that way to the token the endpoint file holds. A 200 and a 503 are different:
        // any local service can answer both of them to a path it has never heard of, and the body is
        // the only thing that says whose answer it is.
        let mark = if status_line_says(&so_far, "503") {
            CAIM_ERROR_MARK
        } else if status_line_says(&so_far, "200") {
            CAIM_HEALTH_MARK
        } else {
            break;
        };
        // This program's own words in the body: the manager is answering, and there is nothing left
        // worth reading.
        if so_far.contains(mark) {
            break;
        }
        // Not ours yet, so the answer counts as finished only when it says so itself: the whole body
        // its headers promised has arrived, or the peer hung up (a reply that declares no length ends
        // that way). Counting reads instead calls the first piece of a manager's body a stranger's
        // whole answer, and starts a second manager over a live one.
        if body_still_missing(&head).is_some_and(|bytes| bytes == 0) {
            break;
        }
    }
    head
}

/// What the bytes of one reply say about the port. A 200 and a 503 each have to carry this program's
/// own words, because either is the ordinary way for any local service to answer a path it has never
/// heard of. A reply that stopped before its headers were over is the other case: a manager writes
/// headers and body in one pass, so a half answer is far more likely a slow manager than a stranger.
fn health_from(head: &[u8]) -> Health {
    let text = String::from_utf8_lossy(head);
    if status_line_says(&text, "200") {
        if text.contains(CAIM_HEALTH_MARK) {
            Health::Ready
        } else if text.contains("\r\n\r\n") {
            // A whole 200 from something that is not this program: a server that answers every path
            // the same way has taken the port a crashed manager left behind. Nothing is running, so
            // a manager may start.
            Health::Absent
        } else {
            // The reply stopped before its headers were over. A manager writes its headers and body
            // in one pass, so a half answer is far more likely a slow manager than a stranger, and
            // starting a second manager over a live one is the worse mistake - round 2's P1.
            Health::Ready
        }
    } else if status_line_says(&text, "503") {
        if text.contains(CAIM_ERROR_MARK) {
            Health::Busy
        } else if text.contains("\r\n\r\n") {
            // A whole 503 from something that is not this program: a dev server or a proxy that has
            // taken the port a crashed manager left behind. Nothing is running, so a manager may start.
            Health::Absent
        } else {
            // The reply stopped after its status line. A busy manager writes its headers and body in
            // one pass, so a half answer is far more likely a slow one than a stranger, and starting a
            // second manager over a live one is the worse mistake - round 2's P1.
            Health::Busy
        }
    } else {
        // A 401 means the port is answering but not with the token the file holds. The launch lock
        // below closes the window where that could happen while a manager is starting, and a port
        // that answers for nobody else is one a real manager can take over, so this stays Absent.
        Health::Absent
    }
}
/// Does this reply carry `code` on its status line, under either HTTP version?
fn status_line_says(text: &str, code: &str) -> bool {
    let after_version = text.strip_prefix("HTTP/1.1 ").or_else(|| text.strip_prefix("HTTP/1.0 "));
    after_version.is_some_and(|rest| rest.starts_with(code))
}

/// How many body bytes this reply still owes: `None` while its headers are still arriving, and
/// how many are missing once they are over - `Some(0)` means the whole body the reply
/// promised is here. A reply that declares no length ends at the hang-up, which the read loop already
/// treats as the end, so a missing `Content-Length` is `None` and means keep reading.
fn body_still_missing(head: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(head);
    let (headers, body) = text.split_once("\r\n\r\n")?;
    let mut promised = None;
    for line in headers.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.eq_ignore_ascii_case("content-length") {
            promised = value.trim().parse::<usize>().ok();
        }
    }
    Some(promised?.saturating_sub(body.len()))
}

/// What `caim open` should do about a manager (amendment 18).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// One is already answering on loopback, and `port` is where.
    AlreadyRunning { port: u16 },
    /// Nothing is answering, so this is the run that starts one.
    Start,
}

/// Look at the endpoint file this run would write, and ask whether a manager is already answering
/// there. A missing, unreadable, or dead file means start one: the file is a hint, not a promise.
///
/// A manager that answers 503 with its own busy sentence, or 200 with its own health body, is a
/// manager that is running, so it counts as already running. A 200 or a 503 from anything else on
/// that port is a stranger that took the port a dead manager left, and it does not stop a start.
#[must_use]
pub fn launch_decision(endpoint: Option<&Path>) -> Launch {
    let Some(path) = endpoint.filter(|path| path.is_file()) else { return Launch::Start };
    let Some((port, token)) = published_credentials(path) else { return Launch::Start };
    match health_probe(port, &token) {
        Health::Ready | Health::Busy => Launch::AlreadyRunning { port },
        Health::Absent => Launch::Start,
    }
}

/// What asking for the launch lock ended up as. Contention and a machine that cannot make a lock
/// are different answers: one says another run is starting a manager, the other says nobody is, and
/// `caim open` has to say which.
pub enum LockOutcome {
    /// This run holds the lock, so this run gets to decide whether a manager is running.
    Taken(LaunchLock),
    /// Another `caim open` holds the lock and has not finished starting, and this run waited out
    /// [LAUNCH_LOCK_WAIT] for it. Nothing to start here: that run is the one writing the endpoint file.
    Held,
    /// The lock could not be made at all: no folder to put it in, a folder that cannot be written, a
    /// token that could not be made, a write that failed. Nobody else is starting a manager, so this is
    /// a failure to report rather than a promise about another run.
    Failed(Fail),
}

/// The lock one `caim open` holds while it decides whether a manager is running and, when none
/// is, starts one. Deciding and starting are one step, not two: without the lock, two `caim open`
/// runs launched at the same moment can both decide to start, both serve, and race to replace
/// `endpoint.json`. The loser keeps serving on a port no client can find, and both write the same
/// activity log. The lock goes as soon as the server is listening, so it is held for a moment
/// rather than for the life of the manager.
///
/// Ownership is the operating system's answer to "is that run still alive?", not a timestamp. The
/// holder keeps the file open - read and write - and holds an exclusive lock on it
/// ([std::fs::File::try_lock]), and the operating system releases that lock when the holder's
/// handle closes, which it does by itself when the process ends - cleanly, after a crash, or
/// because it was killed. So a lock whose lock is free has an owner that is gone, whatever the
/// clock says, and a lock whose lock is held belongs to a live run however old the file is. There
/// is no stale window: a start that is merely slow, descheduled, or stopped in a debugger keeps its
/// lock until it ends, which is the whole point of having one.
///
/// Letting go closes this run's handle and nothing else: the file keeps its name. Unlinking it
/// would let a run that already had that inode open keep locking a file no path leads to while the
/// next run made a fresh one under the same name, and both would be told they may start a manager.
/// With the name in place every run opens and locks the same inode, so the operating system's
/// answer is always about the file that matters. A lock a finished run left behind is simply the
/// next run's lock, free for the taking, which is why nothing here deletes it.
pub struct LaunchLock {
    /// The handle this run holds the operating system's lock through. Keeping it open is what makes
    /// this run the owner; the lock goes when it closes, and the operating system closes it if this
    /// run ends. The file itself is never removed, so the next run locks the same inode.
    file: std::fs::File,
}

impl LaunchLock {
    /// Take the lock: [LockOutcome::Taken] when this run holds it, [LockOutcome::Held] when another
    /// `caim open` holds it, and [LockOutcome::Failed] when no lock could be made at all. A held lock is
    /// waited for up to [LAUNCH_LOCK_WAIT], which is how the second run ends up seeing the manager the
    /// first one started instead of starting its own. A lock whose owner has ended is taken over the
    /// moment it is seen - see [LaunchLock] for why the age of the file plays no part.
    #[must_use]
    pub fn take(path: &Path) -> LockOutcome {
        let nonce = match crate::token::random_token() {
            Ok(nonce) => nonce,
            Err(fail) => return LockOutcome::Failed(fail),
        };
        // The lock lives beside the endpoint file, so the folder comes first: a machine that has
        // never run the manager has no folder yet, and that is not somebody else holding the lock.
        let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
            return LockOutcome::Failed(Fail::internal("Craft AI Manager has no folder to put its launch lock in."));
        };
        if let Err(error) = std::fs::create_dir_all(parent) {
            return LockOutcome::Failed(Fail::internal(format!("Craft AI Manager could not create {}: {error}", parent.display())));
        }
        let deadline = Instant::now() + LAUNCH_LOCK_WAIT;
        loop {
            match OpenOptions::new().read(true).write(true).create_new(true).open(path) {
                // Nobody holds the name, so it is ours to make. The record goes in before the lock
                // does, so the file is never sitting there without saying who wrote it.
                Ok(mut file) => {
                    // A record that cannot be written leaves the file where it is for the next run
                    // to take over. Deleting it would hand a fresh inode to a run already waiting on
                    // this one, which is the split this lock exists to avoid.
                    if let Err(error) = write_record(&mut file, &nonce) {
                        return LockOutcome::Failed(Fail::internal(format!("Craft AI Manager could not write its launch lock at {}: {error}", path.display())));
                    }
                    match file.try_lock() {
                        Ok(()) => return LockOutcome::Taken(LaunchLock { file }),
                        // Somebody opened the file in the moment between this run making it and
                        // locking it, and locked it first: that is another run, so wait like anywhere.
                        Err(std::fs::TryLockError::WouldBlock) => {}
                        Err(error) => {
                            return LockOutcome::Failed(Fail::internal(format!(
                                "Craft AI Manager could not lock its launch lock at {}: {error}",
                                path.display()
                            )));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                // A folder that is not there, a file we cannot create: nothing can be decided safely,
                // and no other run is deciding it either, so this is not contention.
                Err(error) => {
                    return LockOutcome::Failed(Fail::internal(format!("Craft AI Manager could not open its launch lock at {}: {error}", path.display())));
                }
            }
            // The name is taken, and the only question worth asking is whether the run that took it is
            // still there. The operating system answers it: its lock is released when the owner's
            // handle closes, which it does by itself when that run ends.
            let mut file = match OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => file,
                // Somebody outside this program removed the file, so the name is free again and
                // this run can claim it from the start.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return LockOutcome::Failed(Fail::internal(format!("Craft AI Manager could not open its launch lock at {}: {error}", path.display())));
                }
            };
            match file.try_lock() {
                // The owner has ended, so the file is nobody's. Take it over: this run's bytes, and
                // the lock this run now holds through `file`.
                Ok(()) => {
                    if let Err(error) = file.set_len(0).and_then(|()| write_record(&mut file, &nonce)) {
                        return LockOutcome::Failed(Fail::internal(format!("Craft AI Manager could not write its launch lock at {}: {error}", path.display())));
                    }
                    return LockOutcome::Taken(LaunchLock { file });
                }
                // A live run holds it. Waiting it out is the honest answer; taking it would start a
                // second manager, which is the very thing the lock exists to stop.
                Err(std::fs::TryLockError::WouldBlock) => drop(file),
                // The operating system will not lock the file at all, so a live owner cannot be told
                // from a dead one. That is a broken folder, and it is not contention either.
                Err(error) => {
                    return LockOutcome::Failed(Fail::internal(format!("Craft AI Manager could not lock its launch lock at {}: {error}", path.display())));
                }
            }
            if Instant::now() >= deadline {
                return LockOutcome::Held;
            }
            std::thread::sleep(LAUNCH_LOCK_POLL);
        }
    }

    /// The process id this lock's record names as its owner, read through the handle that holds the
    /// lock - while it is held, no other handle can be relied on to read the file. For whoever looks
    /// at the folder, never for deciding: the operating system's lock is what says whether an owner
    /// is alive.
    #[must_use]
    pub fn owner_id(&self) -> Option<u32> {
        let mut file = self.file.try_clone().ok()?;
        let mut text = String::new();
        file.rewind().ok()?;
        file.read_to_string(&mut text).ok()?;
        text.lines().nth(1)?.parse().ok()
    }
}

/// The bytes a launch lock holds: a random nonce, so a person reading the folder can tell one
/// run's record from the next run's, and this run's process id, so they can see who is starting a
/// manager. Neither decides anything: the operating system's lock is what says whether an owner is
/// alive, and it needs nothing from the process table to do that.
fn write_record(file: &mut std::fs::File, nonce: &str) -> std::io::Result<()> {
    file.write_all(format!("{nonce}\n{}\n", std::process::id()).as_bytes())?;
    file.flush()
}

/// The endpoint file as JSON, never reading past the cap so an oversized file is never parsed.
/// `None` when it is missing, too big, or not JSON.
fn read_value(path: &Path) -> Option<Value> {
    let Ok(file) = std::fs::File::open(path) else { return None };
    let mut bytes = Vec::new();
    // Read past the cap so a file that is far too large is never parsed.
    let mut limited = file.take(MAX_FILE_BYTES.saturating_add(1));
    if limited.read_to_end(&mut bytes).is_err() || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return None;
    }
    serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        APP_NAME, APP_VERSION, CAIM_ERROR_MARK, CAIM_HEALTH_MARK, ENDPOINT_VERSION, Health, LAUNCH_LOCK_NAME, LAUNCH_LOCK_WAIT, Launch, LaunchLock,
        LockOutcome, app_path_for, body_still_missing, endpoint_file_from_env, health_from, health_probe, is_ours, launch_decision, launch_lock_path_for,
        published_credentials, published_port, read_reply, remove, temp_path, write, write_app,
    };
    use crate::error::Status;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime};

    fn temp_endpoint(who: &str) -> PathBuf {
        std::env::temp_dir().join(format!("caim-endpoint-{who}-{}.json", std::process::id()))
    }

    struct TempFile(PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn the_file_is_written_whole_and_replaced_on_the_next_start() {
        // Its own folder, so "nothing was left behind" means the rename really cleaned up after itself.
        let folder = std::env::temp_dir().join(format!("caim-endpoint-it-{}-unit", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("the folder is created");
        let path = folder.join("endpoint.json");
        write(&path, 4001, "token-one").expect("the first start writes the file");
        write(&path, 4002, "token-two").expect("the next start replaces it");
        let text = std::fs::read_to_string(&path).expect("the file is readable");
        let value: serde_json::Value = serde_json::from_str(&text).expect("the file is JSON");
        assert_eq!(value.get("version").and_then(serde_json::Value::as_u64), Some(ENDPOINT_VERSION));
        assert_eq!(value.get("port").and_then(serde_json::Value::as_u64), Some(4002));
        assert_eq!(value.get("token").and_then(serde_json::Value::as_str), Some("token-two"));
        let left_behind: Vec<String> = std::fs::read_dir(&folder)
            .expect("the folder is readable")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "endpoint.json")
            .collect();
        assert!(left_behind.is_empty(), "no temp file is left behind: {left_behind:?}");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn removing_leaves_another_runs_file_alone() {
        let path = temp_endpoint("remove");
        let _guard = TempFile(path.clone());
        write(&path, 4003, "mine").expect("the file is written");
        assert!(is_ours(&path, "mine"));
        assert!(!is_ours(&path, "someone-else"));
        remove(&path, "someone-else").expect("another run's file is left alone");
        assert!(path.exists(), "another run's file is not ours to delete");
        remove(&path, "mine").expect("our own file goes");
        assert!(!path.exists());
        remove(&path, "mine").expect("removing a file that is already gone is fine");
    }

    #[test]
    fn temp_names_stay_next_to_the_target() {
        let path = PathBuf::from("some").join("where").join("endpoint.json");
        let temp = temp_path(&path);
        assert_eq!(temp.parent(), path.parent());
        assert!(temp.file_name().unwrap_or_default().to_string_lossy().starts_with("endpoint.json.tmp-"));
    }

    #[test]
    fn the_endpoint_file_is_named_endpoint_json() {
        if let Some(path) = endpoint_file_from_env() {
            assert_eq!(path.file_name().unwrap_or_default(), "endpoint.json");
        }
    }

    /// The MG-01 review follow-up: on Linux and macOS another local user must not be able to read
    /// the token. Windows is covered by the %LOCALAPPDATA% ACL rather than by a mode.
    #[cfg(unix)]
    #[test]
    fn the_endpoint_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_endpoint("mode");
        let _guard = TempFile(path.clone());
        write(&path, 4004, "mode-token").expect("the file is written");
        let mode = std::fs::metadata(&path).expect("the file is there").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the endpoint file is owner-read-write only, got {mode:o}");
    }

    /// The temp file is private too, so there is no moment when the token is exposed.
    #[cfg(unix)]
    #[test]
    fn the_temp_file_before_the_rename_is_private_too() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_endpoint("mode-temp");
        let _guard = TempFile(path.clone());
        let mut file = super::create_private(&path).expect("the temp file is created");
        std::io::Write::write_all(&mut file, b"token").expect("the temp file is written");
        drop(file);
        let mode = std::fs::metadata(&path).expect("the file is there").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the temp file is owner-read-write only, got {mode:o}");
    }

    #[test]
    fn published_port_reads_the_port_and_refuses_a_bad_file() {
        let path = temp_endpoint("port");
        let _guard = TempFile(path.clone());
        write(&path, 4004, "mine").expect("the file is written");
        assert_eq!(published_port(&path), Some(4004), "the port the manager published");
        std::fs::write(&path, r#"{"version": 1, "token": "mine"}"#).expect("a file naming no port can be written");
        assert_eq!(published_port(&path), None, "a file that names no port publishes nothing");
        std::fs::write(&path, r#"{"port": 99999999999}"#).expect("an absurd port can be written");
        assert_eq!(published_port(&path), None, "a number that is not a port is refused");
        std::fs::write(&path, "not json at all").expect("a file that is not JSON can be written");
        assert_eq!(published_port(&path), None, "and so is a file that is not JSON");
        assert_eq!(published_port(&path.with_file_name("not-there.json")), None, "and a file that is not there");
    }

    #[test]
    fn the_app_file_names_the_executable_and_survives_shutdown() {
        let folder = std::env::temp_dir().join(format!("caim-app-it-{}-unit", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("the folder is created");
        let endpoint = folder.join("endpoint.json");
        let app = app_path_for(&endpoint);
        assert_eq!(app, folder.join(APP_NAME), "the app file sits next to the endpoint file");
        write(&endpoint, 4010, "mine").expect("the endpoint file is written");
        write_app(&app, std::path::Path::new("C:/CraftAI/caim.exe")).expect("the app file is written");
        write_app(&app, std::path::Path::new("C:/CraftAI/next/caim.exe")).expect("and the next start replaces it");
        let text = std::fs::read_to_string(&app).expect("the app file is readable");
        let value: serde_json::Value = serde_json::from_str(&text).expect("the app file is JSON");
        assert_eq!(value.get("version").and_then(serde_json::Value::as_u64), Some(APP_VERSION));
        assert_eq!(value.get("exe").and_then(serde_json::Value::as_str), Some("C:/CraftAI/next/caim.exe"));
        remove(&endpoint, "mine").expect("a clean shutdown takes the endpoint file");
        assert!(!endpoint.exists(), "the endpoint file goes when the manager stops");
        assert!(app.is_file(), "the app file stays: it says where the program is, not that the program runs");
        let left_behind: Vec<String> =
            std::fs::read_dir(&folder).expect("the folder is readable").flatten().map(|entry| entry.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(left_behind, vec![APP_NAME.to_string()], "no temp file is left behind: {left_behind:?}");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_app_file_gets_a_temp_name_of_its_own() {
        let path = PathBuf::from("some").join("where").join(APP_NAME);
        let temp = temp_path(&path);
        assert_eq!(temp.parent(), path.parent());
        assert!(
            temp.file_name().unwrap_or_default().to_string_lossy().starts_with("app.json.tmp-"),
            "the temp name hangs off the file being written, not off endpoint.json"
        );
    }

    #[test]
    fn credentials_need_both_a_port_and_a_token() {
        let path = temp_endpoint("credentials");
        let _guard = TempFile(path.clone());
        write(&path, 4011, "both").expect("the file is written");
        assert_eq!(published_credentials(&path), Some((4011, "both".to_string())), "the pair a working manager published");
        std::fs::write(&path, r#"{"version": 1, "port": 4011, "token": ""}"#).expect("a file naming an empty token can be written");
        assert_eq!(published_credentials(&path), None, "an empty token is not a credential");
        std::fs::write(&path, r#"{"version": 1, "token": "no-port"}"#).expect("a file naming no port can be written");
        assert_eq!(published_credentials(&path), None, "and neither is a file that names no port");
    }

    #[test]
    fn a_port_nobody_is_listening_on_is_not_a_running_manager() {
        // Bind a port and let it go, so the port is one this test knows is shut.
        let port = {
            let listener = std::net::TcpListener::bind(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0))).expect("a free port");
            listener.local_addr().expect("the address of a bound port").port()
        };
        assert_eq!(health_probe(port, "token"), Health::Absent, "a shut port answers nothing");
        assert_eq!(health_probe(port, "tok\r\nen"), Health::Absent, "a token that could write its own headers is refused");
        assert_eq!(launch_decision(None), Launch::Start, "no endpoint file means start one");
        let path = temp_endpoint("dead");
        let _guard = TempFile(path.clone());
        assert_eq!(launch_decision(Some(&path)), Launch::Start, "a file that is not there means start one");
        write(&path, port, "gone").expect("a file naming a shut port can be written");
        assert_eq!(launch_decision(Some(&path)), Launch::Start, "and so does a file naming a port nobody answers on");
    }

    #[test]
    fn a_health_answer_that_arrives_in_pieces_is_still_one_manager() {
        // A single read can legally hand back half a status line. Deciding on that fragment would
        // start a second manager on top of the one that is answering, and overwrite its file.
        let listener = std::net::TcpListener::bind(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0))).expect("a free port");
        let port = listener.local_addr().expect("the address of a bound port").port();
        let server = std::thread::spawn(move || {
            let Ok((mut client, _)) = listener.accept() else { return };
            let mut request = [0u8; 512];
            let _ = client.read(&mut request);
            let body = crate::protocol::health(&format!("caim/{}", crate::VERSION));
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let (first, rest) = head.split_at(9);
            // The status line arrives in two writes, with a pause between them.
            let _ = client.write_all(first.as_bytes());
            let _ = client.flush();
            std::thread::sleep(std::time::Duration::from_millis(60));
            let _ = client.write_all(rest.as_bytes());
            let _ = client.write_all(&body);
            let _ = client.flush();
        });
        assert_eq!(health_probe(port, "token"), Health::Ready, "a 200 split across reads is still a manager answering");
        server.join().expect("the fake manager finished");
    }
    #[test]
    fn a_health_body_that_arrives_in_pieces_is_still_this_program() {
        // The manager writes its headers and its body as two writes (http.rs:334-335), and TCP splits
        // them again however it likes, so the body can arrive over several reads. Deciding after a
        // fixed number of reads calls the first piece of a live manager's body a stranger's whole
        // answer, and starts a second manager on top of the one that is running.
        let body = String::from_utf8(crate::protocol::health(&format!("caim/{}", crate::VERSION))).expect("the health answer is UTF-8");
        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let (first, rest) = body.split_at(5);
        assert!(!first.contains(CAIM_HEALTH_MARK), "the test needs a first piece that is not yet the whole answer");
        let mut pieces = Pieces::new(&[head.as_str(), first, rest]);
        let reply = read_reply(&mut pieces, Instant::now() + Duration::from_secs(5));
        assert_eq!(health_from(&reply), Health::Ready, "a 200 whose body arrives in pieces is still this program answering");
    }

    #[test]
    fn a_busy_answer_whose_body_arrives_in_pieces_is_still_this_program() {
        // The same for the 503 a busy manager answers with: its own sentence is what says the port is
        // Craft AI Manager's, and that sentence can arrive in pieces too.
        let body = r#"{"error":"Craft AI Manager already has 8 requests open, so it cannot take another one yet."}"#;
        let head = format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let (first, rest) = body.split_at(12);
        assert!(!first.contains(CAIM_ERROR_MARK), "the test needs a first piece that is not yet the whole answer");
        let mut pieces = Pieces::new(&[head.as_str(), first, rest]);
        let reply = read_reply(&mut pieces, Instant::now() + Duration::from_secs(5));
        assert_eq!(health_from(&reply), Health::Busy, "a 503 whose body arrives in pieces is still a busy manager, not a stranger");
    }

    #[test]
    fn a_stranger_answer_is_read_to_its_end_and_not_to_the_deadline() {
        // The other side of the same rule: reading a reply to its end is not waiting about for a
        // manager that is not there. The stranger's page is complete, so the reply is over at its last
        // promised byte - the reader is never asked for the piece after it - and the port is free.
        let body = "<html><head><title>It works</title></head><body>every path answers 200 here</body></html>";
        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let (first, rest) = body.split_at(20);
        let (second, third) = rest.split_at(rest.len() / 2);
        let mut pieces = Pieces::new(&[head.as_str(), first, second, third, "bytes nobody asked for"]);
        let reply = read_reply(&mut pieces, Instant::now() + Duration::from_secs(5));
        assert_eq!(health_from(&reply), Health::Absent, "a 200 that is not our health answer is still a stranger on the port");
        assert_eq!(pieces.handed, 4, "the reply was over at its last promised byte: the reader was never asked for the piece after it");
    }

    /// A reader that hands back one piece per call, the way a socket splits a reply, and then the
    /// hang-up a `Connection: close` answer ends with. `handed` counts the calls, so a test can ask
    /// whether the reader was pushed past the end of a reply that was already over.
    struct Pieces {
        parts: Vec<Vec<u8>>,
        handed: usize,
    }

    impl Pieces {
        fn new(parts: &[&str]) -> Self {
            Self { parts: parts.iter().map(|part| part.as_bytes().to_vec()).collect(), handed: 0 }
        }
    }

    impl std::io::Read for Pieces {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.handed >= self.parts.len() {
                return Ok(0);
            }
            let part = std::mem::take(&mut self.parts[self.handed]);
            self.handed += 1;
            let take = part.len().min(buffer.len());
            buffer[..take].copy_from_slice(&part[..take]);
            Ok(take)
        }
    }

    #[test]
    fn a_reply_is_only_over_when_the_body_it_declared_has_arrived() {
        // The stopping rule of the read loop on its own: headers still arriving is not an answer, a
        // reply that declares no length ends at the hang-up, and a body is over at its last promised
        // byte.
        assert_eq!(body_still_missing(b"HTTP/1.1 200 OK\r\nContent-Len"), None, "the headers are not over yet");
        assert_eq!(
            body_still_missing(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n<html>"),
            None,
            "a reply with no declared length ends when the peer hangs up"
        );
        assert_eq!(body_still_missing(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc"), Some(2), "three of five body bytes");
        assert_eq!(body_still_missing(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde"), Some(0), "and the whole body is over");
        assert_eq!(body_still_missing(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcdefgh"), Some(0), "more bytes than promised is still a finished answer");
        assert_eq!(body_still_missing(b"HTTP/1.1 200 OK\r\nContent-Length: nine\r\n\r\n"), None, "a length that is not a number promises nothing");
    }

    #[test]
    fn a_manager_with_no_free_connection_slot_is_still_running() {
        // The accept loop answers 503 before it even routes a request, when every connection slot
        // is taken. Reading that as "no manager" would start a second one over the first one's file.
        let listener = std::net::TcpListener::bind(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0))).expect("a free port");
        let port = listener.local_addr().expect("the address of a bound port").port();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut client, _)) = listener.accept() else { return };
                let mut request = [0u8; 512];
                let _ = client.read(&mut request);
                let body = r#"{"error":"Craft AI Manager already has 8 requests open, so it cannot take another one yet."}"#;
                let reply = format!(
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = client.write_all(reply.as_bytes());
                let _ = client.flush();
            }
        });
        assert_eq!(health_probe(port, "token"), Health::Busy, "a 503 is a manager with no free slot, not one that has gone away");
        let path = temp_endpoint("busy");
        let _guard = TempFile(path.clone());
        write(&path, port, "token").expect("a file naming a busy manager can be written");
        assert_eq!(launch_decision(Some(&path)), Launch::AlreadyRunning { port }, "and the launch decision agrees, so nothing is started");
        server.join().expect("the fake manager finished");
    }

    #[test]
    fn a_stranger_answer_503_on_the_port_is_not_the_running_manager() {
        // A manager crashed and left its endpoint file behind, and something else - a dev server,
        // a proxy - has taken the port. Its 503 says nothing about Craft AI Manager, so reading it
        // as "a manager is running" would have caim open report a manager that is not there.
        let listener = std::net::TcpListener::bind(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0))).expect("a free port");
        let port = listener.local_addr().expect("the address of a bound port").port();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut client, _)) = listener.accept() else { return };
                let mut request = [0u8; 512];
                let _ = client.read(&mut request);
                let body = "<html><head><title>503 Service Unavailable</title></head><body>the server next door is busy</body></html>";
                let reply =
                    format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = client.write_all(reply.as_bytes());
                let _ = client.flush();
            }
        });
        assert_eq!(health_probe(port, "token"), Health::Absent, "a 503 that is not ours is a stranger on the port, not a manager");
        let path = temp_endpoint("stranger");
        let _guard = TempFile(path.clone());
        write(&path, port, "token").expect("a file naming a stranger port can be written");
        assert_eq!(launch_decision(Some(&path)), Launch::Start, "so a manager may start and take the port back");
        server.join().expect("the fake stranger finished");
    }

    #[test]
    fn a_stranger_answer_200_on_the_port_is_not_the_running_manager() {
        // The same crashed manager and the same taken port, but this stranger answers every path with
        // a page. Its 200 is not a health answer, so a status-line check would report a manager that
        // is not there and start nothing.
        let listener = std::net::TcpListener::bind(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0))).expect("a free port");
        let port = listener.local_addr().expect("the address of a bound port").port();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut client, _)) = listener.accept() else { return };
                let mut request = [0u8; 512];
                let _ = client.read(&mut request);
                let body = "<html><head><title>It works</title></head><body>every path answers 200 here</body></html>";
                let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = client.write_all(reply.as_bytes());
                let _ = client.flush();
            }
        });
        assert_eq!(health_probe(port, "token"), Health::Absent, "a 200 that is not our health answer is a stranger on the port, not a manager");
        let path = temp_endpoint("stranger-200");
        let _guard = TempFile(path.clone());
        write(&path, port, "token").expect("a file naming a stranger port can be written");
        assert_eq!(launch_decision(Some(&path)), Launch::Start, "so a manager may start and take the port back");
        server.join().expect("the fake stranger finished");
    }

    #[test]
    fn a_cut_off_200_answer_does_not_start_a_second_manager() {
        // The mirror of the cut-off 503: a reply that never finishes its headers is far more likely a
        // slow manager than a stranger, and the worse mistake is starting a second one over a live one.
        let listener = std::net::TcpListener::bind(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0))).expect("a free port");
        let port = listener.local_addr().expect("the address of a bound port").port();
        let server = std::thread::spawn(move || {
            let Ok((mut client, _)) = listener.accept() else { return };
            let mut request = [0u8; 512];
            let _ = client.read(&mut request);
            let _ = client.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n");
            let _ = client.flush();
            std::thread::sleep(std::time::Duration::from_millis(400));
        });
        assert_eq!(health_probe(port, "token"), Health::Ready, "a 200 whose body never arrived is still a manager that is running");
        server.join().expect("the fake manager finished");
    }

    #[test]
    fn a_cut_off_503_answer_does_not_start_a_second_manager() {
        // A busy manager writes its headers and body in one pass, so a reply that stops after the
        // status line is far more likely a slow manager than a stranger. Starting a second manager
        // over a live one is the worse mistake, so an unfinished 503 still counts as running.
        let listener = std::net::TcpListener::bind(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0))).expect("a free port");
        let port = listener.local_addr().expect("the address of a bound port").port();
        let server = std::thread::spawn(move || {
            let Ok((mut client, _)) = listener.accept() else { return };
            let mut request = [0u8; 512];
            let _ = client.read(&mut request);
            let _ = client.write_all(b"HTTP/1.1 503 Service Unavailable\r\n");
            let _ = client.flush();
            std::thread::sleep(std::time::Duration::from_millis(400));
        });
        assert_eq!(health_probe(port, "token"), Health::Busy, "a 503 whose body never arrived is still a manager that is running");
        server.join().expect("the fake manager finished");
    }

    #[test]
    fn one_launch_lock_at_a_time_and_a_clean_hand_over() {
        let folder = lock_folder("hand-over");
        let path = launch_lock_path_for(&folder.join("endpoint.json"));
        assert_eq!(path.file_name().expect("a file name").to_string_lossy(), LAUNCH_LOCK_NAME, "the lock sits beside the endpoint file");
        let first = take_lock(&path);
        // The first run lets go as soon as its server is listening, so the second run waits a
        // moment and then decides with its eyes open rather than starting a manager of its own.
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            drop(first);
        });
        let second = take_lock(&path);
        holder.join().expect("the first run let go");
        drop(second);
        assert_only_the_lock_is_left(&folder);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_launch_lock_another_run_is_holding_is_not_taken_over() {
        // Waiting out the window and giving up is the honest answer. Taking a lock another run is
        // holding would start a second manager, which is the very thing the lock exists to stop.
        let folder = lock_folder("held");
        let path = launch_lock_path_for(&folder.join("endpoint.json"));
        let held = take_lock(&path);
        let started = Instant::now();
        assert!(matches!(LaunchLock::take(&path), LockOutcome::Held), "a second run gives up rather than starting its own");
        assert!(started.elapsed() >= LAUNCH_LOCK_WAIT, "it waits for the whole window before giving up");
        drop(held);
        assert!(matches!(LaunchLock::take(&path), LockOutcome::Taken(_)), "and the lock is free again once the first run is done");
        assert_only_the_lock_is_left(&folder);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_launch_lock_whose_owner_has_ended_is_taken_over_at_once() {
        // What a `caim open` that died between taking the lock and starting a server leaves behind:
        // the file, with the operating system's lock on it already gone. Nothing here is made old on
        // purpose, because the takeover is about the owner being gone and not about the clock.
        let folder = lock_folder("dead-owner");
        let path = launch_lock_path_for(&folder.join("endpoint.json"));
        leave_a_dead_owner_lock(&path);
        assert_eq!(owner_id_in_file(&path), Some(424242), "the lock a dead run left behind still names it");
        let started = Instant::now();
        let taken = take_lock(&path);
        assert!(started.elapsed() < LAUNCH_LOCK_WAIT, "a lock with no owner is taken over at once, not after waiting out the window a live owner gets");
        assert_eq!(taken.owner_id(), Some(std::process::id()), "and the lock now names the run that took it over");
        drop(taken);
        assert_only_the_lock_is_left(&folder);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_launch_lock_a_live_run_holds_is_not_taken_over_however_old_the_file_is() {
        // The rule this replaces took a lock whose owner was merely slow. Age is not the question:
        // while the owner is alive the operating system's lock is held, so an hour-old file that is
        // still being held stays taken, and the run that wants it waits and then says so.
        let folder = lock_folder("old-but-alive");
        let path = launch_lock_path_for(&folder.join("endpoint.json"));
        let held = take_lock(&path);
        assert_eq!(held.owner_id(), Some(std::process::id()), "the lock says who is holding it");
        let file = std::fs::File::options().write(true).open(&path).expect("the lock file can be opened");
        file.set_modified(SystemTime::now() - Duration::from_secs(60 * 60)).expect("the lock can be made an hour old");
        drop(file);
        assert!(matches!(LaunchLock::take(&path), LockOutcome::Held), "an hour old is still somebody else's lock");
        assert!(path.is_file(), "and the run that wanted it did not remove it");
        assert_eq!(held.owner_id(), Some(std::process::id()), "still naming the run that is holding it");
        drop(held);
        assert!(matches!(LaunchLock::take(&path), LockOutcome::Taken(_)), "the lock is free the moment its owner is gone");
        assert_only_the_lock_is_left(&folder);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_launch_lock_handed_over_under_a_waiter_still_has_one_holder() {
        // The shape that used to slip through (D17, option 1): one run holds the lock, a second run
        // has the file open but has not managed to lock it, and a third asks for the lock the normal
        // way. If letting go unlinked the file, the waiter could lock the inode it already had while
        // the third run made a fresh file under the same name, and both would be told they may start
        // a manager. Because the lock keeps its name, all three are looking at one inode.
        let folder = lock_folder("one-inode");
        let path = launch_lock_path_for(&folder.join("endpoint.json"));
        let first = take_lock(&path);
        let waiter = std::fs::OpenOptions::new().read(true).write(true).open(&path).expect("a waiting run can open the lock file");
        assert!(matches!(waiter.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "the run that holds it keeps the waiter out");
        drop(first);
        waiter.try_lock().expect("the lock is free the moment its owner is gone, through the handle the waiter already had");
        let third = LaunchLock::take(&path);
        assert!(matches!(third, LockOutcome::Held), "the third run finds the inode the waiter is holding, not a new file under the same name");
        drop(waiter);
        let fourth = take_lock(&path);
        assert_eq!(fourth.owner_id(), Some(std::process::id()), "and the same lock is there for the next run to take over");
        // The same question from another thread while that run holds it: still exactly one holder.
        let concurrent = {
            let path = path.clone();
            std::thread::spawn(move || matches!(LaunchLock::take(&path), LockOutcome::Taken(_)))
        };
        assert!(!concurrent.join().expect("the other run finished deciding"), "a run asking from another thread waits rather than taking a held lock");
        drop(fourth);
        assert!(path.is_file(), "letting go never unlinks the launch lock, so the name is still there for the next run");
        assert_only_the_lock_is_left(&folder);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_launch_lock_that_cannot_be_made_is_a_failure_not_another_run() {
        // Something is sitting where the lock's folder belongs, so the folder cannot be made. That is
        // a broken machine, not another `caim open` holding the lock: the run has to say so and fail
        // rather than exit 0 promising a manager that nobody is starting.
        let folder = lock_folder("no-folder");
        let blocker = folder.join("not-a-folder");
        std::fs::write(&blocker, "an ordinary file, where a folder belongs").expect("a file can sit where the folder belongs");
        let path = launch_lock_path_for(&blocker.join("endpoint.json"));
        let outcome = LaunchLock::take(&path);
        let LockOutcome::Failed(fail) = outcome else {
            panic!("a lock that cannot be made is a failure, not contention or a lock we now hold");
        };
        assert_eq!(fail.status, Status::Internal, "so `caim open` reports it and exits nonzero");
        assert!(!path.exists(), "and nothing was left behind where the lock would have been");
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The lock a test wants, or a loud failure: these tests only ever expect to take it.
    fn take_lock(path: &Path) -> LaunchLock {
        match LaunchLock::take(path) {
            LockOutcome::Taken(lock) => lock,
            LockOutcome::Held => panic!("another run is holding the launch lock"),
            LockOutcome::Failed(fail) => panic!("the launch lock could not be taken: {}", fail.sentence),
        }
    }

    /// The process id an unlocked launch lock file names, for the case where its owner has ended.
    fn owner_id_in_file(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.lines().nth(1)?.parse().ok()
    }

    /// A lock left by a run that has ended: the file is there, and the operating system's lock on it
    /// is free, which is exactly what a process exit leaves behind. The handle closes, the lock goes
    /// with it, and the file stays.
    fn leave_a_dead_owner_lock(path: &Path) {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).expect("a launch lock can be left behind");
        file.write_all(b"the nonce of a run that died partway through starting\n424242\n").expect("the lock can say who wrote it");
        file.try_lock().expect("the run that wrote the lock holds it");
        drop(file);
    }

    /// A folder of the test's own for one launch lock.
    fn lock_folder(who: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("caim-endpoint-{}-{who}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("a folder for the lock");
        folder
    }

    /// A released lock leaves its folder holding the lock file and nothing else. The lock keeps its
    /// name on purpose: every run has to open and lock the same inode, so a run that already had the
    /// file open can never be left locking an orphan while a fresh file appears under the name.
    fn assert_only_the_lock_is_left(folder: &Path) {
        let left_behind: Vec<String> =
            std::fs::read_dir(folder).expect("the folder is readable").flatten().map(|entry| entry.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(left_behind, vec![LAUNCH_LOCK_NAME.to_string()], "a released lock leaves the lock file and nothing else: {left_behind:?}");
    }
}
