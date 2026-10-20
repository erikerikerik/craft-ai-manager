//! The job side of the protocol: a FIFO queue, one worker thread, and the states a job moves
//! through while it runs. Amendment 11 fixes the states (`queued`, `running`, `done`, `failed`,
//! `cancelled`), the queue cap of eight, and the ten minutes a finished result is kept for.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::activity;
use crate::backend::{self, Filled};
use crate::device;
use crate::error::{Fail, Result};
use crate::image::{self, Frame};
use crate::lama;
use crate::memory::{self, Budget, Reservation};
use crate::routing::{self, TASK_MASK};

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

/// What the worker runs a job on (MG-04 item 2). `caim serve` chooses one at start, through
/// [routing](crate::routing), which is the only place a task is matched to a model:
///
/// * `Real` when MG-02 says at least one of the two models is installed and acknowledged;
/// * `Fake` under `--fake-backend`, exactly as MG-01 had it;
/// * `NoModel` otherwise, which keeps MG-01's answer and sentence.
#[derive(Clone, Debug, Default)]
pub enum Backend {
    /// Nothing is installed, so a job is refused with amendment 11's sentence.
    #[default]
    NoModel,
    /// The built-in fake backend, its ten steps spaced by `step_delay`.
    Fake { step_delay: Duration },
    /// The real ONNX runners, one slot per task, each with its session loaded on the first job and
    /// kept after that. MG-10 added the second slot, so a store that holds only one of the two
    /// models still serves the task it holds. MG-11 adds `device`: the setting every job on this
    /// start resolves against. It is resolved again per job, not once per start, because the memory
    /// free on the card changes between jobs.
    Real { lama: Option<Arc<lama::Pipeline>>, birefnet: Option<Arc<crate::birefnet::Pipeline>>, device: device::Setting },
}

impl Backend {
    /// One line for the terminal: which backend this start is running and which device it was
    /// asked for. MG-11 drops the promise that the run is on the CPU, because it may not be.
    #[must_use]
    pub fn note(&self) -> String {
        match self {
            Backend::NoModel => "none: no model is installed yet".to_string(),
            Backend::Fake { .. } => "fake-fill and fake-mask (the built-in fill and ellipse)".to_string(),
            Backend::Real { lama: Some(_), birefnet: None, device } => format!("lama (device {}, loaded on the first job)", device.token()),
            Backend::Real { lama: None, birefnet: Some(_), device } => format!("birefnet-lite (device {}, loaded on the first job)", device.token()),
            Backend::Real { lama: Some(_), birefnet: Some(_), device } => {
                format!("lama and birefnet-lite (device {}, loaded on the first job)", device.token())
            }
            // The start only makes `Real` when it found a model, so this is defensive rather than
            // reachable; it keeps the line honest if that ever changes.
            Backend::Real { .. } => "none: no model is installed yet".to_string(),
        }
    }
}

/// The device chain for one job (MG-11), decided from the model that is about to run.
///
/// The manifest comes from the pipeline itself, so the resolver measures the card against the same
/// `memory_mib` and the same `backends` table the catalog gave that model. A pipeline built straight
/// from a path claims no backend at all, which the resolver reads as CPU only - what such a pipeline
/// has always done. `--device cpu` and a model with no tested CUDA build never probe the machine,
/// so a serve on a machine with no GPU never runs the vendor's tool. `session_card` comes from the
/// pipeline that is about to run: when it already keeps this model on the card the probe has just
/// named, the resolver must not measure that memory as something still to be found. It names a card,
/// not merely "a CUDA session", because on two cards a session on one says nothing about the other.
#[must_use]
fn decision_for(setting: device::Setting, manifest: Option<device::Manifest>, task: &str, model: &str, session_card: Option<i32>) -> device::Decision {
    let manifest = manifest.unwrap_or_else(|| device::Manifest::cpu_only(model, 0));
    device::Resolver::new(setting).resolve_lazy(task, &manifest, &move || {
        let mut machine = device::Machine::probe();
        machine.mark_resident(device::Device::Cuda, session_card);
        machine
    })
}

/// What `GET /v1/jobs/{id}/result` reports: amendment 14 wants this pinned to the layer later.
///
/// Amendment 18 adds the rest of the account: what kind of result it is, which model version and
/// which bytes ran, what device was asked for, which one ran, and why it fell back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub model: String,
    pub device: String,
    pub provider: String,
    pub elapsed_ms: u64,
    pub image_size: [u32; 2],
    /// `image` or `mask`: the shape of `result.png`.
    pub kind: &'static str,
    /// The catalog version of the model that ran. `None` for the fake backend, which has no model.
    pub model_version: Option<String>,
    /// The SHA-256 of the model file that ran.
    pub model_sha256: Option<String>,
    /// The device the caller asked for: `auto` until CAIM-04 adds a device setting.
    pub device_requested: String,
    /// The hardware that ran, in plain words.
    pub device_name: Option<String>,
    /// Why a backend other than the requested one ran. `None` when the requested one did.
    pub fallback_reason: Option<String>,
    /// How long the session took to load: 0 when a later job reused it.
    pub load_ms: u64,
    /// How long the model itself took, without the preparation or the encoding.
    pub run_ms: u64,
    /// The runtime's own error text, for the log only.
    pub diagnostics: Option<String>,
}

/// One job, from the moment it is accepted until its result has been read or has expired.
struct Entry {
    id: String,
    /// Which of the catalog tasks this job is, so the worker can pick the runner and the activity
    /// log can name it. Amendment 18: the task is checked before the job is queued, so this is
    /// always a task that was ready.
    task: String,
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
    /// When the worker took the job, which is what lets the window say "Started 2 s ago".
    started_at: Option<Instant>,
}

/// The jobs this manager knows about, in the order they arrived.
struct Shared {
    jobs: Vec<Entry>,
    next_id: u64,
    pending: usize,
    /// Where finished jobs are logged, or `None` when this run has nowhere to log.
    activity: Option<PathBuf>,
    /// Tasks whose model this run has removed. The window is the process that removes a model it is
    /// serving (brief §5.2), so the queue has to stop routing to it rather than trust the store.
    retired: Vec<String>,
}

/// The job the worker is inside right now, in the shape the window draws it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Active {
    /// The job id, so the window can name the job it is asking about.
    pub id: String,
    /// The task the job asked for, which picks the window's sentence.
    pub task: String,
    /// How long the worker has had the job.
    pub started_secs: u64,
    /// The backend's progress, as a whole percentage.
    pub progress_percent: u64,
}

/// A queue slot is reserved before HTTP buffers a body; all failure paths release it.
pub(crate) struct Admission {
    shared: Arc<Mutex<Shared>>,
    body: Option<Reservation>,
    work: Option<Reservation>,
    pending: bool,
}

impl Admission {
    pub(crate) fn reserve_work(&mut self, width: u32, height: u32) -> Result<()> {
        let body = self.body.as_mut().ok_or_else(|| Fail::internal("This upload has already reserved its working space."))?;
        let bytes = memory::upload_work_bytes(width, height, body.bytes())?;
        // Atomically replace this request's body charge with its peak charge. Adding a second
        // reservation double-counted the PNG bytes and rejected otherwise legal 64 MP jobs.
        body.resize(bytes)?;
        self.work = self.body.take();
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

/// What this manager can run right now: the tasks it advertises and the backend that runs them.
///
/// MG-13 review round 3: these two are one value because a job is accepted by the first and run by
/// the second. Kept apart, a refresh could advertise a model the queue was not yet pointed at, and a
/// job accepted in that moment ran on the old backend and failed as not ready.
#[derive(Clone, Debug)]
pub struct Snapshot {
    offered: routing::Offered,
    backend: Arc<Backend>,
}

impl Snapshot {
    /// The pair as one start, or one refresh, worked it out.
    #[must_use]
    pub fn new(offered: routing::Offered, backend: Backend) -> Self {
        Self { offered, backend: Arc::new(backend) }
    }
}

/// The routing the worker runs the next job on. The cell is only ever held for the clone, never
/// across a job, so a refresh while a job is running cannot block the worker or interrupt it.
fn snapshot_of(cell: &Mutex<Snapshot>) -> Snapshot {
    cell.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The job with this id, if this manager still knows about it.
fn find_mut<'a>(jobs: &'a mut [Entry], id: &str) -> Option<&'a mut Entry> {
    jobs.iter_mut().find(|entry| entry.id == id)
}

fn find<'a>(jobs: &'a [Entry], id: &str) -> Option<&'a Entry> {
    jobs.iter().find(|entry| entry.id == id)
}

/// Whether this run has stopped routing jobs to this task's model, because the window removed it.
fn is_retired(shared: &Arc<Mutex<Shared>>, task: &str) -> bool {
    lock(shared).retired.iter().any(|retired| retired == task)
}

/// A job's progress, as the whole percentage the window shows at the end of a feature row.
#[must_use]
fn percent(progress: f64) -> u64 {
    (progress.clamp(0.0, 1.0) * 100.0).round_ties_even() as u64
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
    /// What the worker runs jobs on, and what this manager advertises, in one cell the window can
    /// change while the manager is serving (MG-13 review round 2, round 3). The worker takes its own
    /// copy per job, so a job already running keeps the backend it started with and a model installed
    /// afterwards is used by the next job.
    routing: Arc<Mutex<Snapshot>>,
}

impl Runner {
    /// Start the worker. The fake backend's `step_delay` spaces its ten progress steps, which is
    /// what lets the tests have eight jobs in flight at once.
    pub fn new(backend: Backend, max_queue: usize) -> Result<Runner> {
        Self::with_routing(Snapshot::new(routing::Offered::default(), backend), max_queue, DEFAULT_MEMORY_BYTES)
    }

    #[cfg(test)]
    pub(crate) fn with_memory_limit(backend: Backend, max_queue: usize, memory_limit: usize) -> Result<Runner> {
        Self::with_routing(Snapshot::new(routing::Offered::default(), backend), max_queue, memory_limit)
    }

    /// Start the worker with the routing this start worked out, which is how a server's first answer
    /// and its first job come from the same pair (MG-13 review round 3).
    pub(crate) fn with_routing(routing: Snapshot, max_queue: usize, memory_limit: usize) -> Result<Runner> {
        let (queue_tx, queue_rx) = std::sync::mpsc::sync_channel::<String>(MAX_RECORDS);
        let shared = Arc::new(Mutex::new(Shared { jobs: Vec::new(), next_id: 0, pending: 0, activity: None, retired: Vec::new() }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_stop = Arc::clone(&stop);
        let routing_cell = Arc::new(Mutex::new(routing));
        let worker_routing = Arc::clone(&routing_cell);
        let handle = std::thread::Builder::new()
            .name("caim-jobs".to_string())
            .spawn(move || worker(worker_shared, queue_rx, worker_stop, worker_routing))
            .map_err(|error| Fail::internal(format!("Craft AI Manager could not start its job worker: {error}")))?;
        Ok(Runner {
            shared,
            queue_tx,
            worker: Mutex::new(Some(handle)),
            stop,
            max_queue,
            budget: Budget::new(memory_limit),
            routing: Arc::clone(&routing_cell),
        })
    }

    /// Point the queue at an activity log. Amendment 18 puts it next to `endpoint.json`, and a
    /// run with no endpoint file has no log: nothing about a job changes either way.
    pub(crate) fn set_activity_log(&self, path: Option<PathBuf>) {
        lock(&self.shared).activity = path;
    }

    /// Point the queue and the task list at a new routing (MG-13 review round 2, round 3).
    ///
    /// The window installs, acknowledges and removes models while this manager is already serving, so
    /// the models on disk are not the models this start found. A job already inside a backend keeps
    /// the one it took - a session is never pulled out from under a running job - and the next job
    /// gets the new one. The task list and the backend change inside one lock, so a request is never
    /// accepted against a task list the queue has not caught up with.
    pub fn set_routing(&self, offered: routing::Offered, backend: Backend) {
        let mut cell = self.routing.lock().unwrap_or_else(PoisonError::into_inner);
        cell.offered = offered;
        cell.backend = Arc::new(backend);
    }

    /// The tasks this manager advertises, read from the same cell the worker takes its backend from.
    #[must_use]
    pub fn offered(&self) -> routing::Offered {
        self.routing.lock().unwrap_or_else(PoisonError::into_inner).offered.clone()
    }

    /// The job the worker is inside right now, if any. The window asks once per repaint, which is
    /// how it can say "Working." and "Started 2 s ago" without keeping a copy of its own.
    #[must_use]
    pub fn active(&self) -> Option<Active> {
        let shared = lock(&self.shared);
        let entry = shared.jobs.iter().find(|entry| entry.state == State::Running)?;
        let started = entry.started_at.unwrap_or_else(Instant::now);
        Some(Active { id: entry.id.clone(), task: entry.task.clone(), started_secs: started.elapsed().as_secs(), progress_percent: percent(entry.progress) })
    }

    /// Stop routing jobs to a task whose model this run has just deleted (brief §5.2).
    ///
    /// The window is the process that is serving, so it cannot use the "a manager is listening, so
    /// nothing was deleted" guard the CLI relies on. It retires the task first, waits out or cancels
    /// the job that was inside it, and only then deletes the file. A job that arrives afterwards is
    /// refused with the same sentence an uninstalled model gets; a job already queued for it fails
    /// the same way rather than running from a session still held in memory.
    pub fn retire_task(&self, task: &str) {
        let mut shared = lock(&self.shared);
        if !shared.retired.iter().any(|retired| retired == task) {
            shared.retired.push(task.to_string());
        }
    }

    /// Route to a task again after a removal that did not happen (MG-13 review round 2).
    ///
    /// Retirement means "this run deleted the file", so it may only stand while the file is gone. If
    /// the job inside the model would not stop, or the delete failed, the model is still installed and
    /// every job for its task has to be accepted again rather than refused until the next restart.
    pub fn restore_task(&self, task: &str) {
        lock(&self.shared).retired.retain(|retired| retired != task);
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
        Ok(Admission { shared: Arc::clone(&self.shared), body: Some(body), work: None, pending: true })
    }

    /// Accept a job and put it at the back of the queue. The id is opaque to the client, and safe
    /// to put in a URL path.
    pub fn submit(&self, task: &str, frame: Frame, mask: Vec<u8>) -> Result<String> {
        let mut admission = self.admit(0)?;
        admission.reserve_work(frame.width, frame.height)?;
        self.submit_admitted(task, frame, mask, admission)
    }

    pub(crate) fn submit_admitted(&self, task: &str, frame: Frame, mask: Vec<u8>, mut admission: Admission) -> Result<String> {
        admission.body = None;
        let id = {
            let mut shared = lock(&self.shared);
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                drop(shared);
                return Err(Fail::internal(NO_WORKER));
            }
            if shared.retired.iter().any(|retired| retired == task) {
                // Brief §5.2: the window removed the model this run is serving, so a job arriving
                // now gets the same 400 sentence an uninstalled model gets.
                drop(shared);
                return Err(Fail::bad_request(routing::not_ready_sentence(task)));
            }
            shared.pending = shared.pending.saturating_sub(1);
            admission.pending = false;
            shared.next_id = shared.next_id.wrapping_add(1);
            let id = format!("job-{}", shared.next_id);
            shared.jobs.push(Entry {
                id: id.clone(),
                task: task.to_string(),
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
                started_at: None,
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
            let record = activity_record(entry);
            let path = shared.activity.clone();
            drop(shared);
            log_activity(path.as_deref(), &record);
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
    #[cfg(test)]
    fn take_png(&self, id: &str) -> Result<Vec<u8>> {
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
        let path = shared.activity.clone();
        let mut logged = Vec::new();
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
                logged.push(activity_record(entry));
            }
        }
        drop(shared);
        for record in &logged {
            log_activity(path.as_deref(), record);
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
fn worker(shared: Arc<Mutex<Shared>>, queue_rx: Receiver<String>, stop: Arc<std::sync::atomic::AtomicBool>, routing: Arc<Mutex<Snapshot>>) {
    use std::sync::atomic::Ordering;
    while !stop.load(Ordering::Relaxed) {
        match queue_rx.recv_timeout(WORKER_POLL) {
            // The routing is taken per job, so the window can change what runs next without waiting
            // for, or interrupting, the job this worker is inside.
            Ok(id) => {
                let running = snapshot_of(&routing);
                run_one(&shared, &id, &running.backend);
            }
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
                entry.message = routing::running_message(&entry.task).to_string();
                entry.started_at = Some(Instant::now());
                Some((entry.task.clone(), entry.input.take()))
            }
            _ => None,
        }
    };
    let Some((task, Some((image, mask)))) = taken else { return };
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
    // The task picks the runner, and nothing else. A runner never decides which model to use, and
    // a task with no runner in this backend gets the same sentence the queue would have given it
    // before the job arrived.
    let outcome = if is_retired(shared, &task) {
        // Accepted before the window removed the model, so it never reaches a backend.
        Err(Fail::bad_request(routing::not_ready_sentence(&task)))
    } else {
        match (backend, task.as_str()) {
            (Backend::NoModel, _) => Err(Fail::bad_request(routing::not_ready_sentence(&task))),
            (Backend::Fake { step_delay }, TASK_MASK) => backend::run_mask(&image, &mask, &cancel, *step_delay, &mut progress),
            (Backend::Fake { step_delay }, _) => backend::run_fill(&image, &mask, &cancel, *step_delay, &mut progress),
            (Backend::Real { birefnet: Some(pipeline), device: setting, .. }, TASK_MASK) => {
                let decision = decision_for(*setting, pipeline.manifest(), TASK_MASK, crate::birefnet::BIREFNET_ID, pipeline.session_card());
                pipeline.run_mask_on(&image, &mask, &decision, &cancel, &mut progress)
            }
            (Backend::Real { birefnet: None, .. }, TASK_MASK) => Err(Fail::bad_request(routing::not_ready_sentence(TASK_MASK))),
            (Backend::Real { lama: Some(pipeline), device: setting, .. }, _) => {
                let decision = decision_for(*setting, pipeline.manifest(), &task, lama::LAMA_ID, pipeline.session_card());
                pipeline.run_fill_on(&image, &mask, &decision, &cancel, &mut progress)
            }
            (Backend::Real { lama: None, .. }, _) => Err(Fail::bad_request(routing::not_ready_sentence(&task))),
        }
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
                    image_size: [filled.frame.width, filled.frame.height],
                    kind: filled.kind,
                    model: filled.model,
                    device: filled.device,
                    provider: filled.provider,
                    elapsed_ms: filled.elapsed_ms,
                    model_version: filled.model_version,
                    model_sha256: filled.model_sha256,
                    device_requested: filled.device_requested,
                    device_name: filled.device_name,
                    fallback_reason: filled.fallback_reason,
                    load_ms: filled.load_ms,
                    run_ms: filled.run_ms,
                    diagnostics: filled.diagnostics,
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
        // A cancelled job already has its line in the activity log, written where it was cancelled.
        // Writing it again would count the same job twice.
        entry.memory = None;
        entry.message = CANCELLED_MESSAGE.to_string();
        return;
    }
    match ready {
        Ok((png, provenance)) => {
            // This only shrinks the conservative working-space reservation.
            let resized = match entry.memory.as_mut() {
                Some(memory) => memory.resize(png.capacity()),
                None => Ok(()),
            };
            match resized {
                Err(fail) => {
                    entry.memory = None;
                    entry.state = State::Failed;
                    entry.message = fail.sentence;
                }
                Ok(()) => {
                    entry.state = State::Done;
                    entry.progress = 1.0;
                    entry.message = "Done.".to_string();
                    entry.png = Some(png);
                    entry.provenance = Some(provenance);
                }
            }
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
    let record = activity_record(entry);
    let path = shared.activity.clone();
    drop(shared);
    log_activity(path.as_deref(), &record);
}

/// The line the activity log gets for a finished job: amendment 18's "task, model, version,
/// device, requested, fallback, timings, state". Every field comes from the same provenance the
/// result reports, so the log and the answer cannot disagree about which device ran or why. A job
/// that never reached a backend has no provenance, so its line says so with nulls rather than
/// inventing a device.
fn activity_record(entry: &Entry) -> activity::Record {
    let provenance = entry.provenance.as_ref();
    activity::Record {
        time: activity::now_stamp(),
        task: Some(entry.task.clone()),
        model: provenance.map(|source| source.model.clone()),
        version: provenance.and_then(|source| source.model_version.clone()),
        device: provenance.map(|source| source.device.clone()),
        device_requested: provenance.map(|source| source.device_requested.clone()),
        fallback_reason: provenance.and_then(|source| source.fallback_reason.clone()),
        elapsed_ms: provenance.map(|source| source.elapsed_ms),
        state: entry.state.wire().to_string(),
    }
}

/// Write one finished job, and never let the log change what a job reported: amendment 18 says a
/// failed write is a sentence on stderr and nothing else.
fn log_activity(path: Option<&Path>, record: &activity::Record) {
    let Some(path) = path else { return };
    if let Err(fail) = activity::append(path, record) {
        eprintln!("{}", fail.sentence);
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
    use crate::protocol::TASK_FILL;
    use crate::routing::{OUTPUT_IMAGE, OUTPUT_MASK, Offered, TASK_MASK};

    #[test]
    fn a_64mp_rgba16_upload_fits_idle_admission_and_is_busy_only_during_contention() {
        let runner = super::Runner::new(super::Backend::Fake { step_delay: std::time::Duration::ZERO }, 8).expect("worker");
        let request =
            crate::protocol::JobRequest::parse(br#"{"task":"fill_masked_area","image_size":[8000,8000],"region":[0,0,8000,8000],"channels":4,"bit_depth":16}"#)
                .expect("RGBA16 request");
        request.validate(&Offered::fake()).expect("64 MP ceiling unchanged");
        // Reserve the admission path used by the server without allocating a 512 MB test PNG.
        // Includes worst-case raw RGBA16 + grayscale mask and multipart/JSON overhead.
        let body_bytes = 8000 * 8000 * 9 + 65536;
        let mut held = runner.admit(body_bytes).expect("idle body reservation");
        held.reserve_work(8000, 8000).expect("idle 64 MP RGBA16 work accepted");
        assert_eq!(runner.admit(body_bytes).err().expect("contention").status.code(), 503);
        drop(held);
        let mut retry = runner.admit(body_bytes).expect("no permanent busy response");
        retry.reserve_work(8000, 8000).expect("the same 64 MP request fits again");
    }

    #[test]
    fn the_1024_record_cap_refuses_new_jobs_until_records_expire() {
        let runner = super::Runner::new(super::Backend::Fake { step_delay: std::time::Duration::ZERO }, 8).expect("worker");
        for n in 0..super::MAX_RECORDS {
            let mut shared = super::lock(&runner.shared);
            shared.jobs.push(super::Entry {
                id: format!("finished-{n}"),
                task: TASK_FILL.to_string(),
                state: super::State::Cancelled,
                progress: 0.0,
                message: "Cancelled.".into(),
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                input: None,
                png: None,
                delivering: false,
                memory: None,
                provenance: None,
                finished_at: Some(std::time::Instant::now()),
                started_at: None,
            });
        }
        assert_eq!(runner.admit(0).err().expect("record cap").status.code(), 503);
        {
            let mut shared = super::lock(&runner.shared);
            for entry in &mut shared.jobs {
                entry.finished_at = Some(std::time::Instant::now() - super::RESULT_TTL);
            }
        }
        runner.admit(0).expect("expired records free admission");
    }
    use super::{Backend, MAX_QUEUE, Provenance, Runner, Snapshot, State, decision_for, find, lock};
    use crate::backend;
    use crate::device;
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
        let active = runner.submit(TASK_FILL, frame(), mask()).expect("active");
        assert_eq!(settle(&runner, &active), State::Running);
        let queued = runner.submit(TASK_FILL, frame(), mask()).expect("queued");
        runner.stop();
        for id in [&active, &queued] {
            assert_eq!(runner.state(id).expect("record").0, State::Cancelled);
            let shared = lock(&runner.shared);
            let entry = find(&shared.jobs, id).expect("entry");
            assert!(entry.input.is_none() && entry.memory.is_none() && entry.finished_at.is_some());
        }
        assert!(runner.submit(TASK_FILL, frame(), mask()).is_err());
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
        assert!(runner.submit_admitted(TASK_FILL, frame(), mask(), admission).is_err());
        let shared = lock(&runner.shared);
        assert_eq!(shared.pending, 0);
        assert!(shared.jobs.is_empty());
        runner.budget.reserve(super::DEFAULT_MEMORY_BYTES).expect("upload memory returned");
    }

    #[test]
    fn expired_results_release_memory() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("worker");
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("job");
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
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("job");
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
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("job");
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
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("job");
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
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("the job is accepted");
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
                image_size: [2, 2],
                kind: OUTPUT_IMAGE,
                model_version: None,
                model_sha256: None,
                device_requested: "auto".to_string(),
                device_name: Some("CPU".to_string()),
                fallback_reason: None,
                load_ms: 0,
                run_ms: provenance.elapsed_ms,
                diagnostics: None,
            }
        );
        let png = runner.take_png(&id).expect("the result image is there");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(runner.take_png(&id).is_err(), "a result is only served once");
        runner.stop();
    }

    #[test]
    fn a_mask_job_runs_on_the_mask_half_of_the_fake_backend() {
        // Amendment 18: the task picks the runner, and the result says it is a mask.
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("the worker starts");
        let id = runner.submit(TASK_MASK, frame(), mask()).expect("the job is accepted");
        assert_eq!(finish(&runner, &id), State::Done);
        let provenance = runner.provenance(&id).expect("a done job has provenance");
        assert_eq!(provenance.model, "fake-mask");
        assert_eq!(provenance.kind, OUTPUT_MASK);
        let png = runner.take_png(&id).expect("the result image is there");
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let reader = decoder.read_info().expect("the mask result is a PNG");
        let info = reader.info().clone();
        assert_eq!([info.width, info.height], [2, 2], "the mask comes back at the size that was sent");
        assert_eq!(info.color_type, png::ColorType::Grayscale, "a mask result is one channel");
        assert_eq!(info.bit_depth, png::BitDepth::Eight, "and always 8-bit");
        runner.stop();
    }

    #[test]
    fn a_real_backend_with_only_one_model_still_refuses_the_other_task() {
        // MG-10: the two runners are separate slots, so a store holding only LaMa answers the mask
        // task with the mask sentence rather than running the wrong model.
        let loads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pipeline = lama::stand_in_pipeline(128.0, Duration::ZERO, &loads, &runs);
        let runner = Runner::new(Backend::Real { lama: Some(pipeline), birefnet: None, device: device::Setting::Auto }, MAX_QUEUE).expect("the worker starts");
        let id = runner.submit(TASK_MASK, frame(), mask()).expect("the job is queued");
        assert_eq!(finish(&runner, &id), State::Failed, "the queue accepted it, so the worker says why it cannot run");
        let (_, _, message) = runner.state(&id).expect("the job exists");
        assert_eq!(message, "The Select Subject model isn't installed.", "{message}");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0, "LaMa never ran");
        runner.stop();
    }

    #[test]
    fn the_start_line_names_the_device_that_was_asked_for() {
        // MG-11: the line no longer promises the CPU, because the run may land on the card.
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = lama::stand_in_pipeline(128.0, Duration::ZERO, &loads, &runs);
        let backend = Backend::Real { lama: Some(pipeline), birefnet: None, device: device::Setting::Cpu };
        assert_eq!(backend.note(), "lama (device cpu, loaded on the first job)");
        assert_eq!(Backend::Fake { step_delay: Duration::ZERO }.note(), "fake-fill and fake-mask (the built-in fill and ellipse)");
    }

    #[test]
    fn a_card_the_manifest_has_never_tested_is_a_cpu_job_with_amendment_18s_word() {
        // MG-11: the queue decides per job from the model's own manifest. A model the catalog has
        // never run on the card never reaches the card, and the result says so in one of the five
        // allowed words. The machine is not even probed: the manifest already said no.
        let mut backends = std::collections::BTreeMap::new();
        backends.insert("cpu".to_string(), "supported".to_string());
        backends.insert("cuda".to_string(), "untested".to_string());
        let identity = lama::Identity { version: "1.0".to_string(), sha256: "ab".repeat(32), memory_mib: 1_500, backends };
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (loads_in, runs_in) = (Arc::clone(&loads), Arc::clone(&runs));
        let load: lama::Loader = Box::new(move |_path, _device| {
            loads_in.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(lama::StandIn { fill: 0.5, step: Duration::ZERO, runs: Arc::clone(&runs_in) }))
        });
        let pipeline = lama::Pipeline::with_identity_and_loader("lama_fp32.onnx", Some(identity), load);
        let runner = Runner::new(Backend::Real { lama: Some(pipeline), birefnet: None, device: device::Setting::Cuda }, MAX_QUEUE).expect("the worker starts");
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("the job is accepted");
        assert_eq!(finish(&runner, &id), State::Done);
        let provenance = runner.provenance(&id).expect("a done job has provenance");
        assert_eq!(provenance.device_requested, "cuda", "the start asked for the card");
        assert_eq!(provenance.device, "cpu", "and the CPU ran it");
        assert_eq!(provenance.device_name.as_deref(), Some("CPU"));
        assert_eq!(provenance.fallback_reason.as_deref(), Some("unsupported_model"));
        assert!(provenance.diagnostics.as_deref().unwrap_or_default().contains("no tested CUDA build"), "{:?}", provenance.diagnostics);
        assert_eq!((loads.load(Ordering::SeqCst), runs.load(Ordering::SeqCst)), (1, 1), "one session, loaded on the CPU, and it ran");
        runner.stop();
    }

    #[test]
    fn the_activity_line_carries_the_same_device_account_as_the_result() {
        // MG-11: amendment 18 wants the log to carry the requested device and the fallback reason,
        // not just the one that ran. A fallback is the case that matters: a log that says only
        // "cpu" cannot tell you the card was asked for and refused the job.
        let entry = super::Entry {
            id: "job-1".to_string(),
            task: TASK_FILL.to_string(),
            state: State::Done,
            progress: 1.0,
            message: "Done.".to_string(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            input: None,
            png: None,
            delivering: false,
            memory: None,
            provenance: Some(Provenance {
                model: "lama".to_string(),
                device: "cpu".to_string(),
                provider: "local".to_string(),
                elapsed_ms: 1_234,
                image_size: [2, 2],
                kind: OUTPUT_IMAGE,
                model_version: Some("1.0".to_string()),
                model_sha256: Some("ab".repeat(32)),
                device_requested: "cuda".to_string(),
                device_name: Some("CPU".to_string()),
                fallback_reason: Some("unsupported_model".to_string()),
                load_ms: 100,
                run_ms: 1_100,
                diagnostics: Some("the card would not start".to_string()),
            }),
            finished_at: Some(std::time::Instant::now()),
            started_at: None,
        };
        let record = super::activity_record(&entry);
        assert_eq!(record.task.as_deref(), Some(TASK_FILL));
        assert_eq!(record.model.as_deref(), Some("lama"));
        assert_eq!(record.version.as_deref(), Some("1.0"), "the model version MG-10 put on the result reaches the log");
        assert_eq!(record.device.as_deref(), Some("cpu"), "the backend that ran");
        assert_eq!(record.device_requested.as_deref(), Some("cuda"), "the backend that was asked for");
        assert_eq!(record.fallback_reason.as_deref(), Some("unsupported_model"), "and why they differ");
        assert_eq!(record.elapsed_ms, Some(1_234), "the timings are still there");
        assert_eq!(record.state, "done");
    }

    #[test]
    fn a_pipeline_with_no_catalog_entry_is_a_cpu_job_too() {
        // A pipeline built straight from a path - what the tests and any caller without a catalog
        // entry makes - claims no backend, so amendment 18 still gets a word rather than silence.
        let decision = decision_for(device::Setting::Cuda, None, "fill_masked_area", "lama", None);
        assert_eq!(decision.chain, vec![device::Device::Cpu]);
        assert_eq!(decision.head(), device::Device::Cpu);
        assert_eq!(decision.fallback_reason(device::Device::Cpu, None), Some(device::Reason::UnsupportedModel));
        let auto = decision_for(device::Setting::Auto, None, "fill_masked_area", "lama", None);
        assert!(auto.reason.is_none() && auto.note.is_none(), "`auto` landing on the CPU asked for the CPU");
    }

    #[test]
    fn cancelling_a_queued_job_stops_it_before_it_starts() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(20) }, 1).expect("the worker starts");
        let first = runner.submit(TASK_FILL, frame(), mask()).expect("the first job is accepted");
        let second = runner.submit(TASK_FILL, frame(), mask()).expect_err("the queue holds one");
        assert_eq!(second.status.code(), 503);
        runner.cancel(&first).expect("cancelling works");
        assert_eq!(settle(&runner, &first), State::Cancelled);
        assert_eq!(runner.provenance(&first).expect_err("a cancelled job has no result").status.code(), 409);
        runner.stop();
    }

    #[test]
    fn the_queue_cap_answers_busy_and_clears_when_a_job_finishes() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(10) }, 2).expect("the worker starts");
        let first = runner.submit(TASK_FILL, frame(), mask()).expect("the first job is accepted");
        let second = runner.submit(TASK_FILL, frame(), mask()).expect("the second job is accepted");
        assert_eq!(runner.submit(TASK_FILL, frame(), mask()).expect_err("the queue is full").status.code(), 503);
        for id in [&first, &second] {
            settle(&runner, id);
        }
        let third = runner.submit(TASK_FILL, frame(), mask()).expect("a finished job frees a place");
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
        Runner::new(Backend::Real { lama: Some(pipeline), birefnet: None, device: device::Setting::Auto }, MAX_QUEUE).expect("the worker starts")
    }

    #[test]
    fn a_lama_job_goes_through_its_stages_and_answers_with_the_model() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let runner = lama_runner(Duration::from_millis(4), &loads, &runs);
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("the job is accepted");
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
        let first = runner.submit(TASK_FILL, frame(), mask()).expect("the first job is accepted");
        assert_eq!(settle(&runner, &first), State::Running, "the first job is holding the worker");
        let second = runner.submit(TASK_FILL, frame(), mask()).expect("the second job joins the queue");
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
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("the job is accepted");
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
        let first = runner.submit(TASK_FILL, frame(), mask()).expect("the first job is accepted");
        let second = runner.submit(TASK_FILL, frame(), mask()).expect("the second job is accepted");
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
        let first = runner.submit(TASK_FILL, frame(), mask()).expect("the first job is accepted");
        let second = runner.submit(TASK_FILL, frame(), mask()).expect("the second job joins the queue");
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
        let runner = Runner::new(
            Backend::Real {
                lama: Some(lama::stand_in_pipeline(128.0, Duration::from_millis(1), &loads, &runs)),
                birefnet: None,
                device: device::Setting::Auto,
            },
            MAX_QUEUE,
        )
        .expect("the worker starts");
        // Big enough that preparing the crop takes a while, so the cancel lands inside that stage.
        let (width, height) = (3000u32, 3000u32);
        let image = Frame { width, height, channels: 3, bit_depth: 8, samples: vec![9; (width * height * 3) as usize] };
        let mut big_mask = vec![0u8; (width * height) as usize];
        for row in 0..2000 {
            for column in 0..2000 {
                big_mask[row * 3000 + column] = 255;
            }
        }
        let id = runner.submit(TASK_FILL, image, big_mask).expect("the job is accepted");
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
        let fail = runner.submit(TASK_FILL, frame(), mask()).expect_err("there is no worker to take the job");
        assert_eq!(fail.status.code(), 500, "{fail}");
        assert!(!fail.sentence.contains("no job with that id"), "the sentence says what went wrong: {fail}");
        assert!(lock(&runner.shared).jobs.is_empty(), "the job is not left behind in the queue");
        for _ in 0..MAX_QUEUE + 1 {
            assert_eq!(runner.submit(TASK_FILL, frame(), mask()).expect_err("still no worker").status.code(), 500, "never a 503 from leftovers");
        }
    }

    /// The window asks what the worker is inside, so it never keeps a copy of a job of its own.
    #[test]
    fn the_running_job_is_visible_with_its_task_and_how_long_it_has_been_running() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(40) }, MAX_QUEUE).expect("the worker starts");
        assert!(runner.active().is_none(), "an idle manager has no running job");
        let id = runner.submit(TASK_MASK, frame(), mask()).expect("the job is accepted");
        assert_eq!(settle(&runner, &id), State::Running, "the job is inside the backend");
        let active = runner.active().expect("the worker is inside a job");
        assert_eq!(active.id, id);
        assert_eq!(active.task, TASK_MASK, "the window picks its sentence from the task");
        assert!(active.started_secs < 60, "the clock starts when the worker takes the job");
        assert!(active.progress_percent <= 100, "a percentage is a percentage");
        let deadline = std::time::Instant::now() + PATIENCE;
        while runner.active().is_some() {
            assert!(std::time::Instant::now() < deadline, "the job never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(runner.active().is_none(), "a finished job is not a running job");
        runner.stop();
    }

    /// Brief §5.2: the window removes a model this very process is serving.
    #[test]
    fn a_retired_task_is_refused_with_the_uninstalled_model_sentence() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("the worker starts");
        runner.retire_task(TASK_MASK);
        runner.retire_task(TASK_MASK);
        let fail = runner.submit(TASK_MASK, frame(), mask()).expect_err("the model has been removed");
        assert_eq!(fail.status.code(), 400, "{fail}");
        assert_eq!(fail.sentence, "The Select Subject model isn't installed.", "the same sentence an uninstalled model gets");
        assert!(lock(&runner.shared).jobs.is_empty(), "the refused job is never queued");
        let other = runner.submit(TASK_FILL, frame(), mask()).expect("the other feature still works");
        assert_eq!(settle(&runner, &other), State::Done, "retiring one feature leaves the other alone");
        runner.stop();
    }

    #[test]
    fn a_job_queued_before_the_removal_fails_with_that_sentence_instead_of_running() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(30) }, MAX_QUEUE).expect("the worker starts");
        let first = runner.submit(TASK_FILL, frame(), mask()).expect("the first job holds the worker");
        assert_eq!(settle(&runner, &first), State::Running, "the worker is busy with the first job");
        let second = runner.submit(TASK_MASK, frame(), mask()).expect("the second job is queued");
        runner.retire_task(TASK_MASK);
        assert_eq!(finish(&runner, &first), State::Done, "the job already inside the backend finishes normally");
        let deadline = std::time::Instant::now() + PATIENCE;
        let (state, message) = loop {
            let (state, _, message) = runner.state(&second).expect("the job exists");
            if state.is_finished() {
                break (state, message);
            }
            assert!(std::time::Instant::now() < deadline, "the queued job never reached a final state");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(state, State::Failed, "it never reaches a backend that is still holding the file");
        assert_eq!(message, "The Select Subject model isn't installed.");
        runner.stop();
    }

    /// MG-13 review round 2: the window installs a model while this manager is already serving.
    #[test]
    fn a_backend_pointed_at_an_installed_model_serves_the_next_job() {
        let runner = Runner::new(Backend::NoModel, MAX_QUEUE).expect("the worker starts");
        let before = runner.submit(TASK_FILL, frame(), mask()).expect("the queue takes the job");
        assert_eq!(finish(&runner, &before), State::Failed, "nothing is installed yet");
        assert_eq!(
            runner.state(&before).expect("the job exists").2,
            crate::routing::not_ready_sentence(TASK_FILL),
            "the same sentence an uninstalled model gets"
        );
        runner.set_routing(crate::routing::Offered::fake(), Backend::Fake { step_delay: Duration::ZERO });
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("the model the window just installed");
        assert_eq!(finish(&runner, &id), State::Done, "the next job runs on the backend the refresh pointed at");
        runner.stop();
    }

    /// A refresh must not reach into a job that is already inside a backend.
    #[test]
    fn a_job_already_running_keeps_the_backend_it_took_when_the_backend_is_swapped() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::from_millis(40) }, MAX_QUEUE).expect("the worker starts");
        let first = runner.submit(TASK_FILL, frame(), mask()).expect("the first job holds the worker");
        assert_eq!(settle(&runner, &first), State::Running, "the worker is inside the fake backend");
        runner.set_routing(crate::routing::Offered::default(), Backend::NoModel);
        assert_eq!(finish(&runner, &first), State::Done, "the job that was already running finishes on the backend it took");
        let second = runner.submit(TASK_FILL, frame(), mask()).expect("the next job is still accepted");
        assert_eq!(finish(&runner, &second), State::Failed, "the next job gets the new backend");
        assert_eq!(runner.state(&second).expect("the job exists").2, crate::routing::not_ready_sentence(TASK_FILL));
        runner.stop();
    }

    /// MG-13 review round 2: a Remove that did not delete must not leave the task retired.
    #[test]
    fn restoring_a_retired_task_routes_it_again() {
        let runner = Runner::new(Backend::Fake { step_delay: Duration::ZERO }, MAX_QUEUE).expect("the worker starts");
        runner.retire_task(TASK_MASK);
        let fail = runner.submit(TASK_MASK, frame(), mask()).expect_err("the task was retired");
        assert_eq!(fail.status.code(), 400, "{fail}");
        runner.restore_task(TASK_MASK);
        runner.restore_task(TASK_MASK);
        let id = runner.submit(TASK_MASK, frame(), mask()).expect("the model is still installed, so the task routes again");
        assert_eq!(finish(&runner, &id), State::Done, "and the job runs");
        runner.stop();
    }

    /// MG-13 review round 3: the task list a request is checked against and the backend its job runs
    /// on are one value, so they cannot be seen half-updated. The race itself is a window of one
    /// instruction, which no test can land in on purpose; what this test pins is the agreement -
    /// a start that found nothing advertises nothing, and a refresh moves both together.
    #[test]
    fn the_task_list_and_the_backend_move_as_one_pair() {
        let runner =
            Runner::with_routing(Snapshot::new(Offered::default(), Backend::NoModel), MAX_QUEUE, super::DEFAULT_MEMORY_BYTES).expect("the worker starts");
        assert!(!runner.offered().is_ready(TASK_FILL), "a start that found no model advertises no model");
        let early = runner.submit(TASK_FILL, frame(), mask()).expect("the queue still takes the job");
        assert_eq!(finish(&runner, &early), State::Failed, "and it fails rather than running on a backend that was never pointed at it");
        assert_eq!(runner.state(&early).expect("the job exists").2, crate::routing::not_ready_sentence(TASK_FILL));
        runner.set_routing(Offered::fake(), Backend::Fake { step_delay: Duration::ZERO });
        assert!(runner.offered().is_ready(TASK_FILL), "the same call that points the queue at a backend advertises it");
        let id = runner.submit(TASK_FILL, frame(), mask()).expect("the model the window just installed");
        assert_eq!(finish(&runner, &id), State::Done, "the job accepted against that list runs on that backend");
        runner.stop();
    }
}
