//! The job side of the protocol: a FIFO queue, one worker thread, and the states a job moves
//! through while it runs. Amendment 11 fixes the states (`queued`, `running`, `done`, `failed`,
//! `cancelled`), the queue cap of eight, and the ten minutes a finished result is kept for.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{self, Filled};
use crate::error::{Fail, Result};
use crate::image::{self, Frame};
use crate::lama;
use crate::memory::{self, Budget, Reservation};

/// Amendment 11: at most eight jobs queued or running at once; the ninth is a 503.
pub const MAX_QUEUE: usize = 8;
/// Cap image working data and results independently of the number of live jobs.
pub const DEFAULT_MEMORY_BYTES: usize = memory::DEFAULT_LIMIT;
const MAX_RECORDS: usize = 1024;
/// Amendment 11: a finished result is kept for ten minutes, or until it has been read.
pub const RESULT_TTL: Duration = Duration::from_secs(600);
/// How long the worker waits for the next job before it sweeps finished results.
const WORKER_POLL: Duration = Duration::from_millis(50);
/// The sentence for a job that is not finished yet.
const NOT_READY: &str = "The result is not ready yet.";
/// The sentence for an id this manager has never heard of.
const NO_SUCH_JOB: &str = "Craft AI Manager has no job with that id.";
/// The sentence for a job the worker could not be handed, which only happens once it has stopped.
const NO_WORKER: &str = "Craft AI Manager's job worker has stopped, so it cannot take this job. Restart Craft AI Manager and try again.";
/// What a cancelled job's poll says, whatever stage it was in.
const CANCELLED_MESSAGE: &str = "Cancelled.";

/// Where a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting in the queue.
    Queued,
    /// Being filled by the backend.
    Running,
    /// The result is ready.
    Done,
    /// The backend could not finish it.
    Failed,
    /// A `DELETE` stopped it, or the manager is shutting down.
    Cancelled,
}

impl State {
    /// The name on the wire.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// A finished job cannot be cancelled, and its result will not change.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// What the worker runs a job on (MG-04 item 2). `caim serve` chooses one at start:
///
/// * `Lama` when MG-02 says the model is installed and acknowledged;
/// * `Fake` under `--fake-backend`, exactly as MG-01 had it;
/// * `NoModel` otherwise, which keeps MG-01's answer and sentence.
#[derive(Clone, Debug, Default)]
pub enum Backend {
    /// Nothing is installed, so a job is refused with amendment 11's sentence.
    #[default]
    NoModel,
    /// The built-in fake backend, its ten steps spaced by `step_delay`.
    Fake { step_delay: Duration },
    /// The real LaMa runner; its session is loaded on the first job and kept after that.
    Lama(Arc<lama::Pipeline>),
}

impl Backend {
    /// One line for the terminal: which backend this start is running.
    #[must_use]
    pub fn note(&self) -> &'static str {
        match self {
            Backend::NoModel => "none: no model is installed yet",
            Backend::Fake { .. } => "fake-fill (the built-in mean-colour fill)",
            Backend::Lama(_) => "lama (LaMa on the local CPU, loaded on the first job)",
        }
    }
}

/// What `GET /v1/jobs/{id}/result` reports: amendment 14 wants this pinned to the layer later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub model: String,
    pub device: String,
    pub provider: String,
    pub elapsed_ms: u64,
    pub image_size: [u32; 2],
}

/// One job, from the moment it is accepted until its result has been read or has expired.
struct Entry {
    id: String,
    state: State,
    progress: f64,
    message: String,
    /// The backend checks this between row groups, which is how `DELETE` cancels a running job.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Taken by the worker when the job starts; `None` once it has.
    input: Option<(Frame, Vec<u8>)>,
    /// The encoded `result.png`, freed as soon as it has been read.
    png: Option<Vec<u8>>,
    delivering: bool,
    memory: Option<Reservation>,
    provenance: Option<Provenance>,
    /// When the job finished, which starts the ten-minute clock.
    finished_at: Option<Instant>,
}

/// The jobs this manager knows about, in the order they arrived.
struct Shared {
    jobs: Vec<Entry>,
    next_id: u64,
    pending: usize,
}

/// A queue slot is reserved before HTTP buffers a body; all failure paths release it.
pub(crate) struct Admission {
    shared: Arc<Mutex<Shared>>,
    budget: Budget,
    body: Option<Reservation>,
    work: Option<Reservation>,
    pending: bool,
}

impl Admission {
    pub(crate) fn reserve_work(&mut self, width: u32, height: u32) -> Result<()> {
        self.work = Some(self.budget.reserve(memory::work_bytes(width, height)?)?);
        Ok(())
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if self.pending {
            let mut shared = lock(&self.shared);
            shared.pending = shared.pending.saturating_sub(1);
        }
    }
}

/// A failed or unwound write restores the only PNG; a completed write consumes it.
struct Delivery {
    shared: Arc<Mutex<Shared>>,
    id: String,
    png: Option<Vec<u8>>,
    memory: Option<Reservation>,
}

impl Drop for Delivery {
    fn drop(&mut self) {
        if let Some(entry) = find_mut(&mut lock(&self.shared).jobs, &self.id) {
            entry.delivering = false;
            entry.png = self.png.take();
            entry.memory = self.memory.take();
        }
    }
}

/// A lock that keeps working if another thread panicked while holding it: the worst case is a job
/// that fails, never a crash.
fn lock(shared: &Arc<Mutex<Shared>>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The job with this id, if this manager still knows about it.
fn find_mut<'a>(jobs: &'a mut [Entry], id: &str) -> Option<&'a mut Entry> {
    jobs.iter_mut().find(|entry| entry.id == id)
}

fn find<'a>(jobs: &'a [Entry], id: &str) -> Option<&'a Entry> {
    jobs.iter().find(|entry| entry.id == id)
}

/// The queue, the worker, and everything the HTTP layer needs to answer the jobs endpoints.
///
/// The MG-01 backend is the fake one; MG-04 swaps a real runner in behind the same interface.
pub struct Runner {
    shared: Arc<Mutex<Shared>>,
    queue_tx: SyncSender<String>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    max_queue: usize,
    budget: Budget,
}

impl Runner {
    /// Start the worker. The fake backend's `step_delay` spaces its ten progress steps, which is
    /// what lets the tests have eight jobs in flight at once.
    pub fn new(backend: Backend, max_queue: usize) -> Result<Runner> {
        Self::with_memory_limit(backend, max_queue, DEFAULT_MEMORY_BYTES)
    }

    pub(crate) fn with_memory_limit(backend: Backend, max_queue: usize, memory_limit: usize) -> Result<Runner> {
        let (queue_tx, queue_rx) = std::sync::mpsc::sync_channel::<String>(MAX_RECORDS);
        let shared = Arc::new(Mutex::new(Shared { jobs: Vec::new(), next_id: 0, pending: 0 }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_stop = Arc::clone(&stop);
        let backend = Arc::new(backend);
        let handle = std::thread::Builder::new()
            .name("caim-jobs".to_string())
            .spawn(move || worker(worker_shared, queue_rx, worker_stop, backend))
            .map_err(|error| Fail::internal(format!("Craft AI Manager could not start its job worker: {error}")))?;
        Ok(Runner { shared, queue_tx, worker: Mutex::new(Some(handle)), stop, max_queue, budget: Budget::new(memory_limit) })
    }

    pub(crate) fn admit(&self, body_bytes: usize) -> Result<Admission> {
        let mut shared = lock(&self.shared);
        if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Fail::internal(NO_WORKER));
        }
        sweep_locked(&mut shared);
        let in_flight = shared.jobs.iter().filter(|entry| !entry.state.is_finished()).count();
        if in_flight.saturating_add(shared.pending) >= self.max_queue {
            return Err(Fail::busy(format!("Craft AI Manager already has {} jobs waiting, so it cannot take another one yet.", self.max_queue)));
        }
        if shared.jobs.len().saturating_add(shared.pending) >= MAX_RECORDS {
            return Err(Fail::busy("Craft AI Manager's retained job records are full. Try again after results expire."));
        }
        let body = self.budget.reserve(body_bytes)?;
        shared.pending += 1;
        Ok(Admission { shared: Arc::clone(&self.shared), budget: self.budget.clone(), body: Some(body), work: None, pending: true })
    }

    /// Accept a job and put it at the back of the queue. The id is opaque to the client, and safe
    /// to put in a URL path.
    pub fn submit(&self, frame: Frame, mask: Vec<u8>) -> Result<String> {
        let mut admission = self.admit(0)?;
        admission.reserve_work(frame.width, frame.height)?;
        self.submit_admitted(frame, mask, admission)
    }

    pub(crate) fn submit_admitted(&self, frame: Frame, mask: Vec<u8>, mut admission: Admission) -> Result<String> {
        admission.body = None;
        let id = {
            let mut shared = lock(&self.shared);
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                drop(shared);
                return Err(Fail::internal(NO_WORKER));
            }
            shared.pending = shared.pending.saturating_sub(1);
            admission.pending = false;
            shared.next_id = shared.next_id.wrapping_add(1);
            let id = format!("job-{}", shared.next_id);
            shared.jobs.push(Entry {
                id: id.clone(),
                state: State::Queued,
                progress: 0.0,
                message: "Waiting for Craft AI Manager.".to_string(),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                input: Some((frame, mask)),
                png: None,
                delivering: false,
                memory: admission.work.take(),
                provenance: None,
                finished_at: None,
            });
            id
        };
        if let Err(error) = self.queue_tx.try_send(id.clone()) {
            // C8: the worker is gone, so the job would sit in the queue for ever and count against
            // the cap. Forget it and say what actually went wrong.
            lock(&self.shared).jobs.retain(|entry| entry.id != id);
            return Err(match error {
                TrySendError::Full(_) => Fail::busy("Craft AI Manager's worker queue is full. Try again shortly."),
                TrySendError::Disconnected(_) => Fail::internal(NO_WORKER),
            });
        }
        Ok(id)
    }

    /// `DELETE /v1/jobs/{id}`: cancel a queued or running job. A finished job answers the same way.
    pub fn cancel(&self, id: &str) -> Result<()> {
        let mut shared = lock(&self.shared);
        let Some(entry) = find_mut(&mut shared.jobs, id) else {
            return Err(Fail::not_found(NO_SUCH_JOB));
        };
        if !entry.state.is_finished() {
            // C3: a queued job never reaches `finish`, because the worker skips it, so its input is
            // dropped and its ten-minute clock started here. A running job's worker does both.
            if entry.state == State::Queued {
                entry.input = None;
                entry.memory = None;
                entry.finished_at = Some(Instant::now());
            }
            entry.state = State::Cancelled;
            entry.message = CANCELLED_MESSAGE.to_string();
            entry.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    /// `GET /v1/jobs/{id}`.
    pub fn state(&self, id: &str) -> Result<(State, f64, String)> {
        let shared = lock(&self.shared);
        let Some(entry) = find(&shared.jobs, id) else {
            return Err(Fail::not_found(NO_SUCH_JOB));
        };
        Ok((entry.state, entry.progress, entry.message.clone()))
    }

    /// `GET /v1/jobs/{id}/result`: 409 until the job is done.
    pub fn provenance(&self, id: &str) -> Result<Provenance> {
        let shared = lock(&self.shared);
        let Some(entry) = find(&shared.jobs, id) else {
            return Err(Fail::not_found(NO_SUCH_JOB));
        };
        match entry.state {
            State::Done => entry.provenance.clone().ok_or_else(|| Fail::conflict(NOT_READY)),
            State::Queued | State::Running => Err(Fail::conflict(NOT_READY)),
            State::Failed => Err(Fail::conflict(entry.message.clone())),
            State::Cancelled => Err(Fail::conflict("The job was cancelled, so it has no result.")),
        }
    }

    /// `GET /v1/jobs/{id}/result.png`. The encoded image is freed once it has been read, which is
    /// the "or until read" half of the ten-minute rule; the job record stays so polling keeps
    /// working until the record itself expires.
    pub fn take_png(&self, id: &str) -> Result<Vec<u8>> {
        let mut delivery = self.begin_delivery(id)?;
        delivery.memory = None;
        delivery.png.take().ok_or_else(|| Fail::not_found("That result has already been read."))
    }

    fn begin_delivery(&self, id: &str) -> Result<Delivery> {
        let mut shared = lock(&self.shared);
        let Some(entry) = find_mut(&mut shared.jobs, id) else {
            return Err(Fail::not_found(NO_SUCH_JOB));
        };
        match entry.state {
            State::Done => {}
            State::Queued | State::Running => return Err(Fail::conflict(NOT_READY)),
            State::Failed => return Err(Fail::conflict(entry.message.clone())),
            State::Cancelled => return Err(Fail::conflict("The job was cancelled, so it has no result.")),
        }
        if entry.delivering {
            return Err(Fail::conflict("That result is being transferred. Retry when the transfer ends."));
        }
        let png = entry.png.take().ok_or_else(|| Fail::not_found("That result has already been read."))?;
        entry.delivering = true;
        Ok(Delivery { shared: Arc::clone(&self.shared), id: id.to_string(), png: Some(png), memory: entry.memory.take() })
    }

    pub(crate) fn deliver_png(&self, id: &str, send: impl FnOnce(&[u8]) -> Result<()>) -> Result<()> {
        let mut delivery = self.begin_delivery(id)?;
        send(delivery.png.as_deref().unwrap_or_default())?;
        delivery.png = None;
        delivery.memory = None;
        Ok(())
    }

    pub(crate) fn request_stop(&self) {
        let mut shared = lock(&self.shared);
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for entry in &mut shared.jobs {
            if !entry.state.is_finished() {
                if entry.state == State::Queued {
                    entry.input = None;
                    entry.memory = None;
                    entry.finished_at = Some(Instant::now());
                }
                entry.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                entry.state = State::Cancelled;
                entry.message = CANCELLED_MESSAGE.to_string();
            }
        }
    }

    /// Stop the worker and wait for the job it is on to finish.
    pub fn stop(&self) {
        self.request_stop();
        if let Some(handle) = self.worker.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The worker loop: take the next job, run it, then sweep finished results.
fn worker(shared: Arc<Mutex<Shared>>, queue_rx: Receiver<String>, stop: Arc<std::sync::atomic::AtomicBool>, backend: Arc<Backend>) {
    use std::sync::atomic::Ordering;
    while !stop.load(Ordering::Relaxed) {
        match queue_rx.recv_timeout(WORKER_POLL) {
            Ok(id) => run_one(&shared, &id, &backend),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        sweep(&shared);
    }
}

/// Run one job from the queue. A job cancelled while it waited never starts.
fn run_one(shared: &Arc<Mutex<Shared>>, id: &str, backend: &Backend) {
    let taken = {
        let mut shared = lock(shared);
        match find_mut(&mut shared.jobs, id) {
            Some(entry) if entry.state == State::Queued => {
                entry.state = State::Running;
                entry.message = "Craft AI Manager is filling the masked area.".to_string();
                entry.input.take()
            }
            _ => None,
        }
    };
    let Some((image, mask)) = taken else { return };
    let cancel = match find(&lock(shared).jobs, id) {
        Some(entry) => Arc::clone(&entry.cancel),
        None => return,
    };
    // The progress callback takes the lock only for the assignment, never across the fill. Each
    // report names the stage it belongs to, which is what tells PhotoCraft how far a job has got
    // (MG-05).
    let progress_shared = Arc::clone(shared);
    let progress_id = id.to_string();
    let mut progress = move |value: f64, message: &str| {
        // C4: a stage that finishes after a `DELETE` must not overwrite "Cancelled.".
        if let Some(entry) = find_mut(&mut lock(&progress_shared).jobs, &progress_id)
            && !entry.state.is_finished()
        {
            entry.progress = value;
            entry.message = message.to_string();
        }
    };
    let outcome = match backend {
        Backend::NoModel => Err(Fail::bad_request("No model is installed for fill_masked_area yet")),
        Backend::Fake { step_delay } => backend::run_fill(&image, &mask, &cancel, *step_delay, &mut progress),
        Backend::Lama(pipeline) => pipeline.run_fill(&image, &mask, &cancel, &mut progress),
    };
    drop(image);
    drop(mask);
    finish(shared, id, &cancel, outcome);
}

/// Record how the job ended. A job the client already cancelled stays cancelled.
fn finish(shared: &Arc<Mutex<Shared>>, id: &str, cancel: &std::sync::atomic::AtomicBool, outcome: Result<Filled>) {
    // Avoid encoding a full image after shutdown or DELETE has already discarded the job.
    let outcome = if cancel.load(std::sync::atomic::Ordering::Relaxed) { Err(Fail::internal(CANCELLED_MESSAGE)) } else { outcome };
    let ready = match outcome {
        Ok(filled) => match image::encode(&filled.frame) {
            Ok(png) => Ok((
                png,
                Provenance {
                    model: filled.model.to_string(),
                    device: filled.device.to_string(),
                    provider: filled.provider.to_string(),
                    elapsed_ms: filled.elapsed_ms,
                    image_size: [filled.frame.width, filled.frame.height],
                },
            )),
            Err(fail) => Err(fail),
        },
        Err(fail) => Err(fail),
    };
    let mut shared = lock(shared);
    let Some(entry) = find_mut(&mut shared.jobs, id) else { return };
    entry.finished_at = Some(Instant::now());
    if entry.state == State::Cancelled {
        entry.memory = None;
        entry.message = CANCELLED_MESSAGE.to_string();
        return;
    }
    match ready {
        Ok((png, provenance)) => {
            if let Some(memory) = entry.memory.as_mut() {
                // This only shrinks the conservative working-space reservation.
                if let Err(fail) = memory.resize(png.capacity()) {
                    entry.memory = None;
                    entry.state = State::Failed;
                    entry.message = fail.sentence;
                    return;
                }
            }
            entry.state = State::Done;
            entry.progress = 1.0;
            entry.message = "Done.".to_string();
            entry.png = Some(png);
            entry.provenance = Some(provenance);
        }
        Err(fail) => {
            entry.memory = None;
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                entry.state = State::Cancelled;
                entry.message = CANCELLED_MESSAGE.to_string();
            } else {
                entry.state = State::Failed;
                entry.message = fail.sentence;
            }
        }
    }
}

/// Drop the records whose ten minutes are up.
fn sweep(shared: &Arc<Mutex<Shared>>) {
    sweep_locked(&mut lock(shared));
}

fn sweep_locked(shared: &mut Shared) {
    let now = Instant::now();
    shared.jobs.retain(|entry| {
        entry.delivering
            || match entry.finished_at {
                Some(finished) => now.saturating_duration_since(finished) < RESULT_TTL,
                None => true,
            }
    });
}

#[cfg(test)]
mod tests {
    use super::{Backend, MAX_QUEUE, Provenance, Runner, State, find, lock};
    use crate::backend;
    use crate::image::Frame;
    use crate::lama;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// How long a test waits for the worker. Generous on purpose: tests build unoptimised, and a
    /// slow CI runner can take most of a second just to prepare a 512x512 LaMa input.
    const PATIENCE: Duration = Duration::from_secs(30);

    fn frame() -> Frame {
        Frame { width: 2, height: 2, channels: 3, bit_depth: 8, samples: vec![10, 20, 30, 200, 100, 50, 40, 60, 80, 90, 20, 10] }
    }

    #[test]
    fn shutdown_cancels_active_and_queued_work_and_refuses_new_work() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(40) }, MAX_QUEUE).expect("worker");
        let active = runner.submit(frame(), mask()).expect("active");
        assert_eq!(settle(&runner, &active), State::Running);
        let queued = runner.submit(frame(), mask()).expect("queued");
        runner.stop();
        for id in [&active, &queued] {
            assert_eq!(runner.state(id).expect("record").0, State::Cancelled);
            let shared = lock(&runner.shared);
            let entry = find(&shared.jobs, id).expect("entry");
            assert!(entry.input.is_none() && entry.memory.is_none() && entry.finished_at.is_some());
        }
        assert!(runner.submit(frame(), mask()).is_err());
    }

    #[test]
    fn pending_uploads_reserve_queue_slots_and_memory_and_release_both() {
        let runner = Runner::with_memory_limit(Backend::Fake { step_delay: Duration::ZERO }, 2, 100).expect("worker");
        let first = runner.admit(60).expect("first upload");
        assert!(runner.admit(41).is_err());
        let second = runner.admit(40).expect("second upload");
        assert!(runner.admit(0).is_err(), "upload reservations count against the queue cap");
        drop(first);
        drop(second);
        runner.admit(100).expect("all reservations returned");
    }

    #[test]
    fn an_upload_admitted_before_shutdown_cannot_enqueue_after_shutdown() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("worker");
        let mut admission = runner.admit(20).expect("upload begins");
        admission.reserve_work(2, 2).expect("working space");
        runner.stop();
        assert!(runner.submit_admitted(frame(), mask(), admission).is_err());
        let shared = lock(&runner.shared);
        assert_eq!(shared.pending, 0);
        assert!(shared.jobs.is_empty());
        runner.budget.reserve(super::DEFAULT_MEMORY_BYTES).expect("upload memory returned");
    }

    #[test]
    fn expired_results_release_memory() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("worker");
        let id = runner.submit(frame(), mask()).expect("job");
        assert_eq!(finish(&runner, &id), State::Done);
        {
            let mut shared = lock(&runner.shared);
            super::find_mut(&mut shared.jobs, &id).expect("entry").finished_at = Some(std::time::Instant::now() - super::RESULT_TTL);
            super::sweep_locked(&mut shared);
            assert!(shared.jobs.is_empty());
        }
        runner.budget.reserve(super::DEFAULT_MEMORY_BYTES).expect("expired PNG memory returned");
    }

    #[test]
    fn failed_result_writes_restore_the_png_and_its_memory_reservation() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("worker");
        let id = runner.submit(frame(), mask()).expect("job");
        assert_eq!(finish(&runner, &id), State::Done);
        let mut expected = Vec::new();
        assert!(
            runner
                .deliver_png(&id, |png| {
                    expected = png.to_vec();
                    assert!(runner.budget.reserve(super::DEFAULT_MEMORY_BYTES).is_err(), "in-flight PNG is still charged");
                    assert!(runner.deliver_png(&id, |_| Ok(())).is_err(), "only one transfer at once");
                    Err(crate::error::Fail::internal("simulated partial socket write"))
                })
                .is_err()
        );
        assert!(runner.budget.reserve(super::DEFAULT_MEMORY_BYTES).is_err(), "failed transfer retains its charge");
        runner
            .deliver_png(&id, |png| {
                assert_eq!(png, expected);
                Ok(())
            })
            .expect("retry succeeds");
        assert!(runner.take_png(&id).is_err(), "a successful transfer consumes the result");
        runner.budget.reserve(super::DEFAULT_MEMORY_BYTES).expect("successful transfer releases its memory");
    }

    #[test]
    fn a_result_lease_survives_expiry_while_a_transfer_is_in_progress() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("worker");
        let id = runner.submit(frame(), mask()).expect("job");
        assert_eq!(finish(&runner, &id), State::Done);
        runner
            .deliver_png(&id, |_| {
                let mut shared = lock(&runner.shared);
                super::find_mut(&mut shared.jobs, &id).expect("entry").finished_at = Some(std::time::Instant::now() - super::RESULT_TTL);
                super::sweep_locked(&mut shared);
                assert!(find(&shared.jobs, &id).is_some(), "do not evict an active transfer");
                Ok(())
            })
            .expect("transfer completes");
    }

    #[test]
    fn retained_results_block_over_budget_uploads_until_consumed() {
        let limit = crate::memory::work_bytes(2, 2).expect("working space");
        let runner = Runner::with_memory_limit(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE, limit).expect("worker");
        let id = runner.submit(frame(), mask()).expect("job");
        assert_eq!(finish(&runner, &id), State::Done);
        assert!(runner.admit(limit).is_err(), "the retained result uses part of the budget");
        runner.take_png(&id).expect("consume result");
        runner.admit(limit).expect("the full budget is now available");
    }

    fn mask() -> Vec<u8> {
        vec![255, 0, 0, 0]
    }

    /// Wait for a job to reach a final state, without spinning forever if it never does.
    fn finish(runner: &Runner, id: &str) -> State {
        let deadline = std::time::Instant::now() + PATIENCE;
        while std::time::Instant::now() < deadline {
            let (state, _, _) = runner.state(id).expect("the job exists");
            if state.is_finished() {
                return state;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        runner.state(id).expect("the job exists").0
    }

    /// Wait for a job to leave the queue, without spinning forever if it never does.
    fn settle(runner: &Runner, id: &str) -> State {
        let deadline = std::time::Instant::now() + PATIENCE;
        while std::time::Instant::now() < deadline {
            let (state, _, _) = runner.state(id).expect("the job exists");
            if state.is_finished() || state == State::Running {
                return state;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        runner.state(id).expect("the job exists").0
    }

    #[test]
    fn a_job_goes_from_queued_to_done_with_a_result() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("the worker starts");
        let id = runner.submit(frame(), mask()).expect("the job is accepted");
        assert_eq!(id, "job-1");
        assert_eq!(finish(&runner, &id), State::Done);
        let provenance = runner.provenance(&id).expect("a done job has provenance");
        assert_eq!(
            provenance,
            Provenance {
                model: "fake-fill".to_string(),
                device: "cpu".to_string(),
                provider: "local".to_string(),
                elapsed_ms: provenance.elapsed_ms,
                image_size: [2, 2]
            }
        );
        let png = runner.take_png(&id).expect("the result image is there");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(runner.take_png(&id).is_err(), "a result is only served once");
        runner.stop();
    }

    #[test]
    fn cancelling_a_queued_job_stops_it_before_it_starts() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(20) }, 1).expect("the worker starts");
        let first = runner.submit(frame(), mask()).expect("the first job is accepted");
        let second = runner.submit(frame(), mask()).expect_err("the queue holds one");
        assert_eq!(second.status.code(), 503);
        runner.cancel(&first).expect("cancelling works");
        assert_eq!(settle(&runner, &first), State::Cancelled);
        assert_eq!(runner.provenance(&first).expect_err("a cancelled job has no result").status.code(), 409);
        runner.stop();
    }

    #[test]
    fn the_queue_cap_answers_busy_and_clears_when_a_job_finishes() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(10) }, 2).expect("the worker starts");
        let first = runner.submit(frame(), mask()).expect("the first job is accepted");
        let second = runner.submit(frame(), mask()).expect("the second job is accepted");
        assert_eq!(runner.submit(frame(), mask()).expect_err("the queue is full").status.code(), 503);
        for id in [&first, &second] {
            settle(&runner, id);
        }
        let third = runner.submit(frame(), mask()).expect("a finished job frees a place");
        assert_eq!(third, "job-3");
        runner.stop();
    }

    #[test]
    fn an_unknown_id_is_not_found_everywhere() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("the worker starts");
        for error in [
            runner.state("job-9").expect_err("no such job"),
            runner.provenance("job-9").expect_err("no such job"),
            runner.take_png("job-9").expect_err("no such job"),
            runner.cancel("job-9").expect_err("no such job"),
        ] {
            assert_eq!(error.status.code(), 404, "{error}");
        }
        runner.stop();
    }

    #[test]
    fn the_states_use_the_protocol_names() {
        assert_eq!(State::Queued.wire(), "queued");
        assert_eq!(State::Running.wire(), "running");
        assert_eq!(State::Done.wire(), "done");
        assert_eq!(State::Failed.wire(), "failed");
        assert_eq!(State::Cancelled.wire(), "cancelled");
        assert!(State::Done.is_finished() && State::Failed.is_finished() && State::Cancelled.is_finished());
        assert!(!State::Queued.is_finished() && !State::Running.is_finished());
    }

    // MG-04/MG-05: the same lifecycle on the LaMa backend, with a stand-in for the session so it
    // runs on any machine (item 6). The fake-backend tests above stay as they are.

    /// A LaMa backend whose session is a stand-in, plus the counters that show how often it was
    /// loaded and run.
    fn lama_runner(step: Duration, loads: &Arc<AtomicUsize>, runs: &Arc<AtomicUsize>) -> Runner {
        let pipeline = lama::stand_in_pipeline(128.0, step, loads, runs);
        Runner::new(Backend::Lama(pipeline), MAX_QUEUE).expect("the worker starts")
    }

    #[test]
    fn a_lama_job_goes_through_its_stages_and_answers_with_the_model() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let runner = lama_runner(Duration::from_millis(4), &loads, &runs);
        let id = runner.submit(frame(), mask()).expect("the job is accepted");
        // Read the messages as they arrive, so the order the stages report in is the order a
        // client would see them in.
        let mut seen: Vec<String> = Vec::new();
        let mut progress: Vec<f64> = Vec::new();
        let mut last = String::new();
        let deadline = std::time::Instant::now() + PATIENCE;
        while std::time::Instant::now() < deadline {
            let (state, value, message) = runner.state(&id).expect("the job exists");
            if message != last {
                seen.push(message.clone());
                last = message;
            }
            progress.push(value);
            if state.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(finish(&runner, &id), State::Done);
        // The stages a client can see are the MG-05 names, in the order the pipeline reports them,
        // and never the fake backend's sentence. Which messages come first is decided by how fast
        // each stage runs, so the order is checked as a subsequence; the full sequence itself is
        // asserted in `lama`, where nothing is racing with a poll.
        let order = [lama::MSG_PREPARE, lama::MSG_LOAD, lama::MSG_RUN, lama::MSG_FINISH];
        let stages: Vec<&str> = seen.iter().map(String::as_str).filter(|message| order.contains(message)).collect();
        let mut expected = order.iter();
        for stage in &stages {
            let Some(found) = expected.clone().position(|name| *name == *stage) else {
                panic!("{stage} came out of order: {stages:?}");
            };
            for _ in 0..=found {
                expected.next();
            }
        }
        assert!(stages.contains(&lama::MSG_RUN), "the run stage is the long one, so it is always visible: {stages:?}");
        assert!(!seen.iter().any(|message| message == backend::FAKE_MESSAGE), "a LaMa job never reports the fake backend: {seen:?}");
        assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]), "progress only moves forward: {progress:?}");
        let provenance = runner.provenance(&id).expect("a done job has provenance");
        assert_eq!(provenance.model, "lama");
        assert_eq!(provenance.device, "cpu");
        assert_eq!(provenance.provider, "local");
        assert_eq!(provenance.image_size, [2, 2]);
        assert!(runner.take_png(&id).is_ok_and(|png| png.starts_with(b"\x89PNG\r\n\x1a\n")), "the result image is there");
        assert_eq!((loads.load(Ordering::SeqCst), runs.load(Ordering::SeqCst)), (1, 1), "loaded once, run once");
        runner.stop();
    }

    #[test]
    fn a_lama_job_cancelled_while_queued_never_loads_the_model() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let runner = lama_runner(Duration::from_millis(20), &loads, &runs);
        let first = runner.submit(frame(), mask()).expect("the first job is accepted");
        assert_eq!(settle(&runner, &first), State::Running, "the first job is holding the worker");
        let second = runner.submit(frame(), mask()).expect("the second job joins the queue");
        runner.cancel(&second).expect("cancelling works");
        assert_eq!(finish(&runner, &second), State::Cancelled, "the queued job is cancelled before it starts");
        assert_eq!(loads.load(Ordering::SeqCst), 0, "it never got as far as loading the model");
        assert_eq!(finish(&runner, &first), State::Done, "the job that was running is unaffected");
        runner.stop();
    }

    #[test]
    fn a_lama_job_cancelled_while_running_stops_the_run() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let runner = lama_runner(Duration::from_millis(20), &loads, &runs);
        let id = runner.submit(frame(), mask()).expect("the job is accepted");
        assert_eq!(settle(&runner, &id), State::Running);
        runner.cancel(&id).expect("a running job can be cancelled");
        assert_eq!(finish(&runner, &id), State::Cancelled, "the run notices and stops");
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the run never completed");
        assert_eq!(runner.provenance(&id).expect_err("a cancelled job has no result").status.code(), 409);
        runner.stop();
    }

    #[test]
    fn two_lama_jobs_finish_in_the_order_they_arrived() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let runner = lama_runner(Duration::from_millis(10), &loads, &runs);
        let first = runner.submit(frame(), mask()).expect("the first job is accepted");
        let second = runner.submit(frame(), mask()).expect("the second job is accepted");
        let mut order: Vec<&str> = Vec::new();
        let deadline = std::time::Instant::now() + PATIENCE;
        while std::time::Instant::now() < deadline {
            for id in [&first, &second] {
                if runner.state(id).expect("the job exists").0.is_finished() && !order.contains(&id.as_str()) {
                    order.push(id);
                }
            }
            if order.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(order, [first.as_str(), second.as_str()], "one at a time, in arrival order");
        assert_eq!((loads.load(Ordering::SeqCst), runs.load(Ordering::SeqCst)), (1, 2), "the session is loaded once and kept");
        runner.stop();
    }

    #[test]
    fn cancelling_a_queued_job_frees_its_input_and_starts_its_clock() {
        // C3: the worker skips a cancelled job, so cancel() itself has to let the sweep have it.
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(50) }, MAX_QUEUE).expect("the worker starts");
        let first = runner.submit(frame(), mask()).expect("the first job is accepted");
        let second = runner.submit(frame(), mask()).expect("the second job joins the queue");
        runner.cancel(&second).expect("cancelling works");
        {
            let shared = lock(&runner.shared);
            let entry = find(&shared.jobs, &second).expect("the cancelled job is still known");
            assert!(entry.input.is_none(), "the image and mask are dropped at once");
            assert!(entry.finished_at.is_some(), "and the ten-minute clock has started, so the sweep frees the record");
        }
        assert_eq!(finish(&runner, &first), State::Done, "the job ahead of it is unaffected");
        assert_eq!(runner.state(&second).expect("the record is still there").0, State::Cancelled);
        runner.stop();
    }

    #[test]
    fn a_job_cancelled_mid_stage_says_cancelled_and_stops_reporting() {
        // C4: the stage that was running when the DELETE arrived used to overwrite "Cancelled.".
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let runner = Runner::new(Backend::Lama(lama::stand_in_pipeline(128.0, Duration::from_millis(1), &loads, &runs)), MAX_QUEUE).expect("the worker starts");
        // Big enough that preparing the crop takes a while, so the cancel lands inside that stage.
        let (width, height) = (3000u32, 3000u32);
        let image = Frame { width, height, channels: 3, bit_depth: 8, samples: vec![9; (width * height * 3) as usize] };
        let mut big_mask = vec![0u8; (width * height) as usize];
        for row in 0..2000 {
            for column in 0..2000 {
                big_mask[row * 3000 + column] = 255;
            }
        }
        let id = runner.submit(image, big_mask).expect("the job is accepted");
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            let (state, _, message) = runner.state(&id).expect("the job exists");
            if state == State::Running && message == lama::MSG_PREPARE {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the job never reached the prepare stage");
            std::thread::sleep(Duration::from_millis(1));
        }
        runner.cancel(&id).expect("a running job can be cancelled");
        let (_, at_cancel, _) = runner.state(&id).expect("the job exists");
        let deadline = std::time::Instant::now() + PATIENCE;
        while lock(&runner.shared).jobs.iter().any(|entry| entry.id == id && entry.finished_at.is_none()) {
            assert!(std::time::Instant::now() < deadline, "the cancelled job never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (state, progress, message) = runner.state(&id).expect("the job exists");
        assert_eq!(state, State::Cancelled);
        assert_eq!(message, "Cancelled.", "the poll says what happened, not the stage it was in");
        assert_eq!(progress, at_cancel, "and no progress is reported after the cancel");
        runner.stop();
    }

    #[test]
    fn a_job_the_worker_cannot_take_is_forgotten_with_the_right_sentence() {
        // C8: after the worker stopped, the job stayed queued for ever and the 500 said "no job with that id".
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("the worker starts");
        runner.stop();
        let fail = runner.submit(frame(), mask()).expect_err("there is no worker to take the job");
        assert_eq!(fail.status.code(), 500, "{fail}");
        assert!(!fail.sentence.contains("no job with that id"), "the sentence says what went wrong: {fail}");
        assert!(lock(&runner.shared).jobs.is_empty(), "the job is not left behind in the queue");
        for _ in 0..MAX_QUEUE + 1 {
            assert_eq!(runner.submit(frame(), mask()).expect_err("still no worker").status.code(), 500, "never a 503 from leftovers");
        }
    }
}
