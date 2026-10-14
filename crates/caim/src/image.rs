//! The PNG halves of the protocol: the `image` and `mask` parts and the `result.png` that goes
//! back. Amendment 11 fixes the shapes - RGB or RGBA at 8 or 16 bits, an 8-bit grayscale mask, and
//! a result with the same size, depth and channels as the image it came from.
//!
//! Samples are held as `u16` whatever the depth, so the backend does not care whether the document
//! was 8-bit or 16-bit; `max_sample` says where white is.

use std::io::Cursor;

use png::{BitDepth, ColorType, Decoder, Encoder, Limits};

use crate::error::{Fail, Result};

/// The channel counts the protocol allows.
pub const CHANNELS: [u8; 2] = [3, 4];
/// The bit depths the protocol allows.
pub const DEPTHS: [u8; 2] = [8, 16];

/// One image: the samples are interleaved, big-endian-normalised to `u16`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub bit_depth: u8,
    pub samples: Vec<u16>,
}

impl Frame {
    /// Where the brightest sample is: 255 for an 8-bit image, 65535 for a 16-bit one.
    #[must_use]
    pub const fn max_sample(&self) -> u32 {
        if self.bit_depth == 8 { 255 } else { 65535 }
    }

    /// How many pixels the frame holds.
    #[must_use]
    pub fn pixel_count(&self) -> usize {
        self.width.saturating_mul(self.height) as usize
    }

    /// The sample at one pixel and channel, or None if that is outside the frame.
    #[must_use]
    pub fn sample(&self, pixel: usize, channel: usize) -> Option<u16> {
        if channel >= self.channels {
            return None;
        }
        let at = pixel.checked_mul(self.channels)?.checked_add(channel)?;
        self.samples.get(at).copied()
    }
}

/// The number of raw sample bytes for a window, with the multiplications checked.
pub fn raw_byte_len(width: u32, height: u32, channels: usize, bit_depth: u8) -> Result<usize> {
    let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or_else(|| Fail::too_large("The image is too large for Craft AI Manager to read."))?;
    let samples = pixels.checked_mul(channels as u64).ok_or_else(|| Fail::too_large("The image is too large for Craft AI Manager to read."))?;
    let bytes_per_sample = if bit_depth == 8 { 1 } else { 2 };
    let bytes = samples.checked_mul(bytes_per_sample).ok_or_else(|| Fail::too_large("The image is too large for Craft AI Manager to read."))?;
    usize::try_from(bytes).map_err(|_| Fail::too_large("The image is too large for Craft AI Manager to read."))
}

fn depth_of(bit_depth: u8) -> Result<BitDepth> {
    match bit_depth {
        8 => Ok(BitDepth::Eight),
        16 => Ok(BitDepth::Sixteen),
        _ => Err(Fail::bad_request("The bit depth must be 8 or 16.")),
    }
}

/// One channel is the mask, three and four are the image. The job's own channel count is checked
/// against the protocol in `protocol::JobRequest::validate`, which allows only 3 and 4.
fn color_type_of(channels: usize) -> Result<ColorType> {
    match channels {
        1 => Ok(ColorType::Grayscale),
        3 => Ok(ColorType::Rgb),
        4 => Ok(ColorType::Rgba),
        _ => Err(Fail::bad_request("Craft AI Manager reads a grayscale mask and an RGB or RGBA image.")),
    }
}

fn wrong_size() -> Fail {
    Fail::bad_request("The image in the job is not the size the job says it is.")
}

/// Read one PNG and check it against what the job declared, before any of it is decoded.
fn decode_png(bytes: &[u8], width: u32, height: u32, channels: usize, bit_depth: u8, what: &str) -> Result<Vec<u8>> {
    let color = color_type_of(channels)?;
    let depth = depth_of(bit_depth)?;
    let raw_len = raw_byte_len(width, height, channels, bit_depth)?;
    let mut decoder = Decoder::new_with_limits(Cursor::new(bytes), Limits { bytes: raw_len.saturating_add(raw_len / 4).saturating_add(1024) });
    decoder.set_ignore_text_chunk(true);
    let header = decoder.read_header_info()?;
    if header.width != width || header.height != height {
        return Err(wrong_size());
    }
    if header.color_type != color {
        return Err(Fail::bad_request(format!("The {what} PNG does not have the number of channels the job says it has.")));
    }
    if header.bit_depth != depth {
        return Err(Fail::bad_request(format!("The {what} PNG does not have the bit depth the job says it has.")));
    }
    if header.interlaced {
        return Err(Fail::bad_request(format!("The {what} PNG is interlaced, which Craft AI Manager does not read.")));
    }
    if header.palette.is_some() {
        return Err(Fail::bad_request(format!("The {what} PNG uses a palette, which Craft AI Manager does not read.")));
    }
    let mut reader = decoder.read_info()?;
    if reader.output_color_type() != (color, depth) {
        return Err(Fail::bad_request(format!("The {what} PNG is not in the format the protocol asks for.")));
    }
    if reader.output_buffer_size() != Some(raw_len) {
        return Err(wrong_size());
    }
    let mut raw = vec![0u8; raw_len];
    reader.next_frame(&mut raw)?;
    Ok(raw)
}

/// The `image` part: RGB or RGBA, 8- or 16-bit, checked against the job's own numbers.
pub fn decode_image(bytes: &[u8], width: u32, height: u32, channels: usize, bit_depth: u8) -> Result<Frame> {
    let raw = decode_png(bytes, width, height, channels, bit_depth, "image")?;
    let samples = if bit_depth == 8 {
        raw.iter().map(|byte| u16::from(*byte)).collect::<Vec<u16>>()
    } else {
        raw.as_chunks::<2>().0.iter().map(|pair| u16::from_be_bytes(*pair)).collect::<Vec<u16>>()
    };
    Ok(Frame { width, height, channels, bit_depth, samples })
}

/// The `mask` part: 8-bit grayscale, one byte per pixel, 255 fully selected and 0 keep.
pub fn decode_mask(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    decode_png(bytes, width, height, 1, 8, "mask")
}

/// `result.png`: the same size, channels and depth as the image that came in.
pub fn encode(frame: &Frame) -> Result<Vec<u8>> {
    let color = color_type_of(frame.channels)?;
    let depth = depth_of(frame.bit_depth)?;
    let mut raw = Vec::new();
    if frame.bit_depth == 8 {
        raw.extend(frame.samples.iter().map(|sample| u8::try_from(*sample).unwrap_or(0)));
    } else {
        for sample in &frame.samples {
            raw.extend_from_slice(&sample.to_be_bytes());
        }
    }
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, frame.width, frame.height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&raw)?;
    writer.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{Frame, decode_image, decode_mask, encode, raw_byte_len};
    use crate::error::Status;

    fn rgb(width: u32, height: u32, bit_depth: u8, samples: &[u16]) -> Frame {
        Frame { width, height, channels: 3, bit_depth, samples: samples.to_vec() }
    }

    fn gray(width: u32, height: u32, samples: &[u16]) -> Frame {
        Frame { width, height, channels: 1, bit_depth: 8, samples: samples.to_vec() }
    }

    #[test]
    fn an_eight_bit_rgb_frame_survives_a_round_trip() {
        let frame = rgb(2, 2, 8, &[10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120]);
        let png = encode(&frame).expect("the frame encodes");
        let back = decode_image(&png, 2, 2, 3, 8).expect("and decodes");
        assert_eq!(frame, back);
    }

    #[test]
    fn a_sixteen_bit_rgba_frame_survives_a_round_trip() {
        let frame = Frame { width: 2, height: 1, channels: 4, bit_depth: 16, samples: vec![0, 1, 65535, 300, 1234, 5, 6, 7] };
        let png = encode(&frame).expect("the frame encodes");
        let back = decode_image(&png, 2, 1, 4, 16).expect("and decodes");
        assert_eq!(frame, back);
        assert_eq!(back.max_sample(), 65535);
    }

    #[test]
    fn a_mask_is_eight_bit_grayscale() {
        let png = encode(&gray(2, 2, &[0, 64, 192, 255])).expect("the mask encodes");
        let mask = decode_mask(&png, 2, 2).expect("and decodes");
        assert_eq!(mask, vec![0, 64, 192, 255]);
    }

    #[test]
    fn a_size_or_depth_that_disagrees_with_the_job_is_a_bad_request() {
        let png = encode(&rgb(2, 2, 8, &[0; 12])).expect("the frame encodes");
        for fail in [
            decode_image(&png, 3, 2, 3, 8).expect_err("the size is wrong"),
            decode_image(&png, 2, 2, 4, 8).expect_err("the channels are wrong"),
            decode_image(&png, 2, 2, 3, 16).expect_err("the depth is wrong"),
        ] {
            assert_eq!(fail.status, Status::BadRequest, "{fail}");
        }
    }

    #[test]
    fn a_mask_is_not_accepted_as_an_image() {
        let png = encode(&gray(2, 2, &[0; 4])).expect("the mask encodes");
        assert!(decode_image(&png, 2, 2, 3, 8).is_err());
    }

    #[test]
    fn bytes_that_are_not_a_png_are_a_bad_request() {
        let fail = decode_image(b"not a png", 2, 2, 3, 8).expect_err("this is not a PNG");
        assert_eq!(fail.status, Status::BadRequest, "{fail}");
    }

    #[test]
    fn the_raw_length_is_checked_before_anything_allocates() {
        assert_eq!(raw_byte_len(2, 2, 3, 8).expect("a small window"), 12);
        assert_eq!(raw_byte_len(2, 2, 3, 16).expect("a small 16-bit window"), 24);
        assert_eq!(raw_byte_len(u32::MAX, u32::MAX, 4, 16).expect_err("that overflows is refused").status, Status::TooLarge);
    }

    #[test]
    fn samples_outside_the_frame_are_none() {
        let frame = rgb(1, 1, 8, &[7, 8, 9]);
        assert_eq!(frame.sample(0, 2), Some(9));
        assert_eq!(frame.sample(0, 3), None);
        assert_eq!(frame.sample(1, 0), None);
        assert_eq!(frame.max_sample(), 255);
    }
}
