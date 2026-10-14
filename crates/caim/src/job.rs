//! The job side of the protocol: a FIFO queue, one worker thread, and the states a job moves
//! through while it runs. Amendment 11 fixes the states (`queued`, `running`, `done`, `failed`,
//! `cancelled`), the queue cap of eight, and the ten minutes a finished result is kept for.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{self, FAKE_DEVICE, FAKE_MODEL, FAKE_PROVIDER};
use crate::error::{Fail, Result};
use crate::image::{self, Frame};

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
    /// Start the worker. `step_delay` spaces the fake backend's ten progress steps, which is what
    /// lets the tests have eight jobs in flight at once.
    pub fn new(fake_backend: bool, max_queue: usize, step_delay: Duration) -> Result<Runner> {
        let (queue_tx, queue_rx) = std::sync::mpsc::channel::<String>();
        let shared = Arc::new(Mutex::new(Shared { jobs: Vec::new(), next_id: 0 }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("caim-jobs".to_string())
            .spawn(move || worker(worker_shared, queue_rx, worker_stop, fake_backend, step_delay))
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
fn worker(shared: Arc<Mutex<Shared>>, queue_rx: Receiver<String>, stop: Arc<std::sync::atomic::AtomicBool>, fake_backend: bool, step_delay: Duration) {
    use std::sync::atomic::Ordering;
    while !stop.load(Ordering::Relaxed) {
        match queue_rx.recv_timeout(WORKER_POLL) {
            Ok(id) => run_one(&shared, &id, fake_backend, step_delay),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        sweep(&shared);
    }
}

/// Run one job from the queue. A job cancelled while it waited never starts.
fn run_one(shared: &Arc<Mutex<Shared>>, id: &str, fake_backend: bool, step_delay: Duration) {
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
    // The progress callback takes the lock only for the assignment, never across the fill.
    let progress_shared = Arc::clone(shared);
    let progress_id = id.to_string();
    let mut progress = move |value: f64| {
        if let Some(entry) = find_mut(&mut lock(&progress_shared).jobs, &progress_id) {
            entry.progress = value;
        }
    };
    let outcome = if fake_backend {
        backend::run_fill(&image, &mask, &cancel, step_delay, &mut progress).map(|filled| (filled.frame, filled.elapsed_ms))
    } else {
        Err(Fail::bad_request("No model is installed for fill_masked_area yet"))
    };
    finish(shared, id, &cancel, outcome);
}

/// Record how the job ended. A job the client already cancelled stays cancelled.
fn finish(shared: &Arc<Mutex<Shared>>, id: &str, cancel: &std::sync::atomic::AtomicBool, outcome: Result<(Frame, u64)>) {
    let ready = match outcome {
        Ok((frame, elapsed_ms)) => match image::encode(&frame) {
            Ok(png) => Ok((
                png,
                Provenance {
                    model: FAKE_MODEL.to_string(),
                    device: FAKE_DEVICE.to_string(),
                    provider: FAKE_PROVIDER.to_string(),
                    elapsed_ms,
                    image_size: [frame.width, frame.height],
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
    use super::{MAX_QUEUE, Provenance, Runner, State};
    use crate::image::Frame;
    use std::time::Duration;

    fn frame() -> Frame {
        Frame { width: 2, height: 2, channels: 3, bit_depth: 8, samples: vec![10, 20, 30, 200, 100, 50, 40, 60, 80, 90, 20, 10] }
    }

    fn mask() -> Vec<u8> {
        vec![255, 0, 0, 0]
    }

    /// Wait for a job to reach a final state, without spinning forever if it never does.
    fn finish(runner: &Runner, id: &str) -> State {
        for _ in 0..400 {
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
        for _ in 0..400 {
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
        let runner = Runner::new(true, MAX_QUEUE, Duration::ZERO).expect("the worker starts");
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
        let runner = Runner::new(true, 1, Duration::from_millis(20)).expect("the worker starts");
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
        let runner = Runner::new(true, 2, Duration::from_millis(10)).expect("the worker starts");
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
        let runner = Runner::new(true, MAX_QUEUE, Duration::ZERO).expect("the worker starts");
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
}
