//! The job side of the protocol: a FIFO queue, one worker thread, and the states a job moves
//! through while it runs. Amendment 11 fixes the states (`queued`, `running`, `done`, `failed`,
//! `cancelled`), the queue cap of eight, and the ten minutes a finished result is kept for.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{self, Filled};
use crate::error::{Fail, Result};
use crate::image::{self, Frame};
use crate::lama;

/// Amendment 11: at most eight jobs queued or running at once; the ninth is a 503.
pub const MAX_QUEUE: usize = 8;
/// Amendment 11: a finished result is kept for ten minutes, or until it has been read.
pub const RESULT_TTL: Duration = Duration::from_secs(600);
/// How long the worker waits for the next job before it sweeps finished results.
const WORKER_POLL: Duration = Duration::from_millis(50);
/// The sentence for a job that is not finished yet.
const NOT_READY: &str = "The result is not ready yet.";
/// The sentence for an id this manager has never heard of.
const NO_SUCH_JOB: &str = "Craft AI Manager has no job with that id.";

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
    provenance: Option<Provenance>,
    /// When the job finished, which starts the ten-minute clock.
    finished_at: Option<Instant>,
}

/// The jobs this manager knows about, in the order they arrived.
struct Shared {
    jobs: Vec<Entry>,
    next_id: u64,
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
    queue_tx: Sender<String>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    max_queue: usize,
}

impl Runner {
    /// Start the worker. The fake backend's `step_delay` spaces its ten progress steps, which is
    /// what lets the tests have eight jobs in flight at once.
    pub fn new(backend: Backend, max_queue: usize) -> Result<Runner> {
        let (queue_tx, queue_rx) = std::sync::mpsc::channel::<String>();
        let shared = Arc::new(Mutex::new(Shared { jobs: Vec::new(), next_id: 0 }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_stop = Arc::clone(&stop);
        let backend = Arc::new(backend);
        let handle = std::thread::Builder::new()
            .name("caim-jobs".to_string())
            .spawn(move || worker(worker_shared, queue_rx, worker_stop, backend))
            .map_err(|error| Fail::internal(format!("Craft AI Manager could not start its job worker: {error}")))?;
        Ok(Runner { shared, queue_tx, worker: Mutex::new(Some(handle)), stop, max_queue })
    }

    /// Accept a job and put it at the back of the queue. The id is opaque to the client, and safe
    /// to put in a URL path.
    pub fn submit(&self, frame: Frame, mask: Vec<u8>) -> Result<String> {
        let id = {
            let mut shared = lock(&self.shared);
            sweep_locked(&mut shared);
            let in_flight = shared.jobs.iter().filter(|entry| !entry.state.is_finished()).count();
            if in_flight >= self.max_queue {
                return Err(Fail::busy(format!("Craft AI Manager already has {} jobs waiting, so it cannot take another one yet.", self.max_queue)));
            }
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
                provenance: None,
                finished_at: None,
            });
            id
        };
        self.queue_tx.send(id.clone()).map_err(|_| Fail::internal(NO_SUCH_JOB))?;
        Ok(id)
    }

    /// `DELETE /v1/jobs/{id}`: cancel a queued or running job. A finished job answers the same way.
    pub fn cancel(&self, id: &str) -> Result<()> {
        let mut shared = lock(&self.shared);
        let Some(entry) = find_mut(&mut shared.jobs, id) else {
            return Err(Fail::not_found(NO_SUCH_JOB));
        };
        if !entry.state.is_finished() {
            entry.state = State::Cancelled;
            entry.message = "Cancelled.".to_string();
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
        entry.png.take().ok_or_else(|| Fail::not_found("That result has already been read."))
    }

    /// Stop the worker and wait for the job it is on to finish.
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
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
        if let Some(entry) = find_mut(&mut lock(&progress_shared).jobs, &progress_id) {
            entry.progress = value;
            entry.message = message.to_string();
        }
    };
    let outcome = match backend {
        Backend::NoModel => Err(Fail::bad_request("No model is installed for fill_masked_area yet")),
        Backend::Fake { step_delay } => backend::run_fill(&image, &mask, &cancel, *step_delay, &mut progress),
        Backend::Lama(pipeline) => pipeline.run_fill(&image, &mask, &cancel, &mut progress),
    };
    finish(shared, id, &cancel, outcome);
}

/// Record how the job ended. A job the client already cancelled stays cancelled.
fn finish(shared: &Arc<Mutex<Shared>>, id: &str, cancel: &std::sync::atomic::AtomicBool, outcome: Result<Filled>) {
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
        return;
    }
    match ready {
        Ok((png, provenance)) => {
            entry.state = State::Done;
            entry.progress = 1.0;
            entry.message = "Done.".to_string();
            entry.png = Some(png);
            entry.provenance = Some(provenance);
        }
        Err(fail) => {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                entry.state = State::Cancelled;
                entry.message = "Cancelled.".to_string();
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
    shared.jobs.retain(|entry| match entry.finished_at {
        Some(finished) => now.saturating_duration_since(finished) < RESULT_TTL,
        None => true,
    });
}

#[cfg(test)]
mod tests {
    use super::{Backend, MAX_QUEUE, Provenance, Runner, State};
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
}
