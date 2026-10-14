//! Craft AI Manager (CAIM): the small local service that runs AI models for PhotoCraft over a
//! loopback HTTP API. It is part of the PhotoCraft AI project. The contract lives in the PhotoCraft
//! fork (`erikerikerik/photocraft`, `agents/SPEC.md`, v1.1 amendments 11, 13 and 14) and the task
//! list in that repo's `agents/TASKS.md` (the MG-* rows).
//!
//! MG-00 is the scaffold only: the crate builds, lints, tests, and answers `--version`. The server
//! - bind `127.0.0.1:0`, a token per start, `endpoint.json`, `/v1/health` and the job API - is MG-01.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

/// The package version, which `caim --version` prints.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Exit code for "the manager is not implemented yet". MG-01 replaces it with the server.
pub const EXIT_NOT_IMPLEMENTED: u8 = 2;

/// What the binary should do with one set of arguments (the program name excluded).
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Print [`VERSION`].
    Version,
    /// Say the manager is not implemented yet, and exit with [`EXIT_NOT_IMPLEMENTED`].
    NotImplemented,
}

/// Map the arguments after the program name to an [`Action`]. Everything except a version request
/// means "not implemented yet" until MG-01, so unknown flags are not an error here.
#[must_use]
pub fn classify(args: &[String]) -> Action {
    match args {
        [] => Action::NotImplemented,
        [arg] if arg == "--version" || arg == "-V" => Action::Version,
        _ => Action::NotImplemented,
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, EXIT_NOT_IMPLEMENTED, VERSION, classify};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn version_flags_ask_for_the_version() {
        assert_eq!(classify(&args(&["--version"])), Action::Version);
        assert_eq!(classify(&args(&["-V"])), Action::Version);
    }

    #[test]
    fn everything_else_is_not_implemented() {
        assert_eq!(classify(&args(&[])), Action::NotImplemented);
        assert_eq!(classify(&args(&["serve"])), Action::NotImplemented);
        assert_eq!(classify(&args(&["--version", "--verbose"])), Action::NotImplemented);
    }

    #[test]
    fn scaffold_reports_a_version_and_a_distinct_exit_code() {
        assert!(!VERSION.is_empty());
        assert_eq!(EXIT_NOT_IMPLEMENTED, 2);
    }
}
