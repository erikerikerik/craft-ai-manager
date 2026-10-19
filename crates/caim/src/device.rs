//! One resolver decides the device for every runner (MG-11, SPEC amendment 18).
//!
//! Amendment 18 is word for word about the shape: an enum of the ONNX Runtime execution providers -
//! `Cpu` and `Cuda`, nothing else in M2 - and exactly one resolver that takes the task, the model's
//! manifest entry, the device setting, and what the machine has, and hands back the device, its name,
//! and the fallback chain. A runner never chooses a device; it is handed one, and it reports what
//! actually ran.
//!
//! Everything that knows what a GPU *is* - detection, free memory, the vendor's libraries - lives in
//! [crate::cuda]. This module only asks whether the machine has an accelerator and whether that
//! accelerator is ready, which is what lets the whole table be tested on a machine with no GPU at all.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::catalog::Model;
use crate::error::{Fail, Result};

/// The sentence a runner answers with when the job was cancelled. Both runners and the chain share
/// one wording so a cancelled job reads the same whatever ran.
pub const CANCELLED: &str = "Craft AI Manager stopped the job.";

/// The sentence a job fails with when every device in its chain had its turn and none could run the
/// model. Amendment 18 keeps the vendor's own text in `diagnostics`, never here.
pub const NO_DEVICE: &str = "Craft AI Manager could not run the model on any device.";

/// How much of the runtime's own error text a result keeps. Long enough for a real ONNX Runtime
/// message, short enough that one line of `activity.jsonl` stays readable.
pub const MAX_DIAGNOSTICS: usize = 600;

/// Trim the runtime's own error text to something worth logging: control characters become spaces,
/// and only the first [MAX_DIAGNOSTICS] characters are kept.
#[must_use]
pub fn bounded_diagnostics(text: &str) -> String {
    text.chars().map(|letter| if letter.is_control() { ' ' } else { letter }).take(MAX_DIAGNOSTICS).collect()
}

/// Which ONNX Runtime execution provider a job runs on. M2 has these two and no others (D10: no
/// DirectML); a third one later means a new variant here and one new arm in the resolver, not a new
/// decision in a runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Device {
    /// ONNX Runtime's CPU provider. Always available, and always the last stop of a fallback chain.
    Cpu,
    /// ONNX Runtime's CUDA provider. Needs a card and the vendor's libraries.
    Cuda,
}

impl Device {
    /// The word the protocol and the manifest use: `cpu`, `cuda`.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Device::Cpu => "cpu",
            Device::Cuda => "cuda",
        }
    }

    /// The hardware in plain words, for `device_name` when nothing better is known.
    #[must_use]
    pub const fn plain_name(self) -> &'static str {
        match self {
            Device::Cpu => "CPU",
            Device::Cuda => "CUDA",
        }
    }
}

/// What the user (or `caim serve --device`) asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Setting {
    /// The best backend the manifest marks `supported`, then the CPU. What `caim serve` uses by
    /// default, and the only setting that never reports a fallback when it lands on the CPU.
    #[default]
    Auto,
    /// The CPU, whatever the machine has.
    Cpu,
    /// CUDA if it can be had at all, and the CPU if it cannot - never a failed job.
    Cuda,
}

impl Setting {
    /// The word the protocol reports as `device_requested`, and the word the flag takes.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Setting::Auto => "auto",
            Setting::Cpu => "cpu",
            Setting::Cuda => "cuda",
        }
    }

    /// The setting a word names, whatever its case. `None` for anything else, so the command line
    /// can answer with its own sentence.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "auto" => Some(Setting::Auto),
            "cpu" => Some(Setting::Cpu),
            "cuda" => Some(Setting::Cuda),
            _ => None,
        }
    }
}

/// Why a backend other than the one that was asked for ran. Amendment 18 fixes this vocabulary: a
/// result carries one of these words, or nothing at all, and the vendor's own text goes in
/// `diagnostics` instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// No CUDA device, or the device is there but its libraries are not installed.
    BackendUnavailable,
    /// The CUDA session could not be made: ONNX Runtime refused the provider or the graph.
    BackendInitFailed,
    /// Free device memory measured now is below the model's `memory_mib`.
    InsufficientMemory,
    /// The manifest does not mark this model `supported` for that backend.
    UnsupportedModel,
    /// A run on that backend failed partway through, so the job was retried on the next device.
    ExecutionFailed,
}

impl Reason {
    /// The exact word amendment 18 puts in `fallback_reason`.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Reason::BackendUnavailable => "backend_unavailable",
            Reason::BackendInitFailed => "backend_init_failed",
            Reason::InsufficientMemory => "insufficient_memory",
            Reason::UnsupportedModel => "unsupported_model",
            Reason::ExecutionFailed => "execution_failed",
        }
    }
}

/// What the resolver is allowed to know about one model: the manifest's backend table, and the
/// memory one job needs. A thin view of a [crate::catalog::Model] so the resolver can be handed a
/// synthetic entry in a test without inventing a whole catalog entry.
#[derive(Clone, Debug)]
pub struct Manifest {
    pub id: String,
    pub memory_mib: u64,
    /// The `backends` field as the manifest wrote it: `cpu`, `cuda`, `coreml` to `supported`,
    /// `experimental`, or `untested`. A missing key means that backend was never claimed.
    pub backends: BTreeMap<String, String>,
}

impl Manifest {
    /// The manifest view of a real catalog entry.
    #[must_use]
    pub fn from_model(model: &Model) -> Self {
        Self { id: model.id.clone(), memory_mib: model.memory_mib, backends: model.backends.clone() }
    }

    /// A manifest that only ever runs on the CPU, which is what a runner without a catalog entry
    /// hands the resolver.
    #[must_use]
    pub fn cpu_only(id: &str, memory_mib: u64) -> Self {
        let mut backends = BTreeMap::new();
        backends.insert(Device::Cpu.token().to_string(), "supported".to_string());
        Self { id: id.to_string(), memory_mib, backends }
    }

    /// What the manifest says about one backend. `None` when it says nothing, which amendment 18
    /// reads as "CPU only".
    #[must_use]
    pub fn state(&self, device: Device) -> Option<&str> {
        self.backends.get(device.token()).map(String::as_str)
    }
}

/// One accelerator the machine has. [crate::cuda] fills these in; nothing here knows the vendor's
/// names, and a test can build one by hand.
#[derive(Clone, Debug)]
pub struct Accelerator {
    /// Which provider this accelerator serves.
    pub device: Device,
    /// Which card this is in the runtime's own numbering, or `None` when the probe could not prove
    /// which card it is. A session belongs to one card, not to "a CUDA device", so this is what
    /// [Machine::mark_resident] and the runners compare - the second review round found that a
    /// session on one card was excusing a memory check on another.
    pub card: Option<i32>,
    /// The hardware in plain words, for `device_name`: `NVIDIA GeForce RTX 4090`.
    pub name: String,
    /// Memory free *now*, in MiB. `None` when the machine could not say, which the resolver reads as
    /// "not enough" rather than as "unknown, go ahead".
    pub free_mib: Option<u64>,
    /// Are the libraries that provider needs installed?
    pub ready: bool,
    /// Does the pipeline that is about to run this job already keep this model's session on this
    /// accelerator? A probe cannot know, so [crate::cuda] always says no; [crate::job] sets it from
    /// the pipeline that owns the session. It matters because the free memory above was measured
    /// *after* that session allocated its model: charging those same bytes again would send the next
    /// job to the CPU and throw away the warm session the chain exists to keep, which is what the
    /// independent review of the first draft found.
    pub resident: bool,
    /// What to tell a human when `ready` is false.
    pub note: Option<String>,
}

/// What the machine has. Built by probing once per job that could want an accelerator; a test builds
/// one of these by hand instead.
#[derive(Clone, Debug, Default)]
pub struct Machine {
    pub accelerators: Vec<Accelerator>,
}

impl Machine {
    /// Ask the machine. Cheap on a machine with no GPU - one short probe - and the only place the
    /// vendor's own tool is run for a job.
    #[must_use]
    pub fn probe() -> Self {
        Self { accelerators: crate::cuda::accelerators() }
    }

    /// A machine with nothing but a CPU, which is what a test that does not care starts with.
    #[must_use]
    pub fn cpu_only() -> Self {
        Self::default()
    }

    /// The accelerator for one provider, if this machine has one.
    #[must_use]
    pub fn accelerator(&self, device: Device) -> Option<&Accelerator> {
        self.accelerators.iter().find(|accelerator| accelerator.device == device)
    }

    /// Record that the pipeline about to run this job already holds its session on the card `card`,
    /// so the resolver must not charge that memory a second time. The queue knows which pipeline is
    /// running and which card its session sits on; the probe cannot.
    ///
    /// The card has to match, not just the device. On a machine with two cards, a session sitting on
    /// card 1 says nothing about how much of card 0 is free, and excusing card 0 on that evidence
    /// would let a job be measured against one card and run on another - which is what the second
    /// review round found. A session whose card nobody proved excuses nothing either.
    pub fn mark_resident(&mut self, device: Device, card: Option<i32>) {
        let Some(card) = card else { return };
        if let Some(accelerator) = self.accelerators.iter_mut().find(|accelerator| accelerator.device == device && accelerator.card == Some(card)) {
            accelerator.resident = true;
        }
    }
}

/// What the resolver decided: the order the runners try devices in, and the account of why.
///
/// The chain always ends at the CPU, so a job the CPU can run is never failed by a device that
/// cannot have it (amendment 18). `reason` and `note` are only filled when the setting asked for
/// something the machine could not give: `auto` landing on the CPU is what `auto` meant, so it has
/// nothing to apologise for.
#[derive(Clone, Debug)]
pub struct Decision {
    /// What was asked for, which is what `device_requested` reports.
    pub requested: Setting,
    /// The devices to try, in order. The first one is the one that was asked for.
    pub chain: Vec<Device>,
    /// Why the asked-for device is not in the chain at all. `None` when it is.
    pub reason: Option<Reason>,
    /// The hardware the chain's first device is, when it is an accelerator: `NVIDIA GeForce RTX 4090`.
    pub device_name: Option<String>,
    /// Which card that device is, in the runtime's own numbering, or `None` when the decision named
    /// no card. The runners compare it with the card their cached session is on, so a session built
    /// for one card is never reused for a decision about another.
    pub card: Option<i32>,
    /// The same decision as a sentence for the log, when there is one to make.
    pub note: Option<String>,
}

impl Decision {
    /// The device this decision wants, which is the one that counts as "the requested one ran".
    #[must_use]
    pub fn head(&self) -> Device {
        self.chain.first().copied().unwrap_or(Device::Cpu)
    }

    /// A decision that only ever uses the CPU, for a caller with nothing to resolve - the fake
    /// backend, or a `--device cpu` serve.
    #[must_use]
    pub fn for_setting(setting: Setting) -> Self {
        Self { requested: setting, chain: vec![Device::Cpu], reason: None, device_name: None, card: None, note: None }
    }

    /// The CPU decision the old code effectively hardcoded, kept for callers that have no manifest.
    #[must_use]
    pub fn cpu_only() -> Self {
        Self::for_setting(Setting::Cpu)
    }

    /// The `device_name` for a device that actually ran: the card's own words when this decision
    /// named a card, and the plain hardware word otherwise.
    #[must_use]
    pub fn name_for(&self, device: Device) -> String {
        match device {
            // A CPU run is a CPU run whatever the decision was hoping for.
            Device::Cpu => Device::Cpu.plain_name().to_string(),
            Device::Cuda => self.device_name.clone().unwrap_or_else(|| device.plain_name().to_string()),
        }
    }

    /// The `fallback_reason` for a job that ran on `ran_on`, given the reason the chain itself
    /// collected (`[None]` when every device it tried worked).
    ///
    /// Amendment 18: `null` when the requested backend ran. When it did not, the chain's own reason
    /// wins - a CUDA run that died mid-job is `execution_failed`, not whatever the resolver thought
    /// beforehand.
    #[must_use]
    pub fn fallback_reason(&self, ran_on: Device, chain_reason: Option<Reason>) -> Option<Reason> {
        if ran_on == self.head() { self.reason } else { chain_reason.or(self.reason) }
    }
}

/// The one resolver (amendment 18). It takes the task, the model's manifest entry, the device
/// setting, and what the machine has, and returns the device, the device name, and the fallback
/// chain. Nothing else in CAIM chooses a backend.
#[derive(Clone, Copy, Debug)]
pub struct Resolver {
    /// What the user asked for.
    pub setting: Setting,
    /// Whether `experimental` counts as buildable. Nothing in M2 sets this: it is here so the
    /// manifest's third state has a meaning without letting `auto` pick an untested backend.
    pub experimental: bool,
}

impl Resolver {
    #[must_use]
    pub fn new(setting: Setting) -> Self {
        Self { setting, experimental: false }
    }

    #[must_use]
    pub fn with_experimental(setting: Setting, experimental: bool) -> Self {
        Self { setting, experimental }
    }

    /// Can this model run on CUDA at all, as the manifest says? Amendment 18: `untested` and
    /// `experimental` are never picked by `auto`, and a missing `backends` field means CPU only.
    #[must_use]
    fn buildable(&self, manifest: &Manifest) -> bool {
        match manifest.state(Device::Cuda) {
            Some("supported") => true,
            Some("experimental") => self.experimental,
            _ => false,
        }
    }

    /// Decide with the machine already in hand.
    #[must_use]
    pub fn resolve(&self, task: &str, manifest: &Manifest, machine: &Machine) -> Decision {
        if self.setting == Setting::Cpu {
            return Decision::for_setting(Setting::Cpu);
        }
        // A blocked CUDA is a CPU job. The reason and the sentence only belong in the result when
        // the setting actually asked for CUDA; `auto` that ends up on the CPU asked for the CPU.
        let blocked = |reason: Reason, note: String| Decision {
            requested: self.setting,
            chain: vec![Device::Cpu],
            reason: (self.setting == Setting::Cuda).then_some(reason),
            device_name: None,
            card: None,
            note: (self.setting == Setting::Cuda).then_some(note),
        };
        if !self.buildable(manifest) {
            return blocked(Reason::UnsupportedModel, format!("{} has no tested CUDA build for {} yet, so this job runs on the CPU.", manifest.id, task));
        }
        let Some(card) = machine.accelerator(Device::Cuda) else {
            return blocked(Reason::BackendUnavailable, "This machine has no CUDA device, so this job runs on the CPU.".to_string());
        };
        if !card.ready {
            let note = card.note.clone().unwrap_or_else(|| format!("The CUDA libraries for {} are not installed, so this job runs on the CPU.", card.name));
            return blocked(Reason::BackendUnavailable, note);
        }
        // Amendment 18: free memory measured now, never the card's total. PhotoCraft holds VRAM of
        // its own (upstream #5234, #5973), so the whole card is never ours.
        //
        // A pipeline that already keeps this model's session on the card is the exception the probe
        // cannot see: the free memory it measured is what is left over that session, and reusing it
        // allocates nothing new. Charging it twice would fall back to the CPU on every job after the
        // first, so the gate only applies to a model that still has to be loaded.
        let free = card.free_mib.unwrap_or(0);
        if !card.resident && free < manifest.memory_mib {
            return blocked(
                Reason::InsufficientMemory,
                format!(
                    "{} needs {} MiB of device memory and {} has {} MiB free, so this job runs on the CPU.",
                    manifest.id, manifest.memory_mib, card.name, free
                ),
            );
        }
        Decision {
            requested: self.setting,
            chain: vec![Device::Cuda, Device::Cpu],
            reason: None,
            device_name: Some(card.name.clone()),
            card: card.card,
            note: None,
        }
    }

    /// Decide, probing the machine only when the setting could want CUDA *and* the manifest says the
    /// model could run there. A `--device cpu` serve and a model with no CUDA build never touch the
    /// vendor's tool at all.
    #[must_use]
    pub fn resolve_lazy(&self, task: &str, manifest: &Manifest, probe: &dyn Fn() -> Machine) -> Decision {
        if self.setting == Setting::Cpu || !self.buildable(manifest) {
            return self.resolve(task, manifest, &Machine::cpu_only());
        }
        self.resolve(task, manifest, &probe())
    }
}

/// What one device's attempt at a run produced. A runner reports a failure with the reason and the
/// runtime's own text, which [run_on_chain] turns into `fallback_reason` and `diagnostics`.
pub enum Attempt<T> {
    /// The model answered.
    Ran(T),
    /// The model did not, and here is what the runtime said.
    Failed { reason: Reason, diagnostics: String },
}

/// A finished job and the account of how it got there.
#[derive(Debug)]
pub struct ChainOutcome<T> {
    /// Whatever the successful run returned.
    pub value: T,
    /// The device that actually ran it.
    pub device: Device,
    /// How long that device's session took to make: 0 when a later job reused it.
    pub load_ms: u64,
    /// How long the model itself took.
    pub run_ms: u64,
    /// Why an earlier device of the chain did not work. `None` when the first one did.
    pub reason: Option<Reason>,
    /// The runtime's own text for that earlier device, trimmed to [MAX_DIAGNOSTICS].
    pub diagnostics: Option<String>,
}

/// The closure that uses one session: it is handed the session, the device it was built for, and the
/// job's progress callback, and it answers with an [Attempt]. Named because every runner passes the
/// same shape, and un-named it is hard to read.
pub type Runner<'a, S, T> = dyn FnMut(&mut S, Device, &mut dyn FnMut(f64, &str)) -> Attempt<T> + 'a;

/// Where the chain reports the session load: how far through the job it sits, and the sentence that
/// names it. Both runners have their own numbers, so the two travel together.
#[derive(Clone, Copy, Debug)]
pub struct LoadStage<'a> {
    /// How far through the job the load is reported.
    pub at: f64,
    /// The stage's name, as the job sees it.
    pub message: &'a str,
}

/// Try one device after another until a run works, keeping one session per device.
///
/// This is the only place a fallback chain is walked, so both runners get the same behaviour and the
/// same reasons for free. `load` makes a session for one device, `run` uses it, and `cancelled` is
/// the job's `DELETE` flag: a chain never starts a device the user has already stopped, and a run
/// that failed *after* a `DELETE` is a cancellation rather than a failure. `run` is handed the same
/// `progress` callback so it can name its own stage, which keeps there being one borrow of it.
///
/// The session for the device that worked stays in `cache`, so the next job pays one load rather
/// than two. Only one session is kept at a time, on purpose: a model is gigabytes, and a chain that
/// has just fallen back must not be holding a second copy of it - least of all on a card that has
/// just said it has no room. A chain that fails again next job therefore pays that load again.
pub fn run_on_chain<S: ?Sized, T>(
    chain: &[Device],
    cache: &mut Option<(Device, Box<S>)>,
    load: &mut dyn FnMut(Device) -> Result<Box<S>>,
    run: &mut Runner<'_, S, T>,
    progress: &mut dyn FnMut(f64, &str),
    stage: LoadStage<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ChainOutcome<T>> {
    let mut failure: Option<(Reason, String)> = None;
    for &device in chain {
        if cancelled() {
            return Err(Fail::internal(CANCELLED));
        }
        let mut load_ms = 0u64;
        let mut session = match cache.take() {
            // The session we kept is the one this device wants: reuse it, and pay no load.
            Some((cached, session)) if cached == device => session,
            // Anything else: a session for another device goes back into the guard untouched, and
            // the one this device wants is made now.
            other => {
                if let Some((cached, session)) = other {
                    *cache = Some((cached, session));
                }
                progress(stage.at, stage.message);
                let loading = Instant::now();
                match load(device) {
                    Ok(session) => {
                        load_ms = elapsed_ms(&loading);
                        session
                    }
                    Err(fail) => {
                        failure = Some((Reason::BackendInitFailed, bounded_diagnostics(&fail.sentence)));
                        continue;
                    }
                }
            }
        };
        let running = Instant::now();
        let attempt = run(&mut session, device, progress);
        let run_ms = elapsed_ms(&running);
        // The session goes back whatever happened, so a later job can use it again.
        *cache = Some((device, session));
        match attempt {
            Attempt::Ran(value) => {
                let (reason, diagnostics) = match failure {
                    Some((reason, diagnostics)) => (Some(reason), Some(diagnostics)),
                    None => (None, None),
                };
                return Ok(ChainOutcome { value, device, load_ms, run_ms, reason, diagnostics });
            }
            Attempt::Failed { reason, diagnostics } => {
                // A run ORT stopped because the job was cancelled is a cancellation, not a failure,
                // and the next device must not be tried for it.
                if cancelled() {
                    return Err(Fail::internal(CANCELLED));
                }
                failure = Some((reason, bounded_diagnostics(&diagnostics)));
            }
        }
    }
    Err(Fail::internal(NO_DEVICE))
}

/// How long something took, in whole milliseconds.
fn elapsed_ms(started: &Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        Accelerator, Attempt, CANCELLED, ChainOutcome, Decision, Device, LoadStage, MAX_DIAGNOSTICS, Machine, Manifest, NO_DEVICE, Reason, Resolver, Setting,
        bounded_diagnostics, run_on_chain,
    };
    use crate::error::{Fail, Status};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const CUDA_SUPPORTED: &[(&str, &str)] = &[("cpu", "supported"), ("cuda", "supported")];
    const CUDA_UNTESTED: &[(&str, &str)] = &[("cpu", "supported"), ("cuda", "untested")];
    const CUDA_EXPERIMENTAL: &[(&str, &str)] = &[("cpu", "supported"), ("cuda", "experimental")];
    const NO_BACKENDS: &[(&str, &str)] = &[];
    const CARD: &str = "NVIDIA GeForce RTX 4090";

    fn manifest(states: &[(&str, &str)], memory_mib: u64) -> Manifest {
        Manifest { id: "lama".to_string(), memory_mib, backends: states.iter().map(|(name, state)| ((*name).to_string(), (*state).to_string())).collect() }
    }

    fn machine(free_mib: Option<u64>, ready: bool) -> Machine {
        Machine {
            accelerators: vec![Accelerator {
                device: Device::Cuda,
                card: Some(0),
                name: CARD.to_string(),
                free_mib,
                ready,
                resident: false,
                note: (!ready).then(|| "run caim cuda install".to_string()),
            }],
        }
    }

    /// A machine with a card that can take a 1,500 MiB model.
    fn free_card() -> Machine {
        machine(Some(24_000), true)
    }

    #[test]
    fn the_five_reason_words_are_exactly_amendment_18s() {
        assert_eq!(Reason::BackendUnavailable.token(), "backend_unavailable");
        assert_eq!(Reason::BackendInitFailed.token(), "backend_init_failed");
        assert_eq!(Reason::InsufficientMemory.token(), "insufficient_memory");
        assert_eq!(Reason::UnsupportedModel.token(), "unsupported_model");
        assert_eq!(Reason::ExecutionFailed.token(), "execution_failed");
        assert_eq!(Device::Cpu.token(), "cpu");
        assert_eq!(Device::Cuda.token(), "cuda");
        assert_eq!(Setting::Auto.token(), "auto");
        assert_eq!(Setting::Cpu.token(), "cpu");
        assert_eq!(Setting::Cuda.token(), "cuda");
        assert_eq!(Device::Cpu.plain_name(), "CPU");
    }

    #[test]
    fn the_device_setting_parses_its_three_words_in_any_case() {
        assert_eq!(Setting::parse("auto"), Some(Setting::Auto));
        assert_eq!(Setting::parse("CUDA"), Some(Setting::Cuda));
        assert_eq!(Setting::parse("Cpu"), Some(Setting::Cpu));
        assert_eq!(Setting::parse("openvino"), None);
        assert_eq!(Setting::parse(""), None);
        assert_eq!(Setting::default(), Setting::Auto);
    }

    #[test]
    fn a_cpu_setting_never_probes_the_machine_and_never_reports_a_fallback() {
        let probes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&probes);
        let decision = Resolver::new(Setting::Cpu).resolve_lazy("fill_masked_area", &manifest(CUDA_SUPPORTED, 1_500), &move || {
            counter.fetch_add(1, Ordering::SeqCst);
            free_card()
        });
        assert_eq!(decision.chain, vec![Device::Cpu]);
        assert_eq!(decision.requested, Setting::Cpu);
        assert_eq!(decision.reason, None);
        assert_eq!(decision.note, None);
        assert_eq!(decision.fallback_reason(Device::Cpu, None), None);
        assert_eq!(probes.load(Ordering::SeqCst), 0, "a CPU serve has no reason to ask about the GPU");
    }

    #[test]
    fn a_model_with_no_cuda_build_never_probes_the_machine_either() {
        let probes = Arc::new(AtomicUsize::new(0));
        for states in [CUDA_UNTESTED, CUDA_EXPERIMENTAL, NO_BACKENDS] {
            let counter = Arc::clone(&probes);
            let decision = Resolver::new(Setting::Cuda).resolve_lazy("subject_mask", &manifest(states, 6_600), &move || {
                counter.fetch_add(1, Ordering::SeqCst);
                free_card()
            });
            assert_eq!(decision.chain, vec![Device::Cpu], "{states:?} is not buildable, so the card is irrelevant");
            assert_eq!(decision.reason, Some(Reason::UnsupportedModel));
        }
        assert_eq!(probes.load(Ordering::SeqCst), 0, "an untested model is refused before anything is measured");
    }

    #[test]
    fn auto_takes_cuda_when_the_manifest_supports_it_and_the_card_has_room() {
        let decision = Resolver::new(Setting::Auto).resolve("subject_mask", &manifest(CUDA_SUPPORTED, 6_600), &free_card());
        assert_eq!(decision.chain, vec![Device::Cuda, Device::Cpu]);
        assert_eq!(decision.head(), Device::Cuda);
        assert_eq!(decision.requested, Setting::Auto);
        assert_eq!(decision.device_name.as_deref(), Some(CARD));
        assert_eq!(decision.reason, None);
        assert_eq!(decision.name_for(Device::Cuda), CARD);
        assert_eq!(decision.name_for(Device::Cpu), "CPU");
        assert_eq!(decision.fallback_reason(Device::Cuda, None), None);
    }

    #[test]
    fn auto_never_picks_an_untested_experimental_or_missing_cuda() {
        for states in [CUDA_UNTESTED, CUDA_EXPERIMENTAL, NO_BACKENDS] {
            let decision = Resolver::new(Setting::Auto).resolve("fill_masked_area", &manifest(states, 1_500), &free_card());
            assert_eq!(decision.chain, vec![Device::Cpu], "{states:?} is not a claim auto may act on");
            // Auto landing on the CPU is what auto asked for, so there is nothing to report.
            assert_eq!(decision.reason, None);
            assert_eq!(decision.note, None);
            assert_eq!(decision.fallback_reason(Device::Cpu, None), None);
        }
    }

    #[test]
    fn experimental_only_counts_when_someone_asks_for_it() {
        let resolver = Resolver::with_experimental(Setting::Auto, true);
        let decision = resolver.resolve("fill_masked_area", &manifest(CUDA_EXPERIMENTAL, 1_500), &free_card());
        assert_eq!(decision.chain, vec![Device::Cuda, Device::Cpu]);
        assert_eq!(Resolver::new(Setting::Auto).resolve("fill_masked_area", &manifest(CUDA_EXPERIMENTAL, 1_500), &free_card()).chain, vec![Device::Cpu]);
    }

    #[test]
    fn cuda_asked_for_an_untested_model_says_unsupported_model() {
        let decision = Resolver::new(Setting::Cuda).resolve("subject_mask", &manifest(CUDA_UNTESTED, 6_600), &free_card());
        assert_eq!(decision.chain, vec![Device::Cpu]);
        assert_eq!(decision.requested, Setting::Cuda);
        assert_eq!(decision.reason, Some(Reason::UnsupportedModel));
        assert_eq!(decision.fallback_reason(Device::Cpu, None), Some(Reason::UnsupportedModel));
        let note = decision.note.clone().expect("cuda was asked for, so the result says why");
        assert!(note.contains("lama"), "{note}");
        assert!(note.contains("CPU"), "{note}");
    }

    #[test]
    fn a_machine_with_no_card_says_backend_unavailable() {
        let decision = Resolver::new(Setting::Cuda).resolve("fill_masked_area", &manifest(CUDA_SUPPORTED, 1_500), &Machine::cpu_only());
        assert_eq!(decision.chain, vec![Device::Cpu]);
        assert_eq!(decision.reason, Some(Reason::BackendUnavailable));
        assert_eq!(decision.fallback_reason(Device::Cpu, None), Some(Reason::BackendUnavailable));
        assert_eq!(decision.device_name, None);
        // The same machine under auto has nothing to say.
        assert_eq!(Resolver::new(Setting::Auto).resolve("fill_masked_area", &manifest(CUDA_SUPPORTED, 1_500), &Machine::cpu_only()).reason, None);
    }

    #[test]
    fn a_card_without_the_libraries_says_backend_unavailable_and_names_the_command() {
        let decision = Resolver::new(Setting::Cuda).resolve("fill_masked_area", &manifest(CUDA_SUPPORTED, 1_500), &machine(Some(24_000), false));
        assert_eq!(decision.chain, vec![Device::Cpu]);
        assert_eq!(decision.reason, Some(Reason::BackendUnavailable));
        let note = decision.note.clone().expect("the user needs to know what to run");
        assert!(note.contains("caim cuda install"), "{note}");
    }

    #[test]
    fn free_memory_below_the_model_says_insufficient_memory() {
        // The 4090 in the room this was written on: 24,564 MiB total, 991 MiB free with PhotoCraft
        // holding the rest. Total is never the number that matters.
        let decision = Resolver::new(Setting::Cuda).resolve("subject_mask", &manifest(CUDA_SUPPORTED, 6_600), &machine(Some(991), true));
        assert_eq!(decision.chain, vec![Device::Cpu]);
        assert_eq!(decision.reason, Some(Reason::InsufficientMemory));
        let note = decision.note.clone().expect("the numbers belong in the log");
        assert!(note.contains("6600") && note.contains("991"), "{note}");
        assert_eq!(decision.fallback_reason(Device::Cpu, None), Some(Reason::InsufficientMemory));
        // Exactly enough is enough.
        assert_eq!(
            Resolver::new(Setting::Cuda).resolve("subject_mask", &manifest(CUDA_SUPPORTED, 6_600), &machine(Some(6_600), true)).chain,
            vec![Device::Cuda, Device::Cpu]
        );
    }

    #[test]
    fn a_model_already_sitting_on_the_card_is_not_charged_twice() {
        // The 4090 after one CUDA job: 991 MiB free is what is left over the session this pipeline is
        // holding, and the next job for that same model reuses it rather than loading a second copy.
        let mut held = machine(Some(991), true);
        held.mark_resident(Device::Cuda, Some(0));
        assert_eq!(Resolver::new(Setting::Cuda).resolve("subject_mask", &manifest(CUDA_SUPPORTED, 6_600), &held).chain, vec![Device::Cuda, Device::Cpu]);
        // A model the pipeline has not loaded yet still has to fit in what is actually free.
        assert_eq!(Resolver::new(Setting::Cuda).resolve("subject_mask", &manifest(CUDA_SUPPORTED, 6_600), &machine(Some(991), true)).chain, vec![Device::Cpu]);
        // Only the accelerator the session sits on is excused; marking a device the machine does not
        // have changes nothing.
        let mut other = machine(Some(991), true);
        other.mark_resident(Device::Cpu, Some(0));
        assert_eq!(Resolver::new(Setting::Cuda).resolve("subject_mask", &manifest(CUDA_SUPPORTED, 6_600), &other).reason, Some(Reason::InsufficientMemory));
    }

    /// The second review round's finding: a warm session says something about the card it sits on,
    /// and about no other card. Getting this wrong measures one card and runs the job on another.
    #[test]
    fn a_session_on_another_card_does_not_excuse_this_one() {
        // The probe now names card 0 as the one to use, and it has 991 MiB free. The pipeline's warm
        // session is on card 1, which says nothing at all about card 0.
        let mut two_cards = machine(Some(991), true);
        two_cards.accelerators.push(Accelerator {
            device: Device::Cuda,
            card: Some(1),
            name: "NVIDIA GeForce RTX 3060".to_string(),
            free_mib: Some(11_234),
            ready: true,
            resident: false,
            note: None,
        });
        two_cards.mark_resident(Device::Cuda, Some(1));
        let decision = Resolver::new(Setting::Cuda).resolve("subject_mask", &manifest(CUDA_SUPPORTED, 6_600), &two_cards);
        assert_eq!(decision.chain, vec![Device::Cpu], "card 0 has 991 MiB free, and the session on card 1 is not evidence about it");
        assert_eq!(decision.reason, Some(Reason::InsufficientMemory));
        // A session whose card nobody proved excuses nothing either.
        let mut unproven = machine(Some(991), true);
        unproven.mark_resident(Device::Cuda, None);
        assert_eq!(Resolver::new(Setting::Cuda).resolve("subject_mask", &manifest(CUDA_SUPPORTED, 6_600), &unproven).reason, Some(Reason::InsufficientMemory));
    }

    #[test]
    fn a_card_that_cannot_report_free_memory_is_not_given_the_job() {
        let decision = Resolver::new(Setting::Cuda).resolve("fill_masked_area", &manifest(CUDA_SUPPORTED, 1_500), &machine(None, true));
        assert_eq!(decision.reason, Some(Reason::InsufficientMemory));
    }

    #[test]
    fn a_chain_failure_beats_what_the_resolver_thought() {
        let decision = Decision {
            requested: Setting::Cuda,
            chain: vec![Device::Cuda, Device::Cpu],
            reason: None,
            device_name: Some(CARD.to_string()),
            card: None,
            note: None,
        };
        assert_eq!(decision.fallback_reason(Device::Cuda, None), None);
        assert_eq!(decision.fallback_reason(Device::Cpu, Some(Reason::ExecutionFailed)), Some(Reason::ExecutionFailed));
        assert_eq!(decision.fallback_reason(Device::Cpu, None), None, "a chain that started at the CPU and worked has no reason");
    }

    #[test]
    fn diagnostics_are_flattened_and_cut_to_a_loggable_length() {
        let long = format!("ort said: {}\0{}", "x".repeat(400), "y".repeat(400));
        let kept = bounded_diagnostics(&long);
        assert_eq!(kept.chars().count(), MAX_DIAGNOSTICS);
        assert!(!kept.chars().any(|letter| letter.is_control()), "control characters would break one-line JSON");
        assert!(kept.starts_with("ort said: xxx"));
        assert_eq!(bounded_diagnostics("short"), "short");
    }

    /// A session stand-in for the chain tests: the chain only ever needs *a* session.
    fn loader(fails_on: Device, loads: &Arc<AtomicUsize>) -> Box<dyn FnMut(Device) -> Result<Box<()>, Fail> + Send> {
        let loads = Arc::clone(loads);
        Box::new(move |device| {
            loads.fetch_add(1, Ordering::SeqCst);
            if device == fails_on {
                return Err(Fail::internal(format!("the {device:?} session could not be made")));
            }
            Ok(Box::new(()))
        })
    }

    /// A loader that always works, for the tests that are about the run rather than the load.
    fn ok_loader(loads: &Arc<AtomicUsize>) -> Box<dyn FnMut(Device) -> Result<Box<()>, Fail> + Send> {
        let loads = Arc::clone(loads);
        Box::new(move |_device| {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(()))
        })
    }

    #[test]
    fn a_session_that_cannot_be_made_falls_back_and_says_backend_init_failed() {
        let loads = Arc::new(AtomicUsize::new(0));
        let decision = Decision {
            requested: Setting::Cuda,
            chain: vec![Device::Cuda, Device::Cpu],
            reason: None,
            device_name: Some(CARD.to_string()),
            card: None,
            note: None,
        };
        let mut cache: Option<(Device, Box<()>)> = None;
        let outcome: ChainOutcome<usize> = run_on_chain(
            &decision.chain,
            &mut cache,
            &mut loader(Device::Cuda, &loads),
            &mut |_session, _device, _progress| Attempt::Ran(7),
            &mut |_value, _message| {},
            LoadStage { at: 0.25, message: "loading the model" },
            &|| false,
        )
        .expect("the CPU can always have a job CUDA could not");
        assert_eq!(outcome.device, Device::Cpu);
        assert_eq!(outcome.value, 7);
        assert_eq!(outcome.reason, Some(Reason::BackendInitFailed));
        assert!(outcome.diagnostics.expect("the runtime said something").contains("could not be made"));
        assert_eq!(decision.fallback_reason(outcome.device, outcome.reason), Some(Reason::BackendInitFailed));
        assert_eq!(decision.name_for(outcome.device), "CPU");
        assert_eq!(loads.load(Ordering::SeqCst), 2, "one session per device in the chain");
    }

    #[test]
    fn a_run_that_fails_is_retried_once_on_the_next_device() {
        let loads = Arc::new(AtomicUsize::new(0));
        let mut cache: Option<(Device, Box<()>)> = None;
        let outcome = run_on_chain(
            &[Device::Cuda, Device::Cpu],
            &mut cache,
            &mut ok_loader(&loads),
            &mut |_session, device, _progress| {
                if device == Device::Cuda {
                    return Attempt::Failed { reason: Reason::ExecutionFailed, diagnostics: "cuda ran out of memory".to_string() };
                }
                Attempt::Ran(3)
            },
            &mut |_value, _message| {},
            LoadStage { at: 0.25, message: "loading the model" },
            &|| false,
        )
        .expect("the retry works");
        assert_eq!(outcome.device, Device::Cpu);
        assert_eq!(outcome.reason, Some(Reason::ExecutionFailed));
        assert_eq!(outcome.diagnostics.as_deref(), Some("cuda ran out of memory"));
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_job_no_device_can_run_fails_once_with_one_plain_sentence() {
        let loads = Arc::new(AtomicUsize::new(0));
        let mut cache: Option<(Device, Box<()>)> = None;
        let fail = run_on_chain(
            &[Device::Cuda, Device::Cpu],
            &mut cache,
            &mut ok_loader(&loads),
            &mut |_session, device, _progress| Attempt::<i32>::Failed { reason: Reason::ExecutionFailed, diagnostics: format!("{device:?} said no") },
            &mut |_value, _message| {},
            LoadStage { at: 0.25, message: "loading the model" },
            &|| false,
        )
        .expect_err("nothing can run it");
        assert_eq!(fail.status, Status::Internal);
        assert_eq!(fail.sentence, NO_DEVICE);
    }

    #[test]
    fn a_cancelled_job_never_starts_another_device() {
        let loads = Arc::new(AtomicUsize::new(0));
        let mut cache: Option<(Device, Box<()>)> = None;
        let fail = run_on_chain(
            &[Device::Cuda, Device::Cpu],
            &mut cache,
            &mut ok_loader(&loads),
            &mut |_session, _device, _progress| Attempt::Ran(1),
            &mut |_value, _message| {},
            LoadStage { at: 0.25, message: "loading the model" },
            &|| true,
        )
        .expect_err("a deleted job stops");
        assert_eq!(fail.sentence, CANCELLED);
        assert_eq!(loads.load(Ordering::SeqCst), 0, "the chain stopped before loading anything");
    }

    #[test]
    fn a_run_that_died_after_a_delete_is_a_cancellation_not_a_fallback() {
        let loads = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let mut cache: Option<(Device, Box<()>)> = None;
        let fail = run_on_chain(
            &[Device::Cuda, Device::Cpu],
            &mut cache,
            &mut ok_loader(&loads),
            &mut |_session, _device, _progress| {
                flag.store(true, Ordering::SeqCst);
                Attempt::<i32>::Failed { reason: Reason::ExecutionFailed, diagnostics: "terminated".to_string() }
            },
            &mut |_value, _message| {},
            LoadStage { at: 0.25, message: "loading the model" },
            &move || cancelled.load(Ordering::SeqCst),
        )
        .expect_err("the job was cancelled mid-run");
        assert_eq!(fail.sentence, CANCELLED);
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the next device is not tried for a cancellation");
    }

    #[test]
    fn a_session_is_loaded_once_and_kept_for_the_next_job() {
        let loads = Arc::new(AtomicUsize::new(0));
        let mut cache: Option<(Device, Box<()>)> = None;
        for _ in 0..3 {
            let outcome = run_on_chain(
                &[Device::Cpu],
                &mut cache,
                &mut ok_loader(&loads),
                &mut |_session, _device, _progress| Attempt::Ran(1),
                &mut |_value, _message| {},
                LoadStage { at: 0.25, message: "loading the model" },
                &|| false,
            )
            .expect("the CPU session is already there");
            assert_eq!(outcome.device, Device::Cpu);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "MG-04 item 2 still holds: one load per process");
    }

    #[test]
    fn a_fallback_keeps_the_session_that_worked_and_drops_the_other() {
        let loads = Arc::new(AtomicUsize::new(0));
        let mut cache: Option<(Device, Box<()>)> = None;
        for _ in 0..2 {
            let outcome = run_on_chain(
                &[Device::Cuda, Device::Cpu],
                &mut cache,
                &mut ok_loader(&loads),
                &mut |_session, device, _progress| {
                    if device == Device::Cuda {
                        return Attempt::<i32>::Failed { reason: Reason::ExecutionFailed, diagnostics: "no".to_string() };
                    }
                    Attempt::Ran(1)
                },
                &mut |_value, _message| {},
                LoadStage { at: 0.25, message: "loading the model" },
                &|| false,
            )
            .expect("the CPU has it both times");
            assert_eq!(outcome.device, Device::Cpu);
            assert_eq!(cache.as_ref().map(|(device, _)| *device), Some(Device::Cpu), "what stays is what worked");
        }
        assert_eq!(loads.load(Ordering::SeqCst), 4, "one session at a time, so a chain that falls back pays the CUDA load again");
    }
}
