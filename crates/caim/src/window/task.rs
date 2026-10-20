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

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
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
    if lock(&work).busy().is_some() {
        return Err(Fail::conflict("Something else is happening in this window right now."));
    }
    if let Some(id) = task_id(&task)
        && source.model(id).is_none()
    {
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
        Err(_) => Err(Fail::internal("This computer would not start the work, so nothing was changed.")),
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
    let work = source.work();
    match task {
        Task::Install { id, accept_note } => download(source, id, *accept_note, fetcher, cancel),
        Task::Repair { id } => repair(source, id, fetcher, cancel),
        Task::Resume { id } => download(source, id, false, fetcher, cancel),
        Task::Acknowledge { id } => acknowledge(source, id),
        Task::Check { id } => check(source, id),
        Task::Remove { id } => remove(source, id),
        Task::Cuda { accept_note } => cuda_libraries(source, *accept_note, fetcher, cancel),
    }
    lock(&work).set_busy(None);
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
    let guard = source.work();
    let mut work = lock(&guard);
    work.forget_no_space(id);
    match outcome {
        Ok(_) => work.forget_damaged(id),
        Err(fail) => note_failure(&mut work, id, &fail, cancel),
    }
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
    if let Err(fail) = store::write_acknowledgement(source.root(), &model) {
        lock(&source.work()).set_error(Some(fail.sentence));
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
        }
        Err(fail) => lock(&work).set_error(Some(fail.sentence)),
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
    let total = source.cuda_bytes();
    let mut progress = move |done: u64, seen: u64| {
        lock(&work).cuda_progress(done, if seen == 0 { total } else { seen });
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
fn guarded(stream: Result<Stream>, cancel: Arc<AtomicBool>) -> Result<Stream> {
    let stream = stream?;
    Ok(Stream { claimed_len: stream.claimed_len, answer: stream.answer, reader: Box::new(Guarded { reader: stream.reader, cancel }) })
}

/// A reader that stops once the window has asked it to.
struct Guarded {
    reader: Box<dyn Read>,
    cancel: Arc<AtomicBool>,
}

impl Read for Guarded {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other(CANCELLED));
        }
        self.reader.read(buffer)
    }
}

/// Take a lock. A worker stopped mid-write leaves its data consistent, so a poisoned lock is
/// unwrapped rather than turned into a panic the window would never see.
fn lock<T>(cell: &Arc<Mutex<T>>) -> MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
