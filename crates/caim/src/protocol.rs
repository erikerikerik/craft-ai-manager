//! The JSON of amendment 11, from the manager's side: what a job asks for, what each endpoint
//! answers, and the checks that turn a wrong job into a plain sentence rather than a wrong answer.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Fail, Result, quoted};
use crate::image::{CHANNELS, DEPTHS};

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
/// The devices this manager can run on. MG-01 has one.
pub const DEVICES: [&str; 1] = ["cpu"];

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
    /// Without `--fake-backend` there is no model at all, which is the one case amendment 11
    /// spells out in words: "No model is installed for fill_masked_area yet".
    pub fn validate(&self, fake_backend: bool) -> Result<()> {
        if self.task != TASK_FILL {
            return Err(Fail::bad_request(format!("Craft AI Manager does not know the task {} yet.", quoted(&self.task))));
        }
        if !fake_backend {
            return Err(Fail::bad_request(format!("No model is installed for {TASK_FILL} yet")));
        }
        let [width, height] = self.image_size;
        if width == 0 || height == 0 {
            return Err(Fail::bad_request("The image size must be at least one pixel wide and one pixel high."));
        }
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        if pixels > MAX_PIXELS {
            return Err(Fail::too_large(format!("The image is {} pixels, over the {} megapixels Craft AI Manager accepts.", pixels, MAX_PIXELS / 1_000_000)));
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

/// `GET /v1/tasks`: with no backend there is nothing to offer, which is what tells PhotoCraft to
/// grey the menu out.
#[must_use]
pub fn tasks(fake_backend: bool) -> Vec<u8> {
    if fake_backend {
        return json_body(json!({ "tasks": [{ "id": TASK_FILL, "needs_prompt": false, "max_pixels": MAX_PIXELS }] }));
    }
    json_body(json!({ "tasks": [] }))
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

/// `GET /v1/jobs/{id}/result`: the provenance amendment 14 wants pinned to the layer later.
#[must_use]
pub fn result_provenance(model: &str, device: &str, provider: &str, elapsed_ms: u64, image_size: [u32; 2]) -> Vec<u8> {
    let [width, height] = image_size;
    json_body(json!({ "model": model, "device": device, "provider": provider, "elapsed_ms": elapsed_ms, "image_size": [width, height] }))
}

#[cfg(test)]
mod tests {
    use super::{JobRequest, MAX_PIXELS, TASK_FILL, health, is_safe_id, job_state, json_body, result_provenance, tasks};
    use crate::error::Status;
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

    fn value(body: &[u8]) -> Value {
        serde_json::from_slice(body).expect("the body is JSON")
    }

    #[test]
    fn a_well_formed_job_passes_the_checks() {
        job().validate(true).expect("this job can run");
    }

    #[test]
    fn without_the_fake_backend_the_answer_is_the_sentence_amendment_11_spells_out() {
        let fail = job().validate(false).expect_err("no model is installed");
        assert_eq!(fail.status, Status::BadRequest);
        assert_eq!(fail.sentence, "No model is installed for fill_masked_area yet");
    }

    #[test]
    fn an_unknown_task_is_named_in_the_answer() {
        let mut request = job();
        request.task = "select_subject".to_string();
        let fail = request.validate(true).expect_err("MG-01 knows one task");
        assert!(fail.sentence.contains("select_subject"), "{fail}");
    }

    #[test]
    fn the_region_must_be_the_size_of_the_image() {
        let mut request = job();
        request.region = [10, 20, 5, 2];
        assert_eq!(request.validate(true).expect_err("the sizes disagree").status, Status::BadRequest);
        let mut request = job();
        request.region = [10, 20, 0, 0];
        assert!(request.validate(true).is_err());
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
            assert_eq!(broken.validate(true).expect_err("each of these is refused").status, Status::BadRequest, "{broken:?}");
        }
    }

    #[test]
    fn over_the_pixel_cap_is_413() {
        let mut request = job();
        request.image_size = [MAX_PIXELS as u32 / 2, 4];
        request.region = [0, 0, request.image_size[0] as i32, request.image_size[1] as i32];
        assert_eq!(request.validate(true).expect_err("over the cap").status, Status::TooLarge);
    }

    #[test]
    fn the_answers_are_the_shapes_amendment_11_defines() {
        let health = value(&health("caim/0.1.0"));
        assert_eq!(health.get("protocol").and_then(Value::as_u64), Some(1));
        assert_eq!(health.get("app").and_then(Value::as_str), Some("caim/0.1.0"));
        assert_eq!(health.get("devices").and_then(Value::as_array).map(Vec::len), Some(1));

        let with = value(&tasks(true));
        let listed = with.get("tasks").and_then(Value::as_array).expect("a task list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed.first().and_then(|task| task.get("id")).and_then(Value::as_str), Some(TASK_FILL));
        assert_eq!(listed.first().and_then(|task| task.get("max_pixels")).and_then(Value::as_u64), Some(MAX_PIXELS));
        assert_eq!(value(&tasks(false)).get("tasks").and_then(Value::as_array).map(Vec::len), Some(0));

        let state = value(&job_state("running", 1.4, "Craft AI Manager is working"));
        assert_eq!(state.get("state").and_then(Value::as_str), Some("running"));
        assert_eq!(state.get("progress").and_then(Value::as_f64), Some(1.0), "progress is clamped");

        let result = value(&result_provenance("fake-fill", "cpu", "local", 12, [4, 2]));
        assert_eq!(result.get("model").and_then(Value::as_str), Some("fake-fill"));
        assert_eq!(result.get("elapsed_ms").and_then(Value::as_u64), Some(12));
        assert_eq!(result.get("image_size").and_then(Value::as_array).map(Vec::len), Some(2));
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
