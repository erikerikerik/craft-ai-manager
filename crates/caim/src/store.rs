//! Where the models live on disk, and what "installed" means.
//!
//! A model sits at `<data dir>/CraftAI/models/<id>/<version>/<file>`, the same data directory as
//! `endpoint.json`, and `CRAFT_AI_MODELS` moves the models folder (that is how the tests keep out of
//! the real store; most of them pass a folder directly).
//!
//! "Installed" is three things at once: the file is there, its size is what the catalog says, and a
//! `verified.json` - written only after a successful SHA-256 check - names the same hash. Startup
//! never re-hashes a 200 MB file; `caim models verify <id>` does, and that is also what rewrites
//! `verified.json` after the fact.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::catalog::Model;
use crate::error::{Fail, Result};

/// The environment variable that names the models folder instead of the default.
pub const MODELS_ENV: &str = "CRAFT_AI_MODELS";

/// The folder inside the data directory.
const MODELS_DIR: &str = "models";
/// Written after a successful hash check, and the reason startup does not re-hash.
const VERIFIED_NAME: &str = "verified.json";
/// Written when the user accepts a flagged model's note.
const ACKNOWLEDGED_NAME: &str = "acknowledged.json";
/// A download in progress, in the same folder as the file it will become.
const PART_SUFFIX: &str = ".part";
/// Held by the one install that may write the `.part` file.
const LOCK_SUFFIX: &str = ".lock";
/// How much is hashed at a time, so a 200 MB file never sits in memory.
const HASH_CHUNK: usize = 64 * 1024;
/// How deep [folder_usage] will walk. A models folder is two deep (`<id>/<version>`), so this is a
/// wall against a tree built to exhaust memory, not a limit a real store comes near.
const MAX_WALK_DEPTH: usize = 16;
/// How many folders [folder_usage] holds at once, for the same reason.
const MAX_WALK_FOLDERS: usize = 4_096;

/// The models folder: `CRAFT_AI_MODELS` when it names one, otherwise the default. `None` when the
/// platform has nowhere to put files at all.
#[must_use]
pub fn models_dir() -> Option<PathBuf> {
    models_dir_in(std::env::var_os(MODELS_ENV), crate::endpoint::data_dir())
}

/// The same decision with the two inputs handed over, so it can be tested without changing the
/// environment of a running process.
#[must_use]
pub fn models_dir_in(named: Option<std::ffi::OsString>, data_dir: Option<PathBuf>) -> Option<PathBuf> {
    match named {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => data_dir.map(|dir| dir.join(MODELS_DIR)),
    }
}

/// The folder for one model version.
#[must_use]
pub fn model_folder(root: &Path, model: &Model) -> PathBuf {
    root.join(&model.id).join(&model.version)
}

/// The model file itself.
#[must_use]
pub fn model_file(root: &Path, model: &Model) -> PathBuf {
    model_folder(root, model).join(&model.file)
}

/// One install's half-written download, `<file>.<tag>.part`, renamed into place once the hash
/// checks out. Every install picks its own random `tag`, so two installs never share a file (C6).
#[must_use]
pub fn part_file(root: &Path, model: &Model, tag: &str) -> PathBuf {
    let name = format!("{}.{tag}{PART_SUFFIX}", model.file);
    model_folder(root, model).join(name)
}

/// Is `name` a download in progress (or left behind) for this model's file, whoever wrote it? The
/// untagged `<file>.part` of earlier versions counts too.
#[must_use]
pub fn is_part_name(model: &Model, name: &str) -> bool {
    name.strip_prefix(model.file.as_str()).is_some_and(|rest| rest.ends_with(PART_SUFFIX) && (rest == PART_SUFFIX || rest.starts_with('.')))
}

/// The lock an install holds while it owns the `.part` file, so two installs never share one.
#[must_use]
pub fn lock_file(root: &Path, model: &Model) -> PathBuf {
    let name = format!("{}{LOCK_SUFFIX}", model.file);
    model_folder(root, model).join(name)
}

/// The record that this file's hash was checked.
#[must_use]
pub fn verified_file(root: &Path, model: &Model) -> PathBuf {
    model_folder(root, model).join(VERIFIED_NAME)
}

/// The record that the user accepted this model's note.
#[must_use]
pub fn acknowledged_file(root: &Path, model: &Model) -> PathBuf {
    model_folder(root, model).join(ACKNOWLEDGED_NAME)
}

/// What `verified.json` holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verified {
    /// The hash that was actually computed, not the one the catalog hoped for.
    pub sha256: String,
    /// The file it was computed from.
    pub file: String,
    /// Its size in bytes at the time.
    pub size: u64,
    /// Seconds since the epoch, or 0 when the clock is unusable.
    pub verified_at_unix: u64,
}

/// What `acknowledged.json` holds (SPEC v1.1 amendment 14: one acknowledgment per flagged model).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Acknowledgement {
    pub id: String,
    pub version: String,
    /// The SHA-256 of the note that was accepted, so a changed note needs a new acknowledgment.
    pub note_sha256: String,
    /// Seconds since the epoch, or 0 when the clock is unusable.
    pub acknowledged_at_unix: u64,
}

/// Is this model ready to run? File present, size as promised, and a verification that names the
/// same hash. Anything else is "not installed", including a `verified.json` we cannot read.
#[must_use]
pub fn is_installed(root: &Path, model: &Model) -> bool {
    let path = model_file(root, model);
    let Ok(metadata) = std::fs::metadata(&path) else { return false };
    if !metadata.is_file() || metadata.len() != model.size {
        return false;
    }
    read_verified(root, model).is_some_and(|record| record.sha256 == model.sha256)
}

/// Read `verified.json`, if it is there and readable.
#[must_use]
pub fn read_verified(root: &Path, model: &Model) -> Option<Verified> {
    let text = std::fs::read_to_string(verified_file(root, model)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write `verified.json` after a successful hash check.
pub fn write_verified(root: &Path, model: &Model, sha256: &str, size: u64) -> Result<()> {
    let record = Verified { sha256: sha256.to_string(), file: model.file.clone(), size, verified_at_unix: now_unix() };
    let body =
        serde_json::to_string_pretty(&record).map_err(|error| Fail::internal(format!("Craft AI Manager could not write its verification record: {error}")))?;
    std::fs::create_dir_all(model_folder(root, model))
        .map_err(|error| Fail::internal(format!("Craft AI Manager could not create the model folder: {error}")))?;
    let path = verified_file(root, model);
    std::fs::write(&path, body).map_err(|error| Fail::internal(format!("Craft AI Manager could not write {}: {error}", path.display())))
}

/// Drop `verified.json` when a re-check fails, so the model stops counting as installed.
pub fn remove_verified(root: &Path, model: &Model) -> Result<()> {
    match std::fs::remove_file(verified_file(root, model)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Fail::internal(format!("Craft AI Manager could not remove its verification record: {error}"))),
    }
}

/// Has the user already accepted this model's note, and the note as it reads today?
#[must_use]
pub fn is_acknowledged(root: &Path, model: &Model) -> bool {
    let Some(note) = model.note.as_ref() else { return true };
    let text = std::fs::read_to_string(acknowledged_file(root, model)).ok();
    let Some(text) = text else { return false };
    serde_json::from_str::<Acknowledgement>(&text).is_ok_and(|record| record.note_sha256 == sha256_hex(note.note.as_bytes()))
}

/// Record that the user accepted the note. Does nothing for a model with no note.
pub fn write_acknowledgement(root: &Path, model: &Model) -> Result<()> {
    let Some(note) = model.note.as_ref() else { return Ok(()) };
    let record = Acknowledgement {
        id: model.id.clone(),
        version: model.version.clone(),
        note_sha256: sha256_hex(note.note.as_bytes()),
        acknowledged_at_unix: now_unix(),
    };
    let body =
        serde_json::to_string_pretty(&record).map_err(|error| Fail::internal(format!("Craft AI Manager could not write the acknowledgment: {error}")))?;
    std::fs::create_dir_all(model_folder(root, model))
        .map_err(|error| Fail::internal(format!("Craft AI Manager could not create the model folder: {error}")))?;
    let path = acknowledged_file(root, model);
    std::fs::write(&path, body).map_err(|error| Fail::internal(format!("Craft AI Manager could not write {}: {error}", path.display())))
}

/// What a folder holds on disk: the bytes of its regular files, and how many files that is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// Bytes in regular files, counting subfolders.
    pub bytes: u64,
    /// How many regular files those bytes came from.
    pub files: u64,
}

/// True when the path is there but is a link rather than the folder itself. [folder_usage] refuses
/// such a path, and the callers use this to say why nothing was counted rather than asking
/// `Path::is_dir`, which follows the link and answers about the folder on the other side of it.
#[must_use]
pub fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

/// Add up the files under a folder. `None` when the folder is not there, when a link sits in its
/// place, and `None` when the tree is deeper or broader than the two caps above - a tree no model
/// folder has, and the caller is then asked to say it could not count rather than report a number it
/// did not finish computing. A folder that cannot be read adds nothing rather than failing the
/// count, and no link is followed: the folder handed in is refused by [is_link], and a link found
/// inside the walk is skipped, so this never walks outside the folder it was handed.
#[must_use]
pub fn folder_usage(path: &Path) -> Option<Usage> {
    // read_dir follows a link, so the folder handed in is read as it really sits on disk: a link
    // here would walk a tree that is not inside the models folder at all.
    let Ok(metadata) = std::fs::symlink_metadata(path) else { return None };
    if !metadata.is_dir() {
        return None;
    }
    let mut usage = Usage { bytes: 0, files: 0 };
    // The depth travels with the folder rather than with the shape of the stack, so the cap holds
    // however the folders come back out.
    let mut folders = vec![(path.to_path_buf(), 0usize)];
    while let Some((folder, depth)) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if depth >= MAX_WALK_DEPTH || folders.len() >= MAX_WALK_FOLDERS {
                    return None;
                }
                folders.push((entry.path(), depth + 1));
            } else if kind.is_file()
                && let Ok(file) = entry.metadata()
            {
                usage.bytes = usage.bytes.saturating_add(file.len());
                usage.files = usage.files.saturating_add(1);
            }
        }
    }
    Some(usage)
}

/// Hash a file that is already on disk, in chunks.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(|error| Fail::internal(format!("Craft AI Manager could not open {}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_CHUNK];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if let Some(chunk) = buffer.get(..read) {
                    hasher.update(chunk);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Fail::internal(format!("Craft AI Manager could not read {}: {error}", path.display()))),
        }
    }
    Ok(hex(&hasher.finalize()))
}

/// The SHA-256 of a short piece of text, used for the note so a changed note needs a new
/// acknowledgment.
#[must_use]
pub fn sha256_hex(text: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text);
    hex(&hasher.finalize())
}

/// Lowercase hex, the way a SHA-256 is written in the catalog.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Seconds since the epoch, or 0 when the clock cannot be trusted. A timestamp in a record is
/// useful, never load-bearing.
#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|since| since.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use crate::catalog::OverrideNote;
    use crate::testkit::{Scratch, test_model};

    use super::{
        Acknowledgement, MODELS_DIR, VERIFIED_NAME, folder_usage, hash_file, is_acknowledged, is_installed, model_file, model_folder, models_dir_in, part_file,
        read_verified, remove_verified, sha256_hex, verified_file, write_acknowledgement, write_verified,
    };

    const FIXTURE: &[u8] = b"the bytes a small model would be made of";

    #[test]
    fn a_model_is_installed_only_with_the_file_the_size_and_the_record() {
        let scratch = Scratch::new("store-installed");
        let entry = test_model(FIXTURE, None);
        assert!(!is_installed(&scratch.path, &entry), "an empty store has nothing installed");
        std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder can be made");
        std::fs::write(model_file(&scratch.path, &entry), FIXTURE).expect("the model file can be written");
        assert!(!is_installed(&scratch.path, &entry), "the file is there, but nothing has hashed it");
        write_verified(&scratch.path, &entry, &entry.sha256, entry.size).expect("the record can be written");
        assert!(is_installed(&scratch.path, &entry), "file, size and record together are what installed means");
        let wrong_size = super::Model { size: entry.size + 1, ..entry.clone() };
        assert!(!is_installed(&scratch.path, &wrong_size), "a different size is not the promised file");
    }

    #[test]
    fn a_record_naming_another_hash_does_not_count() {
        let scratch = Scratch::new("store-hash");
        let entry = test_model(FIXTURE, None);
        std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder can be made");
        std::fs::write(model_file(&scratch.path, &entry), FIXTURE).expect("the model file can be written");
        write_verified(&scratch.path, &entry, &"0".repeat(64), entry.size).expect("the record can be written");
        assert!(!is_installed(&scratch.path, &entry), "the record names a hash this file does not have");
    }

    #[test]
    fn a_failed_check_stops_the_model_counting_as_installed() {
        let scratch = Scratch::new("store-remove");
        let entry = test_model(FIXTURE, None);
        std::fs::create_dir_all(model_folder(&scratch.path, &entry)).expect("the model folder can be made");
        std::fs::write(model_file(&scratch.path, &entry), FIXTURE).expect("the model file can be written");
        write_verified(&scratch.path, &entry, &entry.sha256, entry.size).expect("the record can be written");
        assert!(is_installed(&scratch.path, &entry));
        remove_verified(&scratch.path, &entry).expect("the record can be removed");
        assert!(!is_installed(&scratch.path, &entry));
        assert!(read_verified(&scratch.path, &entry).is_none());
        remove_verified(&scratch.path, &entry).expect("removing a record that is not there is fine");
    }

    #[test]
    fn the_record_names_the_file_it_was_computed_from() {
        let scratch = Scratch::new("store-record");
        let entry = test_model(FIXTURE, None);
        write_verified(&scratch.path, &entry, &entry.sha256, entry.size).expect("the record can be written");
        let record = read_verified(&scratch.path, &entry).expect("the record can be read back");
        assert_eq!(record.sha256, entry.sha256);
        assert_eq!(record.file, entry.file);
        assert_eq!(record.size, entry.size);
        assert!(verified_file(&scratch.path, &entry).ends_with(VERIFIED_NAME));
    }

    #[test]
    fn hashing_a_file_gives_the_same_hash_as_hashing_its_bytes() {
        let scratch = Scratch::new("store-hash-file");
        let path = scratch.path.join("fixture.bin");
        std::fs::write(&path, FIXTURE).expect("the fixture can be written");
        assert_eq!(hash_file(&path).expect("the fixture can be hashed"), sha256_hex(FIXTURE));
        assert_eq!(sha256_hex(FIXTURE).len(), 64, "a SHA-256 is 64 lowercase hex characters");
        assert!(hash_file(&scratch.path.join("not-there")).is_err(), "a missing file is an error");
    }

    #[test]
    fn an_acknowledgment_belongs_to_one_sentence() {
        let scratch = Scratch::new("store-ack");
        let entry = test_model(FIXTURE, Some("Use it for personal work only."));
        assert!(!is_acknowledged(&scratch.path, &entry), "nothing has been accepted yet");
        write_acknowledgement(&scratch.path, &entry).expect("the acknowledgment can be written");
        assert!(is_acknowledged(&scratch.path, &entry));
        let record: Acknowledgement =
            serde_json::from_str(&std::fs::read_to_string(super::acknowledged_file(&scratch.path, &entry)).expect("the acknowledgment can be read"))
                .expect("it is the shape the store writes");
        assert_eq!(record.id, entry.id);
        assert_eq!(record.version, entry.version);
        assert_eq!(record.note_sha256, sha256_hex("Use it for personal work only.".as_bytes()));
        let changed = super::Model {
            note: Some(OverrideNote {
                by: "Erik".to_string(),
                date: "2026-10-14".to_string(),
                reason: "Changed our mind.".to_string(),
                note: "Now it is fine for anything.".to_string(),
            }),
            ..entry.clone()
        };
        assert!(!is_acknowledged(&scratch.path, &changed), "a changed note needs a new acknowledgment");
    }

    #[test]
    fn a_model_with_no_note_needs_no_acknowledgment() {
        let scratch = Scratch::new("store-no-note");
        let entry = test_model(FIXTURE, None);
        assert!(is_acknowledged(&scratch.path, &entry), "there is nothing to accept");
        write_acknowledgement(&scratch.path, &entry).expect("writing nothing is fine");
        assert!(!super::acknowledged_file(&scratch.path, &entry).exists(), "and nothing was written");
    }

    #[test]
    fn the_models_folder_follows_the_environment_then_the_data_dir() {
        let data = PathBuf::from("data");
        assert_eq!(models_dir_in(Some(OsString::from("elsewhere")), Some(data.clone())), Some(PathBuf::from("elsewhere")), "CRAFT_AI_MODELS wins");
        assert_eq!(models_dir_in(None, Some(data.clone())), Some(data.join(MODELS_DIR)), "otherwise it sits in the data dir");
        assert_eq!(models_dir_in(Some(OsString::new()), Some(data.clone())), Some(data.join(MODELS_DIR)), "an empty override is not a folder");
        assert_eq!(models_dir_in(None, None), None, "and a platform with no data dir has no models folder");
    }

    #[test]
    fn the_part_file_and_the_records_sit_next_to_the_model_file() {
        let scratch = Scratch::new("store-paths");
        let entry = test_model(FIXTURE, None);
        let file = model_file(&scratch.path, &entry);
        assert!(file.starts_with(&scratch.path), "everything stays inside the models folder: {}", file.display());
        assert!(file.ends_with(PathBuf::from("lama").join("1").join("lama_fp32.onnx")), "id, then version, then the file name: {file:?}");
        assert_eq!(part_file(&scratch.path, &entry, "a1b2").parent(), file.parent(), "the .part file is in the same folder");
        assert!(part_file(&scratch.path, &entry, "a1b2").to_string_lossy().ends_with("lama_fp32.onnx.a1b2.part"), "and carries its install's tag");
        assert!(super::is_part_name(&entry, "lama_fp32.onnx.a1b2.part") && super::is_part_name(&entry, "lama_fp32.onnx.part"));
        assert!(!super::is_part_name(&entry, "lama_fp32.onnx") && !super::is_part_name(&entry, "lama_fp32.onnx.lock"));
        assert!(!super::is_part_name(&entry, "other.onnx.a1b2.part") && !super::is_part_name(&entry, "lama_fp32.onnxx.part"));
        assert_eq!(verified_file(&scratch.path, &entry).parent(), file.parent());
    }

    #[test]
    fn folder_usage_adds_up_a_folder_and_refuses_a_missing_one() {
        let scratch = Scratch::new("store-usage");
        assert_eq!(folder_usage(&scratch.path.join("models")), None, "a folder that is not there has no usage");
        let entry = test_model(FIXTURE, None);
        let version = model_folder(&scratch.path, &entry);
        std::fs::create_dir_all(&version).expect("the model folder can be made");
        let file = model_file(&scratch.path, &entry);
        std::fs::write(&file, FIXTURE).expect("the model file can be written");
        assert_eq!(folder_usage(&file), None, "a file is not a folder");
        write_verified(&scratch.path, &entry, &entry.sha256, entry.size).expect("the record can be written");
        let record = std::fs::metadata(verified_file(&scratch.path, &entry)).expect("the record can be checked");
        let usage = folder_usage(&version).expect("the version folder can be counted");
        assert_eq!(usage.files, 2, "the model file and its record are both counted");
        assert_eq!(usage.bytes, FIXTURE.len() as u64 + record.len(), "and so are their bytes");
        assert_eq!(folder_usage(version.parent().expect("the id folder is there")), Some(usage), "the id folder counts the same two");
        assert_eq!(folder_usage(&scratch.path), Some(usage), "and so does the models folder itself");
        let deep = scratch.path.join("deep");
        let mut leaf = deep.clone();
        for _ in 0..super::MAX_WALK_DEPTH + 1 {
            leaf = leaf.join("next");
        }
        std::fs::create_dir_all(&leaf).expect("a tree past the depth cap can be made");
        assert_eq!(folder_usage(&deep), None, "a tree past the depth cap is refused rather than counted");
    }

    #[cfg(unix)]
    #[test]
    fn folder_usage_refuses_a_link_handed_in_as_the_folder() {
        let scratch = Scratch::new("store-usage-link");
        let real = scratch.path.join("real");
        std::fs::create_dir_all(&real).expect("the folder the link points at can be made");
        std::fs::write(real.join("keep.onnx"), "keep these bytes").expect("a file outside the store can be written");
        let linked = scratch.path.join("models");
        std::os::unix::fs::symlink(&real, &linked).expect("the link can be made");
        assert!(!folder_usage(&linked).is_some_and(|usage| usage.files > 0), "a link is not a folder to walk: {:?}", folder_usage(&linked));
        assert!(super::is_link(&linked), "and the caller can say why nothing was counted");
        assert!(!super::is_link(&real), "the folder on the other side of the link is a folder");
    }
}
