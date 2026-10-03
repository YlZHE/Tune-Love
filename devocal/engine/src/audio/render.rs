//! Render thread: plays ring B on the player's endpoint.
//!
//! - Endpoint mix format exactly 44 100 Hz float (stereo or more): `IAudioClient3` activated
//!   directly, `InitializeSharedAudioStream` at the minimum `GetSharedModeEnginePeriod`.
//! - Otherwise: wasapi `EventsShared { autoconvert: true, buffer_duration_hns: 0 }`.
//!
//! Queue target = one device period + one hop (+ jitter headroom), device padding plus ring B.
//! The thread waits on the device event, ring B's data event (`Shared::render_wake`, set by
//! the processing thread after every block) and, in deadline mode, a high-resolution timer
//! ([`Wake`]). Every wake writes the ring B frames it has, up to the target ([`plan_fill`]),
//! so a block reaches the device as soon as it is processed (O2). Silence is padded only at
//! a check: the read deadline [`DEADLINE_GUARD_US`] before the device's next read (the timer,
//! armed on each device wake), or a 20 ms wait with no wake at all. A device wake that comes
//! while its deadline is still armed (the deadline never fired: the wake before was handled
//! late, or the timer was) is a check only if the device ran dry (nothing queued, nothing in
//! ring B); after [`MISSED_DEADLINE_LIMIT`] such misses within [`MISSED_DEADLINE_WINDOW_US`]
//! the stream checks on every device wake until it is reopened. Without a timer, or with a
//! period of at most two guards, every device wake is a check, as before O2.
//! If a check finds less than one period queued the device would starve at its next read, so
//! the shortfall up to the target is written as silence: the real frames before it fade out
//! (`fade_edges`), the audio after it fades in, and one underrun is counted per gap if input
//! kept flowing (`UnderrunJudge`; a gap ends once a check finds no shortfall after real
//! frames were written); a short queue at any other wake is not a starvation. Trim watches
//! the queue, and the latency estimate is published, on device and timeout wakes only.
//! Pre-roll, headroom release and decay, fades and the underrun judge run on every wake.
//!
//! Ruling 18: an underrun while the processing thread was not behind is jitter and raises the
//! target by one hop (at most one capture packet in total). Ruling 24: so does the first one
//! with the processing thread behind (a hiccup, [`OverloadJudge`]); only a second within
//! [`OVERLOAD_REPEAT_US`], or one with the jitter headroom at its cap, is published as forcing
//! the fallback, and never one during the warm-up. When a user "on" is accepted the
//! output pre-rolls once by about one capture packet of silence. Both are released again (O1,
//! [`Headroom`]): the pre-roll once the model has warmed up, the jitter part after 5 s
//! without an underrun (backing off up to 60 s while jitter keeps returning); the released
//! audio is skipped at a quiet spot (at most 1 s later), down to the lowest queue the checks
//! saw beyond the target, and not at all if nothing extra is queued (paused). When devocal
//! goes off (or falls back), the queue the checks see rises (the model delivers a packet's
//! hops later than the passthrough does), so audio devocal left queued is skipped the same
//! way once the crossfade back has left ring B, if it is at least a hop (ruling 21); no
//! decay skips during a crossfade. A pre-roll writes exactly the silence it adds to the
//! headroom. A queue more than 20 ms over target for a whole second is trimmed back. Both
//! skips fade out, skip and fade in; a decay skip larger than ring B goes on discarding
//! arriving frames until it is done. Output gain is ramped across each write; every sample is
//! clamped to [-1, 1] (non-finite -> 0) before it reaches the device.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use rtrb::Consumer;
use wasapi::{
    AudioClient, AudioRenderClient, Device, Direction, Handle, SampleType, StreamMode, WaveFormat,
};
use windows::core::{GUID, HSTRING};
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioClient3, IAudioRenderClient, IAudioSessionControl, IMMDevice,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::WaitForMultipleObjects;

use super::endpoint::{find_render_device, low_latency_eligible, read_mix_format, CoTaskFormat};
use super::{
    count_underrun, edge_fade_frames, is_backlogged, now_us, pack_starvation, render_open_line,
    stage_from_code, stage_runs_model, AudioStats, ComGuard, FadeIn, HiResTimer, Mmcss, OwnedEvent,
    RenderCommand, Shared, SharedGains, Starvation, StarvationDetail, INPUT_FLOWING_US,
    MAX_EXTRA_HEADROOM_FRAMES, PREROLL_FRAMES,
};
use crate::dsp::{fade_edges, frames_for_ms, SAMPLE_RATE};
use crate::processor::Stage;

/// Trim only when the queue exceeds the target by more than this...
pub const TRIM_MARGIN_MS: f32 = 20.0;
/// ...continuously for this long.
pub const TRIM_HOLD_US: u64 = 1_000_000;
const WAIT_MS: u32 = 20;
/// The render session's name in the volume mixer (the UI tells the user to adjust this app's
/// volume there; without a name the row shows "devocal-engine").
pub const SESSION_DISPLAY_NAME: &str = "Tune Love 去人声";
/// A failure to name the session is logged once per process, then ignored.
static DISPLAY_NAME_FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Logs a failed [`SESSION_DISPLAY_NAME`] update the first time `logged` is unset; never fatal.
/// Returns whether it logged.
fn note_display_name(result: Result<(), String>, logged: &AtomicBool) -> bool {
    match result {
        Err(e) if !logged.swap(true, Ordering::Relaxed) => {
            eprintln!("devocal audio: could not name the render session (ignored): {e}");
            true
        }
        _ => false,
    }
}
const FAILED_POLL: Duration = Duration::from_millis(10);

/// The read deadline: this long before the device's next read (one period after its wake)
/// the queue is checked and, if short, padded (O2).
pub const DEADLINE_GUARD_US: u64 = 1_500;

/// A missed deadline is isolated unless this many fall within [`MISSED_DEADLINE_WINDOW_US`];
/// then the stream checks on device wakes, as before O2, until it is reopened (ruling 17).
pub const MISSED_DEADLINE_LIMIT: usize = 3;
/// See [`MISSED_DEADLINE_LIMIT`].
pub const MISSED_DEADLINE_WINDOW_US: u64 = 2_000_000;

/// The times of a stream's last [`MISSED_DEADLINE_LIMIT`] missed deadlines. Pure.
pub(crate) struct MissedDeadlines {
    at: [u64; MISSED_DEADLINE_LIMIT],
    seen: usize,
}

impl MissedDeadlines {
    pub fn new() -> Self {
        Self {
            at: [0; MISSED_DEADLINE_LIMIT],
            seen: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Records a missed deadline at `now_us`; true when it and the
    /// [`MISSED_DEADLINE_LIMIT`] - 1 before it all fall within the last
    /// [`MISSED_DEADLINE_WINDOW_US`] (the oldest less than that long ago: one miss a second
    /// stays isolated).
    pub fn miss(&mut self, now_us: u64) -> bool {
        self.at.rotate_left(1);
        self.at[MISSED_DEADLINE_LIMIT - 1] = now_us;
        self.seen = (self.seen + 1).min(MISSED_DEADLINE_LIMIT);
        self.seen == MISSED_DEADLINE_LIMIT
            && now_us.saturating_sub(self.at[0]) < MISSED_DEADLINE_WINDOW_US
    }
}

/// Why the render thread woke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wake {
    /// The device event: the device has just read a period.
    Device,
    /// The processing thread pushed a block to ring B.
    Data,
    /// The read deadline timer.
    Deadline,
    /// Nothing for 20 ms (or the wait failed).
    Timeout,
}

/// What one wake writes: `real` frames from ring B, then `silence` frames of padding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct FillPlan {
    pub real: usize,
    pub silence: usize,
}

/// One wake's write, with the queue counted as device `padding` plus ring B (`avail`
/// frames): real frames up to the `target`, as many as fit and are available. Padding is
/// checked only at the read deadline in deadline mode, at a timeout, and (outside deadline
/// mode) at every device wake; a check that finds less than one `period` queued after the
/// real frames pads with silence up to the target (or the free `room`). Pure.
pub(crate) fn plan_fill(
    wake: Wake,
    deadline_mode: bool,
    padding: usize,
    avail: usize,
    room: usize,
    period: usize,
    target: usize,
) -> FillPlan {
    let real = target.saturating_sub(padding).min(room).min(avail);
    let silence = if checks_padding(wake, deadline_mode) && padding + real < period {
        target.min(padding + room).saturating_sub(padding + real)
    } else {
        0
    };
    FillPlan { real, silence }
}

/// Whether a `wake` checks the queue for a shortfall (and pads it): the read deadline in
/// deadline mode, every device wake outside it, and a timeout in both.
fn checks_padding(wake: Wake, deadline_mode: bool) -> bool {
    match wake {
        Wake::Deadline => deadline_mode,
        Wake::Device => !deadline_mode,
        Wake::Timeout => true,
        Wake::Data => false,
    }
}

/// Microseconds of `frames` at the engine rate.
fn frames_us(frames: usize) -> u64 {
    frames as u64 * 1_000_000 / u64::from(SAMPLE_RATE)
}

/// Deadline mode needs the timer and a period longer than two guards (else the deadline
/// would come too soon after the device wake to be worth a separate check).
pub(crate) fn deadline_mode(has_timer: bool, period_frames: usize) -> bool {
    has_timer && frames_us(period_frames) > 2 * DEADLINE_GUARD_US
}

/// Maps a `WaitForMultipleObjects` result over [device event, data event, timer?] to a
/// [`Wake`]; the timer is waited on only with `has_timer`. Anything else (timeout, failure)
/// is a [`Wake::Timeout`].
pub(crate) fn wake_from_wait(result: u32, has_timer: bool) -> Wake {
    match result.wrapping_sub(WAIT_OBJECT_0.0) {
        0 => Wake::Device,
        1 => Wake::Data,
        2 if has_timer => Wake::Deadline,
        _ => Wake::Timeout,
    }
}

/// Decides when a persistently over-full queue is trimmed. Pure; times in microseconds.
pub struct TrimPolicy {
    target: usize,
    margin: usize,
    /// Since when the queue has been over target + margin, and the smallest queue seen since.
    over: Option<(u64, usize)>,
}

impl TrimPolicy {
    pub fn new(target_frames: usize) -> Self {
        Self {
            target: target_frames,
            margin: frames_for_ms(TRIM_MARGIN_MS),
            over: None,
        }
    }

    pub fn set_target(&mut self, target_frames: usize) {
        if target_frames != self.target {
            self.target = target_frames;
            self.over = None;
        }
    }

    pub fn reset(&mut self) {
        self.over = None;
    }

    /// Feeds the current queue (device padding + ring frames). Returns the number of frames
    /// to drop once the queue has stayed above target + 20 ms for a full second: down to the
    /// target, but never more than the smallest queue seen in that second minus the target
    /// (input arrives in bursts, so the low point is what is really spare). The timer then
    /// restarts.
    pub fn observe(&mut self, queued_frames: usize, now_us: u64) -> Option<usize> {
        if queued_frames <= self.target + self.margin {
            self.over = None;
            return None;
        }
        match self.over {
            None => {
                self.over = Some((now_us, queued_frames));
                None
            }
            Some((since, low)) => {
                let low = low.min(queued_frames);
                if now_us.saturating_sub(since) >= TRIM_HOLD_US {
                    self.over = None;
                    Some(low - self.target)
                } else {
                    self.over = Some((since, low));
                    None
                }
            }
        }
    }
}

/// Tells an underrun (we starved while the input kept flowing: processing or scheduling too
/// slow) from the input stopping (player paused or ended, which also empties the queue a few
/// ms after the last packet). A starvation is armed only if input was flowing when it began
/// ([`count_underrun`]) and counts only if another capture packet then arrives within
/// [`INPUT_FLOWING_US`]; otherwise it was a pause. Pure; times in microseconds.
pub struct UnderrunJudge {
    starved_at: Option<(u64, Starvation)>,
}

impl UnderrunJudge {
    pub fn new() -> Self {
        Self { starved_at: None }
    }

    pub fn reset(&mut self) {
        self.starved_at = None;
    }

    /// The output starved at `now_us` (call once per gap); `seen` is what the starvation
    /// looked like (model running, ring A backlog) and is returned if it counts. True when
    /// this call armed it.
    pub fn starved(&mut self, last_input_us: u64, now_us: u64, seen: Starvation) -> bool {
        if self.starved_at.is_none() && count_underrun(last_input_us, now_us) {
            self.starved_at = Some((now_us, seen));
            return true;
        }
        false
    }

    /// `Some` once for an armed starvation that input kept flowing through.
    pub fn poll(&mut self, last_input_us: u64, now_us: u64) -> Option<Starvation> {
        let (t, seen) = self.starved_at?;
        if last_input_us > t && last_input_us - t <= INPUT_FLOWING_US {
            self.starved_at = None;
            return Some(seen);
        }
        if now_us.saturating_sub(t) > INPUT_FLOWING_US {
            self.starved_at = None;
        }
        None
    }
}

/// Ruling 18: after a jitter underrun the render target grows by one hop, never beyond `cap`
/// frames of extra headroom in total.
pub fn grow_headroom(extra: usize, hop: usize, cap: usize) -> usize {
    (extra + hop).min(cap)
}

/// Ruling 24: a counted underrun with the processing thread behind forces the fallback only
/// if another one came less than this long before it (or the jitter headroom is at its cap).
pub const OVERLOAD_REPEAT_US: u64 = 10_000_000;

/// Ruling 24: how a counted underrun is judged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// The processing thread kept up (ring A held less than a hop) or the model was not
    /// running: jitter.
    Jitter,
    /// Behind, during the model's warm-up (its output is the passthrough): treated as jitter.
    WarmUp,
    /// Behind, and the first such within [`OVERLOAD_REPEAT_US`]: a hiccup (a slow hop or a
    /// late packet still being processed at the check), treated as jitter.
    Isolated,
    /// Behind again within [`OVERLOAD_REPEAT_US`]: forces the fallback.
    Repeated,
    /// Behind with the jitter headroom already at its cap: forces the fallback.
    AtCap,
}

const VERDICT_NAMES: [&str; 5] = ["jitter", "warm-up", "isolated", "repeated", "at-cap"];

impl Verdict {
    pub fn forces(self) -> bool {
        matches!(self, Verdict::Repeated | Verdict::AtCap)
    }

    /// For the log (`StarvationDetail::verdict`).
    pub fn code(self) -> u64 {
        self as u64
    }

    pub fn name(code: u64) -> &'static str {
        VERDICT_NAMES.get(code as usize).copied().unwrap_or("?")
    }
}

/// Ruling 24: decides whether a counted underrun forces `Fallback(Overload)`. Ring A holding
/// a hop at the starvation does not show by itself that the model is too slow: with
/// 441-frame packets and 128-frame hops it does for much of every packet, and one slow hop or
/// late packet starves a check by a few ms (fallback-investigation.md). So the first such
/// underrun is a hiccup, treated as jitter; a second within [`OVERLOAD_REPEAT_US`], or one
/// with the jitter headroom already at its cap, forces the fallback. A warm-up underrun never
/// forces, but counts as the earlier one. A genuinely slow model keeps underrunning (and the
/// LoadMonitor trips within its 1 s window). Pure; times in microseconds.
pub struct OverloadJudge {
    last_behind_us: Option<u64>,
}

impl OverloadJudge {
    pub fn new() -> Self {
        Self {
            last_behind_us: None,
        }
    }

    pub fn reset(&mut self) {
        self.last_behind_us = None;
    }

    pub fn judge(
        &mut self,
        seen: Starvation,
        hop: usize,
        jitter_at_cap: bool,
        now_us: u64,
    ) -> Verdict {
        if !is_backlogged(seen.ran_model, seen.backlog_frames, hop) {
            return Verdict::Jitter;
        }
        let repeat = self
            .last_behind_us
            .is_some_and(|t| now_us.saturating_sub(t) < OVERLOAD_REPEAT_US);
        self.last_behind_us = Some(now_us);
        if seen.warming {
            Verdict::WarmUp
        } else if repeat {
            Verdict::Repeated
        } else if jitter_at_cap {
            Verdict::AtCap
        } else {
            Verdict::Isolated
        }
    }
}

/// Jitter headroom is released after this long without a counted underrun (design: 5 s).
pub const HEADROOM_DECAY_US: u64 = 5_000_000;
/// Cap of the quiet-period backoff, and how long after a decay a new underrun still counts
/// as the jitter returning (which doubles the quiet period).
pub const HEADROOM_DECAY_MAX_US: u64 = 60_000_000;
/// A pre-roll is kept at least this long when no model stage has been seen yet: the
/// processing thread sets the request before it publishes `WarmingUp` (Review Focus 4).
pub const PREROLL_GRACE_US: u64 = 200_000;
/// A released headroom is skipped at a quiet spot, but waits at most this long for one.
pub const LOW_ENERGY_WAIT_US: u64 = 1_000_000;
/// A quiet spot: the frames to fade and skip have an RMS at most this fraction (-6 dB) of
/// the recent output RMS.
pub const LOW_ENERGY_RATIO: f32 = 0.5;
/// A released headroom is skipped only after the queue's low point has been watched this
/// long: ring B arrives in bursts, so one wake can overstate what is really spare.
pub const DECAY_OBSERVE_US: u64 = 30_000;
/// Time constant of the recent output power average, in frames (100 ms).
const RECENT_POWER_FRAMES: f64 = 4_410.0;

/// Render headroom on top of period + hop: jitter growth (ruling 18) plus the pre-roll of an
/// accepted "on", together at most the cap. Both are released again (O1): the pre-roll once
/// the model has warmed up, the jitter part after a quiet period without underruns that
/// backs off while jitter keeps returning. Pure; times in microseconds.
pub struct Headroom {
    jitter: usize,
    preroll: usize,
    preroll_at_us: u64,
    /// A model warm-up stage (`WarmingUp` / `FadingIn`) was seen since the pre-roll began.
    preroll_model_seen: bool,
    /// The quiet period runs from here: the last counted underrun (which is also the last
    /// growth; after a decay there is nothing left to release until the next underrun).
    last_underrun_us: u64,
    quiet_needed_us: u64,
    last_decay_us: Option<u64>,
}

impl Headroom {
    pub fn new() -> Self {
        Self {
            jitter: 0,
            preroll: 0,
            preroll_at_us: 0,
            preroll_model_seen: false,
            last_underrun_us: 0,
            quiet_needed_us: HEADROOM_DECAY_US,
            last_decay_us: None,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Jitter plus pre-roll headroom in frames. At most the `cap` passed to `on_underrun`
    /// provided every `add_preroll` stays within the headroom left below that cap (as
    /// [`preroll_frames`] sizes it); `add_preroll` itself does not clamp.
    pub fn total(&self) -> usize {
        self.jitter + self.preroll
    }

    /// The jitter part has grown to `cap` (ruling 24: no more growth can absorb a hiccup).
    pub fn jitter_at_cap(&self, cap: usize) -> bool {
        self.jitter >= cap
    }

    /// A counted underrun at `now_us`; `jitter` when the processing thread was not behind,
    /// which grows the jitter part by `hop` (total at most `cap`). Every underrun restarts
    /// the quiet period; one within [`HEADROOM_DECAY_MAX_US`] of the last decay doubles it
    /// (up to that cap), otherwise it is back to [`HEADROOM_DECAY_US`].
    pub fn on_underrun(&mut self, jitter: bool, hop: usize, cap: usize, now_us: u64) {
        self.last_underrun_us = now_us;
        self.quiet_needed_us = match self.last_decay_us {
            Some(d) if now_us.saturating_sub(d) < HEADROOM_DECAY_MAX_US => self
                .quiet_needed_us
                .saturating_mul(2)
                .min(HEADROOM_DECAY_MAX_US),
            _ => HEADROOM_DECAY_US,
        };
        if jitter {
            self.jitter = grow_headroom(self.jitter, hop, cap.saturating_sub(self.preroll));
        }
    }

    /// A pre-roll of `frames` began at `now_us`. Precondition: `frames` fits the headroom
    /// left below the cap (as sized by [`preroll_frames`]); not clamped here.
    pub fn add_preroll(&mut self, frames: usize, now_us: u64) {
        if frames == 0 {
            return;
        }
        self.preroll += frames;
        self.preroll_at_us = now_us;
        self.preroll_model_seen = false;
    }

    /// Releases what is no longer needed, given the processor's `stage`; returns the frames
    /// released. The pre-roll is held while the model warms up (`WarmingUp`, `FadingIn`)
    /// and released at any other stage once a warm-up stage was seen or
    /// [`PREROLL_GRACE_US`] has passed. The jitter part is released whole once the quiet
    /// period has passed without an underrun.
    pub fn release(&mut self, stage: Stage, now_us: u64) -> usize {
        let mut freed = 0;
        if self.preroll > 0 {
            if matches!(stage, Stage::WarmingUp | Stage::FadingIn) {
                self.preroll_model_seen = true;
            } else if self.preroll_model_seen
                || now_us.saturating_sub(self.preroll_at_us) >= PREROLL_GRACE_US
            {
                freed += self.preroll;
                self.preroll = 0;
                self.preroll_model_seen = false;
            }
        }
        if self.jitter > 0 && now_us.saturating_sub(self.last_underrun_us) >= self.quiet_needed_us {
            freed += self.jitter;
            self.jitter = 0;
            self.last_decay_us = Some(now_us);
        }
        freed
    }
}

/// Skip a released headroom now: the frames involved are quiet (RMS at most
/// [`LOW_ENERGY_RATIO`] of the recent output RMS), or it has waited [`LOW_ENERGY_WAIT_US`].
pub fn decay_now(window_rms: f32, recent_rms: f32, waited_us: u64) -> bool {
    window_rms <= LOW_ENERGY_RATIO * recent_rms || waited_us >= LOW_ENERGY_WAIT_US
}

/// Mean square of interleaved samples split across two slices (a ring chunk); 0 if empty.
fn mean_square(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len() + b.len();
    if n == 0 {
        return 0.0;
    }
    let sum: f64 = a
        .iter()
        .chain(b)
        .map(|&s| f64::from(s) * f64::from(s))
        .sum();
    sum / n as f64
}

/// Updates the exponential average of the per-frame mean square (time constant
/// [`RECENT_POWER_FRAMES`]) with `frames` (interleaved stereo); non-finite frames are skipped.
pub(crate) fn track_power(ms: f64, frames: &[f32]) -> f64 {
    let alpha = 1.0 / RECENT_POWER_FRAMES;
    frames.as_chunks::<2>().0.iter().fold(ms, |m, f| {
        let p = (f64::from(f[0]) * f64::from(f[0]) + f64::from(f[1]) * f64::from(f[1])) / 2.0;
        if p.is_finite() {
            m + (p - m) * alpha
        } else {
            m
        }
    })
}

/// Pre-roll size for an accepted "on" (0 = none). Never stacks: nothing while a pre-roll is
/// still being written, nothing when the queue (ring B + device) already holds target + hop,
/// and never more than the headroom left below `cap`: the pre-roll is counted inside the
/// headroom (`extra` = [`Headroom::total`]), so the target rises with it and the standing
/// latency stays within period + hop + `cap`.
pub fn preroll_frames(
    active: bool,
    queued: usize,
    target: usize,
    hop: usize,
    extra: usize,
    cap: usize,
) -> usize {
    if active || queued >= target + hop {
        return 0;
    }
    PREROLL_FRAMES.min(cap.saturating_sub(extra))
}

/// Ramps from the last output frame to silence across `out` (interleaved stereo; the last
/// frame is 0), so silence that starts with no real frames to fade does not step.
pub(crate) fn decay_from(last: [f32; 2], out: &mut [f32]) {
    let frames = out.len() / 2;
    for (i, f) in out.chunks_exact_mut(2).enumerate() {
        let g = 1.0 - (i + 1) as f32 / frames as f32;
        f[0] = last[0] * g;
        f[1] = last[1] * g;
    }
}

/// Fades out the frames before each gap marker (a ring B position where the output jumps to
/// silence). A marker is taken when first seen; its fade covers the 5 ms before it, or the
/// part of that not yet written. Gain matches `fade_edges`: the last frame before the gap
/// is 0.
pub(crate) struct GapFader {
    fade_frames: u64,
    /// `[start, end)` of the fade in progress.
    active: Option<(u64, u64)>,
}

impl GapFader {
    pub fn new(fade_frames: usize) -> Self {
        Self {
            fade_frames: fade_frames as u64,
            active: None,
        }
    }

    pub fn reset(&mut self) {
        self.active = None;
    }

    /// Applies fades to `frames` (interleaved stereo) which start at ring position `pos`;
    /// frames before `pos` have already been written.
    pub fn apply(&mut self, markers: &mut Consumer<u64>, pos: u64, frames: &mut [f32]) {
        let end_pos = pos + (frames.len() / 2) as u64;
        loop {
            if self.active.is_none() {
                match markers.peek() {
                    Ok(&m) => {
                        let _ = markers.pop();
                        if m <= pos {
                            continue; // the gap has begun already; nothing left to fade
                        }
                        let start = m.saturating_sub(self.fade_frames).max(pos);
                        self.active = Some((start, m));
                    }
                    Err(_) => return,
                }
            }
            let Some((start, end)) = self.active else {
                return;
            };
            if end <= pos {
                self.active = None;
                continue;
            }
            let span = (end - 1 - start) as f32;
            let from = start.max(pos);
            let to = end.min(end_pos);
            for x in from..to {
                let g = if span <= 0.0 {
                    0.0
                } else {
                    (end - 1 - x) as f32 / span
                };
                let i = ((x - pos) * 2) as usize;
                frames[i] *= g;
                frames[i + 1] *= g;
            }
            if end <= end_pos {
                self.active = None;
                continue;
            }
            return;
        }
    }
}

/// Multiplies by a gain ramping linearly from `from` to `to` across the block, then clamps
/// every sample to [-1, 1]; non-finite samples become 0.
pub(crate) fn finish_block(block: &mut [f32], from: f32, to: f32) {
    let frames = block.len() / 2;
    for (i, f) in block.chunks_exact_mut(2).enumerate() {
        let g = from + (to - from) * (i + 1) as f32 / frames as f32;
        for s in f.iter_mut() {
            let v = *s * g;
            *s = if v.is_finite() {
                v.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
    }
}

/// A render stream the loop can feed.
pub(crate) trait Sink {
    /// Frames the engine consumes per wake.
    fn period_frames(&self) -> usize;
    fn buffer_frames(&self) -> usize;
    fn padding(&self) -> Result<usize, String>;
    /// The device event (signalled after each device read); owned by the sink.
    fn event(&self) -> HANDLE;
    /// Writes interleaved stereo; `stereo.len() / 2` frames must fit the free space.
    fn write(&mut self, stereo: &[f32]) -> Result<(), String>;
    fn describe(&self) -> &'static str;
}

/// `IAudioClient3` at the minimum shared engine period, in the endpoint's own mix format.
struct LowLatencySink {
    // Field order is drop order: the render client and the audio client are released before
    // the event handle they signal is closed.
    render: IAudioRenderClient,
    client: IAudioClient3,
    event: OwnedEvent,
    channels: usize,
    period: usize,
    buffer: usize,
}

impl LowLatencySink {
    /// `Ok(None)` when the endpoint's mix format is not exactly 44.1 kHz float.
    fn open(dev: &IMMDevice) -> Result<Option<Self>, String> {
        unsafe {
            let client: IAudioClient3 = match dev.Activate(CLSCTX_ALL, None) {
                Ok(c) => c,
                Err(_) => return Ok(None), // IAudioClient3 unavailable
            };
            let fmt = CoTaskFormat(
                client
                    .GetMixFormat()
                    .map_err(|e| format!("mix format: {e}"))?,
            );
            let mix = read_mix_format(fmt.0);
            if !low_latency_eligible(mix) {
                return Ok(None);
            }
            let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
            client
                .GetSharedModeEnginePeriod(
                    fmt.0,
                    &mut default,
                    &mut fundamental,
                    &mut min,
                    &mut max,
                )
                .map_err(|e| format!("engine period: {e}"))?;
            client
                .InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, min, fmt.0, None)
                .map_err(|e| format!("InitializeSharedAudioStream({min} frames): {e}"))?;
            drop(fmt);
            let buffer = client
                .GetBufferSize()
                .map_err(|e| format!("buffer size: {e}"))? as usize;
            let event = OwnedEvent::new()?;
            client
                .SetEventHandle(event.raw())
                .map_err(|e| format!("render event: {e}"))?;
            let render: IAudioRenderClient = client
                .GetService()
                .map_err(|e| format!("render client: {e}"))?;
            let name = HSTRING::from(SESSION_DISPLAY_NAME);
            let named = client
                .GetService::<IAudioSessionControl>()
                .and_then(|c| c.SetDisplayName(&name, &GUID::zeroed()))
                .map_err(|e| e.to_string());
            note_display_name(named, &DISPLAY_NAME_FAILURE_LOGGED);
            client.Start().map_err(|e| format!("start render: {e}"))?;
            Ok(Some(Self {
                render,
                client,
                event,
                channels: usize::from(mix.channels),
                period: (min as usize).max(1),
                buffer,
            }))
        }
    }
}

impl Drop for LowLatencySink {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

impl Sink for LowLatencySink {
    fn period_frames(&self) -> usize {
        self.period
    }
    fn buffer_frames(&self) -> usize {
        self.buffer
    }
    fn padding(&self) -> Result<usize, String> {
        unsafe { self.client.GetCurrentPadding() }
            .map(|p| p as usize)
            .map_err(|e| format!("render padding: {e}"))
    }
    fn event(&self) -> HANDLE {
        self.event.raw()
    }
    fn write(&mut self, stereo: &[f32]) -> Result<(), String> {
        let frames = stereo.len() / 2;
        if frames == 0 {
            return Ok(());
        }
        unsafe {
            let ptr = self
                .render
                .GetBuffer(frames as u32)
                .map_err(|e| format!("render GetBuffer: {e}"))?
                .cast::<f32>();
            let ch = self.channels;
            for (i, f) in stereo.chunks_exact(2).enumerate() {
                let base = ptr.add(i * ch);
                base.write_unaligned(f[0]);
                base.add(1).write_unaligned(f[1]);
                for c in 2..ch {
                    base.add(c).write_unaligned(0.0);
                }
            }
            self.render
                .ReleaseBuffer(frames as u32, 0)
                .map_err(|e| format!("render ReleaseBuffer: {e}"))
        }
    }
    fn describe(&self) -> &'static str {
        "IAudioClient3 low-latency"
    }
}

/// wasapi shared mode with autoconvert (Windows resamples 44.1 kHz to the mix format).
struct WasapiSink {
    // Drop order: render client, audio client, then the event handle.
    render: AudioRenderClient,
    client: AudioClient,
    event: Handle,
    bytes: Vec<u8>,
    period: usize,
    buffer: usize,
}

impl WasapiSink {
    fn open(dev: IMMDevice) -> Result<Self, String> {
        let device = Device::from_immdevice(dev).map_err(|e| format!("render device: {e}"))?;
        let mut client = device
            .get_iaudioclient()
            .map_err(|e| format!("render client: {e}"))?;
        let fmt = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 2, None);
        client
            .initialize_client(
                &fmt,
                &Direction::Render,
                &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: 0,
                },
            )
            .map_err(|e| format!("initialise render: {e}"))?;
        let event = client
            .set_get_eventhandle()
            .map_err(|e| format!("render event: {e}"))?;
        let render = client
            .get_audiorenderclient()
            .map_err(|e| format!("render client: {e}"))?;
        let named = client
            .get_audiosessioncontrol()
            .and_then(|c| c.set_display_name(SESSION_DISPLAY_NAME))
            .map_err(|e| e.to_string());
        note_display_name(named, &DISPLAY_NAME_FAILURE_LOGGED);
        let buffer = client
            .get_buffer_size()
            .map_err(|e| format!("buffer size: {e}"))? as usize;
        let (default_hns, _min_hns) = client
            .get_device_period()
            .map_err(|e| format!("device period: {e}"))?;
        let period = ((default_hns.max(0) as u64 * u64::from(SAMPLE_RATE)).div_ceil(10_000_000)
            as usize)
            .clamp(1, buffer.max(1));
        client
            .start_stream()
            .map_err(|e| format!("start render: {e}"))?;
        Ok(Self {
            render,
            client,
            event,
            bytes: vec![0u8; buffer * 8],
            period,
            buffer,
        })
    }
}

impl Drop for WasapiSink {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

impl Sink for WasapiSink {
    fn period_frames(&self) -> usize {
        self.period
    }
    fn buffer_frames(&self) -> usize {
        self.buffer
    }
    fn padding(&self) -> Result<usize, String> {
        self.client
            .get_current_padding()
            .map(|p| p as usize)
            .map_err(|e| format!("render padding: {e}"))
    }
    fn event(&self) -> HANDLE {
        self.event.as_raw()
    }
    fn write(&mut self, stereo: &[f32]) -> Result<(), String> {
        let frames = stereo.len() / 2;
        let n = (frames * 8).min(self.bytes.len());
        for (b, s) in self.bytes[..n].chunks_exact_mut(4).zip(stereo) {
            b.copy_from_slice(&s.to_le_bytes());
        }
        self.render
            .write_to_device(n / 8, &self.bytes[..n], None)
            .map_err(|e| format!("render write: {e}"))
    }
    fn describe(&self) -> &'static str {
        "wasapi autoconvert"
    }
}

/// Opens the render stream on `endpoint_id` (`None` = default endpoint). COM (MTA) must be
/// initialised on the calling thread.
fn open_sink(endpoint_id: Option<&str>) -> Result<Box<dyn Sink>, String> {
    let dev = find_render_device(endpoint_id)?;
    match LowLatencySink::open(&dev) {
        Ok(Some(s)) => return Ok(Box::new(s)),
        Ok(None) => {}
        Err(e) => {
            eprintln!("devocal audio: low-latency render unavailable ({e}); using autoconvert")
        }
    }
    Ok(Box::new(WasapiSink::open(dev)?))
}

pub(crate) struct RenderCtx {
    pub endpoint: Option<String>,
    pub input: Consumer<f32>,
    pub markers: Consumer<u64>,
    pub control: Consumer<RenderCommand>,
    pub shared: Arc<Shared>,
    pub stats: Arc<AudioStats>,
    pub gains: Arc<SharedGains>,
}

/// Thread body. Reports the first open on `ready`; later failures set `output_failed` and
/// the thread keeps draining ring B until a rebind succeeds.
pub(crate) fn run(ctx: RenderCtx, ready: Sender<Result<(), String>>) {
    let _com = match ComGuard::init_mta() {
        Ok(c) => c,
        Err(e) => {
            ctx.shared.output_failed.store(true, Ordering::Release);
            let _ = ready.send(Err(format!("render: {e}")));
            return;
        }
    };
    let _mmcss = Mmcss::pro_audio("render");
    // Every COM object (the sink) lives inside `Renderer` and is dropped before `_com`.
    let mut r = Renderer::new(ctx);
    match open_sink(r.ctx.endpoint.as_deref()) {
        Ok(s) => {
            r.sink = Some(s);
            let _ = ready.send(Ok(()));
        }
        Err(e) => {
            let _ = ready.send(Err(format!("open output: {e}")));
            return;
        }
    }
    r.run();
}

struct Renderer {
    ctx: RenderCtx,
    sink: Option<Box<dyn Sink>>,
    /// Frames taken from ring B (written, skipped or discarded).
    read_pos: u64,
    staging: Vec<f32>,
    fade_frames: usize,
    fade_in: FadeIn,
    gaps: GapFader,
    trim: TrimPolicy,
    drop_pending: usize,
    /// Fed without a shortfall since the last gap; a starvation is judged only when primed.
    /// Set when a check wake ([`checks_padding`]) finds no shortfall after real frames were
    /// written since the gap (`fed`), on that wake or an earlier one (O2 writes most real
    /// frames on data wakes, which never check).
    primed: bool,
    /// Real frames written since the last gap.
    fed: bool,
    underruns: UnderrunJudge,
    /// Jitter and pre-roll headroom on top of period + hop (ruling 18, O1).
    headroom: Headroom,
    /// Released headroom still queued, to be skipped at a quiet spot; since when.
    decay_drop: usize,
    decay_since_us: u64,
    /// Lowest queue (device padding + ring B) seen at a check wake ([`checks_padding`], in
    /// the effective deadline mode) since `decay_since_us` (`usize::MAX` when no decay is
    /// pending, or no check wake has been seen since the release).
    decay_low: usize,
    /// Smallest spare the pending decay skips: 0 for released headroom, one hop for a
    /// settle after devocal (below that the queue is within O2's own spread).
    decay_min: usize,
    /// The stage seen at the previous wake ran the model.
    model_seen: bool,
    /// Devocal went off (or fell back): once ring B has been read up to here (past the
    /// crossfade back to the passthrough), a settle decay starts (ruling 21 b).
    settle_at: Option<u64>,
    /// The pending decay (or its skip in progress) was started by a settle; devocal back on
    /// cancels it (ruling 23).
    settling: bool,
    /// The pending `drop_pending` is a headroom decay (counted as `render_decay_frames`).
    drop_is_decay: bool,
    /// The pending skip's fade-out has been written; a decay is still discarding.
    drop_faded: bool,
    /// Exponential average of the mean square of the real frames written (`track_power`).
    recent_ms: f64,
    /// Pre-roll silence still to write, and whether its leading fade-out is still to do.
    preroll_left: usize,
    preroll_fade: bool,
    /// Last frame written (before the output gain), for `decay_from`.
    last_frame: [f32; 2],
    last_gain: f32,
    /// The read deadline timer (`None` if the system has no high-resolution timers).
    timer: Option<HiResTimer>,
    /// Pad only at the read deadline (O2); set each wait from the timer and the period.
    deadline_mode: bool,
    /// The deadline was armed by a device wake and has not fired (been handled) since. A
    /// device wake that finds it still armed checks the queue itself (ruling 8).
    deadline_armed: bool,
    /// This stream checks on device wakes, as before O2, until it is reopened: it missed
    /// too many deadlines (ruling 17).
    legacy_checks: bool,
    /// Recent missed deadlines of this stream.
    missed: MissedDeadlines,
    /// Ruling 24: whether a counted underrun forces the fallback.
    overload: OverloadJudge,
    /// What the armed starvation looked like, published when it is counted (ruling 24 log).
    starve_detail: StarvationDetail,
    /// When the armed deadline is due (for its lateness).
    deadline_due_us: u64,
}

impl Renderer {
    fn new(ctx: RenderCtx) -> Self {
        let fade_frames = edge_fade_frames();
        Self {
            ctx,
            sink: None,
            read_pos: 0,
            staging: Vec::new(),
            fade_frames,
            fade_in: FadeIn::new(fade_frames),
            gaps: GapFader::new(fade_frames),
            trim: TrimPolicy::new(0),
            drop_pending: 0,
            primed: false,
            fed: false,
            underruns: UnderrunJudge::new(),
            headroom: Headroom::new(),
            decay_drop: 0,
            decay_since_us: 0,
            decay_low: usize::MAX,
            decay_min: 0,
            model_seen: false,
            settle_at: None,
            settling: false,
            drop_is_decay: false,
            drop_faded: false,
            recent_ms: 0.0,
            preroll_left: 0,
            preroll_fade: false,
            last_frame: [0.0; 2],
            last_gain: 0.0,
            timer: None,
            deadline_mode: false,
            deadline_armed: false,
            legacy_checks: false,
            missed: MissedDeadlines::new(),
            overload: OverloadJudge::new(),
            starve_detail: StarvationDetail::default(),
            deadline_due_us: 0,
        }
    }

    fn run(&mut self) {
        self.timer = HiResTimer::new();
        if self.timer.is_none() {
            eprintln!(
                "devocal audio: high-resolution timer unavailable; render pads on device wakes"
            );
        }
        self.after_open();
        while !self.ctx.shared.stop.load(Ordering::Acquire) {
            while let Ok(cmd) = self.ctx.control.pop() {
                match cmd {
                    RenderCommand::Rebind(endpoint) => self.rebind(endpoint),
                }
            }
            if self.sink.is_none() {
                self.discard_all();
                self.publish(0);
                std::thread::sleep(FAILED_POLL);
                continue;
            }
            match self.fill() {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("devocal audio: output failed: {e}");
                    self.sink = None;
                    self.ctx.shared.output_failed.store(true, Ordering::Release);
                }
            }
        }
        // The sink (stops its stream) is dropped with `self`, before COM uninitialises.
        self.sink = None;
    }

    fn rebind(&mut self, endpoint: Option<String>) {
        self.sink = None; // stop and release the old stream first
        match open_sink(endpoint.as_deref()) {
            Ok(s) => {
                self.sink = Some(s);
                self.ctx.endpoint = endpoint;
                self.after_open();
                self.ctx
                    .shared
                    .output_failed
                    .store(false, Ordering::Release);
            }
            Err(e) => {
                eprintln!("devocal audio: rebind output failed: {e}");
                self.ctx.shared.output_failed.store(true, Ordering::Release);
            }
        }
    }

    /// Fresh stream: drop stale audio, start silent and fade in.
    ///
    /// `shared.preroll_request` is deliberately NOT cleared (ruling 23): the render thread
    /// reports `ready` before `run` calls this, so the engine's first "on" can already have
    /// been accepted and have set the request; wiping it here lost the pre-roll and led to
    /// underruns and a forced fallback. The request persists until a sink consumes it in
    /// `fill`, so it also survives a failed rebind (no sink, `fill` not called) and is
    /// consumed by the next stream that opens. Consumed, it starts one non-stacking pre-roll
    /// that is added to the headroom (at most `MAX_EXTRA_HEADROOM_FRAMES` with the jitter
    /// part) and released once the model has warmed up ([`Headroom::release`]); an open
    /// resets the headroom here.
    fn after_open(&mut self) {
        self.discard_all();
        if let Some(s) = &self.sink {
            eprintln!(
                "{}",
                render_open_line(
                    self.ctx.shared.run_id,
                    s.describe(),
                    s.period_frames(),
                    s.buffer_frames()
                )
            );
            // Preallocated here, never on the per-block path.
            self.staging = vec![0.0; s.buffer_frames() * 2];
        }
        self.fade_in.start(0);
        self.trim.reset();
        self.drop_pending = 0;
        self.primed = false;
        self.fed = false;
        self.underruns.reset();
        self.overload.reset();
        // A new stream starts without learned headroom (which includes any earlier pre-roll),
        // without a pre-roll in progress and without a pending decay; a pending request is
        // kept (see above).
        self.headroom.reset();
        self.end_decay();
        self.model_seen = false;
        self.settle_at = None;
        self.settling = false;
        // No deadline from the old stream may fire into the new one, and a new stream gets
        // deadline mode back with no misses remembered.
        self.deadline_armed = false;
        self.legacy_checks = false;
        self.missed.reset();
        if let Some(t) = &self.timer {
            t.cancel();
        }
        self.end_drop();
        self.recent_ms = 0.0;
        self.ctx.stats.headroom_frames.store(0, Ordering::Relaxed);
        self.preroll_left = 0;
        self.preroll_fade = false;
        self.last_frame = [0.0; 2];
        self.last_gain = 0.0;
    }

    /// The skip in `drop_pending` is done (or cancelled).
    fn end_drop(&mut self) {
        self.drop_is_decay = false;
        self.drop_faded = false;
    }

    /// Ruling 21 (b): devocal off (or a fallback) needs nothing it left queued. The model
    /// delivers each capture packet's hops later than the passthrough does, so the queue
    /// the checks see during devocal is lower; audio a decay could not call spare then (a
    /// pre-roll, say) shows up above the target once the passthrough is back. When the
    /// stage stops running the model, the position after everything in ring B plus the
    /// block that may still be in flight (its stage is published before it is pushed) marks
    /// the end of the crossfade; once ring B has been read past it, a settle decay starts and
    /// skips, through the decay path, what the checks then find above the target if that is
    /// at least a hop (any spare if released headroom was still pending; its observation
    /// starts again), up to the trim margin (beyond it the trim takes the excess anyway).
    /// In that joined case the hop threshold does not apply (`decay_min` stays 0): the
    /// released headroom would have been skipped whole by itself, and the passthrough's lows
    /// bound the skip as always.
    ///
    /// Back on cancels a settle not yet started, and ends one already started (ruling 23):
    /// its observation saw the passthrough's lows, and the model needs more queued (the
    /// pre-roll may add little or nothing to a deep queue), so what it would skip stays
    /// queued for this devocal session; a skip already deciding stops (its fade-out and the
    /// fade-in after it are as for any skip that ends).
    fn settle_after_devocal(&mut self, stage: Stage, avail: usize, hop: usize, now: u64) {
        let runs_model = stage_runs_model(stage);
        if runs_model {
            self.settle_at = None;
            if !self.model_seen {
                if self.settling {
                    self.end_decay();
                    if self.drop_is_decay && self.drop_pending > 0 {
                        self.drop_pending = 0;
                        self.end_drop();
                    }
                }
                self.settling = false;
            }
        } else if self.model_seen {
            self.settle_at = Some(self.read_pos + (avail + hop) as u64);
        }
        self.model_seen = runs_model;
        if self.settle_at.is_some_and(|at| self.read_pos >= at) {
            self.settle_at = None;
            // A released headroom not yet decided joins in, judged by the passthrough's lows
            // like the rest: the observation starts again.
            if self.decay_drop == 0 {
                self.decay_min = hop.max(1);
            }
            self.decay_since_us = now;
            self.decay_low = usize::MAX;
            self.decay_drop = self.decay_drop.max(frames_for_ms(TRIM_MARGIN_MS));
            self.settling = true;
        }
    }

    /// No decay pending any more (done, cancelled, or a new stream).
    fn end_decay(&mut self) {
        self.decay_drop = 0;
        self.decay_low = usize::MAX;
        self.decay_min = 0;
    }

    fn discard_all(&mut self) {
        let n = self.ctx.input.slots() / 2;
        if n > 0 {
            if let Ok(chunk) = self.ctx.input.read_chunk(n * 2) {
                chunk.commit_all();
            }
            self.read_pos += n as u64;
        }
        while let Ok(&m) = self.ctx.markers.peek() {
            if m > self.read_pos {
                break;
            }
            let _ = self.ctx.markers.pop();
        }
        self.gaps.reset();
    }

    /// Reads `n` frames from ring B into `staging[at..]`, with the fade-in and gap fades.
    fn take(&mut self, at: usize, n: usize) {
        if n == 0 {
            return;
        }
        let dst = &mut self.staging[at * 2..(at + n) * 2];
        if let Ok(chunk) = self.ctx.input.read_chunk(n * 2) {
            let (a, b) = chunk.as_slices();
            dst[..a.len()].copy_from_slice(a);
            dst[a.len()..].copy_from_slice(b);
            chunk.commit_all();
        } else {
            dst.fill(0.0);
        }
        self.fade_in.apply(dst);
        self.gaps.apply(&mut self.ctx.markers, self.read_pos, dst);
        self.recent_ms = track_power(self.recent_ms, &self.staging[at * 2..(at + n) * 2]);
        self.read_pos += n as u64;
    }

    /// RMS of the next `n` frames in ring B, read without consuming them (0 if empty or if
    /// fewer than `n` frames are queued).
    fn peek_rms(&mut self, n: usize) -> f32 {
        match self.ctx.input.read_chunk(n * 2) {
            // Dropped without a commit: the slots stay in the ring.
            Ok(chunk) => {
                let (a, b) = chunk.as_slices();
                mean_square(a, b).sqrt() as f32
            }
            Err(_) => 0.0,
        }
    }

    fn skip(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if let Ok(chunk) = self.ctx.input.read_chunk(n * 2) {
            chunk.commit_all();
        }
        self.read_pos += n as u64;
    }

    /// One wake: waits (at most [`WAIT_MS`]) for the device event, ring B's data event and,
    /// in deadline mode, the deadline timer; then [`Self::fill_at`] the current time (which
    /// re-arms the timer on a device wake). A failed wait sleeps [`FAILED_POLL`] first, so an
    /// invalid handle cannot spin the thread. The latency estimate is published on device and
    /// timeout wakes only.
    fn fill(&mut self) -> Result<(), String> {
        let Some(sink) = self.sink.as_ref() else {
            return Ok(());
        };
        let period = sink.period_frames();
        self.deadline_mode = !self.legacy_checks && deadline_mode(self.timer.is_some(), period);
        let timer = self
            .timer
            .as_ref()
            .map_or(HANDLE::default(), HiResTimer::raw);
        let handles = [sink.event(), self.ctx.shared.render_wake.raw(), timer];
        let n = if self.deadline_mode { 3 } else { 2 };
        // SAFETY: the handles are owned by the sink, `Shared` and `self.timer`, all alive.
        let result = unsafe { WaitForMultipleObjects(&handles[..n], false, WAIT_MS) };
        if result == WAIT_FAILED {
            std::thread::sleep(FAILED_POLL);
        }
        let wake = wake_from_wait(result.0, self.deadline_mode);
        let padding = self.fill_at(wake, now_us())?;
        if matches!(wake, Wake::Device | Wake::Timeout) {
            self.publish(padding);
        }
        Ok(())
    }

    /// Fills the device at `now` (microseconds) for a `wake`; returns the device padding
    /// after the write.
    fn fill_at(&mut self, wake: Wake, now: u64) -> Result<usize, String> {
        let Some(sink) = self.sink.as_ref() else {
            return Ok(0);
        };
        let diag = &self.ctx.stats.diag;
        diag.render_wakes.fetch_add(1, Ordering::Relaxed);
        match wake {
            Wake::Data => diag.render_data_wakes.fetch_add(1, Ordering::Relaxed),
            Wake::Deadline => diag.render_deadline_wakes.fetch_add(1, Ordering::Relaxed),
            Wake::Timeout => diag.render_timeout_wakes.fetch_add(1, Ordering::Relaxed),
            Wake::Device => 0,
        };
        let period = sink.period_frames();
        // Ruling 8 as amended by ruling 17: the deadline belongs to the read after the device
        // wake that armed it. A device wake that finds it still armed had a read with no check
        // before it (the wake was handled late, so the next one cancelled the deadline, or the
        // timer was late). Such an isolated miss pads only if the device ran dry (below);
        // [`MISSED_DEADLINE_LIMIT`] misses within [`MISSED_DEADLINE_WINDOW_US`] switch the
        // stream to checking on device wakes until it is reopened. Every device wake in
        // deadline mode arms the next deadline.
        let mut deadline_mode = self.deadline_mode && !self.legacy_checks;
        let mut missed = false;
        match wake {
            Wake::Device if deadline_mode => {
                if self.deadline_armed {
                    diag.render_missed_deadlines.fetch_add(1, Ordering::Relaxed);
                    missed = true;
                    if self.missed.miss(now) {
                        diag.render_legacy_switches.fetch_add(1, Ordering::Relaxed);
                        self.legacy_checks = true;
                        self.deadline_mode = false;
                        deadline_mode = false;
                        self.deadline_armed = false;
                        if let Some(t) = &self.timer {
                            t.cancel();
                        }
                    }
                }
                if deadline_mode {
                    let delay = frames_us(period).saturating_sub(DEADLINE_GUARD_US);
                    if let Some(t) = &self.timer {
                        t.arm_in_us(delay);
                    }
                    self.deadline_armed = true;
                    self.deadline_due_us = now + delay;
                }
            }
            Wake::Deadline => self.deadline_armed = false,
            _ => {}
        }
        let padding = sink.padding()?;
        let buffer = sink.buffer_frames().min(self.staging.len() / 2);
        let hop = self.ctx.shared.proc_hop.load(Ordering::Relaxed) as usize;
        let last_input = self.ctx.shared.last_input_us.load(Ordering::Acquire);
        if let Some(seen) = self.underruns.poll(last_input, now) {
            let verdict = self.overload.judge(
                seen,
                hop,
                self.headroom.jitter_at_cap(MAX_EXTRA_HEADROOM_FRAMES),
                now,
            );
            let seen = Starvation {
                force: verdict.forces(),
                ..seen
            };
            self.starve_detail.verdict = verdict.code();
            self.ctx.shared.fallback_log.set_detail(self.starve_detail);
            // Snapshot first, then the count (the processing thread reads them in that order).
            self.ctx
                .shared
                .underrun_snapshot
                .store(pack_starvation(seen), Ordering::Release);
            self.ctx.stats.underruns.fetch_add(1, Ordering::AcqRel);
            // Jitter, or a hiccup judged as jitter (ruling 24), grows the headroom instead of
            // a fallback; any counted underrun restarts the quiet period before it decays.
            self.headroom
                .on_underrun(!verdict.forces(), hop, MAX_EXTRA_HEADROOM_FRAMES, now);
        }
        let mut avail = self.ctx.input.slots() / 2;
        if missed && deadline_mode && padding == 0 && avail == 0 {
            // The unchecked read left the device dry with nothing to write: a starvation,
            // checked (padded and judged) as before O2. With audio still queued the read was
            // fed after all, and the deadline armed above checks the next one.
            deadline_mode = false;
        }
        // Ruling 24 diag: how late a deadline wake came, and the margin each check found.
        let check_late = if wake == Wake::Deadline {
            now.saturating_sub(self.deadline_due_us)
        } else {
            0
        };
        let d = &self.ctx.stats.diag;
        d.deadline_late_max_us
            .fetch_max(check_late, Ordering::Relaxed);
        if checks_padding(wake, deadline_mode) {
            d.note_check_margin((padding + avail) as i64 - period as i64);
        }
        let (padding_at_wake, avail_at_wake) = (padding, avail);
        // Pre-roll request from an accepted "on". It stays set until a fill with a sink
        // consumes it here (also across a failed rebind). Consuming it may start no pre-roll
        // (`preroll_frames` returns 0 while one is in progress, when the queue is already
        // deep, or at the cap). A started pre-roll is added to the headroom, so the target
        // includes it until `Headroom::release` lets it go after the model's warm-up.
        if self
            .ctx
            .shared
            .preroll_request
            .swap(false, Ordering::AcqRel)
        {
            let extra = self.headroom.total();
            let base = (period + hop + extra).min(buffer);
            let n = preroll_frames(
                self.preroll_left > 0,
                avail + padding,
                base,
                hop,
                extra,
                MAX_EXTRA_HEADROOM_FRAMES,
            );
            if n > 0 {
                self.preroll_left = n;
                self.preroll_fade = true;
                self.headroom.add_preroll(n, now);
                if self.drop_faded {
                    // A decay still discarding: the output already faded to silence, and the
                    // pre-roll writes ring B's frames after its silence, so the skip ends
                    // here (it would otherwise cut later frames without a fade).
                    self.drop_pending = 0;
                    self.end_drop();
                    self.preroll_fade = false;
                }
            }
        }
        // O1: headroom no longer needed leaves the target now; the audio it holds is skipped
        // below at a quiet spot.
        let stage = stage_from_code(self.ctx.stats.stage.load(Ordering::Acquire));
        let freed = self.headroom.release(stage, now);
        if freed > 0 {
            if self.decay_drop == 0 {
                self.decay_since_us = now;
                self.decay_low = usize::MAX;
                self.settling = false;
            }
            self.decay_drop += freed;
            self.decay_min = 0;
        }
        self.settle_after_devocal(stage, avail, hop, now);
        if self.decay_drop > 0 && checks_padding(wake, deadline_mode) {
            // At every check, written to or not: the low point is what is really spare. Only
            // a check sees the queue the next device read will find; in deadline mode a
            // device wake sees it right after a read (one period lower) and a data wake at a
            // burst peak (ruling 17).
            self.decay_low = self.decay_low.min(padding + avail);
        }
        self.ctx
            .stats
            .headroom_frames
            .store(self.headroom.total() as u32, Ordering::Relaxed);
        let target = (period + hop + self.headroom.total()).min(buffer);
        self.trim.set_target(target);

        // Trim watches the queue right after a device read (and at a timeout); a data wake
        // sees ring B at the peak of a burst.
        if matches!(wake, Wake::Device | Wake::Timeout) {
            if let Some(d) = self.trim.observe(avail + padding, now) {
                // Not over a decay still discarding (the trim's second restarts).
                if self.drop_pending == 0 {
                    self.drop_pending = d;
                    self.drop_is_decay = false;
                    // A pending decay must not skip the same excess again (ruling 23): its
                    // observation starts over after the trim.
                    self.decay_low = usize::MAX;
                    self.decay_since_us = now;
                }
            }
        }
        let room = buffer.saturating_sub(padding);
        let need = target.saturating_sub(padding).min(room);
        let check = checks_padding(wake, deadline_mode);
        if need == 0 {
            // A check with the device already at the target finds no shortfall.
            if check && self.fed {
                self.primed = true;
            }
            return Ok(padding);
        }

        if self.preroll_left > 0 {
            // Pre-roll: fade out, then exactly the silence registered as headroom (ruling 17:
            // every frame of it delays the audio, and the decay removes only what the headroom
            // holds), then ring B's frames up to the target, fading in. Only a check that would
            // otherwise leave the device short of a period writes more silence: that much would
            // have been padded anyway.
            let mut w = 0;
            if self.preroll_fade {
                let k = self.fade_frames.min(need).min(avail);
                self.take(0, k);
                fade_edges(&mut self.staging[..k * 2], false, true, k);
                avail -= k;
                self.preroll_fade = false;
                w = k;
            }
            let short = if check {
                period.saturating_sub(padding + w + avail)
            } else {
                0
            };
            let s = (need - w).min(self.preroll_left.max(short));
            self.staging[w * 2..(w + s) * 2].fill(0.0);
            if w == 0 {
                let f = self.fade_frames.min(s);
                decay_from(self.last_frame, &mut self.staging[..f * 2]);
            }
            self.ctx
                .stats
                .diag
                .render_preroll_frames
                .fetch_add(s as u64, Ordering::Relaxed);
            self.preroll_left = self.preroll_left.saturating_sub(s);
            w += s;
            if self.preroll_left == 0 {
                self.fade_in.start(0);
                let real = (need - w).min(avail);
                self.take(w, real);
                self.ctx
                    .stats
                    .diag
                    .render_real_frames
                    .fetch_add(real as u64, Ordering::Relaxed);
                self.fed |= real > 0;
                w += real;
            }
            return self.write_out(w, padding);
        }

        let waited = now.saturating_sub(self.decay_since_us);
        // Never in the middle of a crossfade between passthrough and accompaniment, nor
        // before the crossfade back to the passthrough has left ring B (ruling 21 b).
        let crossfading =
            matches!(stage, Stage::FadingIn | Stage::FadingOut) || self.settle_at.is_some();
        if self.decay_drop > 0
            && self.drop_pending == 0
            && waited >= DECAY_OBSERVE_US
            && !crossfading
        {
            // O1: what is really spare is the lowest queue since the release, beyond the
            // target. None (player paused, or a late packet ate it): nothing to skip. No check
            // wake seen yet (say every deadline was missed): unknown, so wait for one, giving
            // up a second past the forced point.
            if self.decay_low == usize::MAX {
                if waited >= 2 * LOW_ENERGY_WAIT_US {
                    self.end_decay();
                }
            } else if self.decay_low < target + self.decay_min.max(1) {
                // Nothing spare (or, for a settle, less than a hop: O2's own spread).
                self.end_decay();
            } else {
                let spare = self.decay_low - target;
                let window =
                    self.peek_rms((self.fade_frames + self.decay_drop.min(spare)).min(avail));
                let recent = self.recent_ms.sqrt() as f32;
                if decay_now(window, recent, waited) {
                    if avail > self.fade_frames {
                        // The skip need not be in ring B yet: frames go on being discarded
                        // as they arrive until it is done (ruling 17; under O2 ring B rarely
                        // holds a fade plus the whole release). Safe as the low point is: the
                        // next check still finds the target queued.
                        self.drop_pending = self.decay_drop.min(spare);
                        self.drop_is_decay = true;
                        self.end_decay();
                    } else if waited >= 2 * LOW_ENERGY_WAIT_US {
                        // Not one fade's worth queued on any wake for a second past the
                        // forced point: what is left stays.
                        self.end_decay();
                    }
                    // Otherwise too little in ring B for the fade-out now: retry next wake.
                }
            }
        }

        let mut w = 0;
        if self.drop_pending > 0 && avail > 0 {
            // Trim: fade out what plays next, skip the excess, fade the rest in. A decay
            // keeps its full fade even beyond `need` (frames move from ring B to the device;
            // the queue is the same), so small periods do not shorten it. A trim skips what
            // ring B holds; a decay goes on discarding arriving frames (nothing real is
            // written meanwhile) until its whole skip is done.
            if !self.drop_faded {
                let fade_room = if self.drop_is_decay { room } else { need };
                let k = self.fade_frames.min(fade_room).min(avail);
                self.take(0, k);
                fade_edges(&mut self.staging[..k * 2], false, true, k);
                avail -= k;
                w = k;
                self.drop_faded = true;
            }
            let d = self.drop_pending.min(avail);
            let diag = &self.ctx.stats.diag;
            let counter = if self.drop_is_decay {
                &diag.render_decay_frames
            } else {
                &diag.render_trim_frames
            };
            counter.fetch_add(d as u64, Ordering::Relaxed);
            self.skip(d);
            avail -= d;
            self.drop_pending = if self.drop_is_decay {
                self.drop_pending - d
            } else {
                0
            };
            if self.drop_pending == 0 {
                self.end_drop();
            }
            self.fade_in.start(0);
        }
        // The frames already staged (a trim or decay fade-out) count as real. A decay fade
        // can exceed the plan (it is bounded by `room`); then nothing more is taken and, the
        // queue being at the target, nothing is padded.
        let plan = plan_fill(
            wake,
            deadline_mode,
            padding,
            w + avail,
            room,
            period,
            target,
        );
        let real = plan.real.saturating_sub(w);
        self.take(w, real);
        w += real;
        let diag = &self.ctx.stats.diag;
        diag.render_real_frames
            .fetch_add(real as u64, Ordering::Relaxed);

        let silence = plan.silence.min(room.saturating_sub(w));
        if silence > 0 {
            diag.render_pad_events.fetch_add(1, Ordering::Relaxed);
            diag.render_pad_frames
                .fetch_add(silence as u64, Ordering::Relaxed);
            // A check found the device would starve at its next read: pad with silence up to
            // the target (ruling 18), not just one period.
            let k = w.min(self.fade_frames);
            fade_edges(&mut self.staging[(w - k) * 2..w * 2], false, true, k);
            self.staging[w * 2..(w + silence) * 2].fill(0.0);
            if w == 0 {
                let f = self.fade_frames.min(silence);
                decay_from(self.last_frame, &mut self.staging[..f * 2]);
            }
            // The queue was not spare after all: a skip still discarding stops here.
            self.drop_pending = 0;
            self.end_drop();
            if self.primed {
                let stage = stage_from_code(self.ctx.stats.stage.load(Ordering::Acquire));
                let sh = &self.ctx.shared;
                let seen = Starvation {
                    ran_model: stage_runs_model(stage),
                    backlog_frames: sh.in_ring_frames.load(Ordering::Acquire) as usize,
                    warming: stage == Stage::WarmingUp,
                    force: false,
                };
                if self.underruns.starved(last_input, now, seen) {
                    let since = |t: u64| if t == 0 { 0 } else { now.saturating_sub(t) };
                    self.starve_detail = StarvationDetail {
                        at_us: now,
                        wake: wake as u64,
                        padding: padding_at_wake as u64,
                        ring_b: avail_at_wake as u64,
                        shortfall: period.saturating_sub(padding + w) as u64,
                        since_capture_us: since(last_input),
                        since_push_us: since(sh.last_push_us.load(Ordering::Relaxed)),
                        block_busy_us: since(sh.block_start_us.load(Ordering::Relaxed)),
                        check_late_us: check_late,
                        headroom: self.headroom.total() as u64,
                        verdict: 0,
                    };
                }
            }
            self.primed = false;
            self.fed = false;
            self.fade_in.start(0);
            w += silence;
        } else {
            self.fed |= real > 0;
            if check && self.fed {
                self.primed = true;
            }
        }
        self.write_out(w, padding)
    }

    /// Output gain ramp, clamp, and write of `staging[..w]`; returns the new padding.
    fn write_out(&mut self, w: usize, padding: usize) -> Result<usize, String> {
        if w > 0 {
            self.last_frame = [self.staging[(w - 1) * 2], self.staging[(w - 1) * 2 + 1]];
        }
        let gain = self.ctx.gains.output_gain();
        finish_block(&mut self.staging[..w * 2], self.last_gain, gain);
        self.last_gain = gain;
        let Some(sink) = self.sink.as_mut() else {
            return Ok(padding);
        };
        sink.write(&self.staging[..w * 2])?;
        Ok(padding + w)
    }

    fn publish(&self, padding: usize) {
        let sh = &self.ctx.shared;
        let now = now_us();
        let last = sh.last_input_us.load(Ordering::Acquire);
        let since = if last == 0 { sh.start_us } else { last };
        self.ctx
            .stats
            .input_silent_ms
            .store(now.saturating_sub(since) / 1000, Ordering::Relaxed);
        let frames = latency_frames(
            u64::from(sh.capture_packet_frames.load(Ordering::Relaxed)),
            u64::from(sh.in_ring_frames.load(Ordering::Relaxed))
                + u64::from(sh.proc_in_flight_frames.load(Ordering::Relaxed)),
            (self.ctx.input.slots() / 2) as u64,
            padding as u64,
            u64::from(sh.proc_latency_frames.load(Ordering::Relaxed)),
            self.sink.as_ref().map_or(0, |s| s.period_frames() as u64),
        );
        let micros = frames * 1_000_000 / u64::from(SAMPLE_RATE);
        self.ctx
            .stats
            .latency_ms_milli
            .store(micros.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
    }
}

/// Part of the system path that does not scale with the render period, in microseconds.
///
/// Zero on purpose. It would be the process-loopback delivery offset `S` (38,547 us median in
/// the P0 report), but only if that offset were shown not to depend on the period, i.e.
/// |S - S_small| < 2000 us. `S_small` is N/A: no endpoint on the measuring machine offers a
/// smaller period (all six are 48 kHz, 480 frames), so the condition is not shown, `S` is not
/// used, and the whole system path is attributed to the period-scaled term below.
pub(crate) const SYSTEM_PATH_FIXED_US: u64 = 0;

/// Part of the system path that scales with the render period, in tenths of a period.
///
/// Source: `docs/2026-10-03_devocal-latency-p0-report.md` section 2 (Folia, P = 441 frames =
/// 10 ms, measured 2026-10-03). The estimate then still contained 3 periods for the system
/// path, so our own part is `E - 3 * P`: our_a = 55.8 - 30 = 25.8 ms, our_c = 65.8 - 30 =
/// 35.8 ms. Against the measured release jumps M_a = 58.0 ms and M_c = 68.0 ms the system
/// path is sys_a = 58.0 - 25.8 = 32.2 ms and sys_c = 68.0 - 35.8 = 32.2 ms (they agree), so
/// sys = 32.2 ms and tenths = round(10 * 32.2 / 10) = 32, i.e. 3.2 periods. Re-measure on a
/// machine with a smaller period before trusting the scaling with the period.
pub(crate) const SYSTEM_PATH_PERIOD_TENTHS: u64 = 32;

/// Frames the system adds on top of our own pipeline (the audio engine handing the player's
/// mix to process loopback, and our output being mixed again) for a render period of `period`
/// frames. `period` is 0 when there is no sink, and then so is the system path.
fn system_path_frames(period: u64) -> u64 {
    if period == 0 {
        return 0;
    }
    SYSTEM_PATH_FIXED_US * u64::from(SAMPLE_RATE) / 1_000_000
        + SYSTEM_PATH_PERIOD_TENTHS * period / 10
}

/// Estimated end-to-end added latency in frames: capture packet + ring A (with the block being
/// processed) + ring B + device padding + processor latency + [`system_path_frames`] (zero when
/// `period` is 0, i.e. there is no sink).
fn latency_frames(
    capture_packet: u64,
    ring_a: u64,
    ring_b: u64,
    padding: u64,
    proc_latency: u64,
    period: u64,
) -> u64 {
    capture_packet + ring_a + ring_b + padding + proc_latency + system_path_frames(period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::{Producer, RingBuffer};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// A 441-frame-period, 4 410-frame-buffer device. The test can drain `padding` (device
    /// playback) and read every written sample in `out`.
    struct FakeSink {
        padding: Arc<AtomicUsize>,
        out: Arc<Mutex<Vec<f32>>>,
        /// Stands in for the device event (a plain kernel event, no audio device).
        event: Arc<OwnedEvent>,
    }

    impl FakeSink {
        fn new(padding: usize) -> Self {
            Self {
                padding: Arc::new(AtomicUsize::new(padding)),
                out: Arc::new(Mutex::new(Vec::new())),
                event: Arc::new(OwnedEvent::new().unwrap()),
            }
        }
    }

    impl Sink for FakeSink {
        fn period_frames(&self) -> usize {
            441
        }
        fn buffer_frames(&self) -> usize {
            4_410
        }
        fn padding(&self) -> Result<usize, String> {
            Ok(self.padding.load(Ordering::Relaxed))
        }
        fn event(&self) -> HANDLE {
            self.event.raw()
        }
        fn write(&mut self, stereo: &[f32]) -> Result<(), String> {
            self.padding.fetch_add(stereo.len() / 2, Ordering::Relaxed);
            self.out.lock().unwrap().extend_from_slice(stereo);
            Ok(())
        }
        fn describe(&self) -> &'static str {
            "fake"
        }
    }

    /// A renderer on rings only (no devices); `sink` as given. Returns the shared state and
    /// ring B's producer.
    fn test_renderer(sink: Option<FakeSink>) -> (Renderer, Arc<Shared>, Producer<f32>) {
        use crate::audio::{AudioStats, OwnedEvent, SharedGains};
        use std::sync::atomic::{AtomicU32, AtomicU64};
        let (in_tx, in_rx) = RingBuffer::<f32>::new(8_192);
        let (_mk_tx, mk_rx) = RingBuffer::<u64>::new(8);
        let (_ctl_tx, ctl_rx) = RingBuffer::<RenderCommand>::new(8);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            start_us: now_us(),
            run_id: 1,
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
            fallback_log: crate::audio::FallbackLog::default(),
            wake: OwnedEvent::new().unwrap(),
            render_wake: OwnedEvent::new().unwrap(),
        });
        let mut r = Renderer::new(RenderCtx {
            endpoint: None,
            input: in_rx,
            markers: mk_rx,
            control: ctl_rx,
            shared: shared.clone(),
            stats: Arc::new(AudioStats::default()),
            gains: Arc::new(SharedGains::new(1.0, 1.0)),
        });
        if let Some(s) = sink {
            r.sink = Some(Box::new(s));
        }
        (r, shared, in_tx)
    }

    #[test]
    fn a_display_name_failure_is_logged_once_and_never_fatal() {
        let logged = AtomicBool::new(false);
        assert!(!note_display_name(Ok(()), &logged));
        assert!(note_display_name(Err("E_FAIL".into()), &logged));
        assert!(
            !note_display_name(Err("E_FAIL".into()), &logged),
            "logged once"
        );
        assert!(!note_display_name(Ok(()), &logged));
    }

    /// Ruling 23: the first "on" can be accepted (request set) between the render thread's
    /// `ready` and its first `after_open`; opening must not wipe the request.
    #[test]
    fn after_open_keeps_a_pending_preroll_request() {
        let (mut r, shared, _tx) = test_renderer(None);
        // Per-stream state left by an earlier stream: headroom, a pre-roll in progress and a
        // pending decay.
        r.headroom
            .on_underrun(true, 300, MAX_EXTRA_HEADROOM_FRAMES, 1);
        r.headroom.add_preroll(MAX_EXTRA_HEADROOM_FRAMES - 300, 2);
        assert_eq!(r.headroom.total(), MAX_EXTRA_HEADROOM_FRAMES);
        r.decay_drop = 128;
        r.preroll_left = PREROLL_FRAMES;
        r.preroll_fade = true;
        r.ctx
            .stats
            .headroom_frames
            .store(MAX_EXTRA_HEADROOM_FRAMES as u32, Ordering::Relaxed);
        shared.preroll_request.store(true, Ordering::Release);
        r.after_open();
        assert!(shared.preroll_request.load(Ordering::Acquire));
        // Still resets the per-stream state.
        assert_eq!(
            (
                r.headroom.total(),
                r.decay_drop,
                r.preroll_left,
                r.preroll_fade
            ),
            (0, 0, 0, false)
        );
        assert_eq!(r.ctx.stats.headroom_frames.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_request_kept_through_after_open_is_consumed_by_the_next_fill() {
        let (mut r, shared, _tx) = test_renderer(Some(FakeSink::new(0)));
        shared.preroll_request.store(true, Ordering::Release);
        r.after_open();
        assert!(r.fill().is_ok());
        assert!(!shared.preroll_request.load(Ordering::Acquire), "consumed");
        // A full capture packet of pre-roll, counted inside the headroom.
        assert_eq!(r.headroom.total(), PREROLL_FRAMES);
        assert_eq!(
            r.ctx.stats.headroom_frames.load(Ordering::Relaxed),
            PREROLL_FRAMES as u32
        );
    }

    const MS: u64 = 1_000;
    const SEC: u64 = 1_000_000;

    #[test]
    fn preroll_headroom_lasts_only_while_the_model_warms() {
        let t = 10 * SEC;
        let mut h = Headroom::new();
        h.add_preroll(441, t);
        assert_eq!(h.total(), 441);
        assert_eq!(h.release(Stage::WarmingUp, t + MS), 0);
        assert_eq!(h.release(Stage::FadingIn, t + 100 * MS), 0);
        assert_eq!(h.release(Stage::Devocal, t + 300 * MS), 441);
        assert_eq!(h.total(), 0);
    }

    /// Review Focus 4: the request can reach the renderer before the processor publishes
    /// `WarmingUp`; the pre-roll must survive that.
    #[test]
    fn preroll_kept_until_model_stage_seen_or_grace() {
        let t = 10 * SEC;
        let mut h = Headroom::new();
        h.add_preroll(441, t);
        assert_eq!(h.release(Stage::Passthrough, t + MS), 0);
        assert_eq!(h.release(Stage::Passthrough, t + 199 * MS), 0);
        assert_eq!(h.release(Stage::Passthrough, t + 200 * MS), 441);
        assert_eq!(h.total(), 0);
    }

    #[test]
    fn jitter_headroom_decays_after_5s_without_underrun() {
        let t = 10 * SEC;
        let cap = MAX_EXTRA_HEADROOM_FRAMES;
        let mut h = Headroom::new();
        h.on_underrun(true, 128, cap, t);
        h.on_underrun(true, 128, cap, t + MS);
        assert_eq!(h.total(), 256);
        assert_eq!(h.release(Stage::Passthrough, t + MS + 4_999 * MS), 0);
        assert_eq!(h.release(Stage::Devocal, t + MS + 5 * SEC), 256);
        assert_eq!(h.total(), 0);
    }

    #[test]
    fn any_underrun_restarts_the_quiet_timer() {
        let t = 10 * SEC;
        let cap = MAX_EXTRA_HEADROOM_FRAMES;
        let mut h = Headroom::new();
        h.on_underrun(true, 128, cap, t);
        // A backlogged (not jitter) underrun: no growth, but the output was not quiet.
        h.on_underrun(false, 128, cap, t + 3 * SEC);
        assert_eq!(h.total(), 128);
        assert_eq!(h.release(Stage::Devocal, t + 5 * SEC), 0);
        assert_eq!(h.release(Stage::Devocal, t + 8 * SEC), 128);
    }

    #[test]
    fn jitter_regrows_after_decay_up_to_the_cap() {
        let t = 10 * SEC;
        let cap = MAX_EXTRA_HEADROOM_FRAMES;
        let mut h = Headroom::new();
        h.on_underrun(true, 128, cap, t);
        assert_eq!(h.release(Stage::Devocal, t + 5 * SEC), 128);
        let mut steps = Vec::new();
        for i in 1..=4 {
            h.on_underrun(true, 128, cap, t + 5 * SEC + i * MS);
            steps.push(h.total());
        }
        assert_eq!(steps, vec![128, 256, 384, 441]);
        // A pre-roll and the jitter share the cap.
        let mut h = Headroom::new();
        h.add_preroll(300, t);
        h.on_underrun(true, 128, cap, t + MS);
        h.on_underrun(true, 128, cap, t + 2 * MS);
        assert_eq!(h.total(), cap);
    }

    /// Review Focus 5: jitter that keeps coming back must not cost a skip every 5 s.
    #[test]
    fn decay_backs_off_when_jitter_returns() {
        let cap = MAX_EXTRA_HEADROOM_FRAMES;
        let mut h = Headroom::new();
        let t0 = 10 * SEC;
        h.on_underrun(true, 128, cap, t0);
        let t = t0 + 5 * SEC;
        assert_eq!(h.release(Stage::Devocal, t), 128, "first decay after 5 s");
        // Jitter again 2 s after the decay: 10 s of quiet needed.
        h.on_underrun(true, 128, cap, t + 2 * SEC);
        assert_eq!(h.release(Stage::Devocal, t + 2 * SEC + 9_999 * MS), 0);
        assert_eq!(h.release(Stage::Devocal, t + 12 * SEC), 128);
        // Again 1 s after that decay: 20 s.
        h.on_underrun(true, 128, cap, t + 13 * SEC);
        assert_eq!(h.release(Stage::Devocal, t + 13 * SEC + 19_999 * MS), 0);
        assert_eq!(h.release(Stage::Devocal, t + 33 * SEC), 128);
        // 40 s, then never more than 60 s however often it returns.
        h.on_underrun(true, 128, cap, t + 34 * SEC);
        assert_eq!(h.release(Stage::Devocal, t + 34 * SEC + 39_999 * MS), 0);
        assert_eq!(h.release(Stage::Devocal, t + 74 * SEC), 128);
        let mut last = t + 74 * SEC;
        for _ in 0..3 {
            h.on_underrun(true, 128, cap, last + SEC);
            assert_eq!(h.release(Stage::Devocal, last + SEC + 59_999 * MS), 0);
            assert_eq!(h.release(Stage::Devocal, last + 61 * SEC), 128);
            last += 61 * SEC;
        }
        // An underrun 60 s or more after the last decay starts over at 5 s.
        let u = last + HEADROOM_DECAY_MAX_US;
        h.on_underrun(true, 128, cap, u);
        assert_eq!(h.release(Stage::Devocal, u + 4_999 * MS), 0);
        assert_eq!(h.release(Stage::Devocal, u + 5 * SEC), 128);
    }

    #[test]
    fn decay_waits_for_a_quiet_spot_at_most_1s() {
        assert!(decay_now(0.4, 1.0, 0));
        assert!(decay_now(0.5, 1.0, 0), "-6 dB is quiet enough");
        assert!(!decay_now(0.6, 1.0, 999_999));
        assert!(decay_now(0.6, 1.0, 1_000_000));
        assert!(decay_now(0.0, 0.0, 0), "silence");
    }

    #[test]
    fn peeking_ring_b_does_not_consume() {
        let (mut r, _shared, mut tx) = test_renderer(None);
        for i in 0..20 {
            tx.push(if i % 2 == 0 { 0.6 } else { 0.8 }).unwrap();
        }
        let before = r.ctx.input.slots();
        // Plain rtrb: a chunk dropped without a commit leaves the slots in place.
        {
            let chunk = r.ctx.input.read_chunk(12).unwrap();
            assert_eq!(chunk.len(), 12);
        }
        assert_eq!(r.ctx.input.slots(), before);
        // RMS of (0.6, 0.8) frames: sqrt((0.36 + 0.64) / 2).
        let rms = r.peek_rms(6);
        assert!((rms - 0.5f32.sqrt()).abs() < 1e-6, "{rms}");
        assert_eq!(r.ctx.input.slots(), before);
        assert_eq!(r.peek_rms(0), 0.0);
    }

    /// Runs `ms` ms from `t0` in 1 ms steps. Each step `feed(i)` gives `(frames, value)`:
    /// that many frames of `value` arrive in ring B (marked as fresh input, so underruns are
    /// judged); then the device plays one 441-frame period every 10 ms; then `fill_at` runs
    /// as a device wake (the renderer is not in deadline mode, so it pads as before), every
    /// step or, with `period_wakes`, only on the device period (as the real event wakes it).
    /// Calls `each(r, now, first)` after every fill, `first` being the index in `out` of the
    /// first sample that fill wrote.
    #[allow(clippy::too_many_arguments)]
    fn simulate(
        r: &mut Renderer,
        tx: &mut Producer<f32>,
        padding: &AtomicUsize,
        out: &Mutex<Vec<f32>>,
        t0: u64,
        ms: u64,
        period_wakes: bool,
        mut feed: impl FnMut(u64) -> (u64, f32),
        mut each: impl FnMut(&mut Renderer, u64, usize),
    ) {
        for i in 0..ms {
            let now = t0 + i * MS;
            let (frames, value) = feed(i);
            if frames > 0 {
                for _ in 0..frames * 2 {
                    tx.push(value).unwrap();
                }
                r.ctx.shared.last_input_us.store(now, Ordering::Release);
            }
            if i > 0 && i % 10 == 0 {
                let p = padding.load(Ordering::Relaxed);
                padding.store(p.saturating_sub(441), Ordering::Relaxed);
            }
            if period_wakes && i % 10 != 0 {
                continue;
            }
            let first = out.lock().unwrap().len();
            r.fill_at(Wake::Device, now).unwrap();
            each(r, now, first);
        }
    }

    /// 441 frames per 10 ms, spread over every ms.
    fn smooth(i: u64) -> u64 {
        (i + 1) * 441 / 10 - i * 441 / 10
    }

    fn underruns(r: &Renderer) -> u64 {
        r.ctx.stats.underruns.load(Ordering::Relaxed)
    }

    fn pad_events(r: &Renderer) -> u64 {
        r.ctx.stats.diag.render_pad_events.load(Ordering::Relaxed)
    }

    fn decay_frames(r: &Renderer) -> u64 {
        r.ctx.stats.diag.render_decay_frames.load(Ordering::Relaxed)
    }

    #[test]
    fn decay_trims_the_released_headroom_with_fades() {
        let sink = FakeSink::new(0);
        let (padding, out) = (sink.padding.clone(), sink.out.clone());
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        let t0 = 10 * SEC;
        let cap = MAX_EXTRA_HEADROOM_FRAMES;
        r.headroom.on_underrun(true, cap, cap, t0);
        assert_eq!(r.headroom.total(), 441);
        let fade = r.fade_frames;
        let mut decayed_at = None;
        let mut skip_at = 0;
        simulate(
            &mut r,
            &mut tx,
            &padding,
            &out,
            t0,
            6_500,
            false,
            |i| (smooth(i), 0.5),
            |r, now, first| {
                let d = decay_frames(r);
                if now < t0 + 5 * SEC {
                    assert_eq!(d, 0, "no decay before 5 s ({now})");
                }
                if d > 0 && decayed_at.is_none() {
                    decayed_at = Some(now);
                    // Fade out into the skip: the last frame before it is silent.
                    let o = out.lock().unwrap();
                    let block = &o[first..];
                    assert!(block.len() / 2 >= fade, "{}", block.len());
                    assert!(block[0] > 0.4, "starts at full level: {}", block[0]);
                    assert_eq!(block[(fade - 1) * 2], 0.0);
                    assert_eq!(block[(fade - 1) * 2 + 1], 0.0);
                    skip_at = first / 2 + fade;
                }
            },
        );
        let at = decayed_at.expect("the released headroom was trimmed");
        assert!(
            at.abs_diff(t0 + 6 * SEC) <= 20 * MS,
            "forced after a 1 s wait: {} ms",
            (at - t0) / MS
        );
        let d = decay_frames(&r);
        assert!(d > 0 && d <= 441, "{d}");
        // After the skip the audio fades back in from silence to full level.
        let o = out.lock().unwrap();
        let after: Vec<f32> = o[skip_at * 2..(skip_at + 2 * fade) * 2]
            .iter()
            .step_by(2)
            .copied()
            .collect();
        assert!(after[0] < 0.01, "starts near zero: {}", after[0]);
        assert!(
            after.windows(2).all(|p| p[1] >= p[0]),
            "rises monotonically"
        );
        assert!(
            after[fade / 2] > 0.1 && after[fade / 2] < 0.4,
            "{}",
            after[fade / 2]
        );
        assert_eq!(after[2 * fade - 1], 0.5, "back to full level");
        drop(o);
        assert_eq!(underruns(&r), 0);
        assert_eq!(
            r.ctx.stats.diag.render_trim_frames.load(Ordering::Relaxed),
            0,
            "not counted as a trim"
        );
        assert_eq!(r.headroom.total(), 0);
        assert_eq!(r.ctx.stats.headroom_frames.load(Ordering::Relaxed), 0);
    }

    /// Review Focus 5: with the player paused nothing is queued beyond the target, so the
    /// released headroom costs no content.
    #[test]
    fn decay_during_pause_drops_nothing() {
        let sink = FakeSink::new(0);
        let (padding, out) = (sink.padding.clone(), sink.out.clone());
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        let t0 = 10 * SEC;
        let cap = MAX_EXTRA_HEADROOM_FRAMES;
        r.headroom.on_underrun(true, cap, cap, t0);
        simulate(
            &mut r,
            &mut tx,
            &padding,
            &out,
            t0,
            6_000,
            false,
            |i| (if i < 100 { smooth(i) } else { 0 }, 0.5),
            |_, _, _| {},
        );
        assert_eq!(decay_frames(&r), 0);
        assert_eq!(r.headroom.total(), 0);
        assert_eq!(r.decay_drop, 0, "cancelled");
        assert_eq!(r.ctx.stats.headroom_frames.load(Ordering::Relaxed), 0);
    }

    /// Fix round 1: ring B arrives in 441-frame packets, so one wake can overstate the spare
    /// queue by a packet; the decay uses the low point over at least `DECAY_OBSERVE_US` and
    /// must never cause an underrun or a pad. Packets land `phase` ms after each device
    /// period; one of them (due at `due` ms) arrives `late` ms late.
    #[test]
    fn decay_with_bursty_input_never_starves() {
        struct Case {
            phase: u64,
            due: u64,
            late: u64,
            period_wakes: bool,
            /// The audio turns quiet (0.1 after 0.5) just before the release, so the decay
            /// may go at its first chance instead of after the forced 1 s.
            quiet: bool,
            decays: bool,
        }
        let cases = [
            // 3 + 5 = 8 ms: late but before the next period; the spare holds, the forced
            // decay goes ahead.
            Case {
                phase: 3,
                due: 5_503,
                late: 5,
                period_wakes: false,
                quiet: false,
                decays: true,
            },
            // 7 + 5 = 12 ms: misses a period, the queued excess covers it, the low point
            // reaches the target and the decay is cancelled.
            Case {
                phase: 7,
                due: 5_507,
                late: 5,
                period_wakes: false,
                quiet: false,
                decays: false,
            },
            // Device-period wakes, a quiet spot at the release, and the first packet after it
            // misses a period: a decision on the first wake (the old single-wake rule) skips
            // 441 frames and then underruns; watching the low point for 30 ms cancels.
            Case {
                phase: 3,
                due: 5_013,
                late: 9,
                period_wakes: true,
                quiet: true,
                decays: false,
            },
        ];
        for c in cases {
            let sink = FakeSink::new(0);
            let (padding, out) = (sink.padding.clone(), sink.out.clone());
            let (mut r, _shared, mut tx) = test_renderer(Some(sink));
            r.after_open();
            let t0 = 10 * SEC;
            let cap = MAX_EXTRA_HEADROOM_FRAMES;
            r.headroom.on_underrun(true, cap, cap, t0);
            let feed = |i: u64| {
                let on_time = i % 10 == c.phase && i != c.due;
                let frames = 441 * (u64::from(on_time) + u64::from(i == c.due + c.late));
                let value = if c.quiet && i >= 4_980 { 0.1 } else { 0.5 };
                (frames, value)
            };
            // The stream start pads once (not primed, never counted); judge from the
            // release at 5 s on.
            let mut at_release = None;
            simulate(
                &mut r,
                &mut tx,
                &padding,
                &out,
                t0,
                7_000,
                c.period_wakes,
                feed,
                |r, now, _| {
                    if now == t0 + 5 * SEC {
                        assert_eq!(r.headroom.total(), 0, "released");
                        at_release = Some(pad_events(r));
                    }
                },
            );
            let what = format!("phase {} late {} at {}", c.phase, c.late, c.due);
            let p0 = at_release.unwrap();
            assert_eq!(underruns(&r), 0, "{what}");
            assert_eq!(pad_events(&r), p0, "no pad across the decay ({what})");
            assert_eq!(decay_frames(&r) > 0, c.decays, "{what}");
            assert!(decay_frames(&r) <= 441);
            assert_eq!(r.decay_drop, 0, "settled ({what})");
        }
    }

    /// Fix round 1, minors 2 and 3: with too little in ring B for a full fade plus a skip the
    /// decay stays pending and retries (giving up a second past the forced point); when it
    /// goes, its fade-out keeps the full 5 ms even where the wake needs fewer frames.
    #[test]
    fn a_decay_without_room_to_fade_retries_then_gives_up() {
        let sink = FakeSink::new(0);
        let (padding, out) = (sink.padding.clone(), sink.out.clone());
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        let fade = r.fade_frames;
        let t = 100 * SEC;
        let push = |tx: &mut Producer<f32>, frames: usize| {
            for _ in 0..frames * 2 {
                tx.push(0.5).unwrap();
            }
        };
        // Target 441 + 128 = 569; device at 500 (needs 69); ring B 100 frames: queue 600,
        // 31 frames spare, but 100 <= one fade. Forced (waited 1.5 s), yet nothing to do.
        padding.store(500, Ordering::Relaxed);
        push(&mut tx, 100);
        r.decay_drop = 441;
        r.decay_since_us = t - 1_500 * MS;
        r.decay_low = 600;
        r.fill_at(Wake::Device, t).unwrap();
        assert_eq!(decay_frames(&r), 0);
        assert_eq!(r.decay_drop, 441, "kept pending");
        // Next wake: enough queued. The 31 spare frames go, behind a full 221-frame fade
        // although the wake needs only 69.
        padding.store(500, Ordering::Relaxed);
        push(&mut tx, 400);
        let first = out.lock().unwrap().len();
        r.fill_at(Wake::Device, t + 10 * MS).unwrap();
        assert_eq!(decay_frames(&r), 31);
        assert_eq!((r.decay_drop, r.decay_low), (0, usize::MAX));
        let o = out.lock().unwrap();
        assert_eq!((o.len() - first) / 2, fade, "full fade written");
        // (Still inside the stream-start fade-in, so not at full level yet.)
        assert!(o[first] > 0.0);
        assert_eq!(o[first + (fade - 1) * 2], 0.0);
        drop(o);
        // Still nothing to fade a second past the forced point: given up.
        padding.store(500, Ordering::Relaxed);
        r.decay_drop = 441;
        r.decay_since_us = t - 2_000 * MS;
        r.decay_low = 600;
        assert!(r.ctx.input.slots() / 2 <= fade);
        r.fill_at(Wake::Device, t + 20 * MS).unwrap();
        assert_eq!(decay_frames(&r), 31);
        assert_eq!(r.decay_drop, 0, "given up");
    }

    // ---- O2: write on arrival, pad only at the read deadline ----

    /// `plan_fill` with the brief's fixed device: period 441, target 569 (441 + 128, no
    /// headroom), room 4 000.
    fn plan(wake: Wake, deadline: bool, padding: usize, avail: usize) -> FillPlan {
        plan_fill(wake, deadline, padding, avail, 4_000, 441, 569)
    }

    #[test]
    fn data_wake_writes_real_frames_only() {
        assert_eq!(
            plan(Wake::Data, true, 100, 300),
            FillPlan {
                real: 300,
                silence: 0
            }
        );
    }

    #[test]
    fn deadline_wake_pads_up_to_target_when_short() {
        assert_eq!(
            plan(Wake::Deadline, true, 100, 200),
            FillPlan {
                real: 200,
                silence: 269
            }
        );
    }

    #[test]
    fn device_wake_in_deadline_mode_never_pads() {
        assert_eq!(
            plan(Wake::Device, true, 0, 0),
            FillPlan {
                real: 0,
                silence: 0
            }
        );
    }

    #[test]
    fn legacy_mode_pads_on_device_wake() {
        assert_eq!(
            plan(Wake::Device, false, 100, 200),
            FillPlan {
                real: 200,
                silence: 269
            }
        );
    }

    #[test]
    fn timeout_wake_pads_like_legacy() {
        assert_eq!(
            plan(Wake::Timeout, true, 0, 0),
            FillPlan {
                real: 0,
                silence: 569
            }
        );
    }

    #[test]
    fn target_caps_real_frames() {
        assert_eq!(
            plan(Wake::Data, true, 500, 1_000),
            FillPlan {
                real: 69,
                silence: 0
            }
        );
    }

    #[test]
    fn deadline_mode_needs_a_timer_and_a_long_enough_period() {
        assert!(deadline_mode(true, 441));
        // 128 frames = 2.9 ms, not more than two guards (3 ms).
        assert!(!deadline_mode(true, 128));
        assert!(!deadline_mode(false, 441));
    }

    #[test]
    fn wake_from_wait_maps_handles() {
        use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        let w0 = WAIT_OBJECT_0.0;
        assert_eq!(wake_from_wait(w0, true), Wake::Device);
        assert_eq!(wake_from_wait(w0 + 1, true), Wake::Data);
        assert_eq!(wake_from_wait(w0 + 2, true), Wake::Deadline);
        assert_eq!(wake_from_wait(WAIT_TIMEOUT.0, true), Wake::Timeout);
        // Without a timer only two handles are waited on.
        assert_eq!(wake_from_wait(w0, false), Wake::Device);
        assert_eq!(wake_from_wait(w0 + 1, false), Wake::Data);
        assert_eq!(wake_from_wait(w0 + 2, false), Wake::Timeout);
        assert_eq!(wake_from_wait(WAIT_TIMEOUT.0, false), Wake::Timeout);
    }

    /// Pushes `frames` frames of 0.5 into ring B, stamped as input that arrived at `now`.
    fn arrive(r: &Renderer, tx: &mut Producer<f32>, frames: usize, now: u64) {
        for _ in 0..frames * 2 {
            tx.push(0.5).unwrap();
        }
        r.ctx.shared.last_input_us.store(now, Ordering::Release);
    }

    /// The device reads one 441-frame period.
    fn read_period(padding: &AtomicUsize) {
        let p = padding.load(Ordering::Relaxed);
        padding.store(p.saturating_sub(441), Ordering::Relaxed);
    }

    /// Controller check: in deadline mode a short queue on a data wake is not a starvation;
    /// only the deadline (just before the device reads) pads, and only then is an underrun
    /// armed.
    #[test]
    fn a_short_queue_on_a_data_wake_neither_pads_nor_counts() {
        let sink = FakeSink::new(0);
        let (padding, out) = (sink.padding.clone(), sink.out.clone());
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let t = 10 * SEC;
        // Fed to the target on a data wake; the deadline finds no shortfall: primed.
        arrive(&r, &mut tx, 569, t);
        r.fill_at(Wake::Data, t).unwrap();
        assert!(!r.primed, "a data wake checks nothing");
        r.fill_at(Wake::Deadline, t + 8 * MS).unwrap();
        assert!(r.primed);
        read_period(&padding);
        r.fill_at(Wake::Device, t + 10 * MS).unwrap();
        arrive(&r, &mut tx, 172, t + 11 * MS);
        r.fill_at(Wake::Data, t + 11 * MS).unwrap();
        assert_eq!(padding.load(Ordering::Relaxed), 300, "written on arrival");
        // Nothing new: a data wake writes nothing and pads nothing (nor did the device wake
        // at 10 ms, whose deadline is still to come).
        r.fill_at(Wake::Data, t + 12 * MS).unwrap();
        assert_eq!(padding.load(Ordering::Relaxed), 300);
        assert_eq!(pad_events(&r), 0);
        assert!(r.underruns.starved_at.is_none(), "not judged a starvation");
        // The deadline sees 300 < 441 queued: pads up to the target, arms the judge.
        r.fill_at(Wake::Deadline, t + 18 * MS).unwrap();
        assert_eq!(padding.load(Ordering::Relaxed), 569);
        assert_eq!(pad_events(&r), 1);
        assert!(r.underruns.starved_at.is_some());
        assert_eq!(out.lock().unwrap().len() / 2, 569 + 172 + 269);
        let diag = &r.ctx.stats.diag;
        assert_eq!(diag.render_data_wakes.load(Ordering::Relaxed), 3);
        assert_eq!(diag.render_deadline_wakes.load(Ordering::Relaxed), 2);
    }

    /// One underrun per gap, as before O2: after a pad, real frames fed on data wakes do not
    /// re-prime the judge; a check that finds no shortfall does.
    #[test]
    fn a_gap_counts_once_until_a_check_passes() {
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let t = 10 * SEC;
        arrive(&r, &mut tx, 569, t);
        r.fill_at(Wake::Data, t).unwrap();
        r.fill_at(Wake::Deadline, t + 8 * MS).unwrap();
        assert!(r.primed);
        // Period 1: nothing arrives, the deadline pads and arms; input then keeps flowing
        // and the next wake counts it.
        read_period(&padding);
        r.fill_at(Wake::Device, t + 10 * MS).unwrap();
        r.ctx
            .shared
            .last_input_us
            .store(t + 15 * MS, Ordering::Release);
        r.fill_at(Wake::Deadline, t + 18 * MS).unwrap();
        assert!(r.underruns.starved_at.is_some());
        arrive(&r, &mut tx, 100, t + 19 * MS);
        r.fill_at(Wake::Data, t + 19 * MS).unwrap();
        assert_eq!(underruns(&r), 1);
        // Period 2: 100 frames fed on a data wake, still short at the deadline: the same
        // gap, not counted again.
        read_period(&padding);
        r.fill_at(Wake::Device, t + 20 * MS).unwrap();
        assert!(!r.primed);
        r.fill_at(Wake::Deadline, t + 28 * MS).unwrap();
        assert_eq!(pad_events(&r), 2);
        assert!(r.underruns.starved_at.is_none(), "not re-armed");
        // Period 3: fed to the target, the deadline finds no shortfall: primed again.
        read_period(&padding);
        r.fill_at(Wake::Device, t + 30 * MS).unwrap();
        arrive(&r, &mut tx, 441, t + 31 * MS);
        r.fill_at(Wake::Data, t + 31 * MS).unwrap();
        r.fill_at(Wake::Deadline, t + 38 * MS).unwrap();
        assert!(r.primed);
        assert_eq!(pad_events(&r), 2);
        // Period 4: short again, a new gap: armed again.
        read_period(&padding);
        r.fill_at(Wake::Device, t + 40 * MS).unwrap();
        r.ctx
            .shared
            .last_input_us
            .store(t + 45 * MS, Ordering::Release);
        r.fill_at(Wake::Deadline, t + 48 * MS).unwrap();
        assert_eq!(pad_events(&r), 3);
        assert!(r.underruns.starved_at.is_some());
    }

    fn missed_deadlines(r: &Renderer) -> u64 {
        r.ctx
            .stats
            .diag
            .render_missed_deadlines
            .load(Ordering::Relaxed)
    }

    /// Ruling 8 as amended by ruling 17: two device wakes with no deadline between them (the
    /// first was handled late, so the second cancelled its deadline) left a read unchecked;
    /// when the device then ran dry (nothing queued, nothing in ring B) the second device
    /// wake pads to the target and arms the underrun judge.
    #[test]
    fn a_missed_deadline_falls_back_to_a_device_wake_check() {
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let t = 10 * SEC;
        arrive(&r, &mut tx, 569, t);
        r.fill_at(Wake::Data, t).unwrap();
        r.fill_at(Wake::Deadline, t + 8 * MS).unwrap();
        assert!(r.primed);
        read_period(&padding);
        r.fill_at(Wake::Device, t + 10 * MS).unwrap();
        assert!(r.deadline_armed);
        assert_eq!((pad_events(&r), missed_deadlines(&r)), (0, 0));
        // Its deadline never fires; the device reads again (128 -> 0) and wakes.
        read_period(&padding);
        r.ctx
            .shared
            .last_input_us
            .store(t + 15 * MS, Ordering::Release);
        r.fill_at(Wake::Device, t + 20 * MS).unwrap();
        assert_eq!(missed_deadlines(&r), 1);
        assert_eq!(pad_events(&r), 1);
        assert_eq!(padding.load(Ordering::Relaxed), 569, "padded to the target");
        assert!(r.underruns.starved_at.is_some());
        assert!(r.deadline_armed, "re-armed for the next read");
        // Input kept flowing: the gap counts.
        arrive(&r, &mut tx, 128, t + 22 * MS);
        r.fill_at(Wake::Data, t + 22 * MS).unwrap();
        assert_eq!(underruns(&r), 1);
    }

    /// A deadline fires, then two device wakes with the deadline between them never fired:
    /// the device is at `dry_padding` after the second read, ring B holds `queued` frames.
    fn miss_a_deadline(
        r: &mut Renderer,
        padding: &AtomicUsize,
        tx: &mut Producer<f32>,
        at: u64,
        queued: usize,
        dry_padding: usize,
    ) {
        padding.store(500, Ordering::Relaxed);
        // The previous deadline fires as usual (500 queued: no shortfall).
        r.fill_at(Wake::Deadline, at - 2 * MS).unwrap();
        r.fill_at(Wake::Device, at).unwrap();
        assert!(r.deadline_armed || r.legacy_checks);
        padding.store(dry_padding, Ordering::Relaxed);
        arrive(r, tx, queued, at + 9 * MS);
        r.fill_at(Wake::Device, at + 10 * MS).unwrap();
    }

    /// Ruling 17 (2): an isolated miss with the device still holding audio (or ring B
    /// holding some) is no starvation: no pad, no underrun armed; the next deadline checks.
    #[test]
    fn an_isolated_missed_deadline_with_audio_queued_neither_pads_nor_counts() {
        for (dry_padding, queued) in [(128, 0), (0, 100)] {
            let sink = FakeSink::new(0);
            let padding = sink.padding.clone();
            let (mut r, _shared, mut tx) = test_renderer(Some(sink));
            r.after_open();
            r.deadline_mode = true;
            let t = 10 * SEC;
            arrive(&r, &mut tx, 569, t);
            r.fill_at(Wake::Data, t).unwrap();
            r.fill_at(Wake::Deadline, t + 8 * MS).unwrap();
            assert!(r.primed);
            miss_a_deadline(&mut r, &padding, &mut tx, t + 10 * MS, queued, dry_padding);
            assert_eq!(missed_deadlines(&r), 1);
            assert_eq!(pad_events(&r), 0, "padding {dry_padding} queued {queued}");
            assert!(r.underruns.starved_at.is_none());
            assert_eq!(padding.load(Ordering::Relaxed), dry_padding + queued);
            assert!(r.deadline_armed, "the next deadline checks");
            assert!(!r.legacy_checks);
        }
    }

    /// Ruling 17: a decay skip larger than ring B goes on discarding arriving frames (no
    /// second fade-out), and a check that has to pad cancels what is left of it.
    #[test]
    fn a_decay_skip_continues_across_wakes_until_a_pad_cancels_it() {
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let fade = r.fade_frames;
        let t = 10 * SEC;
        padding.store(128, Ordering::Relaxed);
        arrive(&r, &mut tx, 300, t);
        r.drop_pending = 400;
        r.drop_is_decay = true;
        r.fill_at(Wake::Device, t).unwrap();
        assert_eq!(
            padding.load(Ordering::Relaxed),
            128 + fade,
            "the fade-out only"
        );
        assert_eq!(decay_frames(&r), (300 - fade) as u64);
        assert_eq!(r.drop_pending, 400 - (300 - fade));
        // The next block is discarded whole: nothing written, no second fade.
        arrive(&r, &mut tx, 100, t + MS);
        r.fill_at(Wake::Data, t + MS).unwrap();
        assert_eq!(padding.load(Ordering::Relaxed), 128 + fade);
        assert_eq!(decay_frames(&r), (400 - fade) as u64);
        assert_eq!(r.drop_pending, 400 - (400 - fade));
        // The deadline finds less than a period queued: pads, and the rest is cancelled.
        r.fill_at(Wake::Deadline, t + 8_500).unwrap();
        assert_eq!(pad_events(&r), 1);
        assert_eq!(
            (r.drop_pending, r.drop_faded, r.drop_is_decay),
            (0, false, false)
        );
        assert_eq!(padding.load(Ordering::Relaxed), 569);
        // At the target, a new block waits in ring B instead of being discarded.
        arrive(&r, &mut tx, 100, t + 9 * MS);
        r.fill_at(Wake::Data, t + 9 * MS).unwrap();
        assert_eq!(
            decay_frames(&r),
            (400 - fade) as u64,
            "no longer discarding"
        );
        assert_eq!(r.ctx.input.slots() / 2, 100);
    }

    /// A pre-roll that starts while a decay is still discarding ends the discard (the output
    /// is already at silence; no fade-out of its own, no later cut).
    #[test]
    fn a_preroll_ends_a_decay_skip_still_discarding() {
        let sink = FakeSink::new(0);
        let (padding, out) = (sink.padding.clone(), sink.out.clone());
        let (mut r, shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let fade = r.fade_frames;
        let t = 10 * SEC;
        padding.store(128, Ordering::Relaxed);
        arrive(&r, &mut tx, 300, t);
        r.drop_pending = 400;
        r.drop_is_decay = true;
        r.fill_at(Wake::Device, t).unwrap();
        assert!(r.drop_faded && r.drop_pending > 0);
        let first = out.lock().unwrap().len();
        shared.preroll_request.store(true, Ordering::Release);
        arrive(&r, &mut tx, 128, t + MS);
        r.fill_at(Wake::Data, t + MS).unwrap();
        assert_eq!((r.drop_pending, r.drop_faded), (0, false));
        assert_eq!(
            decay_frames(&r),
            (300 - fade) as u64,
            "nothing more discarded"
        );
        // Silence first (no fade-out from full level), then the block fading in.
        let o = out.lock().unwrap();
        assert!(o.len() > first);
        assert_eq!(o[first], 0.0);
        assert!(o[first..].iter().all(|s| s.abs() <= 0.5));
    }

    /// Residual fix (ruling 17 re-review): with no check wake since the release the low point
    /// is unknown, so nothing is spare yet; the decay waits for a check, and gives up at the
    /// usual bound if none ever comes.
    #[test]
    fn a_decay_waits_for_a_check_wake_before_skipping() {
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let t = 100 * SEC;
        // Forced point passed (1.1 s), but no check wake observed yet.
        r.decay_drop = 441;
        r.decay_since_us = t - 1_100 * MS;
        assert_eq!(r.decay_low, usize::MAX);
        padding.store(300, Ordering::Relaxed);
        arrive(&r, &mut tx, 600, t);
        r.fill_at(Wake::Data, t).unwrap();
        assert_eq!(decay_frames(&r), 0, "nothing known to be spare");
        assert_eq!((r.drop_pending, r.decay_drop), (0, 441), "still pending");
        // The deadline sees 569 + 331 queued: 331 spare over the 569 target.
        r.fill_at(Wake::Deadline, t + 8 * MS).unwrap();
        assert_eq!(r.decay_low, 900);
        padding.store(128, Ordering::Relaxed);
        r.fill_at(Wake::Data, t + 10 * MS).unwrap();
        assert_eq!(decay_frames(&r), (331 - r.fade_frames) as u64);
        assert_eq!(r.drop_pending, r.fade_frames, "the rest as it arrives");
        assert_eq!(r.decay_drop, 0);

        // No check wake ever: given up a second past the forced point, nothing skipped.
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        r.decay_drop = 441;
        r.decay_since_us = t - 1_100 * MS;
        padding.store(300, Ordering::Relaxed);
        arrive(&r, &mut tx, 600, t);
        r.fill_at(Wake::Data, t).unwrap();
        assert_eq!(r.decay_drop, 441);
        padding.store(300, Ordering::Relaxed);
        r.fill_at(Wake::Data, t + 900 * MS).unwrap();
        assert_eq!((r.decay_drop, decay_frames(&r)), (0, 0), "given up at 2 s");
    }

    #[test]
    fn missed_deadlines_switch_only_when_three_fall_within_2s() {
        let mut m = MissedDeadlines::new();
        let t = 10 * SEC;
        assert!(!m.miss(t));
        assert!(!m.miss(t + SEC));
        // One a second: the oldest of three is exactly 2 s old, still isolated.
        assert!(!m.miss(t + 2 * SEC));
        assert!(m.miss(t + 2 * SEC + 999 * MS), "three within 2 s");
        m.reset();
        assert!(!m.miss(t));
        assert!(!m.miss(t + MS));
        assert!(m.miss(t + 2 * MS));
    }

    /// The render path with every new branch taken (a pre-roll, its decay discarding across
    /// wakes, isolated and repeated missed deadlines) never allocates.
    #[test]
    fn fill_at_does_not_allocate() {
        use crate::stemgen::alloc_count;
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        sink.out.lock().unwrap().reserve(4_000_000);
        let (mut r, shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let t = 10 * SEC;
        let mut decay_at_off = 0;
        let before = alloc_count::this_thread();
        for p in 0..400u64 {
            let at = t + p * 10 * MS;
            read_period(&padding);
            if p == 20 {
                shared.preroll_request.store(true, Ordering::Release);
            }
            // Devocal on and off again: the settle after it (ruling 21 b).
            let stage = match p {
                20..=199 => Stage::Devocal,
                200..=202 => Stage::FadingOut,
                _ => Stage::Passthrough,
            };
            r.ctx
                .stats
                .stage
                .store(crate::audio::stage_code(stage), Ordering::Release);
            if p == 150 {
                // Left queued by devocal: the settle after the off has it to skip.
                arrive(&r, &mut tx, 300, at);
            }
            if p == 200 {
                decay_at_off = decay_frames(&r);
            }
            r.fill_at(Wake::Device, at).unwrap();
            for h in 0..3u64 {
                arrive(
                    &r,
                    &mut tx,
                    if h == 2 { 185 } else { 128 },
                    at + (h + 1) * MS,
                );
                r.fill_at(Wake::Data, at + (h + 1) * MS).unwrap();
            }
            // An isolated missed deadline every 1.5 s, then three in a row near the end.
            if p % 150 != 75 && !(380..383).contains(&p) {
                r.fill_at(Wake::Deadline, at + 8_500).unwrap();
            }
        }
        assert_eq!(alloc_count::this_thread() - before, 0);
        let diag = &r.ctx.stats.diag;
        assert_eq!(diag.render_preroll_frames.load(Ordering::Relaxed), 441);
        assert!(decay_frames(&r) > 0, "the decay ran");
        assert!(r.settle_at.is_none() && !r.model_seen, "the settle armed");
        assert!(
            decay_frames(&r) > decay_at_off,
            "the settle skipped after the off"
        );
        // 76, 226, 376 isolated; 381 makes three within 2 s (226, 376, 381) and switches.
        assert_eq!(missed_deadlines(&r), 4);
        assert!(r.legacy_checks, "the repeated misses switched");
    }

    /// Ruling 17 (2): three misses within 2 s switch the stream to checking on device wakes
    /// (as before O2) until it is reopened; misses spread wider do not.
    #[test]
    fn three_missed_deadlines_within_2s_switch_the_stream_to_legacy_checks() {
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let t = 10 * SEC;
        let switches = |r: &Renderer| {
            r.ctx
                .stats
                .diag
                .render_legacy_switches
                .load(Ordering::Relaxed)
        };
        // Spread: 0, 1.5 s, 3.1 s (no three within 2 s).
        for at in [t, t + 1_500 * MS, t + 3_100 * MS] {
            miss_a_deadline(&mut r, &padding, &mut tx, at, 0, 128);
        }
        assert_eq!(missed_deadlines(&r), 3);
        assert!(!r.legacy_checks);
        // Two more within 2 s of the last: the third of them switches.
        miss_a_deadline(&mut r, &padding, &mut tx, t + 4_000 * MS, 0, 128);
        assert!(!r.legacy_checks);
        miss_a_deadline(&mut r, &padding, &mut tx, t + 5_000 * MS, 0, 128);
        assert!(r.legacy_checks);
        assert!(!r.deadline_mode);
        assert!(!r.deadline_armed);
        assert_eq!(switches(&r), 1);
        assert_eq!(missed_deadlines(&r), 5);
        // From now on a device wake is a check: 128 queued < a period pads to the target.
        let pads = pad_events(&r);
        padding.store(128, Ordering::Relaxed);
        r.fill_at(Wake::Device, t + 5_100 * MS).unwrap();
        assert_eq!(pad_events(&r), pads + 1);
        assert_eq!(padding.load(Ordering::Relaxed), 569);
        // `fill` keeps the stream out of deadline mode even with a timer.
        r.timer = HiResTimer::new();
        if r.timer.is_some() {
            r.fill().unwrap();
            assert!(!r.deadline_mode);
        }
        // A new stream starts in deadline mode again, with no misses remembered.
        r.after_open();
        assert!(!r.legacy_checks);
        r.deadline_mode = true;
        miss_a_deadline(&mut r, &padding, &mut tx, t + 6_000 * MS, 0, 128);
        miss_a_deadline(&mut r, &padding, &mut tx, t + 6_100 * MS, 0, 128);
        assert!(!r.legacy_checks, "misses of the old stream are forgotten");
        assert_eq!(switches(&r), 1);
    }

    /// The normal order (device, its deadline, the next device wake) never falls back; a new
    /// stream forgets a deadline the old one armed.
    #[test]
    fn a_deadline_between_device_wakes_is_no_fallback() {
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let t = 10 * SEC;
        for p in 0..5u64 {
            let at = t + p * 10 * MS;
            read_period(&padding);
            r.fill_at(Wake::Device, at).unwrap();
            assert!(r.deadline_armed);
            arrive(&r, &mut tx, 441, at + MS);
            r.fill_at(Wake::Data, at + MS).unwrap();
            r.fill_at(Wake::Deadline, at + 8 * MS).unwrap();
            assert!(!r.deadline_armed);
        }
        assert_eq!(missed_deadlines(&r), 0);
        // Every deadline found a full period queued.
        assert_eq!(pad_events(&r), 0);
        r.fill_at(Wake::Device, t + 50 * MS).unwrap();
        assert!(r.deadline_armed);
        r.after_open();
        assert!(
            !r.deadline_armed,
            "a new stream starts without a pending deadline"
        );
        r.deadline_mode = true;
        r.fill_at(Wake::Device, t + 60 * MS).unwrap();
        assert_eq!(missed_deadlines(&r), 0);
    }

    /// Trim watches the queue only on device and timeout wakes (a data wake sees the queue
    /// at a burst peak).
    #[test]
    fn trim_watches_only_device_and_timeout_wakes() {
        let sink = FakeSink::new(0);
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        for _ in 0..3_000 * 2 {
            tx.push(0.5).unwrap();
        }
        r.fill_at(Wake::Data, 10 * SEC).unwrap();
        r.fill_at(Wake::Deadline, 10 * SEC + MS).unwrap();
        assert!(r.trim.over.is_none());
        r.fill_at(Wake::Device, 10 * SEC + 2 * MS).unwrap();
        assert!(r.trim.over.is_some());
    }

    /// `fill` waits on the device event, ring B's data event (and the timer); it publishes
    /// the latency only on device and timeout wakes.
    #[test]
    fn fill_maps_events_to_wakes_and_publishes_on_device_wakes() {
        let sink = FakeSink::new(0);
        let device = sink.event.clone();
        let (mut r, shared, _tx) = test_renderer(Some(sink));
        r.after_open();
        shared.render_wake.set();
        r.fill().unwrap();
        assert_eq!(
            r.ctx.stats.diag.render_data_wakes.load(Ordering::Relaxed),
            1
        );
        assert_eq!(r.ctx.stats.latency_ms_milli.load(Ordering::Relaxed), 0);
        device.set();
        r.fill().unwrap();
        assert_eq!(
            r.ctx.stats.diag.render_data_wakes.load(Ordering::Relaxed),
            1
        );
        assert!(r.ctx.stats.latency_ms_milli.load(Ordering::Relaxed) > 0);
        // Nothing signalled: a timeout after 20 ms.
        r.fill().unwrap();
        let diag = &r.ctx.stats.diag;
        assert_eq!(diag.render_timeout_wakes.load(Ordering::Relaxed), 1);
        assert_eq!(diag.render_wakes.load(Ordering::Relaxed), 3);
    }

    /// Ruling 21 (b): the estimate counts the block the processing thread is working on. A
    /// hop moving from ring A into the model and on to ring B leaves the estimate unchanged;
    /// before, a device wake during inference read one hop (2.9 ms) low, so the estimate
    /// seemed to rise by a hop when devocal was switched off.
    #[test]
    fn the_latency_estimate_counts_the_block_in_flight() {
        let sink = FakeSink::new(300);
        let (mut r, shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        shared.capture_packet_frames.store(441, Ordering::Relaxed);
        shared.proc_latency_frames.store(128, Ordering::Relaxed);
        let estimate = |r: &Renderer| {
            r.publish(300);
            r.ctx.stats.latency_ms_milli.load(Ordering::Relaxed)
        };
        // The hop waits in ring A.
        shared.in_ring_frames.store(200, Ordering::Relaxed);
        let waiting = estimate(&r);
        // The model works on it: ring A holds the rest.
        shared.in_ring_frames.store(72, Ordering::Relaxed);
        shared.proc_in_flight_frames.store(128, Ordering::Relaxed);
        let in_flight = estimate(&r);
        // It reached ring B.
        shared.proc_in_flight_frames.store(0, Ordering::Relaxed);
        for _ in 0..128 * 2 {
            tx.push(0.0).unwrap();
        }
        let in_ring_b = estimate(&r);
        assert_eq!(in_flight, waiting);
        assert_eq!(in_ring_b, waiting);
    }

    /// Integer-time model of the render path for one capture phase (brief, Task 7).
    ///
    /// The device reads 441 frames every 10 000 us (a read with padding < 441 is a starved
    /// read) and wakes the renderer right after each read, plus once at stream start (t = 0);
    /// in O2 a deadline wake follows each device wake 8 500 us later. Capture delivers 441
    /// frames every 10 000 us at `phase` (+ `jitter` from a fixed LCG); processing emits a
    /// 128-frame hop for every 128 frames gathered, 300 us each, one after the other, and in
    /// O2 each hop is a data wake. Every wake writes as `plan_fill` says (headroom 0). The
    /// queue (padding + ring B) is sampled every 100 us for 2 s. The legacy run has device
    /// wakes only and `deadline_mode = false`. At equal times: device, capture, processing,
    /// deadline, sample.
    fn simulate_wakes(phase: u64, o2: bool, jitter_seed: Option<u64>) -> WakeSim {
        const PERIOD: usize = 441;
        const TARGET: usize = 441 + 128;
        const BUFFER: usize = 4_410;
        const PERIOD_US: u64 = 10_000;
        const HOP: usize = 128;
        const HOP_US: u64 = 300;
        const SAMPLE_US: u64 = 100;
        const LEN_US: u64 = 2_000_000;
        const START_US: u64 = 20_000;
        let mut lcg = jitter_seed.unwrap_or(0);
        let mut capture_at = |k: u64| -> u64 {
            let base = phase + k * PERIOD_US;
            match jitter_seed {
                None => base,
                Some(_) => {
                    lcg = lcg
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    let j = ((lcg >> 33) % 1_001) as i64 - 500;
                    (base as i64 + j).max(0) as u64
                }
            }
        };
        let (mut padding, mut ring, mut ring_a) = (0usize, 0usize, 0usize);
        let mut sim = WakeSim::default();
        let wake = |w: Wake, t: u64, padding: &mut usize, ring: &mut usize, sim: &mut WakeSim| {
            let p = plan_fill(w, o2, *padding, *ring, BUFFER - *padding, PERIOD, TARGET);
            *ring -= p.real;
            *padding += p.real + p.silence;
            if p.silence > 0 {
                sim.pads += 1;
                if t >= START_US {
                    sim.pads_after_start += 1;
                }
            }
        };
        let mut next_read = 0u64; // the stream-start wake, no read
        let mut k = 0u64;
        let mut next_capture = capture_at(0);
        let mut busy_until: Option<u64> = None;
        let mut deadline: Option<u64> = None;
        let mut next_sample = 0u64;
        let (mut sum, mut samples) = (0u128, 0u64);
        while next_sample < LEN_US {
            let candidates = [
                Some(next_read),
                Some(next_capture),
                busy_until,
                deadline,
                Some(next_sample),
            ];
            let (which, t) = candidates
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.map(|t| (i, t)))
                .min_by_key(|&(i, t)| (t, i))
                .unwrap();
            match which {
                0 => {
                    if t > 0 {
                        if padding < PERIOD {
                            sim.starved_reads += 1;
                        }
                        padding -= padding.min(PERIOD);
                    }
                    wake(Wake::Device, t, &mut padding, &mut ring, &mut sim);
                    if o2 {
                        deadline = Some(t + PERIOD_US - DEADLINE_GUARD_US);
                    }
                    next_read = t + PERIOD_US;
                }
                1 => {
                    ring_a += PERIOD;
                    if busy_until.is_none() && ring_a >= HOP {
                        ring_a -= HOP;
                        busy_until = Some(t + HOP_US);
                    }
                    k += 1;
                    next_capture = capture_at(k);
                }
                2 => {
                    ring += HOP;
                    if o2 {
                        wake(Wake::Data, t, &mut padding, &mut ring, &mut sim);
                    }
                    busy_until = if ring_a >= HOP {
                        ring_a -= HOP;
                        Some(t + HOP_US)
                    } else {
                        None
                    };
                }
                3 => {
                    deadline = None;
                    wake(Wake::Deadline, t, &mut padding, &mut ring, &mut sim);
                }
                _ => {
                    sum += (padding + ring) as u128;
                    samples += 1;
                    next_sample += SAMPLE_US;
                }
            }
        }
        sim.mean_queue = sum as f64 / samples as f64;
        sim
    }

    #[derive(Default, Debug)]
    struct WakeSim {
        mean_queue: f64,
        starved_reads: u32,
        pads: u32,
        pads_after_start: u32,
    }

    #[test]
    fn write_on_arrival_never_starves_regular_input() {
        for phase in (0..10_000).step_by(250) {
            let s = simulate_wakes(phase, true, None);
            assert_eq!(s.starved_reads, 0, "phase {phase}: {s:?}");
            assert!(s.pads_after_start <= 1, "phase {phase}: {s:?}");
            let j = simulate_wakes(phase, true, Some(0x5eed_0002));
            eprintln!(
                "O2 phase {phase:>5} us: starved {} pads {} (after 20 ms {}); \
                 jitter starved {} pads {}",
                s.starved_reads, s.pads, s.pads_after_start, j.starved_reads, j.pads
            );
            assert_eq!(j.starved_reads, 0, "phase {phase} with jitter: {j:?}");
        }
    }

    #[test]
    fn write_on_arrival_lowers_the_mean_queue() {
        let mut drop_sum = 0.0;
        let mut n = 0.0;
        for phase in (0..10_000).step_by(250) {
            let new = simulate_wakes(phase, true, None);
            let old = simulate_wakes(phase, false, None);
            eprintln!(
                "phase {phase:>5} us: mean queue old {:.1} new {:.1} (drop {:.1}); \
                 starved old {} new {}",
                old.mean_queue,
                new.mean_queue,
                old.mean_queue - new.mean_queue,
                old.starved_reads,
                new.starved_reads
            );
            assert!(
                new.mean_queue <= old.mean_queue + 1.0,
                "phase {phase}: new {:.1} old {:.1}",
                new.mean_queue,
                old.mean_queue
            );
            drop_sum += old.mean_queue - new.mean_queue;
            n += 1.0;
        }
        let mean_drop = drop_sum / n;
        eprintln!("mean drop over all phases: {mean_drop:.1} frames");
        assert!(mean_drop >= 132.0, "mean drop {mean_drop:.1} frames");
    }

    /// One run of [`simulate_events`].
    #[derive(Clone, Copy)]
    struct EvCfg {
        /// Capture packet phase within the 10 ms device period, us.
        phase: u64,
        len_us: u64,
        /// The capture stalls: packets due in `[at, at + late)` all arrive at `at + late`.
        stall: Option<(u64, u64)>,
        /// The pre-roll request (an accepted "on") is set at this time.
        preroll_at: Option<u64>,
        /// One deadline in every `n` never fires (index `k % n == n / 2`): the device wake
        /// after it finds it still armed.
        skip_every: Option<u64>,
        /// Processing time of one 128-frame hop, us (300: passthrough; a model takes longer,
        /// so a packet's hops reach ring B as a slower burst). With `devocal` it applies only
        /// while the model runs, and hops take 300 us otherwise.
        hop_us: u64,
        /// Devocal on at `.0` (with its pre-roll request) and off at `.1`, as the processor
        /// does it: `WarmingUp` for 69 blocks, `FadingIn` for 7, `Devocal`; off: `FadingOut`
        /// for 7 blocks, then `Passthrough`. A block's stage is published before the block
        /// is pushed (as the processing thread does).
        devocal: Option<(u64, u64)>,
        /// This many extra frames reach ring B at this time: audio queued beyond what O2
        /// needs (say, a pre-roll the decay could not skip).
        extra: Option<(u64, usize)>,
        /// Devocal on again at `.0` (another pre-roll request) and off again at `.1`, after
        /// the `devocal` pair; same stages.
        again: Option<(u64, u64)>,
    }

    impl EvCfg {
        fn new(phase: u64, len_us: u64) -> Self {
            Self {
                phase,
                len_us,
                stall: None,
                preroll_at: None,
                skip_every: None,
                hop_us: 300,
                devocal: None,
                extra: None,
                again: None,
            }
        }
    }

    /// What [`simulate_events`] saw; times are sim times in us.
    #[derive(Default, Debug)]
    struct EvSim {
        /// Device reads that found less than a period queued.
        starved: Vec<u64>,
        /// Wakes that padded.
        pads: Vec<u64>,
        underruns: u64,
        decay: u64,
        preroll: u64,
        missed: u64,
        /// The queue (padding + ring B) every 100 us, and at each deadline wake (before it).
        queue: Vec<(u64, usize)>,
        at_deadline: Vec<(u64, usize)>,
        /// Largest headroom seen, and when it was released (dropped to 0).
        max_headroom: usize,
        released_at: Option<u64>,
        /// Wakes that skipped frames as a decay: time and ring B read position before it.
        skips: Vec<(u64, u64)>,
        /// Ring B position after the last block of the crossfade back to passthrough.
        crossfade_end: Option<u64>,
    }

    impl EvSim {
        fn mean_queue(&self, from: u64, to: u64) -> f64 {
            let w: Vec<usize> = self
                .queue
                .iter()
                .filter(|(t, _)| (from..to).contains(t))
                .map(|&(_, q)| q)
                .collect();
            w.iter().sum::<usize>() as f64 / w.len() as f64
        }

        /// Lowest queue a deadline found in `[from, to)`.
        fn low(&self, from: u64, to: u64) -> usize {
            self.at_deadline
                .iter()
                .filter(|(t, _)| (from..to).contains(t))
                .map(|&(_, q)| q)
                .min()
                .unwrap()
        }

        fn pads_after(&self, t: u64) -> usize {
            self.pads.iter().filter(|&&p| p > t).count()
        }
    }

    /// Event-driven model of the real render path in deadline mode (ruling 17): `fill_at` on
    /// a real `Renderer` with a fake 441-frame-period device. The device reads 441 frames
    /// every 10 000 us (a read with padding < 441 is starved) and wakes the renderer right
    /// after each read, plus once at stream start (t = 0); each device wake arms a deadline
    /// wake one period minus [`DEADLINE_GUARD_US`] later. Capture delivers 441 frames every
    /// 10 000 us at `phase` (stamped as fresh input); processing turns them into 128-frame
    /// hops of 0.5, 300 us each, one after the other, each a data wake. At equal times:
    /// device, capture, processing, deadline, sample.
    fn simulate_events(cfg: EvCfg) -> EvSim {
        const PERIOD: usize = 441;
        const PERIOD_US: u64 = 10_000;
        const HOP: usize = 128;
        let hop_us = cfg.hop_us;
        const SAMPLE_US: u64 = 100;
        const T0: u64 = 10 * SEC;
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let capture_at = |k: u64| {
            let due = cfg.phase + k * PERIOD_US;
            match cfg.stall {
                Some((at, late)) if (at..at + late).contains(&due) => at + late,
                _ => due,
            }
        };
        let mut sim = EvSim::default();
        let wake = |r: &mut Renderer, w: Wake, t: u64, sim: &mut EvSim| {
            let (pads, decay, read_pos) = (pad_events(r), decay_frames(r), r.read_pos);
            r.fill_at(w, T0 + t).unwrap();
            if pad_events(r) > pads {
                sim.pads.push(t);
            }
            if decay_frames(r) > decay {
                sim.skips.push((t, read_pos));
            }
            let h = r.headroom.total();
            if h > sim.max_headroom {
                sim.max_headroom = h;
            }
            if h == 0 && sim.max_headroom > 0 && sim.released_at.is_none() {
                sim.released_at = Some(t);
            }
        };
        let (mut next_read, mut k, mut ring_a) = (0u64, 0u64, 0usize);
        let mut next_capture = capture_at(0);
        let mut busy_until: Option<u64> = None;
        let mut deadline: Option<u64> = None;
        let mut deadlines = 0u64;
        let mut next_sample = 0u64;
        let mut preroll_at = cfg.preroll_at.or(cfg.devocal.map(|d| d.0));
        // Devocal on and off times, in order (each "on" also requests a pre-roll).
        let mut ons: Vec<u64> = cfg.devocal.iter().chain(&cfg.again).map(|d| d.0).collect();
        let mut offs: Vec<u64> = cfg.devocal.iter().chain(&cfg.again).map(|d| d.1).collect();
        let mut extra_at = cfg.extra;
        // The processor's stage and the blocks left in it; frames pushed to ring B.
        let (mut stage, mut blocks_left, mut written) = (Stage::Passthrough, 0usize, 0u64);
        let publish = |r: &Renderer, stage: Stage| {
            r.ctx
                .stats
                .stage
                .store(crate::audio::stage_code(stage), Ordering::Release);
        };
        if ons.first() == Some(&0) {
            // Devocal on before the stream's first fill (attach with devocal on).
            ons.remove(0);
            stage = Stage::WarmingUp;
            blocks_left = 69;
            publish(&r, stage);
        }
        if preroll_at == Some(0) {
            // An "on" accepted before the stream's first fill (attach with devocal on).
            preroll_at = None;
            shared.preroll_request.store(true, Ordering::Release);
        }
        let hop_time = |stage: Stage| {
            if cfg.devocal.is_some() && !stage_runs_model(stage) {
                300
            } else {
                hop_us
            }
        };
        while next_sample < cfg.len_us {
            let candidates = [
                Some(next_read),
                Some(next_capture),
                busy_until,
                deadline,
                Some(next_sample),
                preroll_at,
                ons.first().copied(),
                offs.first().copied(),
                extra_at.map(|e| e.0),
            ];
            let (which, t) = candidates
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.map(|t| (i, t)))
                .min_by_key(|&(i, t)| (t, i))
                .unwrap();
            match which {
                0 => {
                    if t > 0 {
                        let p = padding.load(Ordering::Relaxed);
                        if p < PERIOD {
                            sim.starved.push(t);
                        }
                        padding.store(p.saturating_sub(PERIOD), Ordering::Relaxed);
                    }
                    wake(&mut r, Wake::Device, t, &mut sim);
                    let skipped = cfg.skip_every.is_some_and(|n| deadlines % n == n / 2);
                    deadlines += 1;
                    deadline = (!skipped).then_some(t + PERIOD_US - DEADLINE_GUARD_US);
                    next_read = t + PERIOD_US;
                }
                1 => {
                    ring_a += PERIOD;
                    shared.last_input_us.store(T0 + t, Ordering::Release);
                    if busy_until.is_none() && ring_a >= HOP {
                        ring_a -= HOP;
                        busy_until = Some(t + hop_time(stage));
                    }
                    k += 1;
                    next_capture = capture_at(k);
                }
                2 => {
                    let crossfade = stage == Stage::FadingOut;
                    if blocks_left > 0 {
                        blocks_left -= 1;
                        if blocks_left == 0 {
                            stage = match stage {
                                Stage::WarmingUp => {
                                    blocks_left = 7;
                                    Stage::FadingIn
                                }
                                Stage::FadingIn => Stage::Devocal,
                                _ => Stage::Passthrough,
                            };
                        }
                    }
                    // The stage after this block is published before it is pushed.
                    publish(&r, stage);
                    for _ in 0..HOP * 2 {
                        tx.push(0.5).unwrap();
                    }
                    written += HOP as u64;
                    if crossfade {
                        sim.crossfade_end = Some(written);
                    }
                    wake(&mut r, Wake::Data, t, &mut sim);
                    busy_until = if ring_a >= HOP {
                        ring_a -= HOP;
                        Some(t + hop_time(stage))
                    } else {
                        None
                    };
                }
                3 => {
                    deadline = None;
                    let q = padding.load(Ordering::Relaxed) + r.ctx.input.slots() / 2;
                    sim.at_deadline.push((t, q));
                    wake(&mut r, Wake::Deadline, t, &mut sim);
                }
                4 => {
                    let q = padding.load(Ordering::Relaxed) + r.ctx.input.slots() / 2;
                    sim.queue.push((t, q));
                    next_sample += SAMPLE_US;
                }
                5 => {
                    preroll_at = None;
                    shared.preroll_request.store(true, Ordering::Release);
                }
                6 => {
                    ons.remove(0);
                    shared.preroll_request.store(true, Ordering::Release);
                    stage = Stage::WarmingUp;
                    blocks_left = 69;
                    publish(&r, stage);
                }
                7 => {
                    offs.remove(0);
                    stage = Stage::FadingOut;
                    blocks_left = 7;
                    publish(&r, stage);
                }
                _ => {
                    let (_, n) = extra_at.take().unwrap();
                    for _ in 0..n * 2 {
                        tx.push(0.5).unwrap();
                    }
                    written += n as u64;
                    wake(&mut r, Wake::Data, t, &mut sim);
                }
            }
        }
        let diag = &r.ctx.stats.diag;
        sim.underruns = underruns(&r);
        sim.decay = decay_frames(&r);
        sim.preroll = diag.render_preroll_frames.load(Ordering::Relaxed);
        sim.missed = missed_deadlines(&r);
        sim
    }

    /// Ruling 17 (1): under O2 the decay's low point is taken at check wakes only; a device
    /// wake sees the queue right after a read, about one period below what the next read
    /// finds, which used to cancel the decay. A real underrun (the capture stalls 12 ms at
    /// about 1 s: two packets arrive late, together) grows the headroom by a hop; 5 s later
    /// it is released and skipped whole, in every capture phase, with no starved read and no
    /// further pad or underrun.
    #[test]
    fn o2_decay_skips_the_released_jitter_headroom_in_every_phase() {
        for phase in (0..10_000).step_by(500) {
            let mut cfg = EvCfg::new(phase, 8 * SEC);
            cfg.stall = Some((SEC + phase, 12 * MS));
            let s = simulate_events(cfg);
            eprintln!(
                "phase {phase:>5}: headroom {} released {:?} decay {} underruns {} pads {:?} \
                 starved {:?} queue before {:.1} after {:.1}",
                s.max_headroom,
                s.released_at,
                s.decay,
                s.underruns,
                s.pads,
                s.starved,
                s.mean_queue(5 * SEC, 6 * SEC),
                s.mean_queue(7 * SEC, 8 * SEC)
            );
            assert_eq!(s.max_headroom, 128, "phase {phase}");
            assert!(s.released_at.is_some(), "phase {phase}");
            assert_eq!(s.decay, 128, "phase {phase}");
            assert_eq!(s.underruns, 1, "phase {phase}");
            assert!(s.starved.is_empty(), "phase {phase}: {:?}", s.starved);
            assert_eq!(
                s.pads_after(SEC + 50 * MS),
                0,
                "phase {phase}: {:?}",
                s.pads
            );
        }
    }

    /// Ruling 17 (2): one deadline in a hundred never firing (its device wake handled late)
    /// changes nothing when the device has audio queued: the same pads, underruns, decay and
    /// mean queue as with every deadline firing, in every capture phase.
    #[test]
    fn o2_one_missed_deadline_in_a_hundred_changes_nothing() {
        for phase in (0..10_000).step_by(500) {
            let mut cfg = EvCfg::new(phase, 8 * SEC);
            cfg.stall = Some((SEC + phase, 12 * MS));
            let base = simulate_events(cfg);
            cfg.skip_every = Some(100);
            let s = simulate_events(cfg);
            let (q0, q1) = (base.mean_queue(0, 8 * SEC), s.mean_queue(0, 8 * SEC));
            eprintln!(
                "phase {phase:>5}: missed {} pads {} vs {} underruns {} vs {} decay {} vs {} \
                 mean queue {q0:.2} vs {q1:.2} starved {} vs {}",
                s.missed,
                base.pads.len(),
                s.pads.len(),
                base.underruns,
                s.underruns,
                base.decay,
                s.decay,
                base.starved.len(),
                s.starved.len()
            );
            assert_eq!(s.missed, 8, "phase {phase}: one a second");
            assert_eq!(s.pads, base.pads, "phase {phase}");
            assert_eq!(s.underruns, base.underruns, "phase {phase}");
            assert_eq!(s.decay, base.decay, "phase {phase}");
            assert!(s.starved.is_empty(), "phase {phase}");
            assert!((q1 - q0).abs() <= 1.0, "phase {phase}: {q0} vs {q1}");
        }
    }

    /// Ruling 17 (minor): a pre-roll writes exactly the silence it registers as headroom
    /// (it wrote 765 frames for 441 registered), and once the model has warmed up (here the
    /// 200 ms grace) the decay skips everything above the target, in every capture phase,
    /// in one fade-out / fade-in. Ruling 3 floors the decay at the target, and O2's steady
    /// low sits up to a hop below it (only a period must be queued at a deadline), so what
    /// is not skipped is exactly that distance: 441 - (target - low before the "on").
    #[test]
    fn o2_decay_skips_a_released_preroll_in_every_phase() {
        const TARGET: usize = 441 + 128;
        for phase in (0..10_000).step_by(500) {
            let mut cfg = EvCfg::new(phase, 3 * SEC);
            cfg.preroll_at = Some(SEC);
            let s = simulate_events(cfg);
            let (before, after) = (
                s.mean_queue(SEC / 2, SEC),
                s.mean_queue(5 * SEC / 2, 3 * SEC),
            );
            let (low_before, low_after) = (s.low(SEC / 2, SEC), s.low(5 * SEC / 2, 3 * SEC));
            eprintln!(
                "phase {phase:>5}: preroll {} decay {} released {:?} pads {:?} starved {:?} \
                 queue before {before:.1} after {after:.1} low before {low_before} after \
                 {low_after}",
                s.preroll, s.decay, s.released_at, s.pads, s.starved
            );
            assert_eq!(s.preroll, 441, "phase {phase}: silence written");
            assert!(s.starved.is_empty(), "phase {phase}");
            assert_eq!(s.pads_after(50 * MS), 0, "phase {phase}: {:?}", s.pads);
            assert_eq!(s.underruns, 0, "phase {phase}");
            assert!(
                (TARGET..=TARGET + 16).contains(&low_after),
                "phase {phase}: settles at the target, low {low_after}"
            );
            let below = TARGET.saturating_sub(low_before) as u64;
            assert!(below < 128, "phase {phase}: low before {low_before}");
            assert!(
                (441 - 16..=441).contains(&(s.decay + below)),
                "phase {phase}: decay {} below {below}",
                s.decay
            );
            assert!(after <= before + 128.0 + 16.0, "phase {phase}");
        }
    }

    /// Ruling 21 (a): with the pre-roll released the target is period + hop again, which is
    /// the floor the decay skips down to, so a pre-roll leaves the queue within one hop of the
    /// never-toggled queue in every capture phase (the rest is O2's own spread: its steady low
    /// sits between a period and the target). This holds with the "on" at the stream start
    /// (attach with devocal on) and in steady state, and with a packet's hops taking 300 us
    /// each (passthrough) or 1 or 2 ms each (a model: hop-sized bursts spread over the
    /// period). No read starves, nothing is padded after the stream start, no underrun.
    #[test]
    fn o2_preroll_returns_within_a_hop_of_the_never_toggled_queue() {
        for hop_us in [300, 1_000, 2_000] {
            for phase in (0..10_000).step_by(500) {
                let cfg = EvCfg {
                    hop_us,
                    ..EvCfg::new(phase, 3 * SEC)
                };
                let base = simulate_events(cfg);
                let base_low = base.low(5 * SEC / 2, 3 * SEC);
                assert!(base.starved.is_empty(), "hop {hop_us} phase {phase}");
                assert_eq!(base.pads_after(50 * MS), 0, "hop {hop_us} phase {phase}");
                for at in [0, SEC] {
                    let s = simulate_events(EvCfg {
                        preroll_at: Some(at),
                        ..cfg
                    });
                    let low = s.low(5 * SEC / 2, 3 * SEC);
                    let what = format!("hop {hop_us} phase {phase} on at {at}");
                    eprintln!(
                        "{what}: decay {} low {low} never toggled {base_low} (+{}) pads {:?} \
                         starved {}",
                        s.decay,
                        low as i64 - base_low as i64,
                        s.pads,
                        s.starved.len()
                    );
                    assert_eq!(s.preroll, 441, "{what}");
                    assert!(s.starved.is_empty(), "{what}");
                    assert_eq!(s.pads_after(50 * MS), 0, "{what}: {:?}", s.pads);
                    assert_eq!(s.underruns, 0, "{what}");
                    assert!(
                        (base_low..=base_low + 128).contains(&low),
                        "{what}: low {low}, never toggled {base_low}"
                    );
                }
            }
        }
    }

    /// Ruling 21 (b): an on -> off cycle (devocal on at the stream start or later, hops of 1
    /// or 2 ms while the model runs, off 2 s in) leaves the queue within one hop of the
    /// never-toggled queue, in every capture phase, with no starved read, no pad after the
    /// stream start and no underrun.
    #[test]
    fn o2_devocal_off_returns_to_the_never_toggled_queue() {
        for hop_us in [1_000, 2_000] {
            for phase in (0..10_000).step_by(500) {
                let base = simulate_events(EvCfg::new(phase, 4 * SEC));
                let base_low = base.low(7 * SEC / 2, 4 * SEC);
                for on in [0, SEC / 2] {
                    let s = simulate_events(EvCfg {
                        hop_us,
                        devocal: Some((on, 2 * SEC)),
                        ..EvCfg::new(phase, 4 * SEC)
                    });
                    let low = s.low(7 * SEC / 2, 4 * SEC);
                    let what = format!("hop {hop_us} phase {phase} on at {on}");
                    eprintln!(
                        "{what}: decay {} skips {:?} low {low} never toggled {base_low} (+{}) \
                         pads {:?}",
                        s.decay,
                        s.skips,
                        low as i64 - base_low as i64,
                        s.pads
                    );
                    assert!(s.starved.is_empty(), "{what}");
                    assert_eq!(s.pads_after(50 * MS), 0, "{what}: {:?}", s.pads);
                    assert_eq!(s.underruns, 0, "{what}");
                    assert!(
                        (base_low..=base_low + 128).contains(&low),
                        "{what}: low {low}, never toggled {base_low}"
                    );
                }
            }
        }
    }

    /// Ruling 21 (b): audio devocal left queued above the target (here 300 frames reaching
    /// ring B during devocal, as a pre-roll the decay could not skip would) is released once
    /// devocal is off: through the decay path (low point at the checks, quiet spot or 1 s,
    /// fade out, skip, fade in), only after the crossfade back to the passthrough has left
    /// ring B, back to within one hop of the never-toggled queue, in every capture phase,
    /// with no starved read, pad or underrun.
    #[test]
    fn o2_devocal_off_releases_what_devocal_left_queued_after_the_crossfade() {
        const OFF: u64 = 2 * SEC;
        for hop_us in [1_000, 2_000] {
            for phase in (0..10_000).step_by(500) {
                let base = simulate_events(EvCfg::new(phase, 4 * SEC));
                let base_low = base.low(7 * SEC / 2, 4 * SEC);
                let s = simulate_events(EvCfg {
                    hop_us,
                    devocal: Some((0, OFF)),
                    extra: Some((13 * SEC / 10, 300)),
                    ..EvCfg::new(phase, 4 * SEC)
                });
                let (during, low) = (s.low(16 * SEC / 10, OFF), s.low(7 * SEC / 2, 4 * SEC));
                let what = format!("hop {hop_us} phase {phase}");
                let fade_end = s.crossfade_end.expect("a crossfade");
                let after_off: Vec<_> = s.skips.iter().filter(|&&(t, _)| t >= OFF).collect();
                eprintln!(
                    "{what}: decay {} skips {:?} crossfade ends at {fade_end} low during \
                     devocal {during} after {low} never toggled {base_low} (+{}) pads {:?}",
                    s.decay,
                    s.skips,
                    low as i64 - base_low as i64,
                    s.pads
                );
                assert!(during > base_low + 128, "{what}: queued during devocal");
                assert!(!after_off.is_empty(), "{what}: released after the off");
                for &&(t, read_pos) in &after_off {
                    assert!(
                        read_pos >= fade_end,
                        "{what}: skip at {t} from {read_pos}, crossfade ends at {fade_end}"
                    );
                }
                assert!(s.starved.is_empty(), "{what}");
                assert_eq!(s.pads_after(50 * MS), 0, "{what}: {:?}", s.pads);
                assert_eq!(s.underruns, 0, "{what}");
                assert!(
                    (base_low..=base_low + 128).contains(&low),
                    "{what}: low {low}, never toggled {base_low}"
                );
            }
        }
    }

    /// Ruling 21 (b): nothing is skipped while the crossfade back to the passthrough is
    /// running. Devocal goes off 10 ms before the pre-roll's decay would be forced (1 s after
    /// its release), so that decision falls inside the crossfade: it waits until the
    /// crossfade has left ring B (and then observes the passthrough's lows afresh), in every
    /// capture phase, with no starved read, pad or underrun.
    #[test]
    fn o2_no_decay_skips_while_the_crossfade_back_runs() {
        for hop_us in [1_000, 2_000] {
            for phase in (0..10_000).step_by(500) {
                let cfg = EvCfg {
                    hop_us,
                    devocal: Some((0, 10 * SEC)),
                    ..EvCfg::new(phase, 3 * SEC)
                };
                let released = simulate_events(cfg).released_at.expect("released");
                let off = released + SEC - 10 * MS;
                let s = simulate_events(EvCfg {
                    devocal: Some((0, off)),
                    ..cfg
                });
                let what = format!("hop {hop_us} phase {phase} off at {off}");
                let fade_end = s.crossfade_end.expect("a crossfade");
                eprintln!(
                    "{what}: decay {} skips {:?} crossfade ends at {fade_end} pads {:?}",
                    s.decay, s.skips, s.pads
                );
                assert!(
                    s.skips.iter().any(|&(t, _)| t >= off),
                    "{what}: the deferred decay skipped after the off"
                );
                for &(t, read_pos) in s.skips.iter().filter(|&&(t, _)| t >= off) {
                    assert!(
                        read_pos >= fade_end,
                        "{what}: skip at {t} from {read_pos}, crossfade ends at {fade_end}"
                    );
                }
                assert!(s.starved.is_empty(), "{what}");
                assert_eq!(s.pads_after(50 * MS), 0, "{what}: {:?}", s.pads);
                assert_eq!(s.underruns, 0, "{what}");
            }
        }
    }

    /// Ruling 23 (1): devocal back on 150, 300 or 550 ms after an off (100-500 ms after the
    /// settle started), with 300 frames left queued from the first devocal so the queue is
    /// deep and the second "on" pre-rolls little or nothing. The settle must not go on to
    /// skip the cushion the model needs: no starved read, no pad after the start, no
    /// underrun, in every capture phase with hops of 1 and 2 ms. Off again at 3.5 s: the
    /// second settle (no stale one stacked with the new pre-roll's release) brings the queue
    /// back to within a hop of the never-toggled queue.
    #[test]
    fn o2_devocal_back_on_during_a_settle_keeps_the_cushion() {
        const OFF: u64 = 2 * SEC;
        for hop_us in [1_000, 2_000] {
            for phase in (0..10_000).step_by(500) {
                let base = simulate_events(EvCfg::new(phase, 11 * SEC / 2));
                let base_low = base.low(5 * SEC, 11 * SEC / 2);
                for back in [150 * MS, 300 * MS, 550 * MS] {
                    let s = simulate_events(EvCfg {
                        hop_us,
                        devocal: Some((0, OFF)),
                        extra: Some((13 * SEC / 10, 300)),
                        again: Some((OFF + back, 7 * SEC / 2)),
                        ..EvCfg::new(phase, 11 * SEC / 2)
                    });
                    let low = s.low(5 * SEC, 11 * SEC / 2);
                    let what = format!("hop {hop_us} phase {phase} back on after {back}");
                    eprintln!(
                        "{what}: decay {} skips {:?} low {low} never toggled {base_low} (+{}) \
                         pads {:?} starved {:?} underruns {}",
                        s.decay,
                        s.skips,
                        low as i64 - base_low as i64,
                        s.pads,
                        s.starved,
                        s.underruns
                    );
                    assert!(s.starved.is_empty(), "{what}: {:?}", s.starved);
                    assert_eq!(s.pads_after(50 * MS), 0, "{what}: {:?}", s.pads);
                    assert_eq!(s.underruns, 0, "{what}");
                    assert!(
                        (base_low..=base_low + 128).contains(&low),
                        "{what}: low {low}, never toggled {base_low}"
                    );
                }
            }
        }
    }

    /// Ruling 23 (1): devocal back on ends a settle already started, decision pending or skip
    /// deciding, but not a released headroom's decay of its own.
    #[test]
    fn devocal_back_on_ends_a_started_settle_only() {
        let sink = FakeSink::new(441);
        let (mut r, _shared, _tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let set_stage = |r: &Renderer, stage: Stage| {
            r.ctx
                .stats
                .stage
                .store(crate::audio::stage_code(stage), Ordering::Release);
        };
        let t = 10 * SEC;
        // A settle started in passthrough (decision pending, and a skip already decided).
        r.fill_at(Wake::Data, t).unwrap();
        r.settling = true;
        r.decay_drop = 882;
        r.decay_min = 128;
        r.drop_pending = 200;
        r.drop_is_decay = true;
        set_stage(&r, Stage::WarmingUp);
        r.fill_at(Wake::Data, t + MS).unwrap();
        assert_eq!((r.decay_drop, r.decay_min, r.drop_pending), (0, 0, 0));
        assert!(!r.settling && !r.drop_is_decay);
        // Off, then on again with only a released headroom's decay pending: it stays.
        set_stage(&r, Stage::Passthrough);
        r.fill_at(Wake::Data, t + 2 * MS).unwrap();
        r.decay_drop = 441;
        r.decay_since_us = t + 2 * MS;
        set_stage(&r, Stage::WarmingUp);
        r.fill_at(Wake::Data, t + 3 * MS).unwrap();
        assert_eq!(r.decay_drop, 441);
    }

    /// Ruling 23 (2): a trim restarts a pending decay's observation, so the decay cannot skip
    /// the excess the trim has just taken.
    #[test]
    fn a_trim_restarts_a_pending_decays_observation() {
        let sink = FakeSink::new(441);
        let padding = sink.padding.clone();
        let (mut r, _shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        let t = 10 * SEC;
        for _ in 0..3_000 * 2 {
            tx.push(0.5).unwrap();
        }
        // Over target + 20 ms for a whole second already: this device wake trims.
        r.fill_at(Wake::Device, t - 2 * MS).unwrap();
        r.trim.over = Some((t - 2 * SEC, 3_000));
        r.decay_drop = 441;
        r.decay_since_us = t - SEC;
        r.decay_low = 2_000;
        read_period(&padding);
        r.fill_at(Wake::Device, t).unwrap();
        assert!(
            r.ctx.stats.diag.render_trim_frames.load(Ordering::Relaxed) > 0,
            "trimmed"
        );
        assert_eq!((r.decay_low, r.decay_since_us), (usize::MAX, t));
        assert_eq!(decay_frames(&r), 0);
    }

    /// Frames to milliseconds at the engine rate.
    fn frames_to_ms(frames: u64) -> f64 {
        frames as f64 * 1000.0 / f64::from(SAMPLE_RATE)
    }

    #[test]
    fn system_path_matches_the_p0_measurement() {
        // P0 report 2026-10-03: P = 441 frames (10 ms); sys_a = 58.0 - (55.8 - 30) = 32.2 ms,
        // sys_c = 68.0 - (65.8 - 30) = 32.2 ms, so sys = 32.2 ms.
        let ms = frames_to_ms(system_path_frames(441));
        assert!((ms - 32.2).abs() <= 1.0, "{ms} ms");
    }

    #[test]
    fn latency_estimate_reproduces_state_a() {
        // State (a): the engine reported E_a = 55.8 ms with 3 periods (30 ms) as the system
        // path, so our own part is 25.8 ms; the measured release jump M_a was 58.0 ms.
        let our_a = (25.8_f64 * f64::from(SAMPLE_RATE) / 1000.0).round() as u64;
        let ms = frames_to_ms(our_a + system_path_frames(441));
        assert!((ms - 58.0).abs() <= 1.0, "{ms} ms");
    }

    #[test]
    fn latency_frames_adds_the_system_path() {
        assert_eq!(
            latency_frames(441, 128, 1_000, 200, 128, 441),
            1_897 + system_path_frames(441)
        );
        assert_eq!(
            latency_frames(1, 2, 3, 4, 5, 441),
            15 + system_path_frames(441)
        );
        // No sink (period 0): nothing is added.
        assert_eq!(latency_frames(441, 128, 1_000, 200, 128, 0), 1_897);
    }

    #[test]
    fn trim_policy_drops_only_after_one_second_over() {
        // Target 1 000 frames; the margin is 20 ms = 882 frames.
        let mut t = TrimPolicy::new(1_000);
        let over = 1_000 + 882 + 100;
        // At or below target + margin never trims, however long it lasts.
        for ms in 0..3_000u64 {
            assert_eq!(t.observe(1_882, ms * 1_000), None);
        }
        // Over for just under a second: nothing yet.
        let start = 10_000_000;
        for ms in 0..1_000u64 {
            assert_eq!(t.observe(over, start + ms * 1_000), None, "{ms} ms");
        }
        // One full second over: drop to the target.
        assert_eq!(t.observe(over, start + 1_000_000), Some(over - 1_000));
        // The timer restarts after a trim.
        assert_eq!(t.observe(over, start + 1_001_000), None);

        // A dip back under the threshold restarts the second.
        let s2 = 20_000_000;
        assert_eq!(t.observe(1_000, s2 - 1), None);
        assert_eq!(t.observe(over, s2), None);
        assert_eq!(t.observe(1_500, s2 + 600_000), None);
        assert_eq!(t.observe(over, s2 + 700_000), None);
        assert_eq!(t.observe(over, s2 + 1_600_000), None);
        assert_eq!(t.observe(over, s2 + 1_700_000), Some(over - 1_000));

        // The drop never exceeds the smallest queue seen while over (bursty queues).
        let s3 = 30_000_000;
        assert_eq!(t.observe(1_000, s3 - 1), None);
        assert_eq!(t.observe(5_000, s3), None);
        assert_eq!(t.observe(2_500, s3 + 500_000), None);
        assert_eq!(t.observe(6_000, s3 + 1_000_000), Some(1_500));
    }

    const SEEN: Starvation = Starvation {
        ran_model: true,
        backlog_frames: 300,
        warming: false,
        force: false,
    };

    #[test]
    fn starvation_after_input_stops_is_not_an_underrun() {
        let mut j = UnderrunJudge::new();
        let t = 10_000_000;
        // Player paused: last packet 15 ms before the queue ran dry, none after.
        j.starved(t - 15_000, t, SEEN);
        assert_eq!(j.poll(t - 15_000, t + 10_000), None);
        assert_eq!(j.poll(t - 15_000, t + 51_000), None);
        // Playback resumes later: still not an underrun.
        assert_eq!(j.poll(t + 400_000, t + 400_000), None);
    }

    #[test]
    fn headroom_grows_by_a_hop_up_to_one_packet() {
        let cap = MAX_EXTRA_HEADROOM_FRAMES;
        assert_eq!(cap, 441, "about one 10 ms capture packet");
        let mut extra = 0;
        let mut steps = Vec::new();
        for _ in 0..6 {
            extra = grow_headroom(extra, 128, cap);
            steps.push(extra);
        }
        assert_eq!(steps, vec![128, 256, 384, 441, 441, 441]);
    }

    /// Worst-case standing latency with jitter headroom and pre-roll both at their limit:
    /// 10 ms capture packet + 1 hop in ring A + 128 model latency + (441 period + 128 hop +
    /// extra), where extra (jitter growth and every pre-roll together) is capped at 441.
    #[test]
    fn standing_latency_with_preroll_stays_within_budget() {
        let (packet, hop, model, period) = (441, 128, 128, 441);
        let cap = MAX_EXTRA_HEADROOM_FRAMES;
        // Grow the headroom by jitter, then accept "on" several times: extra never passes cap.
        let mut extra = 0;
        for _ in 0..2 {
            extra = grow_headroom(extra, hop, cap);
        }
        for _ in 0..5 {
            let base = period + hop + extra;
            let n = preroll_frames(false, base, base, hop, extra, cap);
            extra += n;
            assert!(extra <= cap, "{extra}");
        }
        assert_eq!(extra, cap);
        let worst = packet + hop + model + period + hop + extra;
        assert!(
            worst <= crate::audio::LATENCY_BUDGET_FRAMES,
            "{worst} frames = {} ms",
            worst * 1000 / 44_100
        );
    }

    #[test]
    fn preroll_never_stacks() {
        let (target, hop, cap) = (569, 128, MAX_EXTRA_HEADROOM_FRAMES);
        // First accepted "on": a full packet of pre-roll.
        assert_eq!(
            preroll_frames(false, target, target, hop, 0, cap),
            PREROLL_FRAMES
        );
        // A second "on" while that pre-roll is still being written adds nothing.
        assert_eq!(preroll_frames(true, target, target, hop, 0, cap), 0);
        // A later "on" while the queue still holds the first pre-roll adds nothing.
        assert_eq!(
            preroll_frames(false, target + PREROLL_FRAMES, target, hop, 0, cap),
            0
        );
        assert_eq!(preroll_frames(false, target + hop, target, hop, 0, cap), 0);
        // Headroom already used up: nothing; partly used: only the rest.
        assert_eq!(preroll_frames(false, target, target, hop, cap, cap), 0);
        assert_eq!(
            preroll_frames(false, target, target, hop, 300, cap),
            cap - 300
        );
    }

    #[test]
    fn decay_ramps_from_last_frame_to_zero() {
        let mut out = vec![9.0f32; 4 * 2];
        decay_from([0.8, -0.4], &mut out);
        assert_eq!(out, vec![0.6, -0.3, 0.4, -0.2, 0.2, -0.1, 0.0, 0.0]);
    }

    #[test]
    fn starvation_while_input_flows_counts_once() {
        let mut j = UnderrunJudge::new();
        let t = 10_000_000;
        j.starved(t - 4_000, t, SEEN);
        let other = Starvation {
            ran_model: false,
            backlog_frames: 0,
            warming: false,
            force: false,
        };
        j.starved(t - 4_000, t + 3_000, other); // same gap, not re-armed
        assert_eq!(j.poll(t - 4_000, t + 3_000), None);
        assert_eq!(
            j.poll(t + 6_000, t + 8_000),
            Some(SEEN),
            "next packet arrived: input kept flowing"
        );
        assert_eq!(j.poll(t + 16_000, t + 18_000), None, "counted once");
        // Input already silent for > 50 ms when the queue ran dry: never armed.
        let mut j = UnderrunJudge::new();
        j.starved(t - 51_000, t, SEEN);
        assert_eq!(j.poll(t + 5_000, t + 5_000), None);
        // A packet more than 50 ms after the starvation is a resume, not flowing input.
        let mut j = UnderrunJudge::new();
        j.starved(t - 4_000, t, SEEN);
        assert_eq!(j.poll(t + 51_000, t + 51_000), None);
    }

    #[test]
    fn trim_policy_restarts_on_target_change() {
        let mut t = TrimPolicy::new(1_000);
        assert_eq!(t.observe(5_000, 0), None);
        t.set_target(1_200);
        assert_eq!(t.observe(5_000, 1_000_000), None, "timer restarted");
        assert_eq!(t.observe(5_000, 2_000_000), Some(3_800));
    }

    fn left(b: &[f32]) -> Vec<f32> {
        b.iter().step_by(2).copied().collect()
    }

    #[test]
    fn gap_fader_fades_out_before_marker_across_writes() {
        let (mut tx, mut rx) = RingBuffer::<u64>::new(8);
        let mut g = GapFader::new(5);
        tx.push(108).unwrap();
        // Write [100, 105): the fade covers [103, 108).
        let mut a = vec![1.0f32; 5 * 2];
        g.apply(&mut rx, 100, &mut a);
        assert_eq!(left(&a), vec![1.0, 1.0, 1.0, 1.0, 0.75]);
        let mut b = vec![1.0f32; 5 * 2];
        g.apply(&mut rx, 105, &mut b);
        assert_eq!(left(&b), vec![0.5, 0.25, 0.0, 1.0, 1.0]);
        assert_eq!(b[5], 0.0, "right channel too");
    }

    #[test]
    fn gap_fader_shortens_fade_to_unwritten_frames_and_drops_stale() {
        let (mut tx, mut rx) = RingBuffer::<u64>::new(8);
        let mut g = GapFader::new(5);
        tx.push(50).unwrap(); // already behind the write position: stale
        tx.push(102).unwrap(); // only 2 unwritten frames before it
        let mut a = vec![1.0f32; 4 * 2];
        g.apply(&mut rx, 100, &mut a);
        assert_eq!(left(&a), vec![1.0, 0.0, 1.0, 1.0]);
        assert!(rx.is_empty());
    }

    #[test]
    fn finish_block_ramps_gain_and_clamps() {
        let mut b = vec![0.5f32, 0.5, 0.5, 0.5, 4.0, -4.0, f32::NAN, 0.5];
        finish_block(&mut b, 0.0, 1.0);
        assert_eq!(&b[..4], &[0.125, 0.125, 0.25, 0.25]);
        assert_eq!(&b[4..6], &[1.0, -1.0], "clamped to [-1, 1]");
        assert_eq!(b[6], 0.0, "non-finite becomes silence");
        assert_eq!(b[7], 0.5);
    }

    // ----- Ruling 24: hiccup simulation (fallback-investigation.md) -----

    /// One run of [`hiccup_sim`]: attach with devocal on at t = 0 (the "on" with its
    /// pre-roll request before the first fill), model-paced hops, injected hiccups.
    #[derive(Clone, Debug)]
    struct HiccupCfg {
        /// Capture packet phase within the 10 ms device period, us.
        phase: u64,
        len_us: u64,
        /// Model hop time, us (passthrough hops take 300 us).
        hop_us: u64,
        /// The first model hop starting at or after `.0` takes `.1` us longer.
        spikes: Vec<(u64, u64)>,
        /// The first `.0` model hops take `.1` us each (cold model after idle).
        cold: Option<(usize, u64)>,
    }

    impl HiccupCfg {
        fn new(phase: u64, len_us: u64) -> Self {
            Self {
                phase,
                len_us,
                hop_us: 1_000,
                spikes: Vec::new(),
                // Measured after 14 s idle: 1.95 / 1.56 / 1.41 / 1.31 ms.
                cold: Some((4, 1_600)),
            }
        }
    }

    #[derive(Default, Debug)]
    struct HiccupOut {
        /// (time, frames) of every pad.
        pads: Vec<(u64, u64)>,
        /// (time counted, snapshot published with it, stage at the count).
        underruns: Vec<(u64, Starvation, Stage)>,
        /// Device reads that found less than a period.
        starved: u64,
        max_jitter: usize,
        /// The detail published with each counted underrun (ruling 24 log).
        details: Vec<StarvationDetail>,
    }

    /// Event-driven model of capture (441 frames per 10 ms at `phase`), the processing thread
    /// (one hop at a time, publishing ring A's backlog after each pop like `pop_block`, the
    /// stage after each block) and the real `Renderer` in deadline mode on a fake 441-frame
    /// device (read every 10 ms, deadline 1.5 ms before it). Stages as the processor runs
    /// them: `WarmingUp` 69 blocks, `FadingIn` 7, then `Devocal`. Deterministic.
    fn hiccup_sim(cfg: &HiccupCfg) -> HiccupOut {
        const PERIOD: usize = 441;
        const PERIOD_US: u64 = 10_000;
        const HOP: usize = 128;
        const T0: u64 = 10 * SEC;
        let sink = FakeSink::new(0);
        let padding = sink.padding.clone();
        let (mut r, shared, mut tx) = test_renderer(Some(sink));
        r.after_open();
        r.deadline_mode = true;
        let publish = |r: &Renderer, stage: Stage| {
            r.ctx
                .stats
                .stage
                .store(crate::audio::stage_code(stage), Ordering::Release);
        };
        let (mut stage, mut blocks_left) = (Stage::WarmingUp, 69usize);
        publish(&r, stage);
        shared.preroll_request.store(true, Ordering::Release);
        let mut spikes = cfg.spikes.clone();
        spikes.sort_unstable();
        let mut o = HiccupOut::default();
        let (mut next_read, mut next_capture) = (0u64, cfg.phase);
        let (mut ring_a, mut model_hops, mut pushed) = (0usize, 0usize, 0u64);
        let (mut busy_until, mut deadline): (Option<u64>, Option<u64>) = (None, None);
        let mut start_hop = |t: u64, stage: Stage, ring_a: &mut usize| -> Option<u64> {
            *ring_a -= HOP;
            shared
                .in_ring_frames
                .store(*ring_a as u32, Ordering::Release);
            let mut d = if stage_runs_model(stage) {
                model_hops += 1;
                match cfg.cold {
                    Some((n, us)) if model_hops <= n => us,
                    _ => cfg.hop_us,
                }
            } else {
                300
            };
            if spikes.first().is_some_and(|&(at, _)| t >= at) {
                d += spikes.remove(0).1;
            }
            Some(t + d)
        };
        while next_read < cfg.len_us {
            let candidates = [Some(next_read), Some(next_capture), busy_until, deadline];
            let (which, t) = candidates
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.map(|t| (i, t)))
                .min_by_key(|&(i, t)| (t, i))
                .unwrap();
            let wake = match which {
                0 => {
                    if t > 0 {
                        let p = padding.load(Ordering::Relaxed);
                        if p < PERIOD {
                            o.starved += 1;
                        }
                        padding.store(p.saturating_sub(PERIOD), Ordering::Relaxed);
                    }
                    deadline = Some(t + PERIOD_US - DEADLINE_GUARD_US);
                    next_read = t + PERIOD_US;
                    Some(Wake::Device)
                }
                1 => {
                    ring_a += PERIOD;
                    shared
                        .in_ring_frames
                        .store(ring_a as u32, Ordering::Release);
                    shared.last_input_us.store(T0 + t, Ordering::Release);
                    if busy_until.is_none() && ring_a >= HOP {
                        busy_until = start_hop(t, stage, &mut ring_a);
                    }
                    next_capture += PERIOD_US;
                    None
                }
                2 => {
                    if blocks_left > 0 {
                        blocks_left -= 1;
                        if blocks_left == 0 && stage == Stage::WarmingUp {
                            stage = Stage::FadingIn;
                            blocks_left = 7;
                        } else if blocks_left == 0 {
                            stage = Stage::Devocal;
                        }
                    }
                    publish(&r, stage);
                    // Music-like content: a quiet stretch now and then (a decay decides early).
                    let v = if (pushed / HOP as u64) % 24 < 4 {
                        0.0
                    } else {
                        0.5
                    };
                    for _ in 0..HOP * 2 {
                        tx.push(v).unwrap();
                    }
                    pushed += HOP as u64;
                    busy_until = if ring_a >= HOP {
                        start_hop(t, stage, &mut ring_a)
                    } else {
                        shared
                            .in_ring_frames
                            .store(ring_a as u32, Ordering::Release);
                        None
                    };
                    Some(Wake::Data)
                }
                _ => {
                    deadline = None;
                    Some(Wake::Deadline)
                }
            };
            let Some(w) = wake else { continue };
            let (pads, pad_fr, un) = (
                pad_events(&r),
                r.ctx.stats.diag.render_pad_frames.load(Ordering::Relaxed),
                underruns(&r),
            );
            r.fill_at(w, T0 + t).unwrap();
            if pad_events(&r) > pads {
                let f = r.ctx.stats.diag.render_pad_frames.load(Ordering::Relaxed) - pad_fr;
                o.pads.push((t, f));
            }
            if underruns(&r) > un {
                let s = crate::audio::unpack_starvation(
                    shared.underrun_snapshot.load(Ordering::Acquire),
                );
                o.underruns.push((t, s, stage));
                o.details.push(shared.fallback_log.detail());
            }
            o.max_jitter = o.max_jitter.max(r.headroom.jitter);
        }
        o
    }

    /// Whether the processing thread forces `Fallback(Overload)` for this counted underrun
    /// (user on, model running, no load overload), at processor `stage`.
    fn forces_fallback(s: Starvation, stage: Stage) -> bool {
        super::super::processing::should_force_fallback(Some(s), true, true, false, stage)
    }

    fn forced(o: &HiccupOut) -> Vec<u64> {
        o.underruns
            .iter()
            .filter(|(_, s, stage)| forces_fallback(*s, *stage))
            .map(|&(t, _, _)| t)
            .collect()
    }

    /// Ruling 24, case b-4 (run 2): a 2 ms slow hop 100 ms into the warm-up leaves the check
    /// 58 frames short with ring A holding 371: pad 627 (target 1010), one underrun with a
    /// backlog. It must not force the fallback (never from `WarmingUp`).
    #[test]
    fn warm_up_hiccup_b4_signature_does_not_fall_back() {
        let mut c = HiccupCfg::new(6_000, SEC);
        c.spikes = vec![(100 * MS, 2_000)];
        let o = hiccup_sim(&c);
        assert_eq!(o.pads, vec![(108_500, 627)], "the b-4 signature: {o:?}");
        assert_eq!(o.underruns.len(), 1, "{o:?}");
        let (_, s, _) = o.underruns[0];
        assert!(s.ran_model && s.backlog_frames >= 128, "{s:?}");
        assert!(forced(&o).is_empty(), "{o:?}");
    }

    /// Ruling 24, cases c-2 (run 1) and c-1 (run 2): in steady devocal one 2 ms slow hop
    /// leaves a single pad of 220 / 144 frames and one backlogged underrun. It is treated as
    /// jitter: no fallback, the headroom grows by a hop, nothing more is padded.
    #[test]
    fn steady_hiccup_c2_c1_signatures_do_not_fall_back() {
        for (phase, at, pad) in [(4_750, 3_360 * MS, 219), (3_750, 3 * SEC, 144)] {
            let mut c = HiccupCfg::new(phase, 4 * SEC);
            c.spikes = vec![(at, 2_000)];
            let o = hiccup_sim(&c);
            let after: Vec<_> = o.pads.iter().filter(|p| p.0 >= 2 * SEC).collect();
            assert_eq!(after.len(), 1, "phase {phase}: {o:?}");
            assert!(after[0].1.abs_diff(pad) <= 2, "phase {phase}: {after:?}");
            let late: Vec<_> = o.underruns.iter().filter(|u| u.0 >= 2 * SEC).collect();
            assert_eq!(late.len(), 1, "phase {phase}: {o:?}");
            assert!(late[0].1.backlog_frames >= 128, "phase {phase}: {late:?}");
            assert!(forced(&o).is_empty(), "phase {phase}: {o:?}");
            assert_eq!(o.max_jitter, 128, "phase {phase}: grown as jitter");
        }
    }

    /// Ruling 24: a second backlogged underrun within `OVERLOAD_REPEAT_US` forces the
    /// fallback; the first one alone does not. (The first one's pad and headroom growth absorb
    /// a second 5 or 10 ms hiccup 2 s later, so the second here is 15 ms.)
    #[test]
    fn a_second_backlogged_hiccup_within_10s_falls_back() {
        let mut c = HiccupCfg::new(3_750, 6 * SEC);
        c.spikes = vec![(3 * SEC, 5_000), (5 * SEC, 15_000)];
        let o = hiccup_sim(&c);
        let late: Vec<_> = o.underruns.iter().filter(|u| u.0 >= 2 * SEC).collect();
        assert_eq!(late.len(), 2, "{o:?}");
        assert!(late.iter().all(|u| u.1.backlog_frames >= 128), "{late:?}");
        let f = forced(&o);
        assert_eq!(f, vec![late[1].0], "only the second forces: {o:?}");
    }

    /// Ruling 24: a model that cannot keep up (every hop 3.2 ms against 2.9 ms of audio)
    /// still reaches the fallback quickly through repeated underruns: within 1.5 s of the
    /// "on", in every capture phase (the LoadMonitor's 1 s window would follow anyway).
    #[test]
    fn a_slow_model_still_falls_back_within_1_5s() {
        for phase in (0..10_000).step_by(1_000) {
            let mut c = HiccupCfg::new(phase, 3 * SEC);
            c.hop_us = 3_200;
            c.cold = None;
            let o = hiccup_sim(&c);
            let f = forced(&o);
            assert!(
                f.first().is_some_and(|&t| t < 1_500 * MS),
                "phase {phase}: {f:?} {o:?}"
            );
        }
    }

    /// Ruling 24: never from `WarmingUp` (its output is the passthrough): a very cold model
    /// (8 hops of 6 ms) underruns during the warm-up without forcing anything there.
    #[test]
    fn warm_up_underruns_never_force() {
        for phase in (0..10_000).step_by(1_000) {
            let mut c = HiccupCfg::new(phase, SEC);
            c.cold = Some((8, 6_000));
            let o = hiccup_sim(&c);
            let warm: Vec<_> = o
                .underruns
                .iter()
                .filter(|u| u.2 == Stage::WarmingUp)
                .collect();
            assert!(!warm.is_empty(), "phase {phase}: not vacuous: {o:?}");
            assert!(
                warm.iter().all(|u| !forces_fallback(u.1, u.2)),
                "phase {phase}: {warm:?}"
            );
        }
    }

    /// Ruling 24: the overload judgement on its own.
    #[test]
    fn overload_judge_forces_only_a_repeat_or_at_cap() {
        let behind = Starvation {
            ran_model: true,
            backlog_frames: 371,
            warming: false,
            force: false,
        };
        let kept_up = Starvation {
            backlog_frames: 127,
            ..behind
        };
        let warming = Starvation {
            warming: true,
            ..behind
        };
        let idle = Starvation {
            ran_model: false,
            ..behind
        };
        let t = 100 * SEC;
        let mut j = OverloadJudge::new();
        assert_eq!(j.judge(kept_up, 128, false, t), Verdict::Jitter);
        assert_eq!(j.judge(idle, 128, false, t), Verdict::Jitter);
        assert_eq!(j.judge(behind, 128, false, t), Verdict::Isolated);
        assert_eq!(
            j.judge(behind, 128, false, t + OVERLOAD_REPEAT_US - 1),
            Verdict::Repeated
        );
        // Long after the last one: a hiccup again.
        let t2 = t + 3 * OVERLOAD_REPEAT_US;
        assert_eq!(j.judge(behind, 128, false, t2), Verdict::Isolated);
        assert_eq!(
            j.judge(behind, 128, false, t2 + OVERLOAD_REPEAT_US),
            Verdict::Isolated,
            "exactly the window apart is not a repeat"
        );
        // Jitter headroom at its cap: the first one forces.
        let mut j = OverloadJudge::new();
        assert_eq!(j.judge(behind, 128, true, t), Verdict::AtCap);
        // Warm-up never forces, even repeated, but counts as the earlier one.
        let mut j = OverloadJudge::new();
        assert_eq!(j.judge(warming, 128, true, t), Verdict::WarmUp);
        assert_eq!(j.judge(warming, 128, false, t + MS), Verdict::WarmUp);
        assert_eq!(j.judge(behind, 128, false, t + SEC), Verdict::Repeated);
        j.reset();
        assert_eq!(j.judge(behind, 128, false, t + 2 * SEC), Verdict::Isolated);
        assert!(Verdict::Repeated.forces() && Verdict::AtCap.forces());
        for v in [Verdict::Jitter, Verdict::WarmUp, Verdict::Isolated] {
            assert!(!v.forces(), "{v:?}");
        }
        assert_eq!(Verdict::name(Verdict::Repeated.code()), "repeated");
        assert_eq!(Verdict::name(99), "?");
    }

    /// Ruling 24 log: a counted underrun publishes what the check saw, with its verdict.
    #[test]
    fn a_counted_underrun_publishes_its_detail() {
        let mut c = HiccupCfg::new(3_750, 4 * SEC);
        c.spikes = vec![(3 * SEC, 2_000)];
        let o = hiccup_sim(&c);
        assert_eq!(o.details.len(), 1, "{o:?}");
        let d = o.details[0];
        assert_eq!(d.verdict, Verdict::Isolated.code());
        assert_eq!(d.wake, Wake::Deadline as u64);
        assert_eq!(d.padding + d.ring_b + d.shortfall, 441, "{d:?}");
        assert!(d.shortfall > 0 && d.since_capture_us < 10_000, "{d:?}");
        // The line the engine logs from it.
        let log = crate::audio::FallbackLog::default();
        let d = StarvationDetail {
            at_us: 5_000,
            wake: Wake::Deadline as u64,
            padding: 300,
            ring_b: 0,
            shortfall: 18,
            since_capture_us: 700,
            since_push_us: 2_100,
            block_busy_us: 1_400,
            check_late_us: 30,
            headroom: 0,
            verdict: Verdict::Repeated.code(),
        };
        log.set_detail(d);
        assert_eq!(log.detail(), d);
        log.trigger.store(1, Ordering::Relaxed);
        log.forced_at_us.store(9_000, Ordering::Relaxed);
        log.load_milli.store(356, Ordering::Relaxed);
        log.stage.store(4, Ordering::Relaxed);
        log.retry_armed.store(true, Ordering::Relaxed);
        let l = crate::audio::fallback_line(3, &log, 59_000);
        assert_eq!(
            l,
            "devocal audio: run=3 forced fallback (overload) 50 ms ago: trigger=underrun \
             load=0.356 stage=4 retry=scheduled | starved 4 ms before: verdict=repeated \
             wake=deadline padding=300 ring_b=0 shortfall=18 since_capture_us=700 \
             since_push_us=2100 block_busy_us=1400 check_late_us=30 headroom=0"
        );
        log.trigger.store(2, Ordering::Relaxed);
        let l = crate::audio::fallback_line(3, &log, 59_000);
        assert!(l.contains("trigger=load") && !l.contains("starved"), "{l}");
    }
}
