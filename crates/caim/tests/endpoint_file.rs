// The endpoint file, which is how PhotoCraft finds the manager: written before the server starts
// listening, valid while it runs, replaced on the next start, and gone after a clean shutdown.
// Each test gets a folder of its own, so "nothing was left behind" is a real check.

mod support;

use craft_ai_manager::server::{ServeOptions, Server};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use support::talk;

/// A folder of its own per test, so the tests cannot read each other's files.
fn folder_for(who: &str) -> PathBuf {
    std::env::temp_dir().join(format!("caim-endpoint-it-{}-{who}", std::process::id()))
}

fn endpoint_in(who: &str) -> PathBuf {
    folder_for(who).join("endpoint.json")
}

fn start(path: &Path) -> Server {
    let _ = std::fs::remove_dir_all(path.parent().expect("the endpoint file has a folder"));
    std::fs::create_dir_all(path.parent().expect("the endpoint file has a folder")).expect("the folder is created");
    Server::start(ServeOptions { fake_backend: true, endpoint_file: Some(path.to_path_buf()), step_delay: Duration::ZERO, ..ServeOptions::default() })
        .expect("the server starts")
}

/// The endpoint file as JSON.
fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("the endpoint file is readable");
    serde_json::from_str(&text).expect("the endpoint file is JSON")
}

/// Anything the run left in its folder besides the endpoint file itself.
fn leftovers(path: &Path) -> Vec<String> {
    std::fs::read_dir(path.parent().expect("the endpoint file has a folder"))
        .expect("the folder is readable")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name != "endpoint.json")
        .collect()
}

#[test]
fn the_endpoint_file_names_the_port_and_token_that_work() {
    let path = endpoint_in("write");
    let mut server = start(&path);
    let value = read(&path);
    assert_eq!(value.get("version").and_then(Value::as_u64), Some(1), "the file says which protocol it is");
    let port = value.get("port").and_then(Value::as_u64).expect("a port");
    assert_eq!(port, u64::from(server.port()), "the port is the one it bound");
    let token = value.get("token").and_then(Value::as_str).expect("the token is there");
    assert_eq!(token, server.token(), "the file carries this start's token");
    assert_eq!(token.len(), 64, "32 random bytes, hex");
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()), "the token is hex");
    assert_eq!(value.as_object().expect("an object").len(), 3, "version, port and token, and nothing else");
    let left = leftovers(&path);
    assert!(left.is_empty(), "the atomic write leaves no temp file behind: {left:?}");

    // A client that only knows the file can reach this server.
    let request = format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n");
    let reply = talk(u16::try_from(port).expect("a port that fits"), request.as_bytes());
    assert_eq!(reply.status, 200, "the endpoint file is enough to talk to the manager: {}", reply.text());

    server.shutdown().expect("a clean shutdown");
    assert!(std::fs::metadata(&path).is_err(), "a clean shutdown removes the endpoint file");
    let left = leftovers(&path);
    assert!(left.is_empty(), "and no temp file either: {left:?}");
}

#[test]
fn the_file_is_replaced_on_the_next_start() {
    let path = endpoint_in("replace");
    let first = start(&path);
    let old_token = first.token().to_string();
    drop(first);
    assert!(std::fs::metadata(&path).is_err(), "dropping the server is a clean shutdown too");

    let mut second = start(&path);
    let value = read(&path);
    let new_token = value.get("token").and_then(Value::as_str).expect("a token").to_string();
    assert_ne!(new_token, old_token, "every start gets a new token");
    assert_eq!(value.get("port").and_then(Value::as_u64), Some(u64::from(second.port())), "and the port of the run that is live");
    let fresh = format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {new_token}\r\nConnection: close\r\n\r\n", second.port());
    assert_eq!(talk(second.port(), fresh.as_bytes()).status, 200, "the new file works");
    let stale = format!("GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {old_token}\r\nConnection: close\r\n\r\n", second.port());
    assert_eq!(talk(second.port(), stale.as_bytes()).status, 401, "the previous start's token no longer works");
    second.shutdown().expect("a clean shutdown");
    assert!(std::fs::metadata(&path).is_err(), "and it cleans up again");
}

#[test]
fn a_file_another_run_owns_is_left_alone() {
    let path = endpoint_in("foreign");
    let mut server = start(&path);
    let foreign = r#"{"version": 1, "port": 41000, "token": "someone-elses-token"}"#;
    std::fs::write(&path, foreign).expect("another manager replaces our file");
    server.shutdown().expect("a clean shutdown");
    let text = std::fs::read_to_string(&path).expect("the other run's file is still there");
    assert_eq!(text, foreign, "we only delete a file that still holds our token");
}

#[test]
fn a_start_that_cannot_write_its_file_does_not_start() {
    // A folder that is really a file cannot hold an endpoint file, so the start must fail cleanly.
    let blocker = folder_for("blocked");
    let _ = std::fs::remove_file(&blocker);
    std::fs::write(&blocker, "not a folder").expect("the blocker is a file");
    let path = blocker.join("endpoint.json");
    match Server::start(ServeOptions { fake_backend: true, endpoint_file: Some(path), ..ServeOptions::default() }) {
        Ok(mut server) => {
            let _ = server.shutdown();
            panic!("a start that cannot write its endpoint file must not report a running server");
        }
        Err(failed) => assert!(failed.to_string().contains("could not create"), "the sentence says what went wrong: {failed}"),
    }
    let _ = std::fs::remove_file(&blocker);
}
