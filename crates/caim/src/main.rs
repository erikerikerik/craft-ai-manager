//! The `caim` binary: `caim serve` (also the default) runs the loopback server, `--fake-backend`
//! runs jobs on the built-in fake fill, `caim models ...` looks after the model store, and
//! `--version` / `--help` answer without starting anything.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::IsTerminal;
use std::io::Write;

use craft_ai_manager::error::{Fail, Status};
use craft_ai_manager::models;
use craft_ai_manager::{Action, EXIT_FAILED, EXIT_MODEL, EXIT_USAGE, ServeOptions, Server, USAGE, VERSION, classify};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match classify(&args) {
        Err(fail) => {
            eprintln!("{}", fail.sentence);
            eprintln!("{USAGE}");
            std::process::ExitCode::from(EXIT_USAGE)
        }
        Ok(Action::Version) => {
            println!("caim {VERSION}");
            std::process::ExitCode::SUCCESS
        }
        Ok(Action::Help) => {
            help();
            std::process::ExitCode::SUCCESS
        }
        Ok(Action::Serve { fake_backend }) => serve(fake_backend),
        Ok(Action::ModelsList) => models_list(),
        Ok(Action::ModelsInstall { id, accept_note }) => models_install(&id, accept_note),
        Ok(Action::ModelsVerify { id }) => models_verify(&id),
    }
}

/// What `caim --help` prints: the usage lines, one plain sentence per command, and the exit codes.
fn help() {
    println!("{USAGE}");
    println!("  serve             run the loopback server (this is also the default)");
    println!("  --fake-backend    run jobs on the built-in mean-colour fill, to try the protocol out");
    println!("  models list       one line per catalog entry: task, size, licences, and whether it is installed");
    println!("  models install    download one model, hashing it while it arrives");
    println!("  --accept-note     accept a flagged model's licence note and go ahead with the download");
    println!("  models verify     re-hash an installed model against the catalog");
    println!("exit codes: 0 everything is fine, 1 something went wrong, 2 the arguments do not make sense,");
    println!("            3 the model is unknown, is not installed, or its note needs acknowledging");
    println!("Craft AI Manager {VERSION}: https://github.com/erikerikerik/craft-ai-manager");
}

/// Run the server until something asks it to stop, then shut down cleanly.
fn serve(fake_backend: bool) -> std::process::ExitCode {
    let mut server = match Server::start(ServeOptions { fake_backend, ..ServeOptions::default() }) {
        Ok(server) => server,
        Err(fail) => return report(fail),
    };
    let endpoint_variable = craft_ai_manager::endpoint::ENDPOINT_ENV;
    println!("Craft AI Manager {VERSION} listening on 127.0.0.1:{}", server.port());
    match server.endpoint_path() {
        Some(path) => println!("endpoint file: {}", path.display()),
        None => println!("endpoint file: none, so a client needs {endpoint_variable} to find this port"),
    }
    println!("backend: {}", server.backend_note());
    if std::io::stdin().is_terminal() {
        println!("press Enter to stop cleanly. Ctrl+C works too, but on Windows it ends the process where it stands and leaves the endpoint file behind.");
        watch_stdin(server.stop_signal());
    }
    server.wait();
    match server.shutdown() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(fail) => report(fail),
    }
}

/// `caim models list`: one line per catalog entry, then the state of each on disk.
fn models_list() -> std::process::ExitCode {
    match models::list() {
        Ok(lines) => {
            if lines.is_empty() {
                println!("The model catalog has no entries.");
            }
            for line in lines {
                println!("{line}");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(fail) => report(fail),
    }
}

/// `caim models install <id> [--accept-note]`: the download prints a percentage while it runs.
fn models_install(id: &str, accept_note: bool) -> std::process::ExitCode {
    let mut progress = Progress::default();
    let outcome = models::install(id, accept_note, &mut |read, total| progress.report(read, total));
    progress.finish();
    match outcome {
        Ok(message) => {
            println!("{message}");
            std::process::ExitCode::SUCCESS
        }
        Err(fail) => report(fail),
    }
}

/// `caim models verify <id>`: re-hash the file on disk against the catalog.
fn models_verify(id: &str) -> std::process::ExitCode {
    match models::verify(id) {
        Ok(message) => {
            println!("{message}");
            std::process::ExitCode::SUCCESS
        }
        Err(fail) => report(fail),
    }
}

/// Print the sentence and pick the exit code: a model that is missing, not installed, or waiting on
/// an acknowledgment is a different kind of no from a download that failed.
fn report(fail: Fail) -> std::process::ExitCode {
    eprintln!("{}", fail.sentence);
    let code = match fail.status {
        Status::BadRequest => EXIT_USAGE,
        Status::NotFound | Status::Conflict => EXIT_MODEL,
        _ => EXIT_FAILED,
    };
    std::process::ExitCode::from(code)
}

/// The one-line progress report for a download: a percentage that only redraws when it changes, and
/// a newline at the end so the next sentence starts on a line of its own.
#[derive(Default)]
struct Progress {
    percent: u64,
    printed: bool,
}

impl Progress {
    fn report(&mut self, read: u64, total: u64) {
        let percent = read.saturating_mul(100).checked_div(total).unwrap_or(100);
        if percent == self.percent {
            return;
        }
        self.percent = percent;
        self.printed = true;
        print!("\r  {percent}% of {total} bytes");
        let _ = std::io::stdout().flush();
    }

    fn finish(&mut self) {
        if self.printed {
            println!();
            let _ = std::io::stdout().flush();
        }
    }
}

/// A Ctrl+C on Windows ends the process where it stands, which would leave the endpoint file
/// behind, so when stdin is a terminal Enter stops the server properly instead. A stale file is
/// harmless - a client sees a refused connection, and the next start replaces it - but this way the
/// shutdown is clean and the file goes away. There is no signal handler in `std`, and MG-01 adds no
/// dependency for one.
fn watch_stdin(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let _ = std::thread::Builder::new().name("caim-stdin".to_string()).spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
}
