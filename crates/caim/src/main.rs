//! The `caim` binary: `caim serve` (also the default) runs the loopback server, `--fake-backend`
//! runs jobs on the built-in fake fill, `--device` asks for a backend, `caim open` starts the manager
//! unless one is already answering, `caim models ...` looks after the model store, `caim cuda ...`
//! looks after the CUDA libraries, and `--version` / `--help` answer without starting anything.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::IsTerminal;
use std::io::Write;

use craft_ai_manager::cuda;
use craft_ai_manager::device;
use craft_ai_manager::endpoint::{self, Launch};
use craft_ai_manager::error::{Fail, Status};
use craft_ai_manager::models;
use craft_ai_manager::server::StopHandle;
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
        Ok(Action::Serve { fake_backend, device }) => serve(fake_backend, device),
        Ok(Action::Open) => open(),
        Ok(Action::ModelsList) => models_list(),
        Ok(Action::ModelsInstall { id, accept_note }) => models_install(&id, accept_note),
        Ok(Action::ModelsVerify { id }) => models_verify(&id),
        Ok(Action::ModelsRemove { id }) => models_remove(&id),
        Ok(Action::ModelsWhere) => models_where(),
        Ok(Action::CudaStatus) => cuda_status(),
        Ok(Action::CudaInstall { accept_note }) => cuda_install(accept_note),
    }
}

/// What `caim --help` prints: the usage lines, one plain sentence per command, and the exit codes.
fn help() {
    println!("{USAGE}");
    println!("  serve             run the loopback server (this is also the default)");
    println!("  open              start the manager, or say that one is already running");
    println!("  --fake-backend    run jobs on the built-in mean-colour fill, to try the protocol out");
    println!("  --device          ask for a backend for every job: auto, cuda or cpu (default auto)");
    println!("  models list       one line per catalog entry: task, size, licences, and whether it is installed");
    println!("  models install    download one model, hashing it while it arrives");
    println!("  --accept-note     accept a flagged model's licence note and go ahead with the download");
    println!("  models verify     re-hash an installed model against the catalog");
    println!("  models remove     delete one model's files from disk (refused while a manager is running)");
    println!("  models where      print the models folder");
    println!("  cuda status       the CUDA libraries, the cards this machine has, and whether a CUDA job could run");
    println!("  cuda install      download the CUDA libraries NVIDIA's own licence covers (about 1.9 GB)");
    println!("exit codes: 0 everything is fine, 1 something went wrong, 2 the arguments do not make sense,");
    println!("            3 the model is unknown, is not installed, or its note needs acknowledging");
    println!("Craft AI Manager {VERSION}: https://github.com/erikerikerik/craft-ai-manager");
}

/// Run the server until something asks it to stop, then shut down cleanly.
fn serve(fake_backend: bool, device: device::Setting) -> std::process::ExitCode {
    serve_and_release(fake_backend, device, None)
}

/// [serve], handing over the launch lock this run took as `caim open`. The lock goes as soon as
/// the server is listening and `endpoint.json` names this port, which is the moment a second
/// `caim open` can stop waiting and see a running manager instead of starting another one.
fn serve_and_release(fake_backend: bool, device: device::Setting, launch_lock: Option<endpoint::LaunchLock>) -> std::process::ExitCode {
    // MG-08 review round 2: the signal handler goes in before `Server::start` writes the endpoint
    // file, so a supervisor that stops the process the moment it can find the port still gets the
    // clean stop instead of the operating system's default handling.
    let stop = StopHandle::new();
    if let Err(fail) = watch_signals(&stop) {
        // A machine that will not take the handler still stops on Enter, and `Server::drop` still
        // removes the endpoint file, so this is a note rather than a reason not to run.
        eprintln!("{}", fail.sentence);
    }
    let mut server = match Server::start_with_stop(ServeOptions { fake_backend, device, ..ServeOptions::default() }, &stop) {
        Ok(server) => server,
        Err(fail) => return report(fail),
    };
    drop(launch_lock);
    let endpoint_variable = endpoint::ENDPOINT_ENV;
    println!("Craft AI Manager {VERSION} listening on 127.0.0.1:{}", server.port());
    match server.endpoint_path() {
        Some(path) => println!("endpoint file: {}", path.display()),
        None => println!("endpoint file: none, so a client needs {endpoint_variable} to find this port"),
    }
    println!("backend: {}", server.backend_note());
    if std::io::stdin().is_terminal() {
        println!("press Enter or Ctrl+C to stop cleanly. A second Ctrl+C leaves at once.");
        watch_stdin(server.stop_signal());
    }
    server.wait();
    match server.shutdown() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(fail) => report(fail),
    }
}

/// `caim open`: start the manager unless one is already answering on loopback (amendment 18).
/// When nothing is answering, this is `caim serve` for now - MG-13 gives the manager its window.
///
/// The decision and the start are one step rather than two: a second `caim open` launched at the
/// same moment waits on a lock file beside the endpoint file instead of starting a manager that
/// nobody can find, and both writing the same activity log. Only a lock another run is genuinely
/// holding counts as "somebody else is starting one"; a lock this machine cannot make at all is an
/// error, because then no manager is coming.
fn open() -> std::process::ExitCode {
    let endpoint_file = endpoint::endpoint_file_from_env();
    let launch_lock = match endpoint_file.as_deref().map(endpoint::launch_lock_path_for) {
        Some(path) => match endpoint::LaunchLock::take(&path) {
            endpoint::LockOutcome::Taken(lock) => Some(lock),
            endpoint::LockOutcome::Held => {
                println!("Craft AI Manager is being started by another `caim open` right now. Nothing was started here.");
                return std::process::ExitCode::SUCCESS;
            }
            // No lock, and no other run holding one: the folder or the disk is the problem, and a
            // manager started here could not publish an endpoint file anybody could read.
            endpoint::LockOutcome::Failed(fail) => return report(fail),
        },
        // Nowhere on this platform to put an endpoint file, so nothing to lock either.
        None => None,
    };
    match endpoint::launch_decision(endpoint_file.as_deref()) {
        Launch::AlreadyRunning { port } => {
            println!("Craft AI Manager is already running on 127.0.0.1:{port}. Nothing was started.");
            std::process::ExitCode::SUCCESS
        }
        Launch::Start => serve_and_release(false, device::Setting::Auto, launch_lock),
    }
}

/// `caim models list`: one line per catalog entry with the bytes it holds on disk, then the total
/// for the models folder beside the folder itself.
fn models_list() -> std::process::ExitCode {
    match models::list() {
        Ok(lines) => {
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

/// `caim models remove <id>`: delete one model's files. Refused while a manager is listening,
/// because that manager may be using the model.
fn models_remove(id: &str) -> std::process::ExitCode {
    match models::remove(id) {
        Ok(message) => {
            println!("{message}");
            std::process::ExitCode::SUCCESS
        }
        Err(fail) => report(fail),
    }
}

/// `caim models where`: the models folder, and nothing else.
fn models_where() -> std::process::ExitCode {
    match models::where_folder() {
        Ok(message) => {
            println!("{message}");
            std::process::ExitCode::SUCCESS
        }
        Err(fail) => report(fail),
    }
}

/// `caim cuda status`: the four archives, the cards the probe found, and whether a CUDA job could
/// actually run here (MG-11).
fn cuda_status() -> std::process::ExitCode {
    match cuda::status_lines() {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(fail) => report(fail),
    }
}

/// `caim cuda install [--accept-note]`: the same progress line a model download prints, and the same
/// exit code when the vendor's note has not been accepted yet.
fn cuda_install(accept_note: bool) -> std::process::ExitCode {
    let mut progress = Progress::default();
    let outcome = cuda::install_all(accept_note, &mut |read, total| progress.report(read, total));
    progress.finish();
    match outcome {
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

/// Enter on a terminal asks the server to stop. MG-08 added a signal handler, so Ctrl+C does the
/// same thing now; this stays because it also works where a signal cannot be delivered, and because
/// the Enter behaviour is documented and people use it.
fn watch_stdin(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let _ = std::thread::Builder::new().name("caim-stdin".to_string()).spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
}

/// MG-08: Ctrl+C, Ctrl+Break and a console close on Windows, and SIGINT, SIGTERM and SIGHUP
/// elsewhere, stop the server exactly as Enter does. A second press while that stop is still
/// running ends the process at once with a non-zero code, which is what someone pressing Ctrl+C
/// again expects. `StopHandle::press` is the part the tests call directly; this is only the wiring.
/// `serve` calls this before `Server::start`, so the handler is in place before the endpoint file
/// says this start is ready.
fn watch_signals(handle: &StopHandle) -> Result<(), Fail> {
    let handle = handle.clone();
    ctrlc::set_handler(move || {
        if handle.press() {
            std::process::exit(i32::from(EXIT_FAILED));
        }
    })
    .map_err(|error| Fail::internal(format!("Craft AI Manager could not watch for Ctrl+C: {error}")))
}
