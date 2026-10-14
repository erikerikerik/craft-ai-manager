//! The built-in fake backend MG-01 runs jobs on, so the protocol can be tested end to end before
//! any model exists (MG-02 and MG-04 bring the real ones).
//!
//! For `fill_masked_area`: outside the mask the output is the input, byte for byte; inside it the
//! colour moves towards the mean colour of the unmasked pixels, by how much of the mask covers
//! that pixel. That is not inpainting, and `model: "fake-fill"` says so in every result.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Fail, Result};
use crate::image::Frame;

/// What `/result` reports as the model, so a fake result can never be mistaken for a real one.
pub const FAKE_MODEL: &str = "fake-fill";
/// The fake backend runs on the CPU.
pub const FAKE_DEVICE: &str = "cpu";
/// Everything MG-01 does is local.
pub const FAKE_PROVIDER: &str = "local";
/// Amendment 11: progress arrives in ten steps.
pub const PROGRESS_STEPS: u32 = 10;
/// The channel index that holds alpha, when the image has one.
const ALPHA_CHANNEL: usize = 3;

/// A finished fill: the image to send back, and how long the work took.
#[derive(Clone, Debug)]
pub struct Filled {
    pub frame: Frame,
    pub elapsed_ms: u64,
}

/// Fill the masked area.
///
/// `cancel` is checked between the steps, as amendment 11 asks (the fake backend works in row
/// groups), so a `DELETE` stops a running job quickly. `progress` is called exactly ten times and
/// `step_delay` spaces those steps, which is what makes the queue and cancellation testable.
pub fn run_fill(image: &Frame, mask: &[u8], cancel: &AtomicBool, step_delay: Duration, progress: &mut dyn FnMut(f64)) -> Result<Filled> {
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
        progress(f64::from(step) / f64::from(PROGRESS_STEPS));
        if !step_delay.is_zero() && step < PROGRESS_STEPS {
            std::thread::sleep(step_delay);
        }
    }
    Ok(Filled { frame: out, elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX) })
}

/// A row index computed from a big multiple, kept inside the image.
fn clamp_row(row: u64, rows: usize) -> usize {
    usize::try_from(row).unwrap_or(rows).min(rows)
}

/// Move the covered pixels of these rows towards the mean colour.
fn blend_rows(out: &mut Frame, mask: &[u8], mean: &[u32], max: f64, rows: std::ops::Range<usize>, width: usize, channels: usize) {
    if rows.is_empty() {
        return;
    }
    let first_pixel = rows.start.saturating_mul(width);
    let last_pixel = rows.end.saturating_mul(width);
    for pixel in first_pixel..last_pixel {
        let coverage = mask.get(pixel).copied().unwrap_or(0);
        if coverage == 0 {
            continue;
        }
        let Some(base) = pixel.checked_mul(channels) else { continue };
        let weight = f64::from(coverage) / 255.0;
        for (channel, target) in mean.iter().enumerate().take(channels) {
            if channel == ALPHA_CHANNEL {
                continue;
            }
            let Some(slot) = out.samples.get_mut(base.saturating_add(channel)) else { continue };
            let source = f64::from(*slot);
            let blended = (source + (f64::from(*target) - source) * weight).round().clamp(0.0, max);
            *slot = u16::try_from(blended as i64).unwrap_or(0);
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
    use super::{FAKE_MODEL, PROGRESS_STEPS, run_fill};
    use crate::image::Frame;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// A 2x2 image: four pixels, so every mask in these tests has four entries.
    fn frame(channels: usize, bit_depth: u8, samples: &[u16]) -> Frame {
        Frame { width: 2, height: 2, channels, bit_depth, samples: samples.to_vec() }
    }

    fn quiet() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn fill(image: &Frame, mask: &[u8], steps: &mut Vec<f64>) -> Frame {
        run_fill(image, mask, &quiet(), Duration::ZERO, &mut |progress| steps.push(progress)).expect("the fill runs").frame
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
    fn a_soft_mask_blends_between() {
        let image = frame(3, 8, &[0, 0, 0, 100, 100, 100, 100, 100, 100, 100, 100, 100]);
        let out = fill(&image, &[128, 0, 0, 0], &mut Vec::new());
        assert_eq!(out.samples.first(), Some(&50), "a half-covered pixel sits halfway to the mean");
        assert_eq!(out.samples.get(3), Some(&100), "and an unmasked pixel stays as it was");
    }

    #[test]
    fn alpha_is_left_alone() {
        let image = Frame { width: 2, height: 2, channels: 4, bit_depth: 8, samples: vec![10, 20, 30, 200, 60, 70, 80, 20, 60, 70, 80, 20, 60, 70, 80, 20] };
        let out = fill(&image, &[255, 0, 0, 0], &mut Vec::new());
        assert_eq!(out.samples.first(), Some(&60), "the colour moves to the unmasked pixels' colour");
        assert_eq!(out.samples.get(3), Some(&200), "the alpha of a filled pixel is untouched");
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
        assert_eq!(soft.samples.first(), Some(&32896), "a half-covered pixel lands half way up the 16-bit range");
    }

    #[test]
    fn a_mask_shorter_than_the_image_is_refused_not_guessed() {
        let image = frame(3, 8, &[0; 12]);
        let fail = run_fill(&image, &[0, 0], &quiet(), Duration::ZERO, &mut |_| {}).expect_err("the mask is too short");
        assert!(fail.sentence.contains("mask is shorter"), "{fail}");
    }

    #[test]
    fn a_cancelled_job_stops_before_the_first_step() {
        let cancel = AtomicBool::new(true);
        let image = frame(3, 8, &[0; 12]);
        let fail = run_fill(&image, &[0, 0, 0, 0], &cancel, Duration::ZERO, &mut |_| {}).expect_err("the job is cancelled before it starts");
        assert!(fail.to_string().contains("stopped the job"), "{fail}");
        assert!(cancel.load(Ordering::Relaxed), "the flag is the caller's, so it stays set");
    }

    #[test]
    fn the_model_name_is_short_and_honest() {
        assert_eq!(FAKE_MODEL, "fake-fill");
    }
}
