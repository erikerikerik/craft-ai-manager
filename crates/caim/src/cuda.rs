//! CUDA in one place (MG-11, SPEC amendment 18): the vendor's own libraries, the card probe, the
//! free-memory measurement, the device name, and the ONNX Runtime provider registration.
//!
//! Amendment 18 wants exactly this: everything that knows what an NVIDIA card *is* lives here, so
//! the resolver in [crate::device] and the two runners only ever see an accelerator that is ready or
//! not ready, a name, and a number of free MiB. Nothing here chooses a device - [accelerators]
//! describes the machine, and the resolver decides.
//!
//! The libraries are an optional, checksum-verified download of about 1.9 GB, fetched from NVIDIA's
//! own redistribution server only when someone runs `caim cuda install`. They are **not** shipped in
//! CAIM's binary, and a CPU-only machine never asks for them: the CUDA execution provider is
//! registered only for a session the resolver chose CUDA for, and its dependencies are preloaded
//! from CAIM's own folder with ONNX Runtime's own `preload_dylibs`, so `PATH` stays untouched.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::catalog::{Model, OverrideNote};
use crate::device;
use crate::error::{Fail, Result};
use crate::fetch;
use crate::store;

/// The folder the CUDA libraries live in, a sibling of the models folder.
pub const CUDA_DIR: &str = "cuda";
/// The environment variable that moves that folder, next to `CRAFT_AI_MODELS`.
pub const CUDA_ENV: &str = "CRAFT_AI_CUDA";
/// The pseudo-task the four library entries belong to. It is not a job task, so [crate::routing]
/// never offers it and a job can never ask for it.
pub const TASK_LIBRARIES: &str = "cuda_libraries";
/// The vendor's own tool, which is the only way to ask a Windows machine what GPU it has and how
/// much of the card is free right now.
pub const PROBE: &str = "nvidia-smi";
/// What to ask it: name, free memory, total memory, one line per card, no units to parse.
pub const PROBE_ARGS: [&str; 2] = ["--query-gpu=name,memory.free,memory.total", "--format=csv,noheader,nounits"];
/// How long the probe may take. `nvidia-smi` answers in tens of milliseconds; a card that has fallen
/// off the bus hangs, and a job must not hang with it.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often to check on the probe while it runs.
pub const PROBE_POLL: Duration = Duration::from_millis(50);
/// The word that marks one graph node as placed on the CUDA provider in ONNX Runtime's own profile.
pub const CUDA_NODE_MARK: &str = "CUDA Execution Provider";

/// The libraries each archive gives CAIM, in the order ONNX Runtime wants them loaded (a dependency
/// before its dependent). These are the names ONNX Runtime itself looks for, which the
/// `the_extraction_covers_what_onnx_runtime_needs` test pins against the runtime's own lists.
const WANTED: [(&str, &[&str]); 4] = [
    ("cuda_cudart", &["cudart64_13.dll"]),
    ("libcublas", &["cublasLt64_13.dll", "cublas64_13.dll", "nvblas64_13.dll"]),
    ("libcufft", &["cufft64_12.dll", "cufftw64_12.dll"]),
    (
        "cudnn",
        &[
            "cudnn64_9.dll",
            "cudnn_graph64_9.dll",
            "cudnn_ops64_9.dll",
            "cudnn_heuristic64_9.dll",
            "cudnn_adv64_9.dll",
            "cudnn_cnn64_9.dll",
            "cudnn_engines_precompiled64_9.dll",
            "cudnn_engines_runtime_compiled64_9.dll",
            "cudnn_engines_tensor_ir64_9.dll",
            "cudnn_ext64_9.dll",
        ],
    ),
];

/// The sentence a user has to accept before CAIM fetches a CUDA library.
const CUDA_NOTE: &str = "Craft AI Manager downloads these files from NVIDIA's own redistribution server; it does not ship them. \
    They are covered by the NVIDIA CUDA End User License Agreement (https://docs.nvidia.com/cuda/eula/index.html), which allows \
    redistribution of the runtime libraries but is not an open-source licence. Nothing is fetched until you run this command with \
    --accept-note, and a CPU-only machine never needs them.";
/// The same for cuDNN, which has its own agreement.
const CUDNN_NOTE: &str = "Craft AI Manager downloads cuDNN from NVIDIA's own redistribution server; it does not ship it. cuDNN is \
    covered by the NVIDIA cuDNN Software License Agreement (https://developer.nvidia.com/cudnn), which allows redistribution with \
    an NVIDIA developer account and is not an open-source licence. Nothing is fetched until you run this command with --accept-note.";

/// The licence a component ships under, and the sentence a user has to accept before it is fetched.
/// They travel together: the code is what the catalog records, the note is what the command shows.
struct Agreement {
    code: &'static str,
    note: &'static str,
}

/// NVIDIA's two agreements: the CUDA Toolkit EULA covers the three runtime archives, and cuDNN has
/// an agreement of its own.
const CUDA_AGREEMENT: Agreement = Agreement { code: "CUDA Toolkit", note: CUDA_NOTE };
const CUDNN_AGREEMENT: Agreement = Agreement { code: "cuDNN SLA", note: CUDNN_NOTE };

/// One pinned archive: what to fetch, what it must hash to, and what to keep from it.
fn pinned(id: &str, version: &str, file: &str, url: &str, size: u64, sha256: &str, agreement: &Agreement) -> Model {
    Model {
        id: id.to_string(),
        version: version.to_string(),
        task: TASK_LIBRARIES.to_string(),
        file: file.to_string(),
        url: url.to_string(),
        revision: version.to_string(),
        sha256: sha256.to_string(),
        size,
        // A library archive is not a model: no pixel cap and no working memory of its own.
        max_pixels: 0,
        memory_mib: 0,
        backends: BTreeMap::new(),
        licence_code: agreement.code.to_string(),
        licence_weights: agreement.code.to_string(),
        licence_training_data: "not applicable".to_string(),
        // The archive URL *is* the provenance: it is the exact bytes, pinned to a version, from the
        // vendor. NVIDIA's redist index is not a stable URL, so it is not cited here.
        provenance: url.to_string(),
        note: Some(OverrideNote {
            by: "MG-11".to_string(),
            date: "2026-10-18".to_string(),
            reason: "the vendor's licence has to be accepted before the files are fetched".to_string(),
            note: agreement.note.to_string(),
        }),
    }
}

/// The four archives, pinned to exact versions and hashes (step 0 of the task, written into the
/// receipt before any code). These are the versions ONNX Runtime 1.24 / ort 2.0.0-rc.14 expects:
/// the CUDA 13 runtime, cuBLAS 13, cuFFT 12, and cuDNN 9.
///
/// Every URL was fetched and hashed on the machine this was written on; the sizes and SHA-256s are
/// what those downloads produced.
#[must_use]
pub fn components() -> Vec<Model> {
    vec![
        pinned(
            "cuda_cudart",
            "13.4.92",
            "cuda_cudart-windows-x86_64-13.4.92-archive.zip",
            "https://developer.download.nvidia.com/compute/cuda/redist/cuda_cudart/windows-x86_64/cuda_cudart-windows-x86_64-13.4.92-archive.zip",
            2_737_397,
            "621a70c4778287b1abf8c27c61b841d797fda2aeff01033374dd19624bcfa249",
            &CUDA_AGREEMENT,
        ),
        pinned(
            "libcublas",
            "13.8.0.4",
            "libcublas-windows-x86_64-13.8.0.4-archive.zip",
            "https://developer.download.nvidia.com/compute/cuda/redist/libcublas/windows-x86_64/libcublas-windows-x86_64-13.8.0.4-archive.zip",
            422_415_931,
            "0974318e9861a61cb9091cf7de8d4c2880e9787fe311cb4bcbd08865663c5f43",
            &CUDA_AGREEMENT,
        ),
        pinned(
            "libcufft",
            "12.4.0.43",
            "libcufft-windows-x86_64-12.4.0.43-archive.zip",
            "https://developer.download.nvidia.com/compute/cuda/redist/libcufft/windows-x86_64/libcufft-windows-x86_64-12.4.0.43-archive.zip",
            159_602_693,
            "69d0ad8dc3a1be66f01a748a8206d0ceaafa56939474663fbe790dc3e91d2009",
            &CUDA_AGREEMENT,
        ),
        pinned(
            "cudnn",
            "9.27.0.42",
            "cudnn-windows-x86_64-9.27.0.42_cuda13-archive.zip",
            "https://developer.download.nvidia.com/compute/cudnn/redist/cudnn/windows-x86_64/cudnn-windows-x86_64-9.27.0.42_cuda13-archive.zip",
            1_330_803_635,
            "15595c7b52896262a0e590f02b3b3851c6a2c9046a72047526deaf59a9cbf7d5",
            &CUDNN_AGREEMENT,
        ),
    ]
}

/// The library names CAIM keeps from one archive. Empty for an id this table does not know, which
/// makes [wanted] total rather than a panic.
#[must_use]
fn names(model: &Model) -> &'static [&'static str] {
    WANTED.iter().find(|(id, _)| *id == model.id).map(|(_, names)| *names).unwrap_or(&[])
}

/// Where one archive's files sit inside it, and what CAIM calls them: `<stem>/bin/x64/<dll>`.
#[must_use]
pub fn wanted(model: &Model) -> Vec<(String, String)> {
    let stem = model.file.strip_suffix(".zip").map(str::to_string).unwrap_or_else(|| model.file.clone());
    names(model).iter().map(|name| (format!("{stem}/bin/x64/{name}"), (*name).to_string())).collect()
}

/// The CUDA folder: `CRAFT_AI_CUDA` when it names one, otherwise a `cuda` folder beside the models.
#[must_use]
pub fn store_root() -> Option<PathBuf> {
    store_root_in(std::env::var_os(CUDA_ENV), store::models_dir())
}

/// The same decision with both inputs handed over, so a test can point CAIM at a scratch folder
/// without touching the environment of a running process.
#[must_use]
pub fn store_root_in(named: Option<std::ffi::OsString>, models_root: Option<PathBuf>) -> Option<PathBuf> {
    match named {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => models_root.and_then(|root| root.parent().map(|parent| parent.join(CUDA_DIR))),
    }
}

/// The folder the DLLs go in. ONNX Runtime's `preload_dylibs` is handed this one folder for both
/// the CUDA and the cuDNN half, so everything it looks for is in one place.
#[must_use]
pub fn library_dir(root: &Path) -> PathBuf {
    root.join("bin")
}

/// Are all nineteen libraries on disk? This is a cheap existence check, not a hash check: the archives
/// were hashed when they arrived, and the extraction verified each file's own CRC.
#[must_use]
pub fn is_ready(root: &Path) -> bool {
    let dir = library_dir(root);
    components().iter().all(|model| wanted(model).iter().all(|(_, name)| dir.join(name).is_file()))
}

/// Fetch the four archives and unpack the libraries CAIM needs, into [library_dir].
///
/// This is [crate::fetch::install] with the same gate a flagged model has to pass: nothing is
/// fetched until the vendor's note has been accepted, each archive is hashed while it arrives, and
/// each library is unpacked only after its own CRC and size check. Re-running it costs nothing - an
/// archive that is already verified is not fetched again, and a library that is already on disk with
/// the right size is not written again.
pub fn install(root: &Path, accept_note: bool, fetcher: &dyn fetch::Fetch, progress: &mut dyn FnMut(u64, u64)) -> Result<String> {
    let dir = library_dir(root);
    std::fs::create_dir_all(&dir).map_err(|error| Fail::internal(format!("Craft AI Manager could not create {}: {error}", dir.display())))?;
    let mut downloaded = 0usize;
    let mut libraries = 0usize;
    for model in components() {
        if accept_note && model.note.is_some() {
            store::write_acknowledgement(root, &model)?;
        }
        if let fetch::Outcome::Downloaded { .. } = fetch::install(&model, root, fetcher, progress)? {
            downloaded += 1;
        }
        libraries += extract(&store::model_file(root, &model), &dir, &wanted(&model))?.len();
    }
    let bytes: u64 = components().iter().map(|model| model.size).sum();
    Ok(format!(
        "CUDA libraries are ready in {}: {libraries} files from {} archives ({bytes} bytes), {downloaded} downloaded now.",
        dir.display(),
        components().len()
    ))
}

/// `caim cuda install [--accept-note]` with the real folder and the real HTTPS fetcher, which is
/// all [install] still needs.
pub fn install_all(accept_note: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<String> {
    let Some(root) = store_root() else {
        return Err(Fail::internal("Craft AI Manager cannot find a folder for the CUDA libraries on this machine."));
    };
    install(&root, accept_note, &fetch::HttpFetch::new(), progress)
}

/// `caim cuda status`: one line per archive, one per card the probe found, and whether a CUDA job
/// could actually run here.
pub fn status_lines() -> Result<Vec<String>> {
    let Some(root) = store_root() else {
        return Err(Fail::internal("Craft AI Manager cannot find a folder for the CUDA libraries on this machine."));
    };
    let dir = library_dir(&root);
    let mut lines = vec![format!("cuda libraries: {}", dir.display())];
    for model in components() {
        let files = wanted(&model);
        let present = files.iter().filter(|(_, name)| dir.join(name).is_file()).count();
        let state = if store::is_installed(&root, &model) { "downloaded" } else { "not downloaded" };
        lines.push(format!("  {} {}: {state}, {} bytes, {present}/{} libraries on disk", model.id, model.version, model.size, files.len()));
    }
    let found = cards();
    if found.is_empty() {
        lines.push(format!("  device: none found ({} did not answer)", PROBE));
    }
    for card in found {
        lines.push(format!("  device: {} ({} MiB free of {} MiB)", card.name, card.free_mib, card.total_mib));
    }
    lines.push(if is_ready(&root) { "  ready: a CUDA job can run here".to_string() } else { "  not ready: run caim cuda install --accept-note".to_string() });
    Ok(lines)
}

/// One card the probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    /// The vendor's own words, which is what amendment 18 wants in `device_name`.
    pub name: String,
    /// Free memory *now*, in MiB - never the card's total.
    pub free_mib: u64,
    /// The whole card, for the status line only.
    pub total_mib: u64,
}

/// Every card on the machine, in the order the probe listed them.
///
/// The probe is a short-lived child process with a deadline: `nvidia-smi` answers in tens of
/// milliseconds, and a card that has fallen off the bus hangs, which must not hang a job. A missing
/// tool, a refused spawn, a timeout, or nonsense output all answer the same way - an empty list.
#[must_use]
pub fn cards() -> Vec<Card> {
    let mut child = match Command::new(PROBE).args(PROBE_ARGS).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(_) => return Vec::new(),
    };
    // The pipe is drained only after the child is done. Its answer is a few hundred bytes, so the
    // pipe cannot fill and deadlock; the deadline is what protects a job from a hung driver.
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        return Vec::new();
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Vec::new();
                }
                std::thread::sleep(PROBE_POLL);
            }
            Err(_) => {
                let _ = child.kill();
                return Vec::new();
            }
        }
    }
    let mut text = String::new();
    if stdout.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    parse_cards(&text)
}

/// Read the probe's own CSV: `name, free, total` per line, no header and no units. The two numbers
/// are read from the end of the line so a card name that contains a comma survives.
#[must_use]
fn parse_cards(text: &str) -> Vec<Card> {
    let mut found = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 3 {
            continue;
        }
        let (Some(free), Some(total)) =
            (fields.get(fields.len() - 2).and_then(|field| field.parse::<u64>().ok()), fields.last().and_then(|field| field.parse::<u64>().ok()))
        else {
            continue;
        };
        let Some(name) = fields.get(..fields.len() - 2).map(|rest| rest.join(", ")) else { continue };
        if name.is_empty() {
            continue;
        }
        found.push(Card { name, free_mib: free, total_mib: total });
    }
    found
}

/// What the machine has, in the words [crate::device] asks for. The resolver calls this once per job
/// that could want CUDA; it is the only place the vendor's tool runs for a job.
#[must_use]
pub fn accelerators() -> Vec<device::Accelerator> {
    let Some(card) = cards().into_iter().max_by_key(|card| card.free_mib) else {
        return Vec::new();
    };
    let ready = store_root().is_some_and(|root| is_ready(&root));
    vec![device::Accelerator {
        device: device::Device::Cuda,
        name: card.name,
        free_mib: Some(card.free_mib),
        ready,
        note: (!ready).then(|| "the CUDA libraries are not installed: run caim cuda install --accept-note".to_string()),
    }]
}

/// One preload per folder per process. ONNX Runtime's own loader leaks the libraries by design -
/// they have to stay mapped for as long as any session may use them - so this is done once and the
/// answer is remembered, including a failure, which is what keeps a broken install from being
/// retried on every job.
static PRELOADED: OnceLock<Mutex<BTreeMap<PathBuf, std::result::Result<(), String>>>> = OnceLock::new();

/// Point ONNX Runtime's CUDA provider at CAIM's own library folder.
///
/// This is the "load the provider and its libraries at run time" half of step 0: CAIM's binary stays
/// small, `PATH` is never touched, and a machine that has no CUDA folder never loads anything.
pub fn preload(root: &Path) -> Result<()> {
    let table = PRELOADED.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut table = table.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(remembered) = table.get(root) {
        return match remembered {
            Ok(()) => Ok(()),
            Err(sentence) => Err(Fail::internal(sentence.clone())),
        };
    }
    let dir = library_dir(root);
    let outcome = ort::ep::cuda::preload_dylibs(Some(&dir), Some(&dir))
        .map_err(|error| Fail::internal(format!("Craft AI Manager could not load the CUDA libraries in {}: {error}", dir.display())));
    let remembered = match &outcome {
        Ok(()) => Ok(()),
        Err(fail) => Err(fail.sentence.clone()),
    };
    table.insert(root.to_path_buf(), remembered);
    outcome
}

/// One GPU job at a time (amendment 18).
///
/// A CUDA session is the only thing that needs this: the card is shared with PhotoCraft, and two
/// CAIM jobs on it at once would be the surest way to run out of the memory the resolver just
/// measured. The guard is held for the length of one run, and a CPU run does not take it at all.
#[must_use]
pub fn job_gate(wanted: bool) -> Option<MutexGuard<'static, ()>> {
    if !wanted {
        return None;
    }
    static GPU_JOB: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = GPU_JOB.get_or_init(|| Mutex::new(()));
    Some(lock.lock().unwrap_or_else(PoisonError::into_inner))
}
// Reading the placement back

/// Where one CUDA session writes its ONNX Runtime profile.
///
/// Step 0's finding is the whole reason this exists: registering the provider is not the same thing
/// as using it. ONNX Runtime will run every node on the CPU next to a CUDA provider that registered
/// happily, and it does not say so. The only honest answer is in the profile of a session that has
/// actually run something, so a CUDA session is built with profiling on and [Placement::verdict]
/// reads the placement back after the first run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    prefix: PathBuf,
}

/// One profile prefix per CUDA session, so two sessions in one process never share a file.
static PLACEMENTS: AtomicUsize = AtomicUsize::new(0);

/// A place to profile, for a device that can be profiled. `None` means "CPU: nothing to read back".
#[must_use]
pub fn placement(device: device::Device) -> Option<Placement> {
    if device != device::Device::Cuda {
        return None;
    }
    let nth = PLACEMENTS.fetch_add(1, Ordering::Relaxed);
    Some(Placement { prefix: std::env::temp_dir().join(format!("caim-cuda-profile-{}-{nth}", std::process::id())) })
}

impl Placement {
    /// The prefix handed to ONNX Runtime when the session was built.
    #[must_use]
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// What the run that just finished actually used.
    ///
    /// ONNX Runtime names the file it wrote, which is not always the prefix it was given, so this
    /// reads that name and then removes both files: a profile is a diagnostic, not something CAIM
    /// keeps.
    pub fn verdict(&self, session: &mut ort::session::Session) -> Verdict {
        let named = match session.end_profiling() {
            Ok(named) => named,
            Err(error) => return Verdict::NotPlaced(format!("Craft AI Manager could not finish the ONNX Runtime profile for this session: {error}")),
        };
        let file = PathBuf::from(&named);
        let text = std::fs::read_to_string(&file);
        let _ = std::fs::remove_file(&file);
        if file != self.prefix {
            let _ = std::fs::remove_file(&self.prefix);
        }
        let Ok(text) = text else {
            return Verdict::NotPlaced(format!(
                "Craft AI Manager could not read the ONNX Runtime profile at {named}, so it cannot tell whether this model used the card."
            ));
        };
        let nodes = text.matches(CUDA_NODE_MARK).count();
        if nodes == 0 {
            return Verdict::NotPlaced(format!("ONNX Runtime ran this model without the card: the profile at {named} names no {CUDA_NODE_MARK} node."));
        }
        Verdict::Placed { nodes }
    }
}

/// Did the nodes land on the card?
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// At least one node ran on the card, so the provider is really doing work.
    Placed { nodes: usize },
    /// Nothing ran on the card. The sentence is what goes into `diagnostics`; the runner then
    /// retries on the CPU with amendment 18's `backend_init_failed`.
    NotPlaced(String),
}

/// Ask ONNX Runtime for the CUDA provider for this session.
///
/// The CPU provider stays underneath it, which is how ONNX Runtime itself handles an operator with
/// no CUDA kernel. *Whether* to ask is the resolver's decision ([crate::device]); this is the half
/// that knows the vendor's name for the thing.
pub fn register(
    builder: ort::session::builder::SessionBuilder,
) -> std::result::Result<ort::session::builder::SessionBuilder, Box<ort::Error<ort::session::builder::SessionBuilder>>> {
    builder.with_execution_providers([ort::ep::CUDA::default().build()]).map_err(Box::new)
}

// The archives

/// The three zip records this reader looks at.
const EOCD_SIGNATURE: [u8; 4] = *b"PK\x05\x06";
const CENTRAL_SIGNATURE: [u8; 4] = *b"PK\x01\x02";
const LOCAL_SIGNATURE: [u8; 4] = *b"PK\x03\x04";
/// The two methods NVIDIA's own archives use: stored and deflated.
const STORED: u16 = 0;
const DEFLATED: u16 = 8;
/// The end-of-central-directory record, plus the largest comment that may sit behind it.
const EOCD_SIZE: u64 = 22;
const EOCD_COMMENT_MAX: u64 = 0xFFFF;
/// A central directory entry before its name, and a local file header before its name.
const CENTRAL_HEADER: usize = 46;
const LOCAL_HEADER: usize = 30;
/// What every 32-bit zip field holds when the real number lives in a Zip64 record.
const ZIP64_MARKER: u32 = 0xFFFF_FFFF;

/// One member of an archive, as its central directory entry describes it.
#[derive(Clone, Debug)]
struct Member {
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    header_at: u64,
}

/// Unpack the members CAIM asked for, into `into`, and return where each one landed.
///
/// CAIM reads NVIDIA's archives itself rather than pulling in an archive library: it needs four
/// fixed names out of each one, and the format's own records say how big each member is and what its
/// CRC is, which is exactly the check amendment 15 wants before a library is loaded. Nothing is
/// written to its final name until the CRC and the length agree, and a member that is already on
/// disk with the right length is left alone, so re-running an install costs a read and no writes.
pub fn extract(archive: &Path, into: &Path, wanted: &[(String, String)]) -> Result<Vec<PathBuf>> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let mut file = File::open(archive).map_err(|error| Fail::internal(format!("Craft AI Manager could not open {}: {error}", archive.display())))?;
    let total =
        file.metadata().map(|meta| meta.len()).map_err(|error| Fail::internal(format!("Craft AI Manager could not measure {}: {error}", archive.display())))?;
    let table = central_directory(archive, &mut file, total)?;
    let mut done = Vec::with_capacity(wanted.len());
    for (inside, name) in wanted {
        let member = table
            .get(inside.as_str())
            .ok_or_else(|| Fail::internal(format!("{} does not contain {inside}, which Craft AI Manager needs for {name}.", archive.display())))?;
        let target = into.join(name);
        if target.metadata().map(|meta| meta.len()).ok() == Some(member.size) {
            done.push(target);
            continue;
        }
        unpack(archive, &mut file, member, &target)?;
        done.push(target);
    }
    Ok(done)
}

/// Every member the central directory lists, by the name it uses inside the archive.
fn central_directory(archive: &Path, file: &mut File, total: u64) -> Result<BTreeMap<String, Member>> {
    let window = (EOCD_SIZE + EOCD_COMMENT_MAX).min(total);
    file.seek(SeekFrom::Start(total - window)).map_err(|error| Fail::internal(format!("Craft AI Manager could not read {}: {error}", archive.display())))?;
    let mut tail = vec![0u8; window as usize];
    file.read_exact(&mut tail).map_err(|error| Fail::internal(format!("Craft AI Manager could not read the end of {}: {error}", archive.display())))?;
    let record = find_eocd(&tail).ok_or_else(|| {
        Fail::internal(format!("{} is not an archive Craft AI Manager can read: it has no end-of-central-directory record.", archive.display()))
    })?;
    let entries = u16_at(&tail, record + 10).unwrap_or(0);
    let directory_size = u32_at(&tail, record + 12).unwrap_or(0);
    let directory_at = u32_at(&tail, record + 16).unwrap_or(0);
    if entries == 0 || directory_size == ZIP64_MARKER || directory_at == ZIP64_MARKER {
        return Err(Fail::internal(format!("{} needs Zip64, which Craft AI Manager's archive reader does not understand.", archive.display())));
    }
    file.seek(SeekFrom::Start(u64::from(directory_at)))
        .map_err(|error| Fail::internal(format!("Craft AI Manager could not read {}: {error}", archive.display())))?;
    let mut table = BTreeMap::new();
    let mut header = [0u8; CENTRAL_HEADER];
    for _ in 0..u64::from(entries) {
        file.read_exact(&mut header)
            .map_err(|error| Fail::internal(format!("{} lists {} members but could not be read that far: {error}", archive.display(), entries)))?;
        if header.get(..4) != Some(&CENTRAL_SIGNATURE) {
            return Err(Fail::internal(format!("{} has a central directory Craft AI Manager does not recognise.", archive.display())));
        }
        let method = u16_at(&header, 10).unwrap_or(0);
        let crc = u32_at(&header, 16).unwrap_or(0);
        let compressed = u64::from(u32_at(&header, 20).unwrap_or(0));
        let size = u64::from(u32_at(&header, 24).unwrap_or(0));
        let name_len = usize::from(u16_at(&header, 28).unwrap_or(0));
        let extra_len = u64::from(u16_at(&header, 30).unwrap_or(0));
        let comment_len = u64::from(u16_at(&header, 32).unwrap_or(0));
        let header_at = u64::from(u32_at(&header, 42).unwrap_or(0));
        let mut name = vec![0u8; name_len];
        file.read_exact(&mut name).map_err(|error| Fail::internal(format!("{} has a member name that could not be read: {error}", archive.display())))?;
        if compressed == u64::from(ZIP64_MARKER) || size == u64::from(ZIP64_MARKER) || header_at == u64::from(ZIP64_MARKER) {
            return Err(Fail::internal(format!("{} has a Zip64 member, which Craft AI Manager's archive reader does not understand.", archive.display())));
        }
        std::io::copy(&mut (&mut *file).take(extra_len + comment_len), &mut std::io::sink())
            .map_err(|error| Fail::internal(format!("{} has a member that could not be skipped: {error}", archive.display())))?;
        table.insert(String::from_utf8_lossy(&name).into_owned(), Member { method, crc, compressed, size, header_at });
    }
    Ok(table)
}

/// The last end-of-central-directory record in the tail of an archive, as an offset into `tail`.
fn find_eocd(tail: &[u8]) -> Option<usize> {
    let mut at = tail.len();
    while at >= EOCD_SIZE as usize {
        at -= 1;
        if tail.get(at..at + 4) == Some(&EOCD_SIGNATURE) && u64::from(u16_at(tail, at + 20).unwrap_or(0)) + EOCD_SIZE + at as u64 == tail.len() as u64 {
            return Some(at);
        }
    }
    None
}

/// Where a member's bytes start: its local header is a second, shorter header of its own.
fn local_data_at(archive: &Path, file: &mut File, header_at: u64) -> Result<u64> {
    file.seek(SeekFrom::Start(header_at)).map_err(|error| Fail::internal(format!("Craft AI Manager could not read {}: {error}", archive.display())))?;
    let mut header = [0u8; LOCAL_HEADER];
    file.read_exact(&mut header).map_err(|error| Fail::internal(format!("{} has a member header that could not be read: {error}", archive.display())))?;
    if header.get(..4) != Some(&LOCAL_SIGNATURE) {
        return Err(Fail::internal(format!("{} has a member whose local header Craft AI Manager does not recognise.", archive.display())));
    }
    Ok(header_at + LOCAL_HEADER as u64 + u64::from(u16_at(&header, 26).unwrap_or(0)) + u64::from(u16_at(&header, 28).unwrap_or(0)))
}

/// Copy one member out, checking its CRC and its length before it gets its real name.
fn unpack(archive: &Path, file: &mut File, member: &Member, target: &Path) -> Result<()> {
    if member.method != STORED && member.method != DEFLATED {
        return Err(Fail::internal(format!(
            "{} holds {} compressed with method {}, which Craft AI Manager cannot read.",
            archive.display(),
            target.display(),
            member.method
        )));
    }
    let data_at = local_data_at(archive, file, member.header_at)?;
    file.seek(SeekFrom::Start(data_at)).map_err(|error| Fail::internal(format!("Craft AI Manager could not read {}: {error}", archive.display())))?;
    let mut source = (&mut *file).take(member.compressed);
    let part = part_path(target);
    let handle = File::create(&part).map_err(|error| Fail::internal(format!("Craft AI Manager could not write {}: {error}", part.display())))?;
    let mut writer = flate2::CrcWriter::new(std::io::BufWriter::new(handle));
    let copied = if member.method == DEFLATED {
        let mut decoder = flate2::read::DeflateDecoder::new(source);
        std::io::copy(&mut decoder, &mut writer)
    } else {
        std::io::copy(&mut source, &mut writer)
    };
    let crc = writer.crc().sum();
    if let Err(error) = writer.flush() {
        let _ = std::fs::remove_file(&part);
        return Err(Fail::internal(format!("Craft AI Manager could not write {}: {error}", part.display())));
    }
    drop(writer);
    let damaged = |sentence: String| {
        let _ = std::fs::remove_file(&part);
        Fail::internal(format!("{}: {sentence}", archive.display()))
    };
    match copied {
        Ok(copied) if copied != member.size => Err(damaged(format!("{} should be {} bytes unpacked but was {copied}", target.display(), member.size))),
        Err(error) => Err(damaged(format!("{} could not be unpacked: {error}", target.display()))),
        Ok(_) if crc != member.crc => {
            Err(damaged(format!("{} unpacked with CRC {crc:#010x}, not the {:#010x} the archive declares", target.display(), member.crc)))
        }
        Ok(_) => std::fs::rename(&part, target).map_err(|error| {
            let _ = std::fs::remove_file(&part);
            Fail::internal(format!("Craft AI Manager could not move {} into place: {error}", target.display()))
        }),
    }
}

/// A name for the half-written file, so an interrupted install never leaves a library that looks
/// complete.
#[must_use]
fn part_path(target: &Path) -> PathBuf {
    let mut text = target.as_os_str().to_os_string();
    text.push(".part");
    PathBuf::from(text)
}

/// A 16-bit little-endian field, or `None` if the buffer stops short of it.
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

/// A 32-bit little-endian field, or `None` if the buffer stops short of it.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Scratch;

    /// A zip archive built the way an archiver builds one: local headers, then the central
    /// directory, then the record that points at it. `method` is [STORED] or [DEFLATED].
    fn archive(members: &[(&str, u16, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, method, bytes) in members {
            let payload = if *method == DEFLATED {
                let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(bytes).expect("the fixture can be deflated");
                encoder.finish().expect("the fixture deflator can finish")
            } else {
                bytes.to_vec()
            };
            let mut hasher = flate2::Crc::new();
            hasher.update(bytes);
            let crc = hasher.sum();
            let header_at = out.len() as u32;
            out.extend_from_slice(&LOCAL_SIGNATURE);
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0u8; 4]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&payload);
            directory.extend_from_slice(&CENTRAL_SIGNATURE);
            directory.extend_from_slice(&[20u8, 0, 20, 0]);
            directory.extend_from_slice(&0u16.to_le_bytes());
            directory.extend_from_slice(&method.to_le_bytes());
            directory.extend_from_slice(&[0u8; 4]);
            directory.extend_from_slice(&crc.to_le_bytes());
            directory.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
            directory.extend_from_slice(&[0u8; 12]);
            directory.extend_from_slice(&header_at.to_le_bytes());
            directory.extend_from_slice(name.as_bytes());
        }
        let directory_at = out.len() as u32;
        out.extend_from_slice(&directory);
        out.extend_from_slice(&EOCD_SIGNATURE);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(directory.len() as u32).to_le_bytes());
        out.extend_from_slice(&directory_at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// An archive in a scratch folder, plus the folder its members are unpacked into.
    fn fixture(who: &str, bytes: &[u8]) -> (Scratch, PathBuf, PathBuf) {
        let scratch = Scratch::new(who);
        let archive_path = scratch.path.join("fixture.zip");
        std::fs::write(&archive_path, bytes).expect("the fixture archive can be written");
        let into = scratch.path.join("bin");
        std::fs::create_dir_all(&into).expect("the fixture folder can be made");
        (scratch, archive_path, into)
    }

    fn asked_for(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(inside, name)| (inside.to_string(), name.to_string())).collect()
    }

    #[test]
    fn every_component_lists_its_own_files_where_nvidia_puts_them() {
        for model in components() {
            let listed = wanted(&model);
            assert!(!listed.is_empty(), "{} names at least one library", model.id);
            let stem = model.file.strip_suffix(".zip").unwrap_or(&model.file);
            for (inside, name) in &listed {
                assert_eq!(inside, &format!("{stem}/bin/x64/{name}"), "{} sits where the archive says", name);
                assert!(name.ends_with(".dll"), "{name} is a Windows library");
            }
        }
    }

    #[test]
    fn ort_only_asks_for_names_caim_unpacks() {
        let mut held: Vec<&str> = Vec::new();
        for (_, names) in WANTED {
            held.extend(names.iter().copied());
        }
        for name in ort::ep::cuda::CUDA_DYLIBS.iter().chain(ort::ep::cuda::CUDNN_DYLIBS.iter()) {
            assert!(held.contains(name), "ONNX Runtime asks for {name}, which CAIM does not unpack");
        }
    }

    #[test]
    fn extract_writes_what_was_asked_for_and_nothing_else() {
        let (scratch, path, into) = fixture(
            "zip-stored",
            &archive(&[
                ("cuda_cudart/bin/x64/one.dll", STORED, b"first library"),
                ("cuda_cudart/bin/x64/two.dll", STORED, b"second library"),
                ("cuda_cudart/bin/x64/three.dll", STORED, b"never asked for"),
            ]),
        );
        let got = extract(&path, &into, &asked_for(&[("cuda_cudart/bin/x64/one.dll", "one.dll"), ("cuda_cudart/bin/x64/two.dll", "two.dll")]))
            .expect("a well-formed archive unpacks");
        assert_eq!(got.len(), 2);
        assert_eq!(std::fs::read(into.join("one.dll")).expect("the first library is on disk"), b"first library");
        assert_eq!(std::fs::read(into.join("two.dll")).expect("the second library is on disk"), b"second library");
        assert!(!into.join("three.dll").exists(), "a member nobody asked for stays in the archive");
        assert_eq!(into.join("one.dll").metadata().expect("asked about").len(), 13);
        drop(scratch);
    }

    #[test]
    fn extract_undoes_deflate_which_is_what_nvidia_uses() {
        let payload = vec![b'z'; 90_000];
        let (scratch, path, into) = fixture("zip-deflate", &archive(&[("libcublas/bin/x64/big.dll", DEFLATED, &payload)]));
        extract(&path, &into, &asked_for(&[("libcublas/bin/x64/big.dll", "big.dll")])).expect("a deflated member unpacks");
        assert_eq!(std::fs::read(into.join("big.dll")).expect("the library is on disk"), payload);
        drop(scratch);
    }

    #[test]
    fn extract_refuses_a_member_whose_crc_does_not_match() {
        let mut bytes = archive(&[("libcufft/bin/x64/bad.dll", STORED, b"0123456789")]);
        let at = bytes.windows(10).position(|window| window == b"0123456789").expect("the fixture holds its payload once");
        bytes[at] ^= 0b_0000_0001;
        let (scratch, path, into) = fixture("zip-crc", &bytes);
        let fail = extract(&path, &into, &asked_for(&[("libcufft/bin/x64/bad.dll", "bad.dll")])).expect_err("a member that does not match its CRC is refused");
        assert!(fail.sentence.contains("bad.dll"), "the sentence names the file: {}", fail.sentence);
        assert!(fail.sentence.contains("CRC"), "and says why: {}", fail.sentence);
        assert!(!into.join("bad.dll").exists(), "nothing reaches its final name");
        assert!(!part_path(&into.join("bad.dll")).exists(), "nor does a half-written file stay behind");
        drop(scratch);
    }

    #[test]
    fn extract_refuses_a_member_whose_length_does_not_match() {
        let (scratch, path, into) = fixture("zip-length", &archive(&[("libcudnn/bin/x64/short.dll", STORED, b"twelve bytes")]));
        let mut bytes = std::fs::read(&path).expect("the fixture can be read back");
        let record = find_eocd(&bytes).expect("the fixture has an end record");
        let at = u32_at(&bytes, record + 16).expect("the fixture names its directory");
        bytes[at as usize + 24..at as usize + 28].copy_from_slice(&13u32.to_le_bytes());
        std::fs::write(&path, bytes).expect("the fixture can be rewritten");
        let fail = extract(&path, &into, &asked_for(&[("libcudnn/bin/x64/short.dll", "short.dll")])).expect_err("a member of the wrong length is refused");
        assert!(fail.sentence.contains("bytes"), "the sentence says what was short: {}", fail.sentence);
        assert!(!into.join("short.dll").exists());
        drop(scratch);
    }

    #[test]
    fn extract_leaves_a_library_that_is_already_right_alone() {
        let (scratch, path, into) = fixture("zip-again", &archive(&[("cuda_cudart/bin/x64/same.dll", STORED, b"0123456789")]));
        extract(&path, &into, &asked_for(&[("cuda_cudart/bin/x64/same.dll", "same.dll")])).expect("the first pass writes it");
        std::fs::write(into.join("same.dll"), b"abcdefghij").expect("the test replaces the bytes, same length");
        extract(&path, &into, &asked_for(&[("cuda_cudart/bin/x64/same.dll", "same.dll")])).expect("the second pass is allowed");
        assert_eq!(std::fs::read(into.join("same.dll")).expect("the library is on disk"), b"abcdefghij", "a library of the right length is not rewritten");
        drop(scratch);
    }

    #[test]
    fn extract_names_a_member_the_archive_does_not_hold() {
        let (scratch, path, into) = fixture("zip-missing", &archive(&[("cuda_cudart/bin/x64/here.dll", STORED, b"here")]));
        let fail = extract(&path, &into, &asked_for(&[("cuda_cudart/bin/x64/gone.dll", "gone.dll")])).expect_err("a missing member is an error");
        assert!(fail.sentence.contains("gone.dll"), "the sentence names what is missing: {}", fail.sentence);
        drop(scratch);
    }

    #[test]
    fn extract_refuses_a_file_that_is_not_an_archive() {
        let (scratch, path, into) = fixture("zip-junk", b"this is a text file, not a zip archive, and it has no records at all");
        let fail = extract(&path, &into, &asked_for(&[("anything/bin/x64/x.dll", "x.dll")])).expect_err("junk is refused");
        assert!(fail.sentence.contains("end-of-central-directory"), "and says so: {}", fail.sentence);
        drop(scratch);
    }

    #[test]
    fn extract_of_nothing_asks_for_nothing() {
        let (scratch, path, into) = fixture("zip-empty", &archive(&[("cuda_cudart/bin/x64/one.dll", STORED, b"one")]));
        assert!(extract(&path, &into, &[]).expect("an empty list is fine").is_empty());
        drop(scratch);
    }

    #[test]
    fn the_probe_answer_is_read_from_the_end_of_each_line() {
        let found = parse_cards("NVIDIA GeForce RTX 4090, 991, 24564\nNVIDIA GeForce RTX 3060, 11234, 12288\n");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "NVIDIA GeForce RTX 4090");
        assert_eq!(found[0].free_mib, 991);
        assert_eq!(found[0].total_mib, 24564);
        assert_eq!(found[1].free_mib, 11234);
    }

    #[test]
    fn the_probe_answer_ignores_lines_that_are_not_a_card() {
        assert!(parse_cards("").is_empty());
        assert!(parse_cards("[NVIDIA-SMI has failed because it could not communicate with the driver]").is_empty());
        assert!(parse_cards("GeForce RTX, 12").is_empty(), "a line without both numbers is not a card");
    }

    #[test]
    fn the_cuda_folder_is_a_sibling_of_the_models_folder() {
        assert_eq!(
            store_root_in(None, Some(PathBuf::from(r"C:\Users\Erik\AppData\Local\craft-ai-manager\models"))),
            Some(PathBuf::from(r"C:\Users\Erik\AppData\Local\craft-ai-manager\cuda"))
        );
        assert_eq!(
            store_root_in(Some(std::ffi::OsString::from(r"D:\cuda-libs")), Some(PathBuf::from(r"C:\Users\Erik\models"))),
            Some(PathBuf::from(r"D:\cuda-libs")),
            "CRAFT_AI_CUDA wins"
        );
        assert_eq!(
            store_root_in(Some(std::ffi::OsString::from("")), Some(PathBuf::from(r"C:\Users\Erik\models"))),
            Some(PathBuf::from(r"C:\Users\Erik\cuda")),
            "an empty CRAFT_AI_CUDA is no answer"
        );
        assert_eq!(store_root_in(None, None), None, "a machine with no models folder has no CUDA folder either");
    }

    #[test]
    fn an_empty_folder_is_not_ready() {
        let scratch = Scratch::new("cuda-not-ready");
        assert!(!is_ready(&scratch.path));
        std::fs::create_dir_all(library_dir(&scratch.path)).expect("the library folder can be made");
        assert!(!is_ready(&scratch.path), "an empty folder is still not ready");
        drop(scratch);
    }

    #[test]
    fn only_a_cuda_session_gets_a_profile() {
        let first = placement(device::Device::Cuda).expect("CUDA is profiled");
        let second = placement(device::Device::Cuda).expect("every CUDA session is");
        assert_ne!(first.prefix(), second.prefix(), "two sessions never share a profile file");
        assert_eq!(placement(device::Device::Cpu), None, "a CPU session has nothing to read back");
    }
}
