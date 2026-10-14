//! What can go wrong, and the status the protocol answers it with. Amendment 11 fixes the
//! shape: every error reply is `{"error": "<plain sentence>"}`, so every sentence here is one a
//! person can read in PhotoCraft's AI panel.

use thiserror::Error;

/// The status codes amendment 11 defines for this protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// 200: a normal answer.
    Ok,
    /// 202: the job is queued.
    Accepted,
    /// 204: done, with no body.
    NoContent,
    /// 400: bad input.
    BadRequest,
    /// 401: a missing or wrong token.
    Unauthorized,
    /// 404: unknown job, or an unknown path.
    NotFound,
    /// 409: the result is not ready yet.
    Conflict,
    /// 413: too large.
    TooLarge,
    /// 500: something went wrong here.
    Internal,
    /// 503: busy - the job queue is full, or too many connections are open.
    Busy,
}

impl Status {
    /// The number on the wire.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::Accepted => 202,
            Self::NoContent => 204,
            Self::BadRequest => 400,
            Self::Unauthorized => 401,
            Self::NotFound => 404,
            Self::Conflict => 409,
            Self::TooLarge => 413,
            Self::Internal => 500,
            Self::Busy => 503,
        }
    }

    /// The reason phrase next to the number.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Accepted => "Accepted",
            Self::NoContent => "No Content",
            Self::BadRequest => "Bad Request",
            Self::Unauthorized => "Unauthorized",
            Self::NotFound => "Not Found",
            Self::Conflict => "Conflict",
            Self::TooLarge => "Content Too Large",
            Self::Internal => "Internal Server Error",
            Self::Busy => "Service Unavailable",
        }
    }
}

/// An error and the status to answer it with. `Display` is the sentence, which is what the client
/// shows, so keep every one of them a plain sentence.
#[derive(Clone, Debug, Error)]
#[error("{sentence}")]
pub struct Fail {
    /// The status to answer with.
    pub status: Status,
    /// The plain sentence for the `{"error": ...}` body.
    pub sentence: String,
}

impl Fail {
    /// One error at one status.
    #[must_use]
    pub fn new(status: Status, sentence: impl Into<String>) -> Self {
        Self { status, sentence: sentence.into() }
    }

    #[must_use]
    pub fn bad_request(sentence: impl Into<String>) -> Self {
        Self::new(Status::BadRequest, sentence)
    }

    #[must_use]
    pub fn unauthorized(sentence: impl Into<String>) -> Self {
        Self::new(Status::Unauthorized, sentence)
    }

    #[must_use]
    pub fn not_found(sentence: impl Into<String>) -> Self {
        Self::new(Status::NotFound, sentence)
    }

    #[must_use]
    pub fn conflict(sentence: impl Into<String>) -> Self {
        Self::new(Status::Conflict, sentence)
    }

    #[must_use]
    pub fn too_large(sentence: impl Into<String>) -> Self {
        Self::new(Status::TooLarge, sentence)
    }

    #[must_use]
    pub fn internal(sentence: impl Into<String>) -> Self {
        Self::new(Status::Internal, sentence)
    }

    #[must_use]
    pub fn busy(sentence: impl Into<String>) -> Self {
        Self::new(Status::Busy, sentence)
    }
}

/// The result type the server modules use.
pub type Result<T> = std::result::Result<T, Fail>;

/// Text that came from the network, made safe to put inside a sentence: the client trims control
/// characters and clips at 300 characters, and so do we.
#[must_use]
pub fn quoted(text: &str) -> String {
    let clipped: String = text.chars().take(80).filter(|c| !c.is_control()).collect();
    let trimmed = clipped.trim();
    format!("\"{trimmed}\"")
}

/// A network failure is never the caller's fault to read about, so it becomes a 500 with a plain
/// sentence. A body that stopped short is bad input.
impl From<std::io::Error> for Fail {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::UnexpectedEof => return Self::bad_request("The request stopped before it was complete."),
            std::io::ErrorKind::TimedOut => return Self::bad_request("The request took too long to arrive."),
            _ => {}
        }
        Self::internal(format!("Craft AI Manager lost the connection: {error}"))
    }
}

/// A body we could not parse is bad input.
impl From<serde_json::Error> for Fail {
    fn from(error: serde_json::Error) -> Self {
        Self::bad_request(format!("The JSON is not the shape the protocol asks for ({error})."))
    }
}

impl From<png::DecodingError> for Fail {
    fn from(error: png::DecodingError) -> Self {
        Self::bad_request(format!("Craft AI Manager could not read one of the PNG images ({error})."))
    }
}

impl From<png::EncodingError> for Fail {
    fn from(error: png::EncodingError) -> Self {
        Self::internal(format!("Craft AI Manager could not write the result image ({error})."))
    }
}

#[cfg(test)]
mod tests {
    use super::{Fail, Status, quoted};

    #[test]
    fn statuses_match_the_protocol_numbers() {
        assert_eq!(Status::BadRequest.code(), 400);
        assert_eq!(Status::Unauthorized.code(), 401);
        assert_eq!(Status::NotFound.code(), 404);
        assert_eq!(Status::Conflict.code(), 409);
        assert_eq!(Status::TooLarge.code(), 413);
        assert_eq!(Status::Internal.code(), 500);
        assert_eq!(Status::Busy.code(), 503);
        assert_eq!(Status::TooLarge.reason(), "Content Too Large");
        assert_eq!(Status::Ok.code(), 200);
        assert_eq!(Status::Accepted.code(), 202);
        assert_eq!(Status::NoContent.code(), 204);
        assert_eq!(Status::NoContent.reason(), "No Content");
    }

    #[test]
    fn quoted_text_cannot_carry_control_characters_or_run_long() {
        assert_eq!(quoted("fill_masked_area"), "\"fill_masked_area\"");
        assert_eq!(quoted("a\r\nb"), "\"ab\"");
        assert_eq!(quoted(""), "\"\"");
        let long = "x".repeat(400);
        assert!(quoted(&long).chars().count() <= 82, "quoted text stays short");
    }

    #[test]
    fn an_io_error_becomes_a_readable_sentence() {
        let fail = Fail::from(std::io::Error::other("boom"));
        assert_eq!(fail.status, Status::Internal);
        assert!(fail.to_string().starts_with("Craft AI Manager lost the connection"));
    }
}
