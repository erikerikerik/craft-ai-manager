//! Where the manager publishes its port and token (SPEC amendment 11 > Discovery), written
//! atomically so a client can never read half a file.
//!
//! On Windows the file lives under %LOCALAPPDATA%\CraftAI\, which already carries per-user
//! permissions, so the token inside it is readable only by this user. CRAFT_AI_ENDPOINT names a
//! different file, which is how the tests keep out of each other's way. On Linux and macOS that
//! folder has no such guarantee, so the file - and the temp file before it - are created with mode
//! 0600 (the MG-01 review follow-up).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::Value;

use crate::error::{Fail, Result};

/// The version an endpoint file declares.
pub const ENDPOINT_VERSION: u64 = 1;
/// The environment variable that names the endpoint file instead of the platform default.
pub const ENDPOINT_ENV: &str = "CRAFT_AI_ENDPOINT";
/// The folder inside the user's data directory.
const ENDPOINT_DIR: &str = "CraftAI";
/// The file inside that folder.
const ENDPOINT_NAME: &str = "endpoint.json";
/// The endpoint file is JSON, and the protocol caps JSON at 64 KiB.
const MAX_FILE_BYTES: u64 = 64 * 1024;

static WRITE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// What the endpoint file holds: the port to connect to and the token to present.
#[derive(Serialize)]
struct EndpointFile {
    version: u64,
    port: u32,
    token: String,
}

/// The file for this platform, or None when the platform has nowhere to put one.
#[must_use]
pub fn default_endpoint_file() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(ENDPOINT_NAME))
}

/// The folder CAIM keeps its files in: `%LOCALAPPDATA%\CraftAI` on Windows, and the platform
/// equivalent elsewhere. The model store hangs off here too, so there is one place to look.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    platform_data_dir().map(|dir| dir.join(ENDPOINT_DIR))
}

fn platform_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()).map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").filter(|value| !value.is_empty()).map(|home| PathBuf::from(home).join("Library").join("Application Support"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()).map(|home| PathBuf::from(home).join(".local").join("share")))
    }
}

/// CRAFT_AI_ENDPOINT when it names a file, otherwise the platform default.
#[must_use]
pub fn endpoint_file_from_env() -> Option<PathBuf> {
    match std::env::var_os(ENDPOINT_ENV) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => default_endpoint_file(),
    }
}

/// Write the endpoint file: a temp file in the same folder, then a rename over the old one. A
/// client that reads the file while we start sees either the previous start or this one, never a
/// mixture, and the rename replaces the previous file rather than failing because it exists.
pub fn write(path: &Path, port: u16, token: &str) -> Result<()> {
    let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
        return Err(Fail::internal(format!("Craft AI Manager has no folder to write {} in.", path.display())));
    };
    std::fs::create_dir_all(parent).map_err(|error| Fail::internal(format!("Craft AI Manager could not create {}: {error}", parent.display())))?;
    let body = serde_json::to_vec(&EndpointFile { version: ENDPOINT_VERSION, port: u32::from(port), token: token.to_string() })?;
    let temp = temp_path(path);
    let written = write_whole(&temp, &body);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(Fail::internal(format!("Craft AI Manager could not write its endpoint file at {}: {error}", path.display())));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(Fail::internal(format!("Craft AI Manager could not put its endpoint file at {}: {error}", path.display())));
    }
    Ok(())
}

/// Create, write and flush one file before it is renamed into place.
fn write_whole(temp: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut file = create_private(temp)?;
    file.write_all(body)?;
    file.flush()
}

/// Make a file that only this user can read. On Linux and macOS the mode is fixed when the file
/// is created, so the token is never world-readable, not even in the moment between writing the
/// temp file and renaming it; a umask can only clear bits, so 0600 is what we get. Windows needs
/// nothing: %LOCALAPPDATA% already carries a per-user ACL that a plain create inherits.
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
}

/// The non-Unix half of [create_private](write_whole), where the folder's permissions are enough.
#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

/// A temp name in the same folder as the target, so the rename stays inside one folder.
fn temp_path(path: &Path) -> PathBuf {
    let counter = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("{ENDPOINT_NAME}.tmp-{}-{counter}", std::process::id());
    path.with_file_name(name)
}

/// Remove the endpoint file on a clean shutdown - but only if it is still ours. A second manager
/// that started while this one was running has replaced it, and its file must stay.
pub fn remove(path: &Path, token: &str) -> Result<()> {
    if !is_ours(path, token) {
        return Ok(());
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Fail::internal(format!("Craft AI Manager could not remove its endpoint file: {error}"))),
    }
}

/// Does the file at this path still hold this token?
fn is_ours(path: &Path, token: &str) -> bool {
    read_value(path).is_some_and(|value| value.get("token").and_then(Value::as_str) == Some(token))
}

/// The port the manager published, when the file is there, within the size cap, and names a port.
/// A file left behind by a manager that died still names a port, so anyone asking whether a server
/// is running has to ask the port as well (MG-09).
#[must_use]
pub fn published_port(path: &Path) -> Option<u16> {
    let port = read_value(path)?.get("port").and_then(Value::as_u64)?;
    u16::try_from(port).ok()
}

/// The endpoint file as JSON, never reading past the cap so an oversized file is never parsed.
/// `None` when it is missing, too big, or not JSON.
fn read_value(path: &Path) -> Option<Value> {
    let Ok(file) = std::fs::File::open(path) else { return None };
    let mut bytes = Vec::new();
    // Read past the cap so a file that is far too large is never parsed.
    let mut limited = file.take(MAX_FILE_BYTES.saturating_add(1));
    if limited.read_to_end(&mut bytes).is_err() || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return None;
    }
    serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)).ok()
}

#[cfg(test)]
mod tests {
    use super::{ENDPOINT_VERSION, endpoint_file_from_env, is_ours, published_port, remove, temp_path, write};
    use std::path::PathBuf;

    fn temp_endpoint(who: &str) -> PathBuf {
        std::env::temp_dir().join(format!("caim-endpoint-{who}-{}.json", std::process::id()))
    }

    struct TempFile(PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn the_file_is_written_whole_and_replaced_on_the_next_start() {
        // Its own folder, so "nothing was left behind" means the rename really cleaned up after itself.
        let folder = std::env::temp_dir().join(format!("caim-endpoint-it-{}-unit", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("the folder is created");
        let path = folder.join("endpoint.json");
        write(&path, 4001, "token-one").expect("the first start writes the file");
        write(&path, 4002, "token-two").expect("the next start replaces it");
        let text = std::fs::read_to_string(&path).expect("the file is readable");
        let value: serde_json::Value = serde_json::from_str(&text).expect("the file is JSON");
        assert_eq!(value.get("version").and_then(serde_json::Value::as_u64), Some(ENDPOINT_VERSION));
        assert_eq!(value.get("port").and_then(serde_json::Value::as_u64), Some(4002));
        assert_eq!(value.get("token").and_then(serde_json::Value::as_str), Some("token-two"));
        let left_behind: Vec<String> = std::fs::read_dir(&folder)
            .expect("the folder is readable")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "endpoint.json")
            .collect();
        assert!(left_behind.is_empty(), "no temp file is left behind: {left_behind:?}");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn removing_leaves_another_runs_file_alone() {
        let path = temp_endpoint("remove");
        let _guard = TempFile(path.clone());
        write(&path, 4003, "mine").expect("the file is written");
        assert!(is_ours(&path, "mine"));
        assert!(!is_ours(&path, "someone-else"));
        remove(&path, "someone-else").expect("another run's file is left alone");
        assert!(path.exists(), "another run's file is not ours to delete");
        remove(&path, "mine").expect("our own file goes");
        assert!(!path.exists());
        remove(&path, "mine").expect("removing a file that is already gone is fine");
    }

    #[test]
    fn temp_names_stay_next_to_the_target() {
        let path = PathBuf::from("some").join("where").join("endpoint.json");
        let temp = temp_path(&path);
        assert_eq!(temp.parent(), path.parent());
        assert!(temp.file_name().unwrap_or_default().to_string_lossy().starts_with("endpoint.json.tmp-"));
    }

    #[test]
    fn the_endpoint_file_is_named_endpoint_json() {
        if let Some(path) = endpoint_file_from_env() {
            assert_eq!(path.file_name().unwrap_or_default(), "endpoint.json");
        }
    }

    /// The MG-01 review follow-up: on Linux and macOS another local user must not be able to read
    /// the token. Windows is covered by the %LOCALAPPDATA% ACL rather than by a mode.
    #[cfg(unix)]
    #[test]
    fn the_endpoint_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_endpoint("mode");
        let _guard = TempFile(path.clone());
        write(&path, 4004, "mode-token").expect("the file is written");
        let mode = std::fs::metadata(&path).expect("the file is there").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the endpoint file is owner-read-write only, got {mode:o}");
    }

    /// The temp file is private too, so there is no moment when the token is exposed.
    #[cfg(unix)]
    #[test]
    fn the_temp_file_before_the_rename_is_private_too() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_endpoint("mode-temp");
        let _guard = TempFile(path.clone());
        let mut file = super::create_private(&path).expect("the temp file is created");
        std::io::Write::write_all(&mut file, b"token").expect("the temp file is written");
        drop(file);
        let mode = std::fs::metadata(&path).expect("the file is there").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the temp file is owner-read-write only, got {mode:o}");
    }

    #[test]
    fn published_port_reads_the_port_and_refuses_a_bad_file() {
        let path = temp_endpoint("port");
        let _guard = TempFile(path.clone());
        write(&path, 4004, "mine").expect("the file is written");
        assert_eq!(published_port(&path), Some(4004), "the port the manager published");
        std::fs::write(&path, r#"{"version": 1, "token": "mine"}"#).expect("a file naming no port can be written");
        assert_eq!(published_port(&path), None, "a file that names no port publishes nothing");
        std::fs::write(&path, r#"{"port": 99999999999}"#).expect("an absurd port can be written");
        assert_eq!(published_port(&path), None, "a number that is not a port is refused");
        std::fs::write(&path, "not json at all").expect("a file that is not JSON can be written");
        assert_eq!(published_port(&path), None, "and so is a file that is not JSON");
        assert_eq!(published_port(&path.with_file_name("not-there.json")), None, "and a file that is not there");
    }
}
