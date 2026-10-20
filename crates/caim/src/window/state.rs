//! The window's state model: facts in, the brief's strings out.
//!
//! This is the half of MG-13 that can be tested without a screen. [Facts] is what the library
//! knows about this machine - the catalog, the store, the activity log, the card, the running job -
//! and [view] turns it into the sentences of `agents/CAIM-UI-BRIEF.md` sections 3 and 4. The
//! eframe layer in [crate::window::app] only draws what comes back.

use crate::activity;
use crate::window::format;

/// One catalog model, as the window sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFacts {
    /// The catalog id, which the window uses to name controls.
    pub id: String,
    /// The catalog version.
    pub version: String,
    /// The catalog task, which picks the feature's words.
    pub task: String,
    /// The size the catalog pins, in bytes.
    pub size_bytes: u64,
    /// The entry's override note, verbatim, when it has one.
    pub note: Option<String>,
    /// Whether the model file is on disk.
    pub installed: bool,
    /// When the file was last checked against the catalog, or `None` when it never has been.
    pub checked_unix: Option<u64>,
    /// Whether the current note has been acknowledged.
    pub acknowledged: bool,
    /// A download this window started: the bytes so far and the bytes in total.
    pub downloading: Option<(u64, u64)>,
    /// A part left on disk from a run that stopped: the bytes on disk and the bytes in total.
    pub paused: Option<(u64, u64)>,
    /// The last check said the bytes on disk are not the bytes the catalog pins.
    pub damaged: bool,
    /// The last write failed because the disk had no room left.
    pub out_of_space: bool,
}

/// The graphics card, as `nvidia-smi` and the CUDA folder report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuFacts {
    /// The card's name, as the probe reports it.
    pub name: String,
    /// Free and total memory in mebibytes.
    pub free_mib: u64,
    pub total_mib: u64,
    /// Whether CUDA and cuDNN are here and a CUDA job could run.
    pub ready: bool,
    /// The size of the libraries NVIDIA's redistribution offers, in bytes.
    pub libraries_bytes: u64,
}

/// One finished job from the activity log, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobFacts {
    pub task: String,
    pub device: String,
    pub fallback: Option<String>,
    pub elapsed_ms: u64,
    /// How long ago the job finished.
    pub ago_secs: u64,
}

/// The job this manager is filling right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningFacts {
    pub task: String,
    /// How long ago the job started.
    pub started_secs: u64,
    pub progress_percent: u64,
}

/// The address this run published, and how long it has been up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointFacts {
    pub port: u16,
    pub path: String,
    pub up_secs: u64,
}

/// The models folder and what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderFacts {
    pub path: String,
    pub bytes: Option<u64>,
}

/// Everything the window knows, gathered by [crate::window::facts].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The version of this manager.
    pub version: String,
    /// Every catalog model, in catalog order. Nothing is hidden.
    pub models: Vec<ModelFacts>,
    /// The card this machine has, or `None` when it has no NVIDIA card.
    pub gpu: Option<GpuFacts>,
    /// Finished jobs, newest first.
    pub jobs: Vec<JobFacts>,
    /// The job being filled now, if any.
    pub running: Option<RunningFacts>,
    /// The published address, if this run published one.
    pub endpoint: Option<EndpointFacts>,
    /// The models folder, if this machine has one.
    pub folder: Option<FolderFacts>,
    /// What this run was asked to run its jobs on, as the queue reports it: `auto`, `cpu` or
    /// `cuda`. Empty when the backend has no device of its own, which is read as "not asked".
    ///
    /// Erik's acceptance review (Oct 20, 2026) asked the window to say which hardware the AI runs
    /// on, and the card alone is not the answer: a run started with `--device cpu` uses the
    /// processor whatever the machine has.
    pub device_setting: String,
    /// The raw error behind a needs-you state, for Details.
    pub error: Option<String>,
}

/// What the right-hand end of a feature row shows, and what the row can be clicked on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    /// Not installed: the button reads "Add · 198 MB".
    Add { size: String },
    /// A download this window started.
    Downloading { percent: u64 },
    /// Installed, checked and acknowledged.
    Ready,
    /// A job is running on this model.
    Working { percent: u64 },
    /// The bytes on disk do not match the catalog.
    NeedsRepair,
    /// A part is on disk with nothing fetching the rest.
    Paused { percent: u64 },
    /// Installed, but the note has to be read again.
    NeedsYourOk,
}

impl RowState {
    /// The words on the right of the row, exactly as section 3 lists them.
    #[must_use]
    pub fn words(&self) -> String {
        match self {
            Self::Add { size } => format!("Add \u{b7} {size}"),
            Self::Downloading { percent } => format!("{percent}%"),
            Self::Ready => "Ready".to_string(),
            Self::Working { .. } => "Working".to_string(),
            Self::NeedsRepair => "\u{25cf} Needs repair".to_string(),
            Self::Paused { percent } => format!("\u{25cf} Paused \u{b7} {percent}%"),
            Self::NeedsYourOk => "\u{25cf} Needs your OK".to_string(),
        }
    }

    /// Whether this state is one of the three the brief marks with the accent dot.
    #[must_use]
    pub const fn needs_you(&self) -> bool {
        matches!(self, Self::NeedsRepair | Self::Paused { .. } | Self::NeedsYourOk)
    }
}

/// One feature row: the model's title, purpose, meta line and right-hand state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub task: String,
    pub title: String,
    pub purpose: String,
    pub meta: String,
    pub state: RowState,
    /// The operations this row offers on itself, in the order the window draws them.
    ///
    /// These are the two Details already offers for an installed model - `Check again` and
    /// `Remove` - said again where the row can be seen. Erik's acceptance review (Oct 20, 2026)
    /// found a row that ended at `Ready` and nothing else: the operations existed, but only behind
    /// Details, so they read as absent. Nothing here is a new operation.
    pub manage: Vec<String>,
}

/// The action block, which only the needs-you states and the inline licence notes show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    /// The model the button acts on, so the window knows which one to repair, resume or acknowledge.
    pub model: Option<String>,
    /// The heading over the note, when a note is being shown.
    pub heading: Option<String>,
    /// The note itself, verbatim from the catalog.
    pub note: Option<String>,
    /// The primary button's words.
    pub button: String,
    /// Whether the button can be used yet, which is false until a checkbox is ticked.
    pub button_enabled: bool,
    /// The 12 px sentence under the button.
    pub explanation: String,
}

/// The graphics-card offer of section 4.3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub title: String,
    pub text: String,
    pub meta: String,
    pub button: String,
    /// The terms shown when the button is pressed.
    pub terms: String,
    /// The checkbox's words.
    pub accept_words: String,
    pub bytes: u64,
}

/// One line of the Details list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    pub label: String,
    pub value: String,
}

/// The whole window, as words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The bold part of the status sentence, 28 px.
    pub bold: String,
    /// The grey part of the status sentence, 28 px.
    pub grey: String,
    /// The one 12 px line under it.
    pub meta: String,
    /// The action block, only when something is needed.
    pub action: Option<Action>,
    /// One row per catalog model.
    pub rows: Vec<Row>,
    /// The graphics-card offer, only when it applies.
    pub offer: Option<Offer>,
    /// The footer sentence.
    pub footer: String,
    /// The labeled device statement of the status block: `AI device: Processor (CPU)` or
    /// `AI device: Graphics card (<name>)`.
    pub device_line: String,
    /// The device Details and the bug report name, which the device line says in full.
    pub device: String,
    /// The Details list.
    pub details: Vec<Detail>,
    /// Whether Details offers "Copy details for a bug report".
    pub copy_details: bool,
}

/// The one problem the window leads with, in the brief's priority order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Problem {
    Damaged,
    NoteChanged,
    Paused,
    NoSpace,
}

/// The whole view for these facts.
#[must_use]
pub fn view(facts: &Facts) -> View {
    let rows = rows(facts);
    let problem = facts.iter_problem_model();
    let (bold, grey, meta, action) = match problem {
        Some((index, Problem::Damaged)) => damaged(facts, index),
        Some((index, Problem::NoteChanged)) => note_changed(facts, index),
        Some((index, Problem::Paused)) => paused(facts, index),
        Some((index, Problem::NoSpace)) => no_space(facts, index),
        None => status(facts),
    };
    View {
        bold,
        grey,
        meta,
        action,
        rows,
        offer: offer(facts),
        footer: footer(),
        device_line: format::device_line(device_choice(facts)),
        device: device_word(facts),
        details: details(facts),
        copy_details: facts.iter_problem_model().is_some(),
    }
}

/// One row per catalog model, in catalog order, with the state section 3 describes.
fn rows(facts: &Facts) -> Vec<Row> {
    let running_task = facts.running.as_ref().map(|running| running.task.clone());
    let running_percent = facts.running.as_ref().map_or(0, |running| running.progress_percent);
    facts
        .models
        .iter()
        .map(|model| {
            let feature = format::feature(&model.task);
            let name = format::display_name(&model.id);
            let title = feature.map_or_else(|| name.clone(), |feature| feature.title.to_string());
            let purpose = feature.map_or_else(String::new, |feature| feature.purpose.to_string());
            let mut meta = format!("{name} v{} \u{b7} {}", model.version, format::size(model.size_bytes));
            if model.note.is_some() {
                // Section 3: the licence words are text in the meta line, never a badge.
                meta.push_str(" \u{b7} non-commercial use only");
            }
            let state = row_state(model, running_task.as_deref(), running_percent);
            Row { id: model.id.clone(), task: model.task.clone(), title, purpose, meta, manage: manage_words(&state), state }
        })
        .collect()
}

/// The right-hand end of one row.
fn row_state(model: &ModelFacts, running_task: Option<&str>, running_percent: u64) -> RowState {
    if model.damaged {
        return RowState::NeedsRepair;
    }
    if model.installed && !model.acknowledged {
        return RowState::NeedsYourOk;
    }
    if let Some((done, total)) = model.downloading {
        return RowState::Downloading { percent: format::percent(done, total) };
    }
    if model.out_of_space {
        return RowState::Paused { percent: model.paused.map_or(0, |(done, total)| format::percent(done, total)) };
    }
    if let Some((done, total)) = model.paused {
        return RowState::Paused { percent: format::percent(done, total) };
    }
    if running_task == Some(model.task.as_str()) {
        return RowState::Working { percent: running_percent };
    }
    if model.installed {
        return RowState::Ready;
    }
    RowState::Add { size: format::size(model.size_bytes) }
}

/// The three rows of section 4.1 and the two of 4.2, when nothing needs the user.
fn status(facts: &Facts) -> (String, String, String, Option<Action>) {
    if let Some(running) = facts.running.as_ref() {
        let mut meta = format!("Started {}", format::ago_words(running.started_secs));
        if let Some((median, device)) = median_ms(&facts.jobs, &running.task) {
            meta.push_str(&format!(" \u{b7} usually about {} {}", format::about_words(median), format::device_words(&device)));
        }
        return ("Working.".to_string(), format::running_words(&running.task).to_string(), meta, None);
    }
    let installed: Vec<&ModelFacts> = facts.models.iter().filter(|model| model.installed).collect();
    let downloading = facts.models.iter().any(|model| model.downloading.is_some());
    if installed.is_empty() {
        // Section 4.1, row 2: the two big lines stay put while a download is in flight, and only
        // the 12 px line changes. Nothing is installed yet, so the bold part is still the offer.
        let meta = if downloading { "Downloading. You can keep working in PhotoCraft.".to_string() } else { "Nothing is installed yet.".to_string() };
        return ("Almost ready.".to_string(), "Add the AI features you want. Each downloads once, then works offline.".to_string(), meta, None);
    }
    let features: usize = installed.iter().map(|model| format::feature(&model.task).map_or(1, |feature| feature.count)).sum();
    let grey = if features == 1 { "PhotoCraft can use 1 AI feature.".to_string() } else { format!("PhotoCraft can use {features} AI features.") };
    let meta = if downloading {
        "Downloading. You can keep working in PhotoCraft.".to_string()
    } else if let Some(job) = facts.jobs.first() {
        let mut line = format!(
            "Last: {}, {} \u{b7} {} {}",
            format::task_words(&job.task),
            format::ago_words(job.ago_secs),
            format::elapsed_words(job.elapsed_ms),
            format::device_words(&job.device)
        );
        if let Some(reason) = job.fallback.as_deref().and_then(format::fallback_words) {
            line.push_str(&format!(", because {reason}"));
        }
        line
    } else {
        "Nothing has run yet.".to_string()
    };
    ("Ready.".to_string(), grey, meta, None)
}

/// Section 4.4, row 1: the bytes on disk are not the bytes the catalog pins.
fn damaged(facts: &Facts, index: usize) -> (String, String, String, Option<Action>) {
    let model = match facts.models.get(index) {
        Some(model) => model,
        None => return status(facts),
    };
    let title = format::feature(&model.task).map_or_else(|| format::display_name(&model.id), |feature| feature.title.to_string());
    // The brief's example is a model behind two features, so it says "Their". A model behind one
    // feature says "Its", which is the same sentence read in English.
    let owner = if format::feature(&model.task).is_some_and(|feature| feature.count > 1) { "Their" } else { "Its" };
    let name = format::display_name(&model.id);
    (
        format!("{title} can't run."),
        format!("{owner} model file is damaged."),
        format!("{name} is here, but its bytes are not the ones the catalog pins."),
        Some(Action {
            model: Some(model.id.clone()),
            heading: None,
            note: None,
            button: format!("Repair \u{b7} {}", format::size(model.size_bytes)),
            button_enabled: true,
            explanation: format!("Downloads {name} again and checks it before use."),
        }),
    )
}

/// Section 4.4, row 3: the note changed, so the install is paused until it is read again.
fn note_changed(facts: &Facts, index: usize) -> (String, String, String, Option<Action>) {
    let model = match facts.models.get(index) {
        Some(model) => model,
        None => return status(facts),
    };
    let title = format::feature(&model.task).map_or_else(|| format::display_name(&model.id), |feature| feature.title.to_string());
    (
        format!("{title} is paused."),
        "Its licence note changed. Read it to keep using it.".to_string(),
        "Nothing is downloaded again.".to_string(),
        Some(Action {
            model: Some(model.id.clone()),
            heading: Some("Non-commercial use only".to_string()),
            note: model.note.clone(),
            button: format!("Continue with {title}"),
            // The brief: the button stays disabled until the note's checkbox is ticked.
            button_enabled: false,
            explanation: "Nothing is downloaded again.".to_string(),
        }),
    )
}

/// Section 4.4, row 2: a part is on disk and nothing is fetching the rest.
fn paused(facts: &Facts, index: usize) -> (String, String, String, Option<Action>) {
    let model = match facts.models.get(index) {
        Some(model) => model,
        None => return status(facts),
    };
    let title = format::feature(&model.task).map_or_else(|| format::display_name(&model.id), |feature| feature.title.to_string());
    let (done, total) = model.paused.unwrap_or((0, model.size_bytes));
    (
        "Download paused.".to_string(),
        format!("{title} is {} in.", bytes_of_both(done, total)),
        format!("{} of {} is already here.", format::size(done), format::size(total)),
        Some(Action {
            model: Some(model.id.clone()),
            heading: None,
            note: None,
            button: "Resume".to_string(),
            button_enabled: true,
            explanation: "Checks the part already here, then fetches only the rest.".to_string(),
        }),
    )
}

/// Section 4.4, row 4: a write failed because the disk was full.
fn no_space(facts: &Facts, index: usize) -> (String, String, String, Option<Action>) {
    let model = match facts.models.get(index) {
        Some(model) => model,
        None => return status(facts),
    };
    let title = format::feature(&model.task).map_or_else(|| format::display_name(&model.id), |feature| feature.title.to_string());
    (
        "Download stopped.".to_string(),
        format!("There isn't room for the rest of {title}."),
        "The part already downloaded is kept, so nothing starts again from the beginning.".to_string(),
        Some(Action {
            model: Some(model.id.clone()),
            heading: None,
            note: None,
            button: "Try again".to_string(),
            button_enabled: true,
            explanation: "Free up space on C: first. The part already downloaded is kept.".to_string(),
        }),
    )
}

/// "120 of 198 MB", which is how the brief writes two sizes that share a unit.
fn bytes_of_both(done: u64, total: u64) -> String {
    if format::size_parts(done).1 == format::size_parts(total).1 {
        format!("{} of {}", format::size_parts(done).0, format::size(total))
    } else {
        format!("{} of {}", format::size(done), format::size(total))
    }
}

/// The offer of section 4.3: an NVIDIA card is here and the libraries are not.
fn offer(facts: &Facts) -> Option<Offer> {
    let gpu = facts.gpu.as_ref()?;
    if gpu.ready {
        return None;
    }
    Some(Offer {
        title: "Use your graphics card".to_string(),
        text: format!("{} found. AI features run many times faster on it.", gpu.name),
        meta: format!("NVIDIA CUDA and cuDNN libraries \u{b7} {} \u{b7} NVIDIA's licences", format::size(gpu.libraries_bytes)),
        button: format!("Add \u{b7} {}", format::size(gpu.libraries_bytes)),
        terms: "NVIDIA allows these libraries to be passed on, but they aren't open source. Without them every feature still works, on the processor."
            .to_string(),
        accept_words: "I accept NVIDIA's licence terms.".to_string(),
        bytes: gpu.libraries_bytes,
    })
}

/// The card this run would use for an AI job, or `None` when it would use the processor.
///
/// Two facts have to agree, which is what the footer's bare hardware name used to hide (Erik's
/// acceptance review, Oct 20, 2026): what this run was asked to use, and what the machine actually
/// has. `--device cpu` is the processor whatever the machine has; a card whose NVIDIA libraries are
/// not installed is not a device this manager can run on yet, which is what section 4.3's offer is
/// for. Only a card that is both here and usable is the graphics card.
#[must_use]
fn device_choice(facts: &Facts) -> Option<&str> {
    if facts.device_setting == crate::device::Setting::Cpu.token() {
        return None;
    }
    facts.gpu.as_ref().filter(|gpu| gpu.ready).map(|gpu| gpu.name.as_str())
}

/// The device Details and the bug report name, in the short form the brief's table uses.
fn device_word(facts: &Facts) -> String {
    device_choice(facts).map_or_else(|| "Processor".to_string(), |name| name.to_string())
}

/// The footer, which no longer carries a device: the labeled line in the status block says that in
/// plain words, and one sentence saying it twice is how it got missed in the first place.
fn footer() -> String {
    "This computer only \u{b7} Closing this window turns AI features off \u{b7} Details".to_string()
}

/// The operations an installed model offers on its own row.
///
/// Both words are the brief's own (section 4.5), and both mean what they say: `Check again` re-hashes
/// the bytes the catalog pins, and `Remove` deletes the model. There is no reload or rebuild to
/// offer, because the library has no such operation - a session is loaded on the next job. A model
/// that is not installed has nothing to check or remove, and a download in flight is not a model yet.
#[must_use]
fn manage_words(state: &RowState) -> Vec<String> {
    match state {
        RowState::Ready | RowState::Working { .. } | RowState::NeedsRepair | RowState::NeedsYourOk => {
            vec![format::CHECK_AGAIN_WORDS.to_string(), format::REMOVE_WORDS.to_string()]
        }
        RowState::Add { .. } | RowState::Downloading { .. } | RowState::Paused { .. } => Vec::new(),
    }
}

/// The Details list of section 4.5, in the brief's order.
fn details(facts: &Facts) -> Vec<Detail> {
    let installed: Vec<&ModelFacts> = facts.models.iter().filter(|model| model.installed).collect();
    let models = if installed.is_empty() {
        "Nothing is installed.".to_string()
    } else {
        installed
            .iter()
            .map(|model| {
                let mut line = format!("{} v{} \u{b7} {}", format::display_name(&model.id), model.version, format::size(model.size_bytes));
                if let Some(checked) = model.checked_unix {
                    line.push_str(&format!(" \u{b7} checked {}", format::date_words(checked)));
                }
                line
            })
            .collect::<Vec<String>>()
            .join("\n")
    };
    let folder = facts.folder.as_ref().map_or_else(
        || "This computer has no models folder.".to_string(),
        |folder| folder.bytes.map_or_else(|| folder.path.clone(), |bytes| format!("{} \u{b7} {}", folder.path, format::size(bytes))),
    );
    let card = facts.gpu.as_ref().map_or_else(
        || "No NVIDIA card found \u{b7} features run on the processor".to_string(),
        |gpu| {
            if gpu.ready {
                format!(
                    "{} \u{b7} CUDA ready \u{b7} {} of {} free",
                    gpu.name,
                    format::gib_words(gpu.free_mib),
                    format::size(gpu.total_mib.saturating_mul(1024 * 1024))
                )
            } else {
                format!("{} \u{b7} libraries not added, so features run on the processor", gpu.name)
            }
        },
    );
    let address = facts.endpoint.as_ref().map_or_else(
        || "This run published no address.".to_string(),
        |endpoint| format!("127.0.0.1:{} \u{b7} this computer only \u{b7} up {}", endpoint.port, format::uptime_words(endpoint.up_secs)),
    );
    let connection = facts.endpoint.as_ref().map_or_else(|| "none".to_string(), |endpoint| endpoint.path.clone());
    let mut lines = vec![
        Detail { label: "Models".to_string(), value: models },
        Detail { label: "Models folder".to_string(), value: folder },
        Detail { label: "Graphics card".to_string(), value: card },
        Detail { label: "Address".to_string(), value: address },
        Detail { label: "Connection file".to_string(), value: connection },
        Detail { label: "Activity".to_string(), value: format!("Last {} jobs are kept", format::group(activity::MAX_LINES as u64)) },
    ];
    if let Some(error) = facts.error.as_ref().filter(|_| facts.iter_problem_model().is_some()) {
        lines.push(Detail { label: "What happened".to_string(), value: error.clone() });
    }
    lines.push(Detail { label: "Version".to_string(), value: facts.version.clone() });
    lines
}

/// The median of the newest five finished jobs of this task, and the device they ran on.
///
/// The brief asks for the median of the last 5 finished jobs "of that task on that device" and
/// says to omit the promise with fewer than two samples. A running job has no device of its own
/// yet, so the device is the one the samples agree on: the device with the most samples, newest
/// first when the count ties.
fn median_ms(jobs: &[JobFacts], task: &str) -> Option<(u64, String)> {
    let matching: Vec<&JobFacts> = jobs.iter().filter(|job| job.task == task).collect();
    let mut device: Option<&str> = None;
    let mut best: Vec<&JobFacts> = Vec::new();
    for candidate in &matching {
        if device == Some(candidate.device.as_str()) {
            continue;
        }
        let group: Vec<&JobFacts> = matching.iter().copied().filter(|job| job.device == candidate.device).take(5).collect();
        if group.len() > best.len() {
            best = group;
            device = Some(candidate.device.as_str());
        }
    }
    if best.len() < 2 {
        return None;
    }
    let mut values: Vec<u64> = best.iter().map(|job| job.elapsed_ms).collect();
    values.sort_unstable();
    let middle = values.len() / 2;
    let median = if values.len() % 2 == 1 {
        values.get(middle).copied().unwrap_or(0)
    } else {
        let low = values.get(middle.saturating_sub(1)).copied().unwrap_or(0);
        let high = values.get(middle).copied().unwrap_or(0);
        low.saturating_add(high) / 2
    };
    Some((median, device.unwrap_or("cpu").to_string()))
}

impl Facts {
    /// The model with the most serious problem, in the brief's order: damaged, then a note that
    /// changed, then a paused download, then a disk that ran out of room.
    #[must_use]
    fn iter_problem_model(&self) -> Option<(usize, Problem)> {
        for (index, model) in self.models.iter().enumerate() {
            if model.damaged {
                return Some((index, Problem::Damaged));
            }
        }
        for (index, model) in self.models.iter().enumerate() {
            if model.installed && !model.acknowledged {
                return Some((index, Problem::NoteChanged));
            }
        }
        for (index, model) in self.models.iter().enumerate() {
            if !model.out_of_space && model.paused.is_some() && model.downloading.is_none() {
                return Some((index, Problem::Paused));
            }
        }
        self.models.iter().position(|model| model.out_of_space).map(|index| (index, Problem::NoSpace))
    }
}

#[cfg(test)]
mod tests {
    use super::{Facts, ModelFacts, RowState, view};
    use crate::window::{format, state};

    /// A model the way the catalog has it: flagged, with a size, and nothing on disk.
    fn model(id: &str, task: &str, size_bytes: u64) -> ModelFacts {
        ModelFacts {
            id: id.to_string(),
            version: "1".to_string(),
            task: task.to_string(),
            size_bytes,
            note: Some("Use it for personal work only.".to_string()),
            installed: false,
            checked_unix: None,
            acknowledged: false,
            downloading: None,
            paused: None,
            damaged: false,
            out_of_space: false,
        }
    }

    fn lama() -> ModelFacts {
        model("lama", "fill_masked_area", 208_044_816)
    }

    fn birefnet() -> ModelFacts {
        model("birefnet-lite", "subject_mask", 224_005_088)
    }

    fn both_installed() -> Facts {
        let mut facts = Facts { version: "0.1.0".to_string(), models: vec![lama(), birefnet()], ..Facts::default() };
        for model in &mut facts.models {
            model.installed = true;
            model.acknowledged = true;
            model.checked_unix = Some(1_760_320_800);
        }
        facts
    }

    /// Section 4.1, row 1: a machine with nothing installed.
    #[test]
    fn nothing_installed_says_almost_ready_and_lists_every_model_with_an_add_button() {
        let facts = Facts { version: "0.1.0".to_string(), models: vec![lama(), birefnet()], ..Facts::default() };
        let view = view(&facts);
        assert_eq!(view.bold, "Almost ready.");
        assert_eq!(view.grey, "Add the AI features you want. Each downloads once, then works offline.");
        assert_eq!(view.meta, "Nothing is installed yet.");
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[0].title, "Generative Fill");
        assert_eq!(view.rows[0].purpose, "Removes what you paint over and fills the gap.");
        assert_eq!(view.rows[0].meta, "LaMa v1 \u{b7} 198 MB \u{b7} non-commercial use only");
        assert_eq!(view.rows[0].state.words(), "Add \u{b7} 198 MB");
        assert_eq!(view.rows[1].title, "Select Subject \u{b7} Remove Background");
        assert_eq!(view.rows[1].state.words(), "Add \u{b7} 214 MB");
        assert!(view.action.is_none());
    }

    /// Section 4.1, row 2: a download is running, and the sentence under the status changes.
    #[test]
    fn a_download_in_flight_says_you_can_keep_working_and_shows_the_percentage() {
        let mut facts = Facts { version: "0.1.0".to_string(), models: vec![lama(), birefnet()], ..Facts::default() };
        facts.models[0].downloading = Some((93 * 1024 * 1024, 198 * 1024 * 1024));
        let view = view(&facts);
        assert_eq!(view.bold, "Almost ready.");
        assert_eq!(view.grey, "Add the AI features you want. Each downloads once, then works offline.");
        assert_eq!(view.meta, "Downloading. You can keep working in PhotoCraft.");
        assert_eq!(view.rows[0].state, RowState::Downloading { percent: 47 });
        assert_eq!(view.rows[0].state.words(), "47%");
    }

    /// Section 4.1, row 3: one model is in and nothing has run.
    #[test]
    fn one_installed_model_with_no_job_says_ready_and_counts_features_not_models() {
        let mut facts = Facts { version: "0.1.0".to_string(), models: vec![lama(), birefnet()], ..Facts::default() };
        facts.models[0].installed = true;
        facts.models[0].acknowledged = true;
        let view = view(&facts);
        assert_eq!(view.bold, "Ready.");
        assert_eq!(view.grey, "PhotoCraft can use 1 AI feature.");
        assert_eq!(view.meta, "Nothing has run yet.");
        assert_eq!(view.rows[0].state, RowState::Ready);
        assert_eq!(view.rows[1].state.words(), "Add \u{b7} 214 MB");
    }

    /// Section 4.2, row 1: idle, with a finished job in the log.
    #[test]
    fn an_idle_machine_with_a_job_in_the_log_reads_the_newest_line() {
        let mut facts = both_installed();
        facts.jobs.push(state::JobFacts { task: "subject_mask".to_string(), device: "cpu".to_string(), fallback: None, elapsed_ms: 300, ago_secs: 240 });
        let view = view(&facts);
        assert_eq!(view.bold, "Ready.");
        assert_eq!(view.grey, "PhotoCraft can use 3 AI features.");
        assert_eq!(view.meta, "Last: Select Subject, 4 min ago \u{b7} 0.3 s on the processor");
        assert!(view.action.is_none());
    }

    /// Section 4.2: a fallback is said plainly, with no accent and no action.
    #[test]
    fn a_fallback_appends_because_and_stays_plain() {
        let mut facts = both_installed();
        facts.jobs.push(state::JobFacts {
            task: "fill_masked_area".to_string(),
            device: "cpu".to_string(),
            fallback: Some("insufficient_memory".to_string()),
            elapsed_ms: 12_400,
            ago_secs: 11_000,
        });
        let view = view(&facts);
        assert_eq!(view.meta, "Last: Generative Fill, 3 h ago \u{b7} 12 s on the processor, because the graphics card didn't have enough free memory");
        assert!(view.action.is_none());
        assert!(!view.rows[0].state.needs_you());
    }

    /// Section 4.2, row 2: a job is running.
    #[test]
    fn a_running_job_says_what_it_is_doing_and_promises_the_median() {
        let mut facts = both_installed();
        facts.jobs.push(state::JobFacts { task: "subject_mask".to_string(), device: "cuda".to_string(), fallback: None, elapsed_ms: 4_500, ago_secs: 60 });
        facts.jobs.push(state::JobFacts { task: "subject_mask".to_string(), device: "cuda".to_string(), fallback: None, elapsed_ms: 5_500, ago_secs: 600 });
        facts.running = Some(state::RunningFacts { task: "subject_mask".to_string(), started_secs: 2, progress_percent: 40 });
        let view = view(&facts);
        assert_eq!(view.bold, "Working.");
        assert_eq!(view.grey, "Selecting the subject for PhotoCraft.");
        assert_eq!(view.meta, "Started 2 s ago \u{b7} usually about 5 s on the graphics card");
        assert_eq!(view.rows[1].state, RowState::Working { percent: 40 });
        assert_eq!(view.rows[1].state.words(), "Working");
    }

    /// Section 4.2: the promise is omitted with fewer than two samples.
    #[test]
    fn one_sample_is_not_enough_to_promise_a_typical_time() {
        let mut facts = both_installed();
        facts.jobs.push(state::JobFacts { task: "subject_mask".to_string(), device: "cuda".to_string(), fallback: None, elapsed_ms: 4_500, ago_secs: 60 });
        facts.running = Some(state::RunningFacts { task: "subject_mask".to_string(), started_secs: 0, progress_percent: 0 });
        let view = view(&facts);
        assert_eq!(view.meta, "Started just now");
    }

    /// Section 4.3: the offer appears only with a card and without the libraries.
    #[test]
    fn the_graphics_card_offer_needs_a_card_that_has_no_libraries_yet() {
        let mut facts = both_installed();
        facts.gpu = Some(state::GpuFacts {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            free_mib: 22_118,
            total_mib: 24_576,
            ready: false,
            libraries_bytes: 1_915_544_656,
        });
        let view = view(&facts);
        let offer = view.offer.expect("the card is here, so the offer is too");
        assert_eq!(offer.title, "Use your graphics card");
        assert_eq!(offer.text, "NVIDIA GeForce RTX 4090 found. AI features run many times faster on it.");
        assert_eq!(offer.meta, "NVIDIA CUDA and cuDNN libraries \u{b7} 1.8 GB \u{b7} NVIDIA's licences");
        assert_eq!(offer.button, "Add \u{b7} 1.8 GB");
        assert_eq!(offer.accept_words, "I accept NVIDIA's licence terms.");
        assert_eq!(view.footer, "This computer only \u{b7} Closing this window turns AI features off \u{b7} Details");
        assert_eq!(view.device_line, "AI device: Processor (CPU)");
        facts.gpu = Some(state::GpuFacts {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            free_mib: 22_118,
            total_mib: 24_576,
            ready: true,
            libraries_bytes: 1_915_544_656,
        });
        assert!(state::view(&facts).offer.is_none());
        assert_eq!(state::view(&facts).footer, "This computer only \u{b7} Closing this window turns AI features off \u{b7} Details");
        assert_eq!(state::view(&facts).device_line, "AI device: Graphics card (NVIDIA GeForce RTX 4090)");
        facts.gpu = None;
        assert!(state::view(&facts).offer.is_none());
    }

    /// Section 4.4, row 1: the bytes on disk are not the bytes the catalog pins.
    #[test]
    fn a_damaged_model_makes_the_status_the_problem_and_offers_a_repair() {
        let mut facts = both_installed();
        facts.models[1].damaged = true;
        facts.error = Some("the file is 224,005,087 bytes, not 224,005,088".to_string());
        let view = view(&facts);
        assert_eq!(view.bold, "Select Subject \u{b7} Remove Background can't run.");
        assert_eq!(view.grey, "Their model file is damaged.");
        let action = view.action.expect("the problem needs a button");
        assert_eq!(action.button, "Repair \u{b7} 214 MB");
        assert_eq!(action.model.as_deref(), Some("birefnet-lite"));
        assert!(action.button_enabled);
        assert_eq!(action.explanation, "Downloads BiRefNet lite again and checks it before use.");
        assert_eq!(view.rows[1].state, RowState::NeedsRepair);
        assert_eq!(view.rows[1].state.words(), "\u{25cf} Needs repair");
        assert_eq!(view.rows[0].state, RowState::Ready);
        assert!(view.copy_details);
        assert!(view.details.iter().any(|line| line.label == "What happened"));
    }

    /// Section 4.4, row 2: a part is on disk with nothing fetching the rest.
    #[test]
    fn a_paused_download_says_how_far_it_got_and_offers_to_resume() {
        let mut facts = Facts { version: "0.1.0".to_string(), models: vec![lama(), birefnet()], ..Facts::default() };
        facts.models[0].paused = Some((120 * 1024 * 1024, 198 * 1024 * 1024));
        let view = view(&facts);
        assert_eq!(view.bold, "Download paused.");
        assert_eq!(view.grey, "Generative Fill is 120 of 198 MB in.");
        let action = view.action.expect("the pause needs a button");
        assert_eq!(action.button, "Resume");
        assert_eq!(action.model.as_deref(), Some("lama"));
        assert_eq!(action.explanation, "Checks the part already here, then fetches only the rest.");
        assert_eq!(view.rows[0].state.words(), "\u{25cf} Paused \u{b7} 61%");
    }

    /// Section 4.4, row 3: the note changed, so the model is paused until it is read again.
    #[test]
    fn a_changed_note_pauses_the_feature_and_disables_the_button_until_the_box_is_ticked() {
        let mut facts = both_installed();
        facts.models[0].acknowledged = false;
        let view = view(&facts);
        assert_eq!(view.bold, "Generative Fill is paused.");
        assert_eq!(view.grey, "Its licence note changed. Read it to keep using it.");
        let action = view.action.expect("the note needs an OK");
        assert_eq!(action.button, "Continue with Generative Fill");
        assert_eq!(action.model.as_deref(), Some("lama"));
        assert!(!action.button_enabled);
        assert_eq!(action.explanation, "Nothing is downloaded again.");
        assert_eq!(action.note, facts.models[0].note);
        assert_eq!(action.heading.as_deref(), Some("Non-commercial use only"));
        assert_eq!(view.rows[0].state, RowState::NeedsYourOk);
        assert_eq!(view.rows[0].state.words(), "\u{25cf} Needs your OK");
    }

    /// Section 4.4, row 4: a write failed because the disk was full.
    #[test]
    fn a_full_disk_stops_the_download_and_asks_for_room() {
        let mut facts = Facts { version: "0.1.0".to_string(), models: vec![lama(), birefnet()], ..Facts::default() };
        facts.models[0].paused = Some((120 * 1024 * 1024, 198 * 1024 * 1024));
        facts.models[0].out_of_space = true;
        let view = view(&facts);
        assert_eq!(view.bold, "Download stopped.");
        assert_eq!(view.grey, "There isn't room for the rest of Generative Fill.");
        let action = view.action.expect("the stopped download needs a button");
        assert_eq!(action.button, "Try again");
        assert_eq!(action.model.as_deref(), Some("lama"));
        assert_eq!(action.explanation, "Free up space on C: first. The part already downloaded is kept.");
        assert_eq!(view.rows[0].state.words(), "\u{25cf} Paused \u{b7} 61%");
    }

    /// Section 4.4: one problem at a time, the most serious first.
    #[test]
    fn the_most_serious_problem_is_the_one_the_window_shows() {
        let mut facts = both_installed();
        facts.models[1].damaged = true;
        facts.models[0].paused = Some((120 * 1024 * 1024, 198 * 1024 * 1024));
        facts.models[0].acknowledged = false;
        facts.models[1].out_of_space = true;
        let view = view(&facts);
        assert_eq!(view.bold, "Select Subject \u{b7} Remove Background can't run.");
        facts.models[1].damaged = false;
        assert_eq!(state::view(&facts).bold, "Generative Fill is paused.");
        facts.models[0].acknowledged = true;
        assert_eq!(state::view(&facts).bold, "Download paused.");
        facts.models[0].paused = None;
        facts.models[1].out_of_space = true;
        assert_eq!(state::view(&facts).bold, "Download stopped.");
    }

    /// Section 4.5: every line of the Details table, with live values.
    #[test]
    fn details_answer_every_line_of_the_briefs_table() {
        let mut facts = both_installed();
        facts.gpu = Some(state::GpuFacts {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            free_mib: 22_118,
            total_mib: 24_576,
            ready: true,
            libraries_bytes: 1_915_544_656,
        });
        facts.endpoint =
            Some(state::EndpointFacts { port: 41_234, path: "C:\\Users\\Erik\\AppData\\Local\\CraftAI\\endpoint.json".to_string(), up_secs: 11_520 });
        facts.folder = Some(state::FolderFacts { path: "C:\\Users\\Erik\\AppData\\Local\\CraftAI\\models".to_string(), bytes: Some(432_049_904) });
        let view = view(&facts);
        let read = |label: &str| view.details.iter().find(|line| line.label == label).expect("the line is there").value.clone();
        assert_eq!(read("Models"), "LaMa v1 \u{b7} 198 MB \u{b7} checked Oct 13\nBiRefNet lite v1 \u{b7} 214 MB \u{b7} checked Oct 13");
        assert!(read("Models folder").starts_with("C:\\Users"));
        assert!(read("Models folder").ends_with("412 MB"));
        assert_eq!(read("Graphics card"), "NVIDIA GeForce RTX 4090 \u{b7} CUDA ready \u{b7} 21.6 of 24 GB free");
        assert_eq!(read("Address"), "127.0.0.1:41234 \u{b7} this computer only \u{b7} up 3 h 12 min");
        assert!(read("Connection file").ends_with("endpoint.json"));
        assert_eq!(read("Activity"), "Last 1,000 jobs are kept");
        assert_eq!(read("Version"), "0.1.0");
        assert!(!view.copy_details);
        assert!(!view.details.iter().any(|line| line.label == "What happened"));
    }

    /// Section 4.5: the two sentences for a machine with no card, and one with a card but no
    /// libraries.
    #[test]
    fn the_graphics_card_line_says_what_this_machine_has() {
        let mut facts = both_installed();
        assert_eq!(details_value(&facts, "Graphics card"), "No NVIDIA card found \u{b7} features run on the processor");
        facts.gpu = Some(state::GpuFacts {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            free_mib: 22_118,
            total_mib: 24_576,
            ready: false,
            libraries_bytes: 1_915_544_656,
        });
        assert_eq!(details_value(&facts, "Graphics card"), "NVIDIA GeForce RTX 4090 \u{b7} libraries not added, so features run on the processor");
    }

    /// Erik's acceptance review (Oct 20, 2026) found the CPU/GPU question unclear: the footer carried
    /// a bare hardware name, with no label and no sentence. The status block now says it in words,
    /// and the footer no longer says it twice.
    #[test]
    fn the_device_line_names_the_processor_when_the_machine_has_no_card() {
        let view = view(&both_installed());
        assert_eq!(view.device_line, "AI device: Processor (CPU)");
        assert_eq!(view.device, "Processor");
        assert_eq!(view.footer, "This computer only \u{b7} Closing this window turns AI features off \u{b7} Details");
    }

    /// A card is not a device this manager can run on until its NVIDIA libraries are here, which is
    /// what section 4.3's offer is for, so the line still says the processor.
    #[test]
    fn the_device_line_waits_for_the_cards_libraries_before_naming_it() {
        let mut facts = both_installed();
        facts.gpu = Some(state::GpuFacts {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            free_mib: 22_118,
            total_mib: 24_576,
            ready: false,
            libraries_bytes: 1_915_544_656,
        });
        assert_eq!(view(&facts).device_line, "AI device: Processor (CPU)");
        facts.gpu = Some(state::GpuFacts {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            free_mib: 22_118,
            total_mib: 24_576,
            ready: true,
            libraries_bytes: 1_915_544_656,
        });
        assert_eq!(view(&facts).device_line, "AI device: Graphics card (NVIDIA GeForce RTX 4090)");
        assert_eq!(view(&facts).device, "NVIDIA GeForce RTX 4090");
    }

    /// A run started with `--device cpu` uses the processor whatever the machine has, and the
    /// line has to say that rather than name the card sitting in the box.
    #[test]
    fn a_run_asked_for_the_processor_says_so_whatever_the_machine_has() {
        let mut facts = both_installed();
        facts.gpu = Some(state::GpuFacts {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            free_mib: 22_118,
            total_mib: 24_576,
            ready: true,
            libraries_bytes: 1_915_544_656,
        });
        facts.device_setting = crate::device::Setting::Cpu.token().to_string();
        assert_eq!(view(&facts).device_line, "AI device: Processor (CPU)");
        assert_eq!(view(&facts).device, "Processor");
        facts.device_setting = crate::device::Setting::Auto.token().to_string();
        assert_eq!(state::view(&facts).device_line, "AI device: Graphics card (NVIDIA GeForce RTX 4090)");
    }

    /// The per-job sentence of section 4.2 stays as it was: one job that fell back to the processor
    /// does not mean the run is using the processor, so both statements are true at once.
    #[test]
    fn a_fallen_back_job_says_the_processor_while_the_run_still_names_the_card() {
        let mut facts = both_installed();
        facts.gpu = Some(state::GpuFacts {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            free_mib: 22_118,
            total_mib: 24_576,
            ready: true,
            libraries_bytes: 1_915_544_656,
        });
        facts.jobs.push(state::JobFacts {
            task: "fill_masked_area".to_string(),
            device: "cpu".to_string(),
            fallback: Some("insufficient_memory".to_string()),
            elapsed_ms: 12_400,
            ago_secs: 11_000,
        });
        let view = view(&facts);
        assert_eq!(view.meta, "Last: Generative Fill, 3 h ago \u{b7} 12 s on the processor, because the graphics card didn't have enough free memory");
        assert_eq!(view.device_line, "AI device: Graphics card (NVIDIA GeForce RTX 4090)");
    }

    /// Erik's acceptance review (Oct 20, 2026): an installed row ended at `Ready` and nothing
    /// else, so its two operations read as absent. They are on the row now, in the words Details uses.
    #[test]
    fn an_installed_row_offers_to_check_again_and_remove_without_opening_details() {
        let view = view(&both_installed());
        for row in &view.rows {
            assert_eq!(row.manage, vec![format::CHECK_AGAIN_WORDS.to_string(), format::REMOVE_WORDS.to_string()]);
        }
    }

    /// A model that is not installed has nothing to check or remove, and a download in flight is not
    /// a model yet, so those rows offer nothing of their own.
    #[test]
    fn a_row_that_is_not_installed_offers_no_management_of_its_own() {
        let fresh = Facts { version: "0.1.0".to_string(), models: vec![lama(), birefnet()], ..Facts::default() };
        assert!(view(&fresh).rows.iter().all(|row| row.manage.is_empty()));
        let mut facts = both_installed();
        facts.models[0].downloading = Some((93 * 1024 * 1024, 198 * 1024 * 1024));
        assert_eq!(view(&facts).rows[0].state.words(), "47%");
        assert!(view(&facts).rows[0].manage.is_empty());
        facts.models[0].downloading = None;
        facts.models[0].paused = Some((120 * 1024 * 1024, 198 * 1024 * 1024));
        assert_eq!(state::view(&facts).rows[0].state.words(), "\u{25cf} Paused \u{b7} 61%");
        assert!(state::view(&facts).rows[0].manage.is_empty());
    }

    /// The states that need the user most are the ones that keep their management: a damaged or
    /// unacknowledged model is exactly when `Check again` is the way out.
    #[test]
    fn a_working_or_needs_you_row_keeps_its_management() {
        let mut facts = both_installed();
        facts.running = Some(state::RunningFacts { task: "subject_mask".to_string(), started_secs: 2, progress_percent: 40 });
        let view = view(&facts);
        assert_eq!(view.rows[1].state, RowState::Working { percent: 40 });
        assert_eq!(view.rows[1].manage, vec![format::CHECK_AGAIN_WORDS.to_string(), format::REMOVE_WORDS.to_string()]);
        let mut facts = both_installed();
        facts.models[1].damaged = true;
        assert_eq!(state::view(&facts).rows[1].state, RowState::NeedsRepair);
        assert!(!state::view(&facts).rows[1].manage.is_empty());
        let mut facts = both_installed();
        facts.models[0].acknowledged = false;
        assert_eq!(state::view(&facts).rows[0].state, RowState::NeedsYourOk);
        assert!(!state::view(&facts).rows[0].manage.is_empty());
    }

    fn details_value(facts: &Facts, label: &str) -> String {
        view(facts).details.iter().find(|line| line.label == label).expect("the line is there").value.clone()
    }

    /// The brief's own size examples, which section 5.8 asks for as a test of its own.
    #[test]
    fn the_three_sizes_in_section_3_are_the_ones_the_window_shows() {
        assert_eq!(format::size(208_044_816), "198 MB");
        assert_eq!(format::size(224_005_088), "214 MB");
        assert_eq!(format::size(1_915_544_656), "1.8 GB");
    }
}
