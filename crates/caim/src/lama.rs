//! The LaMa runner (MG-04): the maths around the model, and the ONNX Runtime session itself.
//!
//! The tensor facts live in `docs/models/lama.md`: `image` is RGB 0-1, `mask` is 1 = fill, the
//! output is RGB 0-255, and the size is fixed at 512x512. So a job crops the mask box plus padding,
//! resizes the crop to 512, runs the model, resizes the answer back, and puts it in. Everything
//! except [OrtSession] is plain arithmetic on vectors, which is what makes it testable with no model
//! file at all (MG-04 item 6).
//!
//! Two SPEC rules shape the last step. Amendment 11: pixels outside the mask come back bit for bit,
//! which is why it uses the *original* mask only - never the dilated mask the model sees, which
//! exists to make LaMa repaint the edge of the selection as well. Amendment 16: where the mask is
//! above 0 the fill goes in at full strength, because PhotoCraft does the blending by coverage.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ort::environment::Environment;

use crate::backend::Filled;
use crate::catalog::{self, Model};
use crate::error::{Fail, Result};
use crate::image::{CHANNELS, Frame};
use crate::protocol::TASK_FILL;
use crate::store;

/// The size LaMa takes, in pixels, on a side. Fixed by the graph, not by us.
pub const MODEL_SIZE: usize = 512;
/// How far the model mask is grown, at 512 scale, so the edge of the selection is repainted too.
pub const DILATE_PX: usize = 4;
/// The smallest padding around the mask box, in image pixels.
pub const MIN_PAD: usize = 32;
/// The other padding rule: this percent of the box's longer side.
pub const PAD_PERCENT: usize = 25;
/// The catalog id of the model this runner can run.
pub const LAMA_ID: &str = "lama";
/// What `/result` reports when a job ran here.
pub const LAMA_MODEL: &str = "lama";
/// LaMa runs on the CPU: no GPU execution provider is compiled in.
pub const LAMA_DEVICE: &str = "cpu";
/// Everything CAIM does is local.
pub const LAMA_PROVIDER: &str = "local";

/// The stages of MG-05, and the progress each one reports.
pub const PREPARE_PROGRESS: f64 = 0.2;
pub const LOAD_PROGRESS: f64 = 0.25;
pub const RUN_PROGRESS: f64 = 0.3;
pub const RUN_END: f64 = 0.9;
/// The `message` of each stage, which is what tells PhotoCraft where a job has got to.
pub const MSG_PREPARE: &str = "Craft AI Manager is preparing the image.";
pub const MSG_LOAD: &str = "Craft AI Manager is loading the LaMa model.";
pub const MSG_RUN: &str = "Craft AI Manager is running LaMa.";
pub const MSG_FINISH: &str = "Craft AI Manager is putting the fill back.";

/// The sentence a job that was cancelled gets; `job` turns it into the `cancelled` state anyway.
const CANCELLED: &str = "Craft AI Manager stopped the job.";
/// How often the watcher thread looks for a `DELETE` while a run is going on.
const WATCH_POLL: Duration = Duration::from_millis(20);
/// The channel index that holds alpha, when the image has one.
const ALPHA_CHANNEL: usize = 3;

/// A rectangle inside an image. `width` and `height` are counts, so the far edge is `x + width`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Window {
    /// The index of one pixel of the whole image this window sits in.
    fn pixel(self, row: usize, column: usize, image_width: usize) -> Option<usize> {
        let (x, y) = (self.x.checked_add(column)?, self.y.checked_add(row)?);
        y.checked_mul(image_width)?.checked_add(x)
    }
}

/// The box around every pixel the mask covers, or `None` when the mask covers nothing.
#[must_use]
pub fn mask_box(width: usize, height: usize, mask: &[u8]) -> Option<Window> {
    let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for row in 0..height {
        for column in 0..width {
            let covered = mask_box_covers(mask, row, column, width);
            if !covered {
                continue;
            }
            left = left.min(column);
            top = top.min(row);
            right = right.max(column.saturating_add(1));
            bottom = bottom.max(row.saturating_add(1));
        }
    }
    if right <= left || bottom <= top {
        return None;
    }
    Some(Window { x: left, y: top, width: right - left, height: bottom - top })
}

/// Does the mask cover one pixel? A mask shorter than the image counts as clear, which the job's own
/// checks are meant to prevent.
fn mask_box_covers(mask: &[u8], row: usize, column: usize, width: usize) -> bool {
    match row.checked_mul(width).and_then(|at| at.checked_add(column)) {
        Some(at) => mask.get(at).copied().unwrap_or(0) > 0,
        None => false,
    }
}

/// The padding around a mask box: at least 32 px, and at least 25% of the box's longer side.
#[must_use]
pub fn pad_for(longer_side: usize) -> usize {
    MIN_PAD.max(longer_side.saturating_mul(PAD_PERCENT) / 100)
}

/// The crop to send to the model: the box plus its padding, grown towards a square, and always
/// clamped to the image. The box is inside the result whatever happens.
///
/// Each side is clamped on its own (C5): a wide box in a short image gets the image's full height
/// rather than falling back to the thin padded box, so the 512x512 resize squashes it less.
#[must_use]
pub fn crop_window(mask: Window, image_width: usize, image_height: usize) -> Window {
    let pad = pad_for(mask.width.max(mask.height));
    let left = mask.x.saturating_sub(pad);
    let top = mask.y.saturating_sub(pad);
    let right = mask.x.saturating_add(mask.width).saturating_add(pad).min(image_width);
    let bottom = mask.y.saturating_add(mask.height).saturating_add(pad).min(image_height);
    let (wide, tall) = (right.saturating_sub(left), bottom.saturating_sub(top));
    let side = wide.max(tall);
    let (width, height) = (side.min(image_width).max(wide), side.min(image_height).max(tall));
    Window { x: centred(left, wide, width, image_width), y: centred(top, tall, height, image_height), width, height }
}

/// Where a span of `size` starts when it is centred on `[start, start + span)` and kept inside
/// `0..limit`. `size` is at least `span`, so the original span stays inside it.
fn centred(start: usize, span: usize, size: usize, limit: usize) -> usize {
    let centre = start.saturating_add(span / 2);
    centre.saturating_sub(size / 2).min(limit.saturating_sub(size))
}

/// Which filter one axis uses: area when the axis shrinks, bilinear when it grows (MG-04 item 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Area,
    Bilinear,
}

#[must_use]
pub const fn filter_for(source: usize, target: usize) -> Filter {
    if target < source { Filter::Area } else { Filter::Bilinear }
}

/// The source indices and weights that build each pixel of a resized axis. Weights always add up to
/// one, so a flat area stays flat whatever the direction, and the edges are clamped rather than
/// wrapped or extrapolated.
#[must_use]
pub fn axis_weights(source: usize, target: usize) -> Vec<Vec<(usize, f64)>> {
    if target == 0 {
        return Vec::new();
    }
    if source == 0 {
        return vec![Vec::new(); target];
    }
    let scale = f64::from(u32::try_from(source).unwrap_or(u32::MAX)) / f64::from(u32::try_from(target).unwrap_or(u32::MAX));
    match filter_for(source, target) {
        Filter::Bilinear => (0..target)
            .map(|target_index| {
                let centre = ((f64::from(u32::try_from(target_index).unwrap_or(u32::MAX)) + 0.5) * scale - 0.5)
                    .clamp(0.0, f64::from(u32::try_from(source - 1).unwrap_or(u32::MAX)));
                let before = centre.floor() as usize;
                let after = (before + 1).min(source - 1);
                let fraction = centre - before as f64;
                weight_pair(before, after, fraction)
            })
            .collect(),
        Filter::Area => (0..target)
            .map(|target_index| {
                let start = f64::from(u32::try_from(target_index).unwrap_or(u32::MAX)) * scale;
                let end = (f64::from(u32::try_from(target_index).unwrap_or(u32::MAX)) + 1.0) * scale;
                let first = start.floor().max(0.0) as usize;
                let last = end.ceil().min(source as f64) as usize;
                let span = (end - start).max(f64::EPSILON);
                let mut list: Vec<(usize, f64)> = Vec::new();
                for source_index in first..last.max(first.saturating_add(1)) {
                    let overlap = end.min((source_index + 1) as f64) - start.max(source_index as f64);
                    if overlap > 0.0 {
                        list.push((source_index, overlap / span));
                    }
                }
                if list.is_empty() {
                    list.push((first.min(source - 1), 1.0));
                }
                list
            })
            .collect(),
    }
}

/// One pair of bilinear weights, merged when both ends are the same source pixel.
fn weight_pair(before: usize, after: usize, fraction: f64) -> Vec<(usize, f64)> {
    if before == after {
        return vec![(before, 1.0)];
    }
    vec![(before, 1.0 - fraction), (after, fraction)]
}

/// Compute a growing axis on demand: a panorama must not allocate a weight Vec per pixel.
fn bilinear_pair(source: usize, target: usize, index: usize) -> (usize, usize, f64) {
    let scale = source as f64 / target as f64;
    let centre = ((index as f64 + 0.5) * scale - 0.5).clamp(0.0, (source - 1) as f64);
    let before = centre.floor() as usize;
    (before, before.saturating_add(1).min(source - 1), centre - before as f64)
}

/// Resize one channel plane, row-major, area for a shrink and bilinear for a growth.
#[must_use]
pub fn resize_plane(plane: &[f32], width: usize, height: usize, target_width: usize, target_height: usize) -> Vec<f32> {
    if target_width == 0 || target_height == 0 {
        return Vec::new();
    }
    // Shrink first when the axes move in opposite directions. Always going across first made
    // a 512-square output grow to 512 x a million before shrinking it to a million x 1.
    if height.saturating_mul(target_width) <= target_height.saturating_mul(width) {
        #[cfg(test)]
        tests::record_intermediate(height.saturating_mul(target_width));
        let across = resample_across(plane, height, width, target_width);
        resample_down(&across, height, target_width, target_height)
    } else {
        #[cfg(test)]
        tests::record_intermediate(target_height.saturating_mul(width));
        let down = resample_down(plane, height, width, target_height);
        resample_across(&down, target_height, width, target_width)
    }
}

/// Resample every row of a plane from `width` to `target_width` samples.
fn resample_across(plane: &[f32], rows: usize, width: usize, target_width: usize) -> Vec<f32> {
    if target_width >= width && width > 0 {
        let mut out = vec![0.0; rows.saturating_mul(target_width)];
        for row in 0..rows {
            for column in 0..target_width {
                let (before, after, weight) = bilinear_pair(width, target_width, column);
                let at = row.saturating_mul(width);
                let first = plane.get(at.saturating_add(before)).copied().unwrap_or(0.0);
                let last = plane.get(at.saturating_add(after)).copied().unwrap_or(0.0);
                if let Some(slot) = out.get_mut(row.saturating_mul(target_width).saturating_add(column)) {
                    *slot = (f64::from(first) * (1.0 - weight) + f64::from(last) * weight) as f32;
                }
            }
        }
        return out;
    }
    let weights = axis_weights(width, target_width);
    let mut out = vec![0.0f32; rows.saturating_mul(target_width)];
    for row in 0..rows {
        let Some(row_start) = row.checked_mul(width) else { continue };
        for (target_index, list) in weights.iter().enumerate() {
            let mut sum = 0.0;
            for (source_index, weight) in list {
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

/// Resample a plane from `rows` rows to `target_rows`; the plane already has `target_width` samples a row.
fn resample_down(plane: &[f32], rows: usize, target_width: usize, target_rows: usize) -> Vec<f32> {
    if target_rows >= rows && rows > 0 {
        let mut out = vec![0.0; target_rows.saturating_mul(target_width)];
        for row in 0..target_rows {
            let (before, after, weight) = bilinear_pair(rows, target_rows, row);
            for column in 0..target_width {
                let first = plane.get(before.saturating_mul(target_width).saturating_add(column)).copied().unwrap_or(0.0);
                let last = plane.get(after.saturating_mul(target_width).saturating_add(column)).copied().unwrap_or(0.0);
                if let Some(slot) = out.get_mut(row.saturating_mul(target_width).saturating_add(column)) {
                    // Match the existing two f32 accumulation steps used by the area path.
                    let first = (f64::from(first) * (1.0 - weight)) as f32;
                    *slot = (f64::from(first) + f64::from(last) * weight) as f32;
                }
            }
        }
        return out;
    }
    let weights = axis_weights(rows, target_rows);
    let mut out = vec![0.0f32; target_rows.saturating_mul(target_width)];
    for (target_index, list) in weights.iter().enumerate() {
        for (source_index, weight) in list {
            let Some(source_start) = source_index.checked_mul(target_width) else { continue };
            let Some(target_start) = target_index.checked_mul(target_width) else { continue };
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

/// The mask of the crop at model size: `mask > 0` becomes 1.0.
///
/// When an axis shrinks, a model pixel is covered if **any** source pixel in its area is (a
/// max-pool, C2), so a 1-2 px stroke such as a wire or a power line still shows LaMa a hole after a
/// 3000 px crop is brought down to 512. When an axis grows, each model pixel samples the one source
/// pixel under its centre. Done one axis at a time, so a shrink reads each source pixel about once.
#[must_use]
pub fn mask_plane(mask: &[u8], window: Window, image_width: usize, size: usize) -> Vec<f32> {
    if window.height > window.width {
        #[cfg(test)]
        tests::record_intermediate(size.saturating_mul(window.width));
        // Pool vertically first for tall narrow images; height * model_size can otherwise
        // dwarf both the image and the model (the same aspect-ratio trap as RGB resizing).
        let mut down = vec![false; size.saturating_mul(window.width)];
        for row in 0..size {
            let (first, last) = source_span(row, window.height, size);
            for column in 0..window.width {
                let covered = (first..last).any(|source_row| window.pixel(source_row, column, image_width).and_then(|at| mask.get(at)).is_some_and(|v| *v > 0));
                if let Some(slot) = down.get_mut(row.saturating_mul(window.width).saturating_add(column)) {
                    *slot = covered;
                }
            }
        }
        let mut out = vec![0.0; size.saturating_mul(size)];
        for row in 0..size {
            for column in 0..size {
                let (first, last) = source_span(column, window.width, size);
                let covered = (first..last).any(|x| down.get(row.saturating_mul(window.width).saturating_add(x)).copied().unwrap_or(false));
                if covered && let Some(slot) = out.get_mut(row.saturating_mul(size).saturating_add(column)) {
                    *slot = 1.0;
                }
            }
        }
        return out;
    }
    #[cfg(test)]
    tests::record_intermediate(window.height.saturating_mul(size));
    let columns: Vec<(usize, usize)> = (0..size).map(|column| source_span(column, window.width, size)).collect();
    // Across first: for every source row of the window, which model columns it covers.
    let mut across = vec![false; window.height.saturating_mul(size)];
    for row in 0..window.height {
        let Some(row_start) = window.y.checked_add(row).and_then(|y| y.checked_mul(image_width)).and_then(|at| at.checked_add(window.x)) else {
            continue;
        };
        for (column, (first, last)) in columns.iter().enumerate() {
            let covered = (*first..*last).any(|x| mask.get(row_start.saturating_add(x)).copied().unwrap_or(0) > 0);
            if covered && let Some(slot) = across.get_mut(row.saturating_mul(size).saturating_add(column)) {
                *slot = true;
            }
        }
    }
    // Then down: a model pixel is covered if any of its source rows covers its column.
    let mut out = vec![0.0f32; size.saturating_mul(size)];
    for row in 0..size {
        let (first, last) = source_span(row, window.height, size);
        for column in 0..size {
            let covered = (first..last).any(|source_row| across.get(source_row.saturating_mul(size).saturating_add(column)).copied().unwrap_or(false));
            if covered && let Some(slot) = out.get_mut(row.saturating_mul(size).saturating_add(column)) {
                *slot = 1.0;
            }
        }
    }
    out
}

/// The source pixels, `first..last` inside the window, that one model pixel stands for on one
/// axis: every pixel of its area when the axis shrinks, the one under its centre when it grows.
fn source_span(target: usize, window_size: usize, size: usize) -> (usize, usize) {
    if window_size == 0 || size == 0 {
        return (0, 0);
    }
    if window_size <= size {
        let at = sample_in_window(target, window_size, size);
        return (at, at.saturating_add(1));
    }
    let (target, whole, size) = (target as u64, window_size as u64, size as u64);
    let first = target.saturating_mul(whole) / size;
    let last = target.saturating_add(1).saturating_mul(whole).saturating_add(size - 1) / size;
    let last = last.max(first.saturating_add(1)).min(whole);
    (usize::try_from(first).unwrap_or(window_size), usize::try_from(last).unwrap_or(window_size))
}

/// Which sample of a window one model pixel stands for.
fn sample_in_window(target: usize, window_size: usize, size: usize) -> usize {
    if window_size == 0 || size == 0 {
        return 0;
    }
    let centre = (f64::from(u32::try_from(target).unwrap_or(u32::MAX)) + 0.5) * f64::from(u32::try_from(window_size).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(size).unwrap_or(u32::MAX));
    centre.floor().clamp(0.0, f64::from(u32::try_from(window_size - 1).unwrap_or(u32::MAX))) as usize
}

/// Grow a square mask plane by `radius` pixels in every direction (a square structuring element,
/// done as two passes). This is the model mask only; the blend never sees it.
#[must_use]
pub fn dilate(plane: &[f32], size: usize, radius: usize) -> Vec<f32> {
    if size == 0 || radius == 0 {
        return plane.to_vec();
    }
    let across = dilate_across(plane, size, radius);
    dilate_down(&across, size, radius)
}

fn dilate_across(plane: &[f32], size: usize, radius: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; plane.len()];
    for row in 0..size {
        for column in 0..size {
            let mut best = 0.0f32;
            for offset in 0..=2 * radius {
                let source = column.saturating_add(offset).checked_sub(radius);
                let Some(source) = source.filter(|source| *source < size) else { continue };
                if let Some(sample) = plane.get(row.saturating_mul(size).saturating_add(source)) {
                    best = best.max(*sample);
                }
            }
            if let Some(slot) = out.get_mut(row.saturating_mul(size).saturating_add(column)) {
                *slot = best;
            }
        }
    }
    out
}

fn dilate_down(plane: &[f32], size: usize, radius: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; plane.len()];
    for row in 0..size {
        for column in 0..size {
            let mut best = 0.0f32;
            for offset in 0..=2 * radius {
                let source = row.saturating_add(offset).checked_sub(radius);
                let Some(source) = source.filter(|source| *source < size) else { continue };
                if let Some(sample) = plane.get(source.saturating_mul(size).saturating_add(column)) {
                    best = best.max(*sample);
                }
            }
            if let Some(slot) = out.get_mut(row.saturating_mul(size).saturating_add(column)) {
                *slot = best;
            }
        }
    }
    out
}
/// The crop, resized to 512x512 and normalised to 0-1, as the three planes LaMa asks for.
///
/// Alpha never reaches the model: LaMa takes RGB, so the alpha channel is blended separately.
#[must_use]
pub fn model_input(image: &Frame, window: Window) -> Vec<f32> {
    let max = f64::from(image.max_sample()).max(1.0);
    let image_width = usize::try_from(image.width).unwrap_or(usize::MAX);
    let plane_size = MODEL_SIZE.saturating_mul(MODEL_SIZE);
    let mut out = vec![0.0f32; 3 * plane_size];
    for channel in 0..3 {
        let mut plane = vec![0.0f32; window.width.saturating_mul(window.height)];
        for row in 0..window.height {
            for column in 0..window.width {
                let Some(at) =
                    window.pixel(row, column, image_width).and_then(|pixel| pixel.checked_mul(image.channels).and_then(|base| base.checked_add(channel)))
                else {
                    continue;
                };
                let sample = image.samples.get(at).copied().unwrap_or(0);
                if let Some(slot) = plane.get_mut(row.saturating_mul(window.width).saturating_add(column)) {
                    *slot = (f64::from(sample) / max) as f32;
                }
            }
        }
        let resized = resize_plane(&plane, window.width, window.height, MODEL_SIZE, MODEL_SIZE);
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

/// The model's answer back at the crop's size: three planes of 0-1 samples, clipped from whatever
/// the graph produced (LaMa is told to answer 0-255 but is not guaranteed to stay there).
#[must_use]
pub fn fill_back(output: &[f32], window: Window) -> Vec<Vec<f32>> {
    let plane_size = MODEL_SIZE.saturating_mul(MODEL_SIZE);
    let mut planes = Vec::with_capacity(3);
    for channel in 0..3usize {
        let start = channel.saturating_mul(plane_size);
        let mut plane = vec![0.0f32; plane_size];
        for offset in 0..plane_size {
            if let Some(sample) = output.get(start.saturating_add(offset))
                && let Some(slot) = plane.get_mut(offset)
            {
                *slot = (f64::from(*sample).clamp(0.0, 255.0) / 255.0) as f32;
            }
        }
        planes.push(resize_plane(&plane, MODEL_SIZE, MODEL_SIZE, window.width, window.height));
    }
    planes
}

/// Put the fill back into the image: wherever the **original** mask is above 0, the pixel is the
/// fill at full strength - never the dilated mask the model saw, and never blended by coverage.
///
/// SPEC amendment 16: exactly one side blends a soft selection, and it is PhotoCraft, which uses the
/// mask's coverage as the alpha of the "Generative Fill" layer. Blending here as well gave a 50%
/// pixel 25% of the fill. Where the mask is 0 the pixel is skipped, so it comes back bit for bit
/// (amendment 11). A 16-bit image keeps 16-bit precision for the pixels that are not filled; the
/// fill itself is 8-bit precision scaled up, which MG-04 accepts. Alpha inside the mask becomes
/// fully opaque, because the fill has no transparency of its own.
#[must_use]
pub fn blend(image: &Frame, mask: &[u8], window: Window, fill: &[Vec<f32>]) -> Frame {
    let max = f64::from(image.max_sample());
    let image_width = usize::try_from(image.width).unwrap_or(usize::MAX);
    let mut out = image.clone();
    for row in 0..window.height {
        for column in 0..window.width {
            let Some(pixel) = window.pixel(row, column, image_width) else { continue };
            if mask.get(pixel).copied().unwrap_or(0) == 0 {
                continue;
            }
            let Some(base) = pixel.checked_mul(image.channels) else { continue };
            let offset = row.saturating_mul(window.width).saturating_add(column);
            for channel in 0..image.channels {
                let Some(slot) = out.samples.get_mut(base.saturating_add(channel)) else { continue };
                let target = if channel >= ALPHA_CHANNEL {
                    max
                } else {
                    match fill.get(channel).and_then(|plane| plane.get(offset)) {
                        Some(sample) => f64::from(*sample) * max,
                        None => f64::from(*slot),
                    }
                };
                *slot = u16::try_from(target.round().clamp(0.0, max) as i64).unwrap_or(0);
            }
        }
    }
    out
}

/// The model itself, behind a trait so the stages and the maths can be tested with no model file
/// at all (MG-04 item 6).
pub trait Session: Send {
    /// Run one crop. `image` and `mask` are the packed planes [model_input] and [mask_plane] make
    /// (after dilation); the answer is the same shape, with samples meant to be 0-255.
    fn run(&mut self, image: &[f32], mask: &[f32], cancel: &AtomicBool) -> Result<Vec<f32>>;
}

/// How a session is made: once per pipeline, on the first job that needs one.
pub type Loader = Box<dyn Fn(&Path) -> Result<Box<dyn Session>> + Send + Sync>;

/// One installed model and the session it runs on. The session is made lazily and kept, so the
/// ~1.2 s model load is paid once per process rather than once per job (MG-04 item 2).
pub struct Pipeline {
    path: PathBuf,
    session: Mutex<Option<Box<dyn Session>>>,
    load: Loader,
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
        Self::with_loader(path, Box::new(load_ort_session))
    }

    /// The same with the session factory replaced, which is how a test walks the stages with a
    /// stand-in instead of the 200 MB graph.
    #[must_use]
    pub fn with_loader(path: impl Into<PathBuf>, load: Loader) -> Arc<Pipeline> {
        Arc::new(Pipeline { path: path.into(), session: Mutex::new(None), load })
    }

    /// Fill the masked area with the model, reporting the MG-05 stages as it goes.
    ///
    /// `cancel` is checked between every stage, and the session itself is asked to stop when the
    /// job is cancelled while the run is going on.
    pub fn run_fill(&self, image: &Frame, mask: &[u8], cancel: &AtomicBool, progress: &mut dyn FnMut(f64, &str)) -> Result<Filled> {
        let started = Instant::now();
        if !CHANNELS.iter().any(|channels| usize::from(*channels) == image.channels) {
            return Err(Fail::bad_request("Craft AI Manager can only fill images with three or four channels."));
        }
        if mask.len() < image.pixel_count() {
            return Err(Fail::internal("The mask is shorter than the image, which the job checks should have caught."));
        }
        let (Ok(width), Ok(height)) = (usize::try_from(image.width), usize::try_from(image.height)) else {
            return Err(Fail::internal("The image is too large for this machine."));
        };
        if stopped(cancel) {
            return Err(Fail::internal(CANCELLED));
        }
        progress(0.0, MSG_PREPARE);
        let Some(box_) = mask_box(width, height, mask) else {
            // Nothing is masked: the input comes back unchanged and the model never runs.
            progress(1.0, MSG_FINISH);
            return Ok(Filled { frame: image.clone(), elapsed_ms: elapsed_ms(&started), model: LAMA_MODEL, device: LAMA_DEVICE, provider: LAMA_PROVIDER });
        };
        let window = crop_window(box_, width, height);
        let model_mask = dilate(&mask_plane(mask, window, width, MODEL_SIZE), MODEL_SIZE, DILATE_PX);
        let model_image = model_input(image, window);
        progress(PREPARE_PROGRESS, MSG_PREPARE);
        if stopped(cancel) {
            return Err(Fail::internal(CANCELLED));
        }
        let output = {
            let mut guard = self.session.lock().unwrap_or_else(PoisonError::into_inner);
            let mut session = match guard.take() {
                Some(session) => session,
                None => {
                    progress(LOAD_PROGRESS, MSG_LOAD);
                    (self.load)(&self.path)?
                }
            };
            if stopped(cancel) {
                *guard = Some(session);
                return Err(Fail::internal(CANCELLED));
            }
            progress(RUN_PROGRESS, MSG_RUN);
            let output = session.run(&model_image, &model_mask, cancel);
            *guard = Some(session);
            output
        };
        if stopped(cancel) {
            return Err(Fail::internal(CANCELLED));
        }
        let output = match output {
            Ok(output) => output,
            Err(fail) => {
                // A run ORT stopped because the job was cancelled is a cancellation, not a failure.
                return Err(if stopped(cancel) { Fail::internal(CANCELLED) } else { fail });
            }
        };
        progress(RUN_END, MSG_FINISH);
        let frame = blend(image, mask, window, &fill_back(&output, window));
        progress(1.0, MSG_FINISH);
        Ok(Filled { frame, elapsed_ms: elapsed_ms(&started), model: LAMA_MODEL, device: LAMA_DEVICE, provider: LAMA_PROVIDER })
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
    fn run(&mut self, image: &[f32], mask: &[f32], cancel: &AtomicBool) -> Result<Vec<f32>> {
        let image_tensor = ort::value::Tensor::from_array(([1usize, 3, MODEL_SIZE, MODEL_SIZE], image.to_vec().into_boxed_slice())).map_err(ort_fail)?;
        let mask_tensor = ort::value::Tensor::from_array(([1usize, 1, MODEL_SIZE, MODEL_SIZE], mask.to_vec().into_boxed_slice())).map_err(ort_fail)?;
        let run_options = ort::session::RunOptions::new().map_err(ort_fail)?;
        // MG-05: a `DELETE` during the run reaches ONNX Runtime's own `terminate`, so the inference
        // stops in flight instead of waiting for its ~1.2 s to finish. `RunOptions` is `Sync` but
        // not `Send`, so the watcher only ever borrows it, inside a scoped thread that cannot
        // outlive this call.
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
            let outcome = self.session.run_with_options(ort::inputs!["image" => image_tensor, "mask" => mask_tensor], &run_options);
            done.store(true, Ordering::Relaxed);
            let _ = watcher.join();
            let outputs = outcome.map_err(ort_fail)?;
            let first = outputs.values().next().ok_or_else(|| Fail::internal("The LaMa model gave no output at all."))?;
            let (_shape, samples) = first.try_extract_tensor::<f32>().map_err(ort_fail)?;
            Ok(samples.to_vec())
        })
    }
}

/// ONNX Runtime allows one environment per process, so it is made once and shared.
static ENVIRONMENT: OnceLock<std::result::Result<Arc<Environment>, String>> = OnceLock::new();

/// The process-wide ONNX Runtime environment, with telemetry off: CAIM is local and offline.
fn environment() -> Result<Arc<Environment>> {
    let made = ENVIRONMENT.get_or_init(|| ort::init().with_telemetry(false).build().map(Arc::new).map_err(|error| error.to_string()));
    match made {
        Ok(environment) => Ok(Arc::clone(environment)),
        Err(message) => Err(Fail::internal(format!("Craft AI Manager could not start ONNX Runtime: {message}"))),
    }
}

/// Load the graph at `path`. This is the ~1.2 s the first job pays.
pub fn load_ort_session(path: &Path) -> Result<Box<dyn Session>> {
    let environment = environment()?;
    let session = ort::session::Session::builder(&environment).map_err(ort_fail)?.commit_from_file(path).map_err(ort_fail)?;
    Ok(Box::new(OrtSession { _environment: environment, session }))
}

/// Turn an ONNX Runtime error into the sentence a job fails with.
fn ort_fail(error: ort::Error) -> Fail {
    Fail::internal(format!("The LaMa model could not run: {error}"))
}

/// A pipeline for `model` when it is installed and acknowledged, and `None` otherwise.
///
/// "Installed" is MG-02's answer - the file, the promised size, and a `verified.json` that matches
/// it. Nothing is re-hashed here: that would cost seconds at every start (MG-04 item 2).
#[must_use]
pub fn installed_pipeline(root: &Path, model: &Model) -> Option<Arc<Pipeline>> {
    if model.id != LAMA_ID || model.task != TASK_FILL {
        return None;
    }
    if !store::is_installed(root, model) || !store::is_acknowledged(root, model) {
        return None;
    }
    Some(Pipeline::new(store::model_file(root, model)))
}

/// The backend `caim serve` should run jobs on: the installed, acknowledged LaMa, or `None`.
///
/// `root` replaces the model store when the caller names one, which is how a test can say
/// "nothing is installed" without touching the model folder on the machine.
#[must_use]
pub fn ready_pipeline_in(root: Option<&Path>) -> Option<Arc<Pipeline>> {
    let root = root.map(std::path::PathBuf::from).or_else(store::models_dir)?;
    let model = catalog::load().ok()?.into_iter().find(|model| model.id == LAMA_ID && model.task == TASK_FILL)?;
    installed_pipeline(&root, &model)
}
/// A stand-in for the ONNX session, shared by this crate's tests: no model file is involved, so the
/// stages, the cancellation, and the queue can be tested on any machine (MG-04 item 6).
#[cfg(test)]
pub(crate) struct StandIn {
    pub(crate) fill: f32,
    pub(crate) step: Duration,
    pub(crate) runs: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl Session for StandIn {
    /// Answers a flat grey, and notices a cancel partway through the run.
    fn run(&mut self, image: &[f32], _mask: &[f32], cancel: &AtomicBool) -> Result<Vec<f32>> {
        for _ in 0..20 {
            if cancel.load(Ordering::Relaxed) {
                return Err(Fail::internal(CANCELLED));
            }
            std::thread::sleep(self.step);
        }
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.fill; image.len()])
    }
}

/// A pipeline whose session is a [StandIn]. `loads` counts how many sessions were made, so a test
/// can see the model was loaded once and then kept.
#[cfg(test)]
pub(crate) fn stand_in_pipeline(
    fill: f32,
    step: Duration,
    loads: &Arc<std::sync::atomic::AtomicUsize>,
    runs: &Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<Pipeline> {
    let (loads, runs) = (Arc::clone(loads), Arc::clone(runs));
    let load: Loader = Box::new(move |_path: &Path| {
        loads.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(StandIn { fill, step, runs: Arc::clone(&runs) }))
    });
    Pipeline::with_loader("lama_fp32.onnx", load)
}
#[cfg(test)]
mod tests {
    thread_local! {
        static INTERMEDIATE_CAP: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
        static INTERMEDIATE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn record_intermediate(elements: usize) {
        INTERMEDIATE_COUNT.set(INTERMEDIATE_COUNT.get() + 1);
        assert!(elements <= INTERMEDIATE_CAP.get(), "intermediate would allocate {elements} elements; cap is {}", INTERMEDIATE_CAP.get());
    }

    struct IntermediateGuard(usize, usize);
    impl IntermediateGuard {
        fn new(cap: usize) -> Self {
            Self(INTERMEDIATE_CAP.replace(cap), INTERMEDIATE_COUNT.replace(0))
        }
    }
    impl Drop for IntermediateGuard {
        fn drop(&mut self) {
            INTERMEDIATE_CAP.set(self.0);
            INTERMEDIATE_COUNT.set(self.1);
        }
    }

    #[test]
    fn resize_order_bounds_the_actual_intermediate_allocation() {
        let _guard = IntermediateGuard::new(512);
        let source = vec![0.4; 512 * 512];
        assert_eq!(super::resize_plane(&source, 512, 512, 200_000, 1).len(), 200_000);
        assert_eq!(INTERMEDIATE_COUNT.get(), 1, "the executed order must report its actual intermediate");
        INTERMEDIATE_COUNT.set(0);
        assert_eq!(super::resize_plane(&source, 512, 512, 1, 200_000).len(), 200_000);
        assert_eq!(INTERMEDIATE_COUNT.get(), 1);
    }

    #[test]
    fn mask_pooling_order_bounds_the_actual_intermediate_allocation() {
        let _guard = IntermediateGuard::new(512);
        let mask = vec![255; 200_000];
        let _ = super::mask_plane(&mask, super::Window { x: 0, y: 0, width: 1, height: 200_000 }, 1, 512);
        assert_eq!(INTERMEDIATE_COUNT.get(), 1);
        INTERMEDIATE_COUNT.set(0);
        let _ = super::mask_plane(&mask, super::Window { x: 0, y: 0, width: 200_000, height: 1 }, 200_000, 512);
        assert_eq!(INTERMEDIATE_COUNT.get(), 1);
    }

    use super::{
        DILATE_PX, Filter, MODEL_SIZE, MSG_FINISH, MSG_LOAD, MSG_PREPARE, MSG_RUN, Window, blend, crop_window, dilate, fill_back, filter_for,
        installed_pipeline, mask_box, mask_plane, model_input, pad_for, resize_plane, stand_in_pipeline,
    };
    use crate::error::Status;
    use crate::image::Frame;
    use crate::store;
    use crate::testkit::{Scratch, test_model};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    /// A 4x4 image, one brightness per pixel, so a blend is easy to read.
    fn image(channels: usize, bit_depth: u8) -> Frame {
        let max = if bit_depth == 8 { 255 } else { 65535 };
        let samples: Vec<u16> = (0..16).flat_map(|pixel| (0..channels).map(move |_| u16::try_from(pixel * 4 + 1).unwrap_or(1).min(max))).collect();
        Frame { width: 4, height: 4, channels, bit_depth, samples }
    }

    /// A 4x4 mask covering the middle 2x2 with `value`, and nothing anywhere else.
    fn centre_mask(value: u8) -> Vec<u8> {
        (0..16)
            .map(|pixel| {
                let (row, column) = (pixel / 4, pixel % 4);
                if (1..3).contains(&row) && (1..3).contains(&column) { value } else { 0 }
            })
            .collect()
    }

    /// The whole 4x4 image, as a crop.
    fn whole() -> Window {
        Window { x: 0, y: 0, width: 4, height: 4 }
    }

    #[test]
    fn mixed_axis_resizes_preserve_values_and_transpose_symmetry() {
        let source: Vec<f32> = (0..64).map(|i| (i % 8) as f32 / 8.0).collect();
        let wide = resize_plane(&source, 8, 8, 100_000, 1);
        let transposed: Vec<f32> = (0..64).map(|i| source[(i % 8) * 8 + i / 8]).collect();
        let tall = resize_plane(&transposed, 8, 8, 1, 100_000);
        assert_eq!(wide.len(), 100_000);
        assert!(wide.iter().zip(&tall).all(|(a, b)| (a - b).abs() < 0.00001));
        assert!((wide[0] - source[0]).abs() < 0.00001);
        assert!((wide[99_999] - source[7]).abs() < 0.00001);
    }

    #[test]
    fn tall_and_wide_mask_pooling_preserve_the_same_thin_features() {
        let mut line = vec![0; 100_000];
        line[0] = 255;
        line[99_999] = 255;
        let wide = mask_plane(&line, Window { x: 0, y: 0, width: line.len(), height: 1 }, line.len(), MODEL_SIZE);
        let tall = mask_plane(&line, Window { x: 0, y: 0, width: 1, height: line.len() }, 1, MODEL_SIZE);
        for y in 0..MODEL_SIZE {
            for x in 0..MODEL_SIZE {
                assert_eq!(wide[y * MODEL_SIZE + x], tall[x * MODEL_SIZE + y]);
            }
        }
        assert!(wide.iter().any(|v| *v > 0.0));
    }

    #[test]
    fn the_mask_box_is_the_smallest_rectangle_over_the_selection() {
        let box_ = mask_box(4, 4, &centre_mask(255)).expect("the mask covers pixels");
        assert_eq!(box_, Window { x: 1, y: 1, width: 2, height: 2 });
        assert!(mask_box(4, 4, &[0u8; 16]).is_none(), "an empty mask has no box at all");
    }

    #[test]
    fn the_padding_is_32_pixels_or_a_quarter_of_the_longer_side() {
        assert_eq!(pad_for(10), 32, "a small box still gets its 32 pixels");
        assert_eq!(pad_for(128), 32, "a quarter of 128 ties with the minimum");
        assert_eq!(pad_for(200), 50, "a bigger box gets a quarter of its longer side");
    }

    #[test]
    fn the_crop_pads_the_box_and_grows_it_to_a_square() {
        let box_ = Window { x: 100, y: 100, width: 20, height: 10 };
        let crop = crop_window(box_, 640, 480);
        assert_eq!(crop, Window { x: 68, y: 63, width: 84, height: 84 }, "32 px of padding, then grown to a square");
        assert!(crop.x <= box_.x && crop.y <= box_.y, "the crop starts before the box");
        assert!(crop.x + crop.width >= box_.x + box_.width && crop.y + crop.height >= box_.y + box_.height, "and ends after it");
    }

    #[test]
    fn the_crop_is_clamped_to_the_image_and_still_covers_the_selection() {
        let (width, height) = (100usize, 40usize);
        let mut mask = vec![0u8; width * height];
        for row in 10..30 {
            for column in 90..width {
                mask[row * width + column] = 255;
            }
        }
        let box_ = mask_box(width, height, &mask).expect("the mask covers pixels");
        let crop = crop_window(box_, width, height);
        assert!(crop.width <= width && crop.height <= height, "the crop never leaves the image");
        assert_eq!(crop.x + crop.width, width, "it is clamped at the right edge");
        assert_eq!(crop.y + crop.height, height, "and at the bottom edge");
        assert!(crop.x <= box_.x && crop.y <= box_.y, "it still starts before the box");
        assert!(crop.x + crop.width >= box_.x + box_.width && crop.y + crop.height >= box_.y + box_.height, "and still covers all of it");
    }

    #[test]
    fn a_resize_lands_on_the_size_asked_for_and_a_flat_area_stays_flat() {
        let plane = vec![0.4f32; 100];
        let down = resize_plane(&plane, 10, 10, 4, 4);
        assert_eq!(down.len(), 4 * 4, "10x10 down to 4x4");
        assert!(down.iter().all(|sample| (f64::from(*sample) - 0.4).abs() < 1e-5), "area averaging keeps a flat area flat");
        let up = resize_plane(&down, 4, 4, 9, 9);
        assert_eq!(up.len(), 9 * 9, "and back up to 9x9");
        assert!(up.iter().all(|sample| (f64::from(*sample) - 0.4).abs() < 1e-5), "bilinear keeps it flat too");
        assert_eq!(filter_for(640, 512), Filter::Area, "a shrink uses area");
        assert_eq!(filter_for(300, 512), Filter::Bilinear, "a growth uses bilinear");
    }

    #[test]
    fn the_model_mask_is_one_where_the_selection_covers_and_zero_elsewhere() {
        let plane = mask_plane(&centre_mask(10), whole(), 4, 4);
        assert_eq!(plane.iter().filter(|sample| **sample == 1.0).count(), 4, "the middle 2x2, whatever the mask value");
        assert!(plane.iter().all(|sample| *sample == 0.0 || *sample == 1.0), "the model mask is never soft");
    }

    #[test]
    fn the_model_mask_is_dilated_by_four_pixels_at_512_scale() {
        let mut plane = vec![0.0f32; MODEL_SIZE * MODEL_SIZE];
        for row in 100..110 {
            for column in 100..110 {
                plane[row * MODEL_SIZE + column] = 1.0;
            }
        }
        let grown = dilate(&plane, MODEL_SIZE, DILATE_PX);
        assert_eq!(grown[99 * MODEL_SIZE + 104], 1.0, "four pixels above the box is inside the dilation");
        assert_eq!(grown[95 * MODEL_SIZE + 104], 0.0, "five pixels above it is not");
        assert_eq!(grown[104 * MODEL_SIZE + 99], 1.0, "four pixels to the left is inside");
        assert_eq!(grown[104 * MODEL_SIZE + 95], 0.0, "five pixels to the left is not");
        assert_eq!(grown.iter().filter(|sample| **sample > 0.0).count(), 18 * 18, "a 10x10 box grows by 4 px on every side");
    }

    #[test]
    fn the_model_planes_are_512_square_and_come_back_at_the_crop_size() {
        let planes = model_input(&image(3, 8), whole());
        assert_eq!(planes.len(), 3 * MODEL_SIZE * MODEL_SIZE, "the image plane is 3 x 512 x 512");
        assert!(planes.iter().all(|sample| (0.0..=1.0).contains(sample)), "the model is told 0-1");
        let back = fill_back(&vec![255.0f32; planes.len()], whole());
        assert_eq!(back.len(), 3, "the answer is read as RGB");
        for plane in back {
            assert_eq!(plane.len(), 4 * 4, "the answer is resized back to the crop");
            assert!(plane.iter().all(|sample| (f64::from(*sample) - 1.0).abs() < 1e-3), "255 comes back as 1.0");
        }
    }

    #[test]
    fn pixels_outside_the_mask_come_back_bit_for_bit() {
        for (channels, bit_depth) in [(3, 8), (4, 8), (3, 16), (4, 16)] {
            let frame = image(channels, bit_depth);
            let mask = centre_mask(255);
            let out = blend(&frame, &mask, whole(), &vec![vec![0.25; 16]; 3]);
            assert_eq!(out.bit_depth, bit_depth, "the depth is kept");
            for (pixel, covered) in mask.iter().enumerate() {
                if *covered == 0 {
                    for channel in 0..channels {
                        assert_eq!(out.sample(pixel, channel), frame.sample(pixel, channel), "({channels}, {bit_depth}) pixel {pixel} channel {channel}");
                    }
                }
            }
        }
    }

    #[test]
    fn any_coverage_takes_the_fill_at_full_strength() {
        // Amendment 16: PhotoCraft blends by coverage, so CAIM must not blend as well.
        let frame = image(3, 8);
        let fill = vec![vec![0.5; 16]; 3];
        let hard = blend(&frame, &centre_mask(255), whole(), &fill);
        assert_eq!(hard.sample(5, 0), Some(128), "0.5 x 255 rounds to 128");
        let soft = blend(&frame, &centre_mask(128), whole(), &fill);
        assert_eq!(soft.sample(5, 0), Some(128), "a half-covered pixel gets the whole fill, not half of it");
        let faint = blend(&frame, &centre_mask(1), whole(), &fill);
        assert_eq!(faint.sample(5, 0), Some(128), "even the faintest coverage gets the whole fill");
        let frame16 = image(3, 16);
        let hard16 = blend(&frame16, &centre_mask(255), whole(), &fill);
        assert_eq!(hard16.sample(5, 0), Some(32768), "the fill is 8-bit precision scaled up into the 16-bit range");
        assert_eq!(hard16.sample(0, 0), frame16.sample(0, 0), "and the untouched pixels keep their 16-bit precision");
    }

    #[test]
    fn alpha_inside_the_mask_becomes_opaque_and_outside_it_does_not_move() {
        let frame = Frame { width: 4, height: 4, channels: 4, bit_depth: 8, samples: vec![10; 64] };
        let fill = vec![vec![0.0; 16]; 3];
        assert_eq!(blend(&frame, &centre_mask(255), whole(), &fill).sample(5, 3), Some(255), "a fully covered pixel becomes opaque");
        assert_eq!(blend(&frame, &centre_mask(128), whole(), &fill).sample(5, 3), Some(255), "a half-covered pixel becomes opaque too");
        let frame16 = Frame { width: 4, height: 4, channels: 4, bit_depth: 16, samples: vec![10; 64] };
        assert_eq!(blend(&frame16, &centre_mask(1), whole(), &fill).sample(5, 3), Some(65535), "and in 16 bits it is 65535");
        assert_eq!(blend(&frame, &centre_mask(255), whole(), &fill).sample(0, 3), Some(10), "an uncovered pixel keeps its alpha");
    }

    #[test]
    fn the_stages_run_in_order_and_say_which_one_they_are() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(128.0, Duration::ZERO, &loads, &runs);
        let mut stages: Vec<(f64, String)> = Vec::new();
        let filled = pipeline
            .run_fill(&image(3, 8), &centre_mask(255), &AtomicBool::new(false), &mut |value, message| stages.push((value, message.to_string())))
            .expect("the stand-in session runs");
        assert_eq!(
            stages.iter().map(|(_, message)| message.as_str()).collect::<Vec<_>>(),
            [MSG_PREPARE, MSG_PREPARE, MSG_LOAD, MSG_RUN, MSG_FINISH, MSG_FINISH]
        );
        assert_eq!(stages.iter().map(|(value, _)| *value).collect::<Vec<_>>(), [0.0, 0.2, 0.25, 0.3, 0.9, 1.0], "prepare, load, run, finish");
        assert_eq!((filled.model, filled.device, filled.provider), ("lama", "cpu", "local"));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the model ran once");
    }

    #[test]
    fn a_cancelled_job_stops_at_the_stage_it_has_reached() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(128.0, Duration::from_millis(2), &loads, &runs);
        let fail =
            pipeline.run_fill(&image(3, 8), &centre_mask(255), &AtomicBool::new(true), &mut |_, _| {}).expect_err("the job was cancelled before it started");
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
        let fail = pipeline.run_fill(&image(3, 8), &centre_mask(255), &cancel, &mut |_, _| {}).expect_err("the run is cancelled partway through");
        watcher.join().expect("the watcher thread finishes");
        assert!(fail.sentence.contains("stopped the job"), "{fail}");
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the run never completed");
    }

    #[test]
    fn the_session_is_loaded_once_and_kept_for_the_next_job() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(64.0, Duration::ZERO, &loads, &runs);
        for _ in 0..2 {
            pipeline.run_fill(&image(3, 8), &centre_mask(255), &AtomicBool::new(false), &mut |_, _| {}).expect("the stand-in session runs");
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the model is loaded once, on the first job");
        assert_eq!(runs.load(Ordering::SeqCst), 2, "and reused by the second one");
    }

    #[test]
    fn an_empty_mask_returns_the_input_unchanged_and_never_loads_the_model() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(128.0, Duration::ZERO, &loads, &runs);
        let frame = image(3, 8);
        let filled = pipeline.run_fill(&frame, &[0u8; 16], &AtomicBool::new(false), &mut |_, _| {}).expect("an empty mask is a valid job");
        assert_eq!(filled.frame, frame, "the input comes back exactly as it went in");
        assert_eq!(loads.load(Ordering::SeqCst), 0, "there is nothing to do, so nothing is loaded");
    }

    #[test]
    fn an_image_that_is_not_rgb_or_rgba_is_refused_before_anything_loads() {
        let (loads, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let pipeline = stand_in_pipeline(128.0, Duration::ZERO, &loads, &runs);
        let frame = Frame { width: 4, height: 4, channels: 1, bit_depth: 8, samples: vec![7; 16] };
        let fail = pipeline.run_fill(&frame, &[255; 16], &AtomicBool::new(false), &mut |_, _| {}).expect_err("grayscale is not in the contract");
        assert_eq!(fail.status, Status::BadRequest, "{fail}");
        assert_eq!(loads.load(Ordering::SeqCst), 0, "the refusal comes before the model load");
    }

    #[test]
    fn a_pipeline_needs_the_file_the_record_and_the_acknowledgment() {
        let scratch = Scratch::new("lama-pipeline");
        let bytes: &[u8] = b"the bytes a small model would be made of";
        let model = test_model(bytes, Some("Read the note before using this model."));
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
        let scratch = Scratch::new("lama-pipeline-task");
        let bytes: &[u8] = b"the bytes a small model would be made of";
        let mut model = test_model(bytes, None);
        model.task = "select_subject".to_string();
        std::fs::create_dir_all(store::model_folder(&scratch.path, &model)).expect("the model folder");
        std::fs::write(store::model_file(&scratch.path, &model), bytes).expect("the model file");
        store::write_verified(&scratch.path, &model, &model.sha256, model.size).expect("the verified record");
        assert!(installed_pipeline(&scratch.path, &model).is_none(), "a model for another task is not offered here");
    }

    /// A session that keeps the model mask it was shown, so a test can look at what LaMa got.
    struct Spy {
        seen: Arc<std::sync::Mutex<Vec<f32>>>,
    }

    impl super::Session for Spy {
        fn run(&mut self, image: &[f32], mask: &[f32], _cancel: &AtomicBool) -> crate::error::Result<Vec<f32>> {
            *self.seen.lock().expect("the spy's lock") = mask.to_vec();
            Ok(image.iter().map(|sample| sample * 255.0).collect())
        }
    }

    /// A 3000x3000 mask with one covered column, the shape of a power line or a wire.
    fn thin_line(width: usize, height: usize, column: usize) -> Vec<u8> {
        let mut mask = vec![0u8; width * height];
        for row in 0..height {
            mask[row * width + column] = 255;
        }
        mask
    }

    #[test]
    fn a_one_pixel_line_survives_the_shrink_to_512() {
        // C2: sampling one source pixel per model pixel missed a 1 px line almost everywhere.
        let (width, height) = (3000usize, 3000usize);
        let mask = thin_line(width, height, 5);
        let window = Window { x: 0, y: 0, width, height };
        let plane = mask_plane(&mask, window, width, MODEL_SIZE);
        for row in 0..MODEL_SIZE {
            assert!(plane[row * MODEL_SIZE..(row + 1) * MODEL_SIZE].iter().any(|sample| *sample > 0.0), "model row {row} lost the line");
        }
        assert!(plane.iter().all(|sample| *sample == 0.0 || *sample == 1.0), "the model mask is never soft");
        let covered_columns = (0..MODEL_SIZE).filter(|column| (0..MODEL_SIZE).any(|row| plane[row * MODEL_SIZE + column] > 0.0)).count();
        assert!((1..=2).contains(&covered_columns), "a 1 px line touches at most two model columns, not a smear: {covered_columns}");
    }

    #[test]
    fn the_model_is_shown_a_thin_line_on_every_row() {
        let (width, height) = (3000u32, 3000u32);
        let mask = thin_line(width as usize, height as usize, 5);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let spy = Arc::clone(&seen);
        let pipeline = super::Pipeline::with_loader(
            "spy.onnx",
            Box::new(move |_path: &std::path::Path| Ok(Box::new(Spy { seen: Arc::clone(&spy) }) as Box<dyn super::Session>)),
        );
        let frame = Frame { width, height, channels: 3, bit_depth: 8, samples: vec![100; (width * height * 3) as usize] };
        pipeline.run_fill(&frame, &mask, &AtomicBool::new(false), &mut |_, _| {}).expect("the spy session runs");
        let model_mask = seen.lock().expect("the spy's lock").clone();
        assert_eq!(model_mask.len(), MODEL_SIZE * MODEL_SIZE);
        for row in 0..MODEL_SIZE {
            assert!(model_mask[row * MODEL_SIZE..(row + 1) * MODEL_SIZE].iter().any(|sample| *sample > 0.0), "model row {row} has no hole");
        }
    }

    #[test]
    fn growing_a_small_crop_still_samples_the_mask() {
        // Enlarging keeps nearest-neighbour sampling: a 2x2 selection in a 4x4 crop stays a quarter of the plane.
        let plane = mask_plane(&centre_mask(255), whole(), 4, 8);
        assert_eq!(plane.iter().filter(|sample| **sample == 1.0).count(), 16, "the middle 4x4 of an 8x8 plane");
    }

    #[test]
    fn a_wide_box_in_a_short_image_gets_the_full_height() {
        // C5: the square did not fit, so the crop used to fall back to the padded box (600x160).
        let box_ = Window { x: 800, y: 40, width: 400, height: 20 };
        let crop = crop_window(box_, 2000, 300);
        assert_eq!(crop, Window { x: 700, y: 0, width: 600, height: 300 }, "each side is clamped on its own, then centred");
        let tall = crop_window(Window { x: 40, y: 800, width: 20, height: 400 }, 300, 2000);
        assert_eq!(tall, Window { x: 0, y: 700, width: 300, height: 600 }, "and the same for a tall box in a narrow image");
    }

    #[test]
    fn the_crop_always_contains_the_box_and_stays_in_the_image() {
        for (image_width, image_height) in [(2000usize, 300usize), (300, 2000), (640, 480), (100, 40), (33, 1000), (5000, 64)] {
            for (x, y, width, height) in
                [(0usize, 0usize, 1usize, 1usize), (10, 5, 20, 3), (image_width - 1, image_height - 1, 1, 1), (0, 0, image_width, image_height)]
            {
                let box_ = Window { x, y, width, height };
                let crop = crop_window(box_, image_width, image_height);
                assert!(crop.x + crop.width <= image_width && crop.y + crop.height <= image_height, "{box_:?} in {image_width}x{image_height}: {crop:?}");
                assert!(crop.x <= box_.x && crop.y <= box_.y, "{box_:?} in {image_width}x{image_height}: {crop:?}");
                assert!(crop.x + crop.width >= box_.x + box_.width && crop.y + crop.height >= box_.y + box_.height, "{box_:?}: {crop:?}");
            }
        }
    }
}
