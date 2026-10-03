//! A scripted `Fetcher` for tests: each URL replays its own list of replies in order, and every
//! `open` is recorded. No network is touched.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use super::fetch::{Body, FetchError, Fetcher, Opened};
use super::manifest::FileSpec;

pub enum FakeReply {
    /// `open` fails with this error.
    Fail(FetchError),
    /// `open` succeeds; the body yields `chunks`, then ends with `then` (an error) or normally.
    Serve {
        status: u16,
        total: Option<u64>,
        html: bool,
        chunks: Vec<Vec<u8>>,
        then: Option<FetchError>,
    },
}

#[derive(Default)]
pub struct FakeFetcher {
    scripts: Mutex<HashMap<String, VecDeque<FakeReply>>>,
    calls: Mutex<Vec<(String, u64)>>,
    gate: Mutex<Option<Arc<Notify>>>,
}

impl FakeFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends replies for `url`, consumed in order; once they run out `open` fails with `Connect`.
    pub fn script(&self, url: &str, replies: Vec<FakeReply>) -> &Self {
        self.scripts.lock().unwrap().entry(url.to_string()).or_default().extend(replies);
        self
    }

    /// Every `open` so far: (url, offset).
    pub fn calls(&self) -> Vec<(String, u64)> {
        self.calls.lock().unwrap().clone()
    }

    /// The next `open` (recorded first) waits until the returned `Notify` gets `notify_one`.
    pub fn gate(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}

pub struct FakeBody {
    chunks: VecDeque<Vec<u8>>,
    then: Option<FetchError>,
}

impl Body for FakeBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, FetchError> {
        // Let other tasks (a status poll, a cancel) run between chunks, like a real stream.
        tokio::task::yield_now().await;
        if let Some(chunk) = self.chunks.pop_front() {
            return Ok(Some(chunk));
        }
        match self.then.take() {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }
}

impl Fetcher for FakeFetcher {
    type Body = FakeBody;

    async fn open(&self, url: &str, offset: u64) -> Result<Opened<FakeBody>, FetchError> {
        self.calls.lock().unwrap().push((url.to_string(), offset));
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let reply = self.scripts.lock().unwrap().get_mut(url).and_then(VecDeque::pop_front);
        match reply {
            None => Err(FetchError::Connect),
            Some(FakeReply::Fail(e)) => Err(e),
            Some(FakeReply::Serve { status, total, html, chunks, then }) => Ok(Opened {
                status,
                total,
                html,
                body: FakeBody { chunks: chunks.into(), then },
            }),
        }
    }
}

/// A `model.onnx` spec whose size and SHA-256 are those of `bytes`.
pub fn spec_for(bytes: &[u8], origin: &str, mirrorable: bool) -> FileSpec {
    FileSpec {
        file: "model.onnx".into(),
        bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        origin: origin.into(),
        mirrorable,
    }
}
