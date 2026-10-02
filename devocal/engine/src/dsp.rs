//! Pure DSP primitives. Audio is interleaved stereo `f32`; times are `u64` microseconds.

use std::collections::VecDeque;
use std::f32::consts::FRAC_PI_2;

pub const SAMPLE_RATE: u32 = 44_100;

/// Milliseconds to frames at `SAMPLE_RATE`, rounded to nearest (20 ms -> 882).
pub fn frames_for_ms(ms: f32) -> usize {
    // f64 keeps 5 ms * 44.1 kHz exactly at 220.5, which rounds up to 221.
    (f64::from(ms) * f64::from(SAMPLE_RATE) / 1000.0).round() as usize
}

/// Fixed delay for interleaved stereo audio (keeps passthrough aligned with
/// the separation model's latency). Preallocated; `process` never allocates.
pub struct DelayLine {
    buf: Vec<f32>,
    pos: usize,
}

impl DelayLine {
    pub fn new(frames: usize) -> Self {
        Self {
            buf: vec![0.0; frames * 2],
            pos: 0,
        }
    }

    /// Replaces each sample with the one `frames` frames earlier. The ring holds
    /// `frames * 2` samples, so channel interleaving is preserved for any block length.
    pub fn process(&mut self, inout: &mut [f32]) {
        let len = self.buf.len();
        if len == 0 {
            return;
        }
        for s in inout.iter_mut() {
            let delayed = self.buf[self.pos];
            self.buf[self.pos] = *s;
            *s = delayed;
            self.pos += 1;
            if self.pos == len {
                self.pos = 0;
            }
        }
    }

    pub fn reset(&mut self) {
        self.buf.fill(0.0);
        self.pos = 0;
    }
}

/// Equal-power crossfade gains `(out, in) = (cos(pi t / 2), sin(pi t / 2))`, `t` clamped to [0, 1].
pub fn equal_power(t: f32) -> (f32, f32) {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    let a = t * FRAC_PI_2;
    (a.cos(), a.sin())
}

/// Equal-power crossfade from one stream to another over a fixed number of frames.
/// A freshly created crossfade is idle (already done, outputs `to`) until `start`.
pub struct Crossfade {
    frames: usize,
    pos: usize,
}

impl Crossfade {
    pub fn new(frames: usize) -> Self {
        Self {
            frames,
            pos: frames,
        }
    }

    pub fn start(&mut self) {
        self.pos = 0;
    }

    /// Restarts `pos` frames into the fade (clamped to `frames()`). Reversing a fade is
    /// `start_at(frames() - position())` with `from` and `to` swapped: equal-power gains are
    /// symmetric, so the output continues from the current mix without a jump.
    pub fn start_at(&mut self, pos: usize) {
        self.pos = pos.min(self.frames);
    }

    /// Frames already mixed: 0 = all `from`, `frames()` = complete (all `to`).
    pub fn position(&self) -> usize {
        self.pos.min(self.frames)
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Mixes interleaved stereo blocks; processes whole frames up to the shortest of the
    /// three slices. Returns true once the fade has completed (afterwards output equals `to`).
    pub fn mix(&mut self, from: &[f32], to: &[f32], out: &mut [f32]) -> bool {
        let frames = self.frames;
        let frame_iter = from
            .chunks_exact(2)
            .zip(to.chunks_exact(2))
            .zip(out.chunks_exact_mut(2));
        for ((f, t), o) in frame_iter {
            if self.pos >= frames {
                o[0] = t[0];
                o[1] = t[1];
            } else {
                let (g_out, g_in) = equal_power(self.pos as f32 / frames as f32);
                o[0] = f[0] * g_out + t[0] * g_in;
                o[1] = f[1] * g_out + t[1] * g_in;
                self.pos += 1;
            }
        }
        self.pos >= frames
    }
}

/// History of session-volume settings. While the player's volume steps (e.g. towards
/// 1e-4) the attenuation of captured audio is uncertain, so the compensating gain uses
/// the largest volume set recently: `original / max(volume)`; never louder than the original.
pub struct GainHistory {
    window_us: u64,
    entries: VecDeque<(u64, f32)>,
}

/// Lowest volume used as a divisor; keeps the gain bounded.
const MIN_VOLUME: f32 = 1e-4;

impl GainHistory {
    pub fn new(window_us: u64) -> Self {
        Self {
            window_us,
            entries: VecDeque::new(),
        }
    }

    pub fn set(&mut self, at_us: u64, volume: f32) {
        if !volume.is_finite() {
            return;
        }
        self.entries.push_back((at_us, volume));
        // Drop entries superseded before the window of the newest set (keep the one in
        // effect at the window start).
        while self.entries.len() > 1 && self.entries[1].0.saturating_add(self.window_us) < at_us {
            self.entries.pop_front();
        }
    }

    /// `original / max(volume in effect at any point in [now - window, now])`, where the
    /// maximum also covers sets stamped after `now` (conservative). If no set is older than
    /// the window, the volume before the first set (`original`) was in effect too, so it is
    /// part of the max; with no history at all the gain is therefore 1.0. A non-finite or
    /// non-positive `original` yields 0.0 (silent rather than loud).
    pub fn conservative_gain(&self, original: f32, now_us: u64) -> f32 {
        if !original.is_finite() || original <= 0.0 {
            return 0.0;
        }
        let mut max: Option<f32> = None;
        let mut last_old: Option<f32> = None;
        for &(at, v) in &self.entries {
            if at.saturating_add(self.window_us) < now_us {
                last_old = Some(v); // only the latest old entry is still in effect
            } else {
                max = Some(max.map_or(v, |m| m.max(v)));
            }
        }
        // The volume in effect at window start: the latest old set, or the original
        // volume when nothing was set before the window.
        let at_window_start = last_old.unwrap_or(original);
        let m = max.map_or(at_window_start, |m| m.max(at_window_start));
        // The floor also bounds the result to original / MIN_VOLUME.
        original / m.max(MIN_VOLUME)
    }
}

/// Linear edge ramps for dropouts: the first frame of a fade-in is 0 and the last frame
/// of a fade-out is 0; the ramp reaches 1 after `fade_frames - 1` frames.
pub fn fade_edges(block: &mut [f32], fade_in: bool, fade_out: bool, fade_frames: usize) {
    if fade_frames == 0 {
        return;
    }
    let denom = (fade_frames - 1).max(1) as f32;
    let ramp = |i: usize| -> f32 {
        if i >= fade_frames {
            1.0
        } else if fade_frames == 1 {
            0.0
        } else {
            i as f32 / denom
        }
    };
    if fade_in {
        for (i, f) in block.chunks_exact_mut(2).take(fade_frames).enumerate() {
            let g = ramp(i);
            f[0] *= g;
            f[1] *= g;
        }
    }
    if fade_out {
        for (j, f) in block
            .chunks_exact_mut(2)
            .rev()
            .take(fade_frames)
            .enumerate()
        {
            let g = ramp(j);
            f[0] *= g;
            f[1] *= g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_for_ms_rounds() {
        assert_eq!(frames_for_ms(20.0), 882);
        assert_eq!(frames_for_ms(5.0), 221);
    }

    #[test]
    fn delay_line_shifts_by_exact_frames() {
        let mut d = DelayLine::new(128);
        // 300 frames; impulse on left at frame 0, on right at frame 10.
        let mut buf = vec![0.0f32; 300 * 2];
        buf[0] = 1.0;
        buf[10 * 2 + 1] = 0.5;
        // Process in uneven blocks to check state carries across calls.
        let (a, b) = buf.split_at_mut(77 * 2);
        d.process(a);
        d.process(b);
        for f in 0..300 {
            let l = buf[f * 2];
            let r = buf[f * 2 + 1];
            assert_eq!(l, if f == 128 { 1.0 } else { 0.0 }, "left frame {f}");
            assert_eq!(r, if f == 138 { 0.5 } else { 0.0 }, "right frame {f}");
        }
        d.reset();
        let mut z = vec![1.0f32; 4];
        d.process(&mut z);
        assert!(z.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn delay_line_zero_frames_is_passthrough() {
        let mut d = DelayLine::new(0);
        let mut x = vec![0.1f32, 0.2, 0.3, 0.4];
        d.process(&mut x);
        assert_eq!(x, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn equal_power_sums_to_unit_power() {
        for &t in &[0.0f32, 0.25, 0.5, 1.0] {
            let (o, i) = equal_power(t);
            assert!((o * o + i * i - 1.0).abs() < 1e-6, "t={t}");
        }
        let (o, i) = equal_power(0.0);
        assert!((o - 1.0).abs() < 1e-6 && i.abs() < 1e-6);
        let (o, i) = equal_power(1.0);
        assert!(o.abs() < 1e-6 && (i - 1.0).abs() < 1e-6);
        assert_eq!(equal_power(-3.0), equal_power(0.0));
        assert_eq!(equal_power(7.0), equal_power(1.0));
    }

    #[test]
    fn crossfade_takes_882_frames_and_reports_done() {
        let mut c = Crossfade::new(882);
        c.start();
        let from = vec![1.0f32; 882 * 2 + 20];
        let to = vec![0.0f32; 882 * 2 + 20];
        let mut out = vec![9.0f32; 881 * 2];
        assert!(!c.mix(&from[..881 * 2], &to[..881 * 2], &mut out));
        // Monotonic fall from 1.0 towards 0.0, channels equal.
        assert_eq!(out[0], 1.0);
        for f in 1..881 {
            assert!(out[f * 2] < out[(f - 1) * 2]);
            assert_eq!(out[f * 2], out[f * 2 + 1]);
        }
        let mut one = vec![9.0f32; 2];
        assert!(c.mix(&from[..2], &to[..2], &mut one));
        // Afterwards output is exactly `to`.
        let mut after = vec![9.0f32; 20];
        assert!(c.mix(&from[..20], &to[..20], &mut after));
        assert!(after.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn crossfade_reverses_from_its_position() {
        let mut c = Crossfade::new(100);
        assert_eq!(c.frames(), 100);
        assert_eq!(c.position(), 100, "idle crossfade is complete");
        c.start();
        let from = vec![1.0f32; 2 * 40];
        let to = vec![0.0f32; 2 * 40];
        let mut out = vec![0.0f32; 2 * 40];
        c.mix(&from, &to, &mut out);
        assert_eq!(c.position(), 40);
        let last = out[39 * 2];
        // Reverse: swap the streams and continue from the mirrored position.
        c.start_at(c.frames() - c.position());
        assert_eq!(c.position(), 60);
        let mut back = vec![0.0f32; 2];
        c.mix(&to[..2], &from[..2], &mut back);
        assert!((back[0] - last).abs() <= 0.02, "{last} -> {}", back[0]);
        c.start_at(1000);
        assert_eq!(c.position(), 100, "clamped");
    }

    #[test]
    fn crossfade_done_in_one_big_block() {
        let mut c = Crossfade::new(882);
        c.start();
        let from = vec![1.0f32; 2000 * 2];
        let to = vec![0.5f32; 2000 * 2];
        let mut out = vec![0.0f32; 2000 * 2];
        assert!(c.mix(&from, &to, &mut out));
        assert!(out[882 * 2..].iter().all(|&s| s == 0.5));
    }

    #[test]
    fn gain_history_never_exceeds_true_gain_while_ramping_down() {
        let mut g = GainHistory::new(100_000);
        g.set(0, 0.5);
        g.set(10_000, 0.05);
        g.set(20_000, 0.005);
        g.set(30_000, 1e-4);
        assert!((g.conservative_gain(0.5, 15_000) - 1.0).abs() < 1e-6);
        let v = g.conservative_gain(0.5, 131_000);
        assert!((v - 5000.0).abs() < 1e-2, "{v}");
    }

    #[test]
    fn gain_history_is_conservative_when_ramping_up() {
        let mut g = GainHistory::new(100_000);
        g.set(0, 1e-4);
        g.set(10_000, 0.5);
        assert!((g.conservative_gain(0.5, 12_000) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn first_lowering_without_seed_is_never_louder() {
        let mut g = GainHistory::new(100_000);
        g.set(10_000, 1e-4);
        let v = g.conservative_gain(0.5, 12_000);
        assert!(v <= 1.0 + 1e-6, "{v}");
    }

    #[test]
    fn set_older_than_window_still_counts() {
        let mut g = GainHistory::new(100_000);
        g.set(0, 0.5);
        g.set(10_000, 0.05);
        g.set(20_000, 0.005);
        g.set(30_000, 1e-4);
        // At 115 ms the 10 ms set (0.05) is the latest one older than the window.
        let v = g.conservative_gain(0.5, 115_000);
        assert!((v - 10.0).abs() < 1e-3, "{v}");
    }

    #[test]
    fn zero_or_negative_volume_stays_finite() {
        let mut g = GainHistory::new(100_000);
        g.set(0, 0.0);
        g.set(1_000, -1.0);
        let v = g.conservative_gain(0.5, 200_000);
        assert!(v.is_finite() && v <= 0.5 / 1e-4 + 1.0, "{v}");
        let v = g.conservative_gain(0.5, 2_000);
        assert!(v.is_finite() && v <= 1.0 + 1e-6, "{v}");
    }

    #[test]
    fn non_finite_original_is_silent() {
        let mut g = GainHistory::new(100_000);
        g.set(0, 0.5);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -0.5] {
            assert_eq!(g.conservative_gain(bad, 1_000), 0.0);
        }
    }

    #[test]
    fn gain_history_empty_is_unity() {
        let g = GainHistory::new(100_000);
        assert_eq!(g.conservative_gain(0.5, 1_000), 1.0);
    }

    #[test]
    fn fade_edges_ramps_over_220_frames() {
        let n = frames_for_ms(5.0);
        assert_eq!(n, 221);
        let mut b = vec![1.0f32; 1000 * 2];
        fade_edges(&mut b, true, false, n);
        assert_eq!(b[0], 0.0);
        assert_eq!(b[1], 0.0);
        assert_eq!(b[n * 2], 1.0);
        assert_eq!(b[(n - 1) * 2], 1.0);
        assert!(b[100 * 2] > 0.0 && b[100 * 2] < 1.0);
        assert!(b[999 * 2] == 1.0);

        let mut b = vec![1.0f32; 1000 * 2];
        fade_edges(&mut b, false, true, n);
        assert_eq!(b[0], 1.0);
        assert_eq!(b[999 * 2], 0.0);
        assert_eq!(b[999 * 2 + 1], 0.0);
        assert_eq!(b[(1000 - n) * 2], 1.0);

        let mut b = vec![1.0f32; 1000 * 2];
        fade_edges(&mut b, false, false, n);
        assert!(b.iter().all(|&s| s == 1.0));
    }
}
