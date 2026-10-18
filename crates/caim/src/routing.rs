//! Amendment 18: the one place that maps a task to the model that serves it.
//!
//! PhotoCraft asks for a task and never names a model, so swapping the model behind a task changes
//! nothing on the client. This module is the only code that reads the catalog to decide what can
//! run: the [Runner](crate::job::Runner) and both ONNX runners are handed a model, they never
//! choose one. It also owns the wording of the two answers amendment 18 spells out in prose.

use std::path::{Path, PathBuf};

use crate::backend::{FAKE_MASK_MODEL, FAKE_MODEL};
use crate::catalog::{self, Model};
use crate::store;

/// The task MG-10 adds: find the subject of the whole image.
pub const TASK_MASK: &str = "subject_mask";
/// What `output` says when `result.png` is an image. Amendment 18: a missing `output` means this.
pub const OUTPUT_IMAGE: &str = "image";
/// What `output` says when `result.png` is an 8-bit grayscale matte.
pub const OUTPUT_MASK: &str = "mask";

/// The shape of a task's `result.png`. Anything this manager does not know answers as an image,
/// which is amendment 18's rule for a client that predates mask tasks.
#[must_use]
pub fn output_for(task: &str) -> &'static str {
    if task == TASK_MASK { OUTPUT_MASK } else { OUTPUT_IMAGE }
}

/// The 400 sentence for a task whose model is not installed and acknowledged. Amendment 18 gives
/// the mask task its own plain sentence naming the feature; Fill keeps the exact sentence MG-01
/// shipped, so an M1-shaped peer sees no change (B20).
#[must_use]
pub fn not_ready_sentence(task: &str) -> String {
    if task == TASK_MASK { "The Select Subject model isn't installed.".to_string() } else { format!("No model is installed for {task} yet") }
}

/// The `message` a job carries the moment the worker takes it, before the runner starts naming its
/// own stages. It names the feature, because that is what the user is waiting for.
#[must_use]
pub fn running_message(task: &str) -> &'static str {
    if task == TASK_MASK { "Craft AI Manager is finding the subject." } else { "Craft AI Manager is filling the masked area." }
}

/// One task `/v1/tasks` lists, and whether a job for it would actually run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The task id PhotoCraft puts in the `job` part.
    pub task: String,
    /// `image` or `mask`: the shape of `result.png`.
    pub output: &'static str,
    /// Whether the task needs a text prompt. Neither M2 task does.
    pub needs_prompt: bool,
    /// The pixel cap for this task, taken from the catalog entry that serves it.
    pub max_pixels: u64,
    /// The catalog id of the model that serves it. Informational: PhotoCraft never chooses by it.
    pub model: String,
    /// Installed *and* acknowledged, so a job for this task would run rather than be refused.
    pub ready: bool,
}

/// The tasks this start can see, in catalog order, and which of them are ready.
#[derive(Clone, Debug, Default)]
pub struct Offered {
    entries: Vec<Entry>,
    /// The catalog entry behind each task, in the same order as `entries`.
    models: Vec<Model>,
}

impl Offered {
    /// What `--fake-backend` offers: every task the catalog knows, with a fake model behind it.
    #[must_use]
    pub fn fake() -> Self {
        Self::build(|_| true, |model| if model.task == TASK_MASK { FAKE_MASK_MODEL } else { FAKE_MODEL }.to_string())
    }

    /// What the model store offers: every task the catalog knows, ready only when the model that
    /// serves it is installed *and* acknowledged.
    ///
    /// `root` replaces the model store when the caller names one, which is how a test can say
    /// "nothing is installed" without touching the model folder on the machine.
    #[must_use]
    pub fn from_store(root: Option<&Path>) -> Self {
        let Some(root) = root.map(PathBuf::from).or_else(store::models_dir) else {
            return Self::default();
        };
        Self::build(|model| store::is_installed(&root, model) && store::is_acknowledged(&root, model), |model| model.id.clone())
    }

    /// The tasks, with `ready` decided by `ready` and the reported model name chosen by `label`.
    fn build(ready: impl Fn(&Model) -> bool, label: impl Fn(&Model) -> String) -> Self {
        let Ok(catalog) = catalog::load() else {
            // The embedded catalog is checked by the licence gate in CI, so this only happens if a
            // shipped entry is broken. Offering nothing is the safe answer: every job then gets a
            // sentence rather than a half-built backend.
            return Self::default();
        };
        // One model per task, and the catalog's order is the choice: the first entry that serves a
        // task wins, which is how MG-11 can add a second `subject_mask` model without changing the
        // answer today.
        let mut models: Vec<Model> = Vec::new();
        for model in catalog {
            if !models.iter().any(|known: &Model| known.task == model.task) {
                models.push(model);
            }
        }
        let entries = models
            .iter()
            .map(|model| Entry {
                task: model.task.clone(),
                output: output_for(&model.task),
                needs_prompt: false,
                max_pixels: model.max_pixels,
                model: label(model),
                ready: ready(model),
            })
            .collect();
        Self { entries, models }
    }

    /// What `GET /v1/tasks` lists.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry for `task`, when this manager knows the task at all.
    #[must_use]
    pub fn get(&self, task: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.task == task)
    }

    /// Whether a job for `task` would run rather than be refused.
    #[must_use]
    pub fn is_ready(&self, task: &str) -> bool {
        self.get(task).is_some_and(|entry| entry.ready)
    }

    /// The catalog entry that serves `task`, for the start that builds the runners. `None` when no
    /// entry serves it, which is the same as not knowing the task.
    #[must_use]
    pub fn model_for(&self, task: &str) -> Option<&Model> {
        self.entries.iter().zip(self.models.iter()).find(|(entry, _)| entry.task == task).map(|(_, model)| model)
    }

    /// Whether either task has a model behind it, which is what the terminal line reports.
    #[must_use]
    pub fn any_ready(&self) -> bool {
        self.entries.iter().any(|entry| entry.ready)
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, OUTPUT_IMAGE, OUTPUT_MASK, Offered, TASK_MASK, not_ready_sentence, output_for};
    use crate::backend::{FAKE_MASK_MODEL, FAKE_MODEL};
    use crate::protocol::TASK_FILL;
    use crate::testkit::Scratch;

    #[test]
    fn the_fake_backend_offers_both_tasks_ready() {
        let offered = Offered::fake();
        let listed: Vec<Entry> = offered.entries().to_vec();
        assert_eq!(listed.len(), 2, "fill and mask: {listed:?}");
        assert_eq!(listed[0].task, TASK_FILL);
        assert_eq!(listed[0].output, OUTPUT_IMAGE);
        assert_eq!(listed[0].model, FAKE_MODEL);
        assert_eq!(listed[1].task, TASK_MASK);
        assert_eq!(listed[1].output, OUTPUT_MASK);
        assert_eq!(listed[1].model, FAKE_MASK_MODEL);
        assert!(listed.iter().all(|entry| entry.ready), "the fake backend can always run");
        assert!(listed.iter().all(|entry| !entry.needs_prompt), "neither M2 task takes a prompt");
        assert!(offered.any_ready());
    }

    #[test]
    fn each_task_carries_the_cap_of_the_entry_that_serves_it() {
        let offered = Offered::fake();
        assert_eq!(offered.get(TASK_FILL).expect("fill is listed").max_pixels, 64_000_000);
        assert_eq!(offered.get(TASK_MASK).expect("mask is listed").max_pixels, 1_048_576, "the mask task has its own, smaller cap");
        assert!(offered.get("select_subject").is_none(), "a task nobody serves is not listed");
        assert!(!offered.is_ready("select_subject"));
    }

    #[test]
    fn an_empty_store_lists_every_task_but_marks_none_ready() {
        let scratch = Scratch::new("routing-empty");
        let offered = Offered::from_store(Some(&scratch.path));
        assert_eq!(offered.entries().len(), 2, "the tasks are still listed, so PhotoCraft can grey them out");
        assert!(offered.entries().iter().all(|entry| !entry.ready), "{:?}", offered.entries());
        assert_eq!(
            offered.entries().iter().map(|entry| entry.model.as_str()).collect::<Vec<&str>>(),
            ["lama", "birefnet-lite"],
            "the real model ids, not fake ones"
        );
        assert!(!offered.any_ready());
    }

    #[test]
    fn no_store_folder_at_all_offers_nothing() {
        // `store::models_dir()` answers None when the platform path cannot be worked out.
        assert!(Offered::from_store(Some(std::path::Path::new("/definitely/not/here"))).entries().iter().all(|entry| !entry.ready));
    }

    #[test]
    fn the_model_behind_a_task_comes_from_the_catalog() {
        let offered = Offered::from_store(None);
        let fill = offered.model_for(TASK_FILL).expect("the catalog serves fill");
        assert_eq!(fill.id, "lama");
        let mask = offered.model_for(TASK_MASK).expect("the catalog serves mask");
        assert_eq!(mask.id, "birefnet-lite");
        assert!(offered.model_for("upscale").is_none(), "a task with no entry has no model");
    }

    #[test]
    fn the_two_not_ready_sentences_are_the_two_amendment_18_asks_for() {
        assert_eq!(not_ready_sentence(TASK_FILL), "No model is installed for fill_masked_area yet", "MG-01's sentence, byte for byte (B20)");
        assert_eq!(not_ready_sentence(TASK_MASK), "The Select Subject model isn't installed.");
    }

    #[test]
    fn an_unknown_task_answers_as_an_image() {
        assert_eq!(output_for(TASK_FILL), OUTPUT_IMAGE);
        assert_eq!(output_for(TASK_MASK), OUTPUT_MASK);
        assert_eq!(output_for("upscale"), OUTPUT_IMAGE, "amendment 18: a missing output means image");
    }
}
