//! The `caim models …` commands: what the catalog offers, what is on disk, and how a flagged
//! model gets its note accepted (SPEC v1.1 amendment 14).
//!
//! The functions that do the work take the models folder and a [Fetch](crate::fetch::Fetch) so the
//! tests can point them at a temp folder and a byte slice; the short wrappers next to them read
//! `CRAFT_AI_MODELS` and build the real HTTPS fetcher.

use std::path::{Path, PathBuf};

use crate::catalog::{self, Model};
use crate::endpoint;
use crate::error::{Fail, Result, quoted};
use crate::fetch::{self, Fetch, HttpFetch};
use crate::store;

/// The models folder for this run, or a plain sentence when the platform has nowhere to put one.
pub fn models_root() -> Result<PathBuf> {
    store::models_dir().ok_or_else(|| Fail::internal("Craft AI Manager has nowhere on this system to keep models."))
}

/// The catalog entry for one model id. More than one version of the same id is an error rather than
/// a silent choice; MG-02 has one entry per model.
pub fn find(id: &str) -> Result<Model> {
    let models = catalog::load()?;
    let named: Vec<&Model> = models.iter().filter(|model| model.id == id).collect();
    match named.as_slice() {
        [] => {
            let catalog = models.iter().map(|model| model.id.clone()).collect::<Vec<String>>().join(", ");
            Err(Fail::not_found(format!("Craft AI Manager has no model called {}. The catalog has: {catalog}.", quoted(id))))
        }
        [one] => Ok((*one).clone()),
        many => Err(Fail::internal(format!(
            "The catalog holds {} versions of {}. Craft AI Manager does not choose between them yet, so make the id unique.",
            many.len(),
            quoted(id)
        ))),
    }
}

/// `caim models list`: one line per catalog entry.
pub fn list() -> Result<Vec<String>> {
    Ok(list_lines(&catalog::load()?, &models_root()?))
}

/// The same, with the folder and the catalog handed over, for the tests. Each line ends with the
/// bytes that model's folder holds on disk, and the last line is the total for the models folder
/// beside the folder itself (MG-09).
#[must_use]
pub fn list_lines(models: &[Model], root: &Path) -> Vec<String> {
    let mut lines: Vec<String> = models
        .iter()
        .map(|model| {
            let state = if store::is_installed(root, model) { "installed" } else { "not installed" };
            let needs = if model.note.is_some() && !store::is_acknowledged(root, model) { "  needs acknowledgment" } else { "" };
            let folder = store::model_folder(root, model);
            // A folder that is not there has nothing on disk; a folder past the walk cap says so
            // rather than showing a number that was never finished, and so does a link, because the
            // walk refuses it rather than counting the tree on the other side of it.
            let on_disk = match linked_step(root, model) {
                Some(_) => "a link, not a folder".to_string(),
                None => match store::folder_usage(&folder) {
                    Some(usage) => format!("{} bytes", usage.bytes),
                    None if folder.is_dir() => "too many folders to count".to_string(),
                    None => "0 bytes".to_string(),
                },
            };
            format!(
                "{id}  {version}  {task}  {size} bytes  licences: code {code}, weights {weights}, training data {training}  {state}{needs}  on disk: {bytes}",
                id = model.id,
                version = model.version,
                task = model.task,
                size = model.size,
                code = model.licence_code,
                weights = model.licence_weights,
                training = model.licence_training_data,
                state = state,
                needs = needs,
                bytes = on_disk,
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("The model catalog has no entries.".to_string());
    }
    let total = if store::is_link(root) {
        "a link, not a folder".to_string()
    } else {
        match store::folder_usage(root) {
            Some(usage) => format!("{} bytes in {} files", usage.bytes, usage.files),
            None if root.is_dir() => "too many folders to count".to_string(),
            None => "0 bytes in 0 files".to_string(),
        }
    };
    lines.push(format!("total on disk: {total}, models folder {}", root.display()));
    lines
}

/// `caim models install <id> [--accept-note]`, with the folder and the fetcher handed over.
///
/// A flagged entry is refused until its note has been accepted; `accept_note` records the
/// acknowledgment and goes ahead. An already installed model makes no request at all.
pub fn install_model(model: &Model, root: &Path, accept_note: bool, fetcher: &dyn Fetch, progress: &mut dyn FnMut(u64, u64)) -> Result<String> {
    if accept_note && model.note.is_some() {
        store::write_acknowledgement(root, model)?;
    }
    match fetch::install(model, root, fetcher, progress)? {
        fetch::Outcome::AlreadyInstalled => {
            Ok(format!("{} is already installed at {}. Nothing was downloaded.", model.label(), fetch::installed_path(root, model).display()))
        }
        fetch::Outcome::Downloaded { sha256, bytes, elapsed_ms } => {
            Ok(format!("{} is installed: {bytes} bytes, SHA-256 {sha256}, in {elapsed_ms} ms. {}", model.label(), fetch::installed_path(root, model).display()))
        }
    }
}

/// `caim models install <id>` with the real folder and the real HTTPS fetcher.
pub fn install(id: &str, accept_note: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<String> {
    let model = find(id)?;
    install_model(&model, &models_root()?, accept_note, &HttpFetch::new(), progress)
}

/// `caim models verify <id>`: re-hash what is on disk and rewrite the verification record.
pub fn verify_model(model: &Model, root: &Path) -> Result<String> {
    let checked = fetch::verify(model, root)?;
    Ok(format!(
        "{} checks out: {bytes} bytes, SHA-256 {sha256}, in {elapsed_ms} ms.",
        model.label(),
        bytes = checked.bytes,
        sha256 = checked.sha256,
        elapsed_ms = checked.elapsed_ms,
    ))
}

/// The same, with the folder read from the environment.
pub fn verify(id: &str) -> Result<String> {
    let model = find(id)?;
    verify_model(&model, &models_root()?)
}

/// `caim models remove <id>`: delete one model's installed files - the model file, `verified.json`,
/// `acknowledged.json`, and any `.part` or `.lock` left behind. `endpoint` is the endpoint file
/// (None when the platform has nowhere to put one) and `probe` says whether a port answers, so a
/// test can decide that without opening a socket.
pub fn remove_model(model: &Model, root: &Path, endpoint: Option<&Path>, probe: &dyn Fn(u16) -> bool) -> Result<String> {
    if let Some(port) = live_server_port(endpoint, probe) {
        return Err(Fail::conflict(format!(
            "Craft AI Manager is listening on port {port}, so it may be using {}. Stop it (press Enter in its window) and try again. Nothing was deleted.",
            quoted(&model.label())
        )));
    }
    let folder = installed_folder(root, model)?;
    let Some(usage) = store::folder_usage(&folder) else {
        if folder.is_dir() {
            return Err(Fail::internal(format!(
                "Craft AI Manager will not delete {} because it holds more folders than a model folder should. Nothing was deleted.",
                folder.display()
            )));
        }
        return Err(Fail::not_found(format!(
            "{} has nothing installed under {}. Nothing was deleted.",
            quoted(&model.label()),
            quoted(&folder.display().to_string())
        )));
    };
    std::fs::remove_dir_all(&folder).map_err(|error| Fail::internal(format!("Craft AI Manager could not remove {}: {error}", folder.display())))?;
    // The <id> folder goes with it when this was its last version, so the models folder does not
    // fill up with empty names. remove_dir only succeeds on an empty folder, which is the check.
    if let Some(parent) = folder.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(format!("{} is removed: {} files, {} bytes, from {}.", model.label(), usage.files, usage.bytes, folder.display()))
}

/// `caim models remove <id>` with the real folder, the real endpoint file and the real probe.
pub fn remove(id: &str) -> Result<String> {
    let model = find(id)?;
    remove_model(&model, &models_root()?, endpoint::endpoint_file_from_env().as_deref(), &port_is_listening)
}

/// The port of a manager that is answering right now: the endpoint file names a port, and something
/// listens on it. A file left behind by a manager that died names a port too, which is why the
/// second half is asked separately.
#[must_use]
pub fn live_server_port(endpoint: Option<&Path>, probe: &dyn Fn(u16) -> bool) -> Option<u16> {
    let port = endpoint::published_port(endpoint?)?;
    probe(port).then_some(port)
}

/// The real probe: a connection that succeeds means something is listening on that loopback port.
/// The timeout is short because the address is loopback, and nothing is ever sent, so a running
/// manager only closes the connection again.
#[must_use]
fn port_is_listening(port: u16) -> bool {
    let address = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, port);
    std::net::TcpStream::connect_timeout(&std::net::SocketAddr::V4(address), std::time::Duration::from_millis(250)).is_ok()
}

/// The folder one model's files live in, or a plain sentence when the catalog's names would point
/// outside the models folder. The catalog gate already refuses such names; this is the belt, because
/// what follows deletes a folder. A link in place of either folder is refused too: the check below
/// reads the path as a name, and remove_dir_all follows a link.
fn installed_folder(root: &Path, model: &Model) -> Result<PathBuf> {
    if !catalog::is_name(&model.id) || !catalog::is_name(&model.version) {
        return Err(Fail::internal(format!(
            "The catalog names {} {}, which cannot be used as folder names, so nothing was deleted.",
            quoted(&model.id),
            quoted(&model.version)
        )));
    }
    let folder = store::model_folder(root, model);
    let outside = folder.strip_prefix(root).map_or(true, |rest| rest.components().count() != 2);
    if outside {
        return Err(Fail::internal(format!(
            "Craft AI Manager will not delete {} because it is not a model folder inside the models folder. Nothing was deleted.",
            folder.display()
        )));
    }
    // Two components is only what the path is called. A link in place of the models folder, the id
    // folder or the version folder keeps that name and points somewhere else, and remove_dir_all
    // follows it, so every step is looked at as it really sits on disk before anything is deleted.
    if let Some(step) = linked_step(root, model) {
        return Err(Fail::internal(format!(
            "Craft AI Manager will not delete {} because {} is a link, not a folder. Nothing was deleted.",
            folder.display(),
            step.display()
        )));
    }
    Ok(folder)
}

/// The first step of `<models folder>/<id>/<version>` that is a link rather than a folder, counting
/// the models folder itself. The disk-use walk and the removal both refuse such a path: they read the
/// path as a name, and the filesystem follows a link sitting in the middle of it, so a link at the id
/// folder would make `<id>/<version>` a folder somewhere else entirely. Each step is looked at as it
/// is spelled, less any trailing separator - [store::is_link] takes that off first, because a models
/// folder named `CRAFT_AI_MODELS=/somewhere/models/` would otherwise answer about its target.
#[must_use]
fn linked_step(root: &Path, model: &Model) -> Option<PathBuf> {
    let id_folder = root.join(&model.id);
    let version_folder = store::model_folder(root, model);
    for step in [root, id_folder.as_path(), version_folder.as_path()] {
        if store::is_link(step) {
            return Some(step.to_path_buf());
        }
    }
    None
}

/// `caim models where`: the models folder, and whether the environment named it.
pub fn where_folder() -> Result<String> {
    let root = models_root()?;
    Ok(where_line(&root, std::env::var_os(store::MODELS_ENV).is_some_and(|value| !value.is_empty())))
}

/// The same sentence with the two inputs handed over, so it can be tested without changing the
/// environment of a running process.
#[must_use]
fn where_line(root: &Path, from_env: bool) -> String {
    if from_env {
        format!("models folder: {} (from {})", root.display(), store::MODELS_ENV)
    } else {
        format!("models folder: {} (the default data folder)", root.display())
    }
}

#[cfg(test)]
mod tests {
    use crate::catalog::{Model, OverrideNote};
    use crate::error::Status;
    use crate::models::{find, install_model, list_lines, live_server_port, remove_model, verify_model, where_line};
    use crate::store::{acknowledged_file, folder_usage, is_acknowledged, is_installed, model_file, model_folder, verified_file};
    use crate::testkit::{FakeFetch, Scratch, test_model};

    const FIXTURE: &[u8] = b"a small stand-in for a two hundred megabyte model";
    const NOTE: &str = "Use it for personal work only.";

    fn no_progress(_read: u64, _total: u64) {}

    #[test]
    fn the_catalog_holds_the_model_mg_02_ships() {
        let lama = find("lama").expect("lama is in the catalog");
        assert_eq!(lama.id, "lama");
        assert_eq!(lama.version, "1");
        assert_eq!(lama.task, "fill_masked_area");
        assert_eq!(lama.file, "lama_fp32.onnx");
        assert_eq!(lama.revision, "a3ee2fca54baebec351b8fa7786154ffa7555aa6");
        assert_eq!(lama.sha256, "1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6");
        assert_eq!(lama.size, 208_044_816);
        assert_eq!(lama.licence_code, "Apache-2.0");
        assert_eq!(lama.licence_weights, "Apache-2.0");
        assert_eq!(lama.licence_training_data, "research-only");
        assert!(lama.url.starts_with("https://huggingface.co/Carve/LaMa-ONNX/resolve/"), "pinned to a revision: {}", lama.url);
        assert!(lama.note.is_some(), "the Places365 note is there to be acknowledged");
    }

    #[test]
    fn an_unknown_id_says_what_the_catalog_has() {
        let fail = find("sdxl").expect_err("that model is not in the catalog");
        assert_eq!(fail.status, Status::NotFound);
        assert!(fail.sentence.contains("lama"), "the sentence names what is available: {fail}");
    }

    #[test]
    fn list_lines_shows_the_state_of_each_model() {
        let scratch = Scratch::new("models-list");
        let entry = test_model(FIXTURE, Some(NOTE));
        let line = list_lines(std::slice::from_ref(&entry), &scratch.path).remove(0);
        assert!(line.contains("lama"), "{line}");
        assert!(line.contains("fill_masked_area"), "{line}");
        assert!(line.contains(&entry.size.to_string()), "{line}");
        assert!(line.contains("licences: code Apache-2.0, weights Apache-2.0, training data Apache-2.0"), "{line}");
        assert!(line.contains("not installed"), "{line}");
        assert!(line.contains("needs acknowledgment"), "{line}");
        install_model(&entry, &scratch.path, true, &FakeFetch::new(FIXTURE), &mut no_progress).expect("the fixture installs");
        let line = list_lines(std::slice::from_ref(&entry), &scratch.path).remove(0);
        assert!(line.contains("  installed  on disk: "), "installed, no longer asking for a note, and its bytes on disk: {line}");
    }

    #[test]
    fn a_flagged_model_is_refused_until_the_note_is_accepted() {
        let scratch = Scratch::new("models-accept");
        let entry = test_model(FIXTURE, Some(NOTE));
        let fetcher = FakeFetch::new(FIXTURE);
        let fail = install_model(&entry, &scratch.path, false, &fetcher, &mut no_progress).expect_err("the note has not been accepted");
        assert_eq!(fail.status, Status::Conflict);
        assert!(fail.sentence.contains(NOTE), "the note is shown, not hidden: {fail}");
        assert_eq!(fetcher.calls.get(), 0, "and nothing was downloaded");
        let message = install_model(&entry, &scratch.path, true, &fetcher, &mut no_progress).expect("accepting the note installs it");
        assert!(message.contains("is installed"), "{message}");
        assert!(message.contains(&entry.label()), "{message}");
        assert!(is_acknowledged(&scratch.path, &entry), "the acknowledgment is on disk");
        assert!(is_installed(&scratch.path, &entry));
    }

    #[test]
    fn a_changed_note_needs_a_new_acknowledgment() {
        let scratch = Scratch::new("models-note");
        let entry = test_model(FIXTURE, Some(NOTE));
        crate::store::write_acknowledgement(&scratch.path, &entry).expect("the first note is accepted");
        let changed = Model {
            note: Some(OverrideNote {
                by: "Erik".to_string(),
                date: "2026-10-14".to_string(),
                reason: "A new sentence.".to_string(),
                note: "Now it is fine for anything.".to_string(),
            }),
            ..entry.clone()
        };
        let fetcher = FakeFetch::new(FIXTURE);
        let fail = install_model(&changed, &scratch.path, false, &fetcher, &mut no_progress).expect_err("the new sentence has not been accepted");
        assert_eq!(fail.status, Status::Conflict);
        assert!(fail.sentence.contains("Now it is fine for anything."), "the new note is the one shown: {fail}");
        assert_eq!(fetcher.calls.get(), 0);
    }

    #[test]
    fn an_installed_model_is_reported_without_another_request() {
        let scratch = Scratch::new("models-twice");
        let entry = test_model(FIXTURE, None);
        let fetcher = FakeFetch::new(FIXTURE);
        install_model(&entry, &scratch.path, false, &fetcher, &mut no_progress).expect("the fixture installs");
        let message = install_model(&entry, &scratch.path, false, &fetcher, &mut no_progress).expect("installing it again is fine");
        assert!(message.contains("already installed"), "{message}");
        assert_eq!(fetcher.calls.get(), 1, "the second install asks the server nothing");
    }

    #[test]
    fn verify_model_names_a_model_that_is_not_on_disk() {
        let scratch = Scratch::new("models-verify");
        let entry = test_model(FIXTURE, None);
        let fail = verify_model(&entry, &scratch.path).expect_err("there is nothing to check");
        assert_eq!(fail.status, Status::NotFound);
        assert!(fail.sentence.contains("lama 1"), "the sentence names the model: {fail}");
    }

    #[test]
    fn list_lines_adds_the_bytes_on_disk_and_a_total() {
        let scratch = Scratch::new("models-usage");
        let root = scratch.path.join("models");
        std::fs::create_dir_all(&root).expect("the models folder can be made");
        let entry = test_model(FIXTURE, None);
        let lines = list_lines(std::slice::from_ref(&entry), &root);
        assert_eq!(lines.len(), 2, "one line for the model and one for the total");
        assert!(lines[0].contains("on disk: 0 bytes"), "nothing is installed yet: {}", lines[0]);
        install_model(&entry, &root, false, &FakeFetch::new(FIXTURE), &mut no_progress).expect("the fixture installs");
        let file = std::fs::metadata(model_file(&root, &entry)).expect("the model file is there");
        let record = std::fs::metadata(verified_file(&root, &entry)).expect("the record is there");
        let wanted = file.len() + record.len();
        let lines = list_lines(std::slice::from_ref(&entry), &root);
        assert!(lines[0].contains(&format!("  installed  on disk: {wanted} bytes")), "the model line names its own bytes: {}", lines[0]);
        assert_eq!(lines[1], format!("total on disk: {wanted} bytes in 2 files, models folder {}", root.display()), "the total names the folder as well");
    }

    #[test]
    fn remove_model_deletes_the_file_and_both_records() {
        let scratch = Scratch::new("models-remove");
        let root = scratch.path.join("models");
        let entry = test_model(FIXTURE, Some(NOTE));
        install_model(&entry, &root, true, &FakeFetch::new(FIXTURE), &mut no_progress).expect("the fixture installs with its acknowledgment");
        assert!(acknowledged_file(&root, &entry).exists(), "the acknowledgment is on disk before the removal");
        let version = model_folder(&root, &entry);
        assert_eq!(folder_usage(&version).expect("the version folder can be counted").files, 3, "the file, its record and the acknowledgment");
        let message = remove_model(&entry, &root, None, &|_port| false).expect("nothing is listening, so it can go");
        assert!(message.starts_with("lama 1 is removed"), "{message}");
        assert!(message.contains("3 files"), "the sentence says how much went: {message}");
        assert!(!version.exists(), "the version folder is gone");
        assert!(!root.join("lama").exists(), "and the id folder with it, because that was its only version");
        assert!(root.exists(), "the models folder itself stays");
        let fail = remove_model(&entry, &root, None, &|_port| false).expect_err("there is nothing left to remove");
        assert_eq!(fail.status, Status::NotFound);
        assert!(fail.sentence.contains("Nothing was deleted"), "{fail}");
    }

    #[test]
    fn remove_is_refused_while_a_manager_is_listening() {
        let scratch = Scratch::new("models-remove-live");
        let root = scratch.path.join("models");
        let entry = test_model(FIXTURE, None);
        install_model(&entry, &root, false, &FakeFetch::new(FIXTURE), &mut no_progress).expect("the fixture installs");
        let file = scratch.path.join("endpoint.json");
        crate::endpoint::write(&file, 41234, "a-token").expect("the endpoint file can be written");
        let fail = remove_model(&entry, &root, Some(&file), &|_port| true).expect_err("a manager that is answering holds its models");
        assert_eq!(fail.status, Status::Conflict);
        assert!(fail.sentence.contains("41234"), "the sentence names the port it asked: {fail}");
        assert!(is_installed(&root, &entry), "and nothing was deleted");
        // A file left behind by a manager that died names a port too, which is why the port is asked as well.
        let message = remove_model(&entry, &root, Some(&file), &|_port| false).expect("nobody answers that port, so the model can go");
        assert!(message.starts_with("lama 1 is removed"), "{message}");
        assert_eq!(live_server_port(None, &|_port| true), None, "no endpoint file means no live server");
    }

    #[test]
    fn remove_never_touches_anything_outside_the_models_folder() {
        let scratch = Scratch::new("models-remove-outside");
        let root = scratch.path.join("models");
        let keep = scratch.path.join("elsewhere").join("keep.txt");
        std::fs::create_dir_all(scratch.path.join("elsewhere")).expect("a folder outside the store can be made");
        std::fs::write(&keep, "keep these bytes").expect("a file outside the store can be written");
        for id in ["..", r"..\elsewhere", "lama/../../elsewhere"] {
            let entry = Model { id: id.to_string(), ..test_model(FIXTURE, None) };
            let fail = remove_model(&entry, &root, None, &|_port| false).expect_err("a name that cannot be a folder is refused");
            assert!(fail.sentence.contains("folder names"), "{id} is refused: {fail}");
            assert!(keep.exists(), "the file outside the models folder is untouched");
        }
        assert!(!root.exists(), "and nothing was made in the models folder either");
    }

    #[test]
    fn where_line_names_the_folder_and_says_who_named_it() {
        let root = std::env::temp_dir().join("caim-models-where");
        assert_eq!(where_line(&root, true), format!("models folder: {} (from CRAFT_AI_MODELS)", root.display()), "the environment gets named");
        assert_eq!(where_line(&root, false), format!("models folder: {} (the default data folder)", root.display()), "the default names itself");
    }

    #[cfg(unix)]
    #[test]
    fn remove_refuses_a_link_in_place_of_a_model_folder() {
        let scratch = Scratch::new("models-remove-link");
        let root = scratch.path.join("models");
        std::fs::create_dir_all(&root).expect("the models folder can be made");
        let entry = test_model(FIXTURE, None);
        // <root>/<id> is a link to a folder elsewhere, so <root>/<id>/<version> is really
        // <elsewhere>/<version> - the folder a delete would follow the link into.
        let outside = scratch.path.join("elsewhere");
        let linked = outside.join(&entry.version);
        std::fs::create_dir_all(&linked).expect("the folder the link points at can be made");
        let keep = linked.join(&entry.file);
        std::fs::write(&keep, "keep these bytes").expect("a file outside the store can be written");
        std::os::unix::fs::symlink(&outside, root.join(&entry.id)).expect("the link can be made");
        let fail = remove_model(&entry, &root, None, &|_port| false).expect_err("a link in place of the model folder is refused");
        assert!(fail.sentence.contains("is a link, not a folder"), "the link is named: {fail}");
        assert!(keep.exists(), "the file the link leads to is untouched");
    }

    #[test]
    fn remove_refuses_a_folder_with_more_folders_than_a_model_folder_should_have() {
        let scratch = Scratch::new("models-remove-deep");
        let root = scratch.path.join("models");
        let entry = test_model(FIXTURE, None);
        let mut deep = model_folder(&root, &entry);
        for _ in 0..17 {
            deep = deep.join("next");
        }
        std::fs::create_dir_all(&deep).expect("a tree past the walk cap can be made");
        let fail = remove_model(&entry, &root, None, &|_port| false).expect_err("a folder past the walk cap is refused");
        assert!(fail.sentence.contains("more folders than a model folder should"), "the refusal says why: {fail}");
        assert!(deep.exists(), "nothing was deleted");
    }

    #[test]
    fn list_says_when_a_folder_holds_too_many_folders_to_count() {
        let scratch = Scratch::new("models-list-deep");
        let root = scratch.path.join("models");
        let entry = test_model(FIXTURE, None);
        let mut deep = model_folder(&root, &entry);
        for _ in 0..17 {
            deep = deep.join("next");
        }
        std::fs::create_dir_all(&deep).expect("a tree past the walk cap can be made");
        let lines = list_lines(&[entry], &root);
        assert!(lines[0].ends_with("  on disk: too many folders to count"), "the model line says it could not count: {}", lines[0]);
        assert!(lines[1].starts_with("total on disk: too many folders to count, models folder "), "the total says it too: {}", lines[1]);
    }

    #[cfg(unix)]
    #[test]
    fn list_refuses_to_count_a_model_folder_that_is_a_link() {
        let scratch = Scratch::new("models-list-link");
        let root = scratch.path.join("models");
        std::fs::create_dir_all(&root).expect("the models folder can be made");
        let entry = test_model(FIXTURE, None);
        // <root>/<id> is a link to a folder elsewhere, so <root>/<id>/<version> is really
        // <elsewhere>/<version> - the tree a count would follow the link into.
        let outside = scratch.path.join("elsewhere");
        let linked = outside.join(&entry.version);
        std::fs::create_dir_all(&linked).expect("the folder the link points at can be made");
        std::fs::write(linked.join(&entry.file), FIXTURE).expect("a file outside the store can be written");
        std::os::unix::fs::symlink(&outside, root.join(&entry.id)).expect("the link can be made");
        let lines = list_lines(&[entry], &root);
        assert!(lines[0].ends_with("  on disk: a link, not a folder"), "the model line says it did not count: {}", lines[0]);
        assert!(lines[1].starts_with("total on disk: 0 bytes in 0 files"), "the total skips the link as well: {}", lines[1]);
    }

    #[cfg(unix)]
    #[test]
    fn list_refuses_to_count_a_models_folder_that_is_a_link() {
        let scratch = Scratch::new("models-list-root-link");
        let entry = test_model(FIXTURE, None);
        let outside = scratch.path.join("elsewhere");
        let linked = outside.join(&entry.id).join(&entry.version);
        std::fs::create_dir_all(&linked).expect("the folder the link points at can be made");
        std::fs::write(linked.join(&entry.file), FIXTURE).expect("a file outside the store can be written");
        let root = scratch.path.join("models");
        std::os::unix::fs::symlink(&outside, &root).expect("the link can be made");
        let lines = list_lines(&[entry], &root);
        assert!(lines[0].ends_with("  on disk: a link, not a folder"), "the model line says it did not count: {}", lines[0]);
        assert!(lines[1].starts_with("total on disk: a link, not a folder"), "the total says it too: {}", lines[1]);
    }

    #[cfg(unix)]
    #[test]
    fn a_models_folder_named_with_a_trailing_separator_is_still_checked_for_a_link() {
        let scratch = Scratch::new("models-root-link-slash");
        let entry = test_model(FIXTURE, None);
        let outside = scratch.path.join("elsewhere");
        let linked = outside.join(&entry.id).join(&entry.version);
        std::fs::create_dir_all(&linked).expect("the folder the link points at can be made");
        std::fs::write(linked.join(&entry.file), FIXTURE).expect("a file outside the store can be written");
        let root = scratch.path.join("models");
        std::os::unix::fs::symlink(&outside, &root).expect("the link can be made");
        // CRAFT_AI_MODELS="/somewhere/models/" names the same folder spelled with a trailing
        // separator, which is how a shell or a config file tends to write it. Asked about like that,
        // the filesystem answers about `outside` rather than about the link, so the link is
        // invisible unless the separator comes off before the link is looked at.
        let with_a_slash = std::path::PathBuf::from(format!("{}/", root.display()));
        let fail = remove_model(&entry, &with_a_slash, None, &|_port| false).expect_err("the link behind the trailing separator is refused");
        assert!(fail.sentence.contains("is a link, not a folder"), "the link is named: {fail}");
        assert!(linked.join(&entry.file).exists(), "the file the link leads to is untouched");
        let lines = list_lines(&[entry], &with_a_slash);
        assert!(lines[0].ends_with("  on disk: a link, not a folder"), "the model line says it did not count: {}", lines[0]);
        assert!(lines[1].starts_with("total on disk: a link, not a folder"), "the total says it too: {}", lines[1]);
    }
}
