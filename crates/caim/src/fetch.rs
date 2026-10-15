//! Getting a model file, and the rules that keep a bad one out of the store.
//!
//! SPEC v1.1 amendment 14 and PhotoCraft FEATURES row CAIM-03: download with a progress report, hash
//! it while it streams, and refuse anything that does not match the catalog. M1 has no resume - a
//! half-download starts again - and cancellation waits for MG-05.
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

/// An open download: what the server says the length is, when it says, and the bytes.
pub struct Stream {
    /// The server's own `Content-Length`, if it sent one.
    pub claimed_len: Option<u64>,
    /// The body.
    pub reader: Box<dyn Read>,
}

/// How CAIM opens a URL. Kept this small so the tests can hand over a byte slice instead of a
/// network, and so MG-05 can add cancellation in one place.
///
/// Implementations must not follow a redirect to plain HTTP.
pub trait Fetch {
    /// Open `url` for reading.
    fn open(&self, url: &str) -> Result<Stream>;
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
        let config = ureq::config::Config::builder()
            .https_only(true)
            .max_redirects(MAX_REDIRECTS)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Fetch for HttpFetch {
    fn open(&self, url: &str) -> Result<Stream> {
        let response = match self.agent.get(url).call() {
            Ok(response) => response,
            Err(error) => return Err(Fail::internal(format!("Craft AI Manager could not start the download of {url}: {error}"))),
        };
        let body = response.into_body();
        let claimed_len = body.content_length();
        Ok(Stream { claimed_len, reader: Box::new(body.into_reader()) })
    }
}

/// What an install did.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Nothing was fetched: the file, its size and its verification were already in place.
    AlreadyInstalled,
    /// The file came down, hashed to what the catalog promised, and was verified.
    Downloaded { sha256: String, bytes: u64, elapsed_ms: u64 },
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

/// The download itself: `.part` in the model's folder, hashed as it streams, refused if the server's
/// length or the bytes read go past the catalog size, renamed and recorded only when the hash matches.
///
/// C6: only the install holding `<file>.lock` may touch `.part`, and `.part` is created with
/// `create_new`, so two installs at once can never write into the same file. A `.part` found while
/// holding the lock was left by a run that died, so it is removed first. The bytes are synced to
/// disk before the rename, so a power cut cannot leave a short file under the verified name.
fn download(model: &Model, root: &Path, fetch: &dyn Fetch, progress: &mut dyn FnMut(u64, u64)) -> Result<Outcome> {
    let folder = store::model_folder(root, model);
    std::fs::create_dir_all(&folder).map_err(|error| Fail::internal(format!("Craft AI Manager could not create {}: {error}", folder.display())))?;
    let mut lock = InstallLock::take(&store::lock_file(root, model), &model.label())?;
    if store::is_installed(root, model) {
        // Another install finished while this one waited for the lock.
        return Ok(Outcome::AlreadyInstalled);
    }
    let part = store::part_file(root, model);
    let target = store::model_file(root, model);
    match std::fs::remove_file(&part) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(Fail::internal(format!("Craft AI Manager could not remove the unfinished download {}: {error}", part.display()))),
    }

    let stream = fetch.open(&model.url)?;
    if let Some(claimed) = stream.claimed_len.filter(|claimed| *claimed > model.size) {
        return Err(Fail::too_large(format!(
            "{} is {} bytes in the catalog, but {} says it has {claimed}. That is not the file the catalog describes, so nothing was downloaded.",
            model.label(),
            model.size,
            model.url
        )));
    }

    let mut file = match OpenOptions::new().write(true).create_new(true).open(&part) {
        Ok(file) => file,
        Err(error) => return Err(Fail::internal(format!("Craft AI Manager could not create {}: {error}", part.display()))),
    };
    let mut reader = stream.reader;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut total: u64 = 0;
    let started = Instant::now();

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let Some(chunk) = buffer.get(..read) else { break };
                let Some(next) = total.checked_add(read as u64) else {
                    return Err(abandon(&part, &lock, Fail::too_large(format!("{} is more bytes than this machine was asked to store.", model.label()))));
                };
                if next > model.size {
                    return Err(abandon(
                        &part,
                        &lock,
                        Fail::too_large(format!(
                            "{} is {} bytes in the catalog, and the server had sent {next} before it stopped. Nothing was saved.",
                            model.label(),
                            model.size
                        )),
                    ));
                }
                hasher.update(chunk);
                if let Err(error) = file.write_all(chunk) {
                    return Err(abandon(&part, &lock, Fail::internal(format!("Craft AI Manager could not write {}: {error}", part.display()))));
                }
                total = next;
                lock.heartbeat();
                progress(total, model.size);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(abandon(&part, &lock, Fail::internal(format!("Craft AI Manager lost the download of {}: {error}", model.url)))),
        }
    }

    if let Err(error) = file.flush().and_then(|()| file.sync_all()) {
        return Err(abandon(&part, &lock, Fail::internal(format!("Craft AI Manager could not finish writing {}: {error}", part.display()))));
    }
    drop(file);

    if total != model.size {
        return Err(abandon(
            &part,
            &lock,
            Fail::internal(format!("{} is {} bytes in the catalog, but the download was {total} bytes. Nothing was saved.", model.label(), model.size)),
        ));
    }
    let actual = store::hex(&hasher.finalize());
    if actual != model.sha256 {
        return Err(abandon(
            &part,
            &lock,
            Fail::internal(format!(
                "The file downloaded for {} has SHA-256 {actual}, not the {} the catalog expects. Nothing was saved.",
                model.label(),
                model.sha256
            )),
        ));
    }
    if !lock.still_ours() {
        // This download stalled for so long that another install took the lock over, so `.part`
        // is now that install's file. Leave it alone.
        return Err(Fail::conflict(format!(
            "The download of {} stalled for so long that another install took over. Nothing was saved by this one.",
            model.label()
        )));
    }
    if let Err(error) = std::fs::rename(&part, &target) {
        return Err(abandon(&part, &lock, Fail::internal(format!("Craft AI Manager could not put {} in place: {error}", target.display()))));
    }
    store::write_verified(root, model, &actual, total)?;
    Ok(Outcome::Downloaded { sha256: actual, bytes: total, elapsed_ms: millis_since(started) })
}

/// The lock an install holds while it owns `.part`: a file created with `create_new`, holding a
/// random value so the holder can tell it is still its own, and touched every few seconds so a lock
/// left by a dead install can be told apart from a live one.
struct InstallLock {
    path: PathBuf,
    nonce: String,
    file: File,
    touched: Instant,
}

impl InstallLock {
    /// Take the lock, or say that another install has it. A lock nobody has touched for
    /// [LOCK_STALE] belonged to an install that died, so it is taken over once.
    fn take(path: &Path, label: &str) -> Result<InstallLock> {
        let nonce = crate::token::random_token()?;
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(nonce.as_bytes()).and_then(|()| file.flush()) {
                        let _ = std::fs::remove_file(path);
                        return Err(Fail::internal(format!("Craft AI Manager could not write {}: {error}", path.display())));
                    }
                    return Ok(InstallLock { path: path.to_path_buf(), nonce, file, touched: Instant::now() });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if attempt == 0 && lock_is_stale(path) {
                        let _ = std::fs::remove_file(path);
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

    /// Tell other installs this one is still alive.
    fn heartbeat(&mut self) {
        if self.touched.elapsed() >= LOCK_HEARTBEAT {
            let _ = self.file.set_modified(SystemTime::now());
            self.touched = Instant::now();
        }
    }

    /// Is the lock file on disk still the one this install wrote?
    fn still_ours(&self) -> bool {
        std::fs::read_to_string(&self.path).is_ok_and(|text| text == self.nonce)
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        if self.still_ours() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Has nobody touched this lock for [LOCK_STALE]? A lock whose age cannot be read counts as live.
fn lock_is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= LOCK_STALE)
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

/// Delete the half-written file, if it is still this install's, and hand back the error that
/// explains why.
fn abandon(part: &Path, lock: &InstallLock, fail: Fail) -> Fail {
    if lock.still_ours() {
        let _ = std::fs::remove_file(part);
    }
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
    use crate::fetch::{LOCK_STALE, Outcome, install, verify};
    use crate::store::{is_installed, lock_file, model_file, model_folder, part_file, read_verified, sha256_hex, write_acknowledgement};
    use crate::testkit::{FakeFetch, Scratch, SlowFetch, test_model};
    use std::time::Duration;

    const FIXTURE: &[u8] = b"the bytes a small model would be made of";
    const NOTE: &str = "Trained on research-only data, so personal use only.";

    fn no_progress(_read: u64, _total: u64) {}

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
            Outcome::Downloaded { sha256, bytes, elapsed_ms } => {
                assert_eq!(bytes, entry.size, "the whole file arrived");
                assert!(elapsed_ms < 60_000, "a 40 byte fixture is quick: {elapsed_ms} ms");
                sha256
            }
            Outcome::AlreadyInstalled => panic!("an empty store cannot already have it"),
        };
        assert_eq!(sha256, entry.sha256);
        let path = model_file(&scratch.path, &entry);
        assert_eq!(std::fs::read(&path).expect("the model file can be read"), FIXTURE, "the file is where the store says: {}", path.display());
        assert!(!part_file(&scratch.path, &entry).exists(), "no half-written file is left behind");
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
        assert!(!part_file(&scratch.path, &entry).exists(), "the half-written file is deleted");
        assert!(!model_file(&scratch.path, &entry).exists(), "nothing is put in place");
        assert!(read_verified(&scratch.path, &entry).is_none(), "and nothing is recorded");
        assert!(!is_installed(&scratch.path, &entry));
    }

    #[test]
    fn a_download_that_stops_short_is_refused() {
        let scratch = Scratch::new("fetch-short");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(&FIXTURE[..FIXTURE.len() - 7]);
        let fail = install_into(&scratch, &entry, &fetcher).expect_err("the download stopped before the catalog size");
        assert!(fail.sentence.contains("bytes"), "the sentence talks about sizes: {fail}");
        assert!(!part_file(&scratch.path, &entry).exists(), "the half-written file is deleted");
        assert!(!is_installed(&scratch.path, &entry));
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
        assert!(!part_file(&scratch.path, &entry).exists(), "the half-written file is deleted");
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
            assert!(!part_file(&scratch.path, &entry).exists(), "round {round}: no half-written file is left behind");
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
    fn a_part_file_left_by_a_dead_run_is_replaced() {
        let scratch = Scratch::new("fetch-stale-part");
        let entry = test_model(FIXTURE, None);
        std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder");
        std::fs::write(part_file(&scratch.path, &entry), b"left over from a run that crashed, and longer than the fixture").expect("a stale .part");
        install_into(&scratch, &entry, &FakeFetch::new(FIXTURE)).expect("the stale file does not block an install");
        assert_eq!(std::fs::read(model_file(&scratch.path, &entry)).expect("the model file"), FIXTURE);
        assert!(!part_file(&scratch.path, &entry).exists());
    }
}
