//! Debug builds only (the `mod` is `#[cfg(debug_assertions)]`): live audio dump for diagnosis.
//!
//! `TUNE_LOVE_DEVOCAL_DUMP=<dir>` makes every processing-thread run write, at most
//! [`MAX_FRAMES`] (60 s), into `<dir>/run-<run id>-<unix ms>/`:
//! - `in.f32`  the processor's input blocks (after the capture gain), in processing order;
//! - `sep.f32` the separator's accompaniment for the same blocks, before the dry/wet mix
//!   (zeros for blocks that did not run the model);
//! - `out.f32` the blocks handed to ring B (after the output fade-in), also the ones ring B
//!   dropped;
//!
//! all raw interleaved f32le stereo 44.1 kHz with the same frame count: frame `f` of each file
//! belongs to the same block.
//! - `blocks.csv` one row per dumped block: `dump_frame` (its first frame in the .f32 files),
//!   `in_pos` (frames taken from ring A before it, including frames skipped at a
//!   discontinuity), `out_pos` (its ring-B position; -1 = dropped, ring B full), `frames`,
//!   `stage` (after the block), `ran_model`, `sep_latency` (the model's latency),
//!   `out_latency` (the audible path's), `model_resets` (model streams started so far; a new
//!   value starts a new model stream), `t_us` (QPC us at the block start), `dropped_before`
//!   (dump drops so far).
//! - `meta.json` format, limits and totals, rewritten every second and at the end
//!   (`complete: true`).
//!
//! The processing thread only copies into preallocated SPSC rings (no allocation, lock or
//! I/O after [`Dump::from_env`]); a dedicated writer thread drains them to disk. A block that
//! does not fit is dropped whole and counted, never waited for.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::processor::Stage;

pub const ENV: &str = "TUNE_LOVE_DEVOCAL_DUMP";
const RATE: u64 = 44_100;
pub const MAX_FRAMES: u64 = 60 * RATE;
/// Two seconds of the three streams.
const RING_SAMPLES: usize = 2 * RATE as usize * 2 * 3;
/// Two seconds of 128-frame blocks.
const META_SLOTS: usize = 2 * RATE as usize / 128;
const POLL: Duration = Duration::from_millis(20);

/// What the processing thread knows about a block besides its audio.
#[derive(Clone, Copy, Debug)]
pub struct BlockMeta {
    pub in_pos: u64,
    pub out_pos: Option<u64>,
    pub stage: Stage,
    pub ran_model: bool,
    pub sep_latency: u32,
    pub out_latency: u32,
    pub model_resets: u32,
    pub t_us: u64,
}

struct Entry {
    meta: BlockMeta,
    frames: u32,
    drops: u64,
}

pub struct Dump {
    audio: Producer<f32>,
    meta: Producer<Entry>,
    frames: u64,
    drops: Arc<AtomicU64>,
}

impl Dump {
    /// `None` unless the env var names a directory. Allocates and spawns the writer thread:
    /// call before the real-time loop.
    pub fn from_env(run_id: u32) -> Option<Self> {
        let dir = std::env::var_os(ENV).filter(|d| !d.is_empty())?;
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        Some(Self::new(
            Path::new(&dir).join(format!("run-{run_id}-{ms}")),
        ))
    }

    pub fn new(dir: PathBuf) -> Self {
        let (audio, audio_rx) = RingBuffer::new(RING_SAMPLES);
        let (meta, meta_rx) = RingBuffer::new(META_SLOTS);
        let drops = Arc::new(AtomicU64::new(0));
        let d = drops.clone();
        let _ = thread::Builder::new()
            .name("devocal-dump".into())
            .spawn(move || {
                if let Err(e) = write_loop(&dir, audio_rx, meta_rx, &d) {
                    eprintln!("devocal dump {}: {e}", dir.display());
                }
            });
        Self {
            audio,
            meta,
            frames: 0,
            drops,
        }
    }

    /// Copies one block (`input`, `out` and `sep` when given: the same interleaved stereo
    /// length). Never blocks, allocates or does I/O; a block that does not fit is counted.
    pub fn block(&mut self, input: &[f32], sep: Option<&[f32]>, out: &[f32], meta: BlockMeta) {
        let n = input.len() / 2 * 2;
        if self.frames >= MAX_FRAMES || n == 0 {
            return;
        }
        if out.len() < n || self.meta.slots() == 0 {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Ok(chunk) = self.audio.write_chunk_uninit(3 * n) else {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let sep = sep.unwrap_or(&[]);
        chunk.fill_from_iter(
            input[..n]
                .iter()
                .copied()
                .chain(sep.iter().copied().chain(std::iter::repeat(0.0)).take(n))
                .chain(out[..n].iter().copied()),
        );
        let frames = (n / 2) as u32;
        let drops = self.drops.load(Ordering::Relaxed);
        // Audio first: the writer reads a block's audio once its record is visible.
        let _ = self.meta.push(Entry {
            meta,
            frames,
            drops,
        });
        self.frames += u64::from(frames);
    }
}

fn write_loop(
    dir: &Path,
    mut audio: Consumer<f32>,
    mut meta: Consumer<Entry>,
    drops: &AtomicU64,
) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let open = |name: &str| File::create(dir.join(name)).map(BufWriter::new);
    let mut files = [
        open("in.f32")?,
        open("sep.f32")?,
        open("out.f32")?,
        open("blocks.csv")?,
    ];
    writeln!(
        files[3],
        "dump_frame,in_pos,out_pos,frames,stage,ran_model,sep_latency,out_latency,model_resets,t_us,dropped_before"
    )?;
    let started_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut written = 0u64;
    let mut last_meta: Option<Instant> = None;
    loop {
        // Read before draining: everything pushed before the producer went away is drained.
        let abandoned = meta.is_abandoned();
        while let Ok(e) = meta.pop() {
            let n = e.frames as usize * 2;
            let chunk = audio
                .read_chunk(3 * n)
                .map_err(|_| io::Error::other("audio ring behind its block record"))?;
            let (a, b) = chunk.as_slices();
            for (i, s) in a.iter().chain(b).enumerate() {
                files[i / n].write_all(&s.to_le_bytes())?;
            }
            chunk.commit_all();
            let m = e.meta;
            writeln!(
                files[3],
                "{written},{},{},{},{:?},{},{},{},{},{},{}",
                m.in_pos,
                m.out_pos.map_or(-1, |p| p as i64),
                e.frames,
                m.stage,
                u8::from(m.ran_model),
                m.sep_latency,
                m.out_latency,
                m.model_resets,
                m.t_us,
                e.drops,
            )?;
            written += u64::from(e.frames);
        }
        let done = abandoned || written >= MAX_FRAMES;
        if done || last_meta.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            for f in &mut files {
                f.flush()?;
            }
            let info = serde_json::json!({
                "format": "f32le interleaved stereo",
                "sampleRate": RATE,
                "channels": 2,
                "files": ["in.f32", "sep.f32", "out.f32"],
                "startedUnixMs": started_ms as u64,
                "maxFrames": MAX_FRAMES,
                "framesWritten": written,
                "droppedBlocks": drops.load(Ordering::Relaxed),
                "complete": done,
            });
            std::fs::write(dir.join("meta.json"), info.to_string())?;
            last_meta = Some(Instant::now());
        }
        if done {
            return Ok(());
        }
        thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stemgen::alloc_count;

    fn meta(i: u64) -> BlockMeta {
        BlockMeta {
            in_pos: i * 128,
            out_pos: Some(i * 128),
            stage: Stage::Devocal,
            ran_model: true,
            sep_latency: 10_143,
            out_latency: 10_143,
            model_resets: 1,
            t_us: i,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("devocal-dump-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn wait_complete(dir: &Path) -> serde_json::Value {
        for _ in 0..500 {
            if let Ok(t) = std::fs::read_to_string(dir.join("meta.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    if v["complete"] == true {
                        return v;
                    }
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("dump never completed");
    }

    #[test]
    fn writes_aligned_streams_and_block_records() {
        let dir = temp_dir("aligned");
        let mut d = Dump::new(dir.clone());
        for i in 0..10u64 {
            let input = vec![i as f32; 256];
            let sep = vec![-(i as f32); 256];
            let out = vec![i as f32 + 0.5; 256];
            let s = (i % 2 == 0).then_some(&sep[..]);
            d.block(&input, s, &out, meta(i));
        }
        drop(d);
        let v = wait_complete(&dir);
        assert_eq!(v["framesWritten"], 1280);
        assert_eq!(v["droppedBlocks"], 0);
        let read = |n: &str| -> Vec<f32> {
            std::fs::read(dir.join(n))
                .unwrap()
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect()
        };
        let (i, s, o) = (read("in.f32"), read("sep.f32"), read("out.f32"));
        assert_eq!((i.len(), s.len(), o.len()), (2560, 2560, 2560));
        assert_eq!(i[256 * 3], 3.0);
        assert_eq!(s[256 * 2], -2.0);
        assert_eq!(s[256 * 3], 0.0, "no separator output: zeros");
        assert_eq!(o[256 * 9 + 255], 9.5);
        let csv = std::fs::read_to_string(dir.join("blocks.csv")).unwrap();
        let rows: Vec<&str> = csv.lines().collect();
        assert_eq!(rows.len(), 11);
        assert_eq!(rows[2], "128,128,128,128,Devocal,1,10143,10143,1,1,0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_never_allocates_and_counts_overflow() {
        let dir = temp_dir("noalloc");
        let mut d = Dump::new(dir.clone());
        let input = vec![0.25f32; 256];
        let out = vec![0.5f32; 256];
        let before = alloc_count::this_thread();
        // Far more than the ring holds (2 s): the writer cannot keep up within this loop,
        // so the overflow path runs too.
        for i in 0..5_000u64 {
            d.block(&input, Some(&out), &out, meta(i));
        }
        assert_eq!(
            alloc_count::this_thread() - before,
            0,
            "allocations on the calling thread"
        );
        drop(d);
        let v = wait_complete(&dir);
        let written = v["framesWritten"].as_u64().unwrap();
        let dropped = v["droppedBlocks"].as_u64().unwrap();
        assert_eq!(written / 128 + dropped, 5_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stops_at_the_cap() {
        let dir = temp_dir("cap");
        let mut d = Dump::new(dir.clone());
        let block = vec![0.0f32; 256];
        let mut sent = 0u64;
        while sent < MAX_FRAMES + 10 * 128 {
            if d.meta.slots() > 0 && d.audio.slots() >= 3 * 256 {
                d.block(&block, None, &block, meta(sent / 128));
                sent += 128;
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        let v = wait_complete(&dir);
        assert_eq!(
            v["framesWritten"].as_u64().unwrap(),
            MAX_FRAMES.div_ceil(128) * 128
        );
        drop(d);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same check as for `TUNE_LOVE_WINDOWED_SLOW_MS`: the env var name must not be in the
    /// release engine. Checks `devocal/target/release/devocal-engine.exe` when it exists
    /// (`cargo build --release -p devocal-engine` first; skipped otherwise).
    #[test]
    fn env_var_absent_from_release_exe() {
        let exe =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/release/devocal-engine.exe");
        let Ok(bytes) = std::fs::read(&exe) else {
            eprintln!("skipped: no {}", exe.display());
            return;
        };
        let needle = ENV.as_bytes();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "{ENV} found in {}",
            exe.display()
        );
    }
}
