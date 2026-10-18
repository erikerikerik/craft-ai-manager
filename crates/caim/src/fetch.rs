//! Getting a model file, and the rules that keep a bad one out of the store.
//!
//! SPEC v1.1 amendment 14 and PhotoCraft FEATURES row CAIM-03: download with a progress report, hash
//! it while it streams, and refuse anything that does not match the catalog. Amendment 18 adds
//! restart-safe resume: a half-download keeps its bytes and a note saying what they are meant to
//! become, and the next install re-hashes them and asks the server for the rest.
//!
//! Everything here goes through the [Fetch](Fetch) trait, so the tests never touch the network: they
//! hand over bytes and count calls. A model that is already installed makes no request at all.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::catalog::Model;
use crate::error::{Fail, Result};
use crate::store;

/// How much is read, hashed and written at a time.
const CHUNK: usize = 64 * 1024;
/// Redirects are followed - Hugging Face sends the download to a CDN - but never to plain HTTP.
const MAX_REDIRECTS: u32 = 10;
/// How long to wait for the connection to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the server's headers. The body has no timeout: it is 200 MB.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a download refreshes its lock, which is how another install can tell it is alive.
const LOCK_HEARTBEAT: Duration = Duration::from_secs(5);
/// A lock nobody has refreshed for this long was left by an install that died (Ctrl+C, a crash).
pub const LOCK_STALE: Duration = Duration::from_secs(120);

/// How much of the file the server sent back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The whole file, from byte zero - either because nothing was asked for, or because the server
    /// ignored the range and started us over.
    Full,
    /// The bytes from the offset that was asked for, which is a `206`.
    Partial,
}

/// An open download: what the server says the length is, when it says, how much of it arrived, and
/// the bytes.
pub struct Stream {
    /// The server's own `Content-Length`, if it sent one.
    pub claimed_len: Option<u64>,
    /// The whole file, or the part that was asked for.
    pub answer: Answer,
    /// The body.
    pub reader: Box<dyn Read>,
}

impl Stream {
    /// A stream holding the whole file.
    #[must_use]
    pub fn full(claimed_len: Option<u64>, reader: Box<dyn Read>) -> Self {
        Self { claimed_len, answer: Answer::Full, reader }
    }

    /// A stream holding the bytes from an offset on.
    #[must_use]
    pub fn partial(claimed_len: Option<u64>, reader: Box<dyn Read>) -> Self {
        Self { claimed_len, answer: Answer::Partial, reader }
    }
}

/// How CAIM opens a URL. Kept this small so the tests can hand over a byte slice instead of a
/// network, and so MG-05 can add cancellation in one place.
///
/// Implementations must not follow a redirect to plain HTTP.
pub trait Fetch {
    /// Open `url` for reading.
    fn open(&self, url: &str) -> Result<Stream>;

    /// Open `url` for reading, asking for the bytes from `offset` on. An implementation that cannot
    /// ask for a range opens the whole file, and says so with [Answer::Full].
    fn open_range(&self, url: &str, _offset: u64) -> Result<Stream> {
        self.open(url)
    }
}

/// The real [Fetch](Fetch): HTTPS through `ureq` with rustls, no async runtime, redirects followed
/// but never down to plain HTTP.
pub struct HttpFetch {
    agent: ureq::Agent,
}

impl Default for HttpFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpFetch {
    /// An agent for the catalog's URLs: TLS only, redirects allowed but never to HTTP.
    #[must_use]
    pub fn new() -> Self {
        Self { agent: ureq::Agent::new_with_config(agent_config(true)) }
    }

    /// The same agent, but willing to talk plain HTTP. Only for a test server on loopback: every
    /// catalog URL is HTTPS, and [HttpFetch::new] is what the program itself uses.
    #[must_use]
    pub fn allowing_plain_http() -> Self {
        Self { agent: ureq::Agent::new_with_config(agent_config(false)) }
    }
}

/// The one agent configuration, so the plain-HTTP test agent differs from the real one in nothing
/// but `https_only`.
fn agent_config(https_only: bool) -> ureq::config::Config {
    ureq::config::Config::builder()
        .https_only(https_only)
        .max_redirects(MAX_REDIRECTS)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .build()
}

impl Fetch for HttpFetch {
    fn open(&self, url: &str) -> Result<Stream> {
        self.open_range(url, 0)
    }

    fn open_range(&self, url: &str, offset: u64) -> Result<Stream> {
        let request = self.agent.get(url);
        let request = if offset == 0 { request } else { request.header("Range", format!("bytes={offset}-")) };
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status)) => return Err(resume_status_error(url, offset, status)),
            Err(error) => return Err(Fail::internal(format!("Craft AI Manager could not start the download of {url}: {error}"))),
        };
        let answer = resume_answer(url, offset, response.status().as_u16())?;
        let body = response.into_body();
        let claimed_len = body.content_length();
        Ok(Stream { claimed_len, answer, reader: Box::new(body.into_reader()) })
    }
}

/// What an answer to a download means: a `206` continues where the bytes on disk stopped, a `200`
/// starts over from zero, and anything else is an error that keeps the part files.
fn resume_answer(url: &str, offset: u64, status: u16) -> Result<Answer> {
    match status {
        206 => Ok(Answer::Partial),
        200 => Ok(Answer::Full),
        other => Err(resume_status_error(url, offset, other)),
    }
}

/// The sentence for a server that answered a download, or a range of one, with a status that is not
/// a file.
fn resume_status_error(url: &str, offset: u64, status: u16) -> Fail {
    let asked = if offset == 0 { "the file".to_string() } else { format!("the bytes from {offset} on") };
    Fail::internal(format!("{url} answered {status} when Craft AI Manager asked for {asked}."))
}

/// How often the resume note is rewritten while bytes arrive (amendment 18: at least every 8 MiB).
const META_EVERY: u64 = 8 * 1024 * 1024;

/// What an install did.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Nothing was fetched: the file, its size and its verification were already in place.
    AlreadyInstalled,
    /// The file came down, hashed to what the catalog promised, and was verified. `resumed_from` is
    /// how many bytes were already on disk when this install picked the download up, so 0 unless a
    /// restart left something worth continuing.
    Downloaded { sha256: String, bytes: u64, elapsed_ms: u64, resumed_from: u64 },
}

/// What a re-check found.
#[derive(Clone, Debug)]
pub struct Checked {
    pub sha256: String,
    pub bytes: u64,
    pub elapsed_ms: u64,
}

/// Install one model: no request at all when it is already installed, a note that has to be
/// accepted first when the entry is flagged, and otherwise a streaming download that is hashed while
/// it is written and renamed into place only once the hash matches.
///
/// `progress` is called with the bytes read so far and the size the catalog expects.
pub fn install(model: &Model, root: &Path, fetch: &dyn Fetch, progress: &mut dyn FnMut(u64, u64)) -> Result<Outcome> {
    if store::is_installed(root, model) {
        return Ok(Outcome::AlreadyInstalled);
    }
    if !store::is_acknowledged(root, model) {
        let note = model.note.as_ref().map(|entry| entry.note.clone()).unwrap_or_default();
        return Err(Fail::conflict(format!(
            "{} has a note you have to accept before it is downloaded. Read it below, then run the same command with --accept-note. Note: {note}",
            model.label()
        )));
    }
    download(model, root, fetch, progress)
}

/// The download itself, under the install lock: clear away what dead installs left, then fetch.
///
/// C6: the lock (`<file>.lock`) keeps two installs from downloading the same 200 MB at once, but
/// correctness does not rest on it. Each install writes its own `<file>.<random>.part`, created with
/// `create_new`, and only ever renames the bytes it hashed itself, so even two installs that both
/// believe they hold the lock cannot put a wrong file under the verified name.
fn download(model: &Model, root: &Path, fetch: &dyn Fetch, progress: &mut dyn FnMut(u64, u64)) -> Result<Outcome> {
    let folder = store::model_folder(root, model);
    std::fs::create_dir_all(&folder).map_err(|error| Fail::internal(format!("Craft AI Manager could not create {}: {error}", folder.display())))?;
    let mut lock = InstallLock::take(&store::lock_file(root, model), &model.label())?;
    if store::is_installed(root, model) {
        // Another install finished while this one waited for the lock.
        return Ok(Outcome::AlreadyInstalled);
    }
    remove_dead_parts(&folder, model);
    let resume = lock.resume();
    fetch_into_place(model, root, fetch, progress, &mut || lock.heartbeat(), resume)
}

/// Whether this install may pick up a half-download at all, and how sure it can be that nobody else
/// is writing one. [InstallLock::take] either creates the lock file itself, which means no other
/// installer exists, or takes it over from a lock that looks dead but might belong to a stalled
/// install that is still writing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resume {
    /// No install lock: the tests that install without one never adopt another run's bytes.
    NoLock,
    /// This install created the lock file, so no other installer exists. A pair can be adopted
    /// however young it is, which is what makes "run the same command again" pick the bytes up.
    FreshLock,
    /// This install took the lock over from an install that looks dead but might only be stalled,
    /// so only a pair nobody has written to for [LOCK_STALE] may be adopted.
    TakenOver,
}

/// Stream the file into this install's own `.part`, hashed as it arrives; refuse it if the server's
/// length or the bytes read go past the catalog size; sync it to disk; and rename it into place and
/// record it only when the hash matches. A failure that means the bytes are wrong deletes this
/// install's part files; a failure that only means the download stopped keeps them, so the next
/// install can pick the bytes up (SPEC v1.1 amendment 18).
///
/// Amendment 18 writes this down as `<file>.part` plus `<file>.part.json`. The tag is kept on both
/// names here for the reason C6 established: two installs must never write the same half-file, so
/// this one is `<file>.<tag>.part` and its note is `<file>.<tag>.part.json`. A restart finds the
/// pair again by [adopt_part], which renames it under the new install's own tag.
///
/// `resume` says which half-download rule applies: only an install holding the model's [InstallLock]
/// adopts a pair at all, and only a lock this install created itself may adopt one that is still
/// young (see [Resume]).
fn fetch_into_place(
    model: &Model,
    root: &Path,
    fetch: &dyn Fetch,
    progress: &mut dyn FnMut(u64, u64),
    alive: &mut dyn FnMut(),
    resume: Resume,
) -> Result<Outcome> {
    let tag = install_tag()?;
    let part = store::part_file(root, model, &tag);
    let meta = store::part_meta_file(root, model, &tag);
    let started = Instant::now();

    let (mut offset, mut hasher) = match adopt_part(root, model, &part, &meta, resume) {
        Some((bytes, hasher)) => (bytes, hasher),
        None => (0, Sha256::new()),
    };
    if offset > 0 {
        progress(offset, model.size);
    }
    if offset > 0 && offset == model.size {
        // The note counts the whole file: the last run died after its final checkpoint, or lost its
        // connection exactly at the catalog size. Asking the server for `bytes=<size>-` would answer
        // 416 and every retry would repeat it, so these bytes are finished here instead.
        return finalize_part(model, root, &part, &meta, hasher, offset, started);
    }
    let stream = if offset == 0 {
        fetch.open(&model.url)?
    } else {
        match fetch.open_range(&model.url, offset) {
            Ok(stream) => stream,
            // Whatever the server said, the bytes on disk are not the problem, so they stay here
            // for the next try rather than being thrown away.
            Err(fail) => {
                return Err(Fail::internal(format!("{fail} {offset} bytes are on disk, so running the same command again picks the download up.")));
            }
        }
    };
    if offset > 0 && stream.answer == Answer::Full {
        // The server ignored the range request: the bytes on disk cannot be trusted to be the front
        // of this file any more, so start over.
        offset = 0;
        hasher = Sha256::new();
        remove_pair(&part, &meta);
    }
    if let Some(claimed) = stream.claimed_len.filter(|claimed| claimed.saturating_add(offset) > model.size) {
        if offset > 0 {
            remove_pair(&part, &meta);
        }
        return Err(Fail::too_large(format!(
            "{} is {} bytes in the catalog, but {} says it has {claimed}. That is not the file the catalog describes, so nothing was downloaded.",
            model.label(),
            model.size,
            model.url
        )));
    }

    let opened = if offset == 0 { OpenOptions::new().write(true).create_new(true).open(&part) } else { OpenOptions::new().append(true).open(&part) };
    let mut file = match opened {
        Ok(file) => file,
        Err(error) => return Err(Fail::internal(format!("Craft AI Manager could not open {}: {error}", part.display()))),
    };
    write_note(&meta, model, offset);
    let mut next_note_at = offset.saturating_add(META_EVERY);

    let mut reader = stream.reader;
    let mut buffer = vec![0u8; CHUNK];
    let mut total: u64 = offset;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let Some(chunk) = buffer.get(..read) else { break };
                let Some(next) = total.checked_add(read as u64) else {
                    return Err(abandon_pair(&part, &meta, Fail::too_large(format!("{} is more bytes than this machine was asked to store.", model.label()))));
                };
                if next > model.size {
                    return Err(abandon_pair(
                        &part,
                        &meta,
                        Fail::too_large(format!(
                            "{} is {} bytes in the catalog, and the server had sent {next} before it stopped. Nothing was saved.",
                            model.label(),
                            model.size
                        )),
                    ));
                }
                hasher.update(chunk);
                if let Err(error) = file.write_all(chunk) {
                    return Err(abandon_pair(&part, &meta, Fail::internal(format!("Craft AI Manager could not write {}: {error}", part.display()))));
                }
                total = next;
                if total >= next_note_at {
                    write_note(&meta, model, total);
                    next_note_at = total.saturating_add(META_EVERY);
                }
                alive();
                progress(total, model.size);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = file.flush();
                return Err(keep_what_arrived(&meta, model, total, &part, &format!("Craft AI Manager lost the download of {}: {error}", model.url)));
            }
        }
    }

    if let Err(error) = file.flush().and_then(|()| file.sync_all()) {
        return Err(abandon_pair(&part, &meta, Fail::internal(format!("Craft AI Manager could not finish writing {}: {error}", part.display()))));
    }
    drop(file);

    if total != model.size {
        return Err(keep_what_arrived(&meta, model, total, &part, &format!("{} stopped short of the catalog size.", model.label())));
    }
    finalize_part(model, root, &part, &meta, hasher, offset, started)
}

/// The bytes are all there: hash what this install collected and put the file in place only when the
/// hash matches the catalog. Both a download that just arrived and a resumed part that was already
/// whole when this install found it end here, and in both cases `total` is the catalog size.
fn finalize_part(model: &Model, root: &Path, part: &Path, meta: &Path, hasher: Sha256, resumed_from: u64, started: Instant) -> Result<Outcome> {
    let total = model.size;
    let actual = store::hex(&hasher.finalize());
    if actual != model.sha256 {
        return Err(abandon_pair(
            part,
            meta,
            Fail::internal(format!(
                "The file downloaded for {} has SHA-256 {actual}, not the {} the catalog expects. Nothing was saved.",
                model.label(),
                model.sha256
            )),
        ));
    }
    let target = store::model_file(root, model);
    // The `.part` is this install's alone, so the bytes renamed here are the bytes just hashed.
    if let Err(error) = std::fs::rename(part, &target) {
        return Err(abandon_pair(part, meta, Fail::internal(format!("Craft AI Manager could not put {} in place: {error}", target.display()))));
    }
    let _ = std::fs::remove_file(meta);
    store::write_verified(root, model, &actual, total)?;
    Ok(Outcome::Downloaded { sha256: actual, bytes: total, elapsed_ms: millis_since(started), resumed_from })
}

/// A short random tag that tells this install's files from any other's.
fn install_tag() -> Result<String> {
    let token = crate::token::random_token()?;
    Ok(token.get(..16).unwrap_or(token.as_str()).to_string())
}

/// Delete the `.part` files that installs which died left behind: a live install writes to its own
/// file every chunk, so one nobody has written to for [LOCK_STALE] is not being downloaded any more.
/// (If a download did stall that long and comes back, its rename finds no file and it fails
/// cleanly; on Windows the file is still open, so the delete simply does not happen.)
///
/// A `.part` with a resume note next to it is not junk: it is a download the next install can pick
/// up, so it stays for [adopt_part] and only its orphaned note is cleared away.
fn remove_dead_parts(folder: &Path, model: &Model) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        if !is_stale(&path) {
            continue;
        }
        if store::is_part_name(model, &name) {
            if !meta_for_part(&path).exists() {
                let _ = std::fs::remove_file(&path);
            }
        } else if store::is_part_meta_name(model, &name) && !part_for_meta(&path).exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Pick up a half-download nobody is writing to any more: put its bytes under this install's own tag
/// (renamed when this install created the lock, copied when it took the lock over - see
/// [copy_prefix_and_hash]), re-hash them so the hash can carry on where the last run left off, and
/// hand back how many bytes there are. `None` means start from zero, which is what happens
/// when the note does not describe this exact file - a different revision or SHA-256 in the note
/// means delete both and start over (amendment 18).
fn adopt_part(root: &Path, model: &Model, part: &Path, meta: &Path, resume: Resume) -> Option<(u64, Sha256)> {
    if resume == Resume::NoLock {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(store::model_folder(root, model)) else { return None };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !store::is_part_meta_name(model, &name.to_string_lossy()) {
            continue;
        }
        let found_meta = entry.path();
        let found_part = part_for_meta(&found_meta);
        // Under a takeover the install that held the lock may still be alive and writing, so both
        // halves have to be old before this one takes them. A lock this install created itself has
        // no other installer at all, so the pair is adopted however young it is.
        if resume == Resume::TakenOver && (!is_stale(&found_meta) || !is_stale(&found_part)) {
            continue;
        }
        let Some(note) = store::read_part_meta(&found_meta) else {
            remove_pair(&found_part, &found_meta);
            continue;
        };
        if note.url != model.url || note.revision != model.revision || note.sha256 != model.sha256 || note.bytes == 0 || note.bytes > model.size {
            remove_pair(&found_part, &found_meta);
            continue;
        }
        let Some(written) = std::fs::metadata(&found_part).ok().filter(|metadata| metadata.is_file()).map(|metadata| metadata.len()) else {
            remove_pair(&found_part, &found_meta);
            continue;
        };
        // A run killed between two checkpoints normally leaves more bytes than its note counts. The
        // tail past the checkpoint is not covered by the note, so it is cut away and the recorded
        // prefix is what resumes; only a file shorter than its note is unusable.
        if written < note.bytes {
            remove_pair(&found_part, &found_meta);
            continue;
        }
        if resume == Resume::TakenOver {
            // The install that held the lock might only be stalled, and renaming its `.part` would
            // leave it writing into the very file this install is about to verify - on Unix the
            // rename keeps the same inode. So the recorded prefix is copied instead: those bytes
            // were written at least [LOCK_STALE] ago, and the copy is this install's alone.
            let Some(hasher) = copy_prefix_and_hash(&found_part, part, note.bytes) else {
                remove_pair(part, meta);
                continue;
            };
            // The old pair is no longer this install's business. An owner still holding the file open
            // keeps its own copy, which is the point.
            let _ = std::fs::remove_file(&found_part);
            let _ = std::fs::remove_file(&found_meta);
            write_note(meta, model, note.bytes);
            return Some((note.bytes, hasher));
        }
        if written > note.bytes && !truncate_to(&found_part, note.bytes) {
            remove_pair(&found_part, &found_meta);
            continue;
        }
        if std::fs::rename(&found_part, part).is_err() || std::fs::rename(&found_meta, meta).is_err() {
            let _ = std::fs::remove_file(part);
            continue;
        }
        let mut hasher = Sha256::new();
        // The file is exactly the bytes the note counts by now, so hashing all of it hashes exactly
        // the recorded prefix.
        if !hash_into(part, &mut hasher) {
            remove_pair(part, meta);
            return None;
        }
        return Some((note.bytes, hasher));
    }
    None
}

/// Copy the first `bytes` of another install's `.part` into this install's own `.part`, hashing them
/// on the way, so a lock takeover never ends up writing into - or verifying - a file some other
/// process may still be appending to. `None` when either file could not be used, and whatever was
/// copied then gets no further use.
fn copy_prefix_and_hash(from: &Path, to: &Path, bytes: u64) -> Option<Sha256> {
    let mut reader = std::fs::File::open(from).ok()?;
    let mut writer = OpenOptions::new().write(true).create_new(true).open(to).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut copied: u64 = 0;
    while copied < bytes {
        let room = usize::try_from(bytes - copied).unwrap_or(buffer.len()).min(buffer.len());
        match reader.read(&mut buffer[..room]) {
            // Fewer bytes than the note counts are here now than were there when the note was
            // written, so nothing on disk can be trusted.
            Ok(0) => return None,
            Ok(read) => {
                let chunk = buffer.get(..read)?;
                hasher.update(chunk);
                writer.write_all(chunk).ok()?;
                copied += read as u64;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    writer.flush().ok()?;
    writer.sync_all().ok()?;
    Some(hasher)
}

/// Feed a file's bytes to `hasher`, so a resumed download continues its SHA-256 where the last run
/// stopped. [adopt_part] has already cut the file to the number of bytes its note counts. False when
/// the file could not be read.
fn hash_into(path: &Path, hasher: &mut Sha256) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return true,
            Ok(read) => {
                let Some(chunk) = buffer.get(..read) else { return false };
                hasher.update(chunk);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
}

/// Write the resume note: what these bytes are meant to become, and how many of them there are. Best
/// effort - a note that cannot be written only costs the bytes after this point, never the download.
fn write_note(meta: &Path, model: &Model, bytes: u64) {
    let note = store::PartMeta { url: model.url.clone(), revision: model.revision.clone(), sha256: model.sha256.clone(), bytes };
    if let Err(error) = store::write_part_meta(meta, &note) {
        eprintln!("{}", error.sentence);
    }
}

/// The `.part` a resume note belongs to.
fn part_for_meta(meta: &Path) -> PathBuf {
    renamed(meta, store::PART_META_SUFFIX, store::PART_SUFFIX)
}

/// The resume note that belongs to a `.part`.
fn meta_for_part(part: &Path) -> PathBuf {
    renamed(part, store::PART_SUFFIX, store::PART_META_SUFFIX)
}

/// The same path with one ending swapped for another.
fn renamed(path: &Path, from: &str, to: &str) -> PathBuf {
    let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
    path.with_file_name(name.strip_suffix(from).map(|stem| format!("{stem}{to}")).unwrap_or(name))
}

/// Cut a `.part` back to the number of bytes its note counts, so bytes no note accounts for are
/// never mistaken for bytes that were checked. False when the file cannot be opened or cut.
fn truncate_to(path: &Path, bytes: u64) -> bool {
    match OpenOptions::new().write(true).open(path) {
        Ok(file) => file.set_len(bytes).is_ok(),
        Err(_) => false,
    }
}

/// Delete a half-download and its note.
fn remove_pair(part: &Path, meta: &Path) {
    let _ = std::fs::remove_file(part);
    let _ = std::fs::remove_file(meta);
}

/// The download stopped, and the bytes are not the problem: keep the part and its note, and say what
/// is waiting for the next try. Nothing is kept when nothing arrived.
fn keep_what_arrived(meta: &Path, model: &Model, total: u64, part: &Path, why: &str) -> Fail {
    if total == 0 {
        remove_pair(part, meta);
        return Fail::internal(format!("{why} Nothing arrived, so nothing was saved."));
    }
    write_note(meta, model, total);
    Fail::internal(format!("{why} {total} bytes are on disk, so running the same command again picks the download up."))
}

/// The lock an install holds while it downloads: a file created with `create_new`, holding a random
/// value so the holder can tell it is its own, and touched every few seconds so a lock left by a
/// dead install can be told apart from a live one.
struct InstallLock {
    path: PathBuf,
    nonce: String,
    file: File,
    touched: Instant,
    /// True when this lock was taken over from a lock file another install left rather than created
    /// here, so the install that left it might still be running.
    taken_over: bool,
}

impl InstallLock {
    /// Take the lock, or say that another install has it. A lock nobody has touched for
    /// [LOCK_STALE] belonged to an install that died, so it is taken over once - see [claim_stale].
    fn take(path: &Path, label: &str) -> Result<InstallLock> {
        let nonce = install_tag()?;
        let mut taken_over = false;
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(nonce.as_bytes()).and_then(|()| file.flush()) {
                        let _ = std::fs::remove_file(path);
                        return Err(Fail::internal(format!("Craft AI Manager could not write {}: {error}", path.display())));
                    }
                    return Ok(InstallLock { path: path.to_path_buf(), nonce, file, touched: Instant::now(), taken_over });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if attempt == 0 && is_stale(path) && claim_stale(path, &nonce, LOCK_STALE) {
                        taken_over = true;
                        continue;
                    }
                    break;
                }
                Err(error) => return Err(Fail::internal(format!("Craft AI Manager could not create {}: {error}", path.display()))),
            }
        }
        Err(Fail::conflict(format!(
            "Another install of {label} is downloading it right now. Wait for it to finish, then run the same command again; if no other install is running, try again in two minutes."
        )))
    }

    /// Whether the install holding this lock may adopt a half-download, and under which rule.
    fn resume(&self) -> Resume {
        if self.taken_over { Resume::TakenOver } else { Resume::FreshLock }
    }

    /// Tell other installs this one is still alive.
    fn heartbeat(&mut self) {
        if self.touched.elapsed() >= LOCK_HEARTBEAT {
            let _ = self.file.set_modified(SystemTime::now());
            self.touched = Instant::now();
        }
    }
}

impl Drop for InstallLock {
    /// Release the lock without a check-then-delete race: move it aside to a name only this install
    /// uses, and delete it there only if it is still the one this install wrote; if another install
    /// had taken it over in the meantime, put that one back.
    fn drop(&mut self) {
        let aside = sibling(&self.path, &format!("{}.release", self.nonce));
        if std::fs::rename(&self.path, &aside).is_err() {
            return;
        }
        let ours = std::fs::read_to_string(&aside).is_ok_and(|text| text == self.nonce);
        if !ours && std::fs::rename(&aside, &self.path).is_ok() {
            return;
        }
        let _ = std::fs::remove_file(&aside);
    }
}

/// Take a stale lock out of the way so that exactly one claimer can take it: rename it to a name
/// only this claimer uses (of several racing, one rename wins and the others find nothing to move),
/// check that what was moved really is the stale lock and not a fresh one that replaced it in the
/// meantime, and only then delete it. [is_older_than] is asked with `limit`, because a lock held
/// across a short step goes stale sooner than one held across a download.
pub(crate) fn claim_stale(path: &Path, nonce: &str, limit: Duration) -> bool {
    let aside = sibling(path, &format!("{nonce}.stale"));
    if std::fs::rename(path, &aside).is_err() {
        return false;
    }
    if !is_older_than(&aside, limit) {
        // A live install's lock arrived between the check and the rename: give it back.
        if std::fs::rename(&aside, path).is_err() {
            let _ = std::fs::remove_file(&aside);
        }
        return false;
    }
    let _ = std::fs::remove_file(&aside);
    true
}

/// `<path>.<suffix>`, next to `path`.
pub(crate) fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Has nobody touched this file for [LOCK_STALE]? A file whose age cannot be read counts as live.
fn is_stale(path: &Path) -> bool {
    is_older_than(path, LOCK_STALE)
}

/// Has nobody touched this file for `limit`? A file whose age cannot be read counts as live,
/// because a lock nobody can date is safer treated as one that is still held.
pub(crate) fn is_older_than(path: &Path, limit: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= limit)
}

/// Re-hash an installed model and rewrite `verified.json`. Startup does not do this; a person does,
/// with `caim models verify <id>`. A failure removes the verification, so the model stops counting
/// as installed until it is downloaded again.
pub fn verify(model: &Model, root: &Path) -> Result<Checked> {
    let path = store::model_file(root, model);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Err(Fail::not_found(format!("{} is not installed, so there is nothing to check.", model.label())));
    };
    let started = Instant::now();
    let actual = store::hash_file(&path)?;
    let elapsed_ms = millis_since(started);
    if metadata.len() != model.size {
        store::remove_verified(root, model)?;
        return Err(Fail::internal(format!(
            "{} is {} bytes on disk, but the catalog says {}. Download it again with: caim models install {}.",
            model.label(),
            metadata.len(),
            model.size,
            model.id
        )));
    }
    if actual != model.sha256 {
        store::remove_verified(root, model)?;
        return Err(Fail::internal(format!(
            "{} has SHA-256 {actual} on disk, not the {} the catalog expects. Download it again with: caim models install {}.",
            model.label(),
            model.sha256,
            model.id
        )));
    }
    store::write_verified(root, model, &actual, metadata.len())?;
    Ok(Checked { sha256: actual, bytes: metadata.len(), elapsed_ms })
}

/// Delete this install's own half-written file and its note, and hand back the error that explains
/// why: the bytes are wrong, so they are not worth resuming.
fn abandon_pair(part: &Path, meta: &Path, fail: Fail) -> Fail {
    remove_pair(part, meta);
    fail
}

/// Milliseconds since `started`, clipped rather than wrapped.
fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Where a downloaded file would go, for the message that names it.
#[must_use]
pub fn installed_path(root: &Path, model: &Model) -> PathBuf {
    store::model_file(root, model)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crate::error::Status;
    use crate::fetch::{Answer, Fetch, LOCK_STALE, Outcome, Resume, Stream, fetch_into_place, install, resume_answer, verify};
    use crate::store::{
        is_installed, is_part_meta_name, is_part_name, lock_file, model_file, model_folder, part_file, part_meta_file, read_verified, sha256_hex,
        write_acknowledgement,
    };
    use crate::testkit::{FakeFetch, RangeFetch, Scratch, SlowFetch, test_model};
    use std::io::Write;
    use std::time::Duration;

    const FIXTURE: &[u8] = b"the bytes a small model would be made of";
    const NOTE: &str = "Trained on research-only data, so personal use only.";

    fn no_progress(_read: u64, _total: u64) {}

    /// Every `.part`, lock or lock-aside file still in the model's folder.
    fn leftovers(scratch: &Scratch, entry: &crate::catalog::Model) -> Vec<String> {
        let Ok(list) = std::fs::read_dir(model_folder(&scratch.path, entry)) else { return Vec::new() };
        list.flatten()
            .map(|item| item.file_name().to_string_lossy().to_string())
            .filter(|name| is_part_name(entry, name) || is_part_meta_name(entry, name) || name.contains(".lock") || name.contains(".tmp-"))
            .collect()
    }

    /// Make a file look as if nobody has touched it for longer than [LOCK_STALE].
    fn age(path: &std::path::Path) {
        let file = std::fs::OpenOptions::new().write(true).open(path).expect("the file opens");
        file.set_modified(std::time::SystemTime::now() - LOCK_STALE - Duration::from_secs(1)).expect("the file can be made old");
    }

    /// The same number of bytes as the fixture, so a hash mismatch is what fails and not the size.
    fn same_length_other_bytes() -> Vec<u8> {
        vec![b'x'; FIXTURE.len()]
    }

    fn install_into(scratch: &Scratch, entry: &crate::catalog::Model, fetcher: &FakeFetch) -> crate::error::Result<Outcome> {
        install(entry, &scratch.path, fetcher, &mut no_progress)
    }

    #[test]
    fn a_good_download_lands_in_place_and_is_recorded() {
        let scratch = Scratch::new("fetch-good");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(FIXTURE);
        let outcome = install_into(&scratch, &entry, &fetcher).expect("the fixture installs");
        let sha256 = match outcome {
            Outcome::Downloaded { sha256, bytes, elapsed_ms, resumed_from } => {
                assert_eq!(bytes, entry.size, "the whole file arrived");
                assert_eq!(resumed_from, 0, "a first install has nothing to resume");
                assert!(elapsed_ms < 60_000, "a 40 byte fixture is quick: {elapsed_ms} ms");
                sha256
            }
            Outcome::AlreadyInstalled => panic!("an empty store cannot already have it"),
        };
        assert_eq!(sha256, entry.sha256);
        let path = model_file(&scratch.path, &entry);
        assert_eq!(std::fs::read(&path).expect("the model file can be read"), FIXTURE, "the file is where the store says: {}", path.display());
        assert!(leftovers(&scratch, &entry).is_empty(), "no half-written file is left behind");
        let record = read_verified(&scratch.path, &entry).expect("the verification record was written");
        assert_eq!(record.sha256, entry.sha256);
        assert_eq!(record.file, entry.file);
        assert_eq!(record.size, entry.size);
        assert!(is_installed(&scratch.path, &entry));
    }

    #[test]
    fn a_hash_that_does_not_match_deletes_the_part_file() {
        let scratch = Scratch::new("fetch-hash");
        let entry = test_model(&same_length_other_bytes(), None);
        let fetcher = FakeFetch::new(FIXTURE);
        let fail = install_into(&scratch, &entry, &fetcher).expect_err("the bytes are not the ones the catalog promises");
        assert_eq!(fail.status, Status::Internal);
        assert!(fail.sentence.contains(&entry.sha256), "the sentence names the hash that was expected: {fail}");
        assert!(fail.sentence.contains(&sha256_hex(FIXTURE)), "and the hash that arrived: {fail}");
        assert!(leftovers(&scratch, &entry).is_empty(), "the half-written file is deleted");
        assert!(!model_file(&scratch.path, &entry).exists(), "nothing is put in place");
        assert!(read_verified(&scratch.path, &entry).is_none(), "and nothing is recorded");
        assert!(!is_installed(&scratch.path, &entry));
    }

    #[test]
    fn a_download_that_stops_short_keeps_what_arrived() {
        // Amendment 18: bytes that stopped short are not wrong, only unfinished, so they stay.
        let scratch = Scratch::new("fetch-short");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(&FIXTURE[..FIXTURE.len() - 7]);
        let fail = install_into(&scratch, &entry, &fetcher).expect_err("the download stopped before the catalog size");
        assert!(fail.sentence.contains("bytes"), "the sentence talks about sizes: {fail}");
        assert!(fail.sentence.contains("picks the download up"), "and says what to do next: {fail}");
        assert!(!is_installed(&scratch.path, &entry));
        let left = leftovers(&scratch, &entry);
        assert_eq!(left.len(), 2, "the half-written file and its note are both kept: {left:?}");
        let fetcher = RangeFetch::new(FIXTURE);
        install(&entry, &scratch.path, &fetcher, &mut no_progress)
            .expect("the very next install carries on from those bytes, without waiting for them to look old");
        assert_eq!(fetcher.asked(), vec![(FIXTURE.len() - 7) as u64], "only the bytes that were missing got asked for");
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE);
        assert!(leftovers(&scratch, &entry).is_empty(), "and nothing is left behind");
    }

    #[test]
    fn a_range_answer_says_whether_to_carry_on_or_start_over() {
        assert_eq!(resume_answer("https://example.invalid/f.onnx", 20, 206).expect("a 206 is an answer"), Answer::Partial);
        assert_eq!(resume_answer("https://example.invalid/f.onnx", 20, 200).expect("a 200 is an answer too"), Answer::Full);
        let fail = resume_answer("https://example.invalid/f.onnx", 20, 416).expect_err("anything else is an error");
        assert!(fail.sentence.contains("416"), "the sentence names the status: {fail}");
        assert!(fail.sentence.contains("bytes from 20 on"), "and what was asked for: {fail}");
    }

    /// Leave a half-download behind the way a run that died leaves one: the first `taken` bytes in a
    /// `.part`, and a note saying what they are meant to become. Both paths come back so the caller
    /// can decide whether they look old enough to pick up.
    fn leave_half(scratch: &Scratch, entry: &crate::catalog::Model, taken: usize, revision: &str, sha256: &str) -> [std::path::PathBuf; 2] {
        let folder = model_folder(&scratch.path, entry);
        std::fs::create_dir_all(&folder).expect("the model folder can be made");
        let part = part_file(&scratch.path, entry, "dead0000");
        let note = part_meta_file(&scratch.path, entry, "dead0000");
        std::fs::write(&part, &FIXTURE[..taken]).expect("the half file can be written");
        let meta = crate::store::PartMeta { url: entry.url.clone(), revision: revision.to_string(), sha256: sha256.to_string(), bytes: taken as u64 };
        crate::store::write_part_meta(&note, &meta).expect("the note can be written");
        [part, note]
    }

    /// Make a half-download look old enough that the next install may pick it up.
    fn age_pair(pair: &[std::path::PathBuf; 2]) {
        for path in pair {
            age(path);
        }
    }

    #[test]
    fn a_half_download_continues_where_it_stopped() {
        let scratch = Scratch::new("fetch-resume");
        let entry = test_model(FIXTURE, None);
        let half = (FIXTURE.len() / 2) as u64;
        age_pair(&leave_half(&scratch, &entry, FIXTURE.len() / 2, &entry.revision, &entry.sha256));
        let fetcher = RangeFetch::new(FIXTURE);
        let mut seen = Vec::new();
        let outcome = install(&entry, &scratch.path, &fetcher, &mut |read, total| seen.push((read, total))).expect("the install picks the download up");
        let resumed_from = match outcome {
            Outcome::Downloaded { bytes, resumed_from, .. } => {
                assert_eq!(bytes, entry.size, "the whole file arrived");
                resumed_from
            }
            Outcome::AlreadyInstalled => panic!("a half file is not an installed model"),
        };
        assert_eq!(resumed_from, half, "the bytes on disk were counted, not fetched again");
        assert_eq!(fetcher.asked(), vec![half], "only the missing bytes were asked for");
        assert_eq!(seen.first(), Some(&(half, entry.size)), "the progress report starts at the resumed offset");
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE, "and the file is whole");
        assert!(is_installed(&scratch.path, &entry));
        assert!(leftovers(&scratch, &entry).is_empty(), "the note went away with the finished file");
    }

    #[test]
    fn a_server_that_ignores_the_range_starts_the_download_over() {
        let scratch = Scratch::new("fetch-resume-full");
        let entry = test_model(FIXTURE, None);
        let half = (FIXTURE.len() / 2) as u64;
        age_pair(&leave_half(&scratch, &entry, FIXTURE.len() / 2, &entry.revision, &entry.sha256));
        let fetcher = RangeFetch::ignoring_range(FIXTURE);
        let outcome = install(&entry, &scratch.path, &fetcher, &mut no_progress).expect("a server that sends the whole file still installs it");
        match outcome {
            Outcome::Downloaded { bytes, resumed_from, .. } => {
                assert_eq!(bytes, entry.size, "the whole file arrived");
                assert_eq!(resumed_from, 0, "the bytes on disk were thrown away, not continued");
            }
            Outcome::AlreadyInstalled => panic!("a half file is not an installed model"),
        }
        assert_eq!(fetcher.asked(), vec![half], "the range was asked for once, and ignored");
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE);
        assert!(leftovers(&scratch, &entry).is_empty(), "the half file that could not be trusted is gone");
    }

    #[test]
    fn a_note_for_another_revision_or_hash_starts_the_download_over() {
        for (who, revision, sha256) in [
            ("revision", "another-revision", None),
            ("hash", "a3ee2fca54baebec351b8fa7786154ffa7555aa6", Some("0000000000000000000000000000000000000000000000000000000000000000")),
        ] {
            let scratch = Scratch::new(&format!("fetch-resume-{who}"));
            let entry = test_model(FIXTURE, None);
            age_pair(&leave_half(&scratch, &entry, FIXTURE.len() / 2, revision, sha256.unwrap_or(entry.sha256.as_str())));
            let fetcher = RangeFetch::new(FIXTURE);
            install(&entry, &scratch.path, &fetcher, &mut no_progress).expect("the install starts over");
            assert_eq!(fetcher.asked(), vec![0], "{who}: the bytes on disk were not trusted");
            assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE, "{who}");
            assert!(leftovers(&scratch, &entry).is_empty(), "{who}: the wrong half file and its note are deleted");
        }
    }

    #[test]
    fn a_refused_range_keeps_the_bytes_for_the_next_try() {
        let scratch = Scratch::new("fetch-resume-refused");
        let entry = test_model(FIXTURE, None);
        let half = (FIXTURE.len() / 2) as u64;
        age_pair(&leave_half(&scratch, &entry, FIXTURE.len() / 2, &entry.revision, &entry.sha256));
        let fetcher = RangeFetch::new(FIXTURE).refusing(416);
        let fail = install(&entry, &scratch.path, &fetcher, &mut no_progress).expect_err("the server refused the range request");
        assert!(fail.sentence.contains("picks the download up"), "the sentence says the bytes are kept for the next try: {fail}");
        assert_eq!(fetcher.asked(), vec![half]);
        let left = leftovers(&scratch, &entry);
        assert_eq!(left.len(), 2, "both files are still there: {left:?}");
        install(&entry, &scratch.path, &RangeFetch::new(FIXTURE), &mut no_progress).expect("the next try carries on from them straight away");
        assert!(is_installed(&scratch.path, &entry));
        assert!(leftovers(&scratch, &entry).is_empty(), "and nothing is left behind");
    }

    #[test]
    fn a_half_file_someone_is_writing_to_is_not_picked_up() {
        // C6 again: two installs must never share bytes, so a half file that is still being written
        // to is not this install's to continue, however good its note looks. That is the rule when
        // the lock was taken over from an install that only looks dead, which is the only way a live
        // writer and a new install can be running at the same time.
        let scratch = Scratch::new("fetch-resume-live");
        let entry = test_model(FIXTURE, None);
        let pair = leave_half(&scratch, &entry, FIXTURE.len() / 2, &entry.revision, &entry.sha256);
        std::fs::write(lock_file(&scratch.path, &entry), "someone-else").expect("the other install holds the lock");
        age(&lock_file(&scratch.path, &entry));
        let fetcher = RangeFetch::new(FIXTURE);
        install(&entry, &scratch.path, &fetcher, &mut no_progress).expect("the install starts its own download");
        assert_eq!(fetcher.asked(), vec![0], "the other install's bytes were not adopted");
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE);
        assert_eq!(std::fs::read(&pair[0]).expect("the other install's file can be read"), &FIXTURE[..FIXTURE.len() / 2], "and it is untouched");
        assert_eq!(leftovers(&scratch, &entry).len(), 2, "the other install's pair is left alone");
    }

    #[test]
    fn a_pair_the_last_try_left_behind_is_adopted_without_waiting() {
        // The sentence promises that running the same command again picks the bytes up, so a pair the
        // previous try left on purpose is adopted right away: this install created the lock itself,
        // which means no other installer exists.
        let scratch = Scratch::new("fetch-resume-at-once");
        let entry = test_model(FIXTURE, None);
        let half = (FIXTURE.len() / 2) as u64;
        leave_half(&scratch, &entry, FIXTURE.len() / 2, &entry.revision, &entry.sha256);
        let fetcher = RangeFetch::new(FIXTURE);
        let outcome = install(&entry, &scratch.path, &fetcher, &mut no_progress).expect("the next run picks the bytes up at once");
        match outcome {
            Outcome::Downloaded { resumed_from, .. } => assert_eq!(resumed_from, half, "the bytes on disk were carried on from"),
            Outcome::AlreadyInstalled => panic!("a half file is not an installed model"),
        }
        assert_eq!(fetcher.asked(), vec![half], "nothing that was already on disk was fetched again");
        assert!(leftovers(&scratch, &entry).is_empty(), "the adopted pair went into the finished file");
    }

    #[test]
    fn a_part_longer_than_its_note_resumes_from_the_last_checkpoint() {
        // A run killed between two checkpoints leaves bytes the note never counted. They are cut away
        // and the recorded prefix is what carries on.
        let scratch = Scratch::new("fetch-resume-long");
        let entry = test_model(FIXTURE, None);
        let taken = FIXTURE.len() / 2;
        let half = taken as u64;
        let pair = leave_half(&scratch, &entry, taken, &entry.revision, &entry.sha256);
        let tail = [FIXTURE.get(..taken).expect("a prefix"), b" bytes nobody counted"].concat();
        std::fs::write(&pair[0], &tail).expect("the uncounted tail is written");
        age_pair(&pair);
        let fetcher = RangeFetch::new(FIXTURE);
        let outcome = install(&entry, &scratch.path, &fetcher, &mut no_progress).expect("the install cuts the tail away and carries on");
        match outcome {
            Outcome::Downloaded { bytes, resumed_from, .. } => {
                assert_eq!(bytes, entry.size, "the whole file arrived");
                assert_eq!(resumed_from, half, "the checkpoint the note counted was kept");
            }
            Outcome::AlreadyInstalled => panic!("a half file is not an installed model"),
        }
        assert_eq!(fetcher.asked(), vec![half], "only the bytes past the checkpoint were asked for");
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE, "and the file is whole");
        assert!(leftovers(&scratch, &entry).is_empty(), "and nothing is left behind");
    }

    #[test]
    fn a_part_shorter_than_its_note_starts_the_download_over() {
        // Bytes the note cannot account for are the other way round: the file is missing bytes the
        // note says were written, so nothing on disk can be trusted.
        let scratch = Scratch::new("fetch-resume-short-part");
        let entry = test_model(FIXTURE, None);
        let pair = leave_half(&scratch, &entry, FIXTURE.len() / 2, &entry.revision, &entry.sha256);
        std::fs::write(&pair[0], FIXTURE.get(..FIXTURE.len() / 4).expect("a shorter prefix")).expect("the part is shorter than its note");
        age_pair(&pair);
        let fetcher = RangeFetch::new(FIXTURE);
        install(&entry, &scratch.path, &fetcher, &mut no_progress).expect("the install starts over");
        assert_eq!(fetcher.asked(), vec![0], "the bytes on disk were not trusted");
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE);
        assert!(leftovers(&scratch, &entry).is_empty(), "the pair that could not be trusted is gone");
    }

    #[test]
    fn a_part_whose_note_counts_the_whole_file_is_finished_without_asking_the_server() {
        // The last run died after its final checkpoint, so the note counts every byte. Asking the
        // server for `bytes=<size>-` would answer 416 and every retry would repeat it, so the bytes
        // on disk are hashed and put in place here instead.
        let scratch = Scratch::new("fetch-resume-whole");
        let entry = test_model(FIXTURE, None);
        leave_half(&scratch, &entry, FIXTURE.len(), &entry.revision, &entry.sha256);
        let fetcher = RangeFetch::new(FIXTURE);
        let outcome = install(&entry, &scratch.path, &fetcher, &mut no_progress).expect("the whole file on disk is finished locally");
        match outcome {
            Outcome::Downloaded { bytes, resumed_from, .. } => {
                assert_eq!(bytes, entry.size, "the whole file is what got installed");
                assert_eq!(resumed_from, entry.size, "every byte of it was already on disk");
            }
            Outcome::AlreadyInstalled => panic!("a part file is not an installed model"),
        }
        assert!(fetcher.asked().is_empty(), "the server was never asked for anything: {:?}", fetcher.asked());
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE);
        assert!(is_installed(&scratch.path, &entry));
        assert!(leftovers(&scratch, &entry).is_empty(), "the finished part left nothing behind");
    }

    /// A range fetcher standing in for a stalled install that wakes up: the moment a range is asked
    /// for, the install that left the pair behind writes again through the handle it still holds on
    /// its own `.part`. Those bytes must never reach the file this install is verifying.
    struct WakingOwner {
        inner: RangeFetch,
        handle: RefCell<std::fs::File>,
        extra: &'static [u8],
    }

    impl Fetch for WakingOwner {
        fn open(&self, url: &str) -> crate::error::Result<Stream> {
            self.inner.open(url)
        }

        fn open_range(&self, url: &str, offset: u64) -> crate::error::Result<Stream> {
            if let Ok(mut file) = self.handle.try_borrow_mut() {
                let _ = file.write_all(self.extra);
                let _ = file.flush();
            }
            self.inner.open_range(url, offset)
        }
    }

    #[test]
    fn a_takeover_copies_the_bytes_it_resumes_from_rather_than_writing_into_them() {
        // A lock can be taken over from an install that is only stalled, and that install may still
        // hold its `.part` open. Renaming it would leave this install verifying a file somebody else
        // is appending to, so the recorded prefix is copied into a file this install owns outright.
        let scratch = Scratch::new("fetch-resume-copied");
        let entry = test_model(FIXTURE, None);
        let taken = FIXTURE.len() / 2;
        let half = taken as u64;
        let pair = leave_half(&scratch, &entry, taken, &entry.revision, &entry.sha256);
        age_pair(&pair);
        std::fs::write(lock_file(&scratch.path, &entry), "someone-else").expect("the lock left by the stalled install is written");
        age(&lock_file(&scratch.path, &entry));
        let handle = std::fs::OpenOptions::new().append(true).open(&pair[0]).expect("the stalled install still holds its own file open");
        let fetcher = WakingOwner { inner: RangeFetch::new(FIXTURE), handle: RefCell::new(handle), extra: b" still writing" };
        let outcome = install(&entry, &scratch.path, &fetcher, &mut no_progress).expect("the takeover carries on from a copy of the bytes");
        match outcome {
            Outcome::Downloaded { bytes, resumed_from, .. } => {
                assert_eq!(bytes, entry.size, "the whole file arrived");
                assert_eq!(resumed_from, half, "the bytes the note counted were carried on from");
            }
            Outcome::AlreadyInstalled => panic!("a half file is not an installed model"),
        }
        assert_eq!(fetcher.inner.asked(), vec![half], "only the missing bytes were asked for");
        assert_eq!(
            std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"),
            FIXTURE,
            "what the stalled owner wrote afterwards is not in the installed file"
        );
        assert!(is_installed(&scratch.path, &entry));
    }
    #[test]
    fn a_server_claiming_more_than_the_catalog_promises_is_refused() {
        let scratch = Scratch::new("fetch-claimed");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(FIXTURE).claiming(entry.size + 1_000);
        let fail = install_into(&scratch, &entry, &fetcher).expect_err("the server promises more than the catalog allows");
        assert_eq!(fail.status, Status::TooLarge);
        assert!(fail.sentence.contains(&entry.size.to_string()), "the sentence names the catalog size: {fail}");
        assert_eq!(fetcher.calls.get(), 1, "the request was opened, but nothing was downloaded");
        assert_eq!(std::fs::read_dir(model_folder(&scratch.path, &entry)).expect("the folder exists").count(), 0, "nothing was written");
        assert!(!is_installed(&scratch.path, &entry));
    }

    #[test]
    fn more_bytes_than_the_catalog_promises_are_refused() {
        let scratch = Scratch::new("fetch-oversize");
        let entry = test_model(&FIXTURE[..12], None);
        let fetcher = FakeFetch::new(FIXTURE);
        let fail = install_into(&scratch, &entry, &fetcher).expect_err("the server sends more than the catalog size");
        assert_eq!(fail.status, Status::TooLarge);
        assert!(fail.sentence.contains(&entry.size.to_string()), "the sentence names the catalog size: {fail}");
        assert!(leftovers(&scratch, &entry).is_empty(), "the half-written file is deleted");
        assert!(!is_installed(&scratch.path, &entry));
    }

    #[test]
    fn an_installed_model_makes_no_request_at_all() {
        let scratch = Scratch::new("fetch-twice");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(FIXTURE);
        install_into(&scratch, &entry, &fetcher).expect("the fixture installs");
        assert_eq!(fetcher.calls.get(), 1, "the first install asks the server once");
        let outcome = install_into(&scratch, &entry, &fetcher).expect("the second install is fine");
        assert!(matches!(outcome, Outcome::AlreadyInstalled), "it is already there");
        assert_eq!(fetcher.calls.get(), 1, "and it asked the server no times");
    }

    #[test]
    fn a_flagged_model_waits_for_the_note_to_be_accepted() {
        let scratch = Scratch::new("fetch-note");
        let entry = test_model(FIXTURE, Some(NOTE));
        let fetcher = FakeFetch::new(FIXTURE);
        let fail = install_into(&scratch, &entry, &fetcher).expect_err("the note has not been accepted");
        assert_eq!(fail.status, Status::Conflict);
        assert!(fail.sentence.contains("--accept-note"), "the sentence says what to do: {fail}");
        assert!(fail.sentence.contains(NOTE), "and shows the note itself: {fail}");
        assert_eq!(fetcher.calls.get(), 0, "a model that is not accepted is never fetched");
        write_acknowledgement(&scratch.path, &entry).expect("the acknowledgment can be recorded");
        install_into(&scratch, &entry, &fetcher).expect("with the note accepted it installs");
        assert_eq!(fetcher.calls.get(), 1);
        assert!(is_installed(&scratch.path, &entry));
    }

    #[test]
    fn progress_is_reported_as_the_bytes_arrive() {
        let scratch = Scratch::new("fetch-progress");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(FIXTURE);
        let seen = RefCell::new(Vec::new());
        install(&entry, &scratch.path, &fetcher, &mut |read, total| seen.borrow_mut().push((read, total))).expect("the fixture installs");
        let seen = seen.into_inner();
        assert!(!seen.is_empty(), "the CLI gets something to print");
        assert!(seen.iter().all(|(_read, total)| *total == entry.size), "every report names the catalog size");
        let mut previous = 0;
        for (read, _total) in &seen {
            assert!(*read > previous, "the count only moves forward: {seen:?}");
            previous = *read;
        }
        assert_eq!(seen.last().expect("there is at least one report"), &(entry.size, entry.size), "and it ends at the whole file");
    }

    #[test]
    fn verify_re_hashes_an_installed_model() {
        let scratch = Scratch::new("fetch-verify");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(FIXTURE);
        install_into(&scratch, &entry, &fetcher).expect("the fixture installs");
        let checked = verify(&entry, &scratch.path).expect("the file checks out");
        assert_eq!(checked.sha256, entry.sha256);
        assert_eq!(checked.bytes, entry.size);
        assert!(read_verified(&scratch.path, &entry).expect("the record is still there").sha256 == entry.sha256);
        assert!(is_installed(&scratch.path, &entry));
    }

    #[test]
    fn verify_clears_the_record_when_the_file_has_changed() {
        let scratch = Scratch::new("fetch-verify-changed");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(FIXTURE);
        install_into(&scratch, &entry, &fetcher).expect("the fixture installs");
        std::fs::write(model_file(&scratch.path, &entry), same_length_other_bytes()).expect("the file can be tampered with");
        assert!(is_installed(&scratch.path, &entry), "a quiet re-hash is not part of startup, so it still looks installed");
        let fail = verify(&entry, &scratch.path).expect_err("the bytes have changed");
        assert!(fail.sentence.contains(&entry.sha256), "the sentence names the hash the catalog expects: {fail}");
        assert!(read_verified(&scratch.path, &entry).is_none(), "the record is dropped");
        assert!(!is_installed(&scratch.path, &entry), "so the model stops counting as installed");
    }

    #[test]
    fn verify_says_a_model_that_is_not_installed_cannot_be_checked() {
        let scratch = Scratch::new("fetch-verify-absent");
        let entry = test_model(FIXTURE, None);
        let fail = verify(&entry, &scratch.path).expect_err("there is nothing to hash");
        assert_eq!(fail.status, Status::NotFound);
        assert!(fail.sentence.contains("not installed"), "{fail}");
    }

    /// 64 KiB of bytes that are not all the same, so a mixed-up file cannot pass for the right one.
    fn patterned(seed: u8) -> Vec<u8> {
        (0..64 * 1024).map(|index: usize| (index % 251) as u8 ^ seed).collect()
    }

    #[test]
    fn two_installs_at_once_never_leave_a_wrong_file_marked_verified() {
        // C6: both used to write the same `.part`; the slower one kept writing into the file the
        // faster one had already renamed into place and recorded as verified.
        for round in 0..4u32 {
            let scratch = Scratch::new(&format!("fetch-race-{round}"));
            let good = patterned(0);
            let entry = test_model(&good, None);
            let right = SlowFetch { bytes: good.clone(), chunk: 4 * 1024, delay: Duration::from_millis(2) };
            let wrong = SlowFetch { bytes: patterned(0x5a), chunk: 4 * 1024, delay: Duration::from_millis(3 + u64::from(round)) };
            std::thread::scope(|scope| {
                let first = scope.spawn(|| install(&entry, &scratch.path, &right, &mut no_progress));
                let second = scope.spawn(|| install(&entry, &scratch.path, &wrong, &mut no_progress));
                let _ = first.join().expect("the first install thread");
                let _ = second.join().expect("the second install thread");
            });
            if is_installed(&scratch.path, &entry) {
                let on_disk = std::fs::read(model_file(&scratch.path, &entry)).expect("the installed file can be read");
                assert!(on_disk == good, "round {round}: the file recorded as verified is not the one the catalog describes");
            }
            assert!(leftovers(&scratch, &entry).is_empty(), "round {round}: no half-written file is left behind");
            // Whatever happened, a plain install afterwards ends with the right file.
            install(&entry, &scratch.path, &FakeFetch::new(&good), &mut no_progress).expect("a later install works");
            assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the file"), good, "round {round}");
        }
    }

    #[test]
    fn an_install_already_running_is_left_alone() {
        let scratch = Scratch::new("fetch-locked");
        let entry = test_model(FIXTURE, None);
        std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder");
        std::fs::write(lock_file(&scratch.path, &entry), "another install").expect("a live lock");
        let fetcher = FakeFetch::new(FIXTURE);
        let fail = install_into(&scratch, &entry, &fetcher).expect_err("another install holds the lock");
        assert_eq!(fail.status, Status::Conflict, "{fail}");
        assert!(fail.sentence.contains("Another install"), "{fail}");
        assert_eq!(fetcher.calls.get(), 0, "nothing was fetched");
        assert!(lock_file(&scratch.path, &entry).exists(), "and the other install's lock is still there");
    }

    #[test]
    fn a_lock_left_by_a_dead_install_is_taken_over() {
        let scratch = Scratch::new("fetch-stale-lock");
        let entry = test_model(FIXTURE, None);
        std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder");
        let lock = std::fs::File::create(lock_file(&scratch.path, &entry)).expect("a lock");
        let long_ago = std::time::SystemTime::now() - LOCK_STALE - Duration::from_secs(1);
        lock.set_modified(long_ago).expect("the lock can be made old");
        drop(lock);
        install_into(&scratch, &entry, &FakeFetch::new(FIXTURE)).expect("a lock nobody has touched for minutes does not block an install");
        assert!(is_installed(&scratch.path, &entry));
        assert!(!lock_file(&scratch.path, &entry).exists(), "the lock is released when the install ends");
    }

    #[test]
    fn part_files_left_by_dead_runs_are_cleared_and_a_live_one_is_not() {
        let scratch = Scratch::new("fetch-stale-part");
        let entry = test_model(FIXTURE, None);
        std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder");
        let old_style = model_folder(&scratch.path, &entry).join(format!("{}.part", entry.file));
        let dead = part_file(&scratch.path, &entry, "dead0000");
        let live = part_file(&scratch.path, &entry, "live0000");
        for path in [&old_style, &dead, &live] {
            std::fs::write(path, b"left over from a run that crashed, and longer than the fixture").expect("a .part");
        }
        age(&old_style);
        age(&dead);
        install_into(&scratch, &entry, &FakeFetch::new(FIXTURE)).expect("old files do not block an install");
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE);
        assert!(!old_style.exists() && !dead.exists(), "what dead runs left is gone");
        assert!(live.exists(), "a .part someone wrote to recently is not this install's to delete");
    }

    /// Two installs of the same model at the same moment, one served the right bytes and one the
    /// wrong ones, both slow enough to overlap. Whatever happens, a file recorded as verified is the
    /// right file and nothing is left behind.
    fn race(scratch: &Scratch, entry: &crate::catalog::Model, good: &[u8], round: u32, run: &(dyn Fn(&SlowFetch) -> crate::error::Result<Outcome> + Sync)) {
        let right = SlowFetch { bytes: good.to_vec(), chunk: 4 * 1024, delay: Duration::from_millis(2) };
        let wrong = SlowFetch { bytes: patterned(0x5a), chunk: 4 * 1024, delay: Duration::from_millis(2 + u64::from(round % 3)) };
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                start.wait();
                run(&right)
            });
            let second = scope.spawn(|| {
                start.wait();
                run(&wrong)
            });
            let _ = first.join().expect("the first install thread");
            let _ = second.join().expect("the second install thread");
        });
        if is_installed(&scratch.path, entry) {
            let on_disk = std::fs::read(model_file(&scratch.path, entry)).expect("the installed file can be read");
            assert!(on_disk == good, "round {round}: the file recorded as verified is not the one the catalog describes");
        }
        assert!(leftovers(scratch, entry).is_empty(), "round {round}: nothing is left behind: {:?}", leftovers(scratch, entry));
    }

    #[test]
    fn two_downloads_that_both_think_they_hold_the_lock_cannot_install_the_wrong_bytes() {
        // The follow-up to C6: correctness must not rest on the lock. Here the lock is skipped
        // altogether, as if a stale-lock takeover had gone wrong, and each download still renames
        // only the bytes it hashed itself.
        for round in 0..6u32 {
            let scratch = Scratch::new(&format!("fetch-no-lock-{round}"));
            let good = patterned(0);
            let entry = test_model(&good, None);
            std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder");
            race(&scratch, &entry, &good, round, &|fetch| fetch_into_place(&entry, &scratch.path, fetch, &mut no_progress, &mut || {}, Resume::NoLock));
        }
    }

    #[test]
    fn two_installs_racing_over_a_stale_lock_never_leave_a_wrong_file_marked_verified() {
        // Both see the dead install's lock as stale at the same moment; only one may take it over.
        for round in 0..6u32 {
            let scratch = Scratch::new(&format!("fetch-stale-race-{round}"));
            let good = patterned(0);
            let entry = test_model(&good, None);
            std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder");
            std::fs::write(lock_file(&scratch.path, &entry), "an install that died").expect("a lock");
            age(&lock_file(&scratch.path, &entry));
            race(&scratch, &entry, &good, round, &|fetch| install(&entry, &scratch.path, fetch, &mut no_progress));
            install(&entry, &scratch.path, &FakeFetch::new(&good), &mut no_progress).expect("a later install works");
            assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the file"), good, "round {round}");
        }
    }
}
