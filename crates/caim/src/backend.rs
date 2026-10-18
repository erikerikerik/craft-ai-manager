//! The built-in fake backend MG-01 runs jobs on, so the protocol can be tested end to end before
//! any model exists (MG-02 and MG-04 bring the real ones).
//!
//! For `fill_masked_area`: outside the mask the output is the input, byte for byte; wherever the
//! mask is above 0 the pixel is the mean colour of the unmasked pixels at full strength, and fully
//! opaque. SPEC amendment 16 says PhotoCraft does the blending by coverage, and this fake follows
//! the same rule as LaMa so the tests see what a real job returns. That is not inpainting, and
//! `model: "fake-fill"` says so in every result.
//!
//! MG-10 adds the same thing for `subject_mask`: a centred soft ellipse, so PhotoCraft's mask tests
//! can run end to end with no model installed. `model: "fake-mask"` says so in every result.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Fail, Result};
use crate::image::Frame;
use crate::protocol::DEVICE_AUTO;
use crate::routing::{OUTPUT_IMAGE, OUTPUT_MASK};

/// What `/result` reports as the model, so a fake result can never be mistaken for a real one.
pub const FAKE_MODEL: &str = "fake-fill";
/// The same for a fake mask job (MG-10).
pub const FAKE_MASK_MODEL: &str = "fake-mask";
/// The fake backend runs on the CPU.
pub const FAKE_DEVICE: &str = "cpu";
/// The hardware words amendment 18 puts in `device_name`.
pub const FAKE_DEVICE_NAME: &str = "CPU";
/// Everything MG-01 does is local.
pub const FAKE_PROVIDER: &str = "local";
/// Amendment 11: progress arrives in ten steps.
pub const PROGRESS_STEPS: u32 = 10;
/// The `message` every step carries: the fake backend has no stages to name, only steps.
pub const FAKE_MESSAGE: &str = "Craft AI Manager is filling the masked area.";
/// The `message` a fake mask job carries instead.
pub const FAKE_MASK_MESSAGE: &str = "Craft AI Manager is finding the subject.";
/// The channel index that holds alpha, when the image has one.
const ALPHA_CHANNEL: usize = 3;
/// The fake mask's ellipse: inside this normalised radius the pixel is fully subject, outside the
/// unit circle it is fully background, and between the two the edge is soft. Amendment 18 asks for
/// soft edges in a mask result, so the fake has to produce them.
const ELLIPSE_SOLID: f64 = 0.75;
/// The soft band only exists if the solid radius is strictly inside the unit circle, so that is
/// checked at compile time rather than in a test.
const _: () = assert!(ELLIPSE_SOLID > 0.0 && ELLIPSE_SOLID < 1.0);

/// A finished fill: the image to send back, how long the work took, and what produced it.
///
/// The three labels are what `/result` reports, so a fake result can never be mistaken for a real
/// one and MG-04's real one says which model and device ran. Amendment 18 adds the rest: which kind
/// of result it is, which model version and bytes ran, what was asked for, and how long the load
/// and the run each took.
#[derive(Clone, Debug)]
pub struct Filled {
    pub frame: Frame,
    pub elapsed_ms: u64,
    pub model: String,
    pub device: String,
    pub provider: String,
    /// `image` or `mask`: the shape of `result.png`.
    pub kind: &'static str,
    /// The catalog version of the model that ran. `None` for the fake backend, which has no model.
    pub model_version: Option<String>,
    /// The SHA-256 of the model file that ran, for amendment 18's provenance.
    pub model_sha256: Option<String>,
    /// The device the caller asked for. `auto` until CAIM-04 adds a device setting.
    pub device_requested: String,
    /// The hardware that ran, in plain words. Optional: amendment 18 leaves it out when there is
    /// nothing useful to say.
    pub device_name: Option<String>,
    /// Why a backend other than the requested one ran. `None` when the requested one did.
    pub fallback_reason: Option<String>,
    /// How long the model session took to load: 0 when it was already loaded.
    pub load_ms: u64,
    /// How long the model itself took, without the preparation or the encoding.
    pub run_ms: u64,
    /// The runtime's own error text, for the log only. Amendment 18 keeps it out of the protocol's
    /// own vocabulary.
    pub diagnostics: Option<String>,
}

/// The three timings amendment 18 reports: the whole job, the model load, and the run. Grouped
/// because every runner measures all three, and a result with one of them missing is a bug.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    /// The whole job, from the moment the runner took it to the moment its answer was ready.
    pub elapsed_ms: u64,
    /// How long the model session took to load: 0 when a later job reused it.
    pub load_ms: u64,
    /// How long the model itself took, without the preparation or the encoding.
    pub run_ms: u64,
}

impl Filled {
    /// A result from a runner with one backend: `auto` was asked for and nothing fell back. The
    /// model's version and hash stay `None` until the caller says which catalog entry it ran.
    #[must_use]
    pub fn new(frame: Frame, timings: Timings, model: &str, device: &str, provider: &str, kind: &'static str) -> Self {
        Self {
            frame,
            elapsed_ms: timings.elapsed_ms,
            model: model.to_string(),
            device: device.to_string(),
            provider: provider.to_string(),
            kind,
            model_version: None,
            model_sha256: None,
            device_requested: DEVICE_AUTO.to_string(),
            device_name: None,
            fallback_reason: None,
            load_ms: timings.load_ms,
            run_ms: timings.run_ms,
            diagnostics: None,
        }
    }
}

/// Fill the masked area.
///
/// `cancel` is checked between the steps, as amendment 11 asks (the fake backend works in row
/// groups), so a `DELETE` stops a running job quickly. `progress` is called exactly ten times and
/// `step_delay` spaces those steps, which is what makes the queue and cancellation testable.
pub fn run_fill(image: &Frame, mask: &[u8], cancel: &AtomicBool, step_delay: Duration, progress: &mut dyn FnMut(f64, &str)) -> Result<Filled> {
    let started = Instant::now();
    let channels = image.channels;
    let width = usize::try_from(image.width).map_err(|_| Fail::internal("The image width does not fit this machine."))?;
    let rows = usize::try_from(image.height).map_err(|_| Fail::internal("The image height does not fit this machine."))?;
    let pixels = width.checked_mul(rows).ok_or_else(|| Fail::internal("The image is too large for this machine."))?;
    if mask.len() < pixels {
        return Err(Fail::internal("The mask is shorter than the image, which the job checks should have caught."));
    }
    let mean = mean_colour(image, mask);
    let max = f64::from(image.max_sample());
    let mut out = image.clone();
    let total_rows = u64::try_from(rows).unwrap_or(u64::MAX);
    for step in 1..=PROGRESS_STEPS {
        if cancel.load(Ordering::Relaxed) {
            return Err(Fail::internal("Craft AI Manager stopped the job."));
        }
        // The rows this step covers: ten steps over the whole image, whatever its height.
        let first_row = clamp_row(total_rows.saturating_mul(u64::from(step - 1)) / u64::from(PROGRESS_STEPS), rows);
        let last_row = clamp_row(total_rows.saturating_mul(u64::from(step)) / u64::from(PROGRESS_STEPS), rows);
        blend_rows(&mut out, mask, &mean, max, first_row..last_row, width, channels);
        progress(f64::from(step) / f64::from(PROGRESS_STEPS), FAKE_MESSAGE);
        if !step_delay.is_zero() && step < PROGRESS_STEPS {
            std::thread::sleep(step_delay);
        }
    }
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // The fake backend has no model to load, so the whole job is the "run".
    let timings = Timings { elapsed_ms, load_ms: 0, run_ms: elapsed_ms };
    let mut filled = Filled::new(out, timings, FAKE_MODEL, FAKE_DEVICE, FAKE_PROVIDER, OUTPUT_IMAGE);
    filled.device_name = Some(FAKE_DEVICE_NAME.to_string());
    Ok(filled)
}

/// Find the subject of the whole image.
///
/// The fake answer is a centred soft ellipse, which is enough for PhotoCraft's mask tests to check
/// the whole path end to end: the size, the 8-bit grayscale encoding, the soft edge, the ten
/// progress steps and the cancellation. The `mask` part is ignored, exactly as amendment 18 says a
/// mask task may ignore it (an all-255 mask means "look everywhere").
///
/// The output is one channel at 8 bits, at the size the job sent, with 255 for the subject.
pub fn run_mask(image: &Frame, _mask: &[u8], cancel: &AtomicBool, step_delay: Duration, progress: &mut dyn FnMut(f64, &str)) -> Result<Filled> {
    let started = Instant::now();
    let width = usize::try_from(image.width).map_err(|_| Fail::internal("The image width does not fit this machine."))?;
    let rows = usize::try_from(image.height).map_err(|_| Fail::internal("The image height does not fit this machine."))?;
    let pixels = width.checked_mul(rows).ok_or_else(|| Fail::internal("The image is too large for this machine."))?;
    let mut out = Frame { width: image.width, height: image.height, channels: 1, bit_depth: 8, samples: vec![0u16; pixels] };
    // A centred ellipse, 70% of the image across, whose edge fades between 75% and 100% of the
    // squared normalised radius. A one-pixel axis still gets a radius, so nothing divides by zero.
    let centre_x = (f64::from(image.width) - 1.0) / 2.0;
    let centre_y = (f64::from(image.height) - 1.0) / 2.0;
    let radius_x = (f64::from(image.width) * 0.35).max(1.0);
    let radius_y = (f64::from(image.height) * 0.35).max(1.0);
    let total_rows = u64::try_from(rows).unwrap_or(u64::MAX);
    for step in 1..=PROGRESS_STEPS {
        if cancel.load(Ordering::Relaxed) {
            return Err(Fail::internal("Craft AI Manager stopped the job."));
        }
        let first_row = clamp_row(total_rows.saturating_mul(u64::from(step - 1)) / u64::from(PROGRESS_STEPS), rows);
        let last_row = clamp_row(total_rows.saturating_mul(u64::from(step)) / u64::from(PROGRESS_STEPS), rows);
        for row in first_row..last_row {
            let dy = (axis_f64(row) - centre_y) / radius_y;
            for column in 0..width {
                let dx = (axis_f64(column) - centre_x) / radius_x;
                let distance = dx * dx + dy * dy;
                let coverage = if distance <= ELLIPSE_SOLID {
                    f64::from(u8::MAX)
                } else if distance >= 1.0 {
                    0.0
                } else {
                    (1.0 - distance) / (1.0 - ELLIPSE_SOLID) * f64::from(u8::MAX)
                };
                let Some(slot) = row.checked_mul(width).and_then(|base| base.checked_add(column)).and_then(|index| out.samples.get_mut(index)) else {
                    continue;
                };
                *slot = coverage.round().clamp(0.0, f64::from(u8::MAX)) as u16;
            }
        }
        progress(f64::from(step) / f64::from(PROGRESS_STEPS), FAKE_MASK_MESSAGE);
        if !step_delay.is_zero() && step < PROGRESS_STEPS {
            std::thread::sleep(step_delay);
        }
    }
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let timings = Timings { elapsed_ms, load_ms: 0, run_ms: elapsed_ms };
    let mut filled = Filled::new(out, timings, FAKE_MASK_MODEL, FAKE_DEVICE, FAKE_PROVIDER, OUTPUT_MASK);
    filled.device_name = Some(FAKE_DEVICE_NAME.to_string());
    Ok(filled)
}

/// A row index computed from a big multiple, kept inside the image.
fn clamp_row(row: u64, rows: usize) -> usize {
    usize::try_from(row).unwrap_or(rows).min(rows)
}

/// A row or column index as a coordinate. The counts come from `usize` arithmetic; the model of the
/// ellipse is in floating point.
fn axis_f64(index: usize) -> f64 {
    f64::from(u32::try_from(index).unwrap_or(u32::MAX))
}

/// Give the covered pixels of these rows the mean colour at full strength, and make them opaque.
fn blend_rows(out: &mut Frame, mask: &[u8], mean: &[u32], max: f64, rows: std::ops::Range<usize>, width: usize, channels: usize) {
    if rows.is_empty() {
        return;
    }
    let first_pixel = rows.start.saturating_mul(width);
    let last_pixel = rows.end.saturating_mul(width);
    for pixel in first_pixel..last_pixel {
        if mask.get(pixel).copied().unwrap_or(0) == 0 {
            continue;
        }
        let Some(base) = pixel.checked_mul(channels) else { continue };
        for channel in 0..channels {
            let Some(slot) = out.samples.get_mut(base.saturating_add(channel)) else { continue };
            let target = if channel >= ALPHA_CHANNEL { max } else { mean.get(channel).map_or(f64::from(*slot), |value| f64::from(*value)) };
            *slot = u16::try_from(target.round().clamp(0.0, max) as i64).unwrap_or(0);
        }
    }
}

/// The mean colour of the unmasked pixels, rounded. If everything is masked, the mean of the whole
/// image is used, so the answer is still a colour from the picture rather than black.
fn mean_colour(image: &Frame, mask: &[u8]) -> Vec<u32> {
    let (mut sums, mut count) = mean_sum(image, Some(mask));
    if count == 0 {
        let (all, total) = mean_sum(image, None);
        sums = all;
        count = total;
    }
    if count == 0 {
        return vec![0; image.channels];
    }
    sums.iter().map(|sum| u32::try_from(sum.saturating_add(count / 2) / count).unwrap_or(u32::MAX)).collect()
}

/// The per-channel sums, and how many pixels went into them.
fn mean_sum(image: &Frame, mask: Option<&[u8]>) -> (Vec<u64>, u64) {
    let mut sums = vec![0u64; image.channels];
    let mut count = 0u64;
    for pixel in 0..image.pixel_count() {
        if let Some(mask) = mask
            && mask.get(pixel).copied().unwrap_or(0) != 0
        {
            continue;
        }
        count = count.saturating_add(1);
        for (channel, sum) in sums.iter_mut().enumerate() {
            if let Some(sample) = image.sample(pixel, channel) {
                *sum = sum.saturating_add(u64::from(sample));
            }
        }
    }
    (sums, count)
}

#[cfg(test)]
mod tests {
    use super::{FAKE_MASK_MESSAGE, FAKE_MASK_MODEL, FAKE_MESSAGE, FAKE_MODEL, PROGRESS_STEPS, run_fill, run_mask};
    use crate::image::Frame;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use crate::routing::{OUTPUT_IMAGE, OUTPUT_MASK};

    /// A 2x2 image: four pixels, so every mask in these tests has four entries.
    fn frame(channels: usize, bit_depth: u8, samples: &[u16]) -> Frame {
        Frame { width: 2, height: 2, channels, bit_depth, samples: samples.to_vec() }
    }

    fn quiet() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn fill(image: &Frame, mask: &[u8], steps: &mut Vec<f64>) -> Frame {
        run_fill(image, mask, &quiet(), Duration::ZERO, &mut |progress, _| steps.push(progress)).expect("the fill runs").frame
    }

    #[test]
    fn outside_the_mask_nothing_changes_and_inside_it_the_mean_wins() {
        let image = frame(3, 8, &[10, 20, 30, 200, 100, 50, 40, 60, 80, 90, 20, 10]);
        let mut steps = Vec::new();
        let out = fill(&image, &[255, 0, 0, 0], &mut steps);
        assert_eq!(out.width, 2);
        assert_eq!(out.bit_depth, 8);
        // Pixels 1, 2 and 3 are unmasked: identical, sample for sample.
        for sample in 3..12 {
            assert_eq!(out.samples.get(sample), image.samples.get(sample), "sample {sample} changed");
        }
        // Pixel 0 is fully masked: the mean of the other three pixels.
        assert_eq!(out.samples.first(), Some(&110), "red is the mean of 200, 40 and 90");
        assert_eq!(out.samples.get(1), Some(&60), "green is the mean of 100, 60 and 20");
        assert_eq!(out.samples.get(2), Some(&47), "blue is the mean of 50, 80 and 10");
        assert_eq!(steps.len(), PROGRESS_STEPS as usize, "progress comes in ten steps");
        assert_eq!(steps.first().copied(), Some(0.1));
        assert_eq!(steps.last().copied(), Some(1.0));
    }

    #[test]
    fn a_soft_mask_takes_the_fill_at_full_strength() {
        // Amendment 16: PhotoCraft blends by coverage, so the fake must not blend as well.
        let image = frame(3, 8, &[0, 0, 0, 100, 100, 100, 100, 100, 100, 100, 100, 100]);
        let out = fill(&image, &[128, 0, 0, 0], &mut Vec::new());
        assert_eq!(out.samples.first(), Some(&100), "a half-covered pixel takes the whole mean");
        let faint = fill(&image, &[1, 0, 0, 0], &mut Vec::new());
        assert_eq!(faint.samples.first(), Some(&100), "and so does the faintest coverage");
        assert_eq!(out.samples.get(3), Some(&100), "and an unmasked pixel stays as it was");
    }

    #[test]
    fn alpha_inside_the_mask_becomes_opaque() {
        let image = Frame { width: 2, height: 2, channels: 4, bit_depth: 8, samples: vec![10, 20, 30, 200, 60, 70, 80, 20, 60, 70, 80, 20, 60, 70, 80, 20] };
        let out = fill(&image, &[128, 0, 0, 0], &mut Vec::new());
        assert_eq!(out.samples.first(), Some(&60), "the colour is the unmasked pixels' colour");
        assert_eq!(out.samples.get(3), Some(&255), "a filled pixel becomes opaque, as LaMa's do");
        assert_eq!(out.samples.get(7), Some(&20), "an unmasked pixel keeps its alpha");
    }

    #[test]
    fn a_fully_masked_image_still_gets_a_colour_from_itself() {
        let image = frame(3, 8, &[10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120]);
        let out = fill(&image, &[255, 255, 255, 255], &mut Vec::new());
        assert_eq!(&out.samples[0..3], &[55, 65, 75], "the mean of every pixel");
    }

    #[test]
    fn sixteen_bit_images_keep_their_range() {
        let image = frame(3, 16, &[0, 0, 0, 65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535]);
        let out = fill(&image, &[255, 0, 0, 0], &mut Vec::new());
        assert_eq!(out.bit_depth, 16);
        assert_eq!(out.samples.first(), Some(&65535), "a fully masked pixel takes the mean exactly");
        let soft = fill(&image, &[128, 0, 0, 0], &mut Vec::new());
        assert_eq!(soft.samples.first(), Some(&65535), "a half-covered pixel takes the whole mean too");
    }

    #[test]
    fn a_mask_shorter_than_the_image_is_refused_not_guessed() {
        let image = frame(3, 8, &[0; 12]);
        let fail = run_fill(&image, &[0, 0], &quiet(), Duration::ZERO, &mut |_, _| {}).expect_err("the mask is too short");
        assert!(fail.sentence.contains("mask is shorter"), "{fail}");
    }

    #[test]
    fn a_cancelled_job_stops_before_the_first_step() {
        let cancel = AtomicBool::new(true);
        let image = frame(3, 8, &[0; 12]);
        let fail = run_fill(&image, &[0, 0, 0, 0], &cancel, Duration::ZERO, &mut |_, _| {}).expect_err("the job is cancelled before it starts");
        assert!(fail.to_string().contains("stopped the job"), "{fail}");
        assert!(cancel.load(Ordering::Relaxed), "the flag is the caller's, so it stays set");
    }

    #[test]
    fn the_model_name_is_short_and_honest() {
        assert_eq!(FAKE_MODEL, "fake-fill");
        assert_eq!(FAKE_MASK_MODEL, "fake-mask");
        assert_ne!(FAKE_MESSAGE, FAKE_MASK_MESSAGE, "a running mask job says something different");
    }

    /// The fake mask of a `side` x `side` image, as one channel per pixel.
    fn ellipse(side: u32) -> Vec<u16> {
        let image = Frame {
            width: side,
            height: side,
            channels: 3,
            bit_depth: 8,
            samples: vec![7u16; usize::try_from(side).expect("a small side") * usize::try_from(side).expect("a small side") * 3],
        };
        let all_masked = vec![255u8; usize::try_from(side).expect("a small side") * usize::try_from(side).expect("a small side")];
        let out = run_mask(&image, &all_masked, &quiet(), Duration::ZERO, &mut |_, _| {}).expect("the mask runs").frame;
        assert_eq!(out.channels, 1);
        assert_eq!(out.bit_depth, 8);
        out.samples
    }

    #[test]
    fn the_fake_mask_is_a_soft_ellipse_at_the_size_that_was_sent() {
        let steps = &mut Vec::new();
        let image = Frame { width: 4, height: 2, channels: 3, bit_depth: 8, samples: vec![9u16; 24] };
        let filled = run_mask(&image, &[255; 8], &quiet(), Duration::ZERO, &mut |progress, message| {
            steps.push(progress);
            assert_eq!(message, FAKE_MASK_MESSAGE);
        })
        .expect("the mask runs");
        assert_eq!(filled.kind, OUTPUT_MASK);
        assert_eq!(filled.model, FAKE_MASK_MODEL);
        let out = filled.frame;
        assert_eq!([out.width, out.height], [4, 2], "the result is at image_size, not at the model's size");
        assert_eq!(out.channels, 1, "a mask is one channel");
        assert_eq!(out.bit_depth, 8, "and always 8-bit, whatever the job sent");
        assert_eq!(out.samples.len(), 8);
        assert_eq!(steps.len(), PROGRESS_STEPS as usize);
        // The centre of a 4x2 image is inside the ellipse; the corners are outside it.
        assert_eq!(out.samples.get(5), Some(&255), "a pixel near the centre is the subject");
        assert_eq!(out.samples.first(), Some(&0), "the top-left corner is background");
        assert_eq!(out.samples.last(), Some(&0), "and so is the bottom-right corner");
    }

    #[test]
    fn the_fake_mask_edge_is_neither_all_or_nothing() {
        // Amendment 18 wants "between = soft edge", so a big enough ellipse has to show it.
        let samples = ellipse(41);
        let middle = samples.get(20 * 41 + 20).copied().expect("the centre pixel");
        assert_eq!(middle, 255, "the centre is fully subject");
        let soft = samples.iter().filter(|value| **value > 0 && **value < 255).count();
        assert!(soft > 0, "the edge between 75% and 100% of the radius is soft");
        assert!(samples.iter().all(|value| *value <= 255), "a mask never goes over 255");
        // Reading a row left to right, the coverage only ever falls as you move away from the centre.
        let row: Vec<u16> = samples.iter().skip(20 * 41).take(41).copied().collect();
        let half = 20;
        assert_eq!(row.get(half), Some(&255));
        assert!(row[..half].windows(2).all(|pair| pair[0] <= pair[1]), "the left half gets brighter towards the centre");
        assert!(row[half + 1..].windows(2).all(|pair| pair[0] >= pair[1]), "and the right half gets darker away from it");
    }

    #[test]
    fn the_fake_mask_is_deterministic_and_cancels() {
        assert_eq!(ellipse(41), ellipse(41), "the same image always gives the same matte");
        let image = Frame { width: 2, height: 2, channels: 3, bit_depth: 8, samples: vec![0u16; 12] };
        let fail = run_mask(&image, &[0, 0, 0, 0], &AtomicBool::new(true), Duration::ZERO, &mut |_, _| {}).expect_err("cancelled before the first step");
        assert!(fail.to_string().contains("stopped the job"), "{fail}");
    }

    #[test]
    fn a_one_pixel_image_still_gets_a_mask() {
        // The radii are clamped, so the maths cannot divide by zero on a degenerate image.
        let image = Frame { width: 1, height: 1, channels: 3, bit_depth: 8, samples: vec![3u16; 3] };
        let out = run_mask(&image, &[255], &quiet(), Duration::ZERO, &mut |_, _| {}).expect("the mask runs").frame;
        assert_eq!(out.samples, vec![255], "the only pixel is the centre of the ellipse");
        assert_ne!(OUTPUT_IMAGE, OUTPUT_MASK, "a mask result is not an image result");
    }
}
