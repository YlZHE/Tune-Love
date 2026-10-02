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
//! one capture packet of silence. A queue more than 20 ms over target for a whole second is trimmed back (fade
//! out, skip, fade in). Output gain is ramped across each write; every sample is clamped to
//! [-1, 1] (non-finite -> 0) before it reaches the device.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use rtrb::Consumer;
use wasapi::{
    AudioClient, AudioRenderClient, Device, Direction, Handle, SampleType, StreamMode, WaveFormat,
};
use windows::Win32::Media::Audio::{
    IAudioClient3, IAudioRenderClient, IMMDevice, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;

use super::endpoint::{find_render_device, low_latency_eligible, read_mix_format, CoTaskFormat};
use super::{
    count_underrun, edge_fade_frames, is_backlogged, now_us, stage_from_code, stage_runs_model,
    AudioStats, ComGuard, FadeIn, Mmcss, OwnedEvent, RenderCommand, Shared, SharedGains,
    Starvation, INPUT_FLOWING_US, MAX_EXTRA_HEADROOM_FRAMES, PREROLL_FRAMES,
};
use crate::dsp::{fade_edges, frames_for_ms, SAMPLE_RATE};

/// Trim only when the queue exceeds the target by more than this...
pub const TRIM_MARGIN_MS: f32 = 20.0;
/// ...continuously for this long.
pub const TRIM_HOLD_US: u64 = 1_000_000;
const WAIT_MS: u32 = 20;
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
    /// Jitter headroom on top of period + hop (ruling 18).
    extra: usize,
    /// Pre-roll silence still to write, and whether its leading fade-out is still to do.
    preroll_left: usize,
    preroll_fade: bool,
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
            extra: 0,
            preroll_left: 0,
            preroll_fade: false,
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
    fn after_open(&mut self) {
        self.discard_all();
        if let Some(s) = &self.sink {
            eprintln!(
                "devocal audio: render {} (period {} frames, buffer {})",
                s.describe(),
                s.period_frames(),
                s.buffer_frames()
            );
            // Preallocated here, never on the per-block path.
            self.staging = vec![0.0; s.buffer_frames() * 2];
        }
        self.fade_in.start(0);
        self.trim.reset();
        self.drop_pending = 0;
        self.primed = false;
        self.underruns.reset();
        // A new device starts without learned headroom or a stale pre-roll.
        self.extra = 0;
        self.ctx.stats.headroom_frames.store(0, Ordering::Relaxed);
        self.preroll_left = 0;
        self.preroll_fade = false;
        self.ctx
            .shared
            .preroll_request
            .store(false, Ordering::Release);
        self.last_gain = 0.0;
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
        self.read_pos += n as u64;
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

    /// One wake: returns the device padding after the write.
    fn fill(&mut self) -> Result<usize, String> {
        let Some(sink) = self.sink.as_ref() else {
            return Ok(0);
        };
        sink.wait(WAIT_MS);
        let padding = sink.padding()?;
        let period = sink.period_frames();
        let buffer = sink.buffer_frames().min(self.staging.len() / 2);
        let hop = self.ctx.shared.proc_hop.load(Ordering::Relaxed) as usize;
        let now = now_us();
        let last_input = self.ctx.shared.last_input_us.load(Ordering::Acquire);
        if let Some(seen) = self.underruns.poll(last_input, now) {
            // Snapshot first, then the count (the processing thread reads them in that order).
            let sh = &self.ctx.shared;
            sh.underrun_ran_model
                .store(seen.ran_model, Ordering::Release);
            sh.underrun_backlog_frames
                .store(seen.backlog_frames as u32, Ordering::Release);
            self.ctx.stats.underruns.fetch_add(1, Ordering::AcqRel);
            if !is_backlogged(seen.ran_model, seen.backlog_frames, hop) {
                // Jitter, not a slow model: more headroom instead of a fallback.
                self.extra = grow_headroom(self.extra, hop, MAX_EXTRA_HEADROOM_FRAMES);
                self.ctx
                    .stats
                    .headroom_frames
                    .store(self.extra as u32, Ordering::Relaxed);
            }
        }
        let target = (period + hop + self.extra).min(buffer);
        self.trim.set_target(target);

        let mut avail = self.ctx.input.slots() / 2;
        if let Some(d) = self.trim.observe(avail + padding, now) {
            self.drop_pending = d;
        }
        let room = buffer.saturating_sub(padding);
        let need = target.saturating_sub(padding).min(room);
        let must = period.saturating_sub(padding).min(room);
        if need == 0 {
            return Ok(padding);
        }

        if self.preroll_left == 0
            && self
                .ctx
                .shared
                .preroll_request
                .swap(false, Ordering::AcqRel)
        {
            self.preroll_left = PREROLL_FRAMES;
            self.preroll_fade = true;
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
            self.preroll_left = self.preroll_left.saturating_sub(need - w);
            if self.preroll_left == 0 {
                self.fade_in.start(0);
            }
            return self.write_out(need, padding);
        }

        let mut w = 0;
        if self.drop_pending > 0 && avail > 0 {
            // Trim: fade out what plays next, skip the excess, fade the rest in.
            let k = self.fade_frames.min(need).min(avail);
            self.take(0, k);
            fade_edges(&mut self.staging[..k * 2], false, true, k);
            avail -= k;
            let d = self.drop_pending.min(avail);
            self.skip(d);
            avail -= d;
            self.drop_pending = 0;
            self.fade_in.start(0);
            w = k;
        }
        let real = (need - w).min(avail);
        self.take(w, real);
        w += real;

        if w < must {
            // The device would starve before the next wake: pad with silence up to the
            // target (ruling 18), not just one period.
            let k = w.min(self.fade_frames);
            fade_edges(&mut self.staging[(w - k) * 2..w * 2], false, true, k);
            self.staging[w * 2..need * 2].fill(0.0);
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
        // latency = capture period + ring A + ring B + device padding + model latency.
        let frames = sh.capture_packet_frames.load(Ordering::Relaxed) as u64
            + sh.in_ring_frames.load(Ordering::Relaxed) as u64
            + (self.ctx.input.slots() / 2) as u64
            + padding as u64
            + sh.proc_latency_frames.load(Ordering::Relaxed) as u64;
        let micros = frames * 1_000_000 / u64::from(SAMPLE_RATE);
        self.ctx
            .stats
            .latency_ms_milli
            .store(micros.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

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
        // Budget: 10 ms packet + 1 hop in ring A + 128 model + (441 period + 128 hop + 441)
        // stays under 50 ms at 44.1 kHz.
        let worst = 441 + 128 + 128 + 441 + 128 + cap;
        assert!(worst * 1000 / 44_100 < 50, "{worst} frames");
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
