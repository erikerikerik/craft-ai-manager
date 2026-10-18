//! MG-12: a download that was cut short is picked up where it stopped.
//!
//! The unit tests in `fetch.rs` hand over bytes instead of a network. This one uses the real
//! [HttpFetch](craft_ai_manager::fetch::HttpFetch) against a server on loopback, so the `Range:`
//! header, the `206`, and the bytes that follow it are the ones `ureq` really reads.
//! Plain HTTP is only ever allowed here by [HttpFetch::allowing_plain_http]; the program itself
//! insists on HTTPS.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;

use craft_ai_manager::catalog::Model;
use craft_ai_manager::fetch::{HttpFetch, Outcome, install};
use craft_ai_manager::store;

mod support;

/// The model file, big enough that half of it is worth resuming.
const SIZE: usize = 64 * 1024;

/// Bytes that are not all the same, so a resumed file that mixed up the halves cannot pass.
fn fixture() -> Vec<u8> {
    (0..SIZE).map(|index| (index % 251) as u8 ^ (index / 251) as u8).collect()
}

/// A catalog entry pointing at the loopback server on `port`.
fn entry(port: u16, bytes: &[u8]) -> Model {
    Model {
        id: "lama".to_string(),
        version: "1".to_string(),
        task: "fill_masked_area".to_string(),
        file: "lama_fp32.onnx".to_string(),
        url: format!("http://127.0.0.1:{port}/lama_fp32.onnx"),
        revision: "test-revision".to_string(),
        sha256: store::sha256_hex(bytes),
        size: bytes.len() as u64,
        max_pixels: 64_000_000,
        memory_mib: 1_500,
        backends: [("cpu".to_string(), "supported".to_string())].into_iter().collect(),
        licence_code: "Apache-2.0".to_string(),
        licence_weights: "Apache-2.0".to_string(),
        licence_training_data: "Apache-2.0".to_string(),
        provenance: "A fixture served by a test.".to_string(),
        note: None,
    }
}

/// The offset a request asked to resume from, or `None` when it asked for the whole file.
fn asked_from(request: &str) -> Option<u64> {
    for line in request.lines() {
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
            return rest.trim().trim_end_matches('-').parse::<u64>().ok();
        }
    }
    None
}

/// Read one request's headers, up to the blank line.
fn read_headers(socket: &mut std::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    while socket.read(&mut byte).unwrap_or(0) == 1 {
        bytes.push(byte[0]);
        if bytes.len() >= 4 && bytes[bytes.len() - 4..] == *b"\r\n\r\n" {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

#[test]
fn a_cut_download_is_resumed_with_a_range_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port on loopback");
    let port = listener.local_addr().expect("the loopback address").port();
    let bytes = fixture();
    let half = bytes.len() / 2;
    let (sender, receiver) = mpsc::channel::<Option<u64>>();
    let server = {
        let bytes = bytes.clone();
        std::thread::spawn(move || {
            // Two requests: the first is answered with the front half of the file and stops there,
            // the second is answered with whatever the client asks to resume from.
            for expected in 0..2 {
                let Ok(mut socket) = listener.accept().map(|(socket, _)| socket) else { return };
                let request = read_headers(&mut socket);
                let from = asked_from(&request);
                let _ = sender.send(from);
                let (status, body) =
                    if expected == 0 { ("200 OK", bytes[..half].to_vec()) } else { ("206 Partial Content", bytes[from.unwrap_or(0) as usize..].to_vec()) };
                let head =
                    format!("HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).and_then(|()| socket.write_all(&body)).and_then(|()| socket.flush());
            }
        })
    };

    let root = support::empty_store("resume");
    let model = entry(port, &bytes);
    let fetcher = HttpFetch::allowing_plain_http();

    // The first install only gets half the file, so it keeps what arrived instead of throwing it away.
    let fail = install(&model, &root, &fetcher, &mut |_read, _total| {}).expect_err("the server stopped at half the file");
    assert!(fail.sentence.contains("picks the download up"), "the sentence says what happens next: {fail}");
    let folder = store::model_folder(&root, &model);
    let pair: Vec<PathBuf> = std::fs::read_dir(&folder)
        .expect("the model folder")
        .flatten()
        .map(|item| item.path())
        .filter(|path| {
            let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
            store::is_part_name(&model, &name) || store::is_part_meta_name(&model, &name)
        })
        .collect();
    assert_eq!(pair.len(), 2, "the half file and its note are both kept: {pair:?}");
    let note = pair.iter().find(|path| path.to_string_lossy().ends_with(".json")).expect("the note is one of them");
    let meta = store::read_part_meta(note).expect("the note can be read");
    assert_eq!(meta.bytes, half as u64, "and it says how far the download got");
    assert_eq!(meta.sha256, model.sha256, "and what the finished file has to hash to");
    assert!(!store::is_installed(&root, &model), "a half file is not an installed model");

    // Pretend the machine was restarted: the files are now old enough to be nobody's download.
    for path in &pair {
        let file = std::fs::OpenOptions::new().write(true).open(path).expect("the kept file opens");
        file.set_modified(std::time::SystemTime::now() - craft_ai_manager::fetch::LOCK_STALE - std::time::Duration::from_secs(1))
            .expect("the kept file can be made old");
    }

    // The second install re-hashes those bytes and asks for the rest.
    let outcome = install(&model, &root, &fetcher, &mut |_read, _total| {}).expect("the next install picks the download up");
    match outcome {
        Outcome::Downloaded { sha256, bytes: written, resumed_from, .. } => {
            assert_eq!(resumed_from, half as u64, "the bytes on disk were continued, not fetched again");
            assert_eq!(written, model.size, "and the whole file arrived");
            assert_eq!(sha256, model.sha256, "which is what the catalog promised");
        }
        Outcome::AlreadyInstalled => panic!("a half file is not an installed model"),
    }
    let asked: Vec<Option<u64>> = receiver.iter().take(2).collect();
    assert_eq!(asked, vec![None, Some(half as u64)], "the first request wanted the file, the second only the rest");
    assert_eq!(std::fs::read(store::model_file(&root, &model)).expect("the installed file"), bytes, "byte for byte");
    assert!(store::is_installed(&root, &model));
    let left = std::fs::read_dir(&folder).expect("the model folder").flatten().filter(|item| {
        let name = item.file_name().to_string_lossy().to_string();
        store::is_part_name(&model, &name) || store::is_part_meta_name(&model, &name) || name.contains(".tmp-")
    });
    let left: Vec<String> = left.map(|item| item.file_name().to_string_lossy().to_string()).collect();
    assert!(left.is_empty(), "nothing is left behind once the file is in place: {left:?}");
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&root);
}
