//! The JSON of amendment 11, from the manager's side: what a job asks for, what each endpoint
//! answers, and the checks that turn a wrong job into a plain sentence rather than a wrong answer.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Fail, Result, quoted};
use crate::image::{CHANNELS, DEPTHS};
use crate::job;
use crate::routing::{self, Offered};

/// The only task MG-01 knows; the real models come with MG-02 and MG-04.
pub const TASK_FILL: &str = "fill_masked_area";
/// The pixel cap, reported as `max_pixels` in `/v1/tasks`.
pub const MAX_PIXELS: u64 = 64_000_000;
/// Amendment 11: JSON bodies at most 64 KiB.
pub const MAX_JSON_BYTES: usize = 64 * 1024;
/// The prompt cap the client already applies; the manager checks it too.
pub const MAX_PROMPT_BYTES: usize = 4096;
/// A client label longer than this is not usable.
pub const MAX_CLIENT_BYTES: usize = 256;
/// Job ids go into a URL path, so they are checked exactly as the client checks them.
pub const MAX_JOB_ID_BYTES: usize = 512;
/// The protocol version this manager speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// The provider value that means "the manager chooses".
pub const PROVIDER_AUTO: &str = "auto";
/// The device value that means "the manager chooses". Amendment 18 reports it back as
/// `device_requested`; it is the only value until CAIM-04 adds a device setting.
pub const DEVICE_AUTO: &str = "auto";
/// The devices this manager can run on. MG-01 has one.
pub const DEVICES: [&str; 1] = ["cpu"];

/// The pixel cap in the words a person reads: megapixels at a megapixel or more, plain pixels below
/// it. The number is rounded down to the nearest tenth so the sentence never claims more room than
/// the task has, and one megapixel is singular because that is how it reads out loud.
fn plain_pixels(pixels: u64) -> String {
    if pixels < 1_000_000 {
        return format!("{pixels} pixels");
    }
    let (whole, tenth) = (pixels / 1_000_000, pixels / 100_000 % 10);
    if tenth == 0 { format!("{whole} {}", if whole == 1 { "megapixel" } else { "megapixels" }) } else { format!("{whole}.{tenth} megapixels") }
}

/// The `job` part of `POST /v1/jobs`.
#[derive(Clone, Debug, Deserialize)]
pub struct JobRequest {
    pub task: String,
    pub image_size: [u32; 2],
    pub region: [i32; 4],
    pub bit_depth: u8,
    pub channels: u8,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub settings: Option<Value>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub client: Option<String>,
}

impl JobRequest {
    /// Read the `job` part. Unknown fields are ignored so a newer client can still be served.
    pub fn parse(bytes: &[u8]) -> Result<JobRequest> {
        if bytes.len() > MAX_JSON_BYTES {
            return Err(Fail::bad_request("The job request is larger than the 64 KiB the protocol allows."));
        }
        Ok(serde_json::from_slice(bytes)?)
    }

    /// Check the job against the protocol, and against what this manager can actually run.
    ///
    /// `offered` is what this start can run: every task the catalog knows, each marked ready or not.
    /// A task nobody serves is refused with the "does not know the task" sentence; a task whose
    /// model is missing gets amendment 18's sentence for that feature.
    pub fn validate(&self, offered: &Offered) -> Result<()> {
        let Some(entry) = offered.get(&self.task) else {
            return Err(Fail::bad_request(format!("Craft AI Manager does not know the task {} yet.", quoted(&self.task))));
        };
        if !entry.ready {
            return Err(Fail::bad_request(routing::not_ready_sentence(&self.task)));
        }
        let [width, height] = self.image_size;
        if width == 0 || height == 0 {
            return Err(Fail::bad_request("The image size must be at least one pixel wide and one pixel high."));
        }
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        // The cap is the task's own, from the catalog entry that serves it. Amendment 18 says a mask
        // task is never refused for the size of the *document* - PhotoCraft downscales first - but
        // the size actually sent is still capped, so a big image cannot walk in through the mask
        // path and outrun the memory budget.
        if pixels > entry.max_pixels {
            return Err(Fail::too_large(format!(
                "The image is {} pixels, which is larger than the {} Craft AI Manager accepts for this task.",
                pixels,
                plain_pixels(entry.max_pixels)
            )));
        }
        let [_, _, region_w, region_h] = self.region;
        if region_w <= 0 || region_h <= 0 {
            return Err(Fail::bad_request("The region must have a width and a height that are both at least one pixel."));
        }
        if u32::try_from(region_w) != Ok(width) || u32::try_from(region_h) != Ok(height) {
            return Err(Fail::bad_request("The region is a different size from the image the job sends."));
        }
        if !DEPTHS.contains(&self.bit_depth) {
            return Err(Fail::bad_request("The bit depth must be 8 or 16."));
        }
        if !CHANNELS.contains(&self.channels) {
            return Err(Fail::bad_request("The channel count must be 3 or 4."));
        }
        if self.settings.as_ref().is_some_and(|settings| !settings.is_object()) {
            return Err(Fail::bad_request("The job settings must be a JSON object."));
        }
        if let Some(prompt) = self.prompt.as_ref() {
            if prompt.len() > MAX_PROMPT_BYTES {
                return Err(Fail::bad_request("The prompt is longer than the 4 KiB the protocol allows."));
            }
            if prompt.chars().any(|letter| letter.is_control() && !matches!(letter, '\n' | '\t')) {
                return Err(Fail::bad_request("The prompt has control characters that are not allowed."));
            }
        }
        if self.provider.as_deref().is_some_and(|provider| provider != PROVIDER_AUTO) {
            return Err(Fail::bad_request(format!(
                "Craft AI Manager does not know the provider {} yet.",
                quoted(self.provider.as_deref().unwrap_or_default())
            )));
        }
        if let Some(client) = self.client.as_ref()
            && (client.len() > MAX_CLIENT_BYTES || client.chars().any(char::is_control))
        {
            return Err(Fail::bad_request("The client name is not usable."));
        }
        Ok(())
    }
}

/// A job id is opaque, but it arrives from the network and goes into a path, so it is checked
/// before it is used - the same rule the client applies to ids we return.
#[must_use]
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_JOB_ID_BYTES
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        && !id.contains("..")
}

/// A JSON body, ready to go on the wire.
#[must_use]
pub fn json_body(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_else(|_| br#"{"error": "Craft AI Manager could not build its answer."}"#.to_vec())
}

/// `GET /v1/health`.
#[must_use]
pub fn health(app: &str) -> Vec<u8> {
    json_body(json!({ "protocol": PROTOCOL_VERSION, "app": app, "devices": DEVICES }))
}

/// `GET /v1/tasks`: every task the catalog knows, each with the shape of its result, whether a model
/// is behind it, and which model that is.
///
/// Amendment 18: the tasks are listed even when nothing is installed, with `ready: false`, so
/// PhotoCraft can grey the menu out and name the feature rather than hide it. `model` is
/// informational - the client never chooses by it.
#[must_use]
pub fn tasks(offered: &Offered) -> Vec<u8> {
    let listed: Vec<Value> = offered
        .entries()
        .iter()
        .map(|entry| {
            json!({
                "id": entry.task,
                "needs_prompt": entry.needs_prompt,
                "max_pixels": entry.max_pixels,
                "output": entry.output,
                "ready": entry.ready,
                "model": entry.model
            })
        })
        .collect();
    json_body(json!({ "tasks": listed }))
}

/// `POST /v1/jobs` → 202.
#[must_use]
pub fn job_id(id: &str) -> Vec<u8> {
    json_body(json!({ "id": id }))
}

/// `GET /v1/jobs/{id}`.
#[must_use]
pub fn job_state(state: &str, progress: f64, message: &str) -> Vec<u8> {
    json_body(json!({ "state": state, "progress": progress.clamp(0.0, 1.0), "message": message }))
}

/// `GET /v1/jobs/{id}/result`: the provenance amendment 14 wants pinned to the layer later, plus
/// amendment 18's account of what actually ran.
///
/// The five M1 keys keep their names and their order, so an M1-shaped peer reads the same answer it
/// always did (B20). `elapsed_ms` stays the total; `load_ms` and `run_ms` split it up.
/// `fallback_reason` is always there, `null` when the requested backend ran. The rest are optional:
/// a field with nothing to say is left out rather than sent as an empty string.
#[must_use]
pub fn result_provenance(found: &job::Provenance) -> Vec<u8> {
    let [width, height] = found.image_size;
    let mut result = json!({
        "model": found.model,
        "device": found.device,
        "provider": found.provider,
        "elapsed_ms": found.elapsed_ms,
        "image_size": [width, height],
        "kind": found.kind,
        "device_requested": found.device_requested,
        "fallback_reason": found.fallback_reason,
        "load_ms": found.load_ms,
        "run_ms": found.run_ms
    });
    let Some(object) = result.as_object_mut() else {
        return json_body(result);
    };
    for (key, value) in [
        ("model_version", &found.model_version),
        ("model_sha256", &found.model_sha256),
        ("device_name", &found.device_name),
        ("diagnostics", &found.diagnostics),
    ] {
        if let Some(text) = value {
            object.insert(key.to_string(), json!(text));
        }
    }
    json_body(result)
}

#[cfg(test)]
mod tests {
    use super::{
        DEVICE_AUTO, JobRequest, MAX_PIXELS, PROVIDER_AUTO, TASK_FILL, health, is_safe_id, job_state, json_body, plain_pixels, result_provenance, tasks,
    };
    use crate::error::Status;
    use crate::job::Provenance;
    use crate::routing::{OUTPUT_IMAGE, OUTPUT_MASK, Offered, TASK_MASK};
    use crate::testkit::Scratch;
    use serde_json::{Value, json};

    fn job() -> JobRequest {
        serde_json::from_value(json!({
            "task": TASK_FILL,
            "image_size": [4, 2],
            "region": [10, 20, 4, 2],
            "bit_depth": 8,
            "channels": 3,
            "prompt": null,
            "settings": {},
            "provider": "auto",
            "client": "photocraft/0.9.0"
        }))
        .expect("the client shape parses")
    }

    /// A start with the fake backend: both tasks ready.
    fn ready() -> Offered {
        Offered::fake()
    }

    /// A start with an empty model store: both tasks listed, neither ready.
    fn empty_store(who: &str) -> Offered {
        let scratch = Scratch::new(who);
        Offered::from_store(Some(&scratch.path))
    }

    fn value(body: &[u8]) -> Value {
        serde_json::from_slice(body).expect("the body is JSON")
    }

    #[test]
    fn a_well_formed_job_passes_the_checks() {
        job().validate(&ready()).expect("this job can run");
    }

    #[test]
    fn without_a_backend_the_answer_is_the_sentence_amendment_11_spells_out() {
        let fail = job().validate(&empty_store("protocol-empty")).expect_err("no model is installed");
        assert_eq!(fail.status, Status::BadRequest);
        assert_eq!(fail.sentence, "No model is installed for fill_masked_area yet");
    }

    #[test]
    fn a_mask_job_for_a_missing_model_names_the_feature() {
        // Amendment 18: a plain sentence about the feature, not about the model id.
        let mut request = job();
        request.task = TASK_MASK.to_string();
        let fail = request.validate(&empty_store("protocol-mask")).expect_err("the mask model is not installed");
        assert_eq!(fail.status, Status::BadRequest);
        assert_eq!(fail.sentence, "The Select Subject model isn't installed.");
    }

    #[test]
    fn a_mask_job_is_accepted_once_the_model_is_ready() {
        let mut request = job();
        request.task = TASK_MASK.to_string();
        request.validate(&ready()).expect("the fake backend serves the mask task too");
    }

    #[test]
    fn an_unknown_task_is_named_in_the_answer() {
        let mut request = job();
        request.task = "select_subject".to_string();
        let fail = request.validate(&ready()).expect_err("the catalog has two tasks, and this is not one of them");
        assert!(fail.sentence.contains("select_subject"), "{fail}");
    }

    #[test]
    fn the_region_must_be_the_size_of_the_image() {
        let mut request = job();
        request.region = [10, 20, 5, 2];
        assert_eq!(request.validate(&ready()).expect_err("the sizes disagree").status, Status::BadRequest);
        let mut request = job();
        request.region = [10, 20, 0, 0];
        assert!(request.validate(&ready()).is_err());
    }

    #[test]
    fn depth_channels_and_an_empty_image_are_all_refused() {
        for broken in [
            {
                let mut request = job();
                request.bit_depth = 32;
                request
            },
            {
                let mut request = job();
                request.channels = 2;
                request
            },
            {
                let mut request = job();
                request.image_size = [0, 2];
                request
            },
            {
                let mut request = job();
                request.settings = Some(json!([]));
                request
            },
            {
                let mut request = job();
                request.provider = Some("openai".to_string());
                request
            },
            {
                let mut request = job();
                request.prompt = Some("x".repeat(5000));
                request
            },
        ] {
            assert_eq!(broken.validate(&ready()).expect_err("each of these is refused").status, Status::BadRequest, "{broken:?}");
        }
    }

    #[test]
    fn over_the_pixel_cap_is_413() {
        let mut request = job();
        request.image_size = [MAX_PIXELS as u32 / 2, 4];
        request.region = [0, 0, request.image_size[0] as i32, request.image_size[1] as i32];
        assert_eq!(request.validate(&ready()).expect_err("over the cap").status, Status::TooLarge);
    }

    #[test]
    fn the_mask_task_is_capped_by_its_own_entry() {
        // Amendment 18: PhotoCraft downscales first, so a mask job over the task's cap means the
        // manager would have to hold more than the model's manifest allows.
        let mut request = job();
        request.task = TASK_MASK.to_string();
        request.image_size = [1024, 1025];
        request.region = [0, 0, 1024, 1025];
        let fail = request.validate(&ready()).expect_err("one pixel over the mask cap");
        assert_eq!(fail.status, Status::TooLarge, "{fail}");
        assert_eq!(
            fail.sentence, "The image is 1049600 pixels, which is larger than the 1 megapixel Craft AI Manager accepts for this task.",
            "the sentence names the task's own cap, in plain words, not Fill's"
        );
        // The same size is fine for Fill, whose cap is 64 megapixels.
        let mut request = job();
        request.image_size = [1024, 1025];
        request.region = [0, 0, 1024, 1025];
        request.validate(&ready()).expect("well inside Fill's cap");
    }

    #[test]
    fn the_pixel_cap_sentence_reads_in_plain_words() {
        // "over the 1 megapixels" is not how anyone speaks: one megapixel is singular, a whole
        // number of them is plural, a part of one keeps its tenth, and below a megapixel the
        // sentence counts pixels.
        assert_eq!(plain_pixels(1_048_576), "1 megapixel", "the mask task's cap");
        assert_eq!(plain_pixels(64_000_000), "64 megapixels", "Fill's cap");
        assert_eq!(plain_pixels(1_500_000), "1.5 megapixels", "a cap that is not a whole megapixel");
        assert_eq!(plain_pixels(640_000), "640000 pixels", "under a megapixel, pixels");
        // Fill's refusal reads the same way, in its plural form.
        let mut request = job();
        request.image_size = [MAX_PIXELS as u32 / 2, 4];
        request.region = [0, 0, request.image_size[0] as i32, request.image_size[1] as i32];
        let fail = request.validate(&ready()).expect_err("over Fill's cap");
        assert_eq!(fail.status, Status::TooLarge, "{fail}");
        assert!(fail.sentence.contains("larger than the 64 megapixels Craft AI Manager accepts"), "{fail}");
    }

    #[test]
    fn the_answers_are_the_shapes_amendment_11_defines() {
        let health = value(&health("caim/0.1.0"));
        assert_eq!(health.get("protocol").and_then(Value::as_u64), Some(1));
        assert_eq!(health.get("app").and_then(Value::as_str), Some("caim/0.1.0"));
        assert_eq!(health.get("devices").and_then(Value::as_array).map(Vec::len), Some(1));

        let listed = value(&tasks(&ready())).get("tasks").and_then(Value::as_array).expect("a task list").clone();
        assert_eq!(listed.len(), 2, "fill and mask: {listed:?}");
        let fill = listed.first().cloned().expect("fill is first");
        assert_eq!(fill.get("id").and_then(Value::as_str), Some(TASK_FILL));
        assert_eq!(fill.get("max_pixels").and_then(Value::as_u64), Some(MAX_PIXELS), "Fill's cap has not moved (B20)");
        assert_eq!(fill.get("output").and_then(Value::as_str), Some(OUTPUT_IMAGE));
        assert_eq!(fill.get("ready").and_then(Value::as_bool), Some(true));
        let mask = listed.get(1).cloned().expect("mask is second");
        assert_eq!(mask.get("id").and_then(Value::as_str), Some(TASK_MASK));
        assert_eq!(mask.get("needs_prompt").and_then(Value::as_bool), Some(false));
        assert_eq!(mask.get("output").and_then(Value::as_str), Some(OUTPUT_MASK));
        assert_eq!(mask.get("model").and_then(Value::as_str), Some("fake-mask"));
        // An empty store still lists both tasks, so PhotoCraft can grey them out.
        let none = value(&tasks(&empty_store("protocol-shapes"))).get("tasks").and_then(Value::as_array).expect("a task list").clone();
        assert_eq!(none.len(), 2, "{none:?}");
        assert!(none.iter().all(|task| task.get("ready").and_then(Value::as_bool) == Some(false)), "{none:?}");

        let state = value(&job_state("running", 1.4, "Craft AI Manager is working"));
        assert_eq!(state.get("state").and_then(Value::as_str), Some("running"));
        assert_eq!(state.get("progress").and_then(Value::as_f64), Some(1.0), "progress is clamped");

        let result = value(&result_provenance(&fill_provenance()));
        assert_eq!(result.get("model").and_then(Value::as_str), Some("fake-fill"));
        assert_eq!(result.get("elapsed_ms").and_then(Value::as_u64), Some(12));
        assert_eq!(result.get("image_size").and_then(Value::as_array).map(Vec::len), Some(2));
        // Amendment 18: the M1 keys are still there, and the new ones say what ran.
        assert_eq!(result.get("kind").and_then(Value::as_str), Some(OUTPUT_IMAGE));
        assert_eq!(result.get("device_requested").and_then(Value::as_str), Some(DEVICE_AUTO));
        assert_eq!(result.get("device_name").and_then(Value::as_str), Some("CPU"));
        assert!(result.get("fallback_reason").is_some_and(Value::is_null), "always present, null when nothing fell back");
        assert_eq!(result.get("load_ms").and_then(Value::as_u64), Some(3));
        assert_eq!(result.get("run_ms").and_then(Value::as_u64), Some(9));
        assert_eq!(result.get("model_version").and_then(Value::as_str), Some("1"));
        assert_eq!(result.get("model_sha256").and_then(Value::as_str), Some("ab12"));
        assert!(result.get("diagnostics").is_none(), "nothing to log, so the key is left out");
        assert_eq!(result.get("provider").and_then(Value::as_str), Some(PROVIDER_AUTO), "the M1 keys keep their meaning");
    }

    /// A provenance with everything a real runner reports.
    fn fill_provenance() -> Provenance {
        Provenance {
            model: "fake-fill".to_string(),
            device: "cpu".to_string(),
            provider: PROVIDER_AUTO.to_string(),
            elapsed_ms: 12,
            image_size: [4, 2],
            kind: OUTPUT_IMAGE,
            model_version: Some("1".to_string()),
            model_sha256: Some("ab12".to_string()),
            device_requested: DEVICE_AUTO.to_string(),
            device_name: Some("CPU".to_string()),
            fallback_reason: None,
            load_ms: 3,
            run_ms: 9,
            diagnostics: None,
        }
    }

    #[test]
    fn a_mask_result_says_so_and_leaves_out_what_it_does_not_know() {
        let mut found = fill_provenance();
        found.kind = OUTPUT_MASK;
        found.model = "birefnet-lite".to_string();
        found.model_version = None;
        found.model_sha256 = None;
        found.device_name = None;
        found.fallback_reason = Some("insufficient_memory".to_string());
        found.diagnostics = Some("the runtime said something long".to_string());
        let result = value(&result_provenance(&found));
        assert_eq!(result.get("kind").and_then(Value::as_str), Some(OUTPUT_MASK));
        assert_eq!(result.get("fallback_reason").and_then(Value::as_str), Some("insufficient_memory"));
        assert_eq!(result.get("diagnostics").and_then(Value::as_str), Some("the runtime said something long"));
        assert!(result.get("model_version").is_none(), "a field with nothing to say is left out");
        assert!(result.get("device_name").is_none(), "{result:?}");
        assert_eq!(result.get("device").and_then(Value::as_str), Some("cpu"), "device still names the backend that ran");
    }

    #[test]
    fn job_ids_are_checked_the_way_the_client_checks_them() {
        assert!(is_safe_id("job-12"));
        assert!(is_safe_id("a:b_c.d"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("../etc"));
        assert!(!is_safe_id("job 1"));
        assert!(!is_safe_id(&"x".repeat(600)));
    }

    #[test]
    fn a_json_body_never_fails_to_produce_bytes() {
        assert!(!json_body(json!({ "ok": true })).is_empty());
    }
}
