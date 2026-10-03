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
mod confirm;
pub mod endpoint;
mod processing;
pub mod render;

use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
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
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CancelWaitableTimer,
    CreateEventW, CreateWaitableTimerExW, SetEvent, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

use devocal_core::protocol::FallbackReason;

use crate::dsp::{frames_for_ms, SAMPLE_RATE};
use crate::holder::AttachRamp;
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
    /// packet + ring A (with the block being processed) + ring B + device padding + processor
    /// latency + the calibrated system
    /// path (about 3.2 render periods, see `render::SYSTEM_PATH_PERIOD_TENTHS`).
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
    /// Extra render headroom in frames (on top of one period + one hop; at most
    /// [`MAX_EXTRA_HEADROOM_FRAMES`]): jitter growth plus a pre-roll, as
    /// `render::Headroom::total`; both decay again (O1).
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
    /// R2: capture chunks (about 2.5 ms) raised above the conservative gain because their
    /// level matched an issued attach step.
    pub capture_confirm_chunks: AtomicU64,
    /// R2: capture chunks inside a confirmation window that kept the conservative gain (no
    /// usable reference, or the level matched no issued step).
    pub capture_fallback_chunks: AtomicU64,
    pub proc_blocks: AtomicU64,
    pub proc_resets: AtomicU64,
    pub proc_ring_b_drops: AtomicU64,
    /// Every render wake (device, data, deadline and timeout alike).
    pub render_wakes: AtomicU64,
    /// Render wakes because the processing thread pushed a block to ring B (O2).
    pub render_data_wakes: AtomicU64,
    /// Render wakes at the read deadline, just before the device reads (O2).
    pub render_deadline_wakes: AtomicU64,
    /// Render wakes after 20 ms with nothing signalled. Device wakes are `render_wakes`
    /// minus data, deadline and timeout wakes.
    pub render_timeout_wakes: AtomicU64,
    /// Device wakes that found their deadline still armed (it never fired). An isolated
    /// miss checks only whether the device ran dry (ruling 17). Close to the device wakes:
    /// deadlines are being missed.
    pub render_missed_deadlines: AtomicU64,
    /// Streams switched to checking on device wakes (legacy, until reopened) after
    /// [`render::MISSED_DEADLINE_LIMIT`] missed deadlines within
    /// [`render::MISSED_DEADLINE_WINDOW_US`].
    pub render_legacy_switches: AtomicU64,
    pub render_real_frames: AtomicU64,
    pub render_pad_events: AtomicU64,
    pub render_pad_frames: AtomicU64,
    pub render_trim_frames: AtomicU64,
    pub render_preroll_frames: AtomicU64,
    /// Ring B frames skipped when released headroom decays (O1); not in `render_trim_frames`.
    pub render_decay_frames: AtomicU64,
    // Ruling 24 columns, appended after `lat_ms` (gauges are reset by every diag line).
    /// Longest `process_block` since the last line, us (gauge).
    pub proc_block_max_us: AtomicU64,
    /// Blocks whose `process_block` took over 2, 3 and 5 ms (counters).
    pub proc_over_2ms: AtomicU64,
    pub proc_over_3ms: AtomicU64,
    pub proc_over_5ms: AtomicU64,
    /// Longest time between two capture packets, us (gauge).
    pub capture_gap_max_us: AtomicU64,
    /// Lowest queue a check wake found, minus a period (gauge; see
    /// [`DiagCounters::note_check_margin`]).
    check_margin_low: AtomicU64,
    /// Latest a deadline wake came after its due time, us (gauge).
    pub deadline_late_max_us: AtomicU64,
}

/// [`DiagCounters::check_margin_low`] holds `MARGIN_BIAS - margin` so `fetch_max` keeps the
/// lowest margin; 0 means no check since the last reset.
const MARGIN_BIAS: i64 = 1 << 32;

impl DiagCounters {
    /// A check wake found `margin` frames above one period queued (negative: short).
    pub fn note_check_margin(&self, margin: i64) {
        let v = (MARGIN_BIAS - margin.clamp(1 - MARGIN_BIAS, MARGIN_BIAS - 1)) as u64;
        self.check_margin_low.fetch_max(v, Ordering::Relaxed);
    }

    /// The lowest margin noted since the last call (`None` if no check), and resets it.
    pub fn take_check_margin(&self) -> Option<i64> {
        match self.check_margin_low.swap(0, Ordering::Relaxed) {
            0 => None,
            v => Some(MARGIN_BIAS - v as i64),
        }
    }

    /// One processed block took `us`.
    pub fn note_block_time(&self, us: u64) {
        self.proc_block_max_us.fetch_max(us, Ordering::Relaxed);
        for (limit, n) in [
            (2_000, &self.proc_over_2ms),
            (3_000, &self.proc_over_3ms),
            (5_000, &self.proc_over_5ms),
        ] {
            if us > limit {
                n.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// What one starvation looked like beyond [`Starvation`] (ruling 24 log): written by the
/// render thread with each counted underrun, logged by the engine with a forced fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StarvationDetail {
    pub at_us: u64,
    /// The wake that padded: 0 device, 1 data, 2 deadline, 3 timeout.
    pub wake: u64,
    /// Device padding and ring B frames when the check started, and how many frames short
    /// of a period the queue was after the real frames written.
    pub padding: u64,
    pub ring_b: u64,
    pub shortfall: u64,
    pub since_capture_us: u64,
    pub since_push_us: u64,
    /// How long the block being processed had been running (0: none in flight).
    pub block_busy_us: u64,
    /// How late the deadline wake was (0 for other wakes).
    pub check_late_us: u64,
    pub headroom: u64,
    /// The overload judgement (`render::Verdict::code`).
    pub verdict: u64,
}

const DETAIL_FIELDS: usize = 11;

impl StarvationDetail {
    fn to_words(self) -> [u64; DETAIL_FIELDS] {
        [
            self.at_us,
            self.wake,
            self.padding,
            self.ring_b,
            self.shortfall,
            self.since_capture_us,
            self.since_push_us,
            self.block_busy_us,
            self.check_late_us,
            self.headroom,
            self.verdict,
        ]
    }

    fn from_words(w: [u64; DETAIL_FIELDS]) -> Self {
        Self {
            at_us: w[0],
            wake: w[1],
            padding: w[2],
            ring_b: w[3],
            shortfall: w[4],
            since_capture_us: w[5],
            since_push_us: w[6],
            block_busy_us: w[7],
            check_late_us: w[8],
            headroom: w[9],
            verdict: w[10],
        }
    }
}

/// Ruling 24 log of forced fallbacks and overload retries (atomics only; the engine loop
/// formats the line). The render thread writes each counted underrun's detail; the
/// processing thread copies it into `forced_detail` when that underrun forces the fallback,
/// before it bumps `forced`, so a later underrun cannot overwrite what is logged.
#[derive(Debug, Default)]
pub(crate) struct FallbackLog {
    detail: [AtomicU64; DETAIL_FIELDS],
    forced_detail: [AtomicU64; DETAIL_FIELDS],
    /// The fallback's trigger: 1 underrun, 2 load.
    pub trigger: AtomicU8,
    pub forced_at_us: AtomicU64,
    pub load_milli: AtomicU32,
    /// The processor's stage just before it was forced ([`stage_code`]).
    pub stage: AtomicU8,
    /// A retry was scheduled for this fallback.
    pub retry_armed: AtomicBool,
    /// Forced fallbacks so far; incremented (Release) after the fields above are written.
    pub forced: AtomicU32,
    /// Overload retries started so far.
    pub retries: AtomicU32,
}

impl FallbackLog {
    pub fn set_detail(&self, d: StarvationDetail) {
        for (a, v) in self.detail.iter().zip(d.to_words()) {
            a.store(v, Ordering::Relaxed);
        }
    }

    pub fn detail(&self) -> StarvationDetail {
        StarvationDetail::from_words(self.detail.each_ref().map(|a| a.load(Ordering::Relaxed)))
    }

    /// Keeps the latest counted underrun's detail as the forced fallback's (processing
    /// thread, before `forced` is bumped).
    pub fn keep_forced_detail(&self) {
        for (to, from) in self.forced_detail.iter().zip(&self.detail) {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }

    pub fn forced_detail(&self) -> StarvationDetail {
        StarvationDetail::from_words(
            self.forced_detail
                .each_ref()
                .map(|a| a.load(Ordering::Relaxed)),
        )
    }
}

/// The engine log line for a forced fallback (ruling 24).
pub(crate) fn fallback_line(run: u32, log: &FallbackLog, now_us: u64) -> String {
    let trigger = log.trigger.load(Ordering::Acquire);
    let at = log.forced_at_us.load(Ordering::Relaxed);
    let mut l = format!(
        "devocal audio: run={run} forced fallback (overload) {} ms ago: trigger={} load={:.3} \
         stage={} retry={}",
        now_us.saturating_sub(at) / 1000,
        if trigger == 2 { "load" } else { "underrun" },
        log.load_milli.load(Ordering::Relaxed) as f64 / 1000.0,
        log.stage.load(Ordering::Relaxed),
        if log.retry_armed.load(Ordering::Relaxed) {
            "scheduled"
        } else {
            "none"
        },
    );
    if trigger != 2 {
        let d = log.forced_detail();
        l.push_str(&format!(
            " | starved {} ms before: verdict={} wake={} padding={} ring_b={} shortfall={} \
             since_capture_us={} since_push_us={} block_busy_us={} check_late_us={} headroom={}",
            at.saturating_sub(d.at_us) / 1000,
            render::Verdict::name(d.verdict),
            ["device", "data", "deadline", "timeout"]
                .get(d.wake as usize)
                .unwrap_or(&"?"),
            d.padding,
            d.ring_b,
            d.shortfall,
            d.since_capture_us,
            d.since_push_us,
            d.block_busy_us,
            d.check_late_us,
            d.headroom,
        ));
    }
    l
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
    diag_loop_with(
        stats,
        shared,
        gains,
        1_000_000,
        || thread::sleep(Duration::from_millis(DIAG_POLL_MS)),
        now_us,
        |l| eprintln!("{l}"),
    );
}

/// [`diag_loop`] with its own interval, poll wait, clock (microseconds) and sink. Emits a
/// start line, a line every `interval_us`, and on stop a last line (marked partial) covering
/// the time since the previous one. Only this detached thread formats strings.
pub(crate) fn diag_loop_with(
    stats: Arc<AudioStats>,
    shared: Arc<Shared>,
    gains: Arc<SharedGains>,
    interval_us: u64,
    mut wait: impl FnMut(),
    mut clock: impl FnMut() -> u64,
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
            &d.capture_confirm_chunks,
            &d.capture_fallback_chunks,
            &d.proc_blocks,
            &d.proc_resets,
            &d.proc_ring_b_drops,
            &d.render_wakes,
            &d.render_data_wakes,
            &d.render_deadline_wakes,
            &d.render_timeout_wakes,
            &d.render_missed_deadlines,
            &d.render_legacy_switches,
            &d.render_real_frames,
            &d.render_pad_events,
            &d.render_pad_frames,
            &d.render_trim_frames,
            &d.render_preroll_frames,
            &d.render_decay_frames,
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
        "r2_ok",
        "r2_fb",
        "proc_blocks",
        "proc_resets",
        "proc_b_drops",
        "r_wakes",
        "r_data",
        "r_deadline",
        "r_timeout",
        "r_missed",
        "r_legacy",
        "r_real",
        "r_pad_ev",
        "r_pad_fr",
        "r_trim",
        "r_preroll",
        "r_decay",
        "guard",
        "underruns",
    ];
    let run = shared.run_id;
    let mut last = read();
    let mut last_us = clock();
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    emit(format!(
        "devocal diag: run={run} start t={:.6} unix_ms={unix_ms}",
        last_us as f64 / 1e6
    ));
    // Ruling 24 columns: counters as deltas, gauges read and reset per line.
    let over = || {
        [&d.proc_over_2ms, &d.proc_over_3ms, &d.proc_over_5ms].map(|a| a.load(Ordering::Relaxed))
    };
    let mut last_over = over();
    let mut gauges = move || {
        let now = over();
        let margin = d
            .take_check_margin()
            .map_or_else(|| "-".to_string(), |m| m.to_string());
        let s = format!(
            " proc_max_us={} proc_over2={} proc_over3={} proc_over5={} cap_gap_max_us={} \
             chk_margin_min={margin} dl_late_max_us={}",
            d.proc_block_max_us.swap(0, Ordering::Relaxed),
            now[0].wrapping_sub(last_over[0]),
            now[1].wrapping_sub(last_over[1]),
            now[2].wrapping_sub(last_over[2]),
            d.capture_gap_max_us.swap(0, Ordering::Relaxed),
            d.deadline_late_max_us.swap(0, Ordering::Relaxed),
        );
        last_over = now;
        s
    };
    let line =
        |last: &[u64; 24], now: [u64; 24], t_us: u64, span_us: u64, partial: bool, extra: &str| {
            let mut fields = [("", 0u64); 24];
            for (i, f) in fields.iter_mut().enumerate() {
                *f = (names[i], now[i].wrapping_sub(last[i]));
            }
            let tail = format!(
                "stage={} in_ring={} headroom={} cap_gain={:.1} out_gain={:.3} lat_ms={:.1}{extra}",
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
        wait();
        let stopping = shared.stop.load(Ordering::Acquire);
        let t = clock();
        let span = t.saturating_sub(last_us);
        if !stopping && span < interval_us {
            continue;
        }
        let now = read();
        let extra = gauges();
        emit(line(&last, now, t, span, stopping, &extra));
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Starvation {
    /// The processor's stage ran the model.
    pub ran_model: bool,
    /// Unprocessed input waiting in ring A, in frames.
    pub backlog_frames: usize,
    /// The stage was `WarmingUp` (its output is the passthrough): never forces a fallback.
    pub warming: bool,
    /// Set by the render thread when the underrun is counted: the overload judgement
    /// (ruling 24, `render::OverloadJudge`) says this one forces `Fallback(Overload)`.
    pub force: bool,
}

const SNAPSHOT_MODEL_BIT: u64 = 1 << 63;
const SNAPSHOT_WARMING_BIT: u64 = 1 << 62;
const SNAPSHOT_FORCE_BIT: u64 = 1 << 61;
const SNAPSHOT_BACKLOG_MASK: u64 = SNAPSHOT_FORCE_BIT - 1;

/// Packs a [`Starvation`] into one word (bit 63 = model ran, 62 = warming, 61 = force, low
/// bits = backlog frames) so the render thread publishes it atomically, without a torn set
/// across two underruns.
pub fn pack_starvation(s: Starvation) -> u64 {
    let mut w = (s.backlog_frames as u64).min(SNAPSHOT_BACKLOG_MASK);
    for (on, bit) in [
        (s.ran_model, SNAPSHOT_MODEL_BIT),
        (s.warming, SNAPSHOT_WARMING_BIT),
        (s.force, SNAPSHOT_FORCE_BIT),
    ] {
        if on {
            w |= bit;
        }
    }
    w
}

/// Inverse of [`pack_starvation`].
pub fn unpack_starvation(word: u64) -> Starvation {
    Starvation {
        ran_model: word & SNAPSHOT_MODEL_BIT != 0,
        backlog_frames: (word & SNAPSHOT_BACKLOG_MASK) as usize,
        warming: word & SNAPSHOT_WARMING_BIT != 0,
        force: word & SNAPSHOT_FORCE_BIT != 0,
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
/// the calibrated system path (`render::SYSTEM_PATH_PERIOD_TENTHS`) on top of it.
pub const LATENCY_BUDGET_FRAMES: usize = 2_205;

/// Gains as `f32` bit patterns; published by the engine loop from the Holder every 1 ms.
#[derive(Debug)]
pub struct SharedGains {
    pub capture_gain_bits: AtomicU32,
    pub output_gain_bits: AtomicU32,
    /// The Holder's [`AttachRamp`] (R2) packed as [`ATTACH_VALID`] | epoch << 16 | steps;
    /// 0 = none. Written after `attach_original_bits` (see [`SharedGains::set_attach`]).
    attach_word: AtomicU64,
    /// [`AttachRamp::original`] as `f32` bits, valid only with the word around it.
    attach_original_bits: AtomicU32,
}

/// Set in a published [`SharedGains::attach_word`].
const ATTACH_VALID: u64 = 1 << 63;
/// Low bits of the attach word holding [`AttachRamp::steps`] (at most `RAMP_STEPS`).
const ATTACH_STEPS_MASK: u64 = 0xFFFF;
const ATTACH_EPOCH_SHIFT: u32 = 16;

impl SharedGains {
    pub fn new(capture_gain: f32, output_gain: f32) -> Self {
        Self {
            capture_gain_bits: AtomicU32::new(capture_gain.to_bits()),
            output_gain_bits: AtomicU32::new(output_gain.to_bits()),
            attach_word: AtomicU64::new(0),
            attach_original_bits: AtomicU32::new(0),
        }
    }

    /// Publishes the Holder's attach ramp (engine loop, every tick). `original` is written
    /// first, then the word (Release), so a reader that sees the word sees its original.
    /// When the original changes the word is cleared first, so a reader can never pair a new
    /// original with an old word.
    pub fn set_attach(&self, r: Option<AttachRamp>) {
        let Some(r) = r else {
            self.attach_word.store(0, Ordering::Release);
            return;
        };
        let bits = r.original.to_bits();
        if self.attach_original_bits.load(Ordering::Relaxed) != bits {
            self.attach_word.store(0, Ordering::Relaxed);
            fence(Ordering::Release);
            self.attach_original_bits.store(bits, Ordering::Relaxed);
        }
        let steps = u64::from(r.steps).min(ATTACH_STEPS_MASK);
        let word = ATTACH_VALID | (u64::from(r.epoch) << ATTACH_EPOCH_SHIFT) | steps;
        self.attach_word.store(word, Ordering::Release);
    }

    /// The published attach ramp; lock-free and allocation-free (capture thread). Reads the
    /// word, the original, then the word again: `None` when the two words differ (a write in
    /// between), when nothing is published, or when the original is not finite and positive.
    pub fn attach(&self) -> Option<AttachRamp> {
        let word = self.attach_word.load(Ordering::Acquire);
        if word & ATTACH_VALID == 0 {
            return None;
        }
        let original = f32::from_bits(self.attach_original_bits.load(Ordering::Acquire));
        if self.attach_word.load(Ordering::Acquire) != word {
            return None;
        }
        if !original.is_finite() || original <= 0.0 {
            return None;
        }
        Some(AttachRamp {
            epoch: (word >> ATTACH_EPOCH_SHIFT) as u32,
            steps: (word & ATTACH_STEPS_MASK) as u32,
            original,
        })
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
    /// Frames the processing thread has taken from ring A and not yet pushed to ring B: one
    /// hop while it processes a block (for the model, most of a hop's time), else 0. Part of
    /// the latency estimate; without it a device wake during inference reads one hop low.
    pub proc_in_flight_frames: AtomicU32,
    /// Processor latency and hop (published by the processing thread).
    pub proc_latency_frames: AtomicU32,
    pub proc_hop: AtomicU32,
    pub output_failed: AtomicBool,
    /// Set by processing when it accepts a user "on"; render takes it and pre-rolls once.
    pub preroll_request: AtomicBool,
    /// Snapshot taken when the output starved ([`pack_starvation`]), published with each
    /// counted underrun before `AudioStats::underruns` is incremented.
    pub underrun_snapshot: AtomicU64,
    /// `now_us()` when the processing thread finished the block it last pushed to ring B
    /// (0 before the first); ruling 24 log.
    pub last_push_us: AtomicU64,
    /// `now_us()` when the block being processed started, 0 when none; ruling 24 log.
    pub block_start_us: AtomicU64,
    /// Forced fallbacks and overload retries, for the engine log (ruling 24).
    pub fallback_log: FallbackLog,
    /// Wakes the processing thread (capture pushed data, control message, stop).
    pub wake: OwnedEvent,
    /// Wakes the render thread: set by the processing thread after every block it pushes to
    /// ring B, so the block is written to the device on arrival (O2).
    pub render_wake: OwnedEvent,
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
    /// Forced fallbacks and retries already logged ([`AudioHandle::take_fallback_log`]).
    logged_forced: AtomicU32,
    logged_retries: AtomicU32,
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
            proc_in_flight_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(processor.latency_frames() as u32),
            proc_hop: AtomicU32::new(processor.hop() as u32),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
            last_push_us: AtomicU64::new(0),
            block_start_us: AtomicU64::new(0),
            fallback_log: FallbackLog::default(),
            wake: OwnedEvent::new()?,
            render_wake: OwnedEvent::new()?,
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
            logged_forced: AtomicU32::new(0),
            logged_retries: AtomicU32::new(0),
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

    /// The next engine log line about a forced fallback or an overload retry not logged yet
    /// (ruling 24); the engine loop calls it every tick.
    pub fn take_fallback_log(&self) -> Option<String> {
        let log = &self.shared.fallback_log;
        let run = self.shared.run_id;
        let forced = log.forced.load(Ordering::Acquire);
        if forced != self.logged_forced.swap(forced, Ordering::Relaxed) {
            return Some(fallback_line(run, log, now_us()));
        }
        let retries = log.retries.load(Ordering::Acquire);
        if retries != self.logged_retries.swap(retries, Ordering::Relaxed) {
            return Some(format!(
                "devocal audio: run={run} overload retry {retries}: devocal on again (with \
                 pre-roll)"
            ));
        }
        None
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

/// One-shot high-resolution waitable timer (auto-reset), closed on drop. The render thread
/// arms it after each device wake for the read deadline (O2). A kernel timer, not an audio
/// device.
pub(crate) struct HiResTimer(HANDLE);

// SAFETY: a timer handle may be set and waited on from any thread.
unsafe impl Send for HiResTimer {}
unsafe impl Sync for HiResTimer {}

impl HiResTimer {
    /// `None` where high-resolution timers are unavailable (before Windows 10 1803); the
    /// renderer then pads on device wakes as before.
    pub fn new() -> Option<Self> {
        unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .ok()
        .map(HiResTimer)
    }

    /// Signals once, `us` microseconds from now (re-arming replaces a pending due time and
    /// resets the signal). Never allocates or blocks.
    pub fn arm_in_us(&self, us: u64) {
        // Relative due time: negative, in 100 ns units.
        let due = -(us.min(i64::MAX as u64 / 10) as i64 * 10);
        let _ = unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false) };
    }

    /// Stops a pending due time and clears a signal not yet waited on, so nothing armed
    /// before fires afterwards. Not on the per-block path (a new stream).
    pub fn cancel(&self) {
        let _ = unsafe { CancelWaitableTimer(self.0) };
        // An auto-reset timer that already fired stays signalled until a wait takes it.
        let _ = unsafe { WaitForSingleObject(self.0, 0) };
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for HiResTimer {
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
            proc_in_flight_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(0),
            proc_hop: AtomicU32::new(128),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
            last_push_us: AtomicU64::new(0),
            block_start_us: AtomicU64::new(0),
            fallback_log: FallbackLog::default(),
            wake: OwnedEvent::new().unwrap(),
            render_wake: OwnedEvent::new().unwrap(),
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

    /// Ruling 17 (4): the loop runs on the test thread against a simulated clock (each poll
    /// wait advances it 50 ms; the stop comes after the fifth), so machine load cannot shift
    /// the lines: start, full lines at 100 and 200 ms, a partial one of 50 ms at 250 ms.
    #[test]
    fn diag_loop_emits_a_final_partial_line_after_stop() {
        use std::cell::Cell;
        let stats = Arc::new(AudioStats::default());
        let shared = Arc::new(test_shared());
        let gains = Arc::new(SharedGains::new(1.0, 1.0));
        let t0 = 7_000_000u64;
        let clock = Cell::new(t0);
        let polls = Cell::new(0u32);
        let mut lines = Vec::new();
        diag_loop_with(
            stats.clone(),
            shared.clone(),
            gains,
            100_000,
            || {
                clock.set(clock.get() + 50_000);
                polls.set(polls.get() + 1);
                stats.diag.render_wakes.fetch_add(3, Ordering::Relaxed);
                if polls.get() == 5 {
                    shared.stop.store(true, Ordering::Release);
                }
            },
            || clock.get(),
            |l| lines.push(l),
        );
        assert_eq!(polls.get(), 5, "returns at the first poll after the stop");
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[0].contains(" start "), "{}", lines[0]);
        let field = |l: &str, key: &str| -> f64 {
            l.split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in {l}"))
                .parse()
                .unwrap()
        };
        let (last, full) = lines[1..].split_last().unwrap();
        assert!(!full.is_empty(), "at least one full line: {lines:?}");
        for l in full {
            assert!(!l.contains("partial"), "{l}");
            assert_eq!(field(l, "span_ms="), 100.0, "{l}");
            assert_eq!(field(l, "r_wakes="), 6.0, "two polls' counts: {l}");
        }
        assert!(last.ends_with(" partial"), "{last}");
        assert!(field(last, "span_ms=") < 100.0, "{last}");
        assert_eq!(field(last, "span_ms="), 50.0, "{last}");
        let run = format!("run={}", shared.run_id);
        assert!(lines.iter().all(|l| l.contains(&run)), "{lines:?}");
        let ts: Vec<f64> = lines.iter().map(|l| field(l, "t=")).collect();
        assert!(ts.windows(2).all(|w| w[1] > w[0]), "{ts:?}");
        assert_eq!(ts, vec![7.0, 7.1, 7.2, 7.25]);
    }

    /// Ruling 24: the new columns come after `lat_ms` (existing order unchanged, ` partial`
    /// still last); counters are per-line deltas, gauges are reset by every line.
    #[test]
    fn diag_lines_end_with_the_ruling_24_columns() {
        use std::cell::Cell;
        let stats = Arc::new(AudioStats::default());
        let shared = Arc::new(test_shared());
        let gains = Arc::new(SharedGains::new(1.0, 1.0));
        let clock = Cell::new(7_000_000u64);
        let polls = Cell::new(0u32);
        let mut lines = Vec::new();
        diag_loop_with(
            stats.clone(),
            shared.clone(),
            gains,
            100_000,
            || {
                clock.set(clock.get() + 50_000);
                polls.set(polls.get() + 1);
                let d = &stats.diag;
                d.note_block_time(2_500);
                match polls.get() {
                    1 => {
                        d.note_block_time(5_200);
                        d.capture_gap_max_us.fetch_max(10_400, Ordering::Relaxed);
                        d.note_check_margin(120);
                        d.deadline_late_max_us.fetch_max(30, Ordering::Relaxed);
                    }
                    3 => {
                        d.note_check_margin(-16);
                        d.note_check_margin(7);
                    }
                    5 => shared.stop.store(true, Ordering::Release),
                    _ => {}
                }
            },
            || clock.get(),
            |l| lines.push(l),
        );
        assert_eq!(lines.len(), 4, "{lines:?}");
        let tail = |l: &str| l[l.find(" lat_ms=").unwrap()..].to_string();
        assert!(
            tail(&lines[1]).ends_with(
                " proc_max_us=5200 proc_over2=3 proc_over3=1 proc_over5=1 \
                 cap_gap_max_us=10400 chk_margin_min=120 dl_late_max_us=30"
            ),
            "{}",
            lines[1]
        );
        assert!(
            tail(&lines[2]).ends_with(
                " proc_max_us=2500 proc_over2=2 proc_over3=0 proc_over5=0 cap_gap_max_us=0 \
                 chk_margin_min=-16 dl_late_max_us=0"
            ),
            "{}",
            lines[2]
        );
        assert!(
            lines[3].ends_with(" chk_margin_min=- dl_late_max_us=0 partial"),
            "{}",
            lines[3]
        );
        // Existing columns keep their order, the new ones follow `lat_ms`.
        let l = &lines[1];
        let at = |k: &str| l.find(k).unwrap_or_else(|| panic!("{k} in {l}"));
        assert!(at(" underruns=") < at(" stage=") && at(" lat_ms=") < at(" proc_max_us="));
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

    /// O2's deadline timer is a kernel timer (not an audio device). Systems without
    /// high-resolution waitable timers (before Windows 10 1803) skip the check.
    #[test]
    fn hires_timer_fires_after_its_delay() {
        let Some(timer) = HiResTimer::new() else {
            eprintln!("skipped: CreateWaitableTimerExW(HIGH_RESOLUTION) failed on this system");
            return;
        };
        let t0 = now_us();
        timer.arm_in_us(2_000);
        let r = unsafe { WaitForSingleObject(timer.raw(), 50) };
        let elapsed = now_us() - t0;
        assert_eq!(r, WAIT_OBJECT_0, "signalled within 50 ms");
        assert!(elapsed >= 1_500, "{elapsed} us");
    }

    /// `cancel` drops both a pending due time and a signal that already fired.
    #[test]
    fn hires_timer_cancel_clears_pending_and_fired() {
        let Some(timer) = HiResTimer::new() else {
            eprintln!("skipped: CreateWaitableTimerExW(HIGH_RESOLUTION) failed on this system");
            return;
        };
        timer.arm_in_us(2_000);
        timer.cancel();
        assert_ne!(
            unsafe { WaitForSingleObject(timer.raw(), 20) },
            WAIT_OBJECT_0,
            "pending due time cancelled"
        );
        timer.arm_in_us(500);
        thread::sleep(Duration::from_millis(10));
        timer.cancel();
        assert_ne!(
            unsafe { WaitForSingleObject(timer.raw(), 0) },
            WAIT_OBJECT_0,
            "fired signal cleared"
        );
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
        let mut headroom = render::Headroom::new();
        let mut missed = render::MissedDeadlines::new();
        let mut power = 0.0f64;
        let mut block = vec![0.25f32; 441 * 2];
        let mut loud = vec![3.0f32; 441 * 2];
        let timer = HiResTimer::new();
        let event = OwnedEvent::new().unwrap();
        let gains = SharedGains::new(1.0, 1.0);
        gains.set_attach(Some(AttachRamp {
            epoch: 1,
            steps: 2,
            original: 0.5,
        }));
        let mut confirm = confirm::LevelConfirm::new();
        // Reference at 0.1; the lowered packet at step 1 of original 0.5 (about -18.5 dB).
        let reference = vec![0.1f32; 441 * 2];
        let mut lowered = vec![0.0f32; 441 * 2];
        let mut confirmed = 0u64;
        let mut overload = render::OverloadJudge::new();
        let fallback_log = FallbackLog::default();
        let before = alloc_count::this_thread();
        for i in 0..50u64 {
            cond.condition(&mut loud, 1.0, &stats, &follow, |_| {});
            cond.condition(&mut block, 1.0, &stats, &follow, |_| {});
            // R2: reference (no ramp), then a window, then the ramp gone again.
            let ramp = (i % 25 >= 10).then_some(AttachRamp {
                epoch: (i / 25) as u32,
                steps: 2,
                original: 0.5,
            });
            let in_window = ramp.is_some();
            confirm.observe(ramp, i * 10_000);
            if in_window {
                lowered.fill(0.0119);
            } else {
                lowered.copy_from_slice(&reference);
            }
            cond.condition_with(
                &mut lowered,
                |raw| confirm.chunk_gain(raw, 1.0),
                &stats,
                &follow,
                |_| {},
            );
            confirmed += confirm.take_counts().0;
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
                    warming: false,
                    force: false,
                },
            );
            let _ = judge.poll(i + 20, i + 30);
            let _ = render::grow_headroom(i as usize, 128, MAX_EXTRA_HEADROOM_FRAMES);
            headroom.on_underrun(i % 2 == 0, 128, MAX_EXTRA_HEADROOM_FRAMES, i * 3_000_000);
            headroom.add_preroll(64, i * 3_000_000 + 1);
            let _ = headroom.release(Stage::WarmingUp, i * 3_000_000 + 2);
            let _ = headroom.release(Stage::Devocal, i * 3_000_000 + 2_900_000);
            let _ = headroom.total();
            let _ = render::decay_now(0.3, 0.5, i * 100_000);
            power = render::track_power(power, &block);
            let _ = is_backlogged(true, 128, 128);
            // Ruling 24: the overload judgement, the starvation detail and the diag gauges.
            let seen = Starvation {
                ran_model: true,
                backlog_frames: (i as usize) * 7,
                ..Starvation::default()
            };
            let v = overload.judge(seen, 128, i % 3 == 0, i * 4_000_000);
            fallback_log.set_detail(StarvationDetail {
                verdict: v.code(),
                ..StarvationDetail::default()
            });
            stats.diag.note_block_time(i * 900);
            stats.diag.note_check_margin(i as i64 - 50);
            let _ = capture::guard_block(&mut block);
            let wake = render::wake_from_wait(i as u32 % 4, true);
            let deadline = render::deadline_mode(timer.is_some(), 441);
            let _ = render::plan_fill(wake, deadline, i as usize, 300, 4_000, 441, 569);
            let _ = missed.miss(i * 700_000);
            if i % 10 == 0 {
                missed.reset();
            }
            if let Some(t) = &timer {
                t.arm_in_us(8_500);
            }
            event.set();
            let _ = gains.attach();
        }
        assert_eq!(alloc_count::this_thread() - before, 0);
        assert!(power.is_finite());
        assert!(confirmed > 0, "the confirmed branch ran");
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
                warming: false,
                force: false,
            },
            Starvation {
                ran_model: false,
                backlog_frames: 44_100,
                warming: false,
                force: false,
            },
            Starvation {
                ran_model: true,
                backlog_frames: 300,
                warming: false,
                force: false,
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

    #[test]
    fn attach_ramp_round_trips_through_shared_gains() {
        let g = SharedGains::new(1.0, 0.0);
        assert_eq!(g.attach(), None, "nothing published yet");
        let r = AttachRamp {
            epoch: 7,
            steps: 3,
            original: 0.5,
        };
        g.set_attach(Some(r));
        assert_eq!(g.attach(), Some(r));
        let r2 = AttachRamp {
            epoch: u32::MAX,
            steps: 4,
            original: 0.25,
        };
        g.set_attach(Some(r2));
        assert_eq!(g.attach(), Some(r2));
        g.set_attach(None);
        assert_eq!(g.attach(), None);
        g.set_attach(Some(AttachRamp {
            original: f32::NAN,
            ..r
        }));
        assert_eq!(g.attach(), None, "NaN original");
        g.set_attach(Some(AttachRamp { original: 0.0, ..r }));
        assert_eq!(g.attach(), None, "zero original");
        g.set_attach(Some(r));
        assert_eq!(g.attach(), Some(r));
        // The gains themselves are untouched.
        assert_eq!((g.capture_gain(), g.output_gain()), (1.0, 0.0));
    }
}
