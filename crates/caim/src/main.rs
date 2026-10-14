//! The `caim` binary: `caim serve` (also the default) runs the loopback server, `--fake-backend`
//! runs jobs on the built-in fake fill, and `--version` / `--help` answer without starting anything.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::IsTerminal;

use craft_ai_manager::{Action, EXIT_START, EXIT_USAGE, ServeOptions, Server, USAGE, VERSION, classify};

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
            println!("{USAGE}");
            println!("  serve           run the loopback server (this is also the default)");
            println!("  --fake-backend  run jobs on the built-in mean-colour fill");
            println!("Craft AI Manager {VERSION}: https://github.com/erikerikerik/craft-ai-manager");
            std::process::ExitCode::SUCCESS
        }
        Ok(Action::Serve { fake_backend }) => serve(fake_backend),
    }
}

/// Run the server until something asks it to stop, then shut down cleanly.
fn serve(fake_backend: bool) -> std::process::ExitCode {
    let mut server = match Server::start(ServeOptions { fake_backend, ..ServeOptions::default() }) {
        Ok(server) => server,
        Err(fail) => {
            eprintln!("{}", fail.sentence);
            return std::process::ExitCode::from(EXIT_START);
        }
    };
    let endpoint_variable = craft_ai_manager::endpoint::ENDPOINT_ENV;
    println!("Craft AI Manager {VERSION} listening on 127.0.0.1:{}", server.port());
    match server.endpoint_path() {
        Some(path) => println!("endpoint file: {}", path.display()),
        None => println!("endpoint file: none, so a client needs {endpoint_variable} to find this port"),
    }
    println!("backend: {}", if fake_backend { "fake-fill (the built-in mean-colour fill)" } else { "none: no model is installed yet" });
    if std::io::stdin().is_terminal() {
        println!("press Enter to stop cleanly. Ctrl+C works too, but on Windows it ends the process where it stands and leaves the endpoint file behind.");
        watch_stdin(server.stop_signal());
    }
    server.wait();
    match server.shutdown() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(fail) => {
            eprintln!("{}", fail.sentence);
            std::process::ExitCode::from(EXIT_START)
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
