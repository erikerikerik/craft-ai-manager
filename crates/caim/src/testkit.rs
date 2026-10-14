//! Plumbing shared by the unit tests: a scratch folder that cleans up after itself, a catalog entry
//! built from fixture bytes, and a [Fetch](crate::fetch::Fetch) that hands over bytes instead of a
//! network. None of it is compiled into the binary, and no test that uses it touches the network.

use std::cell::Cell;
use std::path::PathBuf;

use crate::catalog::{Model, OverrideNote};
use crate::error::Result;
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
        Ok(Stream { claimed_len: self.claimed, reader: Box::new(std::io::Cursor::new(self.bytes.clone())) })
    }
}
