// Shared plumbing for the integration tests: a server on a temp endpoint file, a raw TCP client
// that speaks the protocol by hand, and the PNG and multipart builders that mirror what
// PhotoCraft's ai-client sends. Every test file is its own crate, so a helper one of them does not
// use is not a warning.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use craft_ai_manager::server::{ServeOptions, Server};
use png::{BitDepth, ColorType, Decoder, Encoder};

/// How long a test waits for a job to move through its states.
pub const WAIT: Duration = Duration::from_secs(20);
/// The boundary every test body uses; no PNG in these tests contains it.
pub const BOUNDARY: &str = "caim-integration-boundary";
/// The only task MG-01 knows.
pub const TASK: &str = "fill_masked_area";

/// One reply, read to the end of the connection (the server always closes).
pub struct Reply {
    pub status: u16,
    pub headers: Vec<String>,
    pub body: Vec<u8>,
}

impl Reply {
    /// The body as text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// The body as JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the answer is JSON")
    }

    /// The sentence of an {"error": "..."} body.
    pub fn sentence(&self) -> String {
        self.json().get("error").and_then(serde_json::Value::as_str).expect("the error body has a sentence").to_string()
    }

    /// Is this header in the reply? Names are matched case-insensitively.
    pub fn has_header(&self, name: &str) -> bool {
        let prefix = format!("{name}:").to_ascii_lowercase();
        self.headers.iter().any(|line| line.starts_with(&prefix))
    }

    /// The value of one reply header.
    pub fn header(&self, name: &str) -> Option<&str> {
        let prefix = format!("{name}:").to_ascii_lowercase();
        self.headers.iter().find(|line| line.starts_with(&prefix)).map(|line| line[prefix.len()..].trim())
    }
}

/// Send one request over a fresh connection and read the whole reply.
pub fn talk(port: u16, request: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the server takes a connection");
    stream.set_read_timeout(Some(Duration::from_secs(30))).expect("a read timeout");
    stream.write_all(request).expect("the request goes out");
    stream.flush().expect("the request is flushed");
    // Half-close so a server that is waiting for a body sees the end of the request.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the reply is read to the end");
    let split = find_split(&raw).expect("the reply has a header block");
    Reply { status: status_code(&raw[..split]), headers: header_lines(&raw[..split]), body: raw[split.saturating_add(4)..].to_vec() }
}

/// The end of the header block.
fn find_split(raw: &[u8]) -> Option<usize> {
    (0..raw.len().saturating_sub(3)).find(|at| raw[*at] == b'\r' && raw[*at + 1] == b'\n' && raw[*at + 2] == b'\r' && raw[*at + 3] == b'\n')
}

/// The number on the status line.
fn status_code(head: &[u8]) -> u16 {
    let line: Vec<u8> = head.iter().copied().take_while(|byte| *byte != b'\r' && *byte != b'\n').collect();
    String::from_utf8_lossy(&line).split(' ').nth(1).unwrap_or_default().parse().unwrap_or_default()
}

/// The header lines, lowercased, without the status line.
fn header_lines(head: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(head).split("\r\n").skip(1).filter(|line| !line.is_empty()).map(|line| line.to_ascii_lowercase()).collect()
}

/// An empty model store of the test's own, so a test decides what is installed instead of
/// inheriting the model folder of the machine it runs on.
pub fn empty_store(who: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("caim-it-{}-{who}-models", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("an empty model store folder");
    path
}

/// A server on its own temp endpoint file, plus the framing every test needs.
pub struct TestServer {
    server: Server,
    pub endpoint: PathBuf,
}

impl TestServer {
    /// A fake-backend server that runs jobs as fast as it can.
    pub fn fake(who: &str) -> TestServer {
        TestServer::with(who, true, Duration::ZERO)
    }

    /// A server with the fake backend on or off, and the fake backend's step delay.
    pub fn with(who: &str, fake_backend: bool, step_delay: Duration) -> TestServer {
        TestServer::options(who, ServeOptions { fake_backend, step_delay, ..ServeOptions::default() })
    }

    /// A server that sees no installed model, whatever the machine itself happens to have (MG-04
    /// item 2). The model store is an empty folder of its own.
    pub fn no_model(who: &str) -> TestServer {
        TestServer::options(who, ServeOptions { models_root: Some(empty_store(who)), ..ServeOptions::default() })
    }

    /// A server that looks for LaMa in `root` instead of the machine's model folder.
    pub fn lama_store(who: &str, root: &Path) -> TestServer {
        TestServer::options(who, ServeOptions { models_root: Some(root.to_path_buf()), ..ServeOptions::default() })
    }

    /// Any options at all, with the endpoint file taken care of.
    pub fn options(who: &str, options: ServeOptions) -> TestServer {
        let endpoint = std::env::temp_dir().join(format!("caim-it-{}-{who}.json", std::process::id()));
        let _ = std::fs::remove_file(&endpoint);
        let server = Server::start(ServeOptions { endpoint_file: Some(endpoint.clone()), ..options }).expect("the server starts");
        TestServer { server, endpoint }
    }

    pub fn port(&self) -> u16 {
        self.server.port()
    }

    pub fn token(&self) -> &str {
        self.server.token()
    }

    /// One well-formed request, framed exactly as PhotoCraft's client frames it.
    pub fn request(&self, method: &str, path: &str, content_type: Option<&str>, body: &[u8]) -> Reply {
        let mut out = Vec::new();
        out.extend_from_slice(format!("{method} {path} HTTP/1.1\r\n").as_bytes());
        out.extend_from_slice(format!("Host: 127.0.0.1:{}\r\n", self.port()).as_bytes());
        out.extend_from_slice(format!("Authorization: Bearer {}\r\n", self.token()).as_bytes());
        out.extend_from_slice(b"Accept: application/json, image/png\r\nConnection: close\r\n");
        if let Some(content_type) = content_type {
            out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        out.extend_from_slice(body);
        talk(self.port(), &out)
    }

    pub fn get(&self, path: &str) -> Reply {
        self.request("GET", path, None, &[])
    }

    pub fn delete(&self, path: &str) -> Reply {
        self.request("DELETE", path, None, &[])
    }

    /// A request with the framing this server should refuse.
    pub fn send_raw(&self, request: &[u8]) -> Reply {
        talk(self.port(), request)
    }

    /// POST the three parts of a job, in the order the protocol asks for.
    pub fn submit(&self, job: &str, image: &[u8], mask: &[u8]) -> Reply {
        let parts = job_parts(job, image, mask);
        self.request("POST", "/v1/jobs", Some(&format!("multipart/form-data; boundary={BOUNDARY}")), &multipart(BOUNDARY, &parts))
    }

    /// The id of an accepted job.
    pub fn submit_id(&self, job: &str, image: &[u8], mask: &[u8]) -> String {
        let reply = self.submit(job, image, mask);
        assert_eq!(reply.status, 202, "the job is accepted: {}", reply.text());
        reply.json().get("id").and_then(serde_json::Value::as_str).expect("the answer has an id").to_string()
    }

    pub fn poll(&self, id: &str) -> serde_json::Value {
        self.get(&format!("/v1/jobs/{id}")).json()
    }

    /// Wait for one state, and fail loudly (with the last answer) if it never arrives.
    pub fn wait_for_state(&self, id: &str, want: &str) -> serde_json::Value {
        let until = Instant::now() + WAIT;
        loop {
            let value = self.poll(id);
            if value.get("state").and_then(serde_json::Value::as_str) == Some(want) {
                return value;
            }
            if Instant::now() > until {
                panic!("job {id} never reached {want}: {value}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Stop cleanly and hand back the endpoint path, which should now be gone.
    pub fn shutdown(mut self) -> PathBuf {
        self.server.shutdown().expect("a clean shutdown");
        self.endpoint.clone()
    }
}

/// One part of a multipart body, as the client sends it.
#[derive(Clone)]
pub struct Part {
    pub name: &'static str,
    pub filename: &'static str,
    pub content_type: &'static str,
    pub data: Vec<u8>,
}

/// The three parts of a job body.
pub fn job_parts(job: &str, image: &[u8], mask: &[u8]) -> Vec<Part> {
    vec![
        Part { name: "job", filename: "job.json", content_type: "application/json", data: job.as_bytes().to_vec() },
        Part { name: "image", filename: "image.png", content_type: "image/png", data: image.to_vec() },
        Part { name: "mask", filename: "mask.png", content_type: "image/png", data: mask.to_vec() },
    ]
}

/// The body, byte for byte the way PhotoCraft's client builds it.
pub fn multipart(boundary: &str, parts: &[Part]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
        body.extend_from_slice(part.name.as_bytes());
        body.extend_from_slice(b"\"; filename=\"");
        body.extend_from_slice(part.filename.as_bytes());
        body.extend_from_slice(b"\"\r\nContent-Type: ");
        body.extend_from_slice(part.content_type.as_bytes());
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(&part.data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    body
}

/// The job part: the client's own field names, with the region the size of the image.
pub fn job_json(task: &str, width: u32, height: u32, region: [i32; 4], bit_depth: u8, channels: u8) -> String {
    format!(
        r#"{{"task":"{task}","image_size":[{width},{height}],"region":[{left},{top},{wide},{tall}],"bit_depth":{bit_depth},"channels":{channels},"prompt":null,"settings":{{}},"provider":"auto","client":"photocraft/0.9.0"}}"#,
        task = task,
        width = width,
        height = height,
        left = region[0],
        top = region[1],
        wide = region[2],
        tall = region[3],
        bit_depth = bit_depth,
        channels = channels
    )
}

/// The job part a real PhotoCraft submit would send.
pub fn fill_job(width: u32, height: u32, bit_depth: u8, channels: u8) -> String {
    job_json(TASK, width, height, [0, 0, width as i32, height as i32], bit_depth, channels)
}

/// An RGB PNG, 8- or 16-bit, from interleaved samples.
pub fn png_rgb(width: u32, height: u32, bit_depth: u8, samples: &[u16]) -> Vec<u8> {
    let raw: Vec<u8> = if bit_depth == 8 {
        samples.iter().map(|sample| u8::try_from(*sample).expect("an 8-bit sample")).collect()
    } else {
        samples.iter().flat_map(|sample| sample.to_be_bytes()).collect()
    };
    encode_png(width, height, if bit_depth == 8 { BitDepth::Eight } else { BitDepth::Sixteen }, ColorType::Rgb, &raw)
}

/// An 8-bit grayscale PNG: the mask.
pub fn png_gray(width: u32, height: u32, values: &[u8]) -> Vec<u8> {
    encode_png(width, height, BitDepth::Eight, ColorType::Grayscale, values)
}

fn encode_png(width: u32, height: u32, depth: BitDepth, color: ColorType, raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().expect("the header");
        writer.write_image_data(raw).expect("the pixels");
        writer.finish().expect("the end of the PNG");
    }
    out
}

/// A decoded PNG, so a test can compare pixels rather than bytes.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub bit_depth: u8,
    pub samples: Vec<u16>,
}

impl Decoded {
    /// The sample at one pixel and channel.
    pub fn sample(&self, pixel: usize, channel: usize) -> u16 {
        self.samples[pixel * self.channels + channel]
    }
}

pub fn decode_png(bytes: &[u8]) -> Decoded {
    let decoder = Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("the result is a PNG");
    let (width, height, channels, bit_depth) = {
        let info = reader.info();
        let channels = match info.color_type {
            ColorType::Grayscale => 1,
            ColorType::GrayscaleAlpha => 2,
            ColorType::Rgb => 3,
            ColorType::Rgba => 4,
            _ => 3,
        };
        (info.width, info.height, channels, if info.bit_depth == BitDepth::Sixteen { 16 } else { 8 })
    };
    let size = reader.output_buffer_size().expect("a buffer size for the frame");
    let mut raw = vec![0u8; size];
    reader.next_frame(&mut raw).expect("the pixels of the frame");
    let samples = if bit_depth == 8 {
        raw.iter().map(|byte| u16::from(*byte)).collect()
    } else {
        raw.as_chunks::<2>().0.iter().map(|pair| u16::from_be_bytes(*pair)).collect()
    };
    Decoded { width, height, channels, bit_depth, samples }
}

/// A 4x2 RGB image with eight different pixels, so a mean colour is easy to check by hand.
pub fn sample_image() -> Vec<u8> {
    png_rgb(4, 2, 8, &[10, 20, 30, 200, 100, 50, 40, 60, 80, 90, 20, 10, 100, 100, 100, 0, 0, 0, 7, 9, 11, 33, 66, 99])
}

/// A 4x2 mask that covers only the first pixel.
pub fn sample_mask() -> Vec<u8> {
    png_gray(4, 2, &[255, 0, 0, 0, 0, 0, 0, 0])
}
