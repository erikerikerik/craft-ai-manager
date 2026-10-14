//! The `caim models …` commands: what the catalog offers, what is on disk, and how a flagged
//! model gets its note accepted (SPEC v1.1 amendment 14).
//!
//! The functions that do the work take the models folder and a [Fetch](crate::fetch::Fetch) so the
//! tests can point them at a temp folder and a byte slice; the short wrappers next to them read
//! `CRAFT_AI_MODELS` and build the real HTTPS fetcher.

use std::path::{Path, PathBuf};

use crate::catalog::{self, Model};
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

/// The same, with the folder and the catalog handed over, for the tests.
#[must_use]
pub fn list_lines(models: &[Model], root: &Path) -> Vec<String> {
    models
        .iter()
        .map(|model| {
            let state = if store::is_installed(root, model) { "installed" } else { "not installed" };
            let needs = if model.note.is_some() && !store::is_acknowledged(root, model) { "  needs acknowledgment" } else { "" };
            format!(
                "{id}  {version}  {task}  {size} bytes  licences: code {code}, weights {weights}, training data {training}  {state}{needs}",
                id = model.id,
                version = model.version,
                task = model.task,
                size = model.size,
                code = model.licence_code,
                weights = model.licence_weights,
                training = model.licence_training_data,
                state = state,
                needs = needs,
            )
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use crate::catalog::{Model, OverrideNote};
    use crate::error::Status;
    use crate::models::{find, install_model, list_lines, verify_model};
    use crate::store::{is_acknowledged, is_installed};
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
        assert!(line.ends_with("installed"), "installed, and no longer asking for a note: {line}");
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
}
