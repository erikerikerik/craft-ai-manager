//! Plumbing shared by the unit tests: a scratch folder that cleans up after itself, a catalog entry
//! built from fixture bytes, and a [Fetch](crate::fetch::Fetch) that hands over bytes instead of a
//! network. None of it is compiled into the binary, and no test that uses it touches the network.

use std::cell::Cell;
use std::path::PathBuf;

use crate::catalog::{Model, OverrideNote};
use crate::error::{Fail, Result};
use crate::fetch::{Fetch, Stream};
use crate::store;

/// A folder under the system temp dir, made fresh and removed when the test that owns it ends.
pub(crate) struct Scratch {
    pub(crate) path: PathBuf,
}

impl Scratch {
    /// `who` keeps the tests apart, because they run in parallel in one process.
    pub(crate) fn new(who: &str) -> Self {
        let path = std::env::temp_dir().join(format!("caim-store-{}-{who}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch folder can be made");
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A catalog entry for `bytes`: the hash and the size are the fixture's own, so a download of those
/// bytes installs cleanly. `note` gives the entry an override that has to be acknowledged.
pub(crate) fn test_model(bytes: &[u8], note: Option<&str>) -> Model {
    Model {
        id: "lama".to_string(),
        version: "1".to_string(),
        task: "fill_masked_area".to_string(),
        file: "lama_fp32.onnx".to_string(),
        url: "https://example.invalid/lama_fp32.onnx".to_string(),
        revision: "a3ee2fca54baebec351b8fa7786154ffa7555aa6".to_string(),
        sha256: store::sha256_hex(bytes),
        size: bytes.len() as u64,
        max_pixels: 64_000_000,
        memory_mib: 1_500,
        backends: [("cpu".to_string(), "supported".to_string())].into_iter().collect(),
        licence_code: "Apache-2.0".to_string(),
        licence_weights: "Apache-2.0".to_string(),
        licence_training_data: "Apache-2.0".to_string(),
        provenance: "A fixture made of bytes, for a test.".to_string(),
        note: note.map(|text| OverrideNote {
            by: "Erik".to_string(),
            date: "2026-10-13".to_string(),
            reason: "A test override.".to_string(),
            note: text.to_string(),
        }),
    }
}

/// A [Fetch](Fetch) that hands over fixed bytes and counts the requests, so a test can prove that an
/// installed model makes none at all.
pub(crate) struct FakeFetch {
    pub(crate) bytes: Vec<u8>,
    /// What the fake server says the length is, when it says.
    pub(crate) claimed: Option<u64>,
    pub(crate) calls: Cell<usize>,
}

impl FakeFetch {
    pub(crate) fn new(bytes: &[u8]) -> Self {
        Self { bytes: bytes.to_vec(), claimed: None, calls: Cell::new(0) }
    }

    /// Claim a length the catalog does not promise, which is what a wrong or hostile server does.
    pub(crate) fn claiming(mut self, length: u64) -> Self {
        self.claimed = Some(length);
        self
    }
}

impl Fetch for FakeFetch {
    fn open(&self, _url: &str) -> Result<Stream> {
        self.calls.set(self.calls.get() + 1);
        Ok(Stream::full(self.claimed, Box::new(std::io::Cursor::new(self.bytes.clone()))))
    }
}

/// A [Fetch](Fetch) that can serve part of a file, so restart-safe resume can be tested without a
/// network. It remembers the offsets it was asked for, which is how a test can see that only the
/// missing bytes were requested.
pub(crate) struct RangeFetch {
    pub(crate) bytes: Vec<u8>,
    /// Whether a range request gets the tail (`true`) or the whole file again (`false`).
    pub(crate) supports_range: bool,
    /// A status to answer a range request with instead of bytes, which is what a broken server does.
    pub(crate) refused: Option<u16>,
    /// The offsets it was asked for, in the order it was asked.
    pub(crate) offsets: std::cell::RefCell<Vec<u64>>,
}

impl RangeFetch {
    /// A server that answers a range request with the bytes from that offset on.
    pub(crate) fn new(bytes: &[u8]) -> Self {
        Self { bytes: bytes.to_vec(), supports_range: true, refused: None, offsets: std::cell::RefCell::new(Vec::new()) }
    }

    /// A server that ignores `Range` and sends the whole file again, which is what makes a download
    /// start over.
    pub(crate) fn ignoring_range(bytes: &[u8]) -> Self {
        Self { supports_range: false, ..Self::new(bytes) }
    }

    /// A server that answers a range request with `status` and no bytes.
    pub(crate) fn refusing(mut self, status: u16) -> Self {
        self.refused = Some(status);
        self
    }

    /// The offsets this server was asked for, oldest first.
    pub(crate) fn asked(&self) -> Vec<u64> {
        self.offsets.borrow().clone()
    }
}

impl Fetch for RangeFetch {
    fn open(&self, url: &str) -> Result<Stream> {
        self.open_range(url, 0)
    }

    fn open_range(&self, url: &str, offset: u64) -> Result<Stream> {
        self.offsets.borrow_mut().push(offset);
        if offset > 0 {
            if let Some(status) = self.refused {
                return Err(Fail::internal(format!("{url} answered {status} to the range request.")));
            }
            if self.supports_range {
                let tail = self.bytes.get(offset as usize..).map(<[u8]>::to_vec).unwrap_or_default();
                return Ok(Stream::partial(Some(tail.len() as u64), Box::new(std::io::Cursor::new(tail))));
            }
        }
        Ok(Stream::full(Some(self.bytes.len() as u64), Box::new(std::io::Cursor::new(self.bytes.clone()))))
    }
}

/// A [Fetch](Fetch) that can be shared between threads and hands its bytes over slowly, a chunk at
/// a time, so two installs at once really do overlap.
pub(crate) struct SlowFetch {
    pub(crate) bytes: Vec<u8>,
    pub(crate) chunk: usize,
    pub(crate) delay: std::time::Duration,
}

impl Fetch for SlowFetch {
    fn open(&self, _url: &str) -> Result<Stream> {
        Ok(Stream::full(None, Box::new(SlowReader { bytes: self.bytes.clone(), at: 0, chunk: self.chunk, delay: self.delay })))
    }
}

/// The reader behind [SlowFetch].
struct SlowReader {
    bytes: Vec<u8>,
    at: usize,
    chunk: usize,
    delay: std::time::Duration,
}

impl std::io::Read for SlowReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        std::thread::sleep(self.delay);
        let end = self.at.saturating_add(self.chunk.min(buffer.len())).min(self.bytes.len());
        let piece = &self.bytes[self.at..end];
        buffer[..piece.len()].copy_from_slice(piece);
        self.at = end;
        Ok(piece.len())
    }
}
