//! Capture thread: process loopback of the player's process tree (event driven, shared mode,
//! autoconvert to 44.1 kHz float32 stereo). Every packet is multiplied by the capture gain,
//! passed through the safety guard and pushed to ring A; packet discontinuities and ring
//! overflows push a reset marker first.
//!
//! Capture gain per packet: the conservative gain (`SharedGains::capture_gain`, the Holder's
//! 100 ms `GainHistory`), raised during the attach by R2 ([`LevelConfirm`]): each
//! [`CONFIRM_FRAMES`] chunk (about 2.5 ms) whose raw level, compared with the reference
//! recorded before the attach, matches a volume step already issued (within +-6 dB) gets that
//! step's make-up gain, capped so the chunk's RMS and peak stay at or below the reference's.
//! R2 falls back to the conservative gain for the whole attach when the reference is shorter
//! than 20 ms or below -60 dBFS (silence, a paused player); per chunk when the level matches
//! no issued step; and from the moment the Holder stops publishing the ramp (degraded, a
//! `follow`, a device change, 150 ms after the ramp start) or 150 ms after the window opened.
//! A conservative gain of 0 (device-change mute) is never raised. The safety guard still
//! judges every 10 ms block after the gain. Confirmed and fallen-back chunks are counted in
//! `DiagCounters::{capture_confirm_chunks, capture_fallback_chunks}`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use rtrb::Producer;
use wasapi::{AudioClient, Direction, SampleType, StreamMode, WasapiError, WaveFormat};

use super::confirm::{LevelConfirm, CONFIRM_FRAMES};
use super::{
    edge_fade_frames, now_us, AudioStats, ComGuard, FadeIn, InputMarker, Mmcss, Shared, SharedGains,
};
use crate::dsp::{fade_edges, SAMPLE_RATE};

/// A block whose peak exceeds this after the capture gain was not attenuated as assumed.
pub const GUARD_PEAK: f32 = 1.5;
/// The guard judges (and silences) 10 ms at a time.
pub const GUARD_BLOCK_FRAMES: usize = 441;
/// Largest packet read in one call (1 s); a larger one is dropped as a discontinuity.
const MAX_PACKET_FRAMES: usize = SAMPLE_RATE as usize;
const WAIT_MS: u32 = 20;

/// Safety guard: if any sample is non-finite or its magnitude exceeds [`GUARD_PEAK`], the
/// whole block is set to silence and `true` is returned; otherwise the block is unchanged.
pub fn guard_block(block: &mut [f32]) -> bool {
    let trips = block.iter().any(|s| !s.is_finite() || s.abs() > GUARD_PEAK);
    if trips {
        block.fill(0.0);
    }
    trips
}

/// Applies the capture gain and the safety guard to packets, keeping the edge-fade state
/// across packets. No allocation.
pub(crate) struct Conditioner {
    in_gap: bool,
    fade_in: FadeIn,
    fade_frames: usize,
}

impl Conditioner {
    pub fn new() -> Self {
        let fade_frames = edge_fade_frames();
        Self {
            in_gap: false,
            fade_in: FadeIn::new(fade_frames),
            fade_frames,
        }
    }

    /// Multiplies `samples` (interleaved stereo) by `gain`, then guards each 10 ms block; see
    /// [`Conditioner::condition_with`].
    pub fn condition(
        &mut self,
        samples: &mut [f32],
        gain: f32,
        stats: &AudioStats,
        follow_now: &AtomicBool,
        gap_at: impl FnMut(usize),
    ) {
        self.condition_with(samples, |_| gain, stats, follow_now, gap_at);
    }

    /// Multiplies each [`CONFIRM_FRAMES`] chunk of `samples` (interleaved stereo; the last
    /// chunk may be shorter) by `gain_for(chunk)`, which sees the chunk's raw samples, then
    /// guards each 10 ms block. A silenced block counts in `stats.unattenuated_blocks` and
    /// sets `follow_now`. A gap is faded out over the 5 ms before it (`fade_edges`) when those
    /// frames are in this packet; when the gap starts at the packet's first frame, `gap_at(0)`
    /// is called so the render thread fades the already-queued frames. Audio after a gap
    /// fades in over 5 ms.
    pub fn condition_with<F: FnMut(&[f32]) -> f32>(
        &mut self,
        samples: &mut [f32],
        mut gain_for: F,
        stats: &AudioStats,
        follow_now: &AtomicBool,
        mut gap_at: impl FnMut(usize),
    ) {
        for chunk in samples.chunks_mut(CONFIRM_FRAMES * 2) {
            let gain = gain_for(chunk);
            for s in chunk.iter_mut() {
                *s *= gain;
            }
        }
        let frames = samples.len() / 2;
        let mut start = 0;
        while start < frames {
            let end = (start + GUARD_BLOCK_FRAMES).min(frames);
            let (before, rest) = samples.split_at_mut(start * 2);
            let block = &mut rest[..(end - start) * 2];
            if guard_block(block) {
                stats.unattenuated_blocks.fetch_add(1, Ordering::Relaxed);
                follow_now.store(true, Ordering::Release);
                if !self.in_gap {
                    if start == 0 {
                        gap_at(0);
                    } else {
                        let k = start.min(self.fade_frames);
                        fade_edges(&mut before[(start - k) * 2..], false, true, k);
                    }
                    self.in_gap = true;
                }
            } else {
                if self.in_gap {
                    self.in_gap = false;
                    self.fade_in.start(0);
                }
                self.fade_in.apply(block);
            }
            start = end;
        }
    }
}

/// Pushes as much of `pkt` (interleaved stereo) into ring A as fits; returns the number of
/// frames written.
pub(crate) fn push_frames(output: &mut Producer<f32>, pkt: &[f32]) -> usize {
    // `push_partial_slice` returns (pushed, remainder).
    let (pushed, _) = output.push_partial_slice(pkt);
    pushed.len() / 2
}

pub(crate) struct CaptureCtx {
    pub pid: u32,
    pub output: Producer<f32>,
    pub markers: Producer<InputMarker>,
    pub shared: Arc<Shared>,
    pub stats: Arc<AudioStats>,
    pub gains: Arc<SharedGains>,
    pub follow_now: Arc<AtomicBool>,
}

/// Thread body. Reports the open result on `ready`, then captures until stop or a device
/// error (which sets `capture_failed`).
pub(crate) fn run(ctx: CaptureCtx, ready: Sender<Result<(), String>>) {
    let _com = match ComGuard::init_mta() {
        Ok(c) => c,
        Err(e) => {
            ctx.stats.capture_failed.store(true, Ordering::Release);
            let _ = ready.send(Err(format!("capture: {e}")));
            return;
        }
    };
    let _mmcss = Mmcss::pro_audio("capture");
    // All COM objects live inside `capture` and are released before `_com` uninitialises.
    if let Err(e) = capture(ctx, &ready) {
        let _ = ready.send(Err(e));
    }
}

fn capture(mut ctx: CaptureCtx, ready: &Sender<Result<(), String>>) -> Result<(), String> {
    let fail = |stats: &AudioStats, e: String| {
        stats.capture_failed.store(true, Ordering::Release);
        e
    };
    // Note: activation waits for its completion callback without a timeout (vendored
    // wasapi); `AudioHandle::start` bounds the wait and `stop` detaches a stuck thread.
    let mut client = AudioClient::new_application_loopback_client(ctx.pid, true).map_err(|e| {
        fail(
            &ctx.stats,
            format!("process loopback for pid {}: {e}", ctx.pid),
        )
    })?;
    let fmt = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 2, None);
    client
        .initialize_client(
            &fmt,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            },
        )
        .map_err(|e| fail(&ctx.stats, format!("initialise loopback capture: {e}")))?;
    let event = client
        .set_get_eventhandle()
        .map_err(|e| fail(&ctx.stats, format!("capture event: {e}")))?;
    let cap = client
        .get_audiocaptureclient()
        .map_err(|e| fail(&ctx.stats, format!("capture client: {e}")))?;
    client
        .start_stream()
        .map_err(|e| fail(&ctx.stats, format!("start capture: {e}")))?;
    let _ = ready.send(Ok(()));

    let mut bytes = vec![0u8; MAX_PACKET_FRAMES * 8];
    let mut samples = vec![0.0f32; MAX_PACKET_FRAMES * 2];
    let mut conditioner = Conditioner::new();
    let mut confirm = LevelConfirm::new();
    let mut pushed: u64 = 0;
    let mut pending_reset = false;
    let mut error = None;

    'outer: while !ctx.shared.stop.load(Ordering::Acquire) {
        let _ = event.wait_for_event(WAIT_MS);
        loop {
            let frames = match cap.get_next_packet_size() {
                Ok(Some(n)) => n as usize,
                Ok(None) => 0,
                Err(e) => {
                    error = Some(format!("capture packet size: {e}"));
                    break 'outer;
                }
            };
            if frames == 0 {
                break;
            }
            let n = match cap.read_from_device(&mut bytes) {
                Ok((n, info)) => {
                    let d = &ctx.stats.diag;
                    d.capture_packets.fetch_add(1, Ordering::Relaxed);
                    if info.flags.data_discontinuity {
                        d.capture_flag_discontinuity.fetch_add(1, Ordering::Relaxed);
                    }
                    if info.flags.timestamp_error {
                        d.capture_flag_timestamp.fetch_add(1, Ordering::Relaxed);
                    }
                    if info.flags.data_discontinuity || info.flags.timestamp_error {
                        ctx.stats.discontinuities.fetch_add(1, Ordering::Relaxed);
                        pending_reset = true;
                    }
                    n as usize
                }
                Err(WasapiError::DataLengthTooShort { .. }) => {
                    // Oversized packet: read_from_device released (dropped) it.
                    ctx.stats.discontinuities.fetch_add(1, Ordering::Relaxed);
                    pending_reset = true;
                    continue;
                }
                Err(e) => {
                    error = Some(format!("capture read: {e}"));
                    break 'outer;
                }
            };
            if n == 0 {
                break;
            }
            let now = now_us();
            ctx.shared.last_input_us.store(now, Ordering::Release);
            ctx.shared
                .capture_packet_frames
                .store(n as u32, Ordering::Relaxed);
            let pkt = &mut samples[..n * 2];
            for (s, b) in pkt.iter_mut().zip(bytes.chunks_exact(4)) {
                *s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            // Markers go before the samples they refer to, so the processing thread sees
            // them no later than the data.
            if pending_reset && ctx.markers.push(InputMarker::Discontinuity(pushed)).is_ok() {
                pending_reset = false;
            }
            // R2: the ramp is read before the gain. The engine publishes the gain first, so
            // the gain read here is at least as new as the ramp (a device-change mute, gain 0,
            // is never paired with an older gain).
            confirm.observe(ctx.gains.attach(), now);
            let conservative = ctx.gains.capture_gain();
            let markers = &mut ctx.markers;
            conditioner.condition_with(
                pkt,
                |raw| confirm.chunk_gain(raw, conservative),
                &ctx.stats,
                &ctx.follow_now,
                |offset| {
                    let _ = markers.push(InputMarker::GuardGap(pushed + offset as u64));
                },
            );
            // Counted only now, after R2 recorded the packet's chunks: the engine starts the
            // attach once enough frames are counted, and they must all be in the reference.
            ctx.stats
                .diag
                .capture_frames
                .fetch_add(n as u64, Ordering::Relaxed);
            let (confirmed, fallback) = confirm.take_counts();
            if confirmed > 0 {
                ctx.stats
                    .diag
                    .capture_confirm_chunks
                    .fetch_add(confirmed, Ordering::Relaxed);
            }
            if fallback > 0 {
                ctx.stats
                    .diag
                    .capture_fallback_chunks
                    .fetch_add(fallback, Ordering::Relaxed);
            }
            let written_frames = push_frames(&mut ctx.output, pkt);
            pushed += written_frames as u64;
            if written_frames < n {
                ctx.stats
                    .diag
                    .capture_ring_overflow_frames
                    .fetch_add((n - written_frames) as u64, Ordering::Relaxed);
                // Ring A full (processing stalled): the rest is dropped.
                ctx.stats.discontinuities.fetch_add(1, Ordering::Relaxed);
                pending_reset = true;
            }
            ctx.shared.wake.set();
        }
    }
    let _ = client.stop_stream();
    // Release the capture client and the audio client before closing their event handle.
    drop(cap);
    drop(client);
    drop(event);
    match error {
        Some(e) => {
            ctx.stats.capture_failed.store(true, Ordering::Release);
            eprintln!("devocal audio: {e}");
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-machine regression (2026-10-03): every packet was counted as a ring A overflow,
    /// so a discontinuity marker reset the processor every 10 ms.
    #[test]
    fn push_frames_reports_the_frames_written() {
        let (mut tx, mut rx) = rtrb::RingBuffer::<f32>::new(8 * 2);
        let pkt: Vec<f32> = (0..5 * 2).map(|i| i as f32).collect();
        assert_eq!(push_frames(&mut tx, &pkt), 5, "everything fits");
        assert_eq!(push_frames(&mut tx, &pkt), 3, "only 3 frames of room left");
        assert_eq!(push_frames(&mut tx, &pkt), 0, "ring full");
        let mut out = vec![0.0f32; 8 * 2];
        rx.pop_entire_slice(&mut out).unwrap();
        assert_eq!(&out[..10], &pkt[..]);
        assert_eq!(&out[10..], &pkt[..6]);
    }

    #[test]
    fn guard_silences_unattenuated_block() {
        // Full-scale input that was not attenuated, multiplied by the 1e4 compensation gain.
        let mut block: Vec<f32> = (0..441 * 2)
            .map(|i| (i as f32 * 0.05).sin() * 1.0 * 1.0e4)
            .collect();
        assert!(guard_block(&mut block));
        assert!(block.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn guard_passes_normal_block_unchanged() {
        let original: Vec<f32> = (0..441 * 2)
            .map(|i| (i as f32 * 0.05).sin() * 0.9)
            .collect();
        let mut block = original.clone();
        assert!(!guard_block(&mut block));
        assert_eq!(block, original);
        // 1.5 itself is allowed (the render clamp handles anything above 1.0).
        let mut edge = vec![1.5f32, -1.5];
        assert!(!guard_block(&mut edge));
        assert_eq!(edge, vec![1.5, -1.5]);
    }

    #[test]
    fn guard_trips_on_non_finite() {
        let mut block = vec![0.1f32, f32::NAN, 0.2, 0.3];
        assert!(guard_block(&mut block));
        assert!(block.iter().all(|&s| s == 0.0));
        let mut block = vec![0.1f32, f32::INFINITY];
        assert!(guard_block(&mut block));
    }

    /// Ruling 14: 1.0-amplitude input x gain 1e4 -> silence, counted, follow requested.
    #[test]
    fn conditioner_silences_amplified_input_and_requests_follow() {
        let stats = AudioStats::default();
        let follow = AtomicBool::new(false);
        let mut c = Conditioner::new();
        let mut pkt: Vec<f32> = (0..441 * 2).map(|i| (i as f32 * 0.05).sin()).collect();
        let mut gaps = Vec::new();
        c.condition(&mut pkt, 1.0e4, &stats, &follow, |o| gaps.push(o));
        assert!(pkt.iter().all(|&s| s == 0.0));
        assert_eq!(stats.unattenuated_blocks.load(Ordering::Relaxed), 1);
        assert!(follow.load(Ordering::Acquire));
        assert_eq!(
            gaps,
            vec![0],
            "gap at the packet start is left to the render fade"
        );
    }

    #[test]
    fn conditioner_passes_normal_level_unchanged() {
        let stats = AudioStats::default();
        let follow = AtomicBool::new(false);
        let mut c = Conditioner::new();
        let original: Vec<f32> = (0..1000 * 2)
            .map(|i| (i as f32 * 0.05).sin() * 0.5)
            .collect();
        let mut pkt = original.clone();
        // Session at 1e-4 of an original 0.5: gain 5000 restores the level exactly.
        for s in pkt.iter_mut() {
            *s *= 1.0e-4;
        }
        let mut gaps = Vec::new();
        c.condition(&mut pkt, 1.0e4, &stats, &follow, |o| gaps.push(o));
        for (a, b) in pkt.iter().zip(&original) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
        assert_eq!(stats.unattenuated_blocks.load(Ordering::Relaxed), 0);
        assert!(!follow.load(Ordering::Acquire));
        assert!(gaps.is_empty());

        let mut plain = original.clone();
        c.condition(&mut plain, 1.0, &stats, &follow, |_| {});
        assert_eq!(plain, original, "unity gain at normal level is bit-exact");
    }

    #[test]
    fn conditioner_fades_around_a_gap_inside_a_packet() {
        let stats = AudioStats::default();
        let follow = AtomicBool::new(false);
        let mut c = Conditioner::new();
        // 3 guard blocks: normal, loud, normal.
        let n = GUARD_BLOCK_FRAMES;
        let mut pkt = vec![0.5f32; 3 * n * 2];
        for s in &mut pkt[n * 2..2 * n * 2] {
            *s = 3.0;
        }
        let mut gaps = Vec::new();
        c.condition(&mut pkt, 1.0, &stats, &follow, |o| gaps.push(o));
        assert!(gaps.is_empty(), "fade-out done in place");
        let left: Vec<f32> = pkt.iter().step_by(2).copied().collect();
        let fade = edge_fade_frames();
        // Fade-out ends at 0 on the last frame before the gap; earlier frames untouched.
        assert_eq!(left[n - 1], 0.0);
        assert_eq!(left[n - fade - 1], 0.5);
        assert!(left[n - fade / 2] > 0.0 && left[n - fade / 2] < 0.5);
        // The gap is silent.
        assert!(left[n..2 * n].iter().all(|&s| s == 0.0));
        // Fade-in starts at 0 and reaches full level after 5 ms.
        assert_eq!(left[2 * n], 0.0);
        assert!(left[2 * n + fade / 2] > 0.0 && left[2 * n + fade / 2] < 0.5);
        assert_eq!(left[2 * n + fade - 1], 0.5);
        assert_eq!(left[3 * n - 1], 0.5);
        assert_eq!(stats.unattenuated_blocks.load(Ordering::Relaxed), 1);
        assert!(left.iter().all(|s| s.abs() <= 0.5));
    }

    /// R2: one gain per `CONFIRM_FRAMES` chunk, each asked for with the chunk's raw samples;
    /// the safety guard still judges 441-frame blocks.
    #[test]
    fn conditioner_applies_a_gain_per_confirm_chunk() {
        let stats = AudioStats::default();
        let follow = AtomicBool::new(false);
        let mut c = Conditioner::new();
        let mut pkt = vec![0.01f32; 441 * 2];
        let mut asked = Vec::new();
        c.condition_with(
            &mut pkt,
            |raw| {
                assert!(raw.iter().all(|&s| s == 0.01), "raw, before any gain");
                asked.push(raw.len() / 2);
                asked.len() as f32
            },
            &stats,
            &follow,
            |_| {},
        );
        assert_eq!(
            asked,
            vec![110, 110, 110, 110, 1],
            "the last chunk is 1 frame"
        );
        for (i, f) in pkt.chunks(2).enumerate() {
            let want = 0.01 * (i / CONFIRM_FRAMES + 1) as f32;
            assert_eq!(f, [want, want], "frame {i}");
        }
        assert_eq!(stats.unattenuated_blocks.load(Ordering::Relaxed), 0);

        // A chunk raised too far trips the guard for its whole 441-frame block only.
        let mut pkt = vec![0.01f32; 882 * 2];
        let mut chunk = 0;
        c.condition_with(
            &mut pkt,
            |_| {
                chunk += 1;
                if chunk == 6 {
                    200.0
                } else {
                    1.0
                }
            },
            &stats,
            &follow,
            |_| {},
        );
        assert_eq!(stats.unattenuated_blocks.load(Ordering::Relaxed), 1);
        assert!(
            pkt[441 * 2..].iter().all(|&s| s == 0.0),
            "second block silenced"
        );
        let fade = edge_fade_frames();
        assert!(
            pkt[..(441 - fade) * 2].iter().all(|&s| s == 0.01),
            "first block kept (up to the fade-out before the gap)"
        );
        assert!(follow.load(Ordering::Acquire));
    }

    #[test]
    fn conditioner_fade_in_continues_into_next_packet() {
        let stats = AudioStats::default();
        let follow = AtomicBool::new(false);
        let mut c = Conditioner::new();
        let mut loud = vec![2.0f32; 100 * 2];
        c.condition(&mut loud, 1.0, &stats, &follow, |_| {});
        // Two short packets after the gap: the ramp carries across them.
        let mut a = vec![1.0f32; 100 * 2];
        let mut b = vec![1.0f32; 200 * 2];
        c.condition(&mut a, 1.0, &stats, &follow, |_| {});
        c.condition(&mut b, 1.0, &stats, &follow, |_| {});
        assert_eq!(a[0], 0.0);
        let denom = (edge_fade_frames() - 1) as f32;
        assert!((b[0] - 100.0 / denom).abs() < 1e-6);
        assert_eq!(b[(edge_fade_frames() - 100) * 2], 1.0);
    }
}
