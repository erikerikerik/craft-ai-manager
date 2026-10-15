// The wire contract of SPEC amendment 11, tested over real TCP against a server on a temp endpoint
// file: the token, the Host check, every endpoint, the error sentences, the caps, and the job
// lifecycle.

mod support;

use craft_ai_manager::{catalog, protocol, store};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use support::{BOUNDARY, decode_png, fill_job, job_json, job_parts, multipart, png_gray, png_rgb, sample_image, sample_mask};

/// The task MG-01 knows, as a value the tests compare against.
const TASK: &str = "fill_masked_area";

/// The mean colour of the eight-pixel sample image with its first pixel masked: the seven unmasked
/// pixels sum to 470, 355 and 350, and 470/7, 355/7, 350/7 round to 67, 51 and 50.
const MEAN: [u16; 3] = [67, 51, 50];

#[test]
fn a_request_without_the_token_is_refused() {
    let server = support::TestServer::fake("token");
    let no_token = format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", server.port());
    let wrong =
        format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", server.port(), "0".repeat(64));
    let prefix =
        format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", server.port(), &server.token()[..8]);
    let wrong_scheme =
        format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: {}\r\nConnection: close\r\n\r\n", server.port(), server.token());
    for (name, request) in
        [("no Authorization header", &no_token), ("another token", &wrong), ("the first half of the token", &prefix), ("no Bearer prefix", &wrong_scheme)]
    {
        let reply = server.send_raw(request.as_bytes());
        assert_eq!(reply.status, 401, "{name} is refused: {}", reply.text());
        assert_eq!(reply.header("content-type"), Some("application/json"), "{name} answers JSON");
        assert!(!reply.sentence().is_empty(), "{name} says why");
    }
    // The token comes first, so a request that gets both wrong is a 401, not a 400.
    let both = "GET /v1/health HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";
    assert_eq!(server.send_raw(both.as_bytes()).status, 401, "the token is checked before the Host header");
    assert_eq!(server.get("/v1/health").status, 200, "the right token still works");
}

#[test]
fn a_host_that_is_not_our_loopback_port_is_refused() {
    let server = support::TestServer::fake("host");
    for host in ["localhost:8080", "example.com", "127.0.0.1:1", "127.0.0.1", "0.0.0.0:0", ""] {
        let request = format!("GET /v1/health HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", server.token());
        let reply = server.send_raw(request.as_bytes());
        assert_eq!(reply.status, 400, "Host: {host} is refused: {}", reply.text());
    }
    let right = server.get("/v1/health");
    assert_eq!(right.status, 200, "the loopback Host is accepted");
    assert_eq!(right.json().get("error"), None, "an accepted Host has no error body");
}

#[test]
fn the_happy_path_fills_the_masked_area() {
    let server = support::TestServer::fake("happy");
    let health = server.get("/v1/health");
    assert_eq!(health.status, 200);
    let value = health.json();
    assert_eq!(value.get("protocol").and_then(Value::as_u64), Some(1), "protocol 1");
    assert_eq!(value.get("app").and_then(Value::as_str), Some("caim/0.1.0"), "the app names itself with its version");
    assert_eq!(value.get("devices"), Some(&json!(["cpu"])), "MG-01 runs on the CPU");
    assert!(health.has_header("content-length") && health.has_header("connection"), "every reply closes and says its length");

    let tasks = server.get("/v1/tasks").json();
    let list = tasks.get("tasks").and_then(Value::as_array).expect("a task list");
    assert_eq!(list.len(), 1, "the fake backend offers one task");
    assert_eq!(list.first().and_then(|task| task.get("id")).and_then(Value::as_str), Some(TASK));
    assert_eq!(list.first().and_then(|task| task.get("needs_prompt")).and_then(Value::as_bool), Some(false));
    assert_eq!(list.first().and_then(|task| task.get("max_pixels")).and_then(Value::as_u64), Some(64_000_000));

    let image = sample_image();
    let id = server.submit_id(&fill_job(4, 2, 8, 3), &image, &sample_mask());
    let done = server.wait_for_state(&id, "done");
    assert_eq!(done.get("progress").and_then(Value::as_f64), Some(1.0), "progress ends at one");
    assert!(!done.get("message").and_then(Value::as_str).unwrap_or_default().is_empty(), "the poll says something");

    let result = server.get(&format!("/v1/jobs/{id}/result"));
    assert_eq!(result.status, 200);
    let provenance = result.json();
    assert_eq!(provenance.get("model").and_then(Value::as_str), Some("fake-fill"), "a fake result says so");
    assert_eq!(provenance.get("device").and_then(Value::as_str), Some("cpu"));
    assert_eq!(provenance.get("provider").and_then(Value::as_str), Some("local"));
    assert_eq!(provenance.get("image_size"), Some(&json!([4, 2])));
    assert!(provenance.get("elapsed_ms").and_then(Value::as_u64).is_some(), "the elapsed time is reported");

    let png = server.get(&format!("/v1/jobs/{id}/result.png"));
    assert_eq!(png.status, 200);
    assert_eq!(png.header("content-type"), Some("image/png"));
    let out = decode_png(&png.body);
    let input = decode_png(&image);
    assert_eq!((out.width, out.height), (4, 2), "the same size");
    assert_eq!(out.channels, input.channels, "the same channels");
    assert_eq!(out.bit_depth, input.bit_depth, "the same depth");
    for pixel in 1..8 {
        for channel in 0..3 {
            assert_eq!(out.sample(pixel, channel), input.sample(pixel, channel), "pixel {pixel} is outside the mask");
        }
    }
    for (channel, expected) in MEAN.iter().enumerate() {
        assert_eq!(out.sample(0, channel), *expected, "the masked pixel takes the mean colour");
    }
    assert_eq!(server.get(&format!("/v1/jobs/{id}/result.png")).status, 404, "a result is kept until it has been read");
    assert_eq!(server.poll(&id).get("state").and_then(Value::as_str), Some("done"), "the job record outlives its result");
}

#[test]
fn a_sixteen_bit_rgba_image_comes_back_in_the_same_shape() {
    let server = support::TestServer::fake("rgba");
    let samples: Vec<u16> = (0u16..16).map(|index| index.saturating_mul(4096)).collect();
    let mut raw = Vec::new();
    for sample in &samples {
        raw.extend_from_slice(&sample.to_be_bytes());
    }
    let mut image = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut image, 4, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().expect("the header");
        writer.write_image_data(&raw).expect("the pixels");
        writer.finish().expect("the end");
    }
    let id = server.submit_id(&fill_job(4, 1, 16, 4), &image, &png_gray(4, 1, &[0, 255, 128, 0]));
    server.wait_for_state(&id, "done");
    let png = server.get(&format!("/v1/jobs/{id}/result.png"));
    assert_eq!(png.status, 200);
    let out = decode_png(&png.body);
    assert_eq!((out.width, out.height, out.channels, out.bit_depth), (4, 1, 4, 16), "size, channels and depth survive");
    assert_eq!(out.sample(0, 0), samples[0], "the unmasked pixel is untouched");
    assert_eq!(out.sample(3, 0), samples[12], "the other unmasked pixel is untouched");
    assert_eq!(out.sample(0, 3), samples[3], "the alpha of an unmasked pixel is untouched");
    assert_eq!(out.sample(1, 3), 65535, "a filled pixel is opaque");
    // The mean of the two unmasked pixels is 24576. Amendment 16: CAIM does not blend by coverage
    // (PhotoCraft does), so the fully masked pixel and the half-covered one both take it whole.
    let mean = (u32::from(samples[0]) + u32::from(samples[12])) / 2;
    assert_eq!(u16::try_from(mean).expect("a 16-bit mean"), out.sample(1, 0), "a fully masked pixel takes the mean");
    assert_eq!(u16::try_from(mean).expect("a 16-bit mean"), out.sample(2, 0), "and so does a half-covered one, at full strength");
    assert_eq!(out.sample(2, 3), 65535, "and it is opaque too");
}

#[test]
fn a_body_that_is_not_the_three_parts_is_refused() {
    let server = support::TestServer::fake("multipart");
    let (image, mask, job) = (sample_image(), sample_mask(), fill_job(4, 2, 8, 3));
    let parts = job_parts(&job, &image, &mask);
    let form = format!("multipart/form-data; boundary={BOUNDARY}");
    let good = multipart(BOUNDARY, &parts);
    let out_of_order = multipart(BOUNDARY, &[parts[1].clone(), parts[0].clone(), parts[2].clone()]);
    let missing_mask = multipart(BOUNDARY, &[parts[0].clone(), parts[1].clone()]);
    let mut truncated = good.clone();
    truncated.truncate(truncated.len().saturating_sub(4));
    let cases: Vec<(&str, Option<&str>, Vec<u8>)> = vec![
        ("a JSON body", Some("application/json"), job.as_bytes().to_vec()),
        ("no boundary parameter", Some("multipart/form-data"), good.clone()),
        ("a boundary that never appears", Some("multipart/form-data; boundary=absent"), good.clone()),
        ("the parts in the wrong order", Some(form.as_str()), out_of_order),
        ("a missing mask part", Some(form.as_str()), missing_mask),
        ("no closing boundary", Some(form.as_str()), truncated),
        ("noise", Some(form.as_str()), b"not a body at all".to_vec()),
    ];
    for (name, content_type, body) in cases {
        let reply = server.request("POST", "/v1/jobs", content_type, &body);
        assert_eq!(reply.status, 400, "{name} is refused: {}", reply.text());
        assert!(!reply.sentence().is_empty(), "{name} says why");
    }
    assert_eq!(server.submit(&job, &image, &mask).status, 202, "the good body still works afterwards");
}

#[test]
fn a_part_that_disagrees_with_the_job_is_refused() {
    let server = support::TestServer::fake("mismatch");
    let (image, mask) = (sample_image(), sample_mask());
    let small_image = png_rgb(2, 2, 8, &[7; 12]);
    let deep_image = png_rgb(4, 2, 16, &[700; 24]);
    let small_mask = png_gray(2, 2, &[255, 0, 0, 0]);
    let cases: Vec<(&str, String, Vec<u8>, Vec<u8>)> = vec![
        ("an image of another size", fill_job(4, 2, 8, 3), small_image, mask.clone()),
        ("an image of another depth", fill_job(4, 2, 8, 3), deep_image, mask.clone()),
        ("a mask of another size", fill_job(4, 2, 8, 3), image.clone(), small_mask),
        ("a job that asks for 16 bits", fill_job(4, 2, 16, 3), image.clone(), mask.clone()),
        ("a region that is not the image", job_json(TASK, 4, 2, [0, 0, 3, 2], 8, 3), image.clone(), mask.clone()),
        ("a task this manager does not know", job_json("upscale", 4, 2, [0, 0, 4, 2], 8, 3), image.clone(), mask.clone()),
        ("a channel count the protocol forbids", fill_job(4, 2, 8, 2), image.clone(), mask.clone()),
    ];
    for (name, job, image, mask) in cases {
        let reply = server.submit(&job, &image, &mask);
        assert_eq!(reply.status, 400, "{name} is refused: {}", reply.text());
        assert!(!reply.sentence().is_empty(), "{name} says why");
    }
    assert_eq!(server.submit(&fill_job(4, 2, 8, 3), &image, &mask).status, 202, "the same parts are fine on their own");
}

#[test]
fn a_job_this_manager_does_not_know_is_404() {
    let server = support::TestServer::fake("unknown");
    for path in ["/v1/jobs/job-999", "/v1/jobs/job-999/result", "/v1/jobs/job-999/result.png", "/v1/nope", "/v1/jobs/../secret", "/"] {
        let reply = server.get(path);
        assert_eq!(reply.status, 404, "{path} is not there: {}", reply.text());
    }
    assert_eq!(server.delete("/v1/jobs/job-999").status, 404, "a DELETE needs a real job");
    assert_eq!(server.get("/v1/health").status, 200, "the server is still serving");
}

#[test]
fn a_result_that_is_not_ready_yet_is_409() {
    let server = support::TestServer::with("not-ready", true, Duration::from_millis(150));
    let id = server.submit_id(&fill_job(4, 2, 8, 3), &sample_image(), &sample_mask());
    for path in [format!("/v1/jobs/{id}/result"), format!("/v1/jobs/{id}/result.png")] {
        let reply = server.get(&path);
        assert_eq!(reply.status, 409, "{path} is not ready: {}", reply.text());
        assert!(!reply.sentence().is_empty(), "{path} says why");
    }
    server.wait_for_state(&id, "done");
    assert_eq!(server.get(&format!("/v1/jobs/{id}/result")).status, 200, "and once it is done it answers");
}

#[test]
fn a_body_over_the_cap_is_refused_before_it_is_read() {
    let server = support::TestServer::fake("cap");
    let request = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: 1073741825\r\nConnection: close\r\n\r\n",
        server.port(),
        server.token()
    );
    let reply = server.send_raw(request.as_bytes());
    assert_eq!(reply.status, 413, "one byte over a GiB is refused: {}", reply.text());
    assert!(reply.sentence().contains("GiB"), "the sentence names the cap: {}", reply.sentence());
    assert_eq!(server.get("/v1/health").status, 200, "the server is still serving");
}

#[test]
fn the_ninth_queued_job_is_told_the_manager_is_busy() {
    let server = support::TestServer::with("queue", true, Duration::from_millis(150));
    let (image, mask, job) = (sample_image(), sample_mask(), fill_job(4, 2, 8, 3));
    for _ in 0..8 {
        server.submit_id(&job, &image, &mask);
    }
    let ninth = server.submit(&job, &image, &mask);
    assert_eq!(ninth.status, 503, "the queue holds eight: {}", ninth.text());
    assert!(ninth.sentence().contains("8 jobs"), "the sentence says how many: {}", ninth.sentence());
    server.wait_for_state("job-1", "done");
    assert_eq!(server.submit(&job, &image, &mask).status, 202, "a finished job frees a place");
}

#[test]
fn a_queued_job_is_cancelled_before_it_starts() {
    let server = support::TestServer::with("cancel-queued", true, Duration::from_millis(150));
    let (image, mask, job) = (sample_image(), sample_mask(), fill_job(4, 2, 8, 3));
    let first = server.submit_id(&job, &image, &mask);
    let second = server.submit_id(&job, &image, &mask);
    let deleted = server.delete(&format!("/v1/jobs/{second}"));
    assert_eq!(deleted.status, 204, "the DELETE answers with nothing: {}", deleted.text());
    assert!(deleted.body.is_empty(), "a 204 has no body");
    assert_eq!(server.wait_for_state(&second, "cancelled").get("state").and_then(Value::as_str), Some("cancelled"));
    assert_eq!(server.get(&format!("/v1/jobs/{second}/result")).status, 409, "a cancelled job has no result");
    server.wait_for_state(&first, "done");
    assert_eq!(server.poll(&second).get("state").and_then(Value::as_str), Some("cancelled"), "a cancelled job stays cancelled");
}

#[test]
fn a_running_job_is_cancelled_between_its_steps() {
    let server = support::TestServer::with("cancel-running", true, Duration::from_millis(150));
    let id = server.submit_id(&fill_job(4, 2, 8, 3), &sample_image(), &sample_mask());
    server.wait_for_state(&id, "running");
    assert_eq!(server.delete(&format!("/v1/jobs/{id}")).status, 204);
    assert_eq!(server.wait_for_state(&id, "cancelled").get("state").and_then(Value::as_str), Some("cancelled"));
    assert_eq!(server.get(&format!("/v1/jobs/{id}/result.png")).status, 409, "a cancelled job has no image");
    assert_eq!(server.delete(&format!("/v1/jobs/{id}")).status, 204, "cancelling twice is not an error");
}

#[test]
fn two_jobs_finish_in_the_order_they_arrived() {
    let server = support::TestServer::with("order", true, Duration::from_millis(150));
    let (image, mask, job) = (sample_image(), sample_mask(), fill_job(4, 2, 8, 3));
    let first = server.submit_id(&job, &image, &mask);
    let second = server.submit_id(&job, &image, &mask);
    assert_eq!(first, "job-1", "the ids are opaque but ordered");
    assert_eq!(second, "job-2");
    server.wait_for_state(&first, "done");
    assert_ne!(server.poll(&second).get("state").and_then(Value::as_str), Some("done"), "one worker, so the second job waits");
    server.wait_for_state(&second, "done");
}

#[test]
fn a_broken_request_gets_a_400_and_the_server_keeps_serving() {
    let server = support::TestServer::fake("broken");
    let long_header = format!(
        "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nX-Long: {}\r\nConnection: close\r\n\r\n",
        server.port(),
        server.token(),
        "a".repeat(8 * 1024 + 64)
    );
    let no_length = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: multipart/form-data; boundary={BOUNDARY}\r\nConnection: close\r\n\r\n",
        server.port(),
        server.token()
    );
    let chunked = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        server.port(),
        server.token()
    );
    let cases: Vec<(&str, String)> = vec![
        ("a request line that is not one", "not a request HTTP/1.1\r\n\r\n".to_string()),
        ("a header over 8 KiB", long_header),
        ("a POST with no Content-Length", no_length),
        ("a chunked body", chunked),
    ];
    for (name, request) in cases {
        let reply = server.send_raw(request.as_bytes());
        assert_eq!(reply.status, 400, "{name} is refused: {}", reply.text());
        assert!(reply.has_header("content-length") && reply.has_header("connection"), "{name} still answers properly");
        assert_eq!(server.get("/v1/health").status, 200, "{name} did not break the server");
    }
}

#[test]
fn without_the_fake_backend_there_is_nothing_to_run() {
    let server = support::TestServer::no_model("no-backend");
    let tasks = server.get("/v1/tasks").json();
    assert_eq!(tasks.get("tasks"), Some(&json!([])), "no tasks, so PhotoCraft greys the menu out");
    let reply = server.submit(&fill_job(4, 2, 8, 3), &sample_image(), &sample_mask());
    assert_eq!(reply.status, 400, "a job has nowhere to go: {}", reply.text());
    assert_eq!(reply.sentence(), "No model is installed for fill_masked_area yet", "the sentence amendment 11 spells out");
    assert_eq!(server.get("/v1/health").status, 200, "health does not need a model");
}

/// Put the LaMa entry of the real catalog into `root` with the size the catalog promises but
/// none of the bytes: a sparse file is instant, and MG-02's "installed" check only looks at the
/// size and the record, so this is enough to decide what the task list answers (MG-04 item 2).
fn install_lama(root: &Path, acknowledge: bool) {
    let model = catalog::load().expect("the catalog is part of the binary").into_iter().find(|model| model.id == "lama").expect("lama is in the catalog");
    std::fs::create_dir_all(store::model_folder(root, &model)).expect("the model folder");
    let file = std::fs::File::create(store::model_file(root, &model)).expect("the model file");
    file.set_len(model.size).expect("a file the size the catalog promises");
    store::write_verified(root, &model, &model.sha256, model.size).expect("verified.json");
    if acknowledge {
        store::write_acknowledgement(root, &model).expect("acknowledged.json");
    }
}

#[test]
fn a_model_that_is_installed_and_acknowledged_is_offered_without_loading_it() {
    let root: PathBuf = support::empty_store("lama-offered");
    install_lama(&root, true);
    let server = support::TestServer::lama_store("lama-offered", &root);
    let tasks = server.get("/v1/tasks").json();
    assert_eq!(
        tasks.get("tasks"),
        Some(&json!([{ "id": TASK, "needs_prompt": false, "max_pixels": protocol::MAX_PIXELS }])),
        "LaMa is installed, so the task is on the menu"
    );
    assert_eq!(server.get("/v1/health").status, 200, "the session is still lazy, so the start stayed fast");
}

#[test]
fn a_model_that_is_installed_but_not_acknowledged_is_not_offered() {
    let root: PathBuf = support::empty_store("lama-unacked");
    install_lama(&root, false);
    let server = support::TestServer::lama_store("lama-unacked", &root);
    assert_eq!(server.get("/v1/tasks").json().get("tasks"), Some(&json!([])), "the override note has to be accepted first");
    let reply = server.submit(&fill_job(4, 2, 8, 3), &sample_image(), &sample_mask());
    assert_eq!(reply.status, 400, "and a job is still refused: {}", reply.text());
}
