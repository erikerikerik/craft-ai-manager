//! The model catalog: what models exist, where each one comes from, and whether CAIM may offer it.
//!
//! SPEC v1.1 amendment 14 fixes the shape. Every entry carries id, version, task, file, URL, revision,
//! SHA-256, size, the licence of the code, of the weights and of the training data, a provenance
//! sentence, and - when Erik has accepted a licence that is on the deny list - an override with a
//! reason and the sentence the user has to acknowledge. Amendment 13 is why the training data is in
//! there at all: permissive weights trained on non-commercial data are not commercial-safe.
//!
//! The catalog is compiled in with `include_str!`, and [gate](gate) reports every problem with it at
//! once rather than stopping at the first, so a bad entry fails `cargo test` before it can reach
//! anyone's machine.

use serde::Deserialize;

use crate::error::{Fail, Result};

/// The catalog itself, compiled in so a start never depends on a file that might be missing.
pub const MODELS_TOML: &str = include_str!("../catalog/models.toml");

/// The file endings CAIM will run: ONNX and safetensors, never pickle (AGENTS.md).
const ALLOWED_ENDINGS: [&str; 2] = ["onnx", "safetensors"];

/// Licence words that mean "not commercial-safe" (amendment 13), matched as whole words so `GPL`
/// catches `GPL-3.0` and `NC` catches `CC-BY-NC-4.0` without catching `Apache-2.0`. `research` is
/// a word of its own because a licence may say it in prose rather than as `research-only`: "for
/// research purposes only" is denied as well (MG-02 review).
const DENYLIST_WORDS: [&str; 5] = ["gpl", "lgpl", "agpl", "nc", "research"];

/// The same words when a licence spells them out with a version glued on (`GPLv3`, `OpenRAIL-M`).
const DENYLIST_PREFIXES: [&str; 4] = ["gpl", "lgpl", "agpl", "openrail"];

/// Phrases that say it in prose (`non-commercial`). `research only` is not listed here: the word
/// `research` above catches it, and catches it wherever it turns up.
const DENYLIST_PHRASES: [&str; 3] = ["openrail", "non commercial", "noncommercial"];

/// The fields every entry must fill in. The override is checked separately: it is what lets a
/// denylisted licence through, and a half-written one does not count.
const REQUIRED_FIELDS: [&str; 12] =
    ["id", "version", "task", "file", "url", "revision", "sha256", "size", "licence_code", "licence_weights", "licence_training_data", "provenance"];

/// The three licence fields amendment 13 asks for.
const LICENCE_FIELDS: [&str; 3] = ["licence_code", "licence_weights", "licence_training_data"];

/// One `[[model]]` block exactly as written, with every field optional so the gate can tell
/// "missing" from "empty" from "present".
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawEntry {
    pub id: Option<String>,
    pub version: Option<String>,
    pub task: Option<String>,
    pub file: Option<String>,
    pub url: Option<String>,
    pub revision: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    pub licence_code: Option<String>,
    pub licence_weights: Option<String>,
    pub licence_training_data: Option<String>,
    pub provenance: Option<String>,
    #[serde(rename = "override")]
    pub override_note: Option<RawOverride>,
}

/// The `[model.override]` table: who accepted the licence, when, why, and what the user is told.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawOverride {
    pub by: Option<String>,
    pub date: Option<String>,
    pub reason: Option<String>,
    pub note: Option<String>,
}

impl RawOverride {
    /// A complete override names all four things; a partial one is treated as no override at all.
    #[must_use]
    fn is_complete(&self) -> bool {
        self.missing().is_empty()
    }

    /// The fields that are missing or blank, as a sentence fragment.
    #[must_use]
    fn missing(&self) -> String {
        let wanted = [("by", &self.by), ("date", &self.date), ("reason", &self.reason), ("note", &self.note)];
        let blank: Vec<&str> = wanted.iter().filter(|(_, value)| value.as_deref().is_none_or(|text| text.trim().is_empty())).map(|(name, _)| *name).collect();
        if blank.is_empty() {
            return String::new();
        }
        blank.join(", ")
    }
}

/// A catalog entry the gate has accepted: every field present, the URL HTTPS, the SHA-256 hex, the
/// file ending allowed, and any denylisted licence covered by a complete override.
#[derive(Clone, Debug)]
pub struct Model {
    pub id: String,
    pub version: String,
    pub task: String,
    pub file: String,
    pub url: String,
    pub revision: String,
    pub sha256: String,
    pub size: u64,
    pub licence_code: String,
    pub licence_weights: String,
    pub licence_training_data: String,
    pub provenance: String,
    /// The override the user has to acknowledge, when the entry has a complete one.
    pub note: Option<OverrideNote>,
}

/// A complete override, all four fields filled in.
#[derive(Clone, Debug)]
pub struct OverrideNote {
    pub by: String,
    pub date: String,
    pub reason: String,
    /// The sentence CAIM shows before it will download the model.
    pub note: String,
}

impl Model {
    /// The one-line label used in messages: `lama 1`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} {}", self.id, self.version)
    }

    /// Turn a gate-checked entry into a [Model]. The gate has already run these checks, so this is
    /// the belt to the gate's braces: nothing here can panic, and a bad entry is an error.
    fn from_raw(entry: &RawEntry) -> Result<Self> {
        let label = label(entry, 0);
        let mut problems = field_problems(entry, &label);
        problems.extend(licence_problems(entry, &label));
        if let Some(first) = problems.first() {
            return Err(Fail::internal(first.clone()));
        }
        Ok(Self {
            id: text(entry, "id").unwrap_or_default(),
            version: text(entry, "version").unwrap_or_default(),
            task: text(entry, "task").unwrap_or_default(),
            file: text(entry, "file").unwrap_or_default(),
            url: text(entry, "url").unwrap_or_default(),
            revision: text(entry, "revision").unwrap_or_default(),
            sha256: text(entry, "sha256").unwrap_or_default(),
            size: entry.size.unwrap_or(0),
            licence_code: text(entry, "licence_code").unwrap_or_default(),
            licence_weights: text(entry, "licence_weights").unwrap_or_default(),
            licence_training_data: text(entry, "licence_training_data").unwrap_or_default(),
            provenance: text(entry, "provenance").unwrap_or_default(),
            note: entry.override_note.as_ref().filter(|note| note.is_complete()).map(|note| OverrideNote {
                by: note.by.clone().unwrap_or_default(),
                date: note.date.clone().unwrap_or_default(),
                reason: note.reason.clone().unwrap_or_default(),
                note: note.note.clone().unwrap_or_default(),
            }),
        })
    }
}

/// The catalog as written, parsed. A TOML syntax error or a field of the wrong type is an error;
/// missing fields are not, because [gate](gate) is what reports those.
pub fn parse(text: &str) -> Result<Vec<RawEntry>> {
    #[derive(Deserialize)]
    struct Catalog {
        #[serde(default)]
        model: Vec<RawEntry>,
    }
    let parsed: Catalog = toml::from_str(text).map_err(|error| Fail::internal(format!("The model catalog does not parse: {error}")))?;
    if parsed.model.is_empty() {
        return Err(Fail::internal("The model catalog has no [[model]] entries."));
    }
    Ok(parsed.model)
}

/// Read the embedded catalog, run the gate over it, and hand back usable models. Every problem with
/// the catalog comes back in one error, so a person fixing it sees the whole list.
pub fn load() -> Result<Vec<Model>> {
    let raw = parse(MODELS_TOML)?;
    let problems = gate(&raw);
    if !problems.is_empty() {
        return Err(Fail::internal(format!("The model catalog is not usable. {}", problems.join(" "))));
    }
    raw.iter().map(Model::from_raw).collect()
}

/// Every problem with a catalog, in the order it was found: missing and empty fields, `UNCLEAR`,
/// a non-HTTPS URL, a SHA-256 that is not 64 lowercase hex, a file that is not `.onnx` or
/// `.safetensors`, a duplicated id and version, and a denylisted licence with no complete override.
/// An empty vector means the catalog is fine to offer.
#[must_use]
pub fn gate(entries: &[RawEntry]) -> Vec<String> {
    let mut problems = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let label = label(entry, index);
        problems.extend(field_problems(entry, &label));
        problems.extend(licence_problems(entry, &label));
    }
    for (index, entry) in entries.iter().enumerate() {
        for other in entries.iter().skip(index.saturating_add(1)) {
            let same_id = both_present(entry.id.as_deref(), other.id.as_deref());
            let same_version = both_present(entry.version.as_deref(), other.version.as_deref());
            if same_id && same_version {
                problems.push(format!(
                    "Two catalog entries have the same id and version: \"{}\" \"{}\". One of them needs a different version.",
                    entry.id.clone().unwrap_or_default(),
                    entry.version.clone().unwrap_or_default()
                ));
            }
        }
    }
    problems
}

/// True when both sides are present and equal, and neither is blank.
fn both_present(one: Option<&str>, other: Option<&str>) -> bool {
    match (one, other) {
        (Some(one), Some(other)) => one.trim() == other.trim() && !one.trim().is_empty(),
        _ => false,
    }
}

/// How an entry is named in a problem sentence.
fn label(entry: &RawEntry, index: usize) -> String {
    let id = entry.id.as_deref().unwrap_or_default().trim();
    let version = entry.version.as_deref().unwrap_or_default().trim();
    if id.is_empty() {
        return format!("Catalog entry number {}", index.saturating_add(1));
    }
    if version.is_empty() {
        return format!("Catalog entry \"{id}\"");
    }
    format!("Catalog entry \"{id}\" version \"{version}\"")
}

/// The field a gate sentence is about, as text so one loop can cover them all.
fn text(entry: &RawEntry, field: &str) -> Option<String> {
    match field {
        "id" => entry.id.clone(),
        "version" => entry.version.clone(),
        "task" => entry.task.clone(),
        "file" => entry.file.clone(),
        "url" => entry.url.clone(),
        "revision" => entry.revision.clone(),
        "sha256" => entry.sha256.clone(),
        "licence_code" => entry.licence_code.clone(),
        "licence_weights" => entry.licence_weights.clone(),
        "licence_training_data" => entry.licence_training_data.clone(),
        "provenance" => entry.provenance.clone(),
        "size" => entry.size.map(|value| value.to_string()),
        _ => None,
    }
}

/// The per-entry problems that are not about licences: presence, `UNCLEAR`, the URL, the hash,
/// the file ending, and names that would escape the store folder.
fn field_problems(entry: &RawEntry, label: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for field in REQUIRED_FIELDS {
        match text(entry, field) {
            None => problems.push(format!("{label} is missing the field \"{field}\".")),
            Some(value) if value.trim().is_empty() => problems.push(format!("{label} has an empty \"{field}\".")),
            Some(value) if value.to_lowercase().contains("unclear") => {
                problems.push(format!("{label} says UNCLEAR in \"{field}\". Nobody has checked what it actually is."));
            }
            Some(_) => {}
        }
    }
    if let Some(url) = text(entry, "url")
        && !url.starts_with("https://")
    {
        problems.push(format!("{label} has a URL that is not HTTPS: {url}. CAIM only downloads over TLS."));
    }
    if let Some(sha) = text(entry, "sha256")
        && !is_sha256(&sha)
    {
        problems.push(format!("{label} has a \"sha256\" that is not 64 lowercase hex characters."));
    }
    if let Some(file) = text(entry, "file") {
        let lower = file.to_lowercase();
        if !ALLOWED_ENDINGS.iter().any(|ending| lower.ends_with(&format!(".{ending}"))) {
            problems.push(format!("{label} wants the file \"{file}\". CAIM only runs .onnx and .safetensors files."));
        }
    }
    for field in ["id", "version", "file"] {
        if let Some(value) = text(entry, field)
            && !is_name(&value)
        {
            problems.push(format!("{label} has a \"{field}\" that cannot be used as a folder or file name: \"{value}\"."));
        }
    }
    problems
}

/// The licence problems: anything on the deny list needs a complete override, and an override that
/// is only half written does not count as one.
fn licence_problems(entry: &RawEntry, label: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let covered = entry.override_note.as_ref().is_some_and(|note| note.is_complete());
    for field in LICENCE_FIELDS {
        let Some(value) = text(entry, field) else { continue };
        let Some(hit) = denied_by(&value) else { continue };
        if covered {
            continue;
        }
        problems.push(format!(
            "{label} is licensed \"{value}\" for {field}, which is on the deny list ({hit}). It needs a complete override from Erik - by, date, reason and note - before CAIM may offer it."
        ));
    }
    if let Some(note) = &entry.override_note {
        let missing = note.missing();
        if !missing.is_empty() {
            problems.push(format!("{label} has an override that is missing {missing}. A half-written override does not count."));
        }
    }
    problems
}

/// Which deny-list rule a licence breaks, if any. Case-insensitive, and punctuation is treated as a
/// space, so `CC-BY-NC-4.0` is the words `cc by nc 4 0` and `research-only` is `research only`.
#[must_use]
fn denied_by(licence: &str) -> Option<&'static str> {
    let flat = flatten(licence);
    let words: Vec<&str> = flat.split(' ').filter(|word| !word.is_empty()).collect();
    for word in &words {
        for denied in DENYLIST_WORDS {
            if *word == denied {
                return Some(denied);
            }
        }
        for denied in DENYLIST_PREFIXES {
            if word.starts_with(denied) {
                return Some(denied);
            }
        }
    }
    DENYLIST_PHRASES.into_iter().find(|phrase| flat.contains(*phrase))
}

/// Lowercase, with every character that is not a letter or a digit turned into a space and the runs
/// of spaces collapsed to one.
fn flatten(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<&str>>()
        .join(" ")
}

/// A SHA-256 as the catalog writes it: 64 lowercase hex characters.
#[must_use]
fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether a catalog name is safe to use as one folder or file name. A bad entry must not be able to
/// point the store outside `models\\<id>\\<version>\\`.
#[must_use]
fn is_name(text: &str) -> bool {
    !text.is_empty() && text != "." && text != ".." && !text.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) && !text.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{MODELS_TOML, gate, load, parse};

    /// A complete, clean entry, so each test changes exactly one thing about it. TOML literal
    /// strings (single quotes) keep the fixture readable next to the Rust.
    const GOOD: &str = r#"[[model]]
id = 'lama'
version = '1'
task = 'fill_masked_area'
file = 'lama_fp32.onnx'
url = 'https://example.invalid/lama_fp32.onnx'
revision = 'a3ee2fca54baebec351b8fa7786154ffa7555aa6'
sha256 = '1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6'
size = 208044816
licence_code = 'Apache-2.0'
licence_weights = 'Apache-2.0'
licence_training_data = 'Apache-2.0'
provenance = 'Built by hand for a test.'
"#;

    /// The twelve lines every entry has to fill in, in the order the gate checks them.
    const FIELD_LINES: [&str; 12] = [
        "id = 'lama'",
        "version = '1'",
        "task = 'fill_masked_area'",
        "file = 'lama_fp32.onnx'",
        "url = 'https://example.invalid/lama_fp32.onnx'",
        "revision = 'a3ee2fca54baebec351b8fa7786154ffa7555aa6'",
        "sha256 = '1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6'",
        "size = 208044816",
        "licence_code = 'Apache-2.0'",
        "licence_weights = 'Apache-2.0'",
        "licence_training_data = 'Apache-2.0'",
        "provenance = 'Built by hand for a test.'",
    ];

    const SHA_LINE: &str = "sha256 = '1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6'";
    const TRAINING_LINE: &str = "licence_training_data = 'Apache-2.0'";

    /// A complete override, which is what lets a denylisted licence through.
    const OVERRIDE: &str =
        "[model.override]\nby = 'Erik'\ndate = '2026-10-13'\nreason = 'M1 and personal use only.'\nnote = 'Use it for personal work only.'\n";

    fn problems(toml: &str) -> Vec<String> {
        gate(&parse(toml).expect("the test catalog parses"))
    }

    /// GOOD with one piece replaced by something else; the piece has to appear exactly once.
    fn swap(from: &str, to: &str) -> String {
        assert_eq!(GOOD.matches(from).count(), 1, "the fixture has exactly one {from}");
        GOOD.replace(from, to)
    }

    #[test]
    fn the_embedded_catalog_passes_the_gate() {
        let found = problems(MODELS_TOML);
        assert!(found.is_empty(), "the shipped catalog has to be clean, or CI stops here: {found:?}");
    }

    #[test]
    fn the_embedded_entry_is_the_lama_from_the_photo_craft_note() {
        let models = load().expect("the shipped catalog is usable");
        assert_eq!(models.len(), 1, "MG-02 ships exactly one model");
        let lama = &models[0];
        assert_eq!(lama.id, "lama");
        assert_eq!(lama.version, "1");
        assert_eq!(lama.task, "fill_masked_area");
        assert_eq!(lama.file, "lama_fp32.onnx");
        assert_eq!(lama.revision, "a3ee2fca54baebec351b8fa7786154ffa7555aa6");
        assert!(
            lama.url.starts_with("https://huggingface.co/Carve/LaMa-ONNX/resolve/a3ee2fca54baebec351b8fa7786154ffa7555aa6/"),
            "pinned to a revision: {}",
            lama.url
        );
        assert_eq!(lama.sha256, "1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6");
        assert_eq!(lama.size, 208_044_816);
        assert_eq!(lama.licence_code, "Apache-2.0");
        assert_eq!(lama.licence_weights, "Apache-2.0");
        assert_eq!(lama.licence_training_data, "research-only");
        assert!(!lama.provenance.is_empty(), "and it says where the model came from");
        let note = lama.note.as_ref().expect("the override is complete");
        assert_eq!(note.by, "Erik");
        assert_eq!(note.date, "2026-10-13");
        assert!(note.reason.contains("D2"), "{note:?}");
        assert!(note.note.contains("Places365"), "the user-facing sentence names the training data: {note:?}");
    }

    #[test]
    fn a_clean_entry_passes() {
        assert!(problems(GOOD).is_empty());
    }

    #[test]
    fn every_required_field_is_named_when_it_is_missing() {
        for line in FIELD_LINES {
            let found = problems(&swap(line, "# left out on purpose"));
            assert_eq!(found.len(), 1, "one problem for the missing {line}: {found:?}");
            assert!(found[0].contains("is missing the field"), "{found:?}");
            assert!(found[0].contains(line.split(" = ").next().expect("the field name")), "and names it: {found:?}");
        }
    }

    #[test]
    fn an_empty_field_is_named() {
        let found = problems(&swap("provenance = 'Built by hand for a test.'", "provenance = ''"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("has an empty"), "{found:?}");
        assert!(found[0].contains("provenance"), "{found:?}");
    }

    #[test]
    fn unclear_is_never_good_enough() {
        let found = problems(&swap("licence_weights = 'Apache-2.0'", "licence_weights = 'UNCLEAR'"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("UNCLEAR"), "{found:?}");
        let found = problems(&swap("provenance = 'Built by hand for a test.'", "provenance = 'nobody knows, unclear'"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("UNCLEAR"), "and it is caught in lower case too: {found:?}");
    }

    #[test]
    fn a_plain_http_url_is_refused() {
        let found = problems(&swap("url = 'https://example.invalid/lama_fp32.onnx'", "url = 'http://example.invalid/lama_fp32.onnx'"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("HTTPS"), "{found:?}");
    }

    #[test]
    fn a_sha256_has_to_be_64_lowercase_hex() {
        let upper = problems(&swap(SHA_LINE, "sha256 = '1FAEF5301D78DB7DDA502FE59966957EC4B79DD64E16F03ED96913C7A4EB68D6'"));
        assert_eq!(upper.len(), 1, "{upper:?}");
        assert!(upper[0].contains("64 lowercase hex"), "{upper:?}");
        let short = problems(&swap(SHA_LINE, "sha256 = '1faef5'"));
        assert_eq!(short.len(), 1, "{short:?}");
        assert!(short[0].contains("64 lowercase hex"), "{short:?}");
        let not_hex = problems(&swap(SHA_LINE, "sha256 = 'zfaef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6'"));
        assert_eq!(not_hex.len(), 1, "{not_hex:?}");
        assert!(not_hex[0].contains("64 lowercase hex"), "{not_hex:?}");
    }

    #[test]
    fn only_onnx_and_safetensors_are_allowed() {
        let found = problems(&swap("file = 'lama_fp32.onnx'", "file = 'lama_fp32.pkl'"));
        assert!(found.iter().any(|problem| problem.contains(".onnx and .safetensors")), "{found:?}");
        assert!(problems(&swap("file = 'lama_fp32.onnx'", "file = 'lama_fp32.safetensors'")).is_empty(), "safetensors is fine");
    }

    #[test]
    fn two_entries_with_the_same_id_and_version_are_refused() {
        let found = problems(&format!("{GOOD}{GOOD}"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("same id and version"), "{found:?}");
    }

    #[test]
    fn a_second_version_of_the_same_id_is_fine() {
        let second = GOOD.replace("version = '1'", "version = '2'");
        assert!(problems(&format!("{GOOD}{second}")).is_empty(), "different versions of one model are allowed");
    }

    #[test]
    fn every_denylisted_licence_needs_an_override() {
        for licence in ["GPL-3.0", "GNU LGPLv2.1", "AGPL-3.0", "CC-BY-NC-4.0", "research-only", "OpenRAIL-M", "non-commercial", "NonCommercial", "GPL"] {
            let found = problems(&swap(TRAINING_LINE, &format!("licence_training_data = '{licence}'")));
            assert_eq!(found.len(), 1, "{licence} belongs on the deny list: {found:?}");
            assert!(found[0].contains("deny list"), "{licence}: {found:?}");
            let flagged = swap(TRAINING_LINE, &format!("licence_training_data = '{licence}'"));
            assert!(problems(&format!("{flagged}{OVERRIDE}")).is_empty(), "{licence} is allowed with a complete override");
        }
    }

    #[test]
    fn permissive_licences_pass_the_gate() {
        for licence in ["Apache-2.0", "MIT", "BSD-3-Clause", "CC0-1.0", "CC-BY-4.0", "ISC", "Zlib"] {
            assert!(problems(&swap(TRAINING_LINE, &format!("licence_training_data = '{licence}'"))).is_empty(), "{licence} is commercial-safe");
        }
    }

    #[test]
    fn the_word_research_is_denied_wherever_it_appears() {
        // MG-02 review: the phrase rule only matched `research only` side by side, so a licence
        // that spells it out as "for research purposes only" walked straight through the gate.
        for licence in ["for research purposes only", "Research use only", "non-commercial research", "research", "Academic Research Licence"] {
            let flagged = swap(TRAINING_LINE, &format!("licence_training_data = '{licence}'"));
            let found = problems(&flagged);
            assert_eq!(found.len(), 1, "{licence} belongs on the deny list: {found:?}");
            assert!(found[0].contains("deny list (research)"), "{licence}: {found:?}");
            assert!(problems(&format!("{flagged}{OVERRIDE}")).is_empty(), "{licence} is allowed with a complete override");
        }
    }

    #[test]
    fn lama_is_in_the_catalog_only_because_erik_overrode_it() {
        // The shipped entry says `research-only`, which the word rule catches too, so the override
        // is load-bearing: switch it off and the gate has to stop the release.
        let without = MODELS_TOML.replace("[model.override]", "[model.override_disabled]");
        assert_ne!(MODELS_TOML, without, "the shipped catalog has an override block to find");
        let found = problems(&without);
        assert!(found.iter().any(|problem| problem.contains("deny list (research)")), "LaMa needs its override: {found:?}");
        assert!(problems(MODELS_TOML).is_empty(), "with the override the shipped catalog is clean");
    }

    #[test]
    fn a_half_written_override_does_not_count() {
        let flagged = swap(TRAINING_LINE, "licence_training_data = 'research-only'");
        let found = problems(&format!("{flagged}[model.override]\nby = 'Erik'\ndate = '2026-10-13'\n"));
        assert_eq!(found.len(), 2, "the licence problem and the half-written override: {found:?}");
        assert!(found.iter().any(|problem| problem.contains("deny list")), "{found:?}");
        assert!(found.iter().any(|problem| problem.contains("half-written override")), "{found:?}");
        assert!(found.iter().any(|problem| problem.contains("reason") && problem.contains("note")), "it says what is missing: {found:?}");
    }

    #[test]
    fn a_name_that_could_escape_the_store_is_refused() {
        let found = problems(&swap("id = 'lama'", "id = '../evil'"));
        assert!(found.iter().any(|problem| problem.contains("folder or file name")), "an id with a slash is refused: {found:?}");
        let found = problems(&swap("version = '1'", "version = '..'"));
        assert!(found.iter().any(|problem| problem.contains("folder or file name")), "a version that is .. is refused: {found:?}");
        let found = problems(&swap("file = 'lama_fp32.onnx'", "file = 'sub/dir.onnx'"));
        assert!(found.iter().any(|problem| problem.contains("folder or file name")), "{found:?}");
    }

    #[test]
    fn every_problem_in_one_entry_comes_back_at_once() {
        let bad = swap("url = 'https://example.invalid/lama_fp32.onnx'", "url = 'http://example.invalid/lama.pkl'");
        let bad = bad.replace("file = 'lama_fp32.onnx'", "file = 'lama.pkl'");
        let found = problems(&bad);
        assert_eq!(found.len(), 2, "both problems, not just the first: {found:?}");
        assert!(found.iter().any(|problem| problem.contains("HTTPS")), "{found:?}");
        assert!(found.iter().any(|problem| problem.contains(".onnx and .safetensors")), "{found:?}");
    }

    #[test]
    fn a_catalog_that_does_not_parse_is_an_error() {
        assert!(parse("this is not toml").is_err(), "a broken file is an error, not an empty catalog");
        assert!(parse("").is_err(), "an empty catalog has nothing to offer");
        assert!(parse("[[model]]\nid = 7").is_err(), "a field of the wrong type is an error");
    }
}
