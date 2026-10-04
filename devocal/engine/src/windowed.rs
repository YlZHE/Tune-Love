//! Window-model separator (bytesep, HTDemucs; spec §4.1). The audio thread keeps 128-frame
//! blocks flowing; every H input frames it hands the W-frame window ending L frames past the
//! hop to a dedicated inference thread, then stitches the returned accompaniment at a fixed
//! latency of H + L + ceil(0.3 H).
//!
//! Stitching (plan Ruling 1, crossfade placed after kH per Task 3 ruling 3; reference
//! `scripts/models/stitch_fixture.py`): segment k comes from window positions
//! `[W-L-H, W-L+XF)`, i.e. input `[kH, kH+H+XF)`; accompaniment = window - vocals there.
//! Over `[kH, kH+XF)` it crossfades linearly from segment k-1's extra tail (silence before
//! segment 0); `[kH+XF, kH+H)` is segment k alone; its last XF frames are the tail for k+1.
//! A segment whose result is missing or took longer than ceil(0.3 H) is replaced by the dry
//! input (same fades) and counted as a timeout.
//!
//! Nothing before kH depends on window k, so its result has the full ceil(0.3 H) after the
//! window completes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::dsp::SAMPLE_RATE;
use crate::separator::{check_block, Separator};

pub const WINDOWED_HOP: usize = 128;
/// Crossfade at the start of each segment: 5 ms, linear.
pub const XF: usize = 220;
/// Windows queued for the worker; a window that finds the queue full times out.
const JOB_SLOTS: usize = 2;
const RESULT_SLOTS: usize = 4;
const DUTY_WINDOW_S: usize = 10;
const DUTY_LIMIT: f32 = 0.3;
const OVERLOAD_TIMEOUTS: u32 = 3;
/// An idle worker re-checks `stop` at least this often (wakeups normally come from `unpark`).
const IDLE_PARK: Duration = Duration::from_millis(50);

pub trait WindowModel: Send + 'static {
    /// input: interleaved stereo, window_frames*2; writes vocals the same shape.
    fn run(&mut self, input: &[f32], vocals: &mut [f32]) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug)]
pub struct WindowedParams {
    pub window_frames: usize,
    pub hop_frames: usize,
    pub lookahead_frames: usize,
}

/// Monotonic nanoseconds; injected so tests can simulate inference time.
type Clock = Box<dyn Fn() -> u64 + Send>;

#[derive(Clone, Copy)]
struct Job {
    generation: u64,
    k: u64,
}

#[derive(Clone, Copy)]
struct Outcome {
    generation: u64,
    k: u64,
    elapsed_ns: u64,
}

struct Shared {
    generation: AtomicU64,
    /// Jobs the worker has finished (run or skipped as stale).
    done: AtomicU64,
    stop: AtomicBool,
    duty_bits: AtomicU32,
}

pub struct WindowedSeparator {
    w: usize,
    h: usize,
    l: usize,
    latency: usize,
    budget_ns: u64,
    generation: u64,
    /// Input history, interleaved, circular over `hist_frames`.
    hist: Vec<f32>,
    hist_frames: usize,
    /// Stitched accompaniment by input frame, circular over `stage_frames`.
    stage: Vec<f32>,
    stage_frames: usize,
    /// One segment, `(H + XF) * 2` samples.
    seg: Vec<f32>,
    /// Frames consumed so far.
    pos: u64,
    next_job: u64,
    next_seg: u64,
    submitted: u64,
    timeouts: u64,
    consecutive: u32,
    /// Latched once `consecutive` reaches 3 (a run of timeouts would otherwise clear on the
    /// next good segment, before anyone polls `overloaded`).
    tripped: bool,
    jobs: Producer<Job>,
    windows: Producer<f32>,
    outcomes: Consumer<Outcome>,
    segs: Consumer<f32>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

fn ramp(i: usize) -> f32 {
    (i as f32 + 0.5) / XF as f32
}

fn frames_to_ns(frames: usize) -> f64 {
    frames as f64 * 1e9 / f64::from(SAMPLE_RATE)
}

impl WindowedSeparator {
    /// Spawns the inference worker. Panics on geometry that cannot work: the window must hold
    /// hop + lookahead, and both hop and lookahead must cover the crossfade.
    pub fn new(model: Box<dyn WindowModel>, p: WindowedParams) -> Self {
        let t0 = Instant::now();
        Self::with_clock(model, p, Box::new(move || t0.elapsed().as_nanos() as u64))
    }

    fn with_clock(model: Box<dyn WindowModel>, p: WindowedParams, clock: Clock) -> Self {
        let (w, h, l) = (p.window_frames, p.hop_frames, p.lookahead_frames);
        let budget = (3 * h).div_ceil(10);
        assert!(w >= h + l, "window {w} < hop {h} + lookahead {l}");
        assert!(
            h >= XF && l >= XF,
            "hop {h} and lookahead {l} must cover the {XF}-frame crossfade"
        );
        let latency = h + l + budget;
        let seg_len = (h + XF) * 2;
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            done: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            duty_bits: AtomicU32::new(0),
        });
        let (jobs, job_rx) = RingBuffer::new(JOB_SLOTS);
        let (windows, window_rx) = RingBuffer::new(JOB_SLOTS * w * 2);
        let (outcome_tx, outcomes) = RingBuffer::new(RESULT_SLOTS);
        let (seg_tx, segs) = RingBuffer::new(RESULT_SLOTS * seg_len);
        let worker = Worker {
            model,
            clock,
            p,
            shared: shared.clone(),
            jobs: job_rx,
            windows: window_rx,
            outcomes: outcome_tx,
            segs: seg_tx,
        };
        let handle = thread::Builder::new()
            .name("devocal-window-infer".into())
            .spawn(move || worker.run())
            .expect("spawn inference thread");
        // History reaches back a whole window, and to a dry segment emitted at the latency.
        let hist_frames = w + latency + h + XF + 2 * WINDOWED_HOP;
        let stage_frames = h + XF + 2 * WINDOWED_HOP;
        Self {
            w,
            h,
            l,
            latency,
            budget_ns: frames_to_ns(budget) as u64,
            generation: 0,
            hist: vec![0.0; hist_frames * 2],
            hist_frames,
            stage: vec![0.0; stage_frames * 2],
            stage_frames,
            seg: vec![0.0; seg_len],
            pos: 0,
            next_job: 0,
            next_seg: 0,
            submitted: 0,
            timeouts: 0,
            consecutive: 0,
            tripped: false,
            jobs,
            windows,
            outcomes,
            segs,
            shared,
            worker: Some(handle),
        }
    }

    /// Segments replaced by dry input (late, failed or dropped inference).
    pub fn timeouts(&self) -> u64 {
        self.timeouts
    }

    /// Mean inference time / hop over the last 10 s of windows.
    pub fn duty_10s(&self) -> f32 {
        f32::from_bits(self.shared.duty_bits.load(Ordering::Relaxed))
    }

    fn hist_at(&self, frame: i64, ch: usize) -> f32 {
        if frame < 0 {
            0.0
        } else {
            self.hist[(frame as usize % self.hist_frames) * 2 + ch]
        }
    }

    /// Queues the window ending at kH + H + L. If the worker's queue is full the window is
    /// skipped and its segment will time out.
    fn submit(&mut self, k: u64) {
        let n = self.w * 2;
        if self.jobs.slots() == 0 || self.windows.slots() < n {
            return;
        }
        let start = (k as usize * self.h + self.h + self.l) as i64 - self.w as i64;
        let Ok(chunk) = self.windows.write_chunk_uninit(n) else {
            return;
        };
        let (hist, hist_frames) = (&self.hist, self.hist_frames);
        chunk.fill_from_iter((0..n).map(|i| {
            let f = start + (i / 2) as i64;
            if f < 0 {
                0.0
            } else {
                hist[(f as usize % hist_frames) * 2 + i % 2]
            }
        }));
        // Header after data: a visible job always has its window.
        let _ = self.jobs.push(Job {
            generation: self.generation,
            k,
        });
        self.submitted += 1;
    }

    /// Drops the front outcome and its segment (the worker pushes data before the header,
    /// so a visible header always has its data).
    fn discard_outcome(&mut self) {
        let _ = self.outcomes.pop();
        if let Ok(chunk) = self.segs.read_chunk(self.seg.len()) {
            chunk.commit_all();
        }
    }

    /// Fills `seg` for segment k from its result or, failing that, the dry input, then
    /// stitches it into `stage`.
    fn resolve(&mut self, k: u64) {
        let mut fresh = false;
        while let Ok(&o) = self.outcomes.peek() {
            if o.generation == self.generation && o.k > k {
                break;
            }
            if o.generation == self.generation && o.k == k && o.elapsed_ns <= self.budget_ns {
                let _ = self.outcomes.pop();
                fresh = self.segs.pop_entire_slice(&mut self.seg).is_ok();
                break;
            }
            // Older segment, previous generation, or over the time limit.
            self.discard_outcome();
        }
        let first = k as usize * self.h;
        if fresh {
            self.consecutive = 0;
        } else {
            for i in 0..self.h + XF {
                for c in 0..2 {
                    self.seg[i * 2 + c] = self.hist_at((first + i) as i64, c);
                }
            }
            self.timeouts += 1;
            self.consecutive += 1;
            self.tripped |= self.consecutive >= OVERLOAD_TIMEOUTS;
        }
        // `stage` over [kH, kH+XF) still holds segment k-1's tail (zeros before segment 0).
        for i in 0..self.h + XF {
            let at = ((first + i) % self.stage_frames) * 2;
            for c in 0..2 {
                let v = self.seg[i * 2 + c];
                let s = &mut self.stage[at + c];
                *s = if i < XF {
                    let r = ramp(i);
                    *s * (1.0 - r) + v * r
                } else {
                    v
                };
            }
        }
    }
}

impl Separator for WindowedSeparator {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn hop(&self) -> usize {
        WINDOWED_HOP
    }

    fn latency_frames(&self) -> usize {
        self.latency
    }

    fn passthrough_latency_frames(&self) -> usize {
        WINDOWED_HOP
    }

    /// 3 consecutive timeouts since the last `reset` (latched), or `duty_10s() > 0.3`.
    fn overloaded(&self) -> bool {
        self.tripped || self.duty_10s() > DUTY_LIMIT
    }

    fn process(&mut self, input: &[f32], out_accompaniment: &mut [f32]) -> Result<(), String> {
        check_block(WINDOWED_HOP, input, out_accompaniment)?;
        if !input.iter().all(|s| s.is_finite()) {
            return Err("non-finite input sample".into());
        }
        for (i, frame) in input.as_chunks::<2>().0.iter().enumerate() {
            let at = ((self.pos as usize + i) % self.hist_frames) * 2;
            self.hist[at..at + 2].copy_from_slice(frame);
        }
        self.pos += WINDOWED_HOP as u64;

        let before = self.submitted;
        while self.next_job * self.h as u64 + (self.h + self.l) as u64 <= self.pos {
            self.submit(self.next_job);
            self.next_job += 1;
        }
        if self.submitted != before {
            if let Some(worker) = &self.worker {
                worker.thread().unpark();
            }
        }

        // Output frame t plays input frame t - latency. Resolve every segment touching this
        // block first (input frame a needs segment floor(a / H), and the ones before it).
        let first = self.pos as i64 - (WINDOWED_HOP + self.latency) as i64;
        let end = first + WINDOWED_HOP as i64;
        if end > 0 {
            let need = (end as u64 - 1) / self.h as u64;
            while self.next_seg <= need {
                self.resolve(self.next_seg);
                self.next_seg += 1;
            }
        }
        for (j, out) in out_accompaniment
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .enumerate()
        {
            let a = first + j as i64;
            if a < 0 {
                out.fill(0.0);
            } else {
                let at = (a as usize % self.stage_frames) * 2;
                out.copy_from_slice(&self.stage[at..at + 2]);
            }
        }
        Ok(())
    }

    /// Restarts the stream. Queued and in-flight windows belong to the old generation and are
    /// dropped (by the worker, or here when their results come back). The 3-timeouts overload
    /// latch is cleared; the duty mean is a property of the device and keeps rolling.
    fn reset(&mut self) {
        self.generation += 1;
        self.shared
            .generation
            .store(self.generation, Ordering::Release);
        while self.outcomes.peek().is_ok() {
            self.discard_outcome();
        }
        self.hist.fill(0.0);
        self.stage.fill(0.0);
        self.pos = 0;
        self.next_job = 0;
        self.next_seg = 0;
        self.consecutive = 0;
        self.tripped = false;
    }
}

impl Drop for WindowedSeparator {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

/// The inference thread (may allocate and block).
struct Worker {
    model: Box<dyn WindowModel>,
    clock: Clock,
    p: WindowedParams,
    shared: Arc<Shared>,
    jobs: Consumer<Job>,
    windows: Consumer<f32>,
    outcomes: Producer<Outcome>,
    segs: Producer<f32>,
}

impl Worker {
    fn run(mut self) {
        let WindowedParams {
            window_frames: w,
            hop_frames: h,
            lookahead_frames: l,
        } = self.p;
        let mut win = vec![0.0f32; w * 2];
        let mut vocals = vec![0.0f32; w * 2];
        let mut acc = vec![0.0f32; (h + XF) * 2];
        let part = (w - l - h) * 2..(w - l + XF) * 2;
        let hop_ns = frames_to_ns(h);
        let duty_len = (DUTY_WINDOW_S * SAMPLE_RATE as usize).div_ceil(h);
        let mut duty: VecDeque<f64> = VecDeque::with_capacity(duty_len + 1);
        while !self.shared.stop.load(Ordering::Acquire) {
            let Ok(job) = self.jobs.pop() else {
                thread::park_timeout(IDLE_PARK);
                continue;
            };
            let _ = self.windows.pop_entire_slice(&mut win);
            if job.generation == self.shared.generation.load(Ordering::Acquire) {
                let t0 = (self.clock)();
                let ok = self.model.run(&win, &mut vocals).is_ok();
                let elapsed_ns = (self.clock)().saturating_sub(t0);

                duty.push_back(elapsed_ns as f64 / hop_ns);
                if duty.len() > duty_len {
                    duty.pop_front();
                }
                let mean = duty.iter().sum::<f64>() / duty.len() as f64;
                self.shared
                    .duty_bits
                    .store((mean as f32).to_bits(), Ordering::Relaxed);

                let pairs = win[part.clone()].iter().zip(&vocals[part.clone()]);
                for (a, (x, v)) in acc.iter_mut().zip(pairs) {
                    *a = x - v;
                }
                // A failed run or non-finite output is not delivered: the segment times out
                // and plays dry.
                if ok
                    && acc.iter().all(|s| s.is_finite())
                    && self.outcomes.slots() > 0
                    && self.segs.push_entire_slice(&acc).is_ok()
                {
                    let _ = self.outcomes.push(Outcome {
                        generation: job.generation,
                        k: job.k,
                        elapsed_ns,
                    });
                }
            }
            self.shared.done.fetch_add(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stemgen::alloc_count;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Test geometry: H is not a multiple of the 128-frame block, L covers the crossfade tail,
    /// and the inference budget (ceil(0.3 H) = 375 frames) exceeds one block, so a paced
    /// stream never needs a window in the same call that submits it.
    const P: WindowedParams = WindowedParams {
        window_frames: 2000,
        hop_frames: 1250,
        lookahead_frames: 300,
    };
    const H: usize = 1250;
    const L: usize = 300;
    const BUDGET: usize = 375;
    const D: usize = H + L + BUDGET;
    const H_NS: u64 = 1250 * 1_000_000_000 / 44_100;

    /// Fake model: vocals = 0.5 x window. Each run "takes" `sleep(call index)` x H on the
    /// injected clock; one run (by call index) can be held on a gate.
    struct Fake {
        clock: Arc<AtomicU64>,
        sleep: Box<dyn Fn(usize) -> f64 + Send>,
        calls: usize,
        /// (call index to hold, set on entering it, release)
        gate: Option<(usize, Arc<AtomicBool>, Receiver<()>)>,
        dropped: Option<Arc<AtomicBool>>,
    }

    impl WindowModel for Fake {
        fn run(&mut self, input: &[f32], vocals: &mut [f32]) -> Result<(), String> {
            if self.gate.as_ref().is_some_and(|g| g.0 == self.calls) {
                let (_, entered, release) = self.gate.take().unwrap();
                entered.store(true, Ordering::SeqCst);
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            for (v, x) in vocals.iter_mut().zip(input) {
                *v = 0.5 * x;
            }
            let ns = ((self.sleep)(self.calls) * H_NS as f64) as u64;
            self.clock.fetch_add(ns, Ordering::SeqCst);
            self.calls += 1;
            Ok(())
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            if let Some(flag) = &self.dropped {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    fn fake(sleep: impl Fn(usize) -> f64 + Send + 'static) -> (Fake, Clock) {
        let clock = Arc::new(AtomicU64::new(0));
        let read = clock.clone();
        let model = Fake {
            clock,
            sleep: Box::new(sleep),
            calls: 0,
            gate: None,
            dropped: None,
        };
        (model, Box::new(move || read.load(Ordering::SeqCst)))
    }

    fn make(sleep: impl Fn(usize) -> f64 + Send + 'static) -> WindowedSeparator {
        let (model, clock) = fake(sleep);
        WindowedSeparator::with_clock(Box::new(model), P, clock)
    }

    /// Waits (bounded) until the worker has finished every submitted window.
    fn wait_idle(sep: &WindowedSeparator) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while sep.shared.done.load(Ordering::Acquire) < sep.submitted {
            assert!(Instant::now() < deadline, "worker did not finish in 2 s");
            std::thread::yield_now();
        }
    }

    /// Feeds whole blocks, letting the worker finish after each one (inference is instant
    /// in stream time; lateness comes only from the fake model's simulated duration).
    fn run_paced(sep: &mut WindowedSeparator, input: &[f32]) -> Vec<f32> {
        let n = WINDOWED_HOP * 2;
        let mut out = vec![0.0f32; input.len()];
        for (i, o) in input.chunks_exact(n).zip(out.chunks_exact_mut(n)) {
            sep.process(i, o).unwrap();
            wait_idle(sep);
        }
        out
    }

    /// Same generator as scripts/models/stitch_fixture.py: xorshift32, interleaved stereo,
    /// sample = (x >> 8) / 2^24 - 0.5 (exact in f32).
    fn noise(seed: u32, frames: usize) -> Vec<f32> {
        let mut x = seed;
        (0..frames * 2)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 8) as f32 / 16_777_216.0 - 0.5
            })
            .collect()
    }

    fn max_diff(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max)
    }

    fn base64(s: &str) -> Vec<u8> {
        let val = |c: u8| match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("bad base64 byte {c}"),
        };
        let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
        let mut out = Vec::new();
        for chunk in bytes.chunks(4) {
            let mut acc = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                acc |= u32::from(val(c)) << (18 - 6 * i);
            }
            out.extend_from_slice(&acc.to_be_bytes()[1..chunk.len()]);
        }
        out
    }

    #[test]
    fn latency_is_fixed() {
        let mut sep = make(|_| 0.0);
        assert_eq!(sep.latency_frames(), D);
        assert_eq!(sep.passthrough_latency_frames(), 128);
        assert_eq!(sep.hop(), 128);
        assert_eq!(sep.sample_rate(), 44_100);
        // One impulse mid-segment, one inside a crossfade ([2H, 2H + XF)).
        let frames = 40 * 128;
        let mut input = vec![0.0f32; frames * 2];
        for p in [2600, 3000] {
            input[p * 2] = 1.0;
            input[p * 2 + 1] = -1.0;
        }
        let out = run_paced(&mut sep, &input);
        for t in 0..frames {
            let want = if t == 2600 + D || t == 3000 + D {
                0.5
            } else {
                0.0
            };
            assert!(
                (out[t * 2] - want).abs() <= 1e-6,
                "frame {t}: {}",
                out[t * 2]
            );
            assert!((out[t * 2 + 1] + want).abs() <= 1e-6, "frame {t}");
        }
        assert_eq!(sep.timeouts(), 0);
    }

    #[test]
    fn matches_python_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/windowed_stitch.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (generate with scripts/models/stitch_fixture.py)",
                path.display()
            )
        });
        let fx: serde_json::Value = serde_json::from_str(&text).unwrap();
        let num = |k: &str| fx[k].as_u64().unwrap() as usize;
        assert_eq!(num("window_frames"), P.window_frames);
        assert_eq!(num("hop_frames"), P.hop_frames);
        assert_eq!(num("lookahead_frames"), P.lookahead_frames);
        assert_eq!(num("crossfade_frames"), XF);
        assert_eq!(num("latency_frames"), D);
        let input = noise(num("seed") as u32, num("input_frames"));
        let head: Vec<f32> = fx["input_head"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        assert_eq!(&input[..head.len()], &head[..], "input generator differs");
        let samples = |k: &str| -> Vec<f32> {
            base64(fx[k].as_str().unwrap())
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        };

        // Every window on time.
        let expected = samples("output_f32le_b64");
        let start = num("output_start_frame");
        assert_eq!(start, D);
        let mut sep = make(|_| 0.0);
        let out = run_paced(&mut sep, &input);
        assert!(
            out[..start * 2].iter().all(|&s| s == 0.0),
            "pre-roll is silent"
        );
        let diff = max_diff(&out[start * 2..], &expected);
        println!(
            "max |rust - python| = {diff:e} over {} samples",
            expected.len()
        );
        assert!(diff <= 1e-6, "max diff {diff}");
        assert_eq!(sep.timeouts(), 0);

        // One segment timed out: its dry fades pin the crossfade ramp.
        let late = num("late_segment");
        let expected = samples("late_output_f32le_b64");
        let start = num("late_output_start_frame");
        let mut sep = make(move |k| if k == late { 0.5 } else { 0.0 });
        let out = run_paced(&mut sep, &input);
        let diff = max_diff(&out[start * 2..start * 2 + expected.len()], &expected);
        println!("late segment: max |rust - python| = {diff:e}");
        assert!(diff <= 1e-6, "late segment max diff {diff}");
        assert_eq!(sep.timeouts(), 1);
    }

    #[test]
    fn timeout_outputs_dry_aligned_audio() {
        let mut sep = make(|k| if k == 2 { 0.5 } else { 0.0 });
        let frames = 48 * 128;
        let input = noise(7, frames);
        let out = run_paced(&mut sep, &input);
        assert_eq!(sep.timeouts(), 1);
        assert!(!sep.overloaded());
        // Accompaniment per input frame a: 0.5 x (faded in from silence over [0, XF)) except
        // segment 2 ([2H, 3H + XF)), which is the dry input, faded in over [2H, 2H + XF) and
        // out (as the tail under segment 3) over [3H, 3H + XF).
        assert!(frames - D >= 3 * H + XF);
        for a in 0..frames - D {
            let gain = if a < XF {
                0.5 * ramp(a)
            } else if (2 * H..2 * H + XF).contains(&a) {
                let r = ramp(a - 2 * H);
                0.5 * (1.0 - r) + r
            } else if (2 * H + XF..3 * H).contains(&a) {
                1.0
            } else if (3 * H..3 * H + XF).contains(&a) {
                let r = ramp(a - 3 * H);
                (1.0 - r) + 0.5 * r
            } else {
                0.5
            };
            for c in 0..2 {
                let got = out[(a + D) * 2 + c];
                let want = gain * input[a * 2 + c];
                assert!(
                    (got - want).abs() <= 1e-6,
                    "frame {a} ch {c}: {got} vs {want}"
                );
            }
            assert!(
                out[(a + D) * 2] != 0.0 || out[(a + D) * 2 + 1] != 0.0,
                "silent hole at {a}"
            );
        }
    }

    #[test]
    fn three_consecutive_timeouts_overload() {
        // Scattered timeouts never overload.
        let mut sep = make(|k| if [2, 4, 6].contains(&k) { 0.5 } else { 0.0 });
        let input = noise(3, 80 * 128);
        let n = WINDOWED_HOP * 2;
        let mut out = vec![0.0f32; n];
        for block in input.chunks_exact(n) {
            sep.process(block, &mut out).unwrap();
            wait_idle(&sep);
            assert!(!sep.overloaded());
        }
        assert_eq!(sep.timeouts(), 3);

        // Three in a row do, exactly when the third one lands.
        let mut sep = make(|k| if (2..5).contains(&k) { 0.5 } else { 0.0 });
        for block in input.chunks_exact(n) {
            sep.process(block, &mut out).unwrap();
            wait_idle(&sep);
            assert_eq!(sep.overloaded(), sep.timeouts() >= 3);
        }
        assert_eq!(sep.timeouts(), 3);
        assert!(sep.overloaded());
        // A reset starts a fresh generation: the latch clears (duty here is ~0.21).
        sep.reset();
        assert!(sep.duty_10s() < 0.3);
        assert!(!sep.overloaded());
    }

    #[test]
    fn full_budget_is_usable() {
        // Window 2 completes at input frame e; its result is first needed by the call that
        // emits input frame 2H. Hold the result until after the call before that one.
        let e = 3 * H + L;
        let submit_call = e.div_ceil(WINDOWED_HOP) - 1;
        let need_call = (2 * H + D + 1).div_ceil(WINDOWED_HOP) - 1;
        let last_ok = need_call - 1;
        let arrival = (last_ok + 1) * WINDOWED_HOP - e;
        assert!(
            arrival >= BUDGET - WINDOWED_HOP,
            "arrival {arrival} frames after the window covers ceil(0.3 H) - 1 block"
        );
        let (mut model, clock) = fake(|k| {
            if k == 2 {
                (BUDGET - WINDOWED_HOP) as f64 / H as f64
            } else {
                0.0
            }
        });
        let (release, gate) = channel();
        model.gate = Some((2, Arc::new(AtomicBool::new(false)), gate));
        let mut sep = WindowedSeparator::with_clock(Box::new(model), P, clock);
        let frames = 46 * 128;
        let input = noise(13, frames);
        let mut out = vec![0.0f32; frames * 2];
        let n = WINDOWED_HOP * 2;
        for (c, (block, o)) in input
            .chunks_exact(n)
            .zip(out.chunks_exact_mut(n))
            .enumerate()
        {
            sep.process(block, o).unwrap();
            if c == last_ok {
                assert_eq!(sep.timeouts(), 0);
                release.send(()).unwrap();
            }
            if c < submit_call || c >= last_ok {
                wait_idle(&sep);
            }
        }
        assert_eq!(sep.timeouts(), 0, "a result within the budget is used");
        for a in 2 * H..3 * H {
            for c in 0..2 {
                let want = 0.5 * input[a * 2 + c];
                assert!((out[(a + D) * 2 + c] - want).abs() <= 1e-6, "frame {a}");
            }
        }

        // Measured inference time: exactly ceil(0.3 H) is on time, one frame more is not.
        for (frames_taken, timeouts) in [(BUDGET, 0), (BUDGET + 1, 1)] {
            let mut sep = make(move |k| {
                if k == 2 {
                    frames_taken as f64 / H as f64
                } else {
                    0.0
                }
            });
            run_paced(&mut sep, &input);
            assert_eq!(sep.timeouts(), timeouts, "{frames_taken} frames");
        }
    }

    /// Feeds silent blocks until `jobs` windows have been submitted and finished.
    fn run_jobs(sep: &mut WindowedSeparator, jobs: u64) {
        let input = [0.0f32; WINDOWED_HOP * 2];
        let mut out = [0.0f32; WINDOWED_HOP * 2];
        while sep.submitted < jobs {
            sep.process(&input, &mut out).unwrap();
            wait_idle(sep);
        }
    }

    #[test]
    fn high_duty_overloads() {
        // Hops in 10 s of stream.
        let n = (10 * 44_100u64).div_ceil(H as u64);
        let mut sep = make(|_| 0.35);
        run_jobs(&mut sep, n);
        assert!((sep.duty_10s() - 0.35).abs() < 1e-3, "{}", sep.duty_10s());
        assert!(sep.overloaded());

        // Duty alone (never three timeouts in a row): 0.25/0.45 alternating for 10 s, then
        // 10 s of instant runs push the busy stretch out of the window.
        let mut sep = make(move |k| match k {
            k if k as u64 >= n => 0.0,
            k if k % 2 == 0 => 0.25,
            _ => 0.45,
        });
        run_jobs(&mut sep, n);
        assert!((sep.duty_10s() - 0.35).abs() < 1e-2, "{}", sep.duty_10s());
        assert!(sep.overloaded());
        run_jobs(&mut sep, 2 * n);
        assert_eq!(sep.duty_10s(), 0.0);
        assert!(!sep.overloaded());
    }

    #[test]
    fn reset_drops_late_results() {
        let (mut model, clock) = fake(|_| 0.0);
        let (release, gate) = channel();
        let entered = Arc::new(AtomicBool::new(false));
        model.gate = Some((0, entered.clone(), gate));
        let mut sep = WindowedSeparator::with_clock(Box::new(model), P, clock);
        // Window 0 is submitted and held inside the model.
        let loud = vec![1.0f32; WINDOWED_HOP * 2];
        let mut out = vec![0.0f32; WINDOWED_HOP * 2];
        while sep.submitted == 0 {
            sep.process(&loud, &mut out).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !entered.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "model never started");
            std::thread::yield_now();
        }
        sep.reset();
        release.send(()).unwrap();
        wait_idle(&sep);

        // After the reset the late (old-generation) window 0 must not be used.
        let input = noise(11, 40 * 128);
        let after = run_paced(&mut sep, &input);
        let fresh = run_paced(&mut make(|_| 0.0), &input);
        assert_eq!(after, fresh);
    }

    #[test]
    fn drop_joins_worker() {
        let (mut model, clock) = fake(|_| 0.0);
        let dropped = Arc::new(AtomicBool::new(false));
        model.dropped = Some(dropped.clone());
        let mut sep = WindowedSeparator::with_clock(Box::new(model), P, clock);
        run_paced(&mut sep, &noise(5, 20 * 128));
        drop(sep);
        assert!(
            dropped.load(Ordering::SeqCst),
            "worker (and model) gone after drop"
        );
    }

    #[test]
    fn process_does_not_allocate() {
        // Every third window times out, so both the model and the dry path run.
        let mut sep = make(|k| if k % 3 == 0 { 0.5 } else { 0.0 });
        let input = noise(9, 200 * 128);
        let n = WINDOWED_HOP * 2;
        let mut out = vec![0.0f32; n];
        let mut blocks = input.chunks_exact(n);
        for block in blocks.by_ref().take(20) {
            sep.process(block, &mut out).unwrap();
            wait_idle(&sep);
        }
        let before = alloc_count::this_thread();
        for block in blocks {
            sep.process(block, &mut out).unwrap();
            wait_idle(&sep);
        }
        assert_eq!(
            alloc_count::this_thread() - before,
            0,
            "allocations in process()"
        );
        assert!(sep.timeouts() > 0 && sep.submitted > 15);
        let before = alloc_count::this_thread();
        drop(std::hint::black_box(vec![1u8; 16]));
        assert!(alloc_count::this_thread() > before, "the counter works");
    }

    #[test]
    fn rejects_bad_blocks() {
        let mut sep = make(|_| 0.0);
        let mut out = vec![0.0f32; 256];
        assert!(sep.process(&[0.0; 255], &mut out).is_err());
        assert!(sep.process(&[0.0; 256], &mut [0.0; 128]).is_err());
        let mut bad = vec![0.0f32; 256];
        bad[3] = f32::NAN;
        assert!(sep.process(&bad, &mut out).is_err());
    }
}
