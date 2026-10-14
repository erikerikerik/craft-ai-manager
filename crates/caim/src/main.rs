//! The `caim` binary. MG-00: `caim --version` prints the version, anything else says the manager
//! is not implemented yet and exits 2. MG-01 replaces this with the loopback server.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use craft_ai_manager::{Action, EXIT_NOT_IMPLEMENTED, VERSION, classify};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match classify(&args) {
        Action::Version => {
            println!("caim {VERSION}");
            std::process::ExitCode::SUCCESS
        }
        Action::NotImplemented => {
            println!("Craft AI Manager is not implemented yet: MG-01 builds the loopback server.");
            std::process::ExitCode::from(EXIT_NOT_IMPLEMENTED)
        }
    }
}
