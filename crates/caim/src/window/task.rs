//! The one thing the window is doing, on a thread of its own.
//!
//! Rule 3 of `agents/CAIM-UI-BRIEF.md` is that the UI thread never downloads, hashes, installs or
//! deletes: every one of those happens here, and the window reads the result back through
//! [crate::window::facts]. One [Task] runs at a time, which is what the brief's one-problem-at-a-time
//! layout assumes and what keeps two installs from racing over the same `.part` file.
//!
//! Cancel is a flag the fetcher checks between reads, not a thread that is killed. The part already
//! on disk and its resume note stay exactly where MG-12 left them, so a cancelled row reads
//! "Paused" and the next press carries on from it.
//!
//! MG-13 review round 2: the bytes of a download arrive on a reader thread of their own, so the flag
//! is still polled while the server is sending nothing at all, and a server that has gone quiet
//! entirely stops the download at a deadline rather than holding the window open forever.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cuda;
use crate::error::{Fail, Result};
use crate::fetch::{self, Fetch, Stream};
use crate::models;
use crate::store;
use crate::window::facts::{Busy, Source, Work};

/// How long a Remove waits for the job inside that model before it gives up.
const REMOVE_WAIT: Duration = Duration::from_secs(10);
/// How often that wait looks at the queue again.
const REMOVE_POLL: Duration = Duration::from_millis(100);
/// The sentence the reader returns once a download has been cancelled, and the only sign the worker
/// has that a stopped transfer was asked for rather than lost.
const CANCELLED: &str = "the download was cancelled";
/// How often a download looks at the cancel flag while it waits for the next bytes.
const READ_POLL: Duration = Duration::from_millis(100);
/// How long a download may go without a single byte before it counts as lost. The response body has
/// no read timeout of its own (MG-12: it is 200 MB), so this is what stops a stalled server from
/// holding the window open for as long as the person is willing to wait.
const READ_STALL: Duration = Duration::from_secs(60);
/// How many chunks the reader may hold while the writer catches up, which is the backpressure: a
/// person on a slow line stops the reader rather than filling memory.
const READ_QUEUE: usize = 4;
/// The size of one chunk the reader moves, which is [crate::fetch]'s own chunk size.
const READ_CHUNK: usize = 64 * 1024;

/// Something the person pressed. Every id is looked up again here rather than trusted from the UI,
/// so a row that has gone away in the last repaint cannot make the worker guess at a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Task {
    /// Download a model, with its licence note already accepted.
    Install { id: String, accept_note: bool },
    /// Re-fetch a model whose bytes on disk are not the bytes the catalog pins.
    Repair { id: String },
    /// Carry on with the part already on disk.
    Resume { id: String },
    /// Record that the note has been read, which is all section 4.4's third row needs.
    Acknowledge { id: String },
    /// Re-hash the file on disk against the catalog.
    Check { id: String },
    /// Stop routing jobs to a model, wait its job out, then delete it (brief rule 2).
    Remove { id: String },
    /// Download the CUDA and cuDNN libraries NVIDIA's licence covers.
    Cuda { accept_note: bool },
}

/// The task that is running: its thread, and the flag that asks it to stop.
pub struct Running {
    handle: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}

impl Running {
    /// Ask the task to stop. The part files stay, so the row reads Paused afterwards.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether the worker has finished, which is how the window knows it can let go.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_some_and(|handle| handle.is_finished())
    }

    /// Wait for the thread. The window does this on its way out, so a download cannot be cut off
    /// between one write and the next.
    pub fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Start a task. A machine that cannot start a thread says so in a sentence; the window shows it and
/// stays open.
pub fn start(source: &Arc<Source>, task: Task) -> Result<Running> {
    let work = source.work();
    // MG-13 review round 3: the slot is taken here, before the thread exists. `busy` only says
    // something once a worker has bytes to report, so two presses in the same repaint used to start
    // two workers over the same `.part` file.
    if !lock(&work).claim() {
        return Err(Fail::conflict("Something else is happening in this window right now."));
    }
    if let Some(id) = task_id(&task)
        && source.model(id).is_none()
    {
        lock(&work).release();
        return Err(Fail::not_found(format!("There is no model called {id} in this version of Craft AI Manager.")));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_flag = Arc::clone(&cancel);
    let worker_source = Arc::clone(source);
    let spawned = std::thread::Builder::new().name("caim-task".to_string()).spawn(move || {
        let fetcher = Cancellable { inner: fetch::HttpFetch::new(), cancel: Arc::clone(&worker_flag) };
        fill(&worker_source, &task, &fetcher, &worker_flag);
    });
    match spawned {
        Ok(handle) => Ok(Running { handle: Some(handle), cancel }),
        Err(_) => {
            lock(&work).release();
            Err(Fail::internal("This computer would not start the work, so nothing was changed."))
        }
    }
}

/// Give the window back when its worker is done, whatever the task ended as - including a panic,
/// which would otherwise leave every button refusing to start anything (MG-13 review round 3).
struct Finished(Arc<Mutex<Work>>);

impl Drop for Finished {
    fn drop(&mut self) {
        let mut work = lock(&self.0);
        work.set_busy(None);
        work.release();
    }
}

/// The id a task names, if it names one.
fn task_id(task: &Task) -> Option<&str> {
    match task {
        Task::Install { id, .. } | Task::Repair { id } | Task::Resume { id } | Task::Acknowledge { id } | Task::Check { id } | Task::Remove { id } => Some(id),
        Task::Cuda { .. } => None,
    }
}

/// The worker's whole life: say what is happening, do it, say what happened, and stop saying anything.
fn fill(source: &Source, task: &Task, fetcher: &dyn Fetch, cancel: &AtomicBool) {
    let _done = Finished(source.work());
    match task {
        Task::Install { id, accept_note } => download(source, id, *accept_note, fetcher, cancel),
        Task::Repair { id } => repair(source, id, fetcher, cancel),
        Task::Resume { id } => download(source, id, false, fetcher, cancel),
        Task::Acknowledge { id } => acknowledge(source, id),
        Task::Check { id } => check(source, id),
        Task::Remove { id } => remove(source, id),
        Task::Cuda { accept_note } => cuda_libraries(source, *accept_note, fetcher, cancel),
    }
}

/// A model download, whether it starts from nothing or carries on from a part.
fn download(source: &Source, id: &str, accept_note: bool, fetcher: &dyn Fetch, cancel: &AtomicBool) {
    let Some(model) = source.model(id) else { return };
    let work = source.work();
    let named = id.to_string();
    let mut progress = move |done: u64, total: u64| {
        lock(&work).model_progress(&named, done, total);
    };
    let outcome = models::install_model(&model, source.root(), accept_note, fetcher, &mut progress);
    let installed = outcome.is_ok();
    {
        let guard = source.work();
        let mut work = lock(&guard);
        work.forget_no_space(id);
        match outcome {
            Ok(_) => work.forget_damaged(id),
            Err(fail) => note_failure(&mut work, id, &fail, cancel),
        }
    }
    if installed {
        // MG-13 review round 2: the point of installing from the window is that the feature works
        // afterwards, so the server has to be told that this model is now on disk.
        route_installed(source, &model.task);
    }
}

/// Point the server at a model that is on disk: undo any retirement this run still remembers, then
/// re-read the store (MG-13 review round 3). A `Remove` that gave up halfway retired the task, and
/// without that step a model installed afterwards stayed unroutable until Craft AI Manager restarted.
fn route_installed(source: &Source, task: &str) {
    source.runner().restore_task(task);
    source.refresh_routing();
}

/// Section 4.4's first row: delete the damaged file and its verification, then the normal install.
fn repair(source: &Source, id: &str, fetcher: &dyn Fetch, cancel: &AtomicBool) {
    let Some(model) = source.model(id) else { return };
    let root = source.root();
    let _ = store::remove_verified(root, &model);
    let file = store::model_file(root, &model);
    if file.is_file() {
        let _ = std::fs::remove_file(&file);
    }
    lock(&source.work()).forget_damaged(id);
    // The note has already been read once, and the row is asking for the file back, so the
    // acknowledgement is not asked for a second time.
    download(source, id, true, fetcher, cancel);
}

/// Section 4.4's third row: nothing is downloaded, the note is only recorded as read.
fn acknowledge(source: &Source, id: &str) {
    let Some(model) = source.model(id) else { return };
    match store::write_acknowledgement(source.root(), &model) {
        // A model that was only waiting for the note to be read is routable from here.
        Ok(_) => source.refresh_routing(),
        Err(fail) => lock(&source.work()).set_error(Some(fail.sentence)),
    }
}

/// Details' "Check again": re-hash the file, and let the row read Needs repair if it disagrees.
fn check(source: &Source, id: &str) {
    let Some(model) = source.model(id) else { return };
    let work = source.work();
    lock(&work).set_busy(Some(Busy::Checking { id: id.to_string() }));
    match models::verify_model(&model, source.root()) {
        Ok(_) => {
            let mut work = lock(&work);
            work.forget_damaged(id);
            work.set_error(None);
        }
        Err(fail) => {
            let mut work = lock(&work);
            work.mark_damaged(id);
            work.set_error(Some(fail.sentence));
        }
    }
    // MG-13 review round 3: a failed check deletes `verified.json`, which is the proof the queue
    // routes this task on, and a successful one writes it back. Either way the task list has to
    // follow, or the window reads Needs repair while PhotoCraft is still being offered the model.
    source.refresh_routing();
}

/// Brief rule 2: the window is the process that is serving, so it retires the task, waits out or
/// cancels the job inside it, and only then deletes. A job arriving afterwards gets the normal 400.
fn remove(source: &Source, id: &str) {
    let Some(model) = source.model(id) else { return };
    let work = source.work();
    lock(&work).set_busy(Some(Busy::Removing { id: id.to_string() }));
    let runner = source.runner();
    runner.retire_task(&model.task);
    if let Some(active) = runner.active().filter(|active| active.task == model.task) {
        let _ = runner.cancel(&active.id);
    }
    let deadline = Instant::now() + REMOVE_WAIT;
    while runner.active().is_some_and(|active| active.task == model.task) && Instant::now() < deadline {
        std::thread::sleep(REMOVE_POLL);
    }
    if runner.active().is_some_and(|active| active.task == model.task) {
        // A job that will not stop is a reason not to delete, not a reason to delete under it.
        // MG-13 review round 2: it is also a reason to keep routing to the model, which is still
        // installed and still ready.
        route_installed(source, &model.task);
        let mut work = lock(&work);
        work.set_error(Some("A job is still using that model, so nothing was deleted.".to_string()));
        return;
    }
    match models::remove_model(&model, source.root(), None, &|_| false) {
        Ok(_) => {
            let mut work = lock(&work);
            work.forget_damaged(id);
            work.forget_no_space(id);
            work.set_error(None);
            // The file is gone, so the task list has to stop saying it is ready.
            source.refresh_routing();
        }
        Err(fail) => {
            // The delete failed - a locked folder, a missing permission - so the model is still
            // there and has to keep working (MG-13 review round 2).
            route_installed(source, &model.task);
            lock(&work).set_error(Some(fail.sentence));
        }
    }
}

/// Section 4.3: the CUDA and cuDNN libraries, which are a download like any other except that they
/// are not a model and every feature keeps running on the processor while they arrive.
fn cuda_libraries(source: &Source, accept_note: bool, fetcher: &dyn Fetch, cancel: &AtomicBool) {
    let Some(root) = source.cuda_root() else {
        lock(&source.work()).set_error(Some("This computer has nowhere to keep the graphics card libraries.".to_string()));
        return;
    };
    let root = root.to_path_buf();
    let work = source.work();
    let mut overall = Cumulative { total: source.cuda_bytes(), base: 0, last: None };
    // Each archive reports its own bytes and its own size, so the window adds up the archives that
    // have already finished and keeps the one total the offer promised (MG-13 review round 2).
    let mut progress = move |done: u64, seen: u64| {
        let (done, total) = overall.report(done, seen);
        lock(&work).cuda_progress(done, total);
    };
    let outcome = cuda::install(&root, accept_note, fetcher, &mut progress);
    let guard = source.work();
    let mut work = lock(&guard);
    if let Err(fail) = outcome {
        note_failure(&mut work, "", &fail, cancel);
    }
}

/// What a failed download means. A cancelled one is a pause, not a problem: the part and its note
/// are still there, and the row reads Paused. A full disk is its own row in section 4.4.
fn note_failure(work: &mut Work, id: &str, fail: &Fail, cancel: &AtomicBool) {
    if cancel.load(Ordering::Relaxed) || fail.sentence.contains(CANCELLED) {
        return;
    }
    if !id.is_empty() && is_out_of_space(fail) {
        work.mark_no_space(id);
    }
    work.set_error(Some(fail.sentence.clone()));
}

/// Whether a failure is the disk running out rather than the network, the hash, or the person.
fn is_out_of_space(fail: &Fail) -> bool {
    fail.sentence.contains("os error 112") || fail.sentence.contains("os error 28") || fail.sentence.contains("not enough space")
}

/// The fetcher every worker uses: the real one, with a flag in the way of every read.
struct Cancellable {
    inner: fetch::HttpFetch,
    cancel: Arc<AtomicBool>,
}

impl Fetch for Cancellable {
    fn open(&self, url: &str) -> Result<Stream> {
        guarded(self.inner.open(url), Arc::clone(&self.cancel))
    }

    fn open_range(&self, url: &str, offset: u64) -> Result<Stream> {
        guarded(self.inner.open_range(url, offset), Arc::clone(&self.cancel))
    }
}

/// Put the flag in front of an open download's bytes.
///
/// MG-13 review round 2: the connection is read on a thread of its own. ureq gives a response body no
/// per-read timeout and no way to close it from another thread, so a worker parked inside a `read`
/// could sit there through a Cancel and keep the window from closing. Here the worker waits on a
/// channel instead, which it can always stop waiting on.
fn guarded(stream: Result<Stream>, cancel: Arc<AtomicBool>) -> Result<Stream> {
    let stream = stream?;
    let (claimed_len, answer) = (stream.claimed_len, stream.answer);
    let (tx, rx) = sync_channel::<io::Result<Vec<u8>>>(READ_QUEUE);
    let reader = std::thread::Builder::new().name("caim-read".to_string()).spawn(move || pump_bytes(stream.reader, tx));
    if reader.is_err() {
        return Err(Fail::internal("This computer would not start the reader for that download, so nothing was downloaded."));
    }
    Ok(Stream { claimed_len, answer, reader: Box::new(Guarded { bytes: rx, cancel, poll: READ_POLL, stall: READ_STALL, chunk: Vec::new(), finished: false }) })
}

/// Move the bytes from the connection into the channel, and stop as soon as either side goes away.
///
/// The reader is dropped when the download's own reader is dropped, which is how a cancelled or
/// failed download closes its connection. A reader parked in a stalled `read` ends when the
/// connection does: nothing in ureq 3.4 lets another thread close it, which is why the download
/// itself gives up at [READ_STALL] rather than waiting for that thread.
fn pump_bytes(mut reader: Box<dyn Read + Send>, tx: SyncSender<io::Result<Vec<u8>>>) {
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            // A clean end of the file: dropping the sender is what the reader below reads as 0.
            Ok(0) => break,
            Ok(read) => {
                if tx.send(Ok(buffer[..read].to_vec())).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(error));
                break;
            }
        }
    }
}

/// A reader that stops once the window has asked it to, even while the server is sending nothing.
struct Guarded {
    bytes: Receiver<io::Result<Vec<u8>>>,
    cancel: Arc<AtomicBool>,
    /// How long one wait for bytes lasts before the flag is looked at again.
    poll: Duration,
    /// How long the whole wait may last with nothing arriving at all.
    stall: Duration,
    /// The bytes that have arrived and not been asked for yet.
    chunk: Vec<u8>,
    /// Whether the end of the download has been seen, so a second read says 0 rather than waiting.
    finished: bool,
}

impl Read for Guarded {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut waited = Duration::ZERO;
        while self.chunk.is_empty() && !self.finished {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(io::Error::other(CANCELLED));
            }
            match self.bytes.recv_timeout(self.poll) {
                Ok(Ok(bytes)) => {
                    if bytes.is_empty() {
                        self.finished = true;
                    } else {
                        self.chunk = bytes;
                    }
                }
                Ok(Err(error)) => {
                    self.finished = true;
                    return Err(error);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    waited = waited.saturating_add(self.poll);
                    if waited >= self.stall {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, format!("the server sent nothing for {} seconds", self.stall.as_secs())));
                    }
                }
                // The reader thread has gone, which after a whole file is simply the end of it.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => self.finished = true,
            }
        }
        if self.chunk.is_empty() {
            return Ok(0);
        }
        let take = buffer.len().min(self.chunk.len());
        buffer[..take].copy_from_slice(&self.chunk[..take]);
        self.chunk.drain(..take);
        Ok(take)
    }
}

/// The CUDA archives' progress, added up into the one total the offer shows.
///
/// [crate::cuda::install] reports each archive on its own, with that archive's own bytes and its own
/// size, so the window cannot tell which archive it is inside. What it can tell is that the size
/// changed, or that the counter started again from the front: either one means the previous archive
/// is finished and its bytes belong to the running total.
///
/// MG-13 review round 3: an archive already on disk reports its size in one go, so a report that has
/// reached its own size is banked immediately rather than remembered until the next archive starts.
/// Without that, installing the libraries on a machine that already had one of them stopped short of
/// the total the offer promised, and the row never read finished.
struct Cumulative {
    total: u64,
    base: u64,
    last: Option<(u64, u64)>,
}

impl Cumulative {
    /// The next report, as bytes done out of the whole download.
    fn report(&mut self, done: u64, seen: u64) -> (u64, u64) {
        if let Some((last_done, last_seen)) = self.last
            && (seen != last_seen || done < last_done)
        {
            self.base = self.base.saturating_add(last_seen);
            self.last = None;
        }
        if done >= seen {
            self.base = self.base.saturating_add(seen);
            self.last = None;
            return (self.base, self.total);
        }
        self.last = Some((done, seen));
        (self.base.saturating_add(done), self.total)
    }
}

/// Take a lock. A worker stopped mid-write leaves its data consistent, so a poisoned lock is
/// unwrapped rather than turned into a panic the window would never see.
fn lock<T>(cell: &Arc<Mutex<T>>) -> MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Frame;
    use crate::protocol::TASK_FILL;
    use crate::server::{ServeOptions, Server};
    use crate::testkit::Scratch;
    use std::path::PathBuf;

    /// A download's reader, with the channel the reader thread would fill held open by the test.
    fn waiting_reader(poll: Duration, stall: Duration) -> (Guarded, SyncSender<io::Result<Vec<u8>>>, Arc<AtomicBool>) {
        let (tx, rx) = sync_channel::<io::Result<Vec<u8>>>(READ_QUEUE);
        let cancel = Arc::new(AtomicBool::new(false));
        (Guarded { bytes: rx, cancel: Arc::clone(&cancel), poll, stall, chunk: Vec::new(), finished: false }, tx, cancel)
    }

    /// MG-13 review round 2: Cancel has to be seen even while the worker is waiting for bytes.
    #[test]
    fn a_cancel_is_observed_while_the_server_is_sending_nothing() {
        let (mut reader, _connection, cancel) = waiting_reader(Duration::from_millis(5), Duration::from_secs(30));
        let flag = Arc::clone(&cancel);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let mut buffer = [0u8; 16];
        let error = reader.read(&mut buffer).expect_err("the cancel ends the wait");
        setter.join().expect("the cancel arrived");
        assert_eq!(error.to_string(), CANCELLED, "the same sentence a read between chunks gives");
        assert!(started.elapsed() < Duration::from_secs(5), "the wait ended at the flag, not at the deadline: {:?}", started.elapsed());
    }

    /// A server that stops answering is a lost download, not a window that never closes.
    #[test]
    fn a_server_that_sends_nothing_at_all_ends_the_download_at_the_stall_deadline() {
        let (mut reader, _connection, _cancel) = waiting_reader(Duration::from_millis(5), Duration::from_millis(120));
        let started = Instant::now();
        let mut buffer = [0u8; 16];
        let error = reader.read(&mut buffer).expect_err("a silent server cannot hold the window open");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "the download reads as lost, so the part and its note stay: {error}");
        assert!(started.elapsed() >= Duration::from_millis(120), "the deadline is the deadline");
    }

    #[test]
    fn the_bytes_arrive_in_order_and_the_end_of_the_file_reads_as_the_end() {
        let (mut reader, tx, _cancel) = waiting_reader(Duration::from_millis(5), Duration::from_secs(30));
        tx.send(Ok(vec![1, 2, 3])).expect("the first chunk arrives");
        tx.send(Ok(vec![4, 5])).expect("the second chunk arrives");
        drop(tx);
        let mut got = Vec::new();
        let mut buffer = [0u8; 4];
        loop {
            let read = reader.read(&mut buffer).expect("the bytes are there");
            if read == 0 {
                break;
            }
            got.extend_from_slice(&buffer[..read]);
        }
        assert_eq!(got, vec![1, 2, 3, 4, 5], "the chunks keep their order, even split across buffers");
        assert_eq!(reader.read(&mut buffer).expect("the end stays the end"), 0, "a read after the end is still the end");
    }

    #[test]
    fn cuda_progress_adds_up_across_the_archives() {
        let mut overall = Cumulative { total: 1_800_000_000, base: 0, last: None };
        assert_eq!(overall.report(1_000_000, 3_000_000), (1_000_000, 1_800_000_000), "the first archive reports its own size");
        assert_eq!(overall.report(3_000_000, 3_000_000), (3_000_000, 1_800_000_000));
        assert_eq!(overall.report(0, 1_000_000_000), (3_000_000, 1_800_000_000), "the next archive starts again without resetting the total");
        assert_eq!(overall.report(500_000_000, 1_000_000_000), (503_000_000, 1_800_000_000));
        assert_eq!(overall.report(1_000_000_000, 1_000_000_000), (1_003_000_000, 1_800_000_000));
    }

    #[test]
    fn two_archives_of_the_same_size_are_still_two_archives() {
        let mut overall = Cumulative { total: 100, base: 0, last: None };
        assert_eq!(overall.report(10, 50), (10, 100));
        assert_eq!(overall.report(50, 50), (50, 100));
        assert_eq!(overall.report(0, 50), (50, 100), "the counter starting again from the front is a new archive");
        assert_eq!(overall.report(50, 50), (100, 100));
    }

    /// A server answering from a scratch store, and the window that shares the process with it.
    /// MG-13 review round 3: the task list a press changes lives in the server, so a test that wants
    /// to see what a press does to it needs both halves.
    fn window(root: PathBuf, fake_backend: bool) -> (Server, Arc<Source>) {
        let server = Server::start(ServeOptions {
            fake_backend,
            models_root: Some(root.clone()),
            endpoint_file: Some(root.join("endpoint.json")),
            step_delay: Duration::ZERO,
            ..ServeOptions::default()
        })
        .expect("the server starts");
        let source = Arc::new(Source::at_root(server.runner(), server.port(), None, server.routing(), root).expect("the window has a store"));
        (server, source)
    }

    /// The smallest job the queue will take.
    fn frame() -> Frame {
        Frame { width: 2, height: 2, channels: 3, bit_depth: 8, samples: vec![10, 20, 30, 200, 100, 50, 40, 60, 80, 90, 20, 10] }
    }

    fn mask() -> Vec<u8> {
        vec![255, 0, 0, 0]
    }

    /// MG-13 review round 3: two presses in one repaint used to start two workers over the same part
    /// file, because the first worker has nothing to report until its first byte arrives.
    #[test]
    fn a_second_press_is_refused_before_the_first_worker_has_reported_anything() {
        let scratch = Scratch::new("task-claim");
        let (_server, source) = window(scratch.path.clone(), true);
        let work = source.work();
        assert!(lock(&work).claim(), "an idle window can be taken");
        let fail = match start(&source, Task::Check { id: "lama".to_string() }) {
            Ok(_) => panic!("the slot was already taken, so no second worker should have started"),
            Err(fail) => fail,
        };
        assert_eq!(fail.status.code(), 409, "{fail}");
        assert_eq!(fail.sentence, "Something else is happening in this window right now.");
        lock(&work).release();
        let fail = match start(&source, Task::Check { id: "no-such-model".to_string() }) {
            Ok(_) => panic!("the catalog has no such model"),
            Err(fail) => fail,
        };
        assert_eq!(fail.status.code(), 404, "{fail}");
        assert!(lock(&work).claim(), "a press refused before its worker started gives the window back");
        lock(&work).release();
    }

    /// MG-13 review round 3: a failed Check deletes the verification the queue routes on, so the task
    /// list has to be worked out again - otherwise the row reads Needs repair while PhotoCraft is
    /// still being offered the model.
    #[test]
    fn a_failed_check_takes_the_model_off_the_task_list_the_server_answers_with() {
        let scratch = Scratch::new("task-check-routing");
        let root = scratch.path.clone();
        let (_server, source) = window(root.clone(), false);
        let runner = source.runner();
        let model = source.model("lama").expect("the catalog has a model for the fill task");
        std::fs::create_dir_all(store::model_folder(&root, &model)).expect("the model folder");
        let file = std::fs::File::create(store::model_file(&root, &model)).expect("the model file");
        file.set_len(model.size).expect("a file the size the catalog promises");
        store::write_verified(&root, &model, &model.sha256, model.size).expect("the verification record");
        store::write_acknowledgement(&root, &model).expect("the note has been read");
        source.refresh_routing();
        assert!(runner.offered().is_ready(TASK_FILL), "a whole model the person has acknowledged is offered");
        check(&source, "lama");
        assert!(lock(&source.work()).is_damaged("lama"), "the row reads Needs repair");
        assert!(!runner.offered().is_ready(TASK_FILL), "and the server says the same thing without a restart");
    }

    /// MG-13 review round 3: a Remove that gave up halfway retired the task, and a model installed
    /// afterwards stayed unroutable until Craft AI Manager restarted.
    #[test]
    fn installing_a_model_clears_the_retirement_an_abandoned_remove_left_behind() {
        let scratch = Scratch::new("task-restore-routing");
        let (_server, source) = window(scratch.path.clone(), true);
        let runner = source.runner();
        runner.retire_task(TASK_FILL);
        let fail = runner.submit(TASK_FILL, frame(), mask()).expect_err("the task is still retired");
        assert_eq!(fail.status.code(), 400, "{fail}");
        route_installed(&source, TASK_FILL);
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("the model the window installed routes again");
        assert!(runner.state(&id).is_ok(), "and the job is in the queue: {id}");
        runner.stop();
    }

    /// MG-13 review round 3: an archive that is already on disk reports its size in one go, and those
    /// bytes belong to the total the offer showed. The four sizes are the archives NVIDIA pins today.
    #[test]
    fn an_archive_that_is_already_on_disk_still_counts_towards_the_total() {
        let sizes = [2_737_397_u64, 422_415_931, 159_602_693, 1_330_803_635];
        let total: u64 = sizes.iter().sum();
        let mut overall = Cumulative { total, base: 0, last: None };
        let mut done = 0u64;
        for (index, size) in sizes.iter().enumerate() {
            if index == 0 {
                let report = overall.report(*size, *size);
                assert_eq!(report, (*size, total), "an archive that did not have to be fetched still reports its bytes");
                done = *size;
                continue;
            }
            assert_eq!(overall.report(0, *size), (done, total), "the next archive starts again without losing what came before");
            done += size;
            assert_eq!(overall.report(*size, *size), (done, total), "and it counts when it finishes");
        }
        assert_eq!(done, total, "the four archives are the whole download the offer promised, so the row can read finished");
    }
}
