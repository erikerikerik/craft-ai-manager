// `caim open` when a manager is already answering: it says so, exits 0, and starts nothing - no
// second port, and no new endpoint file (SPEC v1.1 amendment 18, "Starting CAIM on demand"). And
// when two runs start at the same moment, they decide once: one manager, one endpoint file.

mod support;

use std::io::Read;
use std::net::TcpStream;
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
    let (out, err) = read_output(&mut child);
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

#[test]
fn open_does_not_start_a_second_manager_while_the_first_is_busy() {
    // A manager whose every connection slot is taken answers /v1/health with a 503 before it even
    // routes the request. That is a manager that is running, not one that has gone away.
    let options = craft_ai_manager::server::ServeOptions { fake_backend: true, max_connections: 1, ..craft_ai_manager::server::ServeOptions::default() };
    let server = support::TestServer::options("open-busy", options);
    let holder = TcpStream::connect(("127.0.0.1", server.port())).expect("the first connection takes the only slot");
    std::thread::sleep(Duration::from_millis(100));
    let store = support::empty_store("open-busy");
    let mut child = Command::new(env!("CARGO_BIN_EXE_caim"))
        .arg("open")
        .env(craft_ai_manager::endpoint::ENDPOINT_ENV, &server.endpoint)
        .env(craft_ai_manager::store::MODELS_ENV, &store)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    let status = wait(&mut child);
    let (out, err) = read_output(&mut child);
    assert!(status.success(), "a busy manager is not an error: {status}, stderr: {err}");
    assert!(out.contains("already running"), "the busy manager is still the running one: {out}");
    assert!(out.contains(&server.port().to_string()), "and its port is the one named: {out}");
    let text = std::fs::read_to_string(&server.endpoint).expect("the endpoint file is still there");
    let value: serde_json::Value = serde_json::from_str(&text).expect("the endpoint file is JSON");
    assert_eq!(value.get("port").and_then(serde_json::Value::as_u64), Some(u64::from(server.port())), "the busy manager's file was not replaced");
    drop(holder);
    assert_eq!(server.get("/v1/health").status, 200, "the same manager answers again once a slot frees");
    let _ = server.shutdown();
}

#[test]
fn two_caim_open_runs_at_once_start_one_manager_between_them() {
    // Two runs launched at the same moment, with nothing running yet. Deciding and starting have to
    // be one step: otherwise both start a manager, both replace the endpoint file, and the loser
    // keeps serving on a port no client can find.
    let folder = std::env::temp_dir().join(format!("caim-it-{}-open-race", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).expect("a folder for the endpoint file");
    let endpoint = folder.join("endpoint.json");
    let store = support::empty_store("open-race");
    let mut first = open_run(&endpoint, &store);
    let mut second = open_run(&endpoint, &store);
    let deadline = Instant::now() + Duration::from_secs(20);
    while first.try_wait().expect("the first child is ours to watch").is_none() && second.try_wait().expect("the second child is ours to watch").is_none() {
        assert!(Instant::now() < deadline, "neither run decided anything, so the launch lock is not doing its job");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (mut done, mut running) = if second.try_wait().expect("the second child is ours to watch").is_some() { (second, first) } else { (first, second) };
    let status = wait(&mut done);
    let (out, err) = read_output(&mut done);
    assert!(status.success(), "losing the race is not an error: {status}, stderr: {err}");
    assert!(out.contains("already running") || out.contains("being started by another"), "the run that did not start a manager says so: {out}");
    assert!(err.is_empty(), "and complains about nothing: {err}");
    assert!(running.try_wait().expect("the other child is ours to watch").is_none(), "the other run is still serving");

    let text = std::fs::read_to_string(&endpoint).expect("one endpoint file, written by the run that started a manager");
    let value: serde_json::Value = serde_json::from_str(&text).expect("the endpoint file is JSON");
    let port = u16::try_from(value.get("port").and_then(serde_json::Value::as_u64).expect("the file names a port")).expect("a port that fits");
    let token = value.get("token").and_then(serde_json::Value::as_str).expect("the file names a token").to_string();
    let request = format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n");
    assert_eq!(support::talk(port, request.as_bytes()).status, 200, "one manager is answering, and it answers for the token it published");
    let left_behind: Vec<String> = std::fs::read_dir(&folder)
        .expect("the folder is readable")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("open.lock"))
        .collect();
    assert!(left_behind.is_empty(), "the launch lock is gone once a manager is listening: {left_behind:?}");
    running.kill().expect("the manager is ours to stop");
    running.wait().expect("the manager stopped");
    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn open_fails_loudly_when_it_cannot_make_its_launch_lock() {
    // Something is sitting where the endpoint folder belongs, so neither the lock nor the endpoint
    // file can be made. Exiting 0 with "another `caim open` is starting" would promise a manager that
    // nobody is starting, so this run has to fail instead.
    let folder = std::env::temp_dir().join(format!("caim-it-{}-open-no-folder", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).expect("a folder for the test");
    let blocker = folder.join("not-a-folder");
    std::fs::write(&blocker, "an ordinary file, where a folder belongs").expect("a file can sit where the folder belongs");
    let endpoint = blocker.join("endpoint.json");
    let store = support::empty_store("open-no-folder");
    let mut child = open_run(&endpoint, &store);
    let status = wait(&mut child);
    let (out, err) = read_output(&mut child);
    assert!(!status.success(), "a launch lock that cannot be made is a failure, not a success: {status}, stdout: {out}");
    assert!(!out.contains("being started by another"), "and it does not blame another run: {out}");
    assert!(!err.is_empty(), "it says what went wrong: {err}");
    assert!(!endpoint.exists(), "and no endpoint file was written");
    let _ = std::fs::remove_dir_all(&folder);
}

/// One `caim open` pointed at one endpoint file, with its output kept rather than printed. Stdin is
/// closed so the run never waits on the terminal of whoever is running the tests.
fn open_run(endpoint: &std::path::Path, store: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_caim"))
        .arg("open")
        .env(craft_ai_manager::endpoint::ENDPOINT_ENV, endpoint)
        .env(craft_ai_manager::store::MODELS_ENV, store)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs")
}

/// Everything a finished child wrote, in one go.
fn read_output(child: &mut Child) -> (String, String) {
    let mut out = String::new();
    let mut err = String::new();
    if let Some(stdout) = child.stdout.as_mut() {
        stdout.read_to_string(&mut out).expect("the output is readable");
    }
    if let Some(stderr) = child.stderr.as_mut() {
        stderr.read_to_string(&mut err).expect("the complaints are readable");
    }
    (out, err)
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
