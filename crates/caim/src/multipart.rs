//! A bounded `multipart/form-data` parser for exactly the three parts amendment 11 defines:
//! `job`, `image` and `mask`, in that order. No multipart crate: the shape is fixed, so the parser
//! is a scan over the body with a cap on every part of it.
//!
//! The body arrives whole (the framing in `http` already capped it), so nothing here streams and
//! nothing here allocates beyond what the body already holds.

use crate::error::{Fail, Result};
use crate::http::find;

/// A boundary is a header value: bounded, and no control characters.
pub const MAX_BOUNDARY_BYTES: usize = 70;
/// The headers of one part.
pub const MAX_PART_HEADER_BYTES: usize = 8 * 1024;
/// Headers inside one part.
pub const MAX_PART_HEADERS: usize = 8;
/// A part name, from `Content-Disposition`.
pub const MAX_PART_NAME_BYTES: usize = 256;
/// The protocol has three parts; anything else is a bad request, and the scan stops there.
pub const PART_COUNT: usize = 3;

/// The names the parts must have, in this order.
pub const PART_NAMES: [&str; PART_COUNT] = ["job", "image", "mask"];

/// One part of the body.
#[derive(Clone, Copy, Debug)]
pub struct Part<'a> {
    pub name: &'a str,
    pub content_type: Option<&'a str>,
    pub data: &'a [u8],
}

/// The three parts, already checked for order.
#[derive(Clone, Copy, Debug)]
pub struct JobParts<'a> {
    pub job: &'a [u8],
    pub image: &'a [u8],
    pub mask: &'a [u8],
}

/// The boundary a `Content-Type` declares, with the parameter name matched case-insensitively.
pub fn boundary(content_type: &str) -> Result<&str> {
    let (main, rest) = content_type.split_once(';').unwrap_or((content_type, ""));
    if !main.trim().eq_ignore_ascii_case("multipart/form-data") {
        return Err(Fail::bad_request("The job body is not multipart/form-data, so Craft AI Manager cannot read its parts."));
    }
    for param in rest.split(';') {
        let Some((name, raw)) = param.split_once('=') else { continue };
        if !name.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        let value = raw.trim();
        let unquoted = value.strip_prefix('"').and_then(|inner| inner.strip_suffix('"')).unwrap_or(value);
        if unquoted.is_empty() || unquoted.len() > MAX_BOUNDARY_BYTES {
            return Err(Fail::bad_request("The multipart boundary is empty or longer than 70 characters."));
        }
        let usable =
            unquoted.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'\'' | b'+' | b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b'?' | b' '))
                && !unquoted.ends_with(' ');
        if !usable {
            return Err(Fail::bad_request("The multipart boundary has characters that are not allowed."));
        }
        return Ok(unquoted);
    }
    Err(Fail::bad_request("The multipart Content-Type does not say where its boundary is."))
}

/// Split the body into its three parts.
///
/// The parts must be named `job`, `image` and `mask` and come in that order; anything else is a
/// plain sentence, because the client's error text is what the user reads.
pub fn parse<'a>(content_type: Option<&str>, body: &'a [u8]) -> Result<JobParts<'a>> {
    let Some(content_type) = content_type else {
        return Err(Fail::bad_request("The job body has no Content-Type, so Craft AI Manager cannot find its parts."));
    };
    let boundary = boundary(content_type)?;
    let delimiter = format!("--{boundary}");
    let marker = delimiter.as_bytes();
    let mut closing = Vec::with_capacity(marker.len().saturating_add(2));
    closing.extend_from_slice(b"\r\n");
    closing.extend_from_slice(marker);

    if !body.starts_with(marker) {
        return Err(Fail::bad_request("The job body does not start with the multipart boundary."));
    }
    let mut pos = marker.len();
    let mut parts: Vec<Part<'_>> = Vec::with_capacity(PART_COUNT.saturating_add(1));
    loop {
        if parts.len() > PART_COUNT {
            return Err(Fail::bad_request("The job body has more parts than the three the protocol defines."));
        }
        let rest = body.get(pos..).unwrap_or(&[]);
        if rest.starts_with(b"--") {
            // The closing "--boundary" marker: the body is complete.
            break;
        }
        if !rest.starts_with(b"\r\n") {
            return Err(Fail::bad_request("A multipart boundary is not followed by a line break."));
        }
        pos = pos.saturating_add(2);
        let Some(header_end) = find(body, pos, b"\r\n\r\n").filter(|at| at.saturating_sub(pos) <= MAX_PART_HEADER_BYTES) else {
            return Err(Fail::bad_request("A multipart part has no headers, or headers longer than the 8 KiB the protocol allows."));
        };
        let headers = part_headers(body.get(pos..header_end).unwrap_or(&[]))?;
        let data_start = header_end.saturating_add(4);
        let Some(at) = find(body, data_start, &closing) else {
            return Err(Fail::bad_request("The multipart body is missing the boundary that closes a part."));
        };
        parts.push(Part { name: headers.name, content_type: headers.content_type, data: body.get(data_start..at).unwrap_or(&[]) });
        pos = at.saturating_add(closing.len());
    }
    if parts.len() != PART_COUNT {
        return Err(Fail::bad_request(format!("The job body has {} parts; the protocol asks for exactly three: job, image and mask.", parts.len())));
    }
    let names: Vec<&str> = parts.iter().map(|part| part.name).collect();
    if names != PART_NAMES {
        return Err(Fail::bad_request(format!("The job body parts must be job, image and mask in that order; this one sends {}.", names.join(", "))));
    }
    let [job, image, mask] = parts.as_slice() else {
        return Err(Fail::bad_request("The job body has the wrong number of parts."));
    };
    Ok(JobParts { job: job.data, image: image.data, mask: mask.data })
}

/// The two part headers that matter; the rest are ignored, like the client ignores ours.
struct PartHeaders<'a> {
    name: &'a str,
    content_type: Option<&'a str>,
}

fn part_headers(block: &[u8]) -> Result<PartHeaders<'_>> {
    let text = std::str::from_utf8(block).map_err(|_| Fail::bad_request("A multipart part has headers that are not text."))?;
    let mut name: Option<&str> = None;
    let mut content_type: Option<&str> = None;
    let mut seen = 0usize;
    for line in text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        seen = seen.saturating_add(1);
        if seen > MAX_PART_HEADERS {
            return Err(Fail::bad_request("A multipart part has more headers than the protocol allows."));
        }
        let Some((field, value)) = line.split_once(':') else {
            return Err(Fail::bad_request("A multipart part has a header line without a name."));
        };
        if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
            return Err(Fail::bad_request("A multipart part has a header with an unusable name."));
        }
        let value = value.trim();
        if !value.is_ascii() {
            return Err(Fail::bad_request("A multipart part has a header that is not plain text."));
        }
        match field.to_ascii_lowercase().as_str() {
            "content-disposition" => name = Some(disposition_name(value)?),
            "content-type" => content_type = Some(value),
            _ => {}
        }
    }
    let Some(name) = name else {
        return Err(Fail::bad_request("A multipart part has no name in its Content-Disposition."));
    };
    Ok(PartHeaders { name, content_type })
}

/// The `name` parameter of `Content-Disposition: form-data; name="job"`.
fn disposition_name(value: &str) -> Result<&str> {
    if !value.get(..9).is_some_and(|head| head.eq_ignore_ascii_case("form-data")) {
        return Err(Fail::bad_request("A multipart part is not form-data, which is the only kind the protocol sends."));
    }
    for param in value.split(';').skip(1) {
        let Some((field, raw)) = param.split_once('=') else { continue };
        if !field.trim().eq_ignore_ascii_case("name") {
            continue;
        }
        let trimmed = raw.trim();
        let unquoted = trimmed.strip_prefix('"').and_then(|inner| inner.strip_suffix('"')).unwrap_or(trimmed);
        if unquoted.is_empty() || unquoted.len() > MAX_PART_NAME_BYTES || !unquoted.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(Fail::bad_request("A multipart part has a name that is empty, too long, or not plain text."));
        }
        return Ok(unquoted);
    }
    Err(Fail::bad_request("A multipart part has no name in its Content-Disposition."))
}

#[cfg(test)]
mod tests {
    use super::{PART_NAMES, boundary, parse};
    use crate::error::Status;

    /// The body the PhotoCraft client builds, part for part.
    fn body(boundary: &str, parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, content_type, data) in parts {
            out.extend_from_slice(b"--");
            out.extend_from_slice(boundary.as_bytes());
            out.extend_from_slice(
                format!("\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes(),
            );
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"--\r\n");
        out
    }

    fn three(boundary: &str) -> Vec<u8> {
        body(
            boundary,
            &[("job", "application/json", br#"{"task": "fill_masked_area"}"#), ("image", "image/png", b"image bytes"), ("mask", "image/png", b"mask bytes")],
        )
    }

    #[test]
    fn the_happy_path_splits_into_the_three_parts() {
        let built = three("caim-test");
        let parts = parse(Some("multipart/form-data; boundary=caim-test"), &built).expect("the client body parses");
        assert_eq!(parts.job, br#"{"task": "fill_masked_area"}"#);
        assert_eq!(parts.image, b"image bytes");
        assert_eq!(parts.mask, b"mask bytes");
    }

    #[test]
    fn a_quoted_boundary_and_a_preamble_are_both_refused() {
        let built = three("quoted-boundary");
        let parts = parse(Some("multipart/form-data; boundary=\"quoted-boundary\""), &built).expect("a quoted boundary works");
        assert_eq!(parts.mask, b"mask bytes");
        let with_preamble = [b"ignore me\r\n".to_vec(), built.clone()].concat();
        assert!(parse(Some("multipart/form-data; boundary=quoted-boundary"), &with_preamble).is_err());
    }

    #[test]
    fn parts_out_of_order_are_refused_with_the_names_in_the_sentence() {
        let built = body("swap", &[("job", "application/json", b"{}"), ("mask", "image/png", b"m"), ("image", "image/png", b"i")]);
        let fail = parse(Some("multipart/form-data; boundary=swap"), &built).expect_err("the order matters");
        assert_eq!(fail.status, Status::BadRequest);
        assert!(fail.sentence.contains("mask, image"), "the sentence names the order: {fail}");
    }

    #[test]
    fn a_missing_or_extra_part_is_refused() {
        let two = body("two", &[("job", "application/json", b"{}"), ("image", "image/png", b"i")]);
        assert_eq!(parse(Some("multipart/form-data; boundary=two"), &two).expect_err("two parts is not three").status, Status::BadRequest);
        let four = body("four", &[("job", "application/json", b"{}"), ("image", "image/png", b"i"), ("mask", "image/png", b"m"), ("mask", "image/png", b"m2")]);
        assert!(parse(Some("multipart/form-data; boundary=four"), &four).is_err());
    }

    #[test]
    fn garbage_bodies_are_bad_requests_not_panics() {
        let built = three("garbage");
        for broken in [Vec::new(), b"--garbage\r\n".to_vec(), b"--garbage\r\nno headers here".to_vec(), built.get(..8).unwrap_or(&[]).to_vec()] {
            let fail = parse(Some("multipart/form-data; boundary=garbage"), &broken).expect_err("this body cannot be read");
            assert_eq!(fail.status, Status::BadRequest, "{fail}");
        }
        assert!(parse(None, &built).is_err());
        assert!(parse(Some("application/json"), &built).is_err());
        assert!(parse(Some("multipart/form-data"), &built).is_err());
        assert!(parse(Some("multipart/form-data; boundary="), &built).is_err());
        assert!(parse(Some("multipart/form-data; boundary=bad boundary\""), &built).is_err());
    }

    #[test]
    fn a_part_with_no_content_disposition_is_refused() {
        let built = b"--x\r\nContent-Type: image/png\r\n\r\ndata\r\n--x--\r\n".to_vec();
        assert!(parse(Some("multipart/form-data; boundary=x"), &built).is_err());
    }

    #[test]
    fn the_part_names_are_the_protocol_order() {
        assert_eq!(PART_NAMES, ["job", "image", "mask"]);
        assert_eq!(boundary("multipart/form-data; BOUNDARY=abc").expect("the parameter name is case-insensitive"), "abc");
    }
}
