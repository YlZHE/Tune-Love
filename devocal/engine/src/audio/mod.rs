//! Engine audio I/O: three threads joined by lock-free SPSC rings (`rtrb`).
//!
//! ```text
//!  player process tree                                                   player's endpoint
//!        |  process loopback (44.1 kHz f32 stereo, autoconvert)                 ^
//!        v                                                                      |
//!  [capture] --samples--> ring A --> [processing] --samples--> ring B --> [render]
//!     |  x capture gain     (1 s)       hop blocks,              (1 s)   target queue =
//!     |  guard (peak > 1.5) ------.     Processor,                       period + hop;
//!     '--markers (discontinuity,  |     LoadMonitor          '--markers  trim, underrun
//!        guard gap) ----------------->  (overload/underrun     (fade-out  fades, x output
//!                                 |      -> force_fallback)     before a  gain, clamp
//!                                 v                             gap)      [-1, 1]
//!                          follow_now (engine loop runs Holder::follow at once)
//!
//!  control: AudioHandle --rtrb--> processing (set_devocal, set_separator)
//!           AudioHandle --rtrb--> render     (rebind_output)
//!  gains:   engine loop --SharedGains atomics--> capture (capture gain), render (output gain)
//! ```
//!
//! Every thread initialises COM in the MTA where it uses COM (capture, render), asks MMCSS for
//! "Pro Audio" priority, and never blocks without a timeout, so `stop` always returns: it sets
//! the stop flag, wakes the processing thread, waits up to [`STOP_TIMEOUT_MS`] for the threads
//! to finish and joins those that did (a thread stuck inside a driver call is left detached
//! rather than hanging the engine).
//!
//! After start-up the per-block paths neither allocate nor lock: rings and scratch buffers are
//! preallocated, control messages are drained between blocks on the owning thread, and the
//! gains are read from atomics. The [`Holder`](crate::holder::Holder) is never touched here.

pub mod capture;
pub mod endpoint;
mod processing;
pub mod render;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rtrb::{Producer, PushError, RingBuffer};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent,
    WaitForSingleObject,
};

use crate::dsp::{frames_for_ms, SAMPLE_RATE};
use crate::processor::Processor;
use crate::separator::Separator;

/// An underrun counts only if capture data arrived this recently.
pub const INPUT_FLOWING_US: u64 = 50_000;
/// Edge fades around dropouts, guard gaps and discontinuities.
pub const EDGE_FADE_MS: f32 = 5.0;
/// Each ring holds this much audio.
const RING_FRAMES: usize = SAMPLE_RATE as usize;
const MARKER_SLOTS: usize = 256;
const CONTROL_SLOTS: usize = 64;
/// How long `start` waits for a thread to open its device.
const START_TIMEOUT_MS: u64 = 5_000;
/// How long `stop` waits for the threads before leaving a stuck one detached.
pub const STOP_TIMEOUT_MS: u64 = 2_000;

/// Frames of the 5 ms edge fade (221 at 44.1 kHz).
pub fn edge_fade_frames() -> usize {
    frames_for_ms(EDGE_FADE_MS)
}

pub struct AudioConfig {
    /// Root of the player's process tree (process loopback includes the tree).
    pub pid: u32,
    /// Must equal the engine rate (44 100 Hz).
    pub sample_rate: u32,
    /// Must equal the processor's `hop()` at start.
    pub hop: usize,
    /// Endpoint to render to at start (`None` = default render endpoint). Later changes go
    /// through [`AudioHandle::rebind_output`].
    pub output_endpoint: Option<String>,
}

#[derive(Debug, Default)]
pub struct AudioStats {
    /// Render starvations while input was flowing (one per gap).
    pub underruns: AtomicU64,
    /// Milliseconds since the last capture packet (since start if none arrived yet).
    pub input_silent_ms: AtomicU64,
    /// `LoadMonitor::ratio() * 1000` while the model runs, 0 otherwise.
    pub load_ratio_milli: AtomicU32,
    /// Estimated pipeline latency in microseconds (milli-milliseconds).
    pub latency_ms_milli: AtomicU32,
    /// Capture packets flagged discontinuous / timestamp error, plus ring overflows.
    pub discontinuities: AtomicU64,
    /// Blocks silenced by the safety guard (peak above 1.5 after the capture gain).
    pub unattenuated_blocks: AtomicU64,
}

/// Gains as `f32` bit patterns; published by the engine loop from the Holder every 1 ms.
#[derive(Debug)]
pub struct SharedGains {
    pub capture_gain_bits: AtomicU32,
    pub output_gain_bits: AtomicU32,
}

impl SharedGains {
    pub fn new(capture_gain: f32, output_gain: f32) -> Self {
        Self {
            capture_gain_bits: AtomicU32::new(capture_gain.to_bits()),
            output_gain_bits: AtomicU32::new(output_gain.to_bits()),
        }
    }

    pub fn set(&self, capture_gain: f32, output_gain: f32) {
        self.capture_gain_bits
            .store(capture_gain.to_bits(), Ordering::Relaxed);
        self.output_gain_bits
            .store(output_gain.to_bits(), Ordering::Relaxed);
    }

    /// Non-finite or negative values read as 0 (silent rather than loud).
    pub fn capture_gain(&self) -> f32 {
        sane_gain(f32::from_bits(
            self.capture_gain_bits.load(Ordering::Relaxed),
        ))
    }

    pub fn output_gain(&self) -> f32 {
        sane_gain(f32::from_bits(
            self.output_gain_bits.load(Ordering::Relaxed),
        ))
    }
}

fn sane_gain(g: f32) -> f32 {
    if g.is_finite() && g > 0.0 {
        g
    } else {
        0.0
    }
}

/// An underrun counts only while input is flowing: the last capture packet arrived within
/// [`INPUT_FLOWING_US`] of `now_us`. `last_input_us == 0` means no input yet. A packet stamped
/// after `now_us` (read race between threads) counts as flowing.
pub fn count_underrun(last_input_us: u64, now_us: u64) -> bool {
    last_input_us != 0 && now_us.saturating_sub(last_input_us) <= INPUT_FLOWING_US
}

/// Stateful linear fade-in for interleaved stereo that continues across blocks: `skip`
/// frames of silence, then the `fade_edges` ramp (`i / (len - 1)`, first frame 0) over `len`
/// frames. Idle (no effect) until `start`.
pub(crate) struct FadeIn {
    skip: usize,
    pos: usize,
    len: usize,
}

impl FadeIn {
    pub fn new(len: usize) -> Self {
        Self {
            skip: 0,
            pos: len,
            len,
        }
    }

    pub fn start(&mut self, skip_frames: usize) {
        self.skip = skip_frames;
        self.pos = 0;
    }

    pub fn apply(&mut self, block: &mut [f32]) {
        for f in block.chunks_exact_mut(2) {
            if self.skip > 0 {
                f[0] = 0.0;
                f[1] = 0.0;
                self.skip -= 1;
                continue;
            }
            if self.pos >= self.len {
                return;
            }
            let g = if self.len <= 1 {
                0.0
            } else {
                self.pos as f32 / (self.len - 1) as f32
            };
            f[0] *= g;
            f[1] *= g;
            self.pos += 1;
        }
    }
}

/// Where a capture-side event happened, in frames pushed to ring A.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputMarker {
    /// The input jumps here (packet flag or dropped data): reset the model state.
    Discontinuity(u64),
    /// The safety guard silenced input from here on.
    GuardGap(u64),
}

pub(crate) enum ProcCommand {
    SetDevocal(bool),
    SetSeparator(Box<dyn Separator>),
}

pub(crate) enum RenderCommand {
    Rebind(Option<String>),
}

/// State shared by the audio threads (atomics only).
pub(crate) struct Shared {
    pub stop: AtomicBool,
    pub start_us: u64,
    /// `now_us()` of the latest capture packet, 0 before the first.
    pub last_input_us: AtomicU64,
    /// Frames in the latest capture packet (capture period estimate).
    pub capture_packet_frames: AtomicU32,
    /// Frames waiting in ring A (published by the processing thread).
    pub in_ring_frames: AtomicU32,
    /// Processor latency and hop (published by the processing thread).
    pub proc_latency_frames: AtomicU32,
    pub proc_hop: AtomicU32,
    pub output_failed: AtomicBool,
    pub capture_failed: AtomicBool,
    /// Wakes the processing thread (capture pushed data, control message, stop).
    pub wake: OwnedEvent,
}

pub struct AudioHandle {
    pub stats: Arc<AudioStats>,
    /// Set by the safety guard; the engine loop runs `Holder::follow()` immediately and
    /// clears it (see [`AudioHandle::take_follow_request`]).
    pub follow_now: Arc<AtomicBool>,
    shared: Arc<Shared>,
    proc_ctl: Mutex<Producer<ProcCommand>>,
    render_ctl: Mutex<Producer<RenderCommand>>,
    workers: Vec<Worker>,
}

impl AudioHandle {
    /// Starts the processing, capture and render threads. Fails (with every started thread
    /// stopped again) if the config does not match the engine, or if process loopback or the
    /// output endpoint cannot be opened within 5 s.
    pub fn start(
        cfg: AudioConfig,
        gains: Arc<SharedGains>,
        processor: Processor,
    ) -> Result<Self, String> {
        if cfg.sample_rate != SAMPLE_RATE {
            return Err(format!(
                "sample rate {} not supported (engine runs at {SAMPLE_RATE})",
                cfg.sample_rate
            ));
        }
        if cfg.hop != processor.hop() {
            return Err(format!(
                "hop {} does not match the processor's {}",
                cfg.hop,
                processor.hop()
            ));
        }
        let stats = Arc::new(AudioStats::default());
        let follow_now = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            start_us: now_us(),
            last_input_us: AtomicU64::new(0),
            capture_packet_frames: AtomicU32::new(0),
            in_ring_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(processor.latency_frames() as u32),
            proc_hop: AtomicU32::new(processor.hop() as u32),
            output_failed: AtomicBool::new(false),
            capture_failed: AtomicBool::new(false),
            wake: OwnedEvent::new()?,
        });

        let (in_tx, in_rx) = RingBuffer::<f32>::new(RING_FRAMES * 2);
        let (in_mk_tx, in_mk_rx) = RingBuffer::<InputMarker>::new(MARKER_SLOTS);
        let (out_tx, out_rx) = RingBuffer::<f32>::new(RING_FRAMES * 2);
        let (out_mk_tx, out_mk_rx) = RingBuffer::<u64>::new(MARKER_SLOTS);
        let (proc_ctl_tx, proc_ctl_rx) = RingBuffer::<ProcCommand>::new(CONTROL_SLOTS);
        let (render_ctl_tx, render_ctl_rx) = RingBuffer::<RenderCommand>::new(CONTROL_SLOTS);

        let mut handle = AudioHandle {
            stats: stats.clone(),
            follow_now: follow_now.clone(),
            shared: shared.clone(),
            proc_ctl: Mutex::new(proc_ctl_tx),
            render_ctl: Mutex::new(render_ctl_tx),
            workers: Vec::new(),
        };

        let ctx = processing::ProcessingCtx {
            processor,
            input: in_rx,
            in_markers: in_mk_rx,
            output: out_tx,
            out_markers: out_mk_tx,
            control: proc_ctl_rx,
            shared: shared.clone(),
            stats: stats.clone(),
        };
        handle.spawn("devocal-processing", move || processing::run(ctx))?;

        let (ready_tx, ready_rx) = mpsc::channel();
        let ctx = capture::CaptureCtx {
            pid: cfg.pid,
            output: in_tx,
            markers: in_mk_tx,
            shared: shared.clone(),
            stats: stats.clone(),
            gains: gains.clone(),
            follow_now,
        };
        handle.spawn("devocal-capture", move || capture::run(ctx, ready_tx))?;
        handle.await_ready("capture", &ready_rx)?;

        let (ready_tx, ready_rx) = mpsc::channel();
        let ctx = render::RenderCtx {
            endpoint: cfg.output_endpoint,
            input: out_rx,
            markers: out_mk_rx,
            control: render_ctl_rx,
            shared,
            stats,
            gains,
        };
        handle.spawn("devocal-render", move || render::run(ctx, ready_tx))?;
        handle.await_ready("render", &ready_rx)?;
        Ok(handle)
    }

    /// User toggle, forwarded to the processing thread (applied between blocks). Send "on"
    /// only for a genuine user action (caller obligation 1).
    pub fn set_devocal(&self, on: bool) {
        self.send_proc(ProcCommand::SetDevocal(on));
    }

    /// Installs a new model: devocal off, wait for the fade-out, swap, back on if the user
    /// toggle is on (caller obligation 3). The old model is dropped on a helper thread.
    pub fn set_separator(&self, s: Box<dyn Separator>) {
        self.send_proc(ProcCommand::SetSeparator(s));
    }

    /// Reopens the output on `endpoint_id` (`None` = default endpoint).
    pub fn rebind_output(&self, endpoint_id: Option<String>) {
        let ok = match self.render_ctl.lock() {
            Ok(mut q) => push_retry(&mut q, RenderCommand::Rebind(endpoint_id), None),
            Err(_) => false,
        };
        if !ok {
            eprintln!("devocal audio: render control queue full; rebind dropped");
        }
    }

    /// True after the output failed (e.g. AUDCLNT_E_DEVICE_INVALIDATED) until a successful
    /// rebind.
    pub fn output_failed(&self) -> bool {
        self.shared.output_failed.load(Ordering::Acquire)
    }

    /// True once process loopback failed (the capture thread has ended).
    pub fn capture_failed(&self) -> bool {
        self.shared.capture_failed.load(Ordering::Acquire)
    }

    /// Returns and clears the safety guard's request to run `Holder::follow()` now.
    pub fn take_follow_request(&self) -> bool {
        self.follow_now.swap(false, Ordering::AcqRel)
    }

    /// Stops and joins the three threads (bounded by [`STOP_TIMEOUT_MS`]).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn send_proc(&self, cmd: ProcCommand) {
        let ok = match self.proc_ctl.lock() {
            Ok(mut q) => push_retry(&mut q, cmd, Some(&self.shared.wake)),
            Err(_) => false,
        };
        if !ok {
            eprintln!("devocal audio: processing control queue full; command dropped");
        }
    }

    fn spawn(
        &mut self,
        name: &'static str,
        f: impl FnOnce() + Send + 'static,
    ) -> Result<(), String> {
        let exited = Arc::new(AtomicBool::new(false));
        let flag = ExitFlag(exited.clone());
        let spawned = thread::Builder::new().name(name.into()).spawn(move || {
            let _flag = flag;
            f()
        });
        match spawned {
            Ok(handle) => {
                self.workers.push(Worker {
                    name,
                    handle: Some(handle),
                    exited,
                });
                Ok(())
            }
            Err(e) => {
                self.shutdown();
                Err(format!("spawn {name}: {e}"))
            }
        }
    }

    fn await_ready(
        &mut self,
        what: &str,
        rx: &mpsc::Receiver<Result<(), String>>,
    ) -> Result<(), String> {
        let result = match rx.recv_timeout(Duration::from_millis(START_TIMEOUT_MS)) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!("{what} start timed out")),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("{what} thread ended during start"))
            }
        };
        if let Err(e) = result {
            self.shutdown();
            return Err(e);
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        if self.workers.is_empty() {
            return;
        }
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.set();
        let deadline = Instant::now() + Duration::from_millis(STOP_TIMEOUT_MS);
        while Instant::now() < deadline
            && !self
                .workers
                .iter()
                .all(|w| w.exited.load(Ordering::Acquire))
        {
            thread::sleep(Duration::from_millis(2));
        }
        for mut w in self.workers.drain(..) {
            let Some(handle) = w.handle.take() else {
                continue;
            };
            if w.exited.load(Ordering::Acquire) {
                if handle.join().is_err() {
                    eprintln!("devocal audio: {} thread panicked", w.name);
                }
            } else {
                // Stuck in a driver call (e.g. loopback activation never completing):
                // leave it detached rather than hang the engine.
                eprintln!(
                    "devocal audio: {} thread did not stop within {STOP_TIMEOUT_MS} ms; detached",
                    w.name
                );
            }
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    name: &'static str,
    handle: Option<JoinHandle<()>>,
    exited: Arc<AtomicBool>,
}

/// Marks the thread finished when dropped (also on panic unwinding).
struct ExitFlag(Arc<AtomicBool>);

impl Drop for ExitFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Pushes a control message, retrying for up to ~0.5 s while the queue is full (control side
/// only; never called on an audio thread).
fn push_retry<T>(q: &mut Producer<T>, mut v: T, wake: Option<&OwnedEvent>) -> bool {
    for _ in 0..500 {
        match q.push(v) {
            Ok(()) => {
                if let Some(e) = wake {
                    e.set();
                }
                return true;
            }
            Err(PushError::Full(back)) => {
                v = back;
                if let Some(e) = wake {
                    e.set();
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    false
}

/// Microseconds on the QueryPerformanceCounter clock (shared by all threads; never 0 in
/// practice, so 0 can mean "never").
pub(crate) fn now_us() -> u64 {
    static FREQ: OnceLock<u64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // Cannot fail on Windows XP and later.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1) as u64
    });
    let mut c = 0i64;
    let _ = unsafe { QueryPerformanceCounter(&mut c) };
    (c.max(0) as u128 * 1_000_000 / u128::from(freq)) as u64
}

/// COM in the multithreaded apartment for the current thread; uninitialised on drop. Must be
/// created before, and dropped after, every COM object of the thread.
pub(crate) struct ComGuard(());

impl ComGuard {
    pub fn init_mta() -> Result<Self, String> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(format!("CoInitializeEx(MTA): {hr:?}"));
        }
        Ok(ComGuard(()))
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// MMCSS "Pro Audio" registration for the current thread, reverted on drop. Failure (MMCSS
/// service unavailable) is logged and the thread runs at normal priority.
pub(crate) struct Mmcss(Option<HANDLE>);

impl Mmcss {
    pub fn pro_audio(thread: &str) -> Self {
        let mut task_index = 0u32;
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(h) => Mmcss(Some(h)),
            Err(e) => {
                eprintln!("devocal audio: {thread}: MMCSS Pro Audio unavailable: {e}");
                Mmcss(None)
            }
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

/// Auto-reset Win32 event, closed on drop.
pub(crate) struct OwnedEvent(HANDLE);

// SAFETY: an event handle may be signalled and waited on from any thread.
unsafe impl Send for OwnedEvent {}
unsafe impl Sync for OwnedEvent {}

impl OwnedEvent {
    pub fn new() -> Result<Self, String> {
        unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map(OwnedEvent)
            .map_err(|e| format!("CreateEventW: {e}"))
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }

    pub fn set(&self) {
        let _ = unsafe { SetEvent(self.0) };
    }

    /// True if signalled within `timeout_ms`.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        let r = unsafe { WaitForSingleObject(self.0, timeout_ms) };
        r == WAIT_OBJECT_0
    }
}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_input_is_not_an_underrun() {
        let now = 10_000_000;
        assert!(count_underrun(now - 49_000, now));
        assert!(!count_underrun(now - 51_000, now));
        assert!(!count_underrun(0, now), "no input yet");
        assert!(
            count_underrun(now + 1_000, now),
            "input stamped after `now`"
        );
    }

    #[test]
    fn fade_in_skips_then_ramps_across_blocks() {
        let mut f = FadeIn::new(5);
        // Idle: no effect.
        let mut b = vec![1.0f32; 8];
        f.apply(&mut b);
        assert!(b.iter().all(|&s| s == 1.0));

        f.start(2);
        let mut whole = vec![1.0f32; 2 * 10];
        let (a, rest) = whole.split_at_mut(2 * 3);
        f.apply(a);
        f.apply(rest);
        let left: Vec<f32> = whole.iter().step_by(2).copied().collect();
        assert_eq!(
            left,
            vec![0.0, 0.0, 0.0, 0.25, 0.5, 0.75, 1.0, 1.0, 1.0, 1.0]
        );
        assert_eq!(whole[2 * 5 + 1], 0.75, "right channel ramps with the left");
    }

    #[test]
    fn edge_fade_is_5_ms() {
        assert_eq!(edge_fade_frames(), 221);
    }

    /// Ruling 5: the per-block helpers used by the audio threads never allocate.
    #[test]
    fn per_block_helpers_do_not_allocate() {
        use crate::stemgen::alloc_count;
        let stats = AudioStats::default();
        let follow = AtomicBool::new(false);
        let mut cond = capture::Conditioner::new();
        let mut fade = FadeIn::new(edge_fade_frames());
        let mut gaps = render::GapFader::new(edge_fade_frames());
        let (mut mk_tx, mut mk_rx) = RingBuffer::<u64>::new(8);
        let mut trim = render::TrimPolicy::new(1_000);
        let mut block = vec![0.25f32; 441 * 2];
        let mut loud = vec![3.0f32; 441 * 2];
        let before = alloc_count::this_thread();
        for i in 0..50u64 {
            cond.condition(&mut loud, 1.0, &stats, &follow, |_| {});
            cond.condition(&mut block, 1.0, &stats, &follow, |_| {});
            fade.start(10);
            fade.apply(&mut block);
            let _ = mk_tx.push(i * 441 + 300);
            gaps.apply(&mut mk_rx, i * 441, &mut block);
            render::finish_block(&mut block, 0.5, 1.0);
            let _ = trim.observe(5_000, i * 30_000);
            let _ = count_underrun(i, i + 10);
            let mut judge = render::UnderrunJudge::new();
            judge.starved(i, i + 10);
            let _ = judge.poll(i + 20, i + 30);
            let _ = capture::guard_block(&mut block);
        }
        assert_eq!(alloc_count::this_thread() - before, 0);
    }

    #[test]
    fn shared_gains_round_trip_and_sanitise() {
        let g = SharedGains::new(2.0, 0.5);
        assert_eq!(g.capture_gain(), 2.0);
        assert_eq!(g.output_gain(), 0.5);
        g.set(f32::NAN, -1.0);
        assert_eq!(g.capture_gain(), 0.0);
        assert_eq!(g.output_gain(), 0.0);
        g.set(f32::INFINITY, 1.0);
        assert_eq!(g.capture_gain(), 0.0);
    }
}
