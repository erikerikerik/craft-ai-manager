//! The Craft AI Manager window (MG-13). `agents/CAIM-UI-BRIEF.md` is the source of truth for
//! every string, state and rule the window shows.
//!
//! Four layers, so the words can be tested without a screen:
//!
//! - [format] turns a number into the brief's sentence.
//! - [state] turns [state::Facts] into a [state::View]: the whole window, as strings.
//! - [facts] reads the library - catalog, store, activity log, card, queue - into [state::Facts].
//! - [app] draws a [state::View] with eframe and calls the library back.
//!
//! [app::open] is the entry point `caim open` uses: the server and the window are one process, and
//! the window calls these library functions directly rather than going through the CLI.

pub mod app;
pub mod facts;
pub mod format;
pub mod state;
pub mod task;

pub use app::open;
