//! Craft AI Manager (CAIM): the small local service that runs AI models for PhotoCraft over a
//! loopback HTTP API. It is part of the PhotoCraft AI project. The contract lives in the PhotoCraft
//! fork (`erikerikerik/photocraft`, `agents/SPEC.md`, v1.1 amendments 11, 13 and 14) and the task
//! list in that repo's `agents/TASKS.md` (the MG-* rows).
//!
//! MG-01 is the server: bind `127.0.0.1:0`, a token per start, `endpoint.json`, the `/v1/…`
//! endpoints, the job queue, and a built-in fake backend so the protocol can be tested end to end.
//! Real models come in MG-02 (the model store) and MG-04 (the ONNX runner), which plug into
//! `job` without changing the wire contract.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

/// The built-in fake backend: a mean-colour fill, so MG-01 can answer jobs with no model installed.
pub mod backend;
/// The command line.
pub mod cli;
/// `endpoint.json`: where the port and token are published, atomically.
pub mod endpoint;
/// Errors, and the status each one is answered with.
pub mod error;
/// The HTTP/1.1 framing: request head, bounded body, one reply per connection.
pub mod http;
/// The PNG halves of the protocol: `image`, `mask`, `result.png`.
pub mod image;
/// The job queue, the worker thread, and the states a job moves through.
pub mod job;
/// A bounded `multipart/form-data` parser for exactly the three parts the protocol defines.
pub mod multipart;
/// The JSON of the protocol: what a job asks for and what each endpoint answers.
pub mod protocol;
/// The loopback server itself.
pub mod server;
/// The per-start token, and the constant-time check every request goes through.
pub mod token;

/// The package version, which `caim --version` prints and `/v1/health` reports as `caim/<version>`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Exit code for "the arguments do not make sense".
pub const EXIT_USAGE: u8 = 2;

/// Exit code for "the server could not start, or stopped badly".
pub const EXIT_START: u8 = 1;

pub use cli::{Action, USAGE, classify};
pub use server::{ServeOptions, Server};

#[cfg(test)]
mod tests {
    use super::{EXIT_START, EXIT_USAGE, VERSION};

    #[test]
    fn the_crate_reports_a_version_and_two_distinct_exit_codes() {
        assert!(!VERSION.is_empty());
        assert_ne!(EXIT_USAGE, EXIT_START);
        const { assert!(EXIT_USAGE > 0 && EXIT_START > 0, "a shell needs a non-zero code to notice") };
    }

    #[test]
    fn the_modules_are_reachable_from_the_crate_root() {
        // The binary and the integration tests both go through these names.
        let options = super::ServeOptions::default();
        assert!(!options.fake_backend);
        assert!(matches!(super::classify(&[]), Ok(super::Action::Serve { .. })));
    }
}
