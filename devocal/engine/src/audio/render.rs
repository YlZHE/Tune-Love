//! Render thread: plays ring B on the player's endpoint.
//!
//! - Endpoint mix format exactly 44 100 Hz float (stereo or more): `IAudioClient3` activated
//!   directly, `InitializeSharedAudioStream` at the minimum `GetSharedModeEnginePeriod`.
//! - Otherwise: wasapi `EventsShared { autoconvert: true, buffer_duration_hns: 0 }`.
//!
//! Queue target = one device period + one hop (+ jitter headroom), device padding plus ring B.
//! Each wake fills the device up to the target; if that leaves less than one period queued the
//! device would starve, so the shortfall up to the target is written as silence: the real
//! frames before it fade out (`fade_edges`), the audio after it fades in, and one underrun is
//! counted per gap if input kept flowing (`UnderrunJudge`). Ruling 18: an underrun while the
//! processing thread was not behind is jitter and raises the target by one hop (at most one
//! capture packet in total); when a user "on" is accepted the output pre-rolls once by about
//! one capture packet of silence. Both are released again (O1, [`Headroom`]): the pre-roll
//! once the model has warmed up, the jitter part after 5 s without an underrun (backing off
//! up to 60 s while jitter keeps returning); the released audio is skipped at a quiet spot
//! (at most 1 s later), and not at all if nothing extra is queued (paused). A queue more
//! than 20 ms over target for a whole second is trimmed back. Both skips fade out, skip and
//! fade in. Output gain is ramped across each write; every sample is clamped to
//! [-1, 1] (non-finite -> 0) before it reaches the device.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use rtrb::Consumer;
use wasapi::{
    AudioClient, AudioRenderClient, Device, Direction, Handle, SampleType, StreamMode, WaveFormat,
};
use windows::core::{GUID, HSTRING};
use windows::Win32::Media::Audio::{
    IAudioClient3, IAudioRenderClient, IAudioSessionControl, IMMDevice,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;

use super::endpoint::{find_render_device, low_latency_eligible, read_mix_format, CoTaskFormat};
use super::{
    count_underrun, edge_fade_frames, is_backlogged, now_us, pack_starvation, render_open_line,
    stage_from_code, stage_runs_model, AudioStats, ComGuard, FadeIn, Mmcss, OwnedEvent,
    RenderCommand, Shared, SharedGains, Starvation, INPUT_FLOWING_US, MAX_EXTRA_HEADROOM_FRAMES,
    PREROLL_FRAMES,
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
    /// looked like (model running, ring A backlog) and is returned if it counts.
    pub fn starved(&mut self, last_input_us: u64, now_us: u64, seen: Starvation) {
        if self.starved_at.is_none() && count_underrun(last_input_us, now_us) {
            self.starved_at = Some((now_us, seen));
        }
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
    /// Waits for the device event (bounded).
    fn wait(&self, timeout_ms: u32);
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
    fn wait(&self, timeout_ms: u32) {
        self.event.wait(timeout_ms);
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
    fn wait(&self, timeout_ms: u32) {
        let _ = self.event.wait_for_event(timeout_ms);
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
    primed: bool,
    underruns: UnderrunJudge,
    /// Jitter and pre-roll headroom on top of period + hop (ruling 18, O1).
    headroom: Headroom,
    /// Released headroom still queued, to be skipped at a quiet spot; since when.
    decay_drop: usize,
    decay_since_us: u64,
    /// Lowest queue (device padding + ring B) seen at any wake since `decay_since_us`
    /// (`usize::MAX` when no decay is pending).
    decay_low: usize,
    /// The pending `drop_pending` is a headroom decay (counted as `render_decay_frames`).
    drop_is_decay: bool,
    /// Exponential average of the mean square of the real frames written (`track_power`).
    recent_ms: f64,
    /// Pre-roll silence still to write, and whether its leading fade-out is still to do.
    preroll_left: usize,
    preroll_fade: bool,
    /// Last frame written (before the output gain), for `decay_from`.
    last_frame: [f32; 2],
    last_gain: f32,
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
            underruns: UnderrunJudge::new(),
            headroom: Headroom::new(),
            decay_drop: 0,
            decay_since_us: 0,
            decay_low: usize::MAX,
            drop_is_decay: false,
            recent_ms: 0.0,
            preroll_left: 0,
            preroll_fade: false,
            last_frame: [0.0; 2],
            last_gain: 0.0,
        }
    }

    fn run(&mut self) {
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
                Ok(padding) => self.publish(padding),
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
        self.underruns.reset();
        // A new stream starts without learned headroom (which includes any earlier pre-roll),
        // without a pre-roll in progress and without a pending decay; a pending request is
        // kept (see above).
        self.headroom.reset();
        self.end_decay();
        self.drop_is_decay = false;
        self.recent_ms = 0.0;
        self.ctx.stats.headroom_frames.store(0, Ordering::Relaxed);
        self.preroll_left = 0;
        self.preroll_fade = false;
        self.last_frame = [0.0; 2];
        self.last_gain = 0.0;
    }

    /// No decay pending any more (done, cancelled, or a new stream).
    fn end_decay(&mut self) {
        self.decay_drop = 0;
        self.decay_low = usize::MAX;
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

    /// One wake: waits for the device event, then [`Self::fill_at`] the current time.
    fn fill(&mut self) -> Result<usize, String> {
        let Some(sink) = self.sink.as_ref() else {
            return Ok(0);
        };
        sink.wait(WAIT_MS);
        self.fill_at(now_us())
    }

    /// Fills the device at `now` (microseconds); returns the device padding after the write.
    fn fill_at(&mut self, now: u64) -> Result<usize, String> {
        let Some(sink) = self.sink.as_ref() else {
            return Ok(0);
        };
        self.ctx
            .stats
            .diag
            .render_wakes
            .fetch_add(1, Ordering::Relaxed);
        let padding = sink.padding()?;
        let period = sink.period_frames();
        let buffer = sink.buffer_frames().min(self.staging.len() / 2);
        let hop = self.ctx.shared.proc_hop.load(Ordering::Relaxed) as usize;
        let last_input = self.ctx.shared.last_input_us.load(Ordering::Acquire);
        if let Some(seen) = self.underruns.poll(last_input, now) {
            // Snapshot first, then the count (the processing thread reads them in that order).
            self.ctx
                .shared
                .underrun_snapshot
                .store(pack_starvation(seen), Ordering::Release);
            self.ctx.stats.underruns.fetch_add(1, Ordering::AcqRel);
            // Jitter (not a slow model) grows the headroom instead of a fallback; any counted
            // underrun restarts the quiet period before the headroom decays.
            let jitter = !is_backlogged(seen.ran_model, seen.backlog_frames, hop);
            self.headroom
                .on_underrun(jitter, hop, MAX_EXTRA_HEADROOM_FRAMES, now);
        }
        let mut avail = self.ctx.input.slots() / 2;
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
            }
            self.decay_drop += freed;
        }
        if self.decay_drop > 0 {
            // Every wake, written to or not: the low point is what is really spare.
            self.decay_low = self.decay_low.min(padding + avail);
        }
        self.ctx
            .stats
            .headroom_frames
            .store(self.headroom.total() as u32, Ordering::Relaxed);
        let target = (period + hop + self.headroom.total()).min(buffer);
        self.trim.set_target(target);

        if let Some(d) = self.trim.observe(avail + padding, now) {
            self.drop_pending = d;
            self.drop_is_decay = false;
        }
        let room = buffer.saturating_sub(padding);
        let need = target.saturating_sub(padding).min(room);
        let must = period.saturating_sub(padding).min(room);
        if need == 0 {
            return Ok(padding);
        }

        if self.preroll_left > 0 {
            // Pre-roll: fade out, then silence while input keeps filling ring B.
            let mut w = 0;
            if self.preroll_fade {
                let k = self.fade_frames.min(need).min(avail);
                self.take(0, k);
                fade_edges(&mut self.staging[..k * 2], false, true, k);
                self.preroll_fade = false;
                w = k;
            }
            self.staging[w * 2..need * 2].fill(0.0);
            if w == 0 {
                let f = self.fade_frames.min(need);
                decay_from(self.last_frame, &mut self.staging[..f * 2]);
            }
            self.ctx
                .stats
                .diag
                .render_preroll_frames
                .fetch_add((need - w) as u64, Ordering::Relaxed);
            self.preroll_left = self.preroll_left.saturating_sub(need - w);
            if self.preroll_left == 0 {
                self.fade_in.start(0);
            }
            return self.write_out(need, padding);
        }

        let waited = now.saturating_sub(self.decay_since_us);
        if self.decay_drop > 0 && self.drop_pending == 0 && waited >= DECAY_OBSERVE_US {
            // O1: what is really spare is the lowest queue since the release, beyond the
            // target. None (player paused, or a late packet ate it): nothing to skip.
            if self.decay_low <= target {
                self.end_decay();
            } else {
                let spare = self.decay_low - target;
                let window =
                    self.peek_rms((self.fade_frames + self.decay_drop.min(spare)).min(avail));
                let recent = self.recent_ms.sqrt() as f32;
                if decay_now(window, recent, waited) {
                    let d = self
                        .decay_drop
                        .min(spare)
                        .min(avail.saturating_sub(self.fade_frames));
                    if d > 0 {
                        self.drop_pending = d;
                        self.drop_is_decay = true;
                        self.end_decay();
                    } else if waited >= 2 * LOW_ENERGY_WAIT_US {
                        // Not one fade's worth queued on any wake for a second past the
                        // forced point: what is left (< 5 ms) stays.
                        self.end_decay();
                    }
                    // Otherwise too little in ring B for fade + skip now: retry next wake.
                }
            }
        }

        let mut w = 0;
        if self.drop_pending > 0 && avail > 0 {
            // Trim: fade out what plays next, skip the excess, fade the rest in. A decay
            // keeps its full fade even beyond `need` (frames move from ring B to the device;
            // the queue is the same), so small periods do not shorten it.
            let fade_room = if self.drop_is_decay { room } else { need };
            let k = self.fade_frames.min(fade_room).min(avail);
            self.take(0, k);
            fade_edges(&mut self.staging[..k * 2], false, true, k);
            avail -= k;
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
            self.drop_pending = 0;
            self.drop_is_decay = false;
            self.fade_in.start(0);
            w = k;
        }
        let real = need.saturating_sub(w).min(avail);
        self.take(w, real);
        w += real;
        let diag = &self.ctx.stats.diag;
        diag.render_real_frames
            .fetch_add(real as u64, Ordering::Relaxed);

        if w < must {
            diag.render_pad_events.fetch_add(1, Ordering::Relaxed);
            diag.render_pad_frames
                .fetch_add((need - w) as u64, Ordering::Relaxed);
            // The device would starve before the next wake: pad with silence up to the
            // target (ruling 18), not just one period.
            let k = w.min(self.fade_frames);
            fade_edges(&mut self.staging[(w - k) * 2..w * 2], false, true, k);
            self.staging[w * 2..need * 2].fill(0.0);
            if w == 0 {
                let f = self.fade_frames.min(need);
                decay_from(self.last_frame, &mut self.staging[..f * 2]);
            }
            if self.primed {
                let seen = Starvation {
                    ran_model: stage_runs_model(stage_from_code(
                        self.ctx.stats.stage.load(Ordering::Acquire),
                    )),
                    backlog_frames: self.ctx.shared.in_ring_frames.load(Ordering::Acquire) as usize,
                };
                self.underruns.starved(last_input, now, seen);
            }
            self.primed = false;
            self.fade_in.start(0);
            w = need;
        } else if real > 0 {
            self.primed = true;
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
            sh.in_ring_frames.load(Ordering::Relaxed) as u64,
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

/// Render periods the system adds on top of our own pipeline: the audio engine handing the
/// player's mix to process loopback, and our output being mixed again.
///
/// Calibrated on one real-machine measurement (2026-10-03, Folia, 10 ms render period):
/// 75.1 ms measured end to end against 42.9 ms estimated without it, 32.2 ms ~ 3 periods.
/// To be re-measured by the latency sub-project.
const SYSTEM_PATH_PERIODS: u64 = 3;

/// Estimated end-to-end added latency in frames: capture packet + ring A + ring B + device
/// padding + processor latency + [`SYSTEM_PATH_PERIODS`] render periods (`period` is 0 when
/// there is no sink).
fn latency_frames(
    capture_packet: u64,
    ring_a: u64,
    ring_b: u64,
    padding: u64,
    proc_latency: u64,
    period: u64,
) -> u64 {
    capture_packet + ring_a + ring_b + padding + proc_latency + SYSTEM_PATH_PERIODS * period
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
    }

    impl FakeSink {
        fn new(padding: usize) -> Self {
            Self {
                padding: Arc::new(AtomicUsize::new(padding)),
                out: Arc::new(Mutex::new(Vec::new())),
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
        fn wait(&self, _timeout_ms: u32) {}
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
            proc_latency_frames: AtomicU32::new(0),
            proc_hop: AtomicU32::new(128),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
            wake: OwnedEvent::new().unwrap(),
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
    /// judged); then the device plays one 441-frame period every 10 ms; then `fill_at` runs,
    /// every step or, with `period_wakes`, only on the device period (as the real event
    /// wakes it). Calls `each(r, now, first)` after every fill, `first` being the index in
    /// `out` of the first sample that fill wrote.
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
            r.fill_at(now).unwrap();
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
        r.fill_at(t).unwrap();
        assert_eq!(decay_frames(&r), 0);
        assert_eq!(r.decay_drop, 441, "kept pending");
        // Next wake: enough queued. The 31 spare frames go, behind a full 221-frame fade
        // although the wake needs only 69.
        padding.store(500, Ordering::Relaxed);
        push(&mut tx, 400);
        let first = out.lock().unwrap().len();
        r.fill_at(t + 10 * MS).unwrap();
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
        r.fill_at(t + 20 * MS).unwrap();
        assert_eq!(decay_frames(&r), 31);
        assert_eq!(r.decay_drop, 0, "given up");
    }

    #[test]
    fn latency_frames_adds_three_render_periods() {
        // 441-frame (10 ms) period: 3 x 441 = 1 323 frames on top of the pipeline terms.
        assert_eq!(
            latency_frames(441, 128, 1_000, 200, 128, 441),
            1_897 + 1_323
        );
        assert_eq!(latency_frames(1, 2, 3, 4, 5, 441), 15 + 1_323);
        // No sink: nothing is added.
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
}
