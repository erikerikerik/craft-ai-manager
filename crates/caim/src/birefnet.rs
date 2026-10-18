//! The Select Subject runner (MG-10): the maths around BiRefNet, and its ONNX Runtime session.
//!
//! The tensor facts live in the fork's `agents/models/birefnet.md` (task RS-01): `input_image` is
//! `[1,3,1024,1024]` float32 and fixed, `output_image` is `[1,1,1024,1024]` float32 *logits*, and
//! the preprocessing is a bilinear stretch to 1024x1024 (the aspect ratio is not kept), RGB, /255,
//! ImageNet mean and std, NCHW planar. So a job resizes the whole image to 1024x1024, runs the
//! model, turns the logits into a matte with `sigmoid * 255`, and resizes that back to the size the
//! job sent. Everything except [OrtSession] is plain arithmetic on vectors, which is what makes it
//! testable with no model file at all - the same trick MG-04 uses for LaMa.
//!
//! Two SPEC rules shape the last step. Amendment 18: `result.png` for a mask task is 8-bit
//! grayscale at `image_size`, 255 = subject, 0 = background, and the values in between are the soft
//! edge - so the runner never thresholds. Amendment 11: the mask a job sends is still checked and
//! sized as usual; a mask task just ignores its contents, because the whole image is the question.
//!
//! Prior art, credited as amendment 18 asks: upstream PhotoCraft PR #2995 (BiRefNet-lite through
//! `tract`) and #4812 (Remove Background through ONNX Runtime) reached the same checkpoint, the same
//! pin, and the same preprocessing contract independently. Upstream PhotoCraft is MIT OR
//! Apache-2.0. #4812 also found that this graph's logits collapse on the DirectML EP, which is why
//! the catalog marks `cuda` and `coreml` untested and only `cpu` supported.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ort::environment::Environment;

use crate::backend::{Filled, Timings};
use crate::catalog::Model;
use crate::error::{Fail, Result};
use crate::image::{CHANNELS, Frame};
use crate::lama;
use crate::routing::{OUTPUT_MASK, TASK_MASK};
use crate::store;

/// The size BiRefNet takes, in pixels, on a side. Fixed by the graph, not by us.
pub const MODEL_SIZE: usize = 1024;
/// The largest side [preprocess_to] will build. The model wants [MODEL_SIZE]; the headroom is for a
/// caller that wants the same normalised planes at its own size. `size` is public API, so it is
/// bounded before it decides an allocation: `3 * size * size` f32 is 50 MB at 2048, and past the
/// point where that length wraps it is an abort rather than an error a caller can catch.
pub const MAX_PREPROCESS_SIDE: usize = 2048;
/// The longest axis the resamplers will take, in pixels. The widened tent keeps one entry per
/// covered source sample, so an axis only the caller chooses is an allocation only the caller
/// chooses: `axis_weights(u32::MAX as usize, 1)` would collect about four billion tuples and abort
/// the process, which no caller can catch. Every real call is far inside the cap - the widest axis
/// a mask job can present is that task's own `max_pixels` (1,048,576, checked in
/// `protocol::JobRequest::validate`) with a one-pixel other axis, and the model's own side is
/// [MODEL_SIZE]. Raising it is a deliberate decision, not an accident of the caller's input.
pub const MAX_RESAMPLE_AXIS: usize = 1_048_576;
/// The catalog id of the model this runner can run.
pub const BIREFNET_ID: &str = "birefnet-lite";
/// What `/result` reports when a job ran here.
pub const BIREFNET_MODEL: &str = "birefnet-lite";
/// BiRefNet runs on the CPU: no GPU execution provider is compiled in.
pub const BIREFNET_DEVICE: &str = "cpu";
/// The hardware words amendment 18 puts in `device_name`.
pub const BIREFNET_DEVICE_NAME: &str = "CPU";
/// Everything CAIM does is local.
pub const BIREFNET_PROVIDER: &str = "local";
/// The ImageNet channel means RS-01 verified against the official transforms and the export's own
/// `preprocessor_config.json`.
pub const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
/// ... and the matching standard deviations.
pub const STD: [f32; 3] = [0.229, 0.224, 0.225];

/// The stages of MG-05, and the progress each one reports.
pub const PREPARE_PROGRESS: f64 = 0.2;
pub const LOAD_PROGRESS: f64 = 0.25;
pub const RUN_PROGRESS: f64 = 0.3;
pub const RUN_END: f64 = 0.9;
/// The `message` of each stage, which is what tells PhotoCraft where a job has got to.
pub const MSG_PREPARE: &str = "Craft AI Manager is preparing the image.";
pub const MSG_LOAD: &str = "Craft AI Manager is loading the Select Subject model.";
pub const MSG_RUN: &str = "Craft AI Manager is finding the subject.";
pub const MSG_FINISH: &str = "Craft AI Manager is writing the mask.";

/// The sentence a job that was cancelled gets; `job` turns it into the `cancelled` state anyway.
const CANCELLED: &str = "Craft AI Manager stopped the job.";
/// How often the watcher thread looks for a `DELETE` while a run is going on.
const WATCH_POLL: Duration = Duration::from_millis(20);

/// The image at model size: [preprocess] with the model's own side length.
#[must_use]
pub fn preprocess(image: &Frame) -> Vec<f32> {
    preprocess_to(image, MODEL_SIZE)
}

/// The image stretched to `size` by `size` and normalised the way RS-01 verified: bilinear, aspect
/// ratio not kept, RGB, /255, ImageNet mean and std, NCHW planar. Alpha never reaches the model -
/// BiRefNet takes three planes, and the matte it answers is about the subject, not the layer.
///
/// A `size` outside `1..=MAX_PREPROCESS_SIDE` answers empty rather than allocating, because a public
/// function must not let its caller choose the size of the allocation.
#[must_use]
pub fn preprocess_to(image: &Frame, size: usize) -> Vec<f32> {
    let (Ok(width), Ok(height)) = (usize::try_from(image.width), usize::try_from(image.height)) else {
        return Vec::new();
    };
    let max = f64::from(image.max_sample()).max(1.0);
    if size == 0 || size > MAX_PREPROCESS_SIDE || width > MAX_RESAMPLE_AXIS || height > MAX_RESAMPLE_AXIS {
        return Vec::new();
    }
    // Every length here is derived from what the caller sent, so every one is checked: a wrapped
    // allocation length is an abort in a release build, which no caller can catch.
    let (Some(source_len), Some(plane_size)) = (width.checked_mul(height), size.checked_mul(size)) else {
        return Vec::new();
    };
    let Some(out_len) = plane_size.checked_mul(MEAN.len()) else {
        return Vec::new();
    };
    let mut out = vec![0.0f32; out_len];
    for (channel, (mean, std)) in MEAN.iter().zip(STD.iter()).enumerate() {
        let mut plane = vec![0.0f32; source_len];
        for row in 0..height {
            for column in 0..width {
                let at = row.saturating_mul(width).saturating_add(column).saturating_mul(image.channels).saturating_add(channel);
                let sample = image.samples.get(at).copied().unwrap_or(0);
                let normalised = (f64::from(sample) / max - f64::from(*mean)) / f64::from(*std);
                if let Some(slot) = plane.get_mut(row.saturating_mul(width).saturating_add(column)) {
                    *slot = normalised as f32;
                }
            }
        }
        let resized = resize_bilinear(&plane, width, height, size, size);
        copy_plane(&resized, &mut out, channel.saturating_mul(plane_size));
    }
    out
}

/// Copy one plane into the packed model input, starting at `start`.
fn copy_plane(plane: &[f32], out: &mut [f32], start: usize) {
    for (offset, sample) in plane.iter().enumerate() {
        if let Some(slot) = out.get_mut(start.saturating_add(offset)) {
            *slot = *sample;
        }
    }
}

/// The samples and weights each pixel of a resized axis is made of: half-pixel centres, edges
/// clamped rather than wrapped, weights that add up to one.
///
/// On an axis that shrinks the tent widens to cover every source sample the destination pixel spans,
/// which is what PIL `BILINEAR` does and what the preprocessing contract in
/// `agents/models/birefnet.md` records. A shrink is not hypothetical: the mask task's cap of
/// 1,048,576 pixels admits a 4096x256 image, so a 4096-wide axis comes down to 1024. Two taps there
/// would alias high detail straight into the model input. On an axis that grows the rule is the same
/// two samples [lama::axis_weights] uses.
///
/// An axis outside `1..=MAX_RESAMPLE_AXIS` answers empty rather than allocating: the widened tent
/// holds one entry per covered source sample, so the axes are bounded before they decide a length.
#[must_use]
pub fn axis_weights(source: usize, target: usize) -> Vec<Vec<(usize, f64)>> {
    axis_taps(source, target, true)
}

/// [axis_weights] with the tent never widened: always the two samples around the destination centre.
/// [matte] keeps these because the committed fixtures were made that way, as
/// `tests/fixtures/SOURCE.md` records, so the golden test still measures CAIM's runner against the
/// independent Python run rather than against a different resampler.
#[must_use]
pub fn axis_weights_two_tap(source: usize, target: usize) -> Vec<Vec<(usize, f64)>> {
    axis_taps(source, target, false)
}

/// The taps for one axis. `widened` is the antialiased rule; the tent's half-width is one source
/// sample when an axis grows and the reduction factor when it shrinks, which is PIL's rule too.
fn axis_taps(source: usize, target: usize, widened: bool) -> Vec<Vec<(usize, f64)>> {
    // Bounded before anything is collected: `target` is the number of tap lists and, on a shrink,
    // each of those lists holds up to `source` entries.
    if target == 0 || source > MAX_RESAMPLE_AXIS || target > MAX_RESAMPLE_AXIS {
        return Vec::new();
    }
    if source == 0 {
        return vec![Vec::new(); target];
    }
    let scale = f64::from(u32::try_from(source).unwrap_or(u32::MAX)) / f64::from(u32::try_from(target).unwrap_or(u32::MAX));
    let last = f64::from(u32::try_from(source - 1).unwrap_or(u32::MAX));
    let radius = if widened { scale.max(1.0) } else { 1.0 };
    (0..target)
        .map(|index| {
            let centre = ((f64::from(u32::try_from(index).unwrap_or(u32::MAX)) + 0.5) * scale - 0.5).clamp(0.0, last);
            let from = (centre - radius).ceil().clamp(0.0, last) as usize;
            let to = (centre + radius).floor().clamp(0.0, last) as usize;
            let taps: Vec<(usize, f64)> = (from..=to)
                .filter_map(|at| {
                    let distance = (centre - f64::from(u32::try_from(at).unwrap_or(u32::MAX))).abs();
                    (distance < radius).then_some((at, 1.0 - distance / radius))
                })
                .collect();
            let total: f64 = taps.iter().map(|(_, weight)| *weight).sum();
            if taps.is_empty() || total <= 0.0 {
                // Only an edge can leave a destination pixel with nothing to sample; the nearest
                // source sample then carries it on its own.
                return vec![(centre.round() as usize, 1.0)];
            }
            taps.into_iter().map(|(at, weight)| (at, weight / total)).collect()
        })
        .collect()
}

/// Resize one channel plane, row-major, bilinearly on both axes, widening the filter on an axis that
/// shrinks ([axis_weights]).
///
/// A plane or a target with an axis outside `1..=MAX_RESAMPLE_AXIS` answers empty rather than
/// allocating, for the same reason [axis_weights] bounds its axes.
#[must_use]
pub fn resize_bilinear(plane: &[f32], width: usize, height: usize, target_width: usize, target_height: usize) -> Vec<f32> {
    resize_plane(plane, width, height, target_width, target_height, true)
}

/// [resize_bilinear] with the two-tap rule on both axes ([axis_weights_two_tap]).
#[must_use]
pub fn resize_two_tap(plane: &[f32], width: usize, height: usize, target_width: usize, target_height: usize) -> Vec<f32> {
    resize_plane(plane, width, height, target_width, target_height, false)
}

fn resize_plane(plane: &[f32], width: usize, height: usize, target_width: usize, target_height: usize, widened: bool) -> Vec<f32> {
    if target_width == 0
        || target_height == 0
        || width > MAX_RESAMPLE_AXIS
        || height > MAX_RESAMPLE_AXIS
        || target_width > MAX_RESAMPLE_AXIS
        || target_height > MAX_RESAMPLE_AXIS
    {
        return Vec::new();
    }
    let across = resample_across(plane, height, width, target_width, widened);
    resample_down(&across, height, target_width, target_height, widened)
}

/// Resample every row of a plane from `width` to `target_width` samples.
fn resample_across(plane: &[f32], rows: usize, width: usize, target_width: usize, widened: bool) -> Vec<f32> {
    let weights = axis_taps(width, target_width, widened);
    let mut out = vec![0.0f32; rows.saturating_mul(target_width)];
    for row in 0..rows {
        let Some(row_start) = row.checked_mul(width) else { continue };
        for (target_index, taps) in weights.iter().enumerate() {
            let mut sum = 0.0f64;
            for (source_index, weight) in taps {
                if let Some(sample) = plane.get(row_start.saturating_add(*source_index)) {
                    sum += f64::from(*sample) * weight;
                }
            }
            if let Some(slot) = out.get_mut(row.saturating_mul(target_width).saturating_add(target_index)) {
                *slot = sum as f32;
            }
        }
    }
    out
}

/// Resample a plane from `rows` rows to `target_rows`; the plane already has `target_width` samples
/// a row.
fn resample_down(plane: &[f32], rows: usize, target_width: usize, target_rows: usize, widened: bool) -> Vec<f32> {
    let weights = axis_taps(rows, target_rows, widened);
    let mut out = vec![0.0f32; target_rows.saturating_mul(target_width)];
    for (target_index, taps) in weights.iter().enumerate() {
        let Some(target_start) = target_index.checked_mul(target_width) else { continue };
        for (source_index, weight) in taps {
            let Some(source_start) = source_index.checked_mul(target_width) else { continue };
            for column in 0..target_width {
                let (Some(sample), Some(slot)) = (plane.get(source_start.saturating_add(column)), out.get_mut(target_start.saturating_add(column))) else {
                    continue;
                };
                *slot = (f64::from(*slot) + f64::from(*sample) * weight) as f32;
            }
        }
    }
    out
}

/// The logistic of a logit. BiRefNet answers unbounded logits (RS-01 measured -22.6 to +61.0 on
/// this checkpoint), and the matte is the probability, not the sign.
#[must_use]
pub fn sigmoid(logit: f32) -> f32 {
    1.0 / (1.0 + (-logit).exp())
}

/// The model's answer as an 8-bit matte at the size the job asked for.
///
/// `sigmoid(logit) * 255`, rounded, at 1024x1024 first and then resized back - which is what RS-01
/// measured with, and what keeps the soft edge: amendment 18 says the values between 0 and 255 *are*
/// the feature (the hair, the anti-aliased boundary), so nothing is thresholded here.
///
/// The resize back is the two-tap one, not the antialiased one the model input gets: the committed
/// fixtures were made by a Python run that used two taps here (`tests/fixtures/SOURCE.md`), and the
/// golden test is the check against that run.
#[must_use]
pub fn matte(output: &[f32], width: usize, height: usize) -> Vec<u16> {
    let quantised: Vec<f32> = output.iter().map(|logit| (sigmoid(*logit) * 255.0).clamp(0.0, 255.0).round()).collect();
    let resized = resize_two_tap(&quantised, MODEL_SIZE, MODEL_SIZE, width, height);
    resized.iter().map(|sample| u16::try_from(sample.round().clamp(0.0, 255.0) as i64).unwrap_or(0)).collect()
}

/// The model itself, behind a trait so the stages and the maths can be tested with no model file.
pub trait Session: Send {
    /// Run one image. `image` is the packed NCHW input [preprocess] makes; the answer is the
    /// `[1,1,1024,1024]` logit plane, flat.
    fn run(&mut self, image: &[f32], cancel: &AtomicBool) -> Result<Vec<f32>>;
}

/// How a session is made: once per pipeline, on the first job that needs one.
pub type Loader = Box<dyn Fn(&Path) -> Result<Box<dyn Session>> + Send + Sync>;

/// One installed model and the session it runs on. The session is made lazily and kept, so the
/// ~3.7 s model load is paid once per process rather than once per job.
pub struct Pipeline {
    path: PathBuf,
    /// The catalog version and file hash of the model this pipeline runs, so every result can say
    /// which bytes made it (amendment 18). `None` when the pipeline was built straight from a path.
    identity: Option<Identity>,
    session: Mutex<Option<Box<dyn Session>>>,
    load: Loader,
}

/// The part of a catalog entry amendment 18 pins to a result. The model is always its distributed
/// source file: an ONNX Runtime optimised graph is a disposable artifact, never provenance.
#[derive(Clone, Debug)]
pub struct Identity {
    pub version: String,
    pub sha256: String,
}

/// A pipeline is described by its model file and whether the session has been loaded yet; the
/// loader itself is a closure, so it is left out.
impl std::fmt::Debug for Pipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Pipeline").field("path", &self.path).field("loaded", &self.session.lock().is_ok_and(|guard| guard.is_some())).finish()
    }
}

impl Pipeline {
    /// A pipeline that runs the ONNX file at `path` on ONNX Runtime.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Arc<Pipeline> {
        Self::with_identity_and_loader(path, None, Box::new(load_ort_session))
    }

    /// The same with the session factory replaced, which is how a test walks the stages with a
    /// stand-in instead of the 224 MB graph.
    #[must_use]
    pub fn with_loader(path: impl Into<PathBuf>, load: Loader) -> Arc<Pipeline> {
        Self::with_identity_and_loader(path, None, load)
    }

    /// A pipeline for a catalog entry, so its results carry the entry's version and hash.
    #[must_use]
    pub fn for_model(path: impl Into<PathBuf>, model: &Model) -> Arc<Pipeline> {
        Self::with_identity_and_loader(path, Some(Identity { version: model.version.clone(), sha256: model.sha256.clone() }), Box::new(load_ort_session))
    }

    /// `pub(crate)` so a test can hand a stand-in session the identity of a real catalog entry.
    pub(crate) fn with_identity_and_loader(path: impl Into<PathBuf>, identity: Option<Identity>, load: Loader) -> Arc<Pipeline> {
        Arc::new(Pipeline { path: path.into(), identity, session: Mutex::new(None), load })
    }

    /// A finished mask from this pipeline, with amendment 18's account of what ran.
    fn filled(&self, frame: Frame, elapsed_ms: u64, load_ms: u64, run_ms: u64) -> Filled {
        let timings = Timings { elapsed_ms, load_ms, run_ms };
        let mut filled = Filled::new(frame, timings, BIREFNET_MODEL, BIREFNET_DEVICE, BIREFNET_PROVIDER, OUTPUT_MASK);
        if let Some(identity) = self.identity.as_ref() {
            filled.model_version = Some(identity.version.clone());
            filled.model_sha256 = Some(identity.sha256.clone());
        }
        filled.device_name = Some(BIREFNET_DEVICE_NAME.to_string());
        filled
    }

    /// Find the subject of the whole image, reporting the MG-05 stages as it goes.
    ///
    /// The mask the job sent is checked by the queue like any other and ignored here: a mask task
    /// asks about the whole image, so there is no selection to honour.
    pub fn run_mask(&self, image: &Frame, _mask: &[u8], cancel: &AtomicBool, progress: &mut dyn FnMut(f64, &str)) -> Result<Filled> {
        let started = Instant::now();
        if !CHANNELS.iter().any(|channels| usize::from(*channels) == image.channels) {
            return Err(Fail::bad_request("Craft AI Manager can only read images with three or four channels."));
        }
        let (Ok(width), Ok(height)) = (usize::try_from(image.width), usize::try_from(image.height)) else {
            return Err(Fail::internal("The image is too large for this machine."));
        };
        if stopped(cancel) {
            return Err(Fail::internal(CANCELLED));
        }
        progress(0.0, MSG_PREPARE);
        let model_image = preprocess(image);
        progress(PREPARE_PROGRESS, MSG_PREPARE);
        if stopped(cancel) {
            return Err(Fail::internal(CANCELLED));
        }
        // The run's own outcome is carried out of the lock so the session can go back into the
        // guard whether the run worked or not; `map` keeps the error type concrete.
        let run = {
            let mut guard = self.session.lock().unwrap_or_else(PoisonError::into_inner);
            let mut load_ms = 0u64;
            let mut session = match guard.take() {
                Some(session) => session,
                None => {
                    progress(LOAD_PROGRESS, MSG_LOAD);
                    let loading = Instant::now();
                    let session = (self.load)(&self.path)?;
                    load_ms = elapsed_ms(&loading);
                    session
                }
            };
            if stopped(cancel) {
                *guard = Some(session);
                return Err(Fail::internal(CANCELLED));
            }
            progress(RUN_PROGRESS, MSG_RUN);
            let running = Instant::now();
            let outcome = session.run(&model_image, cancel);
            let run_ms = elapsed_ms(&running);
            *guard = Some(session);
            // A run ORT stopped because the job was cancelled is a cancellation, not a failure. The
            // session is back in the guard by now, so the next job still finds it.
            let outcome = outcome.map_err(|fail| if stopped(cancel) { Fail::internal(CANCELLED) } else { fail });
            outcome.map(|samples| (samples, load_ms, run_ms))
        };
        let (output, load_ms, run_ms) = run?;
        if stopped(cancel) {
            return Err(Fail::internal(CANCELLED));
        }
        // The graph is fixed at 1024x1024, so anything else means the file is not the one the
        // catalog promised; say so rather than resize a shape that is not there.
        if output.len() != MODEL_SIZE.saturating_mul(MODEL_SIZE) {
            return Err(Fail::internal(format!(
                "The Select Subject model gave {} samples, not the {} a {}x{} matte needs.",
                output.len(),
                MODEL_SIZE.saturating_mul(MODEL_SIZE),
                MODEL_SIZE,
                MODEL_SIZE
            )));
        }
        progress(RUN_END, MSG_FINISH);
        let frame = Frame { width: image.width, height: image.height, channels: 1, bit_depth: 8, samples: matte(&output, width, height) };
        progress(1.0, MSG_FINISH);
        Ok(self.filled(frame, elapsed_ms(&started), load_ms, run_ms))
    }
}

/// Has a `DELETE` arrived?
fn stopped(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// How long a job has been running, in whole milliseconds.
fn elapsed_ms(started: &Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The real session: an ONNX Runtime graph, held for as long as the pipeline lives.
pub struct OrtSession {
    /// Held so the environment outlives the session that was built from it.
    _environment: Arc<Environment>,
    session: ort::session::Session,
}

impl Session for OrtSession {
    fn run(&mut self, image: &[f32], cancel: &AtomicBool) -> Result<Vec<f32>> {
        let tensor = ort::value::Tensor::from_array(([1usize, 3, MODEL_SIZE, MODEL_SIZE], image.to_vec().into_boxed_slice())).map_err(ort_fail)?;
        let run_options = ort::session::RunOptions::new().map_err(ort_fail)?;
        // MG-05: a `DELETE` during the run reaches ONNX Runtime's own `terminate`, so the inference
        // stops in flight instead of waiting for its ~4 s to finish. `RunOptions` is `Sync` but not
        // `Send`, so the watcher only ever borrows it, inside a scoped thread that cannot outlive
        // this call.
        std::thread::scope(|scope| -> Result<Vec<f32>> {
            let done = Arc::new(AtomicBool::new(false));
            let watcher = {
                let (done, run_options) = (Arc::clone(&done), &run_options);
                scope.spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        if cancel.load(Ordering::Relaxed) {
                            let _ = run_options.terminate();
                            break;
                        }
                        std::thread::sleep(WATCH_POLL);
                    }
                })
            };
            let outcome = self.session.run_with_options(ort::inputs!["input_image" => tensor], &run_options);
            done.store(true, Ordering::Relaxed);
            let _ = watcher.join();
            let outputs = outcome.map_err(ort_fail)?;
            let first = outputs.values().next().ok_or_else(|| Fail::internal("The Select Subject model gave no output at all."))?;
            let (_shape, samples) = first.try_extract_tensor::<f32>().map_err(ort_fail)?;
            Ok(samples.to_vec())
        })
    }
}

/// Load the graph at `path`. This is the ~3.7 s the first job pays (RS-01: 3,638 / 3,667 / 3,787 ms
/// on the 7950X).
///
/// Decision D12: the CPU memory arena is switched **off**. RS-01 measured a 12.08 GiB peak with it
/// on and 6.40 GiB with it off, for about 4% more run time, and the catalog's `memory_mib = 6600`
/// comes from the lower figure.
pub fn load_ort_session(path: &Path) -> Result<Box<dyn Session>> {
    let environment = lama::environment()?;
    let session = ort::session::Session::builder(&environment)
        .map_err(ort_fail)?
        .with_execution_providers([ort::ep::CPU::default().with_arena_allocator(false).build()])
        .map_err(ort_fail)?
        .commit_from_file(path)
        .map_err(ort_fail)?;
    Ok(Box::new(OrtSession { _environment: environment, session }))
}

/// Turn an ONNX Runtime error into the sentence a job fails with. Generic over the recoverable
/// payload because the session builder hands back the half-built builder with the error.
fn ort_fail<T>(error: ort::Error<T>) -> Fail {
    Fail::internal(format!("The Select Subject model could not run: {error}"))
}

/// A pipeline for `model` when it is installed and acknowledged, and `None` otherwise.
///
/// "Installed" is MG-02's answer - the file, the promised size, and a `verified.json` that matches
/// it. Nothing is re-hashed here: that would cost seconds at every start.
#[must_use]
pub fn installed_pipeline(root: &Path, model: &Model) -> Option<Arc<Pipeline>> {
    if model.id != BIREFNET_ID || model.task != TASK_MASK {
        return None;
    }
    if !store::is_installed(root, model) || !store::is_acknowledged(root, model) {
        return None;
    }
    Some(Pipeline::for_model(store::model_file(root, model), model))
}

/// The pipeline `caim serve` should run mask jobs on: the installed, acknowledged BiRefNet, or
/// `None`. Same shape as [lama::ready_pipeline_in], and the same reason for it: the golden test and
/// the server both want the model behind the task, not a path of their own.
///
/// `root` replaces the model store when the caller names one.
#[must_use]
pub fn ready_pipeline_in(root: Option<&Path>) -> Option<Arc<Pipeline>> {
    let root = root.map(PathBuf::from).or_else(store::models_dir)?;
    let model = crate::catalog::load().ok()?.into_iter().find(|model| model.id == BIREFNET_ID && model.task == TASK_MASK)?;
    installed_pipeline(&root, &model)
}

/// A stand-in for the ONNX session, shared by this crate's tests: no model file is involved, so the
/// stages, the cancellation, and the queue can be tested on any machine.
#[cfg(test)]
pub(crate) struct StandIn {
    pub(crate) logit: f32,
    pub(crate) step: Duration,
    pub(crate) runs: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) len: usize,
}

#[cfg(test)]
impl Session for StandIn {
    /// Answers a flat logit plane, and notices a cancel partway through the run.
    fn run(&mut self, _image: &[f32], cancel: &AtomicBool) -> Result<Vec<f32>> {
        for _ in 0..20 {
            if cancel.load(Ordering::Relaxed) {
                return Err(Fail::internal(CANCELLED));
            }
            std::thread::sleep(self.step);
        }
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.logit; self.len])
    }
}

/// A pipeline whose session is a [StandIn]. `loads` counts how many sessions were made, so a test
/// can see the model was loaded once and then kept.
#[cfg(test)]
pub(crate) fn stand_in_pipeline(
    logit: f32,
    step: Duration,
    loads: &Arc<std::sync::atomic::AtomicUsize>,
    runs: &Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<Pipeline> {
    let (loads, runs) = (Arc::clone(loads), Arc::clone(runs));
    let load: Loader = Box::new(move |_path: &Path| {
        loads.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(StandIn { logit, step, runs: Arc::clone(&runs), len: MODEL_SIZE.saturating_mul(MODEL_SIZE) }))
    });
    Pipeline::with_loader("birefnet_lite.onnx", load)
}

#[cfg(test)]
mod tests {
    use super::{
        BIREFNET_DEVICE, BIREFNET_ID, BIREFNET_MODEL, BIREFNET_PROVIDER, Identity, LOAD_PROGRESS, Loader, MAX_PREPROCESS_SIDE, MAX_RESAMPLE_AXIS, MODEL_SIZE,
        MSG_FINISH, MSG_LOAD, MSG_PREPARE, MSG_RUN, PREPARE_PROGRESS, Pipeline, RUN_END, RUN_PROGRESS, StandIn, axis_weights, axis_weights_two_tap,
        installed_pipeline, matte, preprocess_to, resize_bilinear, resize_two_tap, sigmoid, stand_in_pipeline,
    };
    use crate::catalog::Model;
    use crate::error::Status;
    use crate::image::Frame;
    use crate::routing::{OUTPUT_MASK, TASK_MASK};
    use crate::store;
    use crate::testkit::{Scratch, test_model};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    /// A flat 8-bit frame with `channels` channels.
    fn flat(channels: usize, width: u32, height: u32, value: u16) -> Frame {
        let samples = usize::try_from(width.saturating_mul(height)).unwrap_or(0) * channels;
        Frame { width, height, channels, bit_depth: 8, samples: vec![value; samples] }
    }

    /// The catalog entry this runner accepts, made from fixture bytes.
    fn birefnet_entry(bytes: &[u8]) -> Model {
        let mut model = test_model(bytes, Some("Read the note before using this model."));
        model.id = BIREFNET_ID.to_string();
        model.task = TASK_MASK.to_string();
        model.file = "birefnet_lite.onnx".to_string();
        model
    }

    /// What RS-01 recorded: /255, then minus the mean, over the std.
    fn expected(sample: f64, mean: f64, std: f64) -> f64 {
        (sample / 255.0 - mean) / std
    }

    #[test]
    fn the_model_input_is_the_imagenet_normalisation_rs_01_recorded() {
        // A 2x2 frame: black, white, mid grey, black. At 2x2 the resize is the identity, so the
        // tensor says exactly what the normalisation did.
        let samples = vec![0, 0, 0, 255, 255, 255, 128, 128, 128, 0, 0, 0];
        let frame = Frame { width: 2, height: 2, channels: 3, bit_depth: 8, samples };
        let input = preprocess_to(&frame, 2);
        assert_eq!(input.len(), 3 * 4, "NCHW, three planes");
        let (red, green, blue) = (&input[0..4], &input[4..8], &input[8..12]);
        for (plane, mean, std) in [(red, 0.485, 0.229), (green, 0.456, 0.224), (blue, 0.406, 0.225)] {
            assert!((f64::from(plane[0]) - expected(0.0, mean, std)).abs() < 1e-6, "black: {plane:?}");
            assert!((f64::from(plane[1]) - expected(255.0, mean, std)).abs() < 1e-6, "white: {plane:?}");
            assert!((f64::from(plane[2]) - expected(128.0, mean, std)).abs() < 1e-6, "grey: {plane:?}");
        }
    }

    #[test]
    fn the_alpha_channel_never_reaches_the_model() {
        let rgb = Frame { width: 1, height: 1, channels: 3, bit_depth: 8, samples: vec![10, 20, 30] };
        let rgba = Frame { width: 1, height: 1, channels: 4, bit_depth: 8, samples: vec![10, 20, 30, 0] };
        assert_eq!(preprocess_to(&rgb, 1), preprocess_to(&rgba, 1));
    }

    #[test]
    fn a_sixteen_bit_image_is_normalised_by_its_own_ceiling() {
        let eight = Frame { width: 1, height: 1, channels: 3, bit_depth: 8, samples: vec![255, 255, 255] };
        let sixteen = Frame { width: 1, height: 1, channels: 3, bit_depth: 16, samples: vec![65535, 65535, 65535] };
        assert_eq!(preprocess_to(&eight, 1), preprocess_to(&sixteen, 1));
    }

    #[test]
    fn the_axis_weights_add_up_to_one_and_stay_inside_the_source() {
        // (4096, 1024) is the shrink a 4096x256 job sends, which is inside the mask task's cap.
        for (source, target) in [(1usize, 1usize), (3, 7), (7, 3), (1024, 1024), (4, 1), (1024, 2), (4096, 1024)] {
            let weights = axis_weights(source, target);
            assert_eq!(weights.len(), target, "{source} -> {target}");
            for taps in &weights {
                let total: f64 = taps.iter().map(|(_, weight)| *weight).sum();
                assert!((total - 1.0).abs() < 1e-9, "{source} -> {target}: {taps:?}");
                assert!(taps.iter().all(|(index, _)| *index < source), "{source} -> {target}: {taps:?}");
            }
        }
        assert!(axis_weights(4, 0).is_empty());
        assert!(axis_weights(0, 3).iter().all(Vec::is_empty), "nothing to sample from");
    }

    #[test]
    fn a_shrunk_axis_widens_the_tent_and_a_grown_one_does_not() {
        // 8 -> 4: an interior destination pixel covers four source samples, 1:3:3:1 over 8. Two taps
        // would read half the row and throw the rest away.
        assert_eq!(axis_weights(8, 4)[1], vec![(1, 0.125), (2, 0.375), (3, 0.375), (4, 0.125)]);
        assert_eq!(axis_weights_two_tap(8, 4)[1], vec![(2, 0.5), (3, 0.5)], "the matte path keeps the narrow rule the fixtures were made with");
        // Growing is the same rule either way, so both fixtures - smaller than 1024 on every axis -
        // preprocess exactly as RS-01 measured.
        assert_eq!(axis_weights(3, 7), axis_weights_two_tap(3, 7));
    }

    #[test]
    fn a_shrink_spreads_a_narrow_feature_instead_of_choosing_one_side_of_it() {
        // One bright sample in a dark row, shrunk 8 -> 4. The widened tent covers both destination
        // pixels it touches; two taps land on one of them and lose the rest, which is the aliasing
        // that would reach the model.
        let plane: Vec<f32> = (0..8).map(|at| if at == 3 { 1.0 } else { 0.0 }).collect();
        assert_eq!(resize_bilinear(&plane, 8, 1, 4, 1), vec![0.0, 0.375, 0.125, 0.0]);
        assert_eq!(resize_two_tap(&plane, 8, 1, 4, 1), vec![0.0, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn the_public_preprocess_bounds_the_size_before_allocating_it() {
        let frame = flat(3, 4, 4, 128);
        assert!(preprocess_to(&frame, 0).is_empty(), "nothing to build");
        assert!(preprocess_to(&frame, MAX_PREPROCESS_SIDE + 1).is_empty(), "a caller must not get to choose the allocation");
        assert_eq!(preprocess_to(&frame, MODEL_SIZE).len(), 3 * MODEL_SIZE * MODEL_SIZE, "the model's own size is inside the bound");
    }

    #[test]
    fn the_public_resamplers_bound_their_axes_before_allocating_taps() {
        // The widened tent holds one entry per covered source sample, so an axis only the caller
        // chooses is an allocation only the caller chooses: 4,294,967,295 -> 1 would collect about
        // four billion tuples and abort the process.
        assert!(axis_weights(MAX_RESAMPLE_AXIS + 1, 1).is_empty(), "the source axis is bounded");
        assert!(axis_weights(1, MAX_RESAMPLE_AXIS + 1).is_empty(), "the target axis is bounded");
        assert!(axis_weights_two_tap(MAX_RESAMPLE_AXIS + 1, 1).is_empty(), "the narrow rule is bounded too");
        // ... and the same path seen through the resizer, which is what the finding named.
        assert!(resize_bilinear(&[1.0f32; 4], MAX_RESAMPLE_AXIS + 1, 1, 2, 1).is_empty(), "a plane wider than the cap");
        assert!(resize_bilinear(&[1.0f32; 4], 1, 1, MAX_RESAMPLE_AXIS + 1, 1).is_empty(), "a target wider than the cap");
        assert!(resize_two_tap(&[1.0f32; 4], 1, MAX_RESAMPLE_AXIS + 1, 1, 2).is_empty());
        // A frame wider than the cap never reaches the resizer at all, so the bound is not only on
        // the two functions the finding named.
        let wide = flat(3, u32::try_from(MAX_RESAMPLE_AXIS + 1).unwrap_or(u32::MAX), 1, 128);
        assert!(preprocess_to(&wide, MODEL_SIZE).is_empty(), "the resizer is not the only way in");
        // The cap is not in the way of anything real: a mask job's widest legal image axis is
        // exactly MAX_RESAMPLE_AXIS, and the model's own side is far inside it.
        assert_eq!(axis_weights(MAX_RESAMPLE_AXIS, 1).len(), 1, "the cap itself is inside the cap");
        assert_eq!(resize_bilinear(&[0.5f32; 4], 2, 2, MAX_RESAMPLE_AXIS, 1).len(), MAX_RESAMPLE_AXIS);
    }

    #[test]
    fn a_flat_plane_stays_flat_whatever_the_direction() {
        for (width, height, target_width, target_height) in [(3usize, 2usize, 7usize, 5usize), (9, 9, 2, 4), (5, 3, 5, 3), (1, 1, 4, 4)] {
            let plane = vec![0.25f32; width.saturating_mul(height)];
            let out = resize_bilinear(&plane, width, height, target_width, target_height);
            assert_eq!(out.len(), target_width.saturating_mul(target_height));
            assert!(out.iter().all(|sample| (sample - 0.25).abs() < 1e-6), "{width}x{height} -> {target_width}x{target_height}");
        }
        assert!(resize_bilinear(&[1.0f32; 4], 2, 2, 0, 3).is_empty());
    }

    #[test]
    fn the_logistic_reaches_both_ends_without_overshooting() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
        assert!(sigmoid(-22.6) < 1e-9, "the coldest logit RS-01 measured on this checkpoint");
        assert!((sigmoid(61.0) - 1.0).abs() < 1e-6, "and the hottest");
    }

    #[test]
    fn the_matte_is_eight_bits_at_the_size_the_job_sent_and_never_thresholded() {
        let samples = matte(&vec![-1.0f32; MODEL_SIZE.saturating_mul(MODEL_SIZE)], 5, 3);
        assert_eq!(samples.len(), 15);
        // sigmoid(-1) * 255 = 68.6: an in-between value stays in between, which is the soft edge
        // amendment 18 wants. A threshold would have made it 0.
        assert!(samples.iter().all(|sample| *sample == 69), "{samples:?}");
    }

    #[test]
    fn a_hard_logit_saturates_at_the_two_ends() {
        let hot = vec![61.0f32; MODEL_SIZE.saturating_mul(MODEL_SIZE)];
        let cold = vec![-22.6f32; MODEL_SIZE.saturating_mul(MODEL_SIZE)];
        assert!(matte(&hot, 2, 2).iter().all(|sample| *sample == 255), "255 is the subject");
        assert!(matte(&cold, 2, 2).iter().all(|sample| *sample == 0), "0 is the background");
    }

    #[test]
    fn the_soft_edge_survives_the_resize_back_to_the_image_size() {
        // Subject from column 256 on. Brought down to two pixels wide, the left sample straddles the
        // boundary, so it comes back between the two rather than snapped to one of them.
        let mut output = vec![-10.0f32; MODEL_SIZE.saturating_mul(MODEL_SIZE)];
        for row in 0..MODEL_SIZE {
            for column in 256..MODEL_SIZE {
                output[row * MODEL_SIZE + column] = 10.0;
            }
        }
        assert_eq!(matte(&output, 2, 1), vec![128, 255]);
    }

    #[test]
    fn the_stages_are_reported_in_order() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(4.0, Duration::ZERO, &loads, &runs);
        let mut seen: Vec<(f64, String)> = Vec::new();
        pipeline
            .run_mask(&flat(3, 4, 2, 128), &[255u8; 8], &AtomicBool::new(false), &mut |value, message| seen.push((value, message.to_string())))
            .expect("the stand-in session runs");
        assert_eq!(seen.iter().map(|(value, _)| *value).collect::<Vec<f64>>(), [0.0, PREPARE_PROGRESS, LOAD_PROGRESS, RUN_PROGRESS, RUN_END, 1.0]);
        assert_eq!(
            seen.iter().map(|(_, message)| message.as_str()).collect::<Vec<&str>>(),
            [MSG_PREPARE, MSG_PREPARE, MSG_LOAD, MSG_RUN, MSG_FINISH, MSG_FINISH]
        );
    }

    #[test]
    fn a_cancelled_job_stops_at_the_stage_it_has_reached() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(4.0, Duration::from_millis(2), &loads, &runs);
        let fail =
            pipeline.run_mask(&flat(3, 4, 2, 128), &[255u8; 8], &AtomicBool::new(true), &mut |_, _| {}).expect_err("the job was cancelled before it started");
        assert!(fail.sentence.contains("stopped the job"), "{fail}");
        assert_eq!(loads.load(Ordering::SeqCst), 0, "a job cancelled while queued never loads the model");

        // Cancelled while the run is going on: the session notices and stops.
        let cancel = Arc::new(AtomicBool::new(false));
        let watcher = {
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(10));
                cancel.store(true, Ordering::Relaxed);
            })
        };
        let fail = pipeline.run_mask(&flat(3, 4, 2, 128), &[255u8; 8], &cancel, &mut |_, _| {}).expect_err("the run is cancelled partway through");
        watcher.join().expect("the watcher thread finishes");
        assert!(fail.sentence.contains("stopped the job"), "{fail}");
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the run never completed");
    }

    #[test]
    fn the_session_is_loaded_once_and_kept_for_the_next_job() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(4.0, Duration::ZERO, &loads, &runs);
        for _ in 0..2 {
            pipeline.run_mask(&flat(3, 4, 2, 128), &[255u8; 8], &AtomicBool::new(false), &mut |_, _| {}).expect("the stand-in session runs");
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the model is loaded once, on the first job");
        assert_eq!(runs.load(Ordering::SeqCst), 2, "and reused by the second one");
    }

    #[test]
    fn an_empty_mask_is_still_a_real_run_because_the_whole_image_is_the_question() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(4.0, Duration::ZERO, &loads, &runs);
        let filled = pipeline.run_mask(&flat(3, 4, 2, 128), &[0u8; 8], &AtomicBool::new(false), &mut |_, _| {}).expect("a mask task ignores its mask");
        assert_eq!(loads.load(Ordering::SeqCst), 1, "unlike a fill, there is something to do");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(filled.frame.samples.iter().all(|sample| *sample > 0), "{:?}", filled.frame.samples);
    }

    #[test]
    fn the_result_is_an_eight_bit_grayscale_matte_at_the_size_that_was_sent() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(4.0, Duration::ZERO, &loads, &runs);
        let filled = pipeline.run_mask(&flat(3, 3, 2, 200), &[255u8; 6], &AtomicBool::new(false), &mut |_, _| {}).expect("the stand-in session runs");
        assert_eq!((filled.frame.width, filled.frame.height), (3, 2), "the size the job sent, not 1024");
        assert_eq!(filled.frame.channels, 1);
        assert_eq!(filled.frame.bit_depth, 8);
        assert_eq!(filled.frame.samples.len(), 6);
    }

    #[test]
    fn the_mask_result_says_which_bytes_ran() {
        let model = birefnet_entry(b"the bytes a small model would be made of");
        let full = MODEL_SIZE.saturating_mul(MODEL_SIZE);
        let load: Loader = Box::new(move |_path| Ok(Box::new(StandIn { logit: 4.0, step: Duration::ZERO, runs: Arc::new(AtomicUsize::new(0)), len: full })));
        let identity = Identity { version: model.version.clone(), sha256: model.sha256.clone() };
        let pipeline = Pipeline::with_identity_and_loader("birefnet_lite.onnx", Some(identity), load);
        let filled = pipeline.run_mask(&flat(3, 2, 2, 128), &[255u8; 4], &AtomicBool::new(false), &mut |_, _| {}).expect("the stand-in session runs");
        assert_eq!(filled.kind, OUTPUT_MASK);
        assert_eq!(filled.model, BIREFNET_MODEL);
        assert_eq!(filled.device, BIREFNET_DEVICE);
        assert_eq!(filled.provider, BIREFNET_PROVIDER);
        assert_eq!(filled.device_name.as_deref(), Some("CPU"));
        assert_eq!(filled.model_version.as_deref(), Some("1"));
        assert_eq!(filled.model_sha256.as_deref(), Some(model.sha256.as_str()));
        assert_eq!(filled.fallback_reason, None, "the requested backend is the only one there is");
    }

    #[test]
    fn a_model_that_answers_the_wrong_shape_is_reported() {
        // The graph is fixed at 1024x1024, so a short answer means the file is not the one the
        // catalog promised; say so rather than resize a shape that is not there.
        let load: Loader = Box::new(|_path| Ok(Box::new(StandIn { logit: 0.0, step: Duration::ZERO, runs: Arc::new(AtomicUsize::new(0)), len: 7 })));
        let pipeline = Pipeline::with_loader("birefnet_lite.onnx", load);
        let fail = pipeline.run_mask(&flat(3, 2, 2, 128), &[255u8; 4], &AtomicBool::new(false), &mut |_, _| {}).expect_err("seven samples is not a matte");
        assert!(fail.sentence.contains("samples"), "{fail}");
    }

    #[test]
    fn an_image_that_is_not_rgb_or_rgba_is_refused_before_anything_loads() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(4.0, Duration::ZERO, &loads, &runs);
        let frame = Frame { width: 4, height: 4, channels: 1, bit_depth: 8, samples: vec![7; 16] };
        let fail = pipeline.run_mask(&frame, &[255u8; 16], &AtomicBool::new(false), &mut |_, _| {}).expect_err("grayscale is not in the contract");
        assert_eq!(fail.status, Status::BadRequest, "{fail}");
        assert_eq!(loads.load(Ordering::SeqCst), 0, "the refusal comes before the model load");
    }

    #[test]
    fn a_pipeline_needs_the_file_the_record_and_the_acknowledgment() {
        let scratch = Scratch::new("birefnet-pipeline");
        let bytes: &[u8] = b"the bytes a small model would be made of";
        let model = birefnet_entry(bytes);
        std::fs::create_dir_all(store::model_folder(&scratch.path, &model)).expect("the model folder");
        std::fs::write(store::model_file(&scratch.path, &model), bytes).expect("the model file");
        store::write_verified(&scratch.path, &model, &model.sha256, model.size).expect("the verified record");
        assert!(installed_pipeline(&scratch.path, &model).is_none(), "the note has to be acknowledged first");
        store::write_acknowledgement(&scratch.path, &model).expect("the note is acknowledged");
        assert!(installed_pipeline(&scratch.path, &model).is_some(), "installed and acknowledged makes a pipeline");
        std::fs::remove_file(store::model_file(&scratch.path, &model)).expect("the file goes away");
        assert!(installed_pipeline(&scratch.path, &model).is_none(), "a missing file is not installed");
    }

    #[test]
    fn this_runner_only_offers_the_model_it_can_run() {
        let scratch = Scratch::new("birefnet-pipeline-task");
        let bytes: &[u8] = b"the bytes a small model would be made of";
        let mut model = birefnet_entry(bytes);
        model.task = "fill_masked_area".to_string();
        std::fs::create_dir_all(store::model_folder(&scratch.path, &model)).expect("the model folder");
        std::fs::write(store::model_file(&scratch.path, &model), bytes).expect("the model file");
        store::write_verified(&scratch.path, &model, &model.sha256, model.size).expect("the verified record");
        assert!(installed_pipeline(&scratch.path, &model).is_none(), "a model for another task is not offered here");
    }
}
