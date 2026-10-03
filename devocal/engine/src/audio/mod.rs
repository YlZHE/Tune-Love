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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
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

use devocal_core::protocol::FallbackReason;

use crate::dsp::{frames_for_ms, SAMPLE_RATE};
use crate::processor::{Processor, Stage};
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
    /// Estimated end-to-end added latency in microseconds (milli-milliseconds): capture
    /// packet + ring A + ring B + device padding + processor latency + 3 render periods for
    /// the system path (see `render::SYSTEM_PATH_PERIODS`).
    pub latency_ms_milli: AtomicU32,
    /// Capture packets flagged discontinuous / timestamp error, plus ring overflows.
    pub discontinuities: AtomicU64,
    /// Blocks silenced by the safety guard (peak above 1.5 after the capture gain).
    pub unattenuated_blocks: AtomicU64,
    /// Processor stage after the latest block or applied command ([`stage_code`] mapping:
    /// 0 Passthrough, 1 WarmingUp, 2 FadingIn, 3 Devocal, 4 FadingOut, 5 Fallback).
    pub stage: AtomicU8,
    /// Processor fallback reason ([`reason_code`]: 0 none, 1 Overload, 2 ModelError).
    pub fallback_reason: AtomicU8,
    /// Extra render headroom in frames added after jitter underruns (on top of
    /// one period + one hop; at most [`MAX_EXTRA_HEADROOM_FRAMES`]).
    pub headroom_frames: AtomicU32,
    /// A thread ended without being stopped (error or panic). `capture_failed` also covers
    /// process loopback errors; render device errors are `AudioHandle::output_failed`
    /// (recoverable by a rebind), `render_failed` only an unexpected end of the thread.
    pub processing_failed: AtomicBool,
    pub capture_failed: AtomicBool,
    pub render_failed: AtomicBool,
    /// Diagnostic counters, logged once a second when `DEVOCAL_DIAG` is set.
    pub diag: DiagCounters,
}

/// Monotonic counters for the `DEVOCAL_DIAG` log (relaxed increments only).
#[derive(Debug, Default)]
pub struct DiagCounters {
    pub capture_packets: AtomicU64,
    pub capture_frames: AtomicU64,
    pub capture_flag_discontinuity: AtomicU64,
    pub capture_flag_timestamp: AtomicU64,
    pub capture_ring_overflow_frames: AtomicU64,
    pub proc_blocks: AtomicU64,
    pub proc_resets: AtomicU64,
    pub proc_ring_b_drops: AtomicU64,
    pub render_wakes: AtomicU64,
    pub render_real_frames: AtomicU64,
    pub render_pad_events: AtomicU64,
    pub render_pad_frames: AtomicU64,
    pub render_trim_frames: AtomicU64,
    pub render_preroll_frames: AtomicU64,
}

/// Process-wide run counter: tells apart the logs of successive `AudioHandle::start`s.
static RUN_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Next run id, monotonic within the process, starting at 1.
pub(crate) fn next_run_id() -> u32 {
    RUN_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

/// One `DEVOCAL_DIAG` line: run id, QPC time `t_us`, the span the deltas cover, the counter
/// deltas, the free-form `tail`, and a trailing ` partial` when the span is cut short.
pub(crate) fn diag_line(
    run: u32,
    t_us: u64,
    span_us: u64,
    fields: &[(&str, u64)],
    tail: &str,
    partial: bool,
) -> String {
    let mut line = format!(
        "devocal diag: run={run} t={:.6} span_ms={}",
        t_us as f64 / 1e6,
        span_us / 1000
    );
    for (name, n) in fields {
        line.push_str(&format!(" {name}={n}"));
    }
    if !tail.is_empty() {
        line.push(' ');
        line.push_str(tail);
    }
    if partial {
        line.push_str(" partial");
    }
    line
}

/// The renderer's "stream opened" log line.
pub(crate) fn render_open_line(run: u32, describe: &str, period: usize, buffer: usize) -> String {
    format!("devocal audio: run={run} render {describe} (period {period} frames, buffer {buffer})")
}

/// Env var that turns on the once-a-second diagnostic log.
pub const DIAG_ENV: &str = "DEVOCAL_DIAG";

/// How often the diag thread looks at the stop flag.
const DIAG_POLL_MS: u64 = 50;

/// Logs counter deltas once a second until `shared.stop`, then one partial last line.
fn diag_loop(stats: Arc<AudioStats>, shared: Arc<Shared>, gains: Arc<SharedGains>) {
    diag_loop_with(stats, shared, gains, 1_000_000, |l| eprintln!("{l}"));
}

/// [`diag_loop`] with its own interval and sink. Emits a start line, a line every
/// `interval_us`, and on stop a last line (marked partial) covering the time since the
/// previous one. Only this detached thread formats strings.
pub(crate) fn diag_loop_with(
    stats: Arc<AudioStats>,
    shared: Arc<Shared>,
    gains: Arc<SharedGains>,
    interval_us: u64,
    mut emit: impl FnMut(String),
) {
    let d = &stats.diag;
    let read = || {
        [
            &d.capture_packets,
            &d.capture_frames,
            &d.capture_flag_discontinuity,
            &d.capture_flag_timestamp,
            &d.capture_ring_overflow_frames,
            &d.proc_blocks,
            &d.proc_resets,
            &d.proc_ring_b_drops,
            &d.render_wakes,
            &d.render_real_frames,
            &d.render_pad_events,
            &d.render_pad_frames,
            &d.render_trim_frames,
            &d.render_preroll_frames,
            &stats.unattenuated_blocks,
            &stats.underruns,
        ]
        .map(|a| a.load(Ordering::Relaxed))
    };
    let names = [
        "cap_pkts",
        "cap_frames",
        "cap_disc",
        "cap_ts_err",
        "cap_overflow",
        "proc_blocks",
        "proc_resets",
        "proc_b_drops",
        "r_wakes",
        "r_real",
        "r_pad_ev",
        "r_pad_fr",
        "r_trim",
        "r_preroll",
        "guard",
        "underruns",
    ];
    let run = shared.run_id;
    let mut last = read();
    let mut last_us = now_us();
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    emit(format!(
        "devocal diag: run={run} start t={:.6} unix_ms={unix_ms}",
        last_us as f64 / 1e6
    ));
    let line = |last: &[u64; 16], now: [u64; 16], t_us: u64, span_us: u64, partial: bool| {
        let mut fields = [("", 0u64); 16];
        for (i, f) in fields.iter_mut().enumerate() {
            *f = (names[i], now[i].wrapping_sub(last[i]));
        }
        let tail = format!(
            "stage={} in_ring={} headroom={} cap_gain={:.1} out_gain={:.3} lat_ms={:.1}",
            stats.stage.load(Ordering::Relaxed),
            shared.in_ring_frames.load(Ordering::Relaxed),
            stats.headroom_frames.load(Ordering::Relaxed),
            gains.capture_gain(),
            gains.output_gain(),
            stats.latency_ms_milli.load(Ordering::Relaxed) as f64 / 1000.0,
        );
        diag_line(run, t_us, span_us, &fields, &tail, partial)
    };
    loop {
        thread::sleep(Duration::from_millis(DIAG_POLL_MS));
        let stopping = shared.stop.load(Ordering::Acquire);
        let t = now_us();
        let span = t.saturating_sub(last_us);
        if !stopping && span < interval_us {
            continue;
        }
        let now = read();
        emit(line(&last, now, t, span, stopping));
        last = now;
        last_us = t;
        if stopping {
            return;
        }
    }
}

/// [`AudioStats::stage`] encoding.
pub fn stage_code(stage: Stage) -> u8 {
    match stage {
        Stage::Passthrough => 0,
        Stage::WarmingUp => 1,
        Stage::FadingIn => 2,
        Stage::Devocal => 3,
        Stage::FadingOut => 4,
        Stage::Fallback => 5,
    }
}

/// Inverse of [`stage_code`]; unknown codes read as `Passthrough`.
pub fn stage_from_code(code: u8) -> Stage {
    match code {
        1 => Stage::WarmingUp,
        2 => Stage::FadingIn,
        3 => Stage::Devocal,
        4 => Stage::FadingOut,
        5 => Stage::Fallback,
        _ => Stage::Passthrough,
    }
}

/// The model runs in these stages (the processor's `model_running`).
pub fn stage_runs_model(stage: Stage) -> bool {
    matches!(
        stage,
        Stage::WarmingUp | Stage::FadingIn | Stage::Devocal | Stage::FadingOut
    )
}

/// [`AudioStats::fallback_reason`] encoding.
pub fn reason_code(reason: Option<FallbackReason>) -> u8 {
    match reason {
        None => 0,
        Some(FallbackReason::Overload) => 1,
        Some(FallbackReason::ModelError) => 2,
    }
}

/// Inverse of [`reason_code`]; unknown codes read as `None`.
pub fn reason_from_code(code: u8) -> Option<FallbackReason> {
    match code {
        1 => Some(FallbackReason::Overload),
        2 => Some(FallbackReason::ModelError),
        _ => None,
    }
}

/// What the render thread saw when the output starved; carried with a counted underrun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Starvation {
    /// The processor's stage ran the model.
    pub ran_model: bool,
    /// Unprocessed input waiting in ring A, in frames.
    pub backlog_frames: usize,
}

const SNAPSHOT_MODEL_BIT: u64 = 1 << 63;

/// Packs a [`Starvation`] into one word (bit 63 = model ran, low bits = backlog frames) so
/// the render thread publishes it atomically, without a torn pair across two underruns.
pub fn pack_starvation(s: Starvation) -> u64 {
    let backlog = (s.backlog_frames as u64).min(SNAPSHOT_MODEL_BIT - 1);
    if s.ran_model {
        backlog | SNAPSHOT_MODEL_BIT
    } else {
        backlog
    }
}

/// Inverse of [`pack_starvation`].
pub fn unpack_starvation(word: u64) -> Starvation {
    Starvation {
        ran_model: word & SNAPSHOT_MODEL_BIT != 0,
        backlog_frames: (word & !SNAPSHOT_MODEL_BIT) as usize,
    }
}

/// Ruling 18: the processing thread is behind when the model runs and ring A holds at least
/// one hop of unprocessed input at the moment the output starves.
pub fn is_backlogged(ran_model: bool, backlog_frames: usize, hop: usize) -> bool {
    ran_model && backlog_frames >= hop
}

/// Extra render headroom (jitter growth and pre-roll together) never exceeds about one
/// capture packet (10 ms), which keeps capture packet + ring A + model latency + (period +
/// hop + headroom) inside the 50 ms devocal budget for engine periods up to 10 ms.
pub const MAX_EXTRA_HEADROOM_FRAMES: usize = 441;
/// One-time render pre-roll when a user "on" is accepted (about one capture packet), counted
/// inside the headroom.
pub const PREROLL_FRAMES: usize = 441;
/// Devocal latency budget (50 ms at 44.1 kHz). Covers the engine-internal part only (capture
/// packet, ring A, model latency, render target); the published latency estimate adds
/// `render::SYSTEM_PATH_PERIODS` render periods for the system path on top of it.
pub const LATENCY_BUDGET_FRAMES: usize = 2_205;

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
    /// Which `AudioHandle::start` this is ([`next_run_id`]); tags every log line.
    pub run_id: u32,
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
    /// Set by processing when it accepts a user "on"; render takes it and pre-rolls once.
    pub preroll_request: AtomicBool,
    /// Snapshot taken when the output starved ([`pack_starvation`]), published with each
    /// counted underrun before `AudioStats::underruns` is incremented.
    pub underrun_snapshot: AtomicU64,
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
        stats
            .stage
            .store(stage_code(processor.stage()), Ordering::Release);
        stats
            .fallback_reason
            .store(reason_code(processor.fallback_reason()), Ordering::Release);
        let follow_now = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            start_us: now_us(),
            run_id: next_run_id(),
            last_input_us: AtomicU64::new(0),
            capture_packet_frames: AtomicU32::new(0),
            in_ring_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(processor.latency_frames() as u32),
            proc_hop: AtomicU32::new(processor.hop() as u32),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
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
        handle.spawn(
            "devocal-processing",
            |s| &s.processing_failed,
            move || processing::run(ctx),
        )?;

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
        handle.spawn(
            "devocal-capture",
            |s| &s.capture_failed,
            move || {
                capture::run(ctx, ready_tx);
                None
            },
        )?;
        handle.await_ready("capture", &ready_rx)?;

        if std::env::var_os(DIAG_ENV).is_some() {
            let (st, sh, g) = (stats.clone(), shared.clone(), gains.clone());
            // Detached: ends at the next tick after the stop flag.
            let _ = thread::Builder::new()
                .name("devocal-diag".into())
                .spawn(move || diag_loop(st, sh, g));
        }

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
        handle.spawn(
            "devocal-render",
            |s| &s.render_failed,
            move || {
                render::run(ctx, ready_tx);
                None
            },
        )?;
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
        self.stats.capture_failed.load(Ordering::Acquire)
    }

    /// True once any audio thread ended without being stopped (process loopback error, or
    /// a panic in capture, processing or render). The engine treats this as a Failure.
    pub fn failed(&self) -> bool {
        self.stats.processing_failed.load(Ordering::Acquire)
            || self.stats.capture_failed.load(Ordering::Acquire)
            || self.stats.render_failed.load(Ordering::Acquire)
    }

    /// Processor stage after the latest block or applied control command.
    pub fn stage(&self) -> Stage {
        stage_from_code(self.stats.stage.load(Ordering::Acquire))
    }

    /// Processor fallback reason (for the status mapping, caller obligation 5).
    pub fn fallback_reason(&self) -> Option<FallbackReason> {
        reason_from_code(self.stats.fallback_reason.load(Ordering::Acquire))
    }

    /// Returns and clears the safety guard's request to run `Holder::follow()` now.
    pub fn take_follow_request(&self) -> bool {
        self.follow_now.swap(false, Ordering::AcqRel)
    }

    /// Stops and joins the three threads (bounded by [`STOP_TIMEOUT_MS`]) and returns the
    /// model the processing thread held, for reuse. `None` if there was none, or if the
    /// processing thread did not stop in time (left detached) or panicked.
    pub fn stop(mut self) -> Option<Box<dyn Separator>> {
        self.shutdown()
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

    /// Spawns an audio thread; `failed` selects the stats flag set if the thread ends
    /// without a stop request (or panics). The processing thread returns its model.
    fn spawn(
        &mut self,
        name: &'static str,
        failed: fn(&AudioStats) -> &AtomicBool,
        f: impl FnOnce() -> Option<Box<dyn Separator>> + Send + 'static,
    ) -> Result<(), String> {
        let exited = Arc::new(AtomicBool::new(false));
        let flag = ExitFlag {
            exited: exited.clone(),
            shared: self.shared.clone(),
            stats: self.stats.clone(),
            failed,
        };
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

    /// Stops the threads; returns the model a joined thread handed back.
    fn shutdown(&mut self) -> Option<Box<dyn Separator>> {
        if self.workers.is_empty() {
            return None;
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
        let mut model = None;
        for mut w in self.workers.drain(..) {
            let Some(handle) = w.handle.take() else {
                continue;
            };
            if w.exited.load(Ordering::Acquire) {
                match handle.join() {
                    Ok(m) => model = model.or(m),
                    Err(_) => eprintln!("devocal audio: {} thread panicked", w.name),
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
        model
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    name: &'static str,
    handle: Option<JoinHandle<Option<Box<dyn Separator>>>>,
    exited: Arc<AtomicBool>,
}

/// Marks the thread finished when dropped (also on panic unwinding); an end without a stop
/// request, or a panic, also sets the thread's failure flag.
struct ExitFlag {
    exited: Arc<AtomicBool>,
    shared: Arc<Shared>,
    stats: Arc<AudioStats>,
    failed: fn(&AudioStats) -> &AtomicBool,
}

impl Drop for ExitFlag {
    fn drop(&mut self) {
        if thread::panicking() || !self.shared.stop.load(Ordering::Acquire) {
            (self.failed)(&self.stats).store(true, Ordering::Release);
        }
        self.exited.store(true, Ordering::Release);
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

    fn test_shared() -> Shared {
        Shared {
            stop: AtomicBool::new(false),
            start_us: now_us(),
            run_id: next_run_id(),
            last_input_us: AtomicU64::new(0),
            capture_packet_frames: AtomicU32::new(0),
            in_ring_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(0),
            proc_hop: AtomicU32::new(128),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
            wake: OwnedEvent::new().unwrap(),
        }
    }

    #[test]
    fn diag_line_carries_run_time_and_span() {
        let fields = [("cap_pkts", 100u64), ("underruns", 0)];
        assert_eq!(
            diag_line(7, 12_345_678, 1_000_000, &fields, "stage=0", false),
            "devocal diag: run=7 t=12.345678 span_ms=1000 cap_pkts=100 underruns=0 stage=0"
        );
        let partial = diag_line(7, 12_345_678, 420_000, &fields, "stage=0", true);
        assert!(
            partial.ends_with("span_ms=420 cap_pkts=100 underruns=0 stage=0 partial"),
            "{partial}"
        );
    }

    #[test]
    fn run_ids_increase() {
        let a = next_run_id();
        let b = next_run_id();
        assert!(a >= 1 && b > a);
    }

    #[test]
    fn render_open_line_names_the_run() {
        assert_eq!(
            render_open_line(3, "wasapi autoconvert", 441, 1036),
            "devocal audio: run=3 render wasapi autoconvert (period 441 frames, buffer 1036)"
        );
    }

    #[test]
    fn diag_loop_emits_a_final_partial_line_after_stop() {
        let stats = Arc::new(AudioStats::default());
        let shared = Arc::new(test_shared());
        let gains = Arc::new(SharedGains::new(1.0, 1.0));
        let (tx, rx) = mpsc::channel::<String>();
        let (st, sh, g) = (stats, shared.clone(), gains);
        let t = thread::spawn(move || {
            diag_loop_with(st, sh, g, 100_000, move |l| {
                let _ = tx.send(l);
            })
        });
        thread::sleep(Duration::from_millis(250));
        shared.stop.store(true, Ordering::Release);
        let stopped_at = Instant::now();
        while !t.is_finished() && stopped_at.elapsed() < Duration::from_millis(200) {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(t.is_finished(), "diag thread did not stop within 200 ms");
        t.join().unwrap();
        let lines: Vec<String> = rx.try_iter().collect();
        assert!(lines.len() >= 4, "{lines:?}");
        assert!(lines[0].contains(" start "), "{}", lines[0]);
        let (last, mid) = lines[1..].split_last().unwrap();
        assert!(mid.iter().all(|l| !l.contains("partial")), "{lines:?}");
        assert_eq!(mid.len(), 2, "{lines:?}");
        assert!(last.ends_with(" partial"), "{last}");
        let field = |l: &str, key: &str| -> f64 {
            l.split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in {l}"))
                .parse()
                .unwrap()
        };
        assert!(field(last, "span_ms=") < 100.0, "{last}");
        let run = format!("run={}", shared.run_id);
        assert!(lines.iter().all(|l| l.contains(&run)), "{lines:?}");
        let ts: Vec<f64> = lines.iter().map(|l| field(l, "t=")).collect();
        assert!(ts.windows(2).all(|w| w[1] > w[0]), "{ts:?}");
    }

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
            judge.starved(
                i,
                i + 10,
                Starvation {
                    ran_model: true,
                    backlog_frames: 0,
                },
            );
            let _ = judge.poll(i + 20, i + 30);
            let _ = render::grow_headroom(i as usize, 128, MAX_EXTRA_HEADROOM_FRAMES);
            let _ = is_backlogged(true, 128, 128);
            let _ = capture::guard_block(&mut block);
        }
        assert_eq!(alloc_count::this_thread() - before, 0);
    }

    #[test]
    fn stage_and_reason_codes_round_trip() {
        let stages = [
            Stage::Passthrough,
            Stage::WarmingUp,
            Stage::FadingIn,
            Stage::Devocal,
            Stage::FadingOut,
            Stage::Fallback,
        ];
        for (i, &st) in stages.iter().enumerate() {
            assert_eq!(stage_code(st), i as u8, "documented mapping");
            assert_eq!(stage_from_code(stage_code(st)), st);
        }
        assert_eq!(stage_from_code(200), Stage::Passthrough);
        assert_eq!(AudioStats::default().stage.load(Ordering::Relaxed), 0);
        for r in [
            None,
            Some(FallbackReason::Overload),
            Some(FallbackReason::ModelError),
        ] {
            assert_eq!(reason_from_code(reason_code(r)), r);
        }
        assert_eq!(reason_code(None), 0);
        assert_eq!(reason_code(Some(FallbackReason::Overload)), 1);
        assert_eq!(reason_code(Some(FallbackReason::ModelError)), 2);
        assert_eq!(reason_from_code(9), None);
        assert!(stage_runs_model(Stage::WarmingUp) && stage_runs_model(Stage::FadingOut));
        assert!(!stage_runs_model(Stage::Passthrough) && !stage_runs_model(Stage::Fallback));
    }

    #[test]
    fn starvation_snapshot_packs_into_one_word() {
        for s in [
            Starvation {
                ran_model: true,
                backlog_frames: 0,
            },
            Starvation {
                ran_model: false,
                backlog_frames: 44_100,
            },
            Starvation {
                ran_model: true,
                backlog_frames: 300,
            },
        ] {
            assert_eq!(unpack_starvation(pack_starvation(s)), s);
        }
        assert!(!unpack_starvation(0).ran_model);
    }

    #[test]
    fn backlog_means_processing_is_behind() {
        assert!(is_backlogged(true, 128, 128));
        assert!(
            !is_backlogged(true, 127, 128),
            "less than a hop waiting: jitter"
        );
        assert!(!is_backlogged(false, 4_000, 128), "model not running");
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
