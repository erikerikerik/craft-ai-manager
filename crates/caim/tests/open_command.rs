// `caim open` when a manager is already answering: it says so, exits 0, and starts nothing - no
// second port, and no new endpoint file (SPEC v1.1 amendment 18, "Starting CAIM on demand").

mod support;

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[test]
fn open_names_the_manager_that_is_running_and_binds_nothing_new() {
    let server = support::TestServer::fake("open-running");
    let endpoint = server.endpoint.clone();
    let store = support::empty_store("open-running");
    let mut child = Command::new(env!("CARGO_BIN_EXE_caim"))
        .arg("open")
        .env(craft_ai_manager::endpoint::ENDPOINT_ENV, &endpoint)
        .env(craft_ai_manager::store::MODELS_ENV, &store)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    let status = wait(&mut child);
    let mut out = String::new();
    let mut err = String::new();
    if let Some(stdout) = child.stdout.as_mut() {
        stdout.read_to_string(&mut out).expect("the output is readable");
    }
    if let Some(stderr) = child.stderr.as_mut() {
        stderr.read_to_string(&mut err).expect("the complaints are readable");
    }
    assert!(status.success(), "finding a running manager is not an error: {status}, stderr: {err}");
    assert!(out.contains("already running"), "the sentence says what it found: {out}");
    assert!(out.contains(&server.port().to_string()), "and names the port that is answering: {out}");
    assert!(err.is_empty(), "and complains about nothing: {err}");

    let text = std::fs::read_to_string(&endpoint).expect("the endpoint file is still there");
    let value: serde_json::Value = serde_json::from_str(&text).expect("the endpoint file is JSON");
    assert_eq!(value.get("port").and_then(serde_json::Value::as_u64), Some(u64::from(server.port())), "the running manager's file was not replaced");
    assert_eq!(value.get("token").and_then(serde_json::Value::as_str), Some(server.token()), "nor rewritten");
    assert_eq!(server.get("/v1/health").status, 200, "the manager is still serving");
    let _ = server.shutdown();
}

/// Watch the child until it finishes, and stop it if it outstays its welcome: a test that hangs is
/// worse than a test that fails.
fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("the child is still ours to watch") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("caim open never finished, so it was stopped");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
