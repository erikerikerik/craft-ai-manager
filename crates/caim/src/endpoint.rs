//! Where the manager publishes its port and token (SPEC amendment 11 > Discovery), written
//! atomically so a client can never read half a file.
//!
//! On Windows the file lives under %LOCALAPPDATA%\CraftAI\, which already carries per-user
//! permissions, so the token inside it is readable only by this user. CRAFT_AI_ENDPOINT names a
//! different file, which is how the tests keep out of each other's way.

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
    platform_data_dir().map(|dir| dir.join(ENDPOINT_DIR).join(ENDPOINT_NAME))
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
    let mut file = std::fs::File::create(temp)?;
    file.write_all(body)?;
    file.flush()
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
    let Ok(file) = std::fs::File::open(path) else { return false };
    let mut bytes = Vec::new();
    // Read past the cap so a file that is far too large is never parsed.
    let mut limited = file.take(MAX_FILE_BYTES.saturating_add(1));
    if limited.read_to_end(&mut bytes).is_err() || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return false;
    }
    serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)).is_ok_and(|value| value.get("token").and_then(Value::as_str) == Some(token))
}

#[cfg(test)]
mod tests {
    use super::{ENDPOINT_VERSION, endpoint_file_from_env, is_ours, remove, temp_path, write};
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
}
