//! R2: level-confirmed attach gain (capture thread).
//!
//! While the Holder lowers the player over its 4-step ramp, the conservative capture gain
//! (`GainHistory`, 100 ms window) stays near 1.0 for about 100 ms and leaves an audible hole.
//! [`LevelConfirm`] compares each [`CONFIRM_FRAMES`] chunk of raw (not yet gained) capture
//! against a reference recorded before the attach. When the chunk's level matches a step
//! that was issued (within [`CONFIRM_TOLERANCE_DB`]), the chunk gets that step's make-up
//! gain, capped so its RMS and peak stay at or below the reference's; otherwise it gets the
//! conservative gain. The result is never below the conservative gain, so R2 only ever
//! fills the hole and is never quieter than before.
//!
//! "Never louder" in R2: every chunk raised above the conservative gain comes out with an RMS
//! no higher than the reference RMS and a peak no higher than the reference peak; every other
//! chunk is exactly the conservative output, bounded by `GainHistory`.
//!
//! Fallback to the conservative gain:
//! - for the whole window: fewer than [`CONFIRM_MIN_REF_CHUNKS`] chunks' worth of reference
//!   (20 ms), or a reference RMS below [`CONFIRM_SILENCE_RMS`] (-60 dBFS; also a reference of
//!   exact zeros from a paused player);
//! - per chunk: the level matches no issued step, or the samples are not finite; a digital
//!   silence chunk keeps the conservative gain (nothing to judge);
//! - end of the window: the ramp is no longer published (`None`: degraded, a `follow`, a
//!   device change, or the Holder's 150 ms publish window), or [`CONFIRM_TIMEOUT_US`] after
//!   it opened; it does not reopen for the same epoch;
//! - a conservative gain of 0 (device-change mute) always wins.
//!
//! Fixed-size state only: no allocation, no locks.

use crate::holder::{ramp_value, AttachRamp, RAMP_STEPS};
use devocal_core::sessions::HELD_VOLUME;

/// Judging unit: about 2.5 ms.
pub const CONFIRM_FRAMES: usize = 110;
/// A chunk matches step `j` when its level is within this of the step's attenuation.
pub const CONFIRM_TOLERANCE_DB: f32 = 6.0;
/// Longest a confirmation window stays open after it opened.
pub const CONFIRM_TIMEOUT_US: u64 = 150_000;
/// Reference: the latest 20 judged-size chunks (about 50 ms) recorded before the attach.
pub const CONFIRM_REF_CHUNKS: usize = 20;
/// At least this many chunks' worth of reference (about 20 ms), counted in frames.
pub const CONFIRM_MIN_REF_CHUNKS: usize = 8;
/// A reference RMS below this (-60 dBFS) counts as silence.
pub const CONFIRM_SILENCE_RMS: f32 = 1.0e-3;

/// A chunk shorter than this (a packet's remainder, e.g. the 1 frame of 441 = 4 x 110 + 1)
/// is too short to judge: a frame near a zero crossing reads as a deeper step. It keeps the
/// step of the chunk before it in the window (still capped), or the conservative gain, and
/// it is not recorded as reference (it would take a whole ring slot).
const MIN_JUDGED_FRAMES: usize = CONFIRM_FRAMES / 2;

/// Relative margin below the RMS and peak caps (about -0.00001 dB).
const CAP_MARGIN: f32 = 1.0e-6;

/// The smallest issued step `j` in `0..=issued` (0 = not lowered yet) whose attenuation
/// `20 log10(ramp_value(original, HELD_VOLUME, j) / original)` is within `tol_db` of
/// `rel_db`: the largest volume, so the smallest gain. `None` if none is.
pub(crate) fn match_step(rel_db: f32, original: f32, issued: u32, tol_db: f32) -> Option<u32> {
    (0..=issued.min(RAMP_STEPS)).find(|&j| {
        let expected = 20.0 * (ramp_value(original, HELD_VOLUME, j) / original).log10();
        (rel_db - expected).abs() <= tol_db
    })
}

/// The frozen pre-attach reference.
#[derive(Debug, Clone, Copy)]
struct Reference {
    /// Mean square over all reference samples.
    energy: f64,
    peak: f32,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    opened_us: u64,
    steps: u32,
    original: f32,
    /// `None`: no usable reference, the whole window is conservative.
    reference: Option<Reference>,
    /// The step the latest judged chunk matched (`None`: none yet or a mismatch).
    last_step: Option<u32>,
}

/// Per-chunk R2 decision state (see the module docs). Owned by the capture thread.
pub(crate) struct LevelConfirm {
    /// Ring of reference chunks (at least [`MIN_JUDGED_FRAMES`] each): sum of squares,
    /// samples, peak.
    ref_sum_sq: [f64; CONFIRM_REF_CHUNKS],
    ref_samples: [usize; CONFIRM_REF_CHUNKS],
    ref_peak: [f32; CONFIRM_REF_CHUNKS],
    ref_next: usize,
    ref_len: usize,
    /// The latest `observe` saw no ramp: chunks outside a window go into the reference.
    recording: bool,
    /// Epoch of the latest window opened (a window never reopens for it).
    epoch: Option<u32>,
    window: Option<Window>,
    confirmed: u64,
    fallback: u64,
}

impl LevelConfirm {
    pub fn new() -> Self {
        Self {
            ref_sum_sq: [0.0; CONFIRM_REF_CHUNKS],
            ref_samples: [0; CONFIRM_REF_CHUNKS],
            ref_peak: [0.0; CONFIRM_REF_CHUNKS],
            ref_next: 0,
            ref_len: 0,
            recording: true,
            epoch: None,
            window: None,
            confirmed: 0,
            fallback: 0,
        }
    }

    /// Once per packet, before its chunks: the published ramp (`SharedGains::attach`) and the
    /// packet time. A new epoch opens a window and freezes the reference; `None` or the
    /// timeout ends it.
    pub fn observe(&mut self, ramp: Option<AttachRamp>, now_us: u64) {
        let Some(r) = ramp else {
            self.window = None;
            self.recording = true;
            return;
        };
        self.recording = false;
        if self.epoch != Some(r.epoch) {
            if r.steps == 0 {
                // Not a ramp of the open window's epoch, and nothing issued yet.
                self.window = None;
                return;
            }
            self.epoch = Some(r.epoch);
            self.window = Some(Window {
                opened_us: now_us,
                steps: r.steps,
                original: r.original,
                reference: self.freeze_reference(),
                last_step: None,
            });
        }
        if let Some(w) = self.window.as_mut() {
            if now_us.saturating_sub(w.opened_us) >= CONFIRM_TIMEOUT_US {
                self.window = None;
            } else {
                w.steps = r.steps;
                w.original = r.original;
            }
        }
    }

    /// The gain for one chunk of `raw` (interleaved stereo, not yet gained, at most
    /// [`CONFIRM_FRAMES`] frames); never below `conservative`, except that it is 0 when
    /// `conservative` is 0. Outside a window the chunk is recorded as reference while no
    /// ramp is published.
    pub fn chunk_gain(&mut self, raw: &[f32], conservative: f32) -> f32 {
        let (sum_sq, peak) = level(raw);
        let Some(w) = self.window.as_mut() else {
            if self.recording && raw.len() / 2 >= MIN_JUDGED_FRAMES {
                self.push_reference(sum_sq, raw.len(), peak);
            }
            return conservative;
        };
        if conservative <= 0.0 {
            return conservative;
        }
        let Some(reference) = w.reference else {
            self.fallback += 1;
            return conservative;
        };
        if raw.is_empty() || sum_sq == 0.0 {
            return conservative;
        }
        let e = sum_sq / raw.len() as f64;
        if !e.is_finite() {
            w.last_step = None;
            self.fallback += 1;
            return conservative;
        }
        let step = if raw.len() / 2 < MIN_JUDGED_FRAMES {
            w.last_step
        } else {
            let rel_db = (10.0 * (e / reference.energy).log10()) as f32;
            let j = match_step(rel_db, w.original, w.steps, CONFIRM_TOLERANCE_DB);
            w.last_step = j;
            j
        };
        let Some(j) = step else {
            self.fallback += 1;
            return conservative;
        };
        let step_gain = w.original / ramp_value(w.original, HELD_VOLUME, j);
        let rms_cap = (reference.energy / e).sqrt() as f32;
        let peak_cap = reference.peak / peak;
        // The margin absorbs f32 rounding, so a raised chunk is never above the reference.
        let raised = step_gain.min(rms_cap).min(peak_cap) * (1.0 - CAP_MARGIN);
        if raised > conservative {
            self.confirmed += 1;
            raised
        } else {
            conservative
        }
    }

    /// Chunks confirmed (raised above the conservative gain by a matched step) and fallen
    /// back (conservative inside a window: no usable reference, no match, non-finite) since
    /// the last call. A match whose capped gain is not above the conservative gain is
    /// neither.
    pub fn take_counts(&mut self) -> (u64, u64) {
        let counts = (self.confirmed, self.fallback);
        self.confirmed = 0;
        self.fallback = 0;
        counts
    }

    fn push_reference(&mut self, sum_sq: f64, samples: usize, peak: f32) {
        self.ref_sum_sq[self.ref_next] = sum_sq;
        self.ref_samples[self.ref_next] = samples;
        self.ref_peak[self.ref_next] = peak;
        self.ref_next = (self.ref_next + 1) % CONFIRM_REF_CHUNKS;
        self.ref_len = (self.ref_len + 1).min(CONFIRM_REF_CHUNKS);
    }

    /// The reference recorded so far, if usable, and an empty ring for the next one.
    fn freeze_reference(&mut self) -> Option<Reference> {
        let mut sum_sq = 0.0f64;
        let mut samples = 0usize;
        let mut peak = 0.0f32;
        for i in 0..self.ref_len {
            sum_sq += self.ref_sum_sq[i];
            samples += self.ref_samples[i];
            peak = peak.max(self.ref_peak[i]);
        }
        self.ref_len = 0;
        self.ref_next = 0;
        if samples < CONFIRM_MIN_REF_CHUNKS * CONFIRM_FRAMES * 2 {
            return None;
        }
        let energy = sum_sq / samples as f64;
        // Silence (including exact zeros), or non-finite samples in the reference.
        if !energy.is_finite()
            || energy.sqrt() < f64::from(CONFIRM_SILENCE_RMS)
            || !peak.is_finite()
        {
            return None;
        }
        Some(Reference { energy, peak })
    }
}

/// Sum of squares (f64) and peak magnitude of `raw`; NaN propagates into the sum.
fn level(raw: &[f32]) -> (f64, f32) {
    let mut sum_sq = 0.0f64;
    let mut peak = 0.0f32;
    for &s in raw {
        sum_sq += f64::from(s) * f64::from(s);
        peak = peak.max(s.abs());
    }
    (sum_sq, peak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::Conditioner;
    use crate::audio::AudioStats;
    use crate::dsp::GainHistory;
    use crate::holder::{ramp_value, AttachRamp, CONFIRM_WINDOW_US, GAIN_WINDOW_US, RAMP_STEP_US};
    use devocal_core::sessions::HELD_VOLUME;
    use std::sync::atomic::{AtomicBool, Ordering};

    const T0: u64 = 1_000_000;

    /// `frames` interleaved stereo frames of +a / -a: constant amplitude, peak equals RMS.
    fn alt(a: f32, frames: usize) -> Vec<f32> {
        (0..frames * 2)
            .map(|i| if i % 2 == 0 { a } else { -a })
            .collect()
    }

    fn ramp(steps: u32, original: f32) -> Option<AttachRamp> {
        Some(AttachRamp {
            epoch: 1,
            steps,
            original,
        })
    }

    /// Level of a session at `original` lowered to step `j`, relative to `a` at `original`.
    fn step_amp(a: f32, original: f32, j: u32) -> f32 {
        a * ramp_value(original, HELD_VOLUME, j) / original
    }

    fn db_amp(a: f32, db: f32) -> f32 {
        a * 10f32.powf(db / 20.0)
    }

    /// `chunks` reference chunks of `reference` recorded with no ramp, then a window opened
    /// at `T0` with `steps` issued.
    fn confirm_after(reference: &[f32], chunks: usize, steps: u32, original: f32) -> LevelConfirm {
        let mut c = LevelConfirm::new();
        c.observe(None, T0 - 60_000);
        for _ in 0..chunks {
            assert_eq!(c.chunk_gain(reference, 1.0), 1.0, "outside the window");
        }
        c.observe(ramp(steps, original), T0);
        c
    }

    fn confirm_with_reference(a: f32, chunks: usize, steps: u32, original: f32) -> LevelConfirm {
        confirm_after(&alt(a, CONFIRM_FRAMES), chunks, steps, original)
    }

    fn close(a: f32, b: f32) -> bool {
        (a / b - 1.0).abs() < 1e-4
    }

    #[test]
    fn match_step_picks_the_issued_step_within_6db() {
        // original 0.5: about -18.49 dB per step.
        assert_eq!(match_step(-18.0, 0.5, 2, 6.0), Some(1));
        assert_eq!(match_step(-37.0, 0.5, 2, 6.0), Some(2));
        assert_eq!(match_step(-37.0, 0.5, 1, 6.0), None, "step 2 not issued");
        assert_eq!(match_step(-28.0, 0.5, 2, 6.0), None);
        assert_eq!(match_step(0.0, 0.5, 2, 6.0), Some(0));
    }

    /// Review Focus 3: at a very low original the steps are about 5 dB apart and the windows
    /// overlap; the smallest step (largest volume, smallest gain) wins.
    #[test]
    fn overlapping_steps_pick_the_largest_volume() {
        assert_eq!(match_step(-5.0, 0.001, 4, 6.0), Some(0));
        assert_eq!(match_step(-9.0, 0.001, 4, 6.0), Some(1));
    }

    #[test]
    fn confirmed_chunk_gets_the_step_gain_capped_at_reference() {
        let mut c = confirm_with_reference(0.1, 20, 2, 0.5);
        let step_gain = 0.5 / ramp_value(0.5, HELD_VOLUME, 2);
        // Exactly step 2 (-36.99 dB): the step gain.
        let g = c.chunk_gain(&alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES), 1.0);
        assert!(close(g, step_gain), "{g} vs {step_gain}");
        // -31 dB is still within 6 dB of step 2, but the step gain would make it louder than
        // the reference: capped at sqrt(ref_e / e).
        let g = c.chunk_gain(&alt(db_amp(0.1, -31.0), CONFIRM_FRAMES), 1.0);
        let cap = 10f32.powf(31.0 / 20.0);
        assert!(close(g, cap), "{g} vs {cap}");
        assert!(g < step_gain);
        assert_eq!(c.take_counts(), (2, 0));
        assert_eq!(c.take_counts(), (0, 0), "taken");
    }

    #[test]
    fn a_few_loud_samples_are_capped_by_the_reference_peak() {
        let mut c = confirm_with_reference(0.1, 20, 2, 0.5);
        let mut chunk = alt(1.0, CONFIRM_FRAMES);
        let loud = [10usize, 101, 200];
        for &i in &loud {
            chunk[i] *= 10f32.powf(18.0 / 20.0);
        }
        // Scale the whole chunk to exactly step 2.
        let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
        let target = step_amp(0.1, 0.5, 2);
        for s in chunk.iter_mut() {
            *s *= target / rms;
        }
        let g = c.chunk_gain(&chunk, 1.0);
        assert!(g > 1.0, "confirmed: {g}");
        for &i in &loud {
            assert!((chunk[i] * g).abs() <= 0.1, "{}", chunk[i] * g);
        }
        assert_eq!(c.take_counts(), (1, 0));
    }

    /// Review Focus 1 (and the Task 8 carry: a paused player fills the reference with exact
    /// zeros): a reference below -60 dBFS keeps the whole window conservative.
    #[test]
    fn silent_reference_falls_back() {
        for (reference, chunk_amp) in [
            (alt(5.0e-4, CONFIRM_FRAMES), step_amp(5.0e-4, 0.5, 2)),
            (vec![0.0; CONFIRM_FRAMES * 2], step_amp(0.1, 0.5, 2)),
        ] {
            let mut c = confirm_after(&reference, 20, 2, 0.5);
            let chunk = alt(chunk_amp, CONFIRM_FRAMES);
            for k in 0..14 {
                c.observe(ramp(2, 0.5), T0 + k * 10_000);
                for _ in 0..4 {
                    assert_eq!(c.chunk_gain(&chunk, 3.0), 3.0);
                }
            }
            assert_eq!(c.take_counts().0, 0, "nothing confirmed");
        }
    }

    /// Review Focus 1: fewer than 8 reference chunks (20 ms) keeps the window conservative.
    #[test]
    fn short_reference_falls_back() {
        let chunk = alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES);
        let mut c = confirm_with_reference(0.1, 7, 2, 0.5);
        for k in 0..14 {
            c.observe(ramp(2, 0.5), T0 + k * 10_000);
            assert_eq!(c.chunk_gain(&chunk, 1.0), 1.0);
        }
        assert_eq!(c.take_counts().0, 0);
        // 8 chunks are enough.
        let mut c = confirm_with_reference(0.1, 8, 2, 0.5);
        assert!(c.chunk_gain(&chunk, 1.0) > 1.0);
    }

    #[test]
    fn timeout_after_150ms_falls_back() {
        let chunk = alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES);
        let mut c = confirm_with_reference(0.1, 20, 2, 0.5);
        c.observe(ramp(2, 0.5), T0 + CONFIRM_TIMEOUT_US - 1);
        assert!(c.chunk_gain(&chunk, 1.0) > 1.0);
        c.observe(ramp(2, 0.5), T0 + CONFIRM_TIMEOUT_US);
        assert_eq!(c.chunk_gain(&chunk, 1.0), 1.0);
        c.observe(ramp(3, 0.5), T0 + CONFIRM_TIMEOUT_US + 10_000);
        assert_eq!(c.chunk_gain(&chunk, 1.0), 1.0, "same epoch: stays over");
        assert_eq!(c.take_counts(), (1, 0), "a timeout is not a fallback");
    }

    #[test]
    fn mismatch_falls_back_per_chunk() {
        let mut c = confirm_with_reference(0.1, 20, 2, 0.5);
        assert_eq!(
            c.chunk_gain(&alt(db_amp(0.1, -28.0), CONFIRM_FRAMES), 1.5),
            1.5
        );
        let g = c.chunk_gain(&alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES), 1.5);
        assert!(g > 1.5, "{g}");
        assert_eq!(c.take_counts(), (1, 1));
    }

    /// `r2_ok` counts only chunks R2 actually raised: a match whose capped gain is not above
    /// the conservative gain returns the conservative gain and is neither count.
    #[test]
    fn a_match_that_is_not_raised_is_not_counted() {
        let mut c = confirm_with_reference(0.1, 20, 2, 0.5);
        // Not lowered yet (step 0, gain 1).
        assert_eq!(c.chunk_gain(&alt(0.1, CONFIRM_FRAMES), 1.0), 1.0);
        // Step 2 matched, but the conservative gain is already higher.
        let chunk = alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES);
        assert_eq!(c.chunk_gain(&chunk, 100.0), 100.0);
        assert_eq!(c.take_counts(), (0, 0));
        assert!(c.chunk_gain(&chunk, 1.0) > 1.0);
        assert_eq!(c.take_counts(), (1, 0));
    }

    /// Packet remainders (1 frame of 441) are not recorded, so they cannot push the full
    /// chunks out of the 20-slot ring.
    #[test]
    fn short_chunks_stay_out_of_the_reference() {
        let mut c = LevelConfirm::new();
        c.observe(None, T0 - 60_000);
        let full = alt(0.1, CONFIRM_FRAMES);
        let tail = alt(0.5, 1);
        for _ in 0..CONFIRM_REF_CHUNKS {
            c.chunk_gain(&full, 1.0);
        }
        for _ in 0..CONFIRM_REF_CHUNKS {
            c.chunk_gain(&tail, 1.0);
        }
        c.observe(ramp(2, 0.5), T0);
        let g = c.chunk_gain(&alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES), 1.0);
        let step_gain = 0.5 / ramp_value(0.5, HELD_VOLUME, 2);
        assert!(close(g, step_gain), "reference kept: {g} vs {step_gain}");
    }

    /// Review Focus 2: a device change silences the capture; R2 never lifts it.
    #[test]
    fn device_mute_wins_over_confirmation() {
        let mut c = confirm_with_reference(0.1, 20, 2, 0.5);
        let chunk = alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES);
        assert_eq!(c.chunk_gain(&chunk, 0.0), 0.0);
        assert!(c.chunk_gain(&chunk, 1.0) > 1.0, "the same chunk confirms");
    }

    /// Task 8 carry: `attach()` turning `None` mid-window (e.g. an event-driven follow) ends
    /// the window for good; it is not counted as a fallback, and recording resumes.
    #[test]
    fn window_ends_when_the_ramp_disappears() {
        let mut c = confirm_with_reference(0.1, 20, 2, 0.5);
        let chunk = alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES);
        assert!(c.chunk_gain(&chunk, 1.0) > 1.0);
        c.observe(None, T0 + 20_000);
        assert_eq!(c.chunk_gain(&chunk, 2.0), 2.0);
        c.observe(ramp(3, 0.5), T0 + 30_000);
        assert_eq!(c.chunk_gain(&chunk, 2.0), 2.0, "same epoch does not reopen");
        assert_eq!(c.take_counts(), (1, 0));
    }

    /// The reference of a new epoch holds only audio recorded since the previous window.
    #[test]
    fn reference_is_rebuilt_after_each_window() {
        let mut c = confirm_with_reference(0.1, 20, 2, 0.5);
        let chunk = alt(step_amp(0.1, 0.5, 2), CONFIRM_FRAMES);
        assert!(c.chunk_gain(&chunk, 1.0) > 1.0);
        c.observe(None, T0 + 200_000);
        let reference = alt(0.1, CONFIRM_FRAMES);
        for _ in 0..7 {
            assert_eq!(c.chunk_gain(&reference, 1.0), 1.0);
        }
        let next = Some(AttachRamp {
            epoch: 2,
            steps: 2,
            original: 0.5,
        });
        c.observe(next, T0 + 300_000);
        assert_eq!(
            c.chunk_gain(&chunk, 1.0),
            1.0,
            "7 chunks since the last window"
        );
    }

    /// A packet's short remainder (441 = 4 x 110 + 1 frames) is not judged on its own: a
    /// single frame near a zero crossing would read as a deeper step. It keeps the step of
    /// the chunk before it, still capped at the reference.
    #[test]
    fn short_tail_chunk_keeps_the_previous_step() {
        let mut c = confirm_with_reference(0.1, 20, 4, 0.5);
        let g1 = c.chunk_gain(&alt(step_amp(0.1, 0.5, 1), CONFIRM_FRAMES), 1.0);
        assert!(close(g1, 0.5 / ramp_value(0.5, HELD_VOLUME, 1)), "{g1}");
        // On its own this frame would match step 3.
        let tail = alt(step_amp(0.1, 0.5, 3), 1);
        assert!(close(c.chunk_gain(&tail, 1.0), g1));
        // After a mismatch the tail stays conservative even though its level matches.
        assert_eq!(
            c.chunk_gain(&alt(db_amp(0.1, -28.0), CONFIRM_FRAMES), 1.0),
            1.0
        );
        assert_eq!(c.chunk_gain(&alt(step_amp(0.1, 0.5, 2), 1), 1.0), 1.0);
    }

    /// The real capture pattern: 441-frame packets through `condition_with`, so each packet
    /// records 4 full chunks and skips its 1-frame remainder. Two packets (what the engine
    /// may have seen counted when it starts the attach) give a valid reference; one does not.
    #[test]
    fn two_real_packets_make_a_reference_one_does_not() {
        let stats = AudioStats::default();
        let follow = AtomicBool::new(false);
        for packets in [2usize, 1] {
            let mut c = LevelConfirm::new();
            let mut cond = Conditioner::new();
            c.observe(None, T0 - 30_000);
            for _ in 0..packets {
                let mut pkt = alt(0.1, 441);
                cond.condition_with(
                    &mut pkt,
                    |raw| c.chunk_gain(raw, 1.0),
                    &stats,
                    &follow,
                    |_| {},
                );
            }
            c.observe(ramp(2, 0.5), T0);
            let mut pkt = alt(step_amp(0.1, 0.5, 2), 441);
            let mut gains = Vec::new();
            cond.condition_with(
                &mut pkt,
                |raw| {
                    let g = c.chunk_gain(raw, 1.0);
                    gains.push(g);
                    g
                },
                &stats,
                &follow,
                |_| {},
            );
            if packets == 2 {
                assert!(gains.iter().all(|&g| g > 1.0), "{gains:?}");
                assert_eq!(c.take_counts(), (5, 0));
            } else {
                assert!(gains.iter().all(|&g| g == 1.0), "{gains:?}");
                assert_eq!(c.take_counts(), (0, 5));
            }
        }
        assert_eq!(stats.unattenuated_blocks.load(Ordering::Relaxed), 0);
    }

    // ---- Invariant simulation ------------------------------------------------------------

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Source {
        /// White noise, RMS 0.1 as captured before the attach.
        Stationary,
        /// The same noise with a random -10..+10 dB level every 2.5 ms of stream.
        Dynamic,
        /// Two sessions at originals 1.0 and 0.3 (each RMS 0.1 as captured before the attach),
        /// each lowered along its own ramp; the ramp's `original` is 0.3.
        Dual,
    }

    /// xorshift64* (deterministic, no dependency).
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        /// Uniform in (0, 1].
        fn unit(&mut self) -> f64 {
            ((self.next() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let (u, v) = (self.unit(), self.unit());
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    const SIM_T0: u64 = 300_000;
    const PKT_FRAMES: usize = 441;
    const PKT_US: u64 = 10_000;
    /// Ramp steps of the simulated holder.
    const SIM_STEPS: u32 = 4;
    const HOLE_DB: f64 = -30.0;

    #[derive(Debug, Default, Clone, Copy)]
    struct SimResult {
        /// Worst output / reference RMS among the chunks raised above the conservative gain.
        worst_rms: f64,
        /// Worst output / reference peak among the raised chunks.
        worst_peak: f64,
        raised: usize,
        /// Longest run of chunks starting in [t0, t0 + 200 ms] more than 30 dB below the
        /// reference.
        longest_hole: usize,
        guard_trips: u64,
    }

    /// One attach: the holder issues steps 1..=4 at t0, +10, +20, +30 ms; each shows in the
    /// captured audio `d_us` later and is published to `observe` 1 ms after it is issued
    /// (until t0 + 150 ms); 441-frame packets arrive every 10 ms at phase `phi_us`; the
    /// conservative gain is the holder's `GainHistory`. `r2 == false` is the control group
    /// (conservative gain only).
    fn simulate(source: Source, original: f32, d_us: u64, phi_us: u64, r2: bool) -> SimResult {
        let sessions: Vec<(f32, f64)> = match source {
            Source::Dual => vec![(1.0, 0.1), (0.3, 0.1 / 0.3)],
            _ => vec![(original, 0.1 / f64::from(original))],
        };
        let ramp_original = match source {
            Source::Dual => 0.3,
            _ => original,
        };
        let seed = d_us * 1_000 + phi_us * 7 + (original * 1_000.0) as u64 + source as u64;
        let mut rng = Rng::new(seed + 1);
        let mut level_rng = Rng::new(seed + 2);
        let issued = |k: u32| SIM_T0 + u64::from(k - 1) * RAMP_STEP_US;
        let mut confirm = LevelConfirm::new();
        let mut cond = Conditioner::new();
        let stats = AudioStats::default();
        let follow = AtomicBool::new(false);
        let mut history = GainHistory::new(GAIN_WINDOW_US);
        let mut recorded = 0u32;
        let mut window_seen = false;
        // Reference chunks (sum of squares, samples, peak) seen before the window.
        let mut pre: Vec<(f64, usize, f32)> = Vec::new();
        let mut reference: Option<(f64, f64)> = None; // (rms, peak)
        let mut segment_db = Vec::new();
        let mut res = SimResult::default();
        let mut run = 0usize;
        let mut n = 0u64;
        loop {
            let t = (n + 1) * PKT_US + phi_us;
            if t > SIM_T0 + 260_000 {
                break;
            }
            // Audio of [t - 10 ms, t).
            let mut raw = vec![0.0f32; PKT_FRAMES * 2];
            for i in 0..PKT_FRAMES {
                let frame = n as usize * PKT_FRAMES + i;
                let s = (t - PKT_US) as f64 + i as f64 * PKT_US as f64 / PKT_FRAMES as f64;
                let step = (1..=SIM_STEPS)
                    .filter(|&k| (issued(k) + d_us) as f64 <= s)
                    .count() as u32;
                let seg = frame / CONFIRM_FRAMES;
                while segment_db.len() <= seg {
                    segment_db.push(level_rng.unit() * 20.0 - 10.0);
                }
                let dynamic = if source == Source::Dynamic {
                    10f64.powf(segment_db[seg] / 20.0)
                } else {
                    1.0
                };
                for ch in 0..2 {
                    let mut x = 0.0f64;
                    for &(orig, scale) in &sessions {
                        let v = f64::from(ramp_value(orig, HELD_VOLUME, step));
                        x += rng.gauss() * scale * dynamic * v;
                    }
                    raw[i * 2 + ch] = x as f32;
                }
            }
            // Published state at `t` (1 ms after issue).
            let published = (1..=SIM_STEPS).filter(|&k| issued(k) + 1_000 <= t).count() as u32;
            while recorded < published {
                recorded += 1;
                history.set(
                    issued(recorded),
                    ramp_value(ramp_original, HELD_VOLUME, recorded),
                );
            }
            let attach = (published >= 1 && t < SIM_T0 + CONFIRM_WINDOW_US).then_some(AttachRamp {
                epoch: 1,
                steps: published,
                original: ramp_original,
            });
            let conservative = if published == 0 {
                1.0
            } else {
                history.conservative_gain(ramp_original, t)
            };
            if attach.is_some() && !window_seen {
                window_seen = true;
                let last = &pre[pre.len().saturating_sub(CONFIRM_REF_CHUNKS)..];
                let sum: f64 = last.iter().map(|c| c.0).sum();
                let samples: usize = last.iter().map(|c| c.1).sum();
                let peak = last.iter().map(|c| c.2).fold(0.0f32, f32::max);
                reference = Some(((sum / samples as f64).sqrt(), f64::from(peak)));
            }
            if r2 {
                confirm.observe(attach, t);
            }
            let mut out = raw.clone();
            let mut chunk_gains = Vec::new();
            cond.condition_with(
                &mut out,
                |chunk| {
                    let g = if r2 {
                        confirm.chunk_gain(chunk, conservative)
                    } else {
                        conservative
                    };
                    chunk_gains.push(g);
                    g
                },
                &stats,
                &follow,
                |_| {},
            );
            let _ = confirm.take_counts();
            for (ci, (rc, oc)) in raw
                .chunks(CONFIRM_FRAMES * 2)
                .zip(out.chunks(CONFIRM_FRAMES * 2))
                .enumerate()
            {
                if !window_seen {
                    if rc.len() / 2 < MIN_JUDGED_FRAMES {
                        continue; // not recorded as reference
                    }
                    let sum: f64 = rc.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
                    let peak = rc.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                    pre.push((sum, rc.len(), peak));
                    continue;
                }
                let (ref_rms, ref_peak) = reference.unwrap();
                let out_rms = (oc.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>()
                    / oc.len() as f64)
                    .sqrt();
                let out_peak = f64::from(oc.iter().fold(0.0f32, |m, s| m.max(s.abs())));
                if chunk_gains[ci] > conservative {
                    res.raised += 1;
                    res.worst_rms = res.worst_rms.max(out_rms / ref_rms);
                    res.worst_peak = res.worst_peak.max(out_peak / ref_peak);
                }
                let start = (t - PKT_US) as f64
                    + (ci * CONFIRM_FRAMES) as f64 * PKT_US as f64 / PKT_FRAMES as f64;
                if start >= SIM_T0 as f64 && start <= (SIM_T0 + 200_000) as f64 {
                    if out_rms < ref_rms * 10f64.powf(HOLE_DB / 20.0) {
                        run += 1;
                        res.longest_hole = res.longest_hole.max(run);
                    } else {
                        run = 0;
                    }
                }
            }
            n += 1;
        }
        res.guard_trips = stats.unattenuated_blocks.load(Ordering::Relaxed);
        res
    }

    /// Delivery delays: up to the P0 measurement's process-loopback offset S (about 38.5 ms)
    /// and beyond it (ruling 17).
    const DELAYS_US: [u64; 7] = [0, 5_000, 12_000, 16_000, 26_000, 38_000, 45_000];

    #[test]
    fn never_louder_than_the_reference_during_any_attach() {
        let mut cases: Vec<(Source, f32)> = Vec::new();
        for original in [1.0f32, 0.5, 0.3, 0.001] {
            cases.push((Source::Stationary, original));
            cases.push((Source::Dynamic, original));
        }
        cases.push((Source::Dual, 0.3));
        for (source, original) in cases {
            let mut worst = SimResult::default();
            for d in DELAYS_US {
                for phi in (0..10).map(|k| k * 1_000) {
                    let r = simulate(source, original, d, phi, true);
                    assert!(
                        r.worst_rms <= 1.0 && r.worst_peak <= 1.0,
                        "{source:?} original {original} d {d} phi {phi}: {r:?}"
                    );
                    worst.worst_rms = worst.worst_rms.max(r.worst_rms);
                    worst.worst_peak = worst.worst_peak.max(r.worst_peak);
                    worst.raised += r.raised;
                    worst.guard_trips += r.guard_trips;
                }
            }
            println!(
                "R2 {source:?} original {original}: worst rms ratio {:.8}, worst peak ratio {:.8}, raised chunks {}, guard trips {}",
                worst.worst_rms, worst.worst_peak, worst.raised, worst.guard_trips
            );
            assert!(worst.raised > 0, "{source:?} {original}: R2 never engaged");
        }
    }

    #[test]
    fn stationary_attach_leaves_no_hole_longer_than_10ms() {
        for original in [1.0f32, 0.3] {
            let (mut r2_longest, mut control_shortest, mut control_longest) = (0, usize::MAX, 0);
            for d in [12_000u64, 16_000, 26_000, 38_000, 45_000] {
                for phi in (0..10).map(|k| k * 1_000) {
                    let r = simulate(Source::Stationary, original, d, phi, true);
                    let control = simulate(Source::Stationary, original, d, phi, false);
                    assert!(
                        r.longest_hole <= 4,
                        "R2 original {original} d {d} phi {phi}: {r:?}"
                    );
                    // Without R2 the hole lasts until the conservative window lets go, so a
                    // later step (larger d) shortens it: 27..33 chunks at d = 45 ms.
                    let control_min = if d <= 26_000 { 32 } else { 24 };
                    assert!(
                        control.longest_hole >= control_min,
                        "control original {original} d {d} phi {phi}: {control:?}"
                    );
                    r2_longest = r2_longest.max(r.longest_hole);
                    control_shortest = control_shortest.min(control.longest_hole);
                    control_longest = control_longest.max(control.longest_hole);
                }
            }
            println!(
                "original {original}: longest hole with R2 {r2_longest} chunks; without R2 the longest hole is {control_shortest}..{control_longest} chunks"
            );
        }
    }
}
