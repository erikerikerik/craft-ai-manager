// The activity log: one JSON line per finished job, in `activity.jsonl` beside the endpoint file
// (SPEC v1.1 amendment 18, "Activity log"). The log is a convenience and never a dependency, so a
// write that cannot happen is a sentence on stderr and nothing that changes a job's answer.

mod support;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The log beside a running server's endpoint file.
fn log_path(endpoint: &Path) -> PathBuf {
    endpoint.parent().expect("the endpoint file has a folder").join("activity.jsonl")
}

/// The log as parsed lines.
fn lines(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("the log is readable");
    text.lines().map(|line| serde_json::from_str(line).expect("the log is one JSON object per line")).collect()
}

#[test]
fn a_finished_job_leaves_one_line_with_the_fields_that_exist_today() {
    let server = support::TestServer::fake("activity-done");
    let id = server.submit_id(&support::fill_job(4, 2, 8, 3), &support::sample_image(), &support::sample_mask());
    server.wait_for_state(&id, "done");
    let written = lines(&log_path(&server.endpoint));
    assert_eq!(written.len(), 1, "one line for one finished job");
    let line = written.first().expect("a line");
    assert_eq!(line.get("state").and_then(Value::as_str), Some("done"));
    assert_eq!(line.get("task").and_then(Value::as_str), Some(support::TASK), "the task the job asked for");
    assert_eq!(line.get("model").and_then(Value::as_str), Some("fake-fill"), "the model the result named");
    assert_eq!(line.get("device").and_then(Value::as_str), Some("cpu"), "the device the result named");
    assert!(line.get("elapsed_ms").and_then(Value::as_u64).is_some(), "how long the fill took is there");
    let time = line.get("time").and_then(Value::as_str).expect("a time");
    assert_eq!(time.len(), 20, "YYYY-MM-DDThh:mm:ssZ: {time}");
    assert!(time.ends_with('Z'), "and the time is UTC: {time}");
    for field in ["version", "device_requested", "fallback_reason"] {
        assert!(line.get(field).is_none_or(Value::is_null), "{field} stays null until MG-10 and MG-11 fill it in");
    }
}

#[test]
fn a_cancelled_job_leaves_one_cancelled_line_rather_than_two() {
    let server = support::TestServer::with("activity-cancel", true, Duration::from_millis(150));
    let id = server.submit_id(&support::fill_job(4, 2, 8, 3), &support::sample_image(), &support::sample_mask());
    server.wait_for_state(&id, "running");
    assert_eq!(server.delete(&format!("/v1/jobs/{id}")).status, 204, "the job is cancelled");
    server.wait_for_state(&id, "cancelled");
    let written = lines(&log_path(&server.endpoint));
    assert_eq!(written.len(), 1, "the DELETE writes the line, and the worker does not write another: {written:?}");
    assert_eq!(written.first().expect("a line").get("state").and_then(Value::as_str), Some("cancelled"));
    assert_eq!(written.first().expect("a line").get("model").and_then(Value::as_str), None, "a job that never ran names no model");
}

#[test]
fn a_clean_stop_logs_every_job_it_had_to_cancel() {
    let server = support::TestServer::with("activity-stop", true, Duration::from_millis(400));
    let first = server.submit_id(&support::fill_job(4, 2, 8, 3), &support::sample_image(), &support::sample_mask());
    let second = server.submit_id(&support::fill_job(4, 2, 8, 3), &support::sample_image(), &support::sample_mask());
    server.wait_for_state(&first, "running");
    assert_eq!(server.poll(&second).get("state").and_then(Value::as_str), Some("queued"), "the second job is still waiting when the stop arrives");
    let endpoint = server.endpoint.clone();
    let _ = server.shutdown();
    let written = lines(&log_path(&endpoint));
    assert_eq!(written.len(), 2, "the running job and the queued job each get a line: {written:?}");
    assert!(written.iter().all(|line| line.get("state").and_then(Value::as_str) == Some("cancelled")), "both were cancelled by the stop");
}

#[test]
fn a_log_that_cannot_be_written_does_not_change_what_a_job_reports() {
    let server = support::TestServer::fake("activity-readonly");
    let log = log_path(&server.endpoint);
    std::fs::write(&log, "").expect("the log exists before it is closed");
    set_readonly(&log, true);
    let id = server.submit_id(&support::fill_job(4, 2, 8, 3), &support::sample_image(), &support::sample_mask());
    let deadline = Instant::now() + support::WAIT;
    loop {
        let state = server.poll(&id).get("state").and_then(Value::as_str).unwrap_or_default().to_string();
        if state == "done" || state == "failed" {
            assert_eq!(state, "done", "a log that cannot be written never fails a job");
            break;
        }
        assert!(Instant::now() < deadline, "the job never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
    set_readonly(&log, false);
    assert_eq!(std::fs::metadata(&log).expect("the log is still there").len(), 0, "nothing was written to it");
}

/// Close a file for writing, or open it again. A machine where this makes no difference (running as
/// root, say) still passes the test above, which only asks that the job be unaffected.
fn set_readonly(path: &Path, readonly: bool) {
    let mut permissions = std::fs::metadata(path).expect("the file is there").permissions();
    permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions).expect("the file's permissions change");
}
