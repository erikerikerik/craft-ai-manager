//! The command line: `caim` with no arguments serves, `caim serve --fake-backend` serves with the
//! fake backend, and `--version` / `--help` keep working without starting anything.

use crate::error::{Fail, Result, quoted};

/// What the binary should do with one set of arguments (the program name excluded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Print the version and exit.
    Version,
    /// Print how to use the binary and exit.
    Help,
    /// Run the loopback server until the process is asked to stop.
    Serve {
        /// Use the built-in fake backend, which fills a mask with the mean colour of the image.
        fake_backend: bool,
    },
}

/// The usage line, printed by `--help` and when the arguments do not parse.
pub const USAGE: &str = "usage: caim [serve] [--fake-backend] | caim --version | caim --help";

/// Map the arguments after the program name to an [Action](crate::Action). Serving is the default,
/// so the only positional argument is the optional `serve`. The error is a plain sentence, which
/// the binary prints before the usage line.
pub fn classify(args: &[String]) -> Result<Action> {
    let mut serve_seen = false;
    let mut fake_backend = false;
    let mut version_seen = false;
    let mut help_seen = false;
    for arg in args {
        match arg.as_str() {
            "--version" | "-V" => version_seen = true,
            "--help" | "-h" => help_seen = true,
            "--fake-backend" => fake_backend = true,
            "serve" if !serve_seen => serve_seen = true,
            other if other.starts_with('-') => return Err(Fail::bad_request(format!("Craft AI Manager does not know the option {}.", quoted(other)))),
            other => return Err(Fail::bad_request(format!("Craft AI Manager does not know what to do with {}.", quoted(other)))),
        }
    }
    if help_seen {
        return Ok(Action::Help);
    }
    if version_seen {
        return Ok(Action::Version);
    }
    Ok(Action::Serve { fake_backend })
}

#[cfg(test)]
mod tests {
    use super::{Action, USAGE, classify};
    use crate::error::Status;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn no_arguments_serves_without_the_fake_backend() {
        assert!(matches!(classify(&[]), Ok(Action::Serve { fake_backend: false })));
        assert!(matches!(classify(&args(&["serve"])), Ok(Action::Serve { fake_backend: false })));
    }

    #[test]
    fn the_fake_backend_flag_works_with_and_without_serve() {
        assert!(matches!(classify(&args(&["--fake-backend"])), Ok(Action::Serve { fake_backend: true })));
        assert!(matches!(classify(&args(&["serve", "--fake-backend"])), Ok(Action::Serve { fake_backend: true })));
        assert!(matches!(classify(&args(&["--fake-backend", "serve"])), Ok(Action::Serve { fake_backend: true })));
    }

    #[test]
    fn version_and_help_beat_serving() {
        assert!(matches!(classify(&args(&["--version"])), Ok(Action::Version)));
        assert!(matches!(classify(&args(&["-V"])), Ok(Action::Version)));
        assert!(matches!(classify(&args(&["--help"])), Ok(Action::Help)));
        assert!(matches!(classify(&args(&["-h"])), Ok(Action::Help)));
        assert!(matches!(classify(&args(&["serve", "--help"])), Ok(Action::Help)));
    }

    #[test]
    fn unknown_arguments_are_a_plain_sentence_and_a_bad_request() {
        let fail = classify(&args(&["--nope"])).expect_err("an unknown option is an error");
        assert_eq!(fail.status, Status::BadRequest);
        assert!(fail.sentence.contains("--nope"), "the sentence names the option: {fail}");
        assert!(classify(&args(&["run"])).is_err());
        assert!(classify(&args(&["serve", "serve"])).is_err());
    }

    #[test]
    fn the_usage_line_mentions_the_flags() {
        assert!(USAGE.contains("--fake-backend"));
        assert!(USAGE.contains("--version"));
    }
}
