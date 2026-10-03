//! Downloads one manifest file: resumable `.part` + `.part.json`, source fallback with retries,
//! a SHA-256 gate, and an atomic rename into place. The final file name only ever appears by
//! renaming a complete `.part` whose hash matched.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::fetch::{Body, FetchError, Fetcher};
use super::manifest::{Candidate, FileSpec, SourceKind};
use super::verify::note_verified;

/// `.part.json` is rewritten after every this many newly written bytes.
const META_INTERVAL: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    Network,
    Timeout,
    HttpStatus(u16),
    SourceHtml,
    SizeMismatch,
    Sha256Mismatch,
    DiskFull,
    WriteFailed,
    InstallDenied,
    Cancelled,
    AlreadyRunning,
    AllSourcesFailed,
    InvalidPrefix,
    UnknownModel,
    DeleteDenied,
    ImportMismatch,
    ImportUnsupported,
}

impl ModelError {
    /// The machine code the frontend maps to a message.
    pub fn code(&self) -> String {
        let code = match self {
            ModelError::HttpStatus(n) => return format!("http_status:{n}"),
            ModelError::Network => "network_unreachable",
            ModelError::Timeout => "timeout",
            ModelError::SourceHtml => "source_html",
            ModelError::SizeMismatch => "size_mismatch",
            ModelError::Sha256Mismatch => "sha256_mismatch",
            ModelError::DiskFull => "disk_full",
            ModelError::WriteFailed => "write_failed",
            ModelError::InstallDenied => "install_denied",
            ModelError::Cancelled => "cancelled",
            ModelError::AlreadyRunning => "already_running",
            ModelError::AllSourcesFailed => "all_sources_failed",
            ModelError::InvalidPrefix => "invalid_prefix",
            ModelError::UnknownModel => "unknown_model",
            ModelError::DeleteDenied => "delete_denied",
            ModelError::ImportMismatch => "import_mismatch",
            ModelError::ImportUnsupported => "import_unsupported",
        };
        code.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Extra attempts per source after the first.
    pub retries: u32,
    /// The k-th retry of a source waits `backoff * k` first.
    pub backoff: Duration,
}

impl RetryPolicy {
    pub fn standard() -> Self {
        RetryPolicy { retries: 2, backoff: Duration::from_secs(1) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// Hashing bytes already on disk (resume) or checking the finished file.
    Verifying,
    /// `received`: bytes of this file now in `.part`.
    Downloading { received: u64, source: SourceKind },
}

/// `<file>.part`, next to the file.
pub fn part_path(final_path: &Path) -> PathBuf {
    with_suffix(final_path, ".part")
}

/// `<file>.part.json`, next to the file.
pub fn meta_path(final_path: &Path) -> PathBuf {
    with_suffix(final_path, ".part.json")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// What `.part.json` records about the `.part` beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartMeta {
    /// Label of the source the latest bytes came from.
    pub source: String,
    pub url: String,
    /// Bytes of `.part` known to be written (never more than its length when saved).
    pub written: u64,
    /// The manifest size and hash this `.part` belongs to; a different manifest discards it.
    pub total: u64,
    pub sha256: String,
}

/// A local file-system failure: a full disk is reported as such, anything else as a write failure.
pub fn io_error(e: &std::io::Error) -> ModelError {
    match e.kind() {
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => ModelError::DiskFull,
        _ => ModelError::WriteFailed,
    }
}

fn local<T>(r: std::io::Result<T>) -> Result<T, ModelError> {
    r.map_err(|e| io_error(&e))
}

fn remove_if_present(path: &Path) -> Result<(), ModelError> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(io_error(&e)),
        _ => Ok(()),
    }
}

/// Puts a complete, verified `.part` in place: flush to disk, rename over the final name, then
/// remember `sha256` for that path. `sha256` must be the hash the caller computed over the
/// `.part` (see [`note_verified`]). A final file held open by another program is `InstallDenied`.
pub fn install(part: &Path, final_path: &Path, sha256: &str) -> Result<(), ModelError> {
    // Windows' FlushFileBuffers needs a handle with write access.
    let file = local(OpenOptions::new().write(true).open(part))?;
    local(file.sync_all())?;
    drop(file);
    fs::rename(part, final_path).map_err(|e| if install_denied(&e) { ModelError::InstallDenied } else { io_error(&e) })?;
    note_verified(final_path, sha256);
    Ok(())
}

fn install_denied(e: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION: the target is open elsewhere.
    e.kind() == ErrorKind::PermissionDenied || (cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)))
}

/// The `.part` being filled, the hash of everything in it so far, and its `.part.json`.
struct Part {
    file: File,
    path: PathBuf,
    meta_path: PathBuf,
    total: u64,
    sha256: String,
    hasher: Sha256,
    written: u64,
    /// Bytes written since `.part.json` was last saved.
    unsaved: u64,
    source: String,
    url: String,
}

impl Part {
    /// Opens the `.part`, keeping what an earlier run left only if its `.part.json` belongs to
    /// this manifest entry and the `.part` is not longer than the file. Kept bytes are re-hashed.
    fn open(spec: &FileSpec, final_path: &Path, progress: &(dyn Fn(Update) + Send + Sync)) -> Result<Part, ModelError> {
        let path = part_path(final_path);
        let meta_path = meta_path(final_path);
        let meta = fs::read(&meta_path).ok().and_then(|b| serde_json::from_slice::<PartMeta>(&b).ok());
        let part_len = fs::metadata(&path).ok().filter(|m| m.is_file()).map(|m| m.len());
        let resume = match (&meta, part_len) {
            (Some(m), Some(len)) if m.total == spec.bytes && m.sha256 == spec.sha256 && len <= spec.bytes => m.written.min(len),
            _ => 0,
        };
        if resume == 0 {
            remove_if_present(&path)?;
            remove_if_present(&meta_path)?;
        }
        let mut file = local(OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path))?;
        // Bytes past the recorded point may never have been fully written.
        local(file.set_len(resume))?;
        let mut hasher = Sha256::new();
        if resume > 0 {
            progress(Update::Verifying);
            let hashed = local(std::io::copy(&mut (&mut file).take(resume), &mut hasher))?;
            if hashed != resume {
                return Err(ModelError::WriteFailed);
            }
        }
        local(file.seek(SeekFrom::Start(resume)))?;
        let (source, url) = meta.filter(|_| resume > 0).map(|m| (m.source, m.url)).unwrap_or_default();
        Ok(Part {
            file,
            path,
            meta_path,
            total: spec.bytes,
            sha256: spec.sha256.clone(),
            hasher,
            written: resume,
            unsaved: 0,
            source,
            url,
        })
    }

    /// Back to an empty `.part` (a 200 sends the whole file again; an oversized body is junk).
    fn restart(&mut self) -> Result<(), ModelError> {
        local(self.file.set_len(0))?;
        local(self.file.seek(SeekFrom::Start(0)))?;
        self.hasher = Sha256::new();
        self.written = 0;
        self.unsaved = 0;
        Ok(())
    }

    fn append(&mut self, chunk: &[u8]) -> Result<(), ModelError> {
        local(self.file.write_all(chunk))?;
        self.hasher.update(chunk);
        self.written += chunk.len() as u64;
        self.unsaved += chunk.len() as u64;
        if self.unsaved >= META_INTERVAL {
            self.save()?;
        }
        Ok(())
    }

    /// Flushes the data, then records how much of it there is.
    fn save(&mut self) -> Result<(), ModelError> {
        local(self.file.flush())?;
        let meta = PartMeta {
            source: self.source.clone(),
            url: self.url.clone(),
            written: self.written,
            total: self.total,
            sha256: self.sha256.clone(),
        };
        let json = serde_json::to_vec(&meta).map_err(|_| ModelError::WriteFailed)?;
        local(fs::write(&self.meta_path, json))?;
        self.unsaved = 0;
        Ok(())
    }

    /// Saves what is there for a later resume and reports the cancellation.
    fn cancel(&mut self) -> ModelError {
        if let Err(e) = self.save() {
            eprintln!("model download: could not record the partial download: {}", e.code());
        }
        ModelError::Cancelled
    }

    /// Closes the `.part` and returns its hash.
    fn finish(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

/// Downloads `file` to `final_path`, trying `candidates` in order. On success the verified file
/// is in place and `.part`/`.part.json` are gone. On failure or cancellation the `.part` and an
/// up-to-date `.part.json` are kept for a later resume (except after a hash mismatch).
pub async fn download_file<F: Fetcher>(
    fetcher: &F,
    file: &FileSpec,
    final_path: &Path,
    candidates: &[Candidate],
    policy: &RetryPolicy,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Update) + Send + Sync),
) -> Result<(), ModelError> {
    if let Some(dir) = final_path.parent() {
        local(fs::create_dir_all(dir))?;
    }
    let mut mismatched_before = false;
    loop {
        let mut part = Part::open(file, final_path, progress)?;
        if part.written < file.bytes {
            fetch_into(fetcher, file, candidates, policy, cancel, progress, &mut part).await?;
        }
        progress(Update::Verifying);
        let (part_file, meta_file) = (part.path.clone(), part.meta_path.clone());
        let sha256 = part.finish();
        if sha256 == file.sha256 {
            install(&part_file, final_path, &sha256)?;
            if let Err(e) = fs::remove_file(&meta_file) {
                eprintln!("model download: could not remove {}: {e}", meta_file.display());
            }
            return Ok(());
        }
        eprintln!("model download: {} SHA-256 mismatch (got {sha256})", file.file);
        remove_if_present(&part_file)?;
        remove_if_present(&meta_file)?;
        if mismatched_before {
            return Err(ModelError::Sha256Mismatch);
        }
        // Once more, in full, from the first source.
        mismatched_before = true;
    }
}

enum Outcome {
    /// `.part` holds exactly `bytes` bytes.
    Complete,
    /// Try this source again after the backoff.
    Retry(String),
    /// Try this source again at once, from byte 0 (it refused the range).
    RetryFromZero(String),
    /// Give up on this source.
    Drop(String),
}

/// Fills `part` up to the file size from the first source that manages it.
async fn fetch_into<F: Fetcher>(
    fetcher: &F,
    file: &FileSpec,
    candidates: &[Candidate],
    policy: &RetryPolicy,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Update) + Send + Sync),
    part: &mut Part,
) -> Result<(), ModelError> {
    let attempts = policy.retries.saturating_add(1);
    for candidate in candidates {
        let mut use_range = true;
        let mut made = 0;
        let reason = loop {
            made += 1;
            let reason = match attempt(fetcher, candidate, file, part, cancel, progress, use_range).await? {
                Outcome::Complete => return Ok(()),
                Outcome::Drop(reason) => break reason,
                Outcome::RetryFromZero(reason) => {
                    use_range = false;
                    if made >= attempts {
                        break reason;
                    }
                    continue;
                }
                Outcome::Retry(reason) => reason,
            };
            if made >= attempts {
                break reason;
            }
            tokio::time::sleep(policy.backoff * made).await;
        };
        eprintln!("model download: {} from {} failed: {reason}", file.file, candidate.kind.label());
    }
    Err(ModelError::AllSourcesFailed)
}

/// One GET from one source, written into `part`. Local write errors and cancellation are `Err`.
async fn attempt<F: Fetcher>(
    fetcher: &F,
    candidate: &Candidate,
    file: &FileSpec,
    part: &mut Part,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Update) + Send + Sync),
    use_range: bool,
) -> Result<Outcome, ModelError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(part.cancel());
    }
    let offset = if use_range { part.written } else { 0 };
    let opened = match fetcher.open(&candidate.url, offset).await {
        Ok(opened) => opened,
        // The source does not accept our resume point; the whole file may still work.
        Err(FetchError::Status(416)) if offset > 0 => {
            return Ok(Outcome::RetryFromZero(format!("range from {offset} not satisfiable")))
        }
        Err(e) if e.retryable() => return Ok(Outcome::Retry(describe(&e))),
        Err(e) => return Ok(Outcome::Drop(describe(&e))),
    };
    if cancel.load(Ordering::SeqCst) {
        return Err(part.cancel());
    }
    if opened.html {
        return Ok(Outcome::Drop(format!("returned a web page ({})", ModelError::SourceHtml.code())));
    }
    // A 206 must state the full size and it must match; a 200 may omit it.
    let total_ok = match opened.status {
        206 => opened.total == Some(file.bytes),
        200 => opened.total.is_none_or(|t| t == file.bytes),
        _ => false,
    };
    if !total_ok {
        return Ok(Outcome::Drop(format!(
            "status {} with total {:?}, expected {} ({})",
            opened.status,
            opened.total,
            file.bytes,
            ModelError::SizeMismatch.code()
        )));
    }
    // A 200 is the whole file from byte 0, even when a range was asked for: never append it.
    if opened.status == 200 || offset == 0 {
        part.restart()?;
    }
    part.source = candidate.kind.label();
    part.url = candidate.url.clone();
    let mut body = opened.body;
    loop {
        match body.next_chunk().await {
            Ok(Some(chunk)) => {
                if part.written + chunk.len() as u64 > file.bytes {
                    part.restart()?;
                    part.save()?;
                    return Ok(Outcome::Drop(format!("sent more than {} bytes", file.bytes)));
                }
                part.append(&chunk)?;
                progress(Update::Downloading { received: part.written, source: candidate.kind.clone() });
                if cancel.load(Ordering::SeqCst) {
                    return Err(part.cancel());
                }
            }
            Ok(None) if part.written == file.bytes => return Ok(Outcome::Complete),
            Ok(None) => {
                part.save()?;
                return Ok(Outcome::Retry(format!("body ended at {} of {} bytes", part.written, file.bytes)));
            }
            Err(e) => {
                part.save()?;
                return Ok(Outcome::Retry(describe(&e)));
            }
        }
    }
}

fn describe(e: &FetchError) -> String {
    match e {
        FetchError::Connect => "could not connect".into(),
        FetchError::Timeout => "timed out".into(),
        FetchError::Status(code) => format!("HTTP {code}"),
        FetchError::BadRange => "bad Content-Range".into(),
        FetchError::Network(text) | FetchError::Fatal(text) => text.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devocal::model::fake::{spec_for, FakeFetcher, FakeReply};
    use crate::devocal::model::fetch::FetchError;
    use crate::devocal::model::manifest::candidates;
    use crate::devocal::model::verify::file_verified;
    use crate::devocal::tests::temp_dir;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    const ORIGIN: &str = "https://github.com/a/b/raw/c/model.onnx";
    const SIZE: usize = 64 * 1024;
    const HALF: usize = SIZE / 2;
    const POLICY: RetryPolicy = RetryPolicy { retries: 2, backoff: Duration::ZERO };

    fn mirror(host: &str) -> String {
        format!("https://{host}/{ORIGIN}")
    }

    /// 64 KiB of xorshift bytes.
    fn data() -> Vec<u8> {
        let mut x: u32 = 0x9E37_79B9;
        (0..SIZE)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    fn chunks(bytes: &[u8]) -> Vec<Vec<u8>> {
        bytes.chunks(4096).map(<[u8]>::to_vec).collect()
    }

    fn serve(status: u16, total: Option<u64>, body: &[u8]) -> FakeReply {
        FakeReply::Serve { status, total, html: false, chunks: chunks(body), then: None }
    }

    /// 200 with the whole of `file`.
    fn ok200(file: &[u8]) -> FakeReply {
        serve(200, Some(file.len() as u64), file)
    }

    /// 206 with `file` from `from` on.
    fn ok206(file: &[u8], from: usize) -> FakeReply {
        serve(206, Some(file.len() as u64), &file[from..])
    }

    fn calls(list: &[(&str, usize)]) -> Vec<(String, u64)> {
        list.iter().map(|(u, o)| (u.to_string(), *o as u64)).collect()
    }

    struct T {
        data: Vec<u8>,
        spec: FileSpec,
        final_path: PathBuf,
        cands: Vec<Candidate>,
    }

    impl T {
        fn new(tag: &str) -> T {
            let data = data();
            let spec = spec_for(&data, ORIGIN, true);
            let final_path = temp_dir(tag).join("stemgenrt-hop128").join("model.onnx");
            let mirrors = ["https://ghfast.top/".to_string(), "https://ghproxy.net/".to_string()];
            let cands = candidates(&spec, None, &mirrors);
            T { data, spec, final_path, cands }
        }

        fn part(&self) -> PathBuf {
            part_path(&self.final_path)
        }

        fn meta(&self) -> PathBuf {
            meta_path(&self.final_path)
        }

        async fn run(&self, f: &FakeFetcher) -> Result<(), ModelError> {
            self.run_with(f, &AtomicBool::new(false), &|_| {}).await
        }

        async fn run_with(
            &self,
            f: &FakeFetcher,
            cancel: &AtomicBool,
            progress: &(dyn Fn(Update) + Send + Sync),
        ) -> Result<(), ModelError> {
            download_file(f, &self.spec, &self.final_path, &self.cands, &POLICY, cancel, progress).await
        }

        /// Leaves `part` as the `.part` and, if `written` is given, a matching-looking `.part.json`.
        fn preset(&self, part: &[u8], written: Option<u64>, sha256: &str) {
            std::fs::create_dir_all(self.final_path.parent().unwrap()).unwrap();
            std::fs::write(self.part(), part).unwrap();
            if let Some(written) = written {
                let meta = PartMeta {
                    source: "origin".into(),
                    url: ORIGIN.into(),
                    written,
                    total: self.spec.bytes,
                    sha256: sha256.into(),
                };
                std::fs::write(self.meta(), serde_json::to_vec(&meta).unwrap()).unwrap();
            }
        }

        fn assert_installed(&self) {
            assert_eq!(std::fs::read(&self.final_path).unwrap(), self.data);
            assert!(!self.part().exists(), ".part left behind");
            assert!(!self.meta().exists(), ".part.json left behind");
            assert!(file_verified(&self.final_path, &self.spec));
        }
    }

    #[test]
    fn temp_paths_sit_next_to_the_file() {
        let f = Path::new("models").join("x").join("model.onnx");
        assert_eq!(part_path(&f), Path::new("models").join("x").join("model.onnx.part"));
        assert_eq!(meta_path(&f), Path::new("models").join("x").join("model.onnx.part.json"));
    }

    #[test]
    fn part_meta_is_camel_case() {
        let m = PartMeta { source: "origin".into(), url: "u".into(), written: 1, total: 2, sha256: "s".into() };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::json!({ "source": "origin", "url": "u", "written": 1, "total": 2, "sha256": "s" })
        );
    }

    #[tokio::test]
    async fn downloads_from_origin_and_installs_atomically() {
        let t = T::new("dl-origin");
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        t.assert_installed();
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0)]));
    }

    #[tokio::test]
    async fn retryable_errors_retry_twice_per_source_then_move_on() {
        let t = T::new("dl-retry");
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![
            FakeReply::Fail(FetchError::Connect),
            FakeReply::Fail(FetchError::Timeout),
            FakeReply::Fail(FetchError::Status(503)),
            ok200(&t.data),
        ]);
        f.script(&mirror("ghfast.top"), vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        let m1 = mirror("ghfast.top");
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (ORIGIN, 0), (ORIGIN, 0), (&m1, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn retries_wait_backoff_times_k() {
        let t = T::new("dl-backoff");
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![FakeReply::Fail(FetchError::Connect), FakeReply::Fail(FetchError::Connect), ok200(&t.data)]);
        let policy = RetryPolicy { retries: 2, backoff: Duration::from_millis(40) };
        let started = std::time::Instant::now();
        let r = download_file(&f, &t.spec, &t.final_path, &t.cands, &policy, &AtomicBool::new(false), &|_| {}).await;
        assert_eq!(r, Ok(()));
        assert!(started.elapsed() >= Duration::from_millis(120), "40 ms + 80 ms, got {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn non_retryable_status_moves_on_without_retry() {
        let t = T::new("dl-404");
        let f = FakeFetcher::new();
        let (m1, m2) = (mirror("ghfast.top"), mirror("ghproxy.net"));
        f.script(ORIGIN, vec![FakeReply::Fail(FetchError::Status(404)), ok200(&t.data)]);
        f.script(&m1, vec![FakeReply::Fail(FetchError::BadRange), ok200(&t.data)]);
        f.script(&m2, vec![FakeReply::Fail(FetchError::Fatal("redirect loop".into())), ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Err(ModelError::AllSourcesFailed));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (&m1, 0), (&m2, 0)]));
    }

    #[tokio::test]
    async fn html_source_is_dropped_immediately() {
        let t = T::new("dl-html");
        let f = FakeFetcher::new();
        let (m1, m2) = (mirror("ghfast.top"), mirror("ghproxy.net"));
        f.script(ORIGIN, vec![FakeReply::Fail(FetchError::Status(403))]);
        let page = FakeReply::Serve { status: 200, total: None, html: true, chunks: vec![b"<html>".to_vec()], then: None };
        f.script(&m1, vec![page, ok200(&t.data)]);
        f.script(&m2, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (&m1, 0), (&m2, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn wrong_total_drops_source() {
        let t = T::new("dl-total");
        let f = FakeFetcher::new();
        let m1 = mirror("ghfast.top");
        let wrong = serve(200, Some(SIZE as u64 + 1), &t.data);
        f.script(ORIGIN, vec![wrong, ok200(&t.data)]);
        f.script(&m1, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (&m1, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn body_longer_than_spec_drops_source() {
        let t = T::new("dl-long");
        let f = FakeFetcher::new();
        let m1 = mirror("ghfast.top");
        let mut longer = t.data.clone();
        longer.extend_from_slice(b"extra");
        f.script(ORIGIN, vec![serve(200, None, &longer), ok200(&t.data)]);
        f.script(&m1, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (&m1, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn mid_body_failure_resumes_from_written_offset() {
        let t = T::new("dl-mid");
        let f = FakeFetcher::new();
        let broken = FakeReply::Serve {
            status: 200,
            total: Some(SIZE as u64),
            html: false,
            chunks: chunks(&t.data[..HALF]),
            then: Some(FetchError::Network("connection reset".into())),
        };
        f.script(ORIGIN, vec![broken, ok206(&t.data, HALF)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (ORIGIN, HALF)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn early_end_of_body_is_retried_from_written_offset() {
        let t = T::new("dl-short");
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![serve(200, None, &t.data[..HALF]), ok206(&t.data, HALF)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (ORIGIN, HALF)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn resume_appends_on_206_with_matching_total() {
        let t = T::new("dl-resume");
        t.preset(&t.data[..HALF], Some(HALF as u64), &t.spec.sha256);
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok206(&t.data, HALF)]);
        let log = Mutex::new(Vec::new());
        let r = t.run_with(&f, &AtomicBool::new(false), &|u| log.lock().unwrap().push(u)).await;
        assert_eq!(r, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, HALF)]));
        t.assert_installed();
        let log = log.into_inner().unwrap();
        assert_eq!(log.first(), Some(&Update::Verifying), "existing bytes are re-hashed first");
        assert_eq!(log.last(), Some(&Update::Verifying));
        assert!(log.contains(&Update::Downloading { received: SIZE as u64, source: SourceKind::Origin }));
    }

    #[tokio::test]
    async fn resume_restarts_on_200() {
        let t = T::new("dl-resume-200");
        t.preset(&t.data[..HALF], Some(HALF as u64), &t.spec.sha256);
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, HALF)]));
        assert_eq!(std::fs::metadata(&t.final_path).unwrap().len(), SIZE as u64);
        t.assert_installed();
    }

    #[tokio::test]
    async fn resume_206_with_wrong_total_drops_source() {
        let t = T::new("dl-resume-total");
        t.preset(&t.data[..HALF], Some(HALF as u64), &t.spec.sha256);
        let f = FakeFetcher::new();
        let (m1, m2) = (mirror("ghfast.top"), mirror("ghproxy.net"));
        f.script(ORIGIN, vec![serve(206, Some(SIZE as u64 + 1), &t.data[HALF..]), ok206(&t.data, HALF)]);
        // An unknown total (`bytes a-b/*`) cannot be checked, so it counts as a mismatch too.
        f.script(&m1, vec![serve(206, None, &t.data[HALF..]), ok206(&t.data, HALF)]);
        f.script(&m2, vec![ok206(&t.data, HALF)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, HALF), (&m1, HALF), (&m2, HALF)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn range_not_satisfiable_restarts_from_zero_on_the_same_source() {
        let t = T::new("dl-416");
        t.preset(&t.data[..HALF], Some(HALF as u64), &t.spec.sha256);
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![FakeReply::Fail(FetchError::Status(416)), ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, HALF), (ORIGIN, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn range_not_satisfiable_keeps_the_part_if_the_source_then_fails() {
        let t = T::new("dl-416-keep");
        t.preset(&t.data[..HALF], Some(HALF as u64), &t.spec.sha256);
        let f = FakeFetcher::new();
        let m1 = mirror("ghfast.top");
        f.script(ORIGIN, vec![FakeReply::Fail(FetchError::Status(416)), FakeReply::Fail(FetchError::Status(404))]);
        f.script(&m1, vec![ok206(&t.data, HALF)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, HALF), (ORIGIN, 0), (&m1, HALF)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn meta_ahead_of_part_uses_part_length() {
        let t = T::new("dl-meta-ahead");
        t.preset(&t.data[..32 * 1024], Some(40 * 1024), &t.spec.sha256);
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok206(&t.data, 32 * 1024)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 32 * 1024)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn part_ahead_of_meta_resumes_at_meta_and_overwrites_the_rest() {
        let t = T::new("dl-part-ahead");
        let mut part = t.data[..40 * 1024].to_vec();
        part[36 * 1024] ^= 0xFF; // unflushed garbage past the recorded point
        t.preset(&part, Some(32 * 1024), &t.spec.sha256);
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok206(&t.data, 32 * 1024)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 32 * 1024)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn unrecorded_tail_is_trimmed_even_if_nothing_downloads() {
        let t = T::new("dl-trim");
        t.preset(&t.data[..40 * 1024], Some(32 * 1024), &t.spec.sha256);
        let f = FakeFetcher::new();
        assert_eq!(t.run(&f).await, Err(ModelError::AllSourcesFailed));
        assert_eq!(std::fs::read(t.part()).unwrap(), t.data[..32 * 1024]);
        assert!(f.calls().iter().all(|(_, offset)| *offset == 32 * 1024));
    }

    #[tokio::test]
    async fn part_without_meta_restarts_from_zero() {
        let t = T::new("dl-no-meta");
        t.preset(&t.data[..HALF], None, "");
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn part_longer_than_spec_restarts() {
        let t = T::new("dl-part-long");
        let mut part = t.data.clone();
        part.extend_from_slice(b"tail");
        t.preset(&part, Some(SIZE as u64), &t.spec.sha256);
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn meta_for_other_sha_restarts() {
        let t = T::new("dl-other-sha");
        t.preset(&t.data[..HALF], Some(HALF as u64), &"0".repeat(64));
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn corrupt_meta_restarts() {
        let t = T::new("dl-bad-meta");
        t.preset(&t.data[..HALF], None, "");
        std::fs::write(t.meta(), b"{\"written\":").unwrap();
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn complete_part_is_installed_without_network() {
        let t = T::new("dl-complete");
        t.preset(&t.data, Some(SIZE as u64), &t.spec.sha256);
        let f = FakeFetcher::new();
        assert_eq!(t.run(&f).await, Ok(()));
        assert!(f.calls().is_empty());
        t.assert_installed();
    }

    #[tokio::test]
    async fn sha_mismatch_redownloads_once_from_the_first_source() {
        let t = T::new("dl-sha-once");
        let mut bad = t.data.clone();
        bad[1234] ^= 1;
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&bad), ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (ORIGIN, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn sha_mismatch_restarts_from_the_first_source_even_after_a_mirror() {
        let t = T::new("dl-sha-mirror");
        let mut bad = t.data.clone();
        bad[0] ^= 1;
        let f = FakeFetcher::new();
        let m1 = mirror("ghfast.top");
        f.script(ORIGIN, vec![FakeReply::Fail(FetchError::Status(404)), ok200(&t.data)]);
        f.script(&m1, vec![ok200(&bad)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls(), calls(&[(ORIGIN, 0), (&m1, 0), (ORIGIN, 0)]));
        t.assert_installed();
    }

    #[tokio::test]
    async fn second_sha_mismatch_fails_and_leaves_nothing() {
        let t = T::new("dl-sha-twice");
        let mut bad = t.data.clone();
        bad[SIZE - 1] ^= 1;
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&bad), ok200(&bad), ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Err(ModelError::Sha256Mismatch));
        assert_eq!(f.calls().len(), 2);
        assert!(!t.final_path.exists() && !t.part().exists() && !t.meta().exists());
    }

    #[tokio::test]
    async fn all_sources_failed_when_every_source_fails() {
        let t = T::new("dl-all-fail");
        let f = FakeFetcher::new();
        assert_eq!(t.run(&f).await, Err(ModelError::AllSourcesFailed));
        assert_eq!(f.calls().len(), 9, "3 sources x 3 attempts");
        assert!(!t.final_path.exists());
    }

    #[tokio::test]
    async fn failed_sources_leave_a_resumable_part() {
        let t = T::new("dl-fail-keep");
        let f = FakeFetcher::new();
        let broken = FakeReply::Serve {
            status: 200,
            total: Some(SIZE as u64),
            html: false,
            chunks: chunks(&t.data[..HALF]),
            then: Some(FetchError::Timeout),
        };
        f.script(ORIGIN, vec![broken]);
        assert_eq!(t.run(&f).await, Err(ModelError::AllSourcesFailed));
        let meta: PartMeta = serde_json::from_slice(&std::fs::read(t.meta()).unwrap()).unwrap();
        assert_eq!(meta.written, HALF as u64);
        assert_eq!(std::fs::read(t.part()).unwrap(), t.data[..HALF]);
    }

    #[tokio::test]
    async fn cancel_keeps_part_and_meta() {
        let t = T::new("dl-cancel");
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&t.data)]);
        let cancel = AtomicBool::new(false);
        let r = t
            .run_with(&f, &cancel, &|u| {
                if matches!(u, Update::Downloading { .. }) {
                    cancel.store(true, Ordering::SeqCst);
                }
            })
            .await;
        assert_eq!(r, Err(ModelError::Cancelled));
        assert!(!t.final_path.exists());
        let meta: PartMeta = serde_json::from_slice(&std::fs::read(t.meta()).unwrap()).unwrap();
        let part_len = std::fs::metadata(t.part()).unwrap().len();
        assert!(meta.written > 0);
        assert_eq!(part_len, meta.written);
        assert_eq!((meta.total, meta.sha256.as_str(), meta.source.as_str()), (SIZE as u64, t.spec.sha256.as_str(), "origin"));

        // The next run continues where the cancelled one stopped.
        f.script(ORIGIN, vec![ok206(&t.data, part_len as usize)]);
        assert_eq!(t.run(&f).await, Ok(()));
        assert_eq!(f.calls()[1], (ORIGIN.to_string(), part_len));
        t.assert_installed();
    }

    #[tokio::test]
    async fn cancel_while_waiting_for_open_stops_before_reading() {
        let t = T::new("dl-cancel-open");
        let f = std::sync::Arc::new(FakeFetcher::new());
        f.script(ORIGIN, vec![ok200(&t.data)]);
        let gate = f.gate();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let task = {
            let (f, cancel, spec, path, cands) = (f.clone(), cancel.clone(), t.spec.clone(), t.final_path.clone(), t.cands.clone());
            tokio::spawn(async move { download_file(&*f, &spec, &path, &cands, &POLICY, &cancel, &|_| {}).await })
        };
        while f.calls().is_empty() {
            tokio::task::yield_now().await;
        }
        cancel.store(true, Ordering::SeqCst);
        gate.notify_one();
        assert_eq!(task.await.unwrap(), Err(ModelError::Cancelled));
        assert!(!t.final_path.exists());
    }

    #[tokio::test]
    async fn progress_reports_source_kind() {
        let t = T::new("dl-progress");
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![FakeReply::Fail(FetchError::Status(404))]);
        f.script(&mirror("ghfast.top"), vec![ok200(&t.data)]);
        let log = Mutex::new(Vec::new());
        let r = t.run_with(&f, &AtomicBool::new(false), &|u| log.lock().unwrap().push(u)).await;
        assert_eq!(r, Ok(()));
        let log = log.into_inner().unwrap();
        let downloading: Vec<_> = log.iter().filter(|u| matches!(u, Update::Downloading { .. })).collect();
        assert_eq!(downloading.len(), SIZE / 4096);
        let mirror_kind = SourceKind::Mirror("ghfast.top".into());
        assert!(downloading.iter().all(|u| matches!(u, Update::Downloading { source, .. } if *source == mirror_kind)));
        assert_eq!(downloading.last(), Some(&&Update::Downloading { received: SIZE as u64, source: mirror_kind }));
        assert_eq!(log.last(), Some(&Update::Verifying));
    }

    #[tokio::test]
    async fn local_write_failure_stops_without_trying_other_sources() {
        let t = T::new("dl-write-fail");
        // The model directory's name is taken by a file, so nothing can be created in it.
        std::fs::write(t.final_path.parent().unwrap(), b"not a directory").unwrap();
        let f = FakeFetcher::new();
        f.script(ORIGIN, vec![ok200(&t.data)]);
        assert_eq!(t.run(&f).await, Err(ModelError::WriteFailed));
        assert!(f.calls().is_empty());
    }

    #[test]
    fn download_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let t = T::new("dl-send");
        let f = FakeFetcher::new();
        let cancel = AtomicBool::new(false);
        let fut = download_file(&f, &t.spec, &t.final_path, &t.cands, &POLICY, &cancel, &|_| {});
        assert_send(&fut);
    }

    #[test]
    fn install_renames_and_remembers_the_hash() {
        let t = T::new("dl-install");
        t.preset(&t.data, None, "");
        assert_eq!(install(&t.part(), &t.final_path, &t.spec.sha256), Ok(()));
        assert!(!t.part().exists());
        assert!(file_verified(&t.final_path, &t.spec));
    }

    #[cfg(windows)]
    #[test]
    fn install_reports_install_denied_when_target_locked() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = T::new("dl-locked");
        t.preset(&t.data, None, "");
        std::fs::write(&t.final_path, b"old").unwrap();
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&t.final_path).unwrap();
        assert_eq!(install(&t.part(), &t.final_path, &t.spec.sha256), Err(ModelError::InstallDenied));
        assert!(t.part().exists());
        drop(lock);
        assert_eq!(std::fs::read(&t.final_path).unwrap(), b"old");
    }

    #[test]
    fn io_errors_map_to_disk_full_or_write_failed() {
        use std::io::{Error, ErrorKind};
        assert_eq!(io_error(&Error::from(ErrorKind::StorageFull)), ModelError::DiskFull);
        assert_eq!(io_error(&Error::from(ErrorKind::PermissionDenied)), ModelError::WriteFailed);
        assert_eq!(io_error(&Error::other("x")), ModelError::WriteFailed);
    }

    #[test]
    fn codes_match_the_contract() {
        use ModelError::*;
        for (e, code) in [
            (Network, "network_unreachable"),
            (Timeout, "timeout"),
            (HttpStatus(429), "http_status:429"),
            (HttpStatus(404), "http_status:404"),
            (SourceHtml, "source_html"),
            (SizeMismatch, "size_mismatch"),
            (Sha256Mismatch, "sha256_mismatch"),
            (DiskFull, "disk_full"),
            (WriteFailed, "write_failed"),
            (InstallDenied, "install_denied"),
            (Cancelled, "cancelled"),
            (AlreadyRunning, "already_running"),
            (AllSourcesFailed, "all_sources_failed"),
            (InvalidPrefix, "invalid_prefix"),
            (UnknownModel, "unknown_model"),
            (DeleteDenied, "delete_denied"),
            (ImportMismatch, "import_mismatch"),
            (ImportUnsupported, "import_unsupported"),
        ] {
            assert_eq!(e.code(), code);
        }
        assert_eq!(RetryPolicy::standard(), RetryPolicy { retries: 2, backoff: Duration::from_secs(1) });
    }
}
