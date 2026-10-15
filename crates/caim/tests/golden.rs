// MG-04 item 7: the evaluation gate SPEC amendment 14 asks for. It runs the real LaMa graph, so it
// is ignored by default. Run it once the model is installed and acknowledged:
//
// `text
// caim models install lama --accept-note
// cargo test -- --ignored
//
// It looks in CRAFT_AI_MODELS first, then the platform model folder, exactly like `caim serve`.

use craft_ai_manager::{image, lama, store};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// The committed fixture: a photo-sized image, a hand-made mask, and the answer the first good
/// run produced.
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
/// How far the answer may sit from the committed one, per channel, averaged over the masked area.
const TOLERANCE: f64 = 2.0;
/// How wide the band of untouched pixels around the mask is, used as the surrounding background.
const RING: usize = 12;

fn fixture(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

/// A count as a float. No count in this test comes near four billion, so the step is exact.
fn real(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

#[test]
#[ignore = "needs an installed LaMa: cargo test -- --ignored"]
fn lama_takes_the_object_out_and_leaves_the_rest_alone() {
    let (input, expected) = (fixture("input.png"), fixture("expected.png"));
    let input_bytes = std::fs::read(&input).expect("the fixture image is committed");
    let expected_bytes = std::fs::read(&expected).expect("the committed answer is there");
    let mask_bytes = std::fs::read(fixture("mask.png")).expect("the fixture mask is committed");
    let frame = image::decode_image(&input_bytes, 256, 256, 3, 8).expect("the fixture image is 8-bit RGB");
    let mask = image::decode_mask(&mask_bytes, 256, 256).expect("the fixture mask is 8-bit grayscale");
    let root = store::models_dir().expect("CRAFT_AI_MODELS or the platform model folder");
    let pipeline = lama::ready_pipeline_in(Some(&root)).expect("install LaMa first: caim models install lama --accept-note");
    let filled = pipeline.run_fill(&frame, &mask, &AtomicBool::new(false), &mut |_, _| {}).expect("the model runs");

    // Same size, channels and depth as the image that went in.
    assert_eq!((filled.frame.width, filled.frame.height), (256, 256), "the answer is the same size");
    assert_eq!((filled.frame.channels, filled.frame.bit_depth), (3, 8), "and the same shape");
    assert_eq!((filled.model, filled.device, filled.provider), ("lama", "cpu", "local"), "the provenance is the real model");

    // Amendment 11: every pixel the mask does not cover is bit for bit the input.
    let mut outside = 0usize;
    for (pixel, &covered) in mask.iter().enumerate() {
        if covered == 0 {
            for channel in 0..3 {
                assert_eq!(filled.frame.sample(pixel, channel), frame.sample(pixel, channel), "pixel {pixel} channel {channel} is outside the mask");
            }
            outside += 1;
        }
    }
    assert!(outside > 0, "the fixture mask does not cover the whole image");

    // Inside the mask the answer is the committed one, to within two levels a channel.
    let answer = image::decode_image(&expected_bytes, 256, 256, 3, 8).expect("the committed answer is 8-bit RGB");
    let mut total = 0.0f64;
    let mut inside = 0usize;
    for (pixel, &covered) in mask.iter().enumerate() {
        if covered == 0 {
            continue;
        }
        for channel in 0..3 {
            let (got, want) = (f64::from(filled.frame.sample(pixel, channel).unwrap_or(0)), f64::from(answer.sample(pixel, channel).unwrap_or(0)));
            total += (got - want).abs();
        }
        inside += 1;
    }
    let mean = total / real(inside.saturating_mul(3));
    assert!(mean <= TOLERANCE, "the masked area is {mean:.3} levels a channel away from the committed answer");

    // And the object really is gone: the masked area is closer to the background around it than
    // the object used to be.
    let plane: Vec<f32> = mask.iter().map(|sample| f32::from(*sample) / 255.0).collect();
    let ring = lama::dilate(&plane, 256, RING);
    let mut colour = [0.0f64; 3];
    let mut around = 0usize;
    for (pixel, &covered) in mask.iter().enumerate() {
        if covered == 0 && ring[pixel] > 0.0 {
            for (value, channel) in colour.iter_mut().zip(0..3) {
                *value += f64::from(frame.sample(pixel, channel).unwrap_or(0));
            }
            around += 1;
        }
    }
    assert!(around > 0, "there are pixels around the mask");
    for channel in colour.iter_mut() {
        *channel /= real(around);
    }
    let distance = |frame: &image::Frame| -> f64 {
        let mut sum = 0.0;
        for (pixel, &covered) in mask.iter().enumerate() {
            if covered == 0 {
                continue;
            }
            for (want, channel) in colour.into_iter().zip(0..3) {
                sum += (f64::from(frame.sample(pixel, channel).unwrap_or(0)) - want).abs();
            }
        }
        sum / real(inside.saturating_mul(3))
    };
    let (before, after) = (distance(&frame), distance(&filled.frame));
    println!(
        "{outside} pixels outside the mask are identical; inside, {mean:.3} levels a channel from the committed answer, and the masked area moved from {before:.1} to {after:.1} away from the background colour"
    );
    assert!(after < before, "the masked area moved from {before:.1} to {after:.1} away from the background colour");
}
