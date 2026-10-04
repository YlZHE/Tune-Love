//! Vocal separator interface. Audio is interleaved stereo `f32` in fixed blocks of `hop` frames.

use crate::dsp::{DelayLine, SAMPLE_RATE};

/// A streaming separator. Each `process` call consumes one block of `hop()` frames
/// (`hop() * 2` interleaved samples) and writes the same number of accompaniment samples,
/// delayed by `latency_frames()` relative to the input.
pub trait Separator: Send {
    fn sample_rate(&self) -> u32;
    fn hop(&self) -> usize;
    fn latency_frames(&self) -> usize;
    /// Latency of the pass-through (devocal off) path paired with this separator. Defaults to
    /// `latency_frames()`; window separators keep pass-through at one block.
    fn passthrough_latency_frames(&self) -> usize {
        self.latency_frames()
    }
    /// `input` and `out_accompaniment` must both be `hop() * 2` samples long. Errors (bad
    /// length, inference failure) are reported as `Err`, never as a panic; the contents of
    /// `out_accompaniment` are unspecified after an error.
    fn process(&mut self, input: &[f32], out_accompaniment: &mut [f32]) -> Result<(), String>;
    /// Clears all streaming state, as if freshly loaded.
    fn reset(&mut self);
    /// The model itself cannot keep up (window models: spec 4.1 timeouts / duty). Must be a
    /// cheap, lock-free read: polled on the processing thread after every model block.
    fn overloaded(&self) -> bool {
        false
    }
    /// Window models: (segments played dry since load, 10 s duty). Cheap and lock-free like
    /// `overloaded`; `None` for streaming models.
    fn window_stats(&self) -> Option<(u64, f32)> {
        None
    }
}

/// Block size used by `DelayOnly`; matches StemgenRT.
pub const DELAY_ONLY_HOP: usize = 128;

/// Pass-through "separator" for tests and alignment: output = input delayed by
/// `latency_frames`.
pub struct DelayOnly {
    delay: DelayLine,
    latency_frames: usize,
}

impl DelayOnly {
    pub fn new(latency_frames: usize) -> Self {
        Self {
            delay: DelayLine::new(latency_frames),
            latency_frames,
        }
    }
}

/// Checks the `process` contract shared by all separators.
pub fn check_block(hop: usize, input: &[f32], output: &[f32]) -> Result<(), String> {
    let want = hop * 2;
    if input.len() != want || output.len() != want {
        return Err(format!(
            "block length mismatch: input {} / output {} samples, expected {want}",
            input.len(),
            output.len()
        ));
    }
    Ok(())
}

impl Separator for DelayOnly {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn hop(&self) -> usize {
        DELAY_ONLY_HOP
    }

    fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    fn process(&mut self, input: &[f32], out_accompaniment: &mut [f32]) -> Result<(), String> {
        check_block(DELAY_ONLY_HOP, input, out_accompaniment)?;
        out_accompaniment.copy_from_slice(input);
        self.delay.process(out_accompaniment);
        Ok(())
    }

    fn reset(&mut self) {
        self.delay.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_only_matches_delay_line() {
        let mut sep = DelayOnly::new(128);
        let mut reference = DelayLine::new(128);
        assert_eq!(sep.latency_frames(), 128);
        assert_eq!(sep.sample_rate(), 44_100);
        let hop = sep.hop();
        let mut out = vec![0.0f32; hop * 2];
        for block in 0..10 {
            let input: Vec<f32> = (0..hop * 2)
                .map(|i| ((block * hop * 2 + i) as f32 * 0.013).sin())
                .collect();
            let mut expected = input.clone();
            reference.process(&mut expected);
            sep.process(&input, &mut out).unwrap();
            assert_eq!(out, expected, "block {block}");
        }
    }

    #[test]
    fn delay_only_rejects_wrong_length_and_resets() {
        let mut sep = DelayOnly::new(128);
        let mut out = vec![0.0f32; 256];
        assert!(sep.process(&[0.0; 10], &mut out).is_err());
        assert!(sep.process(&[0.0; 256], &mut [0.0; 10]).is_err());
        sep.process(&[1.0; 256], &mut out).unwrap();
        sep.reset();
        sep.process(&[0.5; 256], &mut out).unwrap();
        assert!(out.iter().all(|&s| s == 0.0), "reset clears the delay");
    }
}
