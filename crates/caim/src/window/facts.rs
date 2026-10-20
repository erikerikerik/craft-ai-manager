//! What this machine has, read from the library rather than remembered.
//!
//! [Source] is the window's only way to the rest of CAIM: the catalog, the store, the activity log,
//! the queue and the card. It owns no copy of any of them, which is what lets a restart show the
//! same window and lets a job started by PhotoCraft appear in it a second later. The two things that
//! cost anything to find out - the card probe and the activity log - are cached, so a repaint every
//! hundred milliseconds never starts a process or re-reads a file.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::activity;
use crate::catalog;
use crate::catalog::Model;
use crate::cuda;
use crate::error::Result;
use crate::job::Runner;
use crate::models;
use crate::server;
use crate::store;
use crate::window::{format, state};

/// How long a card probe stays an answer. `nvidia-smi` costs a process, so the window caches it
/// rather than running one on every repaint.
const CARD_TTL: Duration = Duration::from_secs(10);
/// How long the Details list may keep an activity answer it has already read.
const LOG_TTL: Duration = Duration::from_millis(2_000);
/// The finished jobs the window reads: more than the status sentence uses, and the number the
/// brief's "Copy details for a bug report" asks for.
const LOG_LINES: usize = 20;

/// Something the window started, as its worker reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Busy {
    /// A model download: which model, how many bytes so far, how many in total.
    Model { id: String, done: u64, total: u64 },
    /// The CUDA libraries: how many bytes so far, how many in total.
    Cuda { done: u64, total: u64 },
    /// A model being re-hashed against the catalog.
    Checking { id: String },
    /// A model being deleted.
    Removing { id: String },
}

/// What the window's workers have learned, kept apart from the facts so a worker can update it
/// while the UI thread reads it.
#[derive(Clone, Debug, Default)]
pub struct Work {
    /// Whether a worker has taken this window. `busy` stays empty until a download's first byte, so
    /// it cannot be the thing that says a second press is too early (MG-13 review round 3).
    claimed: bool,
    busy: Option<Busy>,
    damaged: Vec<String>,
    out_of_space: Vec<String>,
    error: Option<String>,
}

impl Work {
    /// What the window is doing right now, if anything.
    #[must_use]
    pub fn busy(&self) -> Option<&Busy> {
        self.busy.as_ref()
    }

    /// Say what the window is doing, or that it is doing nothing.
    pub fn set_busy(&mut self, busy: Option<Busy>) {
        self.busy = busy;
    }

    /// Take this window for one task, or say that somebody already has it.
    ///
    /// The claim happens before the worker starts, which is the only way to refuse a second press
    /// that arrives in the moment before the first worker has anything to report.
    pub fn claim(&mut self) -> bool {
        if self.claimed || self.busy.is_some() {
            return false;
        }
        self.claimed = true;
        true
    }

    /// Give the window back, so the next press can have it.
    pub fn release(&mut self) {
        self.claimed = false;
    }

    /// Whether a worker has this window, which is true a moment before it has anything to report.
    #[must_use]
    pub fn claimed(&self) -> bool {
        self.claimed
    }

    /// A model download's progress callback.
    pub fn model_progress(&mut self, id: &str, done: u64, total: u64) {
        self.busy = Some(Busy::Model { id: id.to_string(), done, total });
    }

    /// The CUDA libraries' progress callback.
    pub fn cuda_progress(&mut self, done: u64, total: u64) {
        self.busy = Some(Busy::Cuda { done, total });
    }

    /// A model whose bytes on disk are not the bytes the catalog pins.
    pub fn mark_damaged(&mut self, id: &str) {
        keep(&mut self.damaged, id);
    }

    /// A model that has just been checked and found whole.
    pub fn forget_damaged(&mut self, id: &str) {
        drop_name(&mut self.damaged, id);
    }

    /// A model whose download stopped because the disk had no room.
    pub fn mark_no_space(&mut self, id: &str) {
        keep(&mut self.out_of_space, id);
    }

    /// A model whose bytes are being fetched again.
    pub fn forget_no_space(&mut self, id: &str) {
        drop_name(&mut self.out_of_space, id);
    }

    /// The raw error behind a needs-you state, for Details. `None` clears it.
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    #[must_use]
    pub fn is_damaged(&self, id: &str) -> bool {
        self.damaged.iter().any(|named| named == id)
    }

    #[must_use]
    pub fn is_out_of_space(&self, id: &str) -> bool {
        self.out_of_space.iter().any(|named| named == id)
    }

    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// Add a name once, so three failed writes do not make three problems.
fn keep(names: &mut Vec<String>, id: &str) {
    if !names.iter().any(|named| named == id) {
        names.push(id.to_string());
    }
}

/// Remove a name, which is what a successful check or a fresh download does.
fn drop_name(names: &mut Vec<String>, id: &str) {
    names.retain(|named| named != id);
}

/// The card, as the last probe found it.
#[derive(Debug, Default)]
struct Probe {
    in_flight: bool,
    at: Option<Instant>,
    card: Option<cuda::Card>,
}

/// The finished jobs, as the last read found them.
#[derive(Debug, Default)]
struct Log {
    at: Option<Instant>,
    jobs: Vec<state::JobFacts>,
}

/// Where this manager keeps things, and what its workers know.
///
/// One of these is shared by the UI thread and every worker it starts, so it holds only things that
/// can be read while something else is writing them.
pub struct Source {
    runner: Arc<Runner>,
    /// The server's live routing, which this window changes whenever it installs or removes a model
    /// (MG-13 review round 2).
    routing: server::Routing,
    port: u16,
    endpoint: Option<PathBuf>,
    activity: Option<PathBuf>,
    started: Instant,
    root: PathBuf,
    cuda_root: Option<PathBuf>,
    cuda_bytes: u64,
    /// NVIDIA's own catalog entries, which carry the two agreements the offer has to show.
    cuda: Vec<Model>,
    models: Vec<Model>,
    work: Arc<Mutex<Work>>,
    card: Arc<Mutex<Probe>>,
    log: Mutex<Log>,
}

impl Source {
    /// Open the window's view of this machine. A machine with no models folder, or a catalog that
    /// will not parse, has no window: the caller serves in the terminal instead.
    pub fn new(runner: Arc<Runner>, port: u16, endpoint: Option<PathBuf>, routing: server::Routing) -> Result<Source> {
        Self::build(runner, port, endpoint, routing, models::models_root()?)
    }

    /// The same view pointed at a models folder of the caller's choosing. MG-13 review round 3: the
    /// tests for installing, checking and removing a model need a store they can put a whole model
    /// into, rather than whatever this machine happens to have.
    #[cfg(test)]
    pub(crate) fn at_root(runner: Arc<Runner>, port: u16, endpoint: Option<PathBuf>, routing: server::Routing, root: PathBuf) -> Result<Source> {
        Self::build(runner, port, endpoint, routing, root)
    }

    fn build(runner: Arc<Runner>, port: u16, endpoint: Option<PathBuf>, routing: server::Routing, root: PathBuf) -> Result<Source> {
        let models = catalog::load()?;
        let cuda_components = cuda::components();
        let cuda_bytes = cuda_components.iter().map(|component| component.size).sum();
        Ok(Source {
            cuda: cuda_components,
            runner,
            routing,
            port,
            activity: activity::activity_path_for(endpoint.as_deref()),
            endpoint,
            started: Instant::now(),
            cuda_root: cuda::store_root(),
            cuda_bytes,
            root,
            models,
            work: Arc::new(Mutex::new(Work::default())),
            card: Arc::new(Mutex::new(Probe::default())),
            log: Mutex::new(Log::default()),
        })
    }

    /// The models folder, for the Details list and for the worker's own file work.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The activity log this manager writes, which Details offers to open.
    #[must_use]
    pub fn activity_path(&self) -> Option<&Path> {
        self.activity.as_deref()
    }
    /// The folder NVIDIA's libraries live in, when this machine has somewhere to put them.
    #[must_use]
    pub fn cuda_root(&self) -> Option<&Path> {
        self.cuda_root.as_deref()
    }

    /// The CUDA libraries' size, which is the number the offer and its button show.
    #[must_use]
    pub fn cuda_bytes(&self) -> u64 {
        self.cuda_bytes
    }

    /// The two agreements the offer has to show before it fetches anything: what each is called and
    /// the sentence NVIDIA's catalog carries for it, verbatim.
    #[must_use]
    pub fn cuda_notes(&self) -> Vec<(String, String)> {
        self.cuda.iter().filter_map(|component| component.note.as_ref().map(|note| (component.licence_code.clone(), note.note.clone()))).collect()
    }

    /// The queue this manager answers with, which the worker needs to retire and cancel.
    #[must_use]
    pub fn runner(&self) -> Arc<Runner> {
        Arc::clone(&self.runner)
    }

    /// Tell the server that what this machine has installed may have changed (MG-13 review round 2).
    ///
    /// The worker does this after every install, acknowledgement and removal, so a model installed
    /// from the window is routable without restarting Craft AI Manager, and one the window removed
    /// stops being offered.
    pub fn refresh_routing(&self) {
        self.routing.refresh();
    }

    /// The catalog entry with this id, which the worker looks up rather than trusts the UI with.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<Model> {
        self.models.iter().find(|model| model.id == id).cloned()
    }

    /// The shared work state, for a worker's progress callback.
    #[must_use]
    pub fn work(&self) -> Arc<Mutex<Work>> {
        Arc::clone(&self.work)
    }

    /// What this window is doing right now, which is what a row shows in place of its button.
    #[must_use]
    pub fn busy(&self) -> Option<Busy> {
        lock(&self.work).busy().cloned()
    }

    /// Whether a worker has this window. This is true before [Source::busy] can say what the worker is
    /// doing, which is what lets a refused press say something honest about the gap in between
    /// (MG-13 review round 5).
    #[must_use]
    pub fn is_claimed(&self) -> bool {
        lock(&self.work).claimed()
    }

    /// The whole state model, as this machine is right now.
    #[must_use]
    pub fn gather(&self) -> state::Facts {
        let work = lock(&self.work);
        state::Facts {
            version: crate::VERSION.to_string(),
            models: self.models.iter().map(|model| self.model_facts(model, &work)).collect(),
            gpu: self.gpu(),
            jobs: self.jobs(),
            running: self.runner.active().map(|active| state::RunningFacts {
                task: active.task,
                started_secs: active.started_secs,
                progress_percent: active.progress_percent,
            }),
            endpoint: Some(state::EndpointFacts {
                port: self.port,
                path: self.endpoint.as_ref().map_or_else(|| "none".to_string(), |path| path.display().to_string()),
                up_secs: self.started.elapsed().as_secs(),
            }),
            folder: Some(state::FolderFacts { path: self.root.display().to_string(), bytes: store::folder_usage(&self.root).map(|usage| usage.bytes) }),
            error: work.error.clone(),
        }
    }

    /// Ask for the card, if it is due. The probe runs on its own thread and the answer appears in
    /// the next repaint, which is what keeps `nvidia-smi` off the UI thread.
    pub fn probe_card(&self) {
        {
            let probe = lock(&self.card);
            if probe.in_flight {
                return;
            }
            if probe.at.is_some_and(|at| at.elapsed() < CARD_TTL) {
                return;
            }
        }
        let cell = Arc::clone(&self.card);
        {
            let mut probe = lock(&cell);
            probe.in_flight = true;
        }
        let spawned = std::thread::Builder::new().name("caim-card-probe".to_string()).spawn(move || {
            let found = cuda::cards().into_iter().next();
            let mut probe = lock(&cell);
            probe.card = found;
            probe.at = Some(Instant::now());
            probe.in_flight = false;
        });
        if spawned.is_err() {
            // A machine that cannot start a thread keeps the card it had, which is honest: the
            // window says what it knows and tries again when the answer is due.
            lock(&self.card).in_flight = false;
        }
    }

    /// The text behind "Copy details for a bug report" (section 4.5): the version, the operating
    /// system, the device, the raw error and the last twenty finished jobs. It carries no paths and
    /// never the token, which is the brief's rule and the reason the activity lines are re-worded
    /// rather than pasted.
    #[must_use]
    pub fn bug_report(&self, view: &state::View) -> String {
        let facts = self.gather();
        let mut lines = vec![
            format!("Craft AI Manager {}", crate::VERSION),
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            format!("device: {}", view.device),
            format!("address: 127.0.0.1:{}", self.port),
        ];
        if let Some(error) = facts.error {
            lines.push(String::new());
            lines.push(format!("what happened: {error}"));
        }
        lines.push(String::new());
        lines.push("last jobs:".to_string());
        if facts.jobs.is_empty() {
            lines.push("  nothing has run yet".to_string());
        }
        for job in &facts.jobs {
            lines.push(format!(
                "  {} \u{b7} {} \u{b7} {} \u{b7} {} ms \u{b7} {} ago",
                job.task,
                job.device,
                job.fallback.as_deref().unwrap_or("as asked"),
                job.elapsed_ms,
                format::ago_words(job.ago_secs)
            ));
        }
        lines.join("\n")
    }

    /// One catalog entry, as the state model needs it.
    fn model_facts(&self, model: &Model, work: &Work) -> state::ModelFacts {
        let installed = store::is_installed(&self.root, model);
        let downloading = match work.busy() {
            Some(Busy::Model { id, done, total }) if id == &model.id => Some((*done, *total)),
            _ => None,
        };
        let paused = if installed || downloading.is_some() { None } else { paused_pair(&self.root, model) };
        state::ModelFacts {
            id: model.id.clone(),
            version: model.version.clone(),
            task: model.task.clone(),
            size_bytes: model.size,
            note: model.note.as_ref().map(|note| note.note.clone()),
            installed,
            checked_unix: store::read_verified(&self.root, model).map(|verified| verified.verified_at_unix),
            acknowledged: model.note.is_none() || store::is_acknowledged(&self.root, model),
            downloading,
            paused,
            damaged: work.is_damaged(&model.id),
            out_of_space: work.is_out_of_space(&model.id),
        }
    }

    /// The finished jobs, newest first, read at most every [LOG_TTL].
    fn jobs(&self) -> Vec<state::JobFacts> {
        let mut cell = lock(&self.log);
        if !cell.at.is_some_and(|at| at.elapsed() < LOG_TTL) {
            cell.jobs =
                self.activity.as_ref().map(|path| activity::read_recent(path, LOG_LINES).into_iter().filter_map(job_facts).collect()).unwrap_or_default();
            cell.at = Some(Instant::now());
        }
        cell.jobs.clone()
    }

    /// The card, as the last probe found it. No card yet is not "no card": the state model only
    /// offers the libraries once a card has actually been seen.
    fn gpu(&self) -> Option<state::GpuFacts> {
        let card = lock(&self.card).card.clone()?;
        let ready = self.cuda_root.as_deref().is_some_and(cuda::is_ready);
        Some(state::GpuFacts { name: card.name, free_mib: card.free_mib, total_mib: card.total_mib, ready, libraries_bytes: self.cuda_bytes })
    }
}

/// One log line, if it is a finished job the window has words for.
///
/// The brief's rule is the newest line that names a task and reports a time, which is what a done,
/// failed or cancelled job always does. A line with no task, or one written by a run that never
/// reached a backend, is skipped rather than guessed at.
fn job_facts(record: activity::Record) -> Option<state::JobFacts> {
    let task = record.task?;
    let elapsed_ms = record.elapsed_ms?;
    let now = store::now_unix();
    let then = activity::unix_from_stamp(&record.time).unwrap_or(now);
    Some(state::JobFacts { task, device: record.device.unwrap_or_default(), fallback: record.fallback_reason, elapsed_ms, ago_secs: now.saturating_sub(then) })
}

/// A part left on disk that the next install could carry on from: the bytes here and the bytes the
/// catalog wants. This is section 4.4's "paused download", read from the folder rather than
/// remembered, so a window that starts after a crash still says "Paused \u{b7} 61%".
///
/// The note has to agree with the catalog about the URL, the revision and the hash, and the part
/// file it belongs to has to still be there. A part that is empty, or already the whole file, is not
/// a paused download.
fn paused_pair(root: &Path, model: &Model) -> Option<(u64, u64)> {
    let entries = std::fs::read_dir(store::model_folder(root, model)).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !store::is_part_meta_name(model, &name) {
            continue;
        }
        let note_path = entry.path();
        if !note_path.with_extension("").is_file() {
            continue;
        }
        let Some(note) = store::read_part_meta(&note_path) else {
            continue;
        };
        if note.url != model.url || note.revision != model.revision || note.sha256 != model.sha256 {
            continue;
        }
        if note.bytes == 0 || note.bytes >= model.size {
            continue;
        }
        return Some((note.bytes, model.size));
    }
    None
}

/// Take a lock. A worker that was stopped mid-write leaves its data consistent, so a poisoned lock
/// is unwrapped rather than turned into a panic the window would never see.
fn lock<T>(cell: &Mutex<T>) -> MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{Busy, Work};

    /// MG-13 review round 3: the claim, not the progress report, is what says a second press is too
    /// early - a download has nothing to show until its first byte has arrived.
    #[test]
    fn a_claimed_window_stays_claimed_until_the_worker_gives_it_back() {
        let mut work = Work::default();
        assert!(work.claim(), "an idle window can be taken");
        assert!(!work.claim(), "a second press is refused while the first worker has it");
        work.release();
        assert!(work.claim(), "and the next press gets it");
        work.set_busy(Some(Busy::Checking { id: "lama".to_string() }));
        work.release();
        assert!(!work.claim(), "a worker that is already reporting is a window that is busy");
    }
}
