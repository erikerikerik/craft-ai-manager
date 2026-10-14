//! The command line: `caim` with no arguments serves, `caim serve --fake-backend` serves with the
//! fake backend, `caim models ...` looks after the model store, and `--version` / `--help` keep
//! working without starting anything.

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
    /// Print one line per catalog entry: what it is, its licences, and whether it is installed.
    ModelsList,
    /// Download one model, unless it is already installed.
    ModelsInstall {
        /// The model id, as the catalog writes it.
        id: String,
        /// Accept the model's licence note and go ahead with the download.
        accept_note: bool,
    },
    /// Re-hash an installed model against the catalog.
    ModelsVerify {
        /// The model id, as the catalog writes it.
        id: String,
    },
}

/// The usage lines, printed by `--help` and when the arguments do not parse.
pub const USAGE: &str = "usage: caim [serve] [--fake-backend]\n       caim models list\n       caim models install <id> [--accept-note]\n       caim models verify <id>\n       caim --version | caim --help";

/// Map the arguments after the program name to an [Action](crate::Action). Serving is the default, so
/// the only positional words are the optional `serve` and the `models` commands. The error is a
/// plain sentence, which the binary prints before the usage line.
pub fn classify(args: &[String]) -> Result<Action> {
    let mut fake_backend = false;
    let mut accept_note = false;
    let mut version_seen = false;
    let mut help_seen = false;
    let mut words: Vec<&str> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--version" | "-V" => version_seen = true,
            "--help" | "-h" => help_seen = true,
            "--fake-backend" => fake_backend = true,
            "--accept-note" => accept_note = true,
            other if other.starts_with('-') => return Err(Fail::bad_request(format!("Craft AI Manager does not know the option {}.", quoted(other)))),
            other => words.push(other),
        }
    }
    if help_seen {
        return Ok(Action::Help);
    }
    if version_seen {
        return Ok(Action::Version);
    }
    let action = match words.as_slice() {
        [] | ["serve"] => Action::Serve { fake_backend },
        ["models"] => return Err(Fail::bad_request("Craft AI Manager needs a models command: list, install or verify.")),
        ["models", "list"] => Action::ModelsList,
        ["models", "install"] => return Err(Fail::bad_request("Craft AI Manager needs a model id: caim models install <id>.")),
        ["models", "verify"] => return Err(Fail::bad_request("Craft AI Manager needs a model id: caim models verify <id>.")),
        ["models", "install", id] => Action::ModelsInstall { id: (*id).to_string(), accept_note },
        ["models", "verify", id] => Action::ModelsVerify { id: (*id).to_string() },
        ["models", other] => {
            return Err(Fail::bad_request(format!("Craft AI Manager does not know the models command {}. Try list, install or verify.", quoted(other))));
        }
        other => return Err(Fail::bad_request(format!("Craft AI Manager does not know what to do with {}.", quoted(&other.join(" "))))),
    };
    if fake_backend && !matches!(action, Action::Serve { .. }) {
        return Err(Fail::bad_request("The --fake-backend flag goes with caim serve, not with this command."));
    }
    if accept_note && !matches!(action, Action::ModelsInstall { .. }) {
        return Err(Fail::bad_request("The --accept-note flag goes with caim models install <id>, not with this command."));
    }
    Ok(action)
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

    #[test]
    fn the_models_commands_parse() {
        assert_eq!(classify(&args(&["models", "list"])).expect("list is a models command"), Action::ModelsList);
        assert_eq!(
            classify(&args(&["models", "install", "lama"])).expect("install takes an id"),
            Action::ModelsInstall { id: "lama".to_string(), accept_note: false }
        );
        assert_eq!(classify(&args(&["models", "verify", "lama"])).expect("verify takes an id"), Action::ModelsVerify { id: "lama".to_string() });
    }

    #[test]
    fn accept_note_can_come_before_or_after_the_id() {
        let wanted = Action::ModelsInstall { id: "lama".to_string(), accept_note: true };
        assert_eq!(classify(&args(&["models", "install", "lama", "--accept-note"])).expect("the flag can come last"), wanted.clone());
        assert_eq!(classify(&args(&["models", "install", "--accept-note", "lama"])).expect("the flag can come first"), wanted);
    }

    #[test]
    fn a_models_command_without_an_id_is_a_plain_sentence() {
        for command in ["install", "verify"] {
            let fail = classify(&args(&["models", command])).expect_err("a model id is needed");
            assert_eq!(fail.status, Status::BadRequest);
            assert!(fail.sentence.contains("<id>"), "the sentence says what is missing: {fail}");
        }
        assert!(classify(&args(&["models"])).is_err());
        assert!(classify(&args(&["models", "list", "lama"])).is_err());
    }

    #[test]
    fn an_unknown_models_command_names_the_command() {
        let fail = classify(&args(&["models", "upload"])).expect_err("upload is not a models command");
        assert_eq!(fail.status, Status::BadRequest);
        assert!(fail.sentence.contains("upload"), "the sentence names the command: {fail}");
    }

    #[test]
    fn a_flag_only_fits_the_command_it_belongs_to() {
        let fail = classify(&args(&["models", "list", "--fake-backend"])).expect_err("the fake backend is a serve flag");
        assert!(fail.sentence.contains("--fake-backend"), "{fail}");
        let fail = classify(&args(&["serve", "--accept-note"])).expect_err("the note belongs to an install");
        assert!(fail.sentence.contains("--accept-note"), "{fail}");
        assert!(classify(&args(&["models", "verify", "lama", "--accept-note"])).is_err());
    }

    #[test]
    fn the_usage_line_mentions_the_models_commands() {
        assert!(USAGE.contains("models list"));
        assert!(USAGE.contains("models install"));
        assert!(USAGE.contains("models verify"));
        assert!(USAGE.contains("--accept-note"));
        assert!(USAGE.contains("models install <id>"), "the usage line shows a model id, not a stray bracket");
        assert!(USAGE.contains("models verify <id>"), "the usage line shows a model id, not a stray bracket");
    }
}
