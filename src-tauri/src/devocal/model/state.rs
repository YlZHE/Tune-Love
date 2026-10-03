//! App-level model state: one status per manifest model, at most one download or import job at a
//! time, cancel, delete and import of a local file.
//!
//! A job is registered ("running") synchronously by `begin_*`, before its future is returned, so
//! a second request is refused at once. The future clears the registration when it ends; so does
//! dropping it unpolled.

use std::collections::HashMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::download::{
    download_file, install, io_error, meta_path, meta_tmp_path, part_path, remove_if_present, ModelError, PartMeta, RetryPolicy, Update,
};
use super::fetch::Fetcher;
use super::manifest::{candidates, FileSpec, Manifest, ModelSpec, SourceKind, STEMGENRT_ID};
use super::verify::{file_verified, installed_files, model_dir};

/// Where the origin is pointed when the "break origin" debug switch is on: nothing listens there.
const BLOCKED_ORIGIN: &str = "https://127.0.0.1:9/blocked";

/// Import copies and hashes this much at a time, checking for cancel in between.
const IMPORT_CHUNK: usize = 1 << 20;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelPhase {
    Missing,
    Downloading,
    Verifying,
    Installed,
    Failed,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub id: String,
    pub phase: ModelPhase,
    pub received_bytes: u64,
    pub total_bytes: u64,
    pub source: Option<String>,
    pub path: Option<String>,
    /// Machine code of the last run's failure (`Failed` only).
    pub error: Option<String>,
    /// Machine code of the most recently dropped source in the current or last run: the detail
    /// behind `all_sources_failed`, or why a running download moved on to another source.
    pub source_error: Option<String>,
}

#[derive(Clone)]
pub struct ModelState {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
struct Running {
    id: String,
    cancel: Arc<AtomicBool>,
    phase: ModelPhase,
    /// Bytes of finished files plus bytes of the current file so far.
    received: u64,
    source: Option<String>,
}

struct Inner {
    manifest: &'static Manifest,
    /// `TUNE_LOVE_STEMGENRT_ONNX`; applies to `STEMGENRT_ID` only.
    env_override: Option<PathBuf>,
    models_dir: Mutex<Option<PathBuf>>,
    running: Mutex<Option<Running>>,
    /// Per model: machine code of the last failed run.
    errors: Mutex<HashMap<String, String>>,
    /// Per model: source label of the last successful run in this session.
    sources: Mutex<HashMap<String, String>>,
    /// Per model: machine code of the most recently dropped source.
    source_errors: Mutex<HashMap<String, String>>,
}

/// The registration of the one running job. Dropping it ends the registration (and tells any
/// work still going on its behalf to stop).
struct RunGuard {
    inner: Arc<Inner>,
    id: String,
    cancel: Arc<AtomicBool>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        let mut running = lock(&self.inner.running);
        if running.as_ref().is_some_and(|r| Arc::ptr_eq(&r.cancel, &self.cancel)) {
            *running = None;
        }
    }
}

/// Sets the flag when dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl RunGuard {
    /// Records how the job ended, ends the registration, then reports a successful install.
    fn finish(self, result: Result<Option<String>, ModelError>, on_installed: Box<dyn FnOnce() + Send>) {
        let installed = self.inner.record(&self.id, result);
        drop(self);
        if installed {
            on_installed();
        }
    }
}

impl Inner {
    /// The env override file, if it applies to `id` and exists.
    fn env_file(&self, id: &str) -> Option<&Path> {
        self.env_override.as_deref().filter(|p| id == STEMGENRT_ID && p.is_file())
    }

    /// The checks every job makes first, in this order.
    fn preflight(&self, id: &str) -> Result<(&'static ModelSpec, PathBuf), ModelError> {
        if lock(&self.running).is_some() {
            return Err(ModelError::AlreadyRunning);
        }
        let manifest: &'static Manifest = self.manifest;
        let spec = manifest.model(id).ok_or(ModelError::UnknownModel)?;
        let models_dir = lock(&self.models_dir).clone().ok_or(ModelError::WriteFailed)?;
        Ok((spec, models_dir))
    }

    /// May hash (cached); holds no lock while doing so.
    fn is_installed(&self, spec: &ModelSpec, models_dir: &Path) -> bool {
        self.env_file(&spec.id).is_some() || installed_files(self.manifest, models_dir, &spec.id).is_some()
    }

    /// Takes the job slot (checked again: another job may have started since `preflight`) and
    /// clears what the model's previous run left.
    fn register(self: &Arc<Self>, id: &str, phase: ModelPhase) -> Result<RunGuard, ModelError> {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut running = lock(&self.running);
            if running.is_some() {
                return Err(ModelError::AlreadyRunning);
            }
            *running = Some(Running { id: id.into(), cancel: cancel.clone(), phase, received: 0, source: None });
            // Still under `running`, so no status shows the new run with the old run's detail.
            lock(&self.errors).remove(id);
            lock(&self.source_errors).remove(id);
        }
        Ok(RunGuard { inner: self.clone(), id: id.into(), cancel })
    }

    fn with_running(&self, id: &str, f: impl FnOnce(&mut Running)) {
        if let Some(r) = lock(&self.running).as_mut().filter(|r| r.id == id) {
            f(r);
        }
    }

    /// Progress of a file download; `done` is the size of the model's files already finished.
    fn on_update(&self, id: &str, done: u64, update: Update) {
        match update {
            Update::SourceFailed { error, .. } => {
                lock(&self.source_errors).insert(id.into(), error.code());
            }
            Update::Verifying => self.with_running(id, |r| r.phase = ModelPhase::Verifying),
            Update::Downloading { received, source } => self.with_running(id, |r| {
                r.phase = ModelPhase::Downloading;
                r.received = done + received;
                r.source = Some(source.label());
            }),
        }
    }

    /// Keeps the result of a finished job; true if the model got installed.
    fn record(&self, id: &str, result: Result<Option<String>, ModelError>) -> bool {
        match result {
            Ok(source) => {
                match source {
                    Some(source) => lock(&self.sources).insert(id.into(), source),
                    None => lock(&self.sources).remove(id),
                };
                lock(&self.errors).remove(id);
                lock(&self.source_errors).remove(id);
                true
            }
            Err(ModelError::Cancelled) => false,
            Err(e) => {
                lock(&self.errors).insert(id.into(), e.code());
                false
            }
        }
    }

    fn status(&self, spec: &ModelSpec, running: Option<&Running>, models_dir: Option<&Path>) -> ModelStatus {
        let id = spec.id.as_str();
        let total = spec.files.iter().map(|f| f.bytes).sum();
        let mut s = ModelStatus {
            id: id.into(),
            phase: ModelPhase::Missing,
            received_bytes: 0,
            total_bytes: total,
            source: None,
            path: None,
            error: None,
            source_error: None,
        };
        if let Some(r) = running.filter(|r| r.id == id) {
            s.phase = r.phase;
            s.received_bytes = r.received;
            s.source = r.source.clone();
            s.source_error = lock(&self.source_errors).get(id).cloned();
            return s;
        }
        if let Some(env) = self.env_file(id) {
            s.phase = ModelPhase::Installed;
            s.received_bytes = total;
            s.source = Some("env".into());
            s.path = Some(env.to_string_lossy().into_owned());
            return s;
        }
        if let Some(models_dir) = models_dir {
            if let Some(files) = installed_files(self.manifest, models_dir, id) {
                s.phase = ModelPhase::Installed;
                s.received_bytes = total;
                s.source = lock(&self.sources).get(id).cloned();
                s.path = files.first().map(|p| p.to_string_lossy().into_owned());
                return s;
            }
            s.received_bytes = part_bytes(spec, models_dir);
        }
        s.source_error = lock(&self.source_errors).get(id).cloned();
        if let Some(code) = lock(&self.errors).get(id) {
            s.phase = ModelPhase::Failed;
            s.error = Some(code.clone());
        }
        s
    }
}

/// Total length of the model's `.part` files.
fn part_bytes(spec: &ModelSpec, models_dir: &Path) -> u64 {
    let dir = model_dir(models_dir, &spec.id);
    spec.files
        .iter()
        .filter_map(|f| fs::metadata(part_path(&dir.join(&f.file))).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Bytes a resume would keep: the recorded point of a `.part.json` that belongs to this file.
fn resumable_bytes(file: &FileSpec, final_path: &Path) -> u64 {
    let meta = fs::read(meta_path(final_path)).ok().and_then(|b| serde_json::from_slice::<PartMeta>(&b).ok());
    match meta {
        Some(m) if m.total == file.bytes && m.sha256 == file.sha256 => {
            m.written.min(fs::metadata(part_path(final_path)).map(|p| p.len()).unwrap_or(0))
        }
        _ => 0,
    }
}

/// Like `remove_if_present`, but a protected file or one open elsewhere is `DeleteDenied`.
fn remove_for_delete(path: &Path) -> Result<(), ModelError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION: open in another program.
        Err(e) if e.kind() == ErrorKind::PermissionDenied || (cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33))) => {
            Err(ModelError::DeleteDenied)
        }
        Err(e) => Err(io_error(&e)),
    }
}

/// Downloads the model's files in manifest order; returns the label of the last file's source.
async fn download_all<F: Fetcher>(
    guard: &RunGuard,
    fetcher: &F,
    spec: &ModelSpec,
    models_dir: &Path,
    custom_prefix: Option<&str>,
    policy: &RetryPolicy,
    break_origin: bool,
) -> Result<Option<String>, ModelError> {
    let inner: &Inner = &guard.inner;
    let id = guard.id.as_str();
    let dir = model_dir(models_dir, id);
    let mut done = 0;
    for file in &spec.files {
        let final_path = dir.join(&file.file);
        // An earlier run may have finished some files of a multi-file model.
        if file_verified(&final_path, file) {
            done += file.bytes;
            continue;
        }
        let resumed = resumable_bytes(file, &final_path);
        inner.with_running(id, |r| r.received = done + resumed);
        let mut cands = candidates(file, custom_prefix, &inner.manifest.mirrors);
        if break_origin {
            for c in cands.iter_mut().filter(|c| c.kind == SourceKind::Origin) {
                c.url = BLOCKED_ORIGIN.into();
            }
        }
        let base = done;
        let progress = move |u: Update| inner.on_update(id, base, u);
        download_file(fetcher, file, &final_path, &cands, policy, &guard.cancel, &progress).await?;
        done += file.bytes;
    }
    Ok(lock(&inner.running).as_ref().filter(|r| r.id == id).and_then(|r| r.source.clone()))
}

/// Copies `source` into `<file>.part` while hashing it, then installs it if size and hash match.
/// Blocking. A `.part` left by a failure here is removed.
fn import_file(guard: &RunGuard, file: &FileSpec, source: &Path, final_path: &Path) -> Result<(), ModelError> {
    let size = fs::metadata(source).ok().filter(|m| m.is_file()).map(|m| m.len());
    if size != Some(file.bytes) {
        return Err(ModelError::ImportMismatch);
    }
    if let Some(dir) = final_path.parent() {
        fs::create_dir_all(dir).map_err(|e| io_error(&e))?;
    }
    // Opened before anything is removed: a source that cannot be read leaves a partial download alone.
    let input = File::open(source).map_err(|_| ModelError::ImportMismatch)?;
    let part = part_path(final_path);
    // An import replaces whatever download of this file was in progress.
    remove_if_present(&part)?;
    remove_if_present(&meta_path(final_path))?;
    let copied = copy_hashing(input, &part, file.bytes, &guard.cancel, |n| {
        guard.inner.with_running(&guard.id, |r| r.received = n);
    });
    let result = match copied {
        Ok(sha256) if sha256 == file.sha256 => install(&part, final_path, &sha256),
        Ok(_) => Err(ModelError::ImportMismatch),
        Err(e) => Err(e),
    };
    if result.is_err() {
        if let Err(e) = fs::remove_file(&part) {
            if e.kind() != ErrorKind::NotFound {
                eprintln!("model import: could not remove {}: {e}", part.display());
            }
        }
    }
    result
}

/// Returns the SHA-256 of what was copied. A source that fails to read, or whose length turns out
/// not to be `expected` after all, is not the model (`ImportMismatch`).
fn copy_hashing(mut input: File, part: &Path, expected: u64, cancel: &AtomicBool, progress: impl Fn(u64)) -> Result<String, ModelError> {
    let mut output = File::create(part).map_err(|e| io_error(&e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; IMPORT_CHUNK];
    let mut copied = 0u64;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(ModelError::Cancelled);
        }
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(ModelError::ImportMismatch),
        };
        copied += n as u64;
        if copied > expected {
            return Err(ModelError::ImportMismatch);
        }
        output.write_all(&buf[..n]).map_err(|e| io_error(&e))?;
        hasher.update(&buf[..n]);
        progress(copied);
    }
    if copied != expected {
        return Err(ModelError::ImportMismatch);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

impl ModelState {
    pub fn new(manifest: &'static Manifest, env_override: Option<PathBuf>) -> Self {
        ModelState {
            inner: Arc::new(Inner {
                manifest,
                env_override,
                models_dir: Mutex::new(None),
                running: Mutex::new(None),
                errors: Mutex::default(),
                sources: Mutex::default(),
                source_errors: Mutex::default(),
            }),
        }
    }

    pub fn set_models_dir(&self, dir: PathBuf) {
        *lock(&self.inner.models_dir) = Some(dir);
    }

    /// One status per manifest model, in manifest order. May hash files (cached), so call it
    /// off the UI thread.
    pub fn statuses(&self) -> Vec<ModelStatus> {
        let running = lock(&self.inner.running).clone();
        let models_dir = lock(&self.inner.models_dir).clone();
        self.inner
            .manifest
            .models
            .iter()
            .map(|spec| self.inner.status(spec, running.as_ref(), models_dir.as_deref()))
            .collect()
    }

    /// Registers a download of `id` and returns the future that performs it. For a model that is
    /// already installed the future completes at once, without calling `on_installed`.
    pub fn begin_download<F: Fetcher>(
        &self,
        fetcher: Arc<F>,
        id: &str,
        custom_prefix: Option<&str>,
        policy: RetryPolicy,
        break_origin: bool,
        on_installed: Box<dyn FnOnce() + Send>,
    ) -> Result<impl Future<Output = ()> + Send + 'static, ModelError> {
        let (spec, models_dir) = self.inner.preflight(id)?;
        let guard = if self.inner.is_installed(spec, &models_dir) {
            None
        } else {
            Some(self.inner.register(id, ModelPhase::Downloading)?)
        };
        let custom_prefix = custom_prefix.map(str::to_owned);
        Ok(async move {
            let Some(guard) = guard else { return };
            let result =
                download_all(&guard, &*fetcher, spec, &models_dir, custom_prefix.as_deref(), &policy, break_origin)
                    .await;
            guard.finish(result, on_installed);
        })
    }

    /// Registers an import of `source_file` as the single-file model `id` and returns the future
    /// that performs it. For a model that is already installed the future completes at once,
    /// without calling `on_installed`.
    pub fn begin_import(
        &self,
        id: &str,
        source_file: PathBuf,
        on_installed: Box<dyn FnOnce() + Send>,
    ) -> Result<impl Future<Output = ()> + Send + 'static, ModelError> {
        let (spec, models_dir) = self.inner.preflight(id)?;
        let [file] = spec.files.as_slice() else {
            return Err(ModelError::ImportUnsupported);
        };
        let guard = if self.inner.is_installed(spec, &models_dir) {
            None
        } else {
            Some(self.inner.register(id, ModelPhase::Verifying)?)
        };
        let final_path = model_dir(&models_dir, id).join(&file.file);
        Ok(async move {
            let Some(guard) = guard else { return };
            // If this future is dropped mid-copy, the copy is told to stop; the guard (moved into
            // the closure) ends the registration once it has.
            let _stop_on_drop = CancelOnDrop(guard.cancel.clone());
            // Copying and hashing a large file is blocking work.
            let joined = tokio::task::spawn_blocking(move || {
                let result = import_file(&guard, file, &source_file, &final_path);
                (guard, result)
            })
            .await;
            let (guard, result) = match joined {
                Ok(done) => done,
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                // The runtime is shutting down; dropping the closure dropped the guard.
                Err(_) => return,
            };
            guard.finish(result.map(|()| Some("import".into())), on_installed);
        })
    }

    /// Asks the running job for `id` to stop; does nothing if `id` is not the one running.
    pub fn cancel(&self, id: &str) {
        if let Some(r) = lock(&self.inner.running).as_ref().filter(|r| r.id == id) {
            r.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Removes the model's manifest files and their `.part`/`.part.json` (nothing else), then its
    /// directory if that left it empty, and forgets the model's recorded error and source.
    pub fn delete(&self, id: &str) -> Result<(), ModelError> {
        let spec = self.inner.manifest.model(id).ok_or(ModelError::UnknownModel)?;
        // Held throughout, so no job can start on these files meanwhile.
        let running = lock(&self.inner.running);
        if running.as_ref().is_some_and(|r| r.id == id) {
            return Err(ModelError::AlreadyRunning);
        }
        let models_dir = lock(&self.inner.models_dir).clone().ok_or(ModelError::WriteFailed)?;
        let dir = model_dir(&models_dir, id);
        let mut first_error = None;
        for f in &spec.files {
            let path = dir.join(&f.file);
            for p in [part_path(&path), meta_path(&path), meta_tmp_path(&path), path] {
                if let Err(e) = remove_for_delete(&p) {
                    first_error.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }
        // Fails (as intended) unless the directory is now empty.
        let _ = fs::remove_dir(&dir);
        lock(&self.inner.errors).remove(id);
        lock(&self.inner.sources).remove(id);
        lock(&self.inner.source_errors).remove(id);
        drop(running);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devocal::model::download::{meta_path, meta_tmp_path, part_path};
    use crate::devocal::model::fake::{FakeFetcher, FakeReply};
    use crate::devocal::model::fetch::FetchError;
    use crate::devocal::model::manifest::STEMGENRT_ID;
    use crate::devocal::tests::temp_dir;
    use sha2::{Digest, Sha256};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const A_ORIGIN: &str = "https://github.com/o/r/raw/c/a.bin";
    const GHFAST: &str = "https://ghfast.top/";
    const GHPROXY: &str = "https://ghproxy.net/";
    const BLOCKED: &str = "https://127.0.0.1:9/blocked";
    const B1_ORIGIN: &str = "https://example.com/b1.bin";
    const B2_ORIGIN: &str = "https://example.com/b2.bin";
    const POLICY: RetryPolicy = RetryPolicy { retries: 2, backoff: Duration::ZERO };

    fn bytes(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    fn file_json(name: &str, data: &[u8], origin: &str, mirrorable: bool) -> serde_json::Value {
        serde_json::json!({
            "file": name, "bytes": data.len(), "sha256": format!("{:x}", Sha256::digest(data)),
            "origin": origin, "mirrorable": mirrorable
        })
    }

    fn model_json(id: &str, files: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": id, "tier": "realtime", "files": files,
            "sampleRate": 44100, "latencyMs": 5.8, "runtime": "cpu",
            "license": { "code": "MIT", "weights": "pending", "trainingData": [] },
            "source": "https://example.com"
        })
    }

    fn leak(models: Vec<serde_json::Value>) -> &'static Manifest {
        let json = serde_json::json!({ "version": 1, "mirrors": [GHFAST, GHPROXY], "models": models });
        Box::leak(Box::new(Manifest::parse(&json.to_string()).unwrap()))
    }

    fn mirror(prefix: &str, origin: &str) -> String {
        format!("{prefix}{origin}")
    }

    fn ok200(data: &[u8]) -> FakeReply {
        FakeReply::Serve {
            status: 200,
            total: Some(data.len() as u64),
            html: false,
            chunks: data.chunks(4096).map(<[u8]>::to_vec).collect(),
            then: None,
        }
    }

    fn html_page() -> FakeReply {
        FakeReply::Serve { status: 200, total: None, html: true, chunks: vec![b"<html>".to_vec()], then: None }
    }

    /// `a`: one mirrorable file. `b`: two files on a non-GitHub host (`mirrorable: false`).
    struct Fx {
        dir: PathBuf,
        state: ModelState,
        a: Vec<u8>,
        b1: Vec<u8>,
        b2: Vec<u8>,
    }

    fn fx(tag: &str) -> Fx {
        let (a, b1, b2) = (bytes(64 * 1024, 0x9E37_79B9), bytes(8 * 1024, 7), bytes(12 * 1024, 11));
        let manifest = leak(vec![
            model_json("a", vec![file_json("a.bin", &a, A_ORIGIN, true)]),
            model_json("b", vec![
                file_json("b1.bin", &b1, B1_ORIGIN, false),
                file_json("b2.bin", &b2, B2_ORIGIN, false),
            ]),
        ]);
        let dir = temp_dir(tag);
        let state = ModelState::new(manifest, None);
        state.set_models_dir(dir.clone());
        Fx { dir, state, a, b1, b2 }
    }

    impl Fx {
        fn status(&self, id: &str) -> ModelStatus {
            self.state.statuses().into_iter().find(|s| s.id == id).unwrap()
        }

        fn a_file(&self) -> PathBuf {
            self.dir.join("a").join("a.bin")
        }
    }

    /// An `on_installed` that counts its calls.
    fn counter() -> (Arc<AtomicUsize>, Box<dyn FnOnce() + Send>) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        (n, Box::new(move || {
            m.fetch_add(1, Ordering::SeqCst);
        }))
    }

    fn noop() -> Box<dyn FnOnce() + Send> {
        Box::new(|| {})
    }

    /// Yields until `cond` holds (the spawned job runs on the same test thread meanwhile).
    async fn until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[tokio::test]
    async fn statuses_list_every_manifest_model_in_order() {
        let t = fx("st-list");
        let s = t.state.statuses();
        assert_eq!(s.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(s.iter().all(|s| s.phase == ModelPhase::Missing));
        assert_eq!(s[0].total_bytes, t.a.len() as u64);
        assert_eq!(s[1].total_bytes, (t.b1.len() + t.b2.len()) as u64);
        assert!(s.iter().all(|s| s.received_bytes == 0 && s.error.is_none() && s.path.is_none()));
    }

    #[tokio::test]
    async fn download_installs_and_calls_on_installed_once() {
        let t = fx("st-dl");
        let f = Arc::new(FakeFetcher::new());
        f.script(A_ORIGIN, vec![ok200(&t.a)]);
        let (n, cb) = counter();
        t.state.begin_download(f.clone(), "a", None, POLICY, false, cb).unwrap().await;
        let s = t.status("a");
        assert_eq!(s.phase, ModelPhase::Installed);
        assert_eq!(s.source.as_deref(), Some("origin"));
        assert!(Path::new(s.path.as_deref().unwrap()).ends_with(Path::new("a").join("a.bin")));
        assert_eq!((s.error, s.source_error), (None, None));
        assert_eq!(std::fs::read(t.a_file()).unwrap(), t.a);
        assert_eq!(n.load(Ordering::SeqCst), 1);
        // Already installed: the next request completes at once, without touching the network.
        let (n2, cb2) = counter();
        t.state.begin_download(f.clone(), "a", None, POLICY, false, cb2).unwrap().await;
        assert_eq!(f.calls().len(), 1);
        assert_eq!(n2.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failure_records_code_and_skips_on_installed() {
        let t = fx("st-fail");
        let f = Arc::new(FakeFetcher::new()); // every open fails with Connect
        let (n, cb) = counter();
        t.state.begin_download(f, "a", None, POLICY, false, cb).unwrap().await;
        let s = t.status("a");
        assert_eq!(s.phase, ModelPhase::Failed);
        assert_eq!(s.error.as_deref(), Some("all_sources_failed"));
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(!t.a_file().exists());
    }

    #[tokio::test]
    async fn retry_clears_the_previous_error() {
        let t = fx("st-retry");
        let f = Arc::new(FakeFetcher::new());
        t.state.begin_download(f.clone(), "a", None, POLICY, false, noop()).unwrap().await;
        assert_eq!(t.status("a").phase, ModelPhase::Failed);
        f.script(A_ORIGIN, vec![ok200(&t.a)]);
        t.state.begin_download(f, "a", None, POLICY, false, noop()).unwrap().await;
        let s = t.status("a");
        assert_eq!(s.phase, ModelPhase::Installed);
        assert_eq!((s.error, s.source_error), (None, None));
    }

    #[tokio::test]
    async fn second_task_is_already_running() {
        let t = fx("st-busy");
        let f = Arc::new(FakeFetcher::new());
        f.script(A_ORIGIN, vec![ok200(&t.a)]);
        let gate = f.gate();
        let job = tokio::spawn(t.state.begin_download(f.clone(), "a", None, POLICY, false, noop()).unwrap());
        until("the gated open", || !f.calls().is_empty()).await;
        let r = t.state.begin_download(f.clone(), "b", None, POLICY, false, noop());
        assert_eq!(r.err(), Some(ModelError::AlreadyRunning));
        let r = t.state.begin_import("a", t.dir.join("x.bin"), noop());
        assert_eq!(r.err(), Some(ModelError::AlreadyRunning));
        assert_eq!(t.status("a").phase, ModelPhase::Downloading);
        assert_eq!(t.status("b").phase, ModelPhase::Missing);
        gate.notify_one();
        job.await.unwrap();
        assert_eq!(t.status("a").phase, ModelPhase::Installed);
        // The slot is free again.
        f.script(B1_ORIGIN, vec![ok200(&t.b1)]);
        f.script(B2_ORIGIN, vec![ok200(&t.b2)]);
        assert!(t.state.begin_download(f, "b", None, POLICY, false, noop()).is_ok());
    }

    #[tokio::test]
    async fn cancel_returns_to_missing_with_partial_bytes() {
        let t = fx("st-cancel");
        let f = Arc::new(FakeFetcher::new());
        // Two chunks, then the connection drops and the retry waits a long backoff.
        f.script(A_ORIGIN, vec![FakeReply::Serve {
            status: 200,
            total: Some(t.a.len() as u64),
            html: false,
            chunks: vec![t.a[..4096].to_vec(), t.a[4096..8192].to_vec()],
            then: Some(FetchError::Network("reset".into())),
        }]);
        let slow = RetryPolicy { retries: 2, backoff: Duration::from_secs(30) };
        let (n, cb) = counter();
        let job = tokio::spawn(t.state.begin_download(f.clone(), "a", None, slow, false, cb).unwrap());
        until("two chunks", || t.status("a").received_bytes == 8192).await;
        let s = t.status("a");
        assert_eq!((s.phase, s.source.as_deref()), (ModelPhase::Downloading, Some("origin")));
        t.state.cancel("b"); // not the running one: no effect
        t.state.cancel("a");
        tokio::time::timeout(Duration::from_secs(5), job).await.expect("cancel ignored").unwrap();
        let s = t.status("a");
        assert_eq!(s.phase, ModelPhase::Missing);
        assert_eq!(s.received_bytes, 8192);
        assert_eq!((s.error, s.source_error), (None, None));
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(part_path(&t.a_file()).exists() && meta_path(&t.a_file()).exists(), "cancel keeps the .part");
        t.state.cancel("a"); // nothing running: no effect
    }

    #[tokio::test]
    async fn multi_file_model_installs_all_files() {
        let t = fx("st-multi");
        let f = Arc::new(FakeFetcher::new());
        f.script(B1_ORIGIN, vec![ok200(&t.b1)]);
        f.script(B2_ORIGIN, vec![ok200(&t.b2)]);
        let (n, cb) = counter();
        t.state.begin_download(f, "b", None, POLICY, false, cb).unwrap().await;
        assert_eq!(std::fs::read(t.dir.join("b").join("b1.bin")).unwrap(), t.b1);
        assert_eq!(std::fs::read(t.dir.join("b").join("b2.bin")).unwrap(), t.b2);
        let s = t.status("b");
        assert_eq!(s.phase, ModelPhase::Installed);
        assert!(Path::new(s.path.as_deref().unwrap()).ends_with(Path::new("b").join("b1.bin")));
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multi_file_progress_adds_finished_files() {
        let t = fx("st-multi-progress");
        let f = Arc::new(FakeFetcher::new());
        f.script(B1_ORIGIN, vec![ok200(&t.b1)]);
        f.script(B2_ORIGIN, vec![ok200(&t.b2)]);
        let gate_b1 = f.gate();
        let job = tokio::spawn(t.state.begin_download(f.clone(), "b", None, POLICY, false, noop()).unwrap());
        until("b1 open", || f.calls().len() == 1).await;
        let gate_b2 = f.gate();
        gate_b1.notify_one();
        until("b2 open", || f.calls().len() == 2).await;
        let s = t.status("b");
        assert_eq!(s.received_bytes, t.b1.len() as u64, "first file done, second not started");
        assert_eq!(s.total_bytes, (t.b1.len() + t.b2.len()) as u64);
        gate_b2.notify_one();
        job.await.unwrap();
        assert_eq!(t.status("b").phase, ModelPhase::Installed);
    }

    #[tokio::test]
    async fn break_origin_falls_through_to_mirror() {
        let t = fx("st-break");
        let f = Arc::new(FakeFetcher::new());
        f.script(A_ORIGIN, vec![ok200(&t.a)]);
        f.script(&mirror(GHFAST, A_ORIGIN), vec![ok200(&t.a)]);
        t.state.begin_download(f.clone(), "a", None, POLICY, true, noop()).unwrap().await;
        let calls = f.calls();
        assert_eq!(calls[0].0, BLOCKED);
        assert!(calls.iter().all(|(url, _)| url != A_ORIGIN));
        let s = t.status("a");
        assert_eq!(s.phase, ModelPhase::Installed);
        assert_eq!(s.source.as_deref(), Some("mirror:ghfast.top"));
    }

    #[tokio::test]
    async fn custom_prefix_is_tried_after_origin() {
        let t = fx("st-custom");
        let f = Arc::new(FakeFetcher::new());
        f.script(A_ORIGIN, vec![FakeReply::Fail(FetchError::Status(404))]);
        f.script(&mirror("https://my.proxy/", A_ORIGIN), vec![ok200(&t.a)]);
        t.state.begin_download(f.clone(), "a", Some("https://my.proxy/"), POLICY, false, noop()).unwrap().await;
        let s = t.status("a");
        assert_eq!((s.phase, s.source.as_deref()), (ModelPhase::Installed, Some("custom")));
        assert_eq!(s.source_error, None, "cleared on success");
    }

    #[tokio::test]
    async fn source_error_keeps_the_last_dropped_source() {
        let t = fx("st-source-error");
        let f = Arc::new(FakeFetcher::new());
        let (m1, m2) = (mirror(GHFAST, A_ORIGIN), mirror(GHPROXY, A_ORIGIN));
        f.script(A_ORIGIN, vec![FakeReply::Fail(FetchError::Status(404))]);
        f.script(&m1, vec![html_page()]);
        // m2 has no script: every open fails with Connect.
        let gate_origin = f.gate();
        let job = tokio::spawn(t.state.begin_download(f.clone(), "a", None, POLICY, false, noop()).unwrap());
        until("origin open", || f.calls().len() == 1).await;
        let gate_m1 = f.gate();
        gate_origin.notify_one();
        until("mirror 1 open", || f.calls().len() == 2).await;
        assert_eq!(t.status("a").source_error.as_deref(), Some("http_status:404"));
        let gate_m2 = f.gate();
        gate_m1.notify_one();
        until("mirror 2 open", || f.calls().len() == 3).await;
        let s = t.status("a");
        assert_eq!(s.phase, ModelPhase::Downloading);
        assert_eq!(s.source_error.as_deref(), Some("source_html"));
        assert_eq!(s.error, None);
        gate_m2.notify_one();
        job.await.unwrap();
        let s = t.status("a");
        assert_eq!(s.phase, ModelPhase::Failed);
        assert_eq!(s.error.as_deref(), Some("all_sources_failed"));
        assert_eq!(s.source_error.as_deref(), Some("network_unreachable"));
        assert_eq!(f.calls().iter().filter(|(url, _)| *url == m2).count(), 3, "m2 retried, then dropped");
        // A new run starts without the old detail.
        let gate = f.gate();
        let job = tokio::spawn(t.state.begin_download(f.clone(), "a", None, POLICY, false, noop()).unwrap());
        until("new run open", || f.calls().len() == 6).await;
        let s = t.status("a");
        assert_eq!((s.phase, s.error, s.source_error), (ModelPhase::Downloading, None, None));
        t.state.cancel("a");
        gate.notify_one();
        job.await.unwrap();
    }

    #[tokio::test]
    async fn source_error_survives_cancel() {
        let t = fx("st-source-error-cancel");
        let f = Arc::new(FakeFetcher::new());
        f.script(A_ORIGIN, vec![FakeReply::Fail(FetchError::Status(404))]);
        f.script(&mirror(GHFAST, A_ORIGIN), vec![html_page()]);
        let gate_origin = f.gate();
        let job = tokio::spawn(t.state.begin_download(f.clone(), "a", None, POLICY, false, noop()).unwrap());
        until("origin open", || f.calls().len() == 1).await;
        let gate_m1 = f.gate();
        gate_origin.notify_one();
        until("mirror 1 open", || f.calls().len() == 2).await;
        let gate_m2 = f.gate();
        gate_m1.notify_one();
        until("mirror 2 open", || f.calls().len() == 3).await;
        t.state.cancel("a");
        gate_m2.notify_one();
        job.await.unwrap();
        let s = t.status("a");
        assert_eq!((s.phase, s.error), (ModelPhase::Missing, None));
        assert_eq!(s.source_error.as_deref(), Some("source_html"));
    }

    #[test]
    fn delete_removes_only_manifest_files_and_parts() {
        let t = fx("st-delete");
        let dir = t.dir.join("a");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(t.a_file(), &t.a).unwrap();
        std::fs::write(part_path(&t.a_file()), b"part").unwrap();
        std::fs::write(meta_path(&t.a_file()), b"{}").unwrap();
        // Left by a crash between writing the next .part.json and renaming it.
        std::fs::write(meta_tmp_path(&t.a_file()), b"{").unwrap();
        std::fs::write(dir.join("notes.txt"), b"mine").unwrap();
        assert_eq!(t.status("a").phase, ModelPhase::Installed);
        assert_eq!(t.state.delete("a"), Ok(()));
        assert!(!t.a_file().exists() && !part_path(&t.a_file()).exists() && !meta_path(&t.a_file()).exists());
        assert!(!meta_tmp_path(&t.a_file()).exists());
        assert_eq!(std::fs::read(dir.join("notes.txt")).unwrap(), b"mine");
        assert_eq!(t.status("a").phase, ModelPhase::Missing);

        // A directory left empty is removed; deleting again is fine.
        let bdir = t.dir.join("b");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(bdir.join("b1.bin"), &t.b1).unwrap();
        std::fs::write(part_path(&bdir.join("b2.bin")), b"half").unwrap();
        assert_eq!(t.state.delete("b"), Ok(()));
        assert!(!bdir.exists());
        assert_eq!(t.state.delete("b"), Ok(()));
        assert_eq!(t.status("b").received_bytes, 0);
    }

    #[tokio::test]
    async fn delete_clears_the_recorded_error() {
        let t = fx("st-delete-error");
        let f = Arc::new(FakeFetcher::new());
        t.state.begin_download(f, "a", None, POLICY, false, noop()).unwrap().await;
        assert_eq!(t.status("a").phase, ModelPhase::Failed);
        assert_eq!(t.state.delete("a"), Ok(()));
        let s = t.status("a");
        assert_eq!((s.phase, s.error, s.source_error), (ModelPhase::Missing, None, None));
    }

    #[tokio::test]
    async fn delete_while_running_is_already_running() {
        let t = fx("st-delete-busy");
        let f = Arc::new(FakeFetcher::new());
        let gate = f.gate();
        let job = tokio::spawn(t.state.begin_download(f.clone(), "a", None, POLICY, false, noop()).unwrap());
        until("the gated open", || !f.calls().is_empty()).await;
        assert_eq!(t.state.delete("a"), Err(ModelError::AlreadyRunning));
        assert_eq!(t.state.delete("b"), Ok(()), "another model is not busy");
        t.state.cancel("a");
        gate.notify_one();
        job.await.unwrap();
        assert_eq!(t.state.delete("a"), Ok(()));
    }

    #[cfg(windows)]
    #[test]
    fn delete_of_a_file_in_use_is_delete_denied() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = fx("st-delete-locked");
        std::fs::create_dir_all(t.dir.join("a")).unwrap();
        std::fs::write(t.a_file(), &t.a).unwrap();
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(t.a_file()).unwrap();
        assert_eq!(t.state.delete("a"), Err(ModelError::DeleteDenied));
        drop(lock);
        assert!(t.a_file().exists());
    }

    #[test]
    fn env_override_reports_installed_from_env_for_stemgenrt_only() {
        let content = bytes(1024, 3);
        let manifest = leak(vec![
            model_json(STEMGENRT_ID, vec![file_json("model.onnx", &content, A_ORIGIN, true)]),
            model_json("a", vec![file_json("a.bin", &content, A_ORIGIN, true)]),
        ]);
        let dir = temp_dir("st-env");
        let env_file = dir.join("elsewhere.onnx");
        std::fs::write(&env_file, b"not even the right size").unwrap();
        let state = ModelState::new(manifest, Some(env_file.clone()));
        let s = state.statuses();
        assert_eq!(s[0].id, STEMGENRT_ID);
        assert_eq!(s[0].phase, ModelPhase::Installed, "even without a models dir, and unverified");
        assert_eq!(s[0].source.as_deref(), Some("env"));
        assert_eq!(s[0].path.as_deref(), Some(env_file.to_string_lossy().as_ref()));
        assert_eq!(s[1].phase, ModelPhase::Missing);
        state.set_models_dir(dir.join("models"));
        assert_eq!(state.statuses()[1].phase, ModelPhase::Missing);

        // A set but missing env file does not count.
        let state = ModelState::new(manifest, Some(dir.join("missing.onnx")));
        state.set_models_dir(dir.join("models"));
        assert_eq!(state.statuses()[0].phase, ModelPhase::Missing);
    }

    #[tokio::test]
    async fn import_verifies_then_installs_with_import_source() {
        let t = fx("st-import");
        let src = t.dir.join("picked.onnx");
        std::fs::write(&src, &t.a).unwrap();
        // A stale download of the same model is replaced.
        std::fs::create_dir_all(t.dir.join("a")).unwrap();
        std::fs::write(part_path(&t.a_file()), b"stale").unwrap();
        std::fs::write(meta_path(&t.a_file()), b"{}").unwrap();
        let (n, cb) = counter();
        t.state.begin_import("a", src.clone(), cb).unwrap().await;
        let s = t.status("a");
        assert_eq!(s.phase, ModelPhase::Installed);
        assert_eq!(s.source.as_deref(), Some("import"));
        assert_eq!(std::fs::read(t.a_file()).unwrap(), t.a);
        assert!(!part_path(&t.a_file()).exists() && !meta_path(&t.a_file()).exists());
        assert_eq!(std::fs::read(&src).unwrap(), t.a, "the picked file is copied, not moved");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn import_mismatch_leaves_no_part_and_reports_import_mismatch() {
        let t = fx("st-import-bad");
        let src = t.dir.join("picked.onnx");
        let (n, cb) = counter();

        // Wrong size: rejected before copying.
        std::fs::write(&src, &t.a[1..]).unwrap();
        t.state.begin_import("a", src.clone(), cb).unwrap().await;
        let s = t.status("a");
        assert_eq!((s.phase, s.error.as_deref()), (ModelPhase::Failed, Some("import_mismatch")));
        assert!(!part_path(&t.a_file()).exists());

        // Right size, wrong content: copied, hashed, then removed.
        let mut bad = t.a.clone();
        bad[100] ^= 1;
        std::fs::write(&src, &bad).unwrap();
        let (n2, cb2) = counter();
        t.state.begin_import("a", src.clone(), cb2).unwrap().await;
        let s = t.status("a");
        assert_eq!((s.phase, s.error.as_deref()), (ModelPhase::Failed, Some("import_mismatch")));
        assert_eq!(s.received_bytes, 0);
        assert!(!part_path(&t.a_file()).exists() && !t.a_file().exists());

        // A missing file is not the model either.
        t.state.begin_import("a", t.dir.join("gone.onnx"), noop()).unwrap().await;
        assert_eq!(t.status("a").error.as_deref(), Some("import_mismatch"));
        assert_eq!(n.load(Ordering::SeqCst) + n2.load(Ordering::SeqCst), 0);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn import_of_an_unopenable_source_keeps_the_partial_download() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = fx("st-import-locked");
        let src = t.dir.join("picked.onnx");
        std::fs::write(&src, &t.a).unwrap();
        std::fs::create_dir_all(t.dir.join("a")).unwrap();
        std::fs::write(part_path(&t.a_file()), b"partial").unwrap();
        std::fs::write(meta_path(&t.a_file()), b"{\"meta\":1}").unwrap();
        // Right size, but another program holds it open exclusively.
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&src).unwrap();
        t.state.begin_import("a", src.clone(), noop()).unwrap().await;
        drop(lock);
        assert_eq!(t.status("a").error.as_deref(), Some("import_mismatch"));
        assert_eq!(std::fs::read(part_path(&t.a_file())).unwrap(), b"partial");
        assert_eq!(std::fs::read(meta_path(&t.a_file())).unwrap(), b"{\"meta\":1}");
    }

    /// A one-model manifest (`big`, file `big.bin`) of `len` bytes, plus a matching source file
    /// to import (one import chunk is 1 MiB).
    struct Big {
        dir: PathBuf,
        state: ModelState,
        src: PathBuf,
        file: PathBuf,
    }

    fn big(tag: &str, len: usize) -> Big {
        let content = vec![0x5A_u8; len];
        let manifest = leak(vec![model_json("big", vec![file_json("big.bin", &content, A_ORIGIN, true)])]);
        let dir = temp_dir(tag);
        let src = dir.join("picked.onnx");
        std::fs::write(&src, &content).unwrap();
        let state = ModelState::new(manifest, None);
        state.set_models_dir(dir.join("models"));
        let file = dir.join("models").join("big").join("big.bin");
        Big { dir, state, src, file }
    }

    impl Big {
        fn status(&self) -> ModelStatus {
            self.state.statuses().remove(0)
        }
    }

    #[tokio::test]
    async fn cancelling_an_import_returns_to_missing_and_removes_the_part() {
        let t = big("st-import-cancel", 1 << 20);
        let (n, cb) = counter();
        let job = t.state.begin_import("big", t.src.clone(), cb).unwrap();
        // Registered synchronously, so the cancel lands before the copy's first check.
        assert_eq!(t.status().phase, ModelPhase::Verifying);
        t.state.cancel("big");
        let job = tokio::spawn(job);
        tokio::time::timeout(Duration::from_secs(10), job).await.expect("cancel ignored").unwrap();
        let s = t.status();
        assert_eq!((s.phase, s.error, s.received_bytes), (ModelPhase::Missing, None, 0));
        assert!(!part_path(&t.file).exists() && !t.file.exists());
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(t.src.exists());
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[tokio::test]
    async fn dropping_an_import_mid_copy_stops_it() {
        // 64 MiB: the copy takes far longer than the abort below.
        let t = big("st-import-drop", 64 << 20);
        let (n, cb) = counter();
        let job = tokio::spawn(t.state.begin_import("big", t.src.clone(), cb).unwrap());
        // On the current-thread test runtime this polls the job once, which starts the copy.
        tokio::task::yield_now().await;
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        // The slot stays taken until the copy has actually stopped.
        until("the copy to stop", || t.status().phase != ModelPhase::Verifying).await;
        let s = t.status();
        assert_eq!((s.phase, s.error), (ModelPhase::Missing, None), "nothing recorded");
        assert!(!t.file.exists(), "not installed");
        assert!(!part_path(&t.file).exists());
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(t.state.begin_import("big", t.src.clone(), noop()).is_ok(), "slot free again");
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[tokio::test]
    async fn import_of_an_installed_model_completes_at_once() {
        let t = fx("st-import-installed");
        std::fs::create_dir_all(t.dir.join("a")).unwrap();
        std::fs::write(t.a_file(), &t.a).unwrap();
        let (n, cb) = counter();
        // The picked file is not even looked at.
        t.state.begin_import("a", t.dir.join("does-not-exist.onnx"), cb).unwrap().await;
        let s = t.status("a");
        assert_eq!((s.phase, s.error), (ModelPhase::Installed, None));
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(t.a_file()).unwrap(), t.a);
    }

    #[tokio::test]
    async fn download_of_stemgenrt_with_env_override_completes_at_once() {
        let content = bytes(1024, 5);
        let manifest = leak(vec![model_json(STEMGENRT_ID, vec![file_json("model.onnx", &content, A_ORIGIN, true)])]);
        let dir = temp_dir("st-env-download");
        let env_file = dir.join("elsewhere.onnx");
        std::fs::write(&env_file, b"anything").unwrap();
        let state = ModelState::new(manifest, Some(env_file));
        state.set_models_dir(dir.join("models"));
        let f = Arc::new(FakeFetcher::new());
        let (n, cb) = counter();
        state.begin_download(f.clone(), STEMGENRT_ID, None, POLICY, false, cb).unwrap().await;
        assert!(f.calls().is_empty());
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(!dir.join("models").exists(), "nothing written");
        let s = state.statuses().remove(0);
        assert_eq!((s.phase, s.source.as_deref()), (ModelPhase::Installed, Some("env")));
    }

    #[test]
    fn import_of_multi_file_model_is_unsupported() {
        let t = fx("st-import-multi");
        let src = t.dir.join("picked.bin");
        std::fs::write(&src, &t.b1).unwrap();
        assert_eq!(t.state.begin_import("b", src, noop()).err(), Some(ModelError::ImportUnsupported));
        assert_eq!(t.status("b").phase, ModelPhase::Missing);
    }

    #[test]
    fn unknown_id_is_unknown_model() {
        let t = fx("st-unknown");
        let f = Arc::new(FakeFetcher::new());
        let r = t.state.begin_download(f, "zzz", None, POLICY, false, noop());
        assert_eq!(r.err(), Some(ModelError::UnknownModel));
        assert_eq!(t.state.begin_import("zzz", t.dir.join("x"), noop()).err(), Some(ModelError::UnknownModel));
        assert_eq!(t.state.delete("zzz"), Err(ModelError::UnknownModel));
        t.state.cancel("zzz");
    }

    #[test]
    fn without_a_models_dir_jobs_are_write_failed() {
        let t = fx("st-no-dir");
        let state = ModelState::new(t.state.inner.manifest, None);
        let f = Arc::new(FakeFetcher::new());
        assert_eq!(state.begin_download(f, "a", None, POLICY, false, noop()).err(), Some(ModelError::WriteFailed));
        assert_eq!(state.begin_import("a", t.dir.join("x"), noop()).err(), Some(ModelError::WriteFailed));
        assert!(state.statuses().iter().all(|s| s.phase == ModelPhase::Missing && s.received_bytes == 0));
    }

    #[test]
    fn dropping_an_unpolled_job_frees_the_slot() {
        let t = fx("st-drop");
        let f = Arc::new(FakeFetcher::new());
        let job = t.state.begin_download(f.clone(), "a", None, POLICY, false, noop()).unwrap();
        assert_eq!(t.status("a").phase, ModelPhase::Downloading, "registered before the future runs");
        drop(job);
        assert_eq!(t.status("a").phase, ModelPhase::Missing);
        assert!(t.state.begin_download(f, "a", None, POLICY, false, noop()).is_ok());
    }

    #[test]
    fn status_serialises_camel_case() {
        let v = serde_json::to_value(ModelStatus {
            id: "a".into(),
            phase: ModelPhase::Downloading,
            received_bytes: 1,
            total_bytes: 2,
            source: Some("mirror:ghfast.top".into()),
            path: None,
            error: None,
            source_error: None,
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "id": "a", "phase": "downloading", "receivedBytes": 1, "totalBytes": 2,
            "source": "mirror:ghfast.top", "path": null, "error": null, "sourceError": null }));
    }

    #[test]
    fn job_futures_are_send_and_static() {
        fn assert_send_static<T: Send + 'static>(_: &T) {}
        let t = fx("st-send");
        let job = t.state.begin_download(Arc::new(FakeFetcher::new()), "a", None, POLICY, false, noop()).unwrap();
        assert_send_static(&job);
        drop(job);
        let job = t.state.begin_import("a", t.dir.join("x"), noop()).unwrap();
        assert_send_static(&job);
    }
}
