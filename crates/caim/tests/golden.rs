// MG-04 item 7 and MG-10: the evaluation gate SPEC amendment 14 asks for, plus amendment 18's mask
// gate. These run the real ONNX graphs, so they are ignored by default. Run them once both models
// are installed and acknowledged:
//
// `text
// caim models install lama --accept-note
// caim models install birefnet --accept-note
// cargo test --test golden -- --ignored --nocapture
//
// They look in CRAFT_AI_MODELS first, then the platform model folder, exactly like `caim serve`.
//
// The two `*_on_the_card` tests are MG-11's CUDA half of the same gate. They need the vendor's
// libraries as well as the models, and they are the evidence amendment 18 wants before a catalog
// entry may say `cuda = "supported"`:
//
// `text
// caim cuda install --accept-note
// cargo test --test golden -- --ignored card
//
// Run them with nothing else holding the card: a CUDA session needs the model's `memory_mib` free,
// and a run that cannot start falls back to the CPU, which these tests report as a failure.

use craft_ai_manager::backend::Filled;
use craft_ai_manager::device::{Decision, Device, Setting};
use craft_ai_manager::{birefnet, catalog, image, lama, store};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// The committed fixture: a photo-sized image, a hand-made mask, and the answer the first good
/// run produced.
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
/// How far the answer may sit from the committed one, per channel, averaged over the masked area.
const TOLERANCE: f64 = 2.0;
/// How wide the band of untouched pixels around the mask is, used as the surrounding background.
const RING: usize = 12;
/// How much of the subject has to be in the right place: amendment 18's mask gate, measured against
/// the committed matte. The threshold is only for the metric - the answer itself is never thresholded.
const IOU_FLOOR: f64 = 0.95;
/// How far a fresh matte may sit from the committed one, averaged over the image. The committed
/// mattes came from the same contract run through Python and ONNX Runtime 1.31, so this only moves
/// when the maths really changed.
const MATTE_TOLERANCE: f64 = 2.0;
/// The committed fixture is square: 448x448. `lama::dilate` only works on a square plane, so the
/// photo is a square crop of the original rather than the full landscape.
const SIDE: u32 = 448;
/// MG-11: how much of the image the card's matte may disagree with the CPU matte about. Amendment
/// 18's CUDA gate: within 1/255 on at least 99.5% of the pixels.
const CARD_DISAGREEMENT_CEILING: f64 = 0.005;

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
    let frame = image::decode_image(&input_bytes, SIDE, SIDE, 3, 8).expect("the fixture image is 8-bit RGB");
    let mask = image::decode_mask(&mask_bytes, SIDE, SIDE).expect("the fixture mask is 8-bit grayscale");
    let root = store::models_dir().expect("CRAFT_AI_MODELS or the platform model folder");
    let pipeline = lama::ready_pipeline_in(Some(&root)).expect("install LaMa first: caim models install lama --accept-note");
    // Two runs: the first pays for the session, the second shows the warm time, which is what
    // amendment 18's timing table asks for.
    let filled = pipeline.run_fill(&frame, &mask, &AtomicBool::new(false), &mut |_, _| {}).expect("the model runs");
    let warm = pipeline.run_fill(&frame, &mask, &AtomicBool::new(false), &mut |_, _| {}).expect("the session is kept for the next job");
    assert!(filled.load_ms > 0, "the first job paid for the session: {} ms", filled.load_ms);
    assert_eq!(warm.load_ms, 0, "the second job reused the session");

    // Same size, channels and depth as the image that went in.
    assert_eq!((filled.frame.width, filled.frame.height), (SIDE, SIDE), "the answer is the same size");
    assert_eq!((filled.frame.channels, filled.frame.bit_depth), (3, 8), "and the same shape");
    assert_eq!((filled.model.as_str(), filled.device.as_str(), filled.provider.as_str()), ("lama", "cpu", "local"), "the provenance is the real model");

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
    let answer = image::decode_image(&expected_bytes, SIDE, SIDE, 3, 8).expect("the committed answer is 8-bit RGB");
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
    let ring = lama::dilate(&plane, SIDE as usize, RING);
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
        "lama on the CPU: session load {} ms, first run {} ms, warm run {} ms; {outside} pixels outside the mask are identical; inside, {mean:.3} levels a channel from the committed answer, and the masked area moved from {before:.1} to {after:.1} away from the background colour",
        filled.load_ms, filled.run_ms, warm.run_ms
    );
    assert!(after < before, "the masked area moved from {before:.1} to {after:.1} away from the background colour");
}

/// One golden mask run: the real BiRefNet on a committed photo, checked against a committed matte.
///
/// The committed mattes were made by a Python run of the same contract (RS-01's preprocessing,
/// `sigmoid * 255` at 1024, then the same bilinear resize back), not by this code, so the comparison
/// is a real check on the runner rather than a copy of its own output.
fn mask_golden(who: &str, photo: &str, expected: &str, width: u32, height: u32) {
    let input_bytes = std::fs::read(fixture(photo)).expect("the fixture photo is committed");
    let expected_bytes = std::fs::read(fixture(expected)).expect("the committed matte is there");
    let frame = image::decode_image(&input_bytes, width, height, 3, 8).expect("the fixture photo is 8-bit RGB");
    // Amendment 18: a mask job sends the same three parts, with a mask that is all 255.
    let pixels = u64::from(width).checked_mul(u64::from(height)).expect("a fixture-sized image");
    let all = vec![255u8; usize::try_from(pixels).expect("a fixture-sized image")];
    let root = store::models_dir().expect("CRAFT_AI_MODELS or the platform model folder");
    let pipeline = birefnet::ready_pipeline_in(Some(&root)).expect("install BiRefNet first: caim models install birefnet --accept-note");

    // Two runs: the first pays for the session, the second shows the warm time and the drift.
    let started = Instant::now();
    let first = pipeline.run_mask(&frame, &all, &AtomicBool::new(false), &mut |_, _| {}).expect("the model runs");
    let first_wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let second = pipeline.run_mask(&frame, &all, &AtomicBool::new(false), &mut |_, _| {}).expect("the session is kept for the next job");

    // Amendment 18: 8-bit grayscale at the size that was sent, whatever went in.
    assert_eq!((first.frame.width, first.frame.height), (width, height), "the matte is the size that was sent");
    assert_eq!((first.frame.channels, first.frame.bit_depth), (1, 8), "8-bit grayscale");
    assert_eq!(first.kind, "mask", "the result says what it is");
    assert_eq!((first.model.as_str(), first.device.as_str(), first.provider.as_str()), ("birefnet-lite", "cpu", "local"), "the provenance is the real model");
    assert_eq!(first.device_requested, "auto", "the job asked for whatever is available");
    assert_eq!(first.device_name.as_deref(), Some("CPU"), "the hardware in plain words");
    assert_eq!(first.fallback_reason, None, "nothing fell back");
    let entry = catalog::load()
        .expect("the catalog is part of the binary")
        .into_iter()
        .find(|model| model.id == birefnet::BIREFNET_ID)
        .expect("birefnet-lite is in the catalog");
    assert_eq!(first.model_version.as_deref(), Some(entry.version.as_str()), "the catalog version is pinned to the result");
    assert_eq!(first.model_sha256.as_deref(), Some(entry.sha256.as_str()), "and so are the bytes that ran");
    assert!(first.load_ms > 0, "the first job paid for the session: {} ms", first.load_ms);
    assert_eq!(second.load_ms, 0, "the second job reused the session");

    // Against the committed matte: how much of the subject is in the right place, and how far the
    // soft edge sits.
    let answer = image::decode_mask(&expected_bytes, width, height).expect("the committed matte is 8-bit grayscale at the same size");
    let (mut both, mut either, mut total, mut soft, mut drift) = (0usize, 0usize, 0usize, 0usize, 0u16);
    for (pixel, &want) in answer.iter().enumerate() {
        let (Some(got), Some(warm)) = (first.frame.sample(pixel, 0), second.frame.sample(pixel, 0)) else {
            continue;
        };
        let (want, got) = (u16::from(want), got);
        if got >= 128 && want >= 128 {
            both += 1;
        }
        if got >= 128 || want >= 128 {
            either += 1;
        }
        total += usize::from(got.abs_diff(want));
        if (1..255).contains(&got) {
            soft += 1;
        }
        drift = drift.max(got.abs_diff(warm));
    }
    assert!(either > 0, "the committed matte and the answer both have a subject");
    let iou = real(both) / real(either);
    let mean = real(total) / real(all.len());
    assert!(iou >= IOU_FLOOR, "{who}: the subject is only {iou:.4} agreed with the committed matte");
    assert!(mean <= MATTE_TOLERANCE, "{who}: the matte is {mean:.3} levels a pixel away from the committed one");
    assert!(soft > 0, "{who}: amendment 18 wants a soft edge, and there is none");
    assert!(drift <= 1, "{who}: the same image twice differs by {drift} levels, over the one level the gate allows");

    println!(
        "{who}: session load {} ms, first run {} ms ({first_wall_ms} ms wall), warm run {} ms; IoU {iou:.4}, mean {mean:.3} levels from the committed matte, {soft} soft-edge pixels, warm-run drift {drift} levels",
        first.load_ms, first.run_ms, second.run_ms
    );
}

#[test]
#[ignore = "needs an installed BiRefNet: caim models install birefnet --accept-note"]
fn birefnet_finds_the_buoy() {
    mask_golden("buoy", "input.png", "mask-buoy.png", 448, 448);
}

#[test]
#[ignore = "needs an installed BiRefNet: caim models install birefnet --accept-note"]
fn birefnet_finds_the_person_and_keeps_the_hair() {
    mask_golden("person", "input-person.png", "mask-person.png", 768, 512);
}

/// The decision that forces the card. The catalog keeps `cuda = "untested"` until these two tests
/// have passed, so the resolver would - correctly - keep such a model off the card. Gathering the
/// evidence means asking for the card by hand, chain and all: CUDA first, and the CPU behind it so
/// a card that cannot start the graph still answers rather than failing the job.
fn card_decision() -> Decision {
    Decision { requested: Setting::Cuda, chain: vec![Device::Cuda, Device::Cpu], reason: None, device_name: None, card: None, note: None }
}

/// A CUDA run that came back on the CPU is not evidence: say which of amendment 18's reasons it
/// gave, because that is the sentence that tells Erik what to fix. One helper for both models
/// because both answers are the same struct.
fn assert_on_the_card(filled: &Filled, who: &str) {
    assert_eq!(
        filled.device,
        "cuda",
        "{who}: the card was asked for and the result says it ran on {} (fallback_reason: {}, diagnostics: {})",
        filled.device,
        filled.fallback_reason.as_deref().unwrap_or("none"),
        filled.diagnostics.as_deref().unwrap_or("none")
    );
    assert_eq!(filled.device_requested, "cuda", "{who}: the result says what was asked for");
    assert!(filled.device_name.as_deref().unwrap_or_default().starts_with("NVIDIA"), "{who}: device_name is {:?}", filled.device_name);
}

#[test]
#[ignore = "needs an installed LaMa and the CUDA libraries: caim cuda install --accept-note"]
fn lama_on_the_card() {
    let input_bytes = std::fs::read(fixture("input.png")).expect("the fixture image is committed");
    let expected_bytes = std::fs::read(fixture("expected.png")).expect("the committed answer is there");
    let mask_bytes = std::fs::read(fixture("mask.png")).expect("the fixture mask is committed");
    let frame = image::decode_image(&input_bytes, SIDE, SIDE, 3, 8).expect("the fixture image is 8-bit RGB");
    let mask = image::decode_mask(&mask_bytes, SIDE, SIDE).expect("the fixture mask is 8-bit grayscale");
    let root = store::models_dir().expect("CRAFT_AI_MODELS or the platform model folder");
    let pipeline = lama::ready_pipeline_in(Some(&root)).expect("install LaMa first: caim models install lama --accept-note");
    let cancel = AtomicBool::new(false);

    let cpu = pipeline.run_fill(&frame, &mask, &cancel, &mut |_, _| {}).expect("the CPU run works");
    let card = pipeline.run_fill_on(&frame, &mask, &card_decision(), &cancel, &mut |_, _| {}).expect("the model runs on the card");
    let warm = pipeline.run_fill_on(&frame, &mask, &card_decision(), &cancel, &mut |_, _| {}).expect("the card session is kept for the next job");
    assert_on_the_card(&card, "lama");
    assert_on_the_card(&warm, "lama");
    assert!(card.load_ms > 0, "the card session was made for this run: {} ms", card.load_ms);
    assert_eq!(warm.load_ms, 0, "the second card job reused it");

    // The same gate as the CPU golden: the masked area is within two levels a channel of the
    // committed answer, and every pixel outside the mask is untouched.
    let answer = image::decode_image(&expected_bytes, SIDE, SIDE, 3, 8).expect("the committed answer is 8-bit RGB");
    let (mut total, mut inside, mut outside, mut over_one) = (0.0f64, 0usize, 0usize, 0usize);
    for (pixel, &covered) in mask.iter().enumerate() {
        if covered == 0 {
            for channel in 0..3 {
                assert_eq!(card.frame.sample(pixel, channel), frame.sample(pixel, channel), "pixel {pixel} channel {channel} is outside the mask");
            }
            outside += 1;
            continue;
        }
        for channel in 0..3 {
            let (got, want) = (f64::from(card.frame.sample(pixel, channel).unwrap_or(0)), f64::from(answer.sample(pixel, channel).unwrap_or(0)));
            total += (got - want).abs();
            if card.frame.sample(pixel, channel).unwrap_or(0).abs_diff(cpu.frame.sample(pixel, channel).unwrap_or(0)) > 1 {
                over_one += 1;
            }
        }
        inside += 1;
    }
    assert!(outside > 0 && inside > 0, "the fixture mask covers part of the image");
    let mean = total / real(inside.saturating_mul(3));
    assert!(mean <= TOLERANCE, "lama on the card: the masked area is {mean:.3} levels a channel from the committed answer");
    let samples = real(inside.saturating_mul(3));
    println!(
        "lama on the card ({}): card load {} ms, first run {} ms, warm run {} ms; CPU load {} ms, run {} ms; {mean:.3} levels a channel from the committed answer, {} of {samples:.0} masked samples over one level from the CPU answer",
        card.device_name.as_deref().unwrap_or("?"),
        card.load_ms,
        card.run_ms,
        warm.run_ms,
        cpu.load_ms,
        cpu.run_ms,
        over_one
    );
}

/// One golden mask run on the card, against both the committed matte and the CPU matte of the same
/// photo. Upstream #4812 found this graph's logits collapse on DirectML, so the CPU comparison is
/// the check that the card has not done the same thing quietly.
fn card_mask_golden(who: &str, photo: &str, expected: &str, width: u32, height: u32) {
    let input_bytes = std::fs::read(fixture(photo)).expect("the fixture photo is committed");
    let expected_bytes = std::fs::read(fixture(expected)).expect("the committed matte is there");
    let frame = image::decode_image(&input_bytes, width, height, 3, 8).expect("the fixture photo is 8-bit RGB");
    let pixels = u64::from(width).checked_mul(u64::from(height)).expect("a fixture-sized image");
    let all = vec![255u8; usize::try_from(pixels).expect("a fixture-sized image")];
    let root = store::models_dir().expect("CRAFT_AI_MODELS or the platform model folder");
    let pipeline = birefnet::ready_pipeline_in(Some(&root)).expect("install BiRefNet first: caim models install birefnet --accept-note");
    let cancel = AtomicBool::new(false);

    let cpu = pipeline.run_mask(&frame, &all, &cancel, &mut |_, _| {}).expect("the CPU run works");
    let card = pipeline.run_mask_on(&frame, &all, &card_decision(), &cancel, &mut |_, _| {}).expect("the model runs on the card");
    let warm = pipeline.run_mask_on(&frame, &all, &card_decision(), &cancel, &mut |_, _| {}).expect("the card session is kept for the next job");
    assert_on_the_card(&card, who);
    assert_on_the_card(&warm, who);
    assert!(card.load_ms > 0, "{who}: the card session was made for this run: {} ms", card.load_ms);
    assert_eq!(warm.load_ms, 0, "{who}: the second card job reused it");

    let answer = image::decode_mask(&expected_bytes, width, height).expect("the committed matte is 8-bit grayscale at the same size");
    let (mut both, mut either, mut total, mut soft, mut over_one) = (0usize, 0usize, 0usize, 0usize, 0usize);
    for (pixel, &want) in answer.iter().enumerate() {
        let (Some(got), Some(from_cpu)) = (card.frame.sample(pixel, 0), cpu.frame.sample(pixel, 0)) else {
            continue;
        };
        let want = u16::from(want);
        if got >= 128 && want >= 128 {
            both += 1;
        }
        if got >= 128 || want >= 128 {
            either += 1;
        }
        total += usize::from(got.abs_diff(want));
        if (1..255).contains(&got) {
            soft += 1;
        }
        if got.abs_diff(from_cpu) > 1 {
            over_one += 1;
        }
    }
    assert!(either > 0, "{who}: the committed matte and the card's answer both have a subject");
    let iou = real(both) / real(either);
    let mean = real(total) / real(all.len());
    assert!(iou >= IOU_FLOOR, "{who} on the card: the subject is only {iou:.4} agreed with the committed matte");
    assert!(mean <= MATTE_TOLERANCE, "{who} on the card: the matte is {mean:.3} levels a pixel away from the committed one");
    assert!(soft > 0, "{who} on the card: amendment 18 wants a soft edge, and there is none");
    // Amendment 18's CUDA gate, and the DirectML failure upstream #4812 hit: a card matte that has
    // collapsed would disagree with the CPU matte over a large part of the image.
    let share = real(over_one) / real(all.len());
    assert!(share <= CARD_DISAGREEMENT_CEILING, "{who} on the card: {share:.4} of the pixels sit over one level from the CPU matte");

    println!(
        "{who} on the card ({}): card load {} ms, first run {} ms, warm run {} ms; CPU load {} ms, run {} ms; IoU {iou:.4}, mean {mean:.3} levels from the committed matte, {soft} soft-edge pixels, {share:.4} of the pixels over one level from the CPU matte",
        card.device_name.as_deref().unwrap_or("?"),
        card.load_ms,
        card.run_ms,
        warm.run_ms,
        cpu.load_ms,
        cpu.run_ms
    );
}

#[test]
#[ignore = "needs an installed BiRefNet and the CUDA libraries: caim cuda install --accept-note"]
fn birefnet_on_the_card_finds_the_buoy() {
    card_mask_golden("buoy", "input.png", "mask-buoy.png", 448, 448);
}

#[test]
#[ignore = "needs an installed BiRefNet and the CUDA libraries: caim cuda install --accept-note"]
fn birefnet_on_the_card_finds_the_person_and_keeps_the_hair() {
    card_mask_golden("person", "input-person.png", "mask-person.png", 768, 512);
}
