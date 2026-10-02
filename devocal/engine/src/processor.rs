//! Block processor on the engine's processing thread: one hop of interleaved stereo `f32`
//! per call. The passthrough always goes through a `DelayLine` matching the model latency,
//! so passthrough and accompaniment stay time-aligned and switching between them never
//! shifts the audio.
//!
//! Stages (`fade` = 20 ms equal-power crossfade between passthrough and accompaniment):
//!
//! ```text
//!   Passthrough --on--> WarmingUp --200 ms--> FadingIn --fade--> Devocal
//!        ^   <--off (gain is 0)--'              |  ^               |
//!        |                                  off |  | on        off |
//!        |                                      v  |               |
//!        '------------- fade done ---------- FadingOut <-----------'
//!                                               |  (force_fallback: done -> Fallback)
//!   Fallback --on (reset model)--> WarmingUp    v
//!   Fallback <------------------------- fade done
//! ```
//!
//! Reversing direction during a fade continues from the current gain. Any model error (or
//! non-finite model output, or a malformed block while the model runs) outputs that block as
//! passthrough and enters `Fallback` at once; the model is `reset()` before it runs again.

use devocal_core::protocol::FallbackReason;

use crate::dsp::{frames_for_ms, Crossfade, DelayLine};
use crate::separator::Separator;

/// Hop and passthrough latency without a model, matching StemgenRT so that loading the
/// model later does not change the timing.
pub const NO_MODEL_HOP: usize = 128;
pub const NO_MODEL_LATENCY_FRAMES: usize = 128;
/// The model runs (in parallel with the passthrough) this long before the fade-in.
pub const WARM_UP_MS: f32 = 200.0;
/// Equal-power crossfade between passthrough and accompaniment.
pub const FADE_MS: f32 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Passthrough,
    WarmingUp,
    FadingIn,
    Devocal,
    FadingOut,
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockReport {
    /// The separator's `process` was called for this block.
    pub ran_model: bool,
    /// Stage after this block.
    pub stage: Stage,
}

pub struct Processor {
    separator: Option<Box<dyn Separator>>,
    hop: usize,
    /// Passthrough delay in frames (the model's latency).
    latency_frames: usize,
    delay: DelayLine,
    stage: Stage,
    /// Set while falling back (fading out towards, or in, `Fallback`).
    fallback: Option<FallbackReason>,
    warm_frames: usize,
    warm_left: usize,
    /// Fade position in the current direction: `FadingIn` mixes passthrough -> accompaniment,
    /// `FadingOut` mixes accompaniment -> passthrough.
    fade: Crossfade,
    /// Sanitised copy of the input block (model input).
    input: Vec<f32>,
    /// Delayed passthrough block.
    pass: Vec<f32>,
    /// Accompaniment block from the model.
    acc: Vec<f32>,
}

impl Processor {
    /// Without a model the processor is a 128-frame delay (aligned with StemgenRT).
    pub fn new(separator: Option<Box<dyn Separator>>) -> Self {
        let fade_frames = frames_for_ms(FADE_MS);
        let mut p = Self {
            separator: None,
            hop: NO_MODEL_HOP,
            latency_frames: NO_MODEL_LATENCY_FRAMES,
            delay: DelayLine::new(NO_MODEL_LATENCY_FRAMES),
            stage: Stage::Passthrough,
            fallback: None,
            warm_frames: 0,
            warm_left: 0,
            fade: Crossfade::new(fade_frames),
            input: Vec::new(),
            pass: Vec::new(),
            acc: Vec::new(),
        };
        match separator {
            Some(s) => {
                p.set_separator(s);
            }
            None => p.configure(NO_MODEL_HOP, NO_MODEL_LATENCY_FRAMES),
        }
        p
    }

    /// Installs a new model: back to `Passthrough` (fallback cleared) with the passthrough
    /// delay set to the model's latency. Allocates; call off the audio path or between blocks.
    /// Returns the previous model so the caller can drop it off the audio thread (tearing
    /// down an inference session can take milliseconds).
    pub fn set_separator(&mut self, s: Box<dyn Separator>) -> Option<Box<dyn Separator>> {
        let hop = s.hop();
        let latency = s.latency_frames();
        let old = self.separator.replace(s);
        self.configure(hop, latency);
        old
    }

    /// Removes the model so it can be reused (e.g. after the processing thread ends); the
    /// processor is the no-model delay again.
    pub fn take_separator(&mut self) -> Option<Box<dyn Separator>> {
        let s = self.separator.take();
        if s.is_some() {
            self.configure(NO_MODEL_HOP, NO_MODEL_LATENCY_FRAMES);
        }
        s
    }

    fn configure(&mut self, hop: usize, latency_frames: usize) {
        self.hop = hop.max(1);
        self.latency_frames = latency_frames;
        self.delay = DelayLine::new(latency_frames);
        self.stage = Stage::Passthrough;
        self.fallback = None;
        self.warm_frames = frames_for_ms(WARM_UP_MS).div_ceil(self.hop) * self.hop;
        self.warm_left = 0;
        self.fade = Crossfade::new(self.fade.frames());
        let samples = self.hop * 2;
        self.input = vec![0.0; samples];
        self.pass = vec![0.0; samples];
        self.acc = vec![0.0; samples];
    }

    /// Turns devocal on or off. Without a model "on" is ignored. Leaves `Fallback` only on
    /// "on". Reversing a fade continues from the current gain.
    pub fn request_devocal(&mut self, on: bool) {
        if on {
            match self.stage {
                Stage::Passthrough | Stage::Fallback => {
                    if let Some(sep) = self.separator.as_mut() {
                        // The model may hold stale or poisoned state (after an error).
                        sep.reset();
                        self.fallback = None;
                        self.warm_left = self.warm_frames;
                        self.stage = Stage::WarmingUp;
                    }
                }
                Stage::FadingOut => {
                    // The model ran through the fade-out without error; its state is valid.
                    self.fallback = None;
                    self.reverse_fade();
                    self.stage = Stage::FadingIn;
                }
                Stage::WarmingUp | Stage::FadingIn | Stage::Devocal => {}
            }
        } else {
            match self.stage {
                // Warm-up outputs pure passthrough, so stopping it is seamless.
                Stage::WarmingUp => self.stage = Stage::Passthrough,
                Stage::FadingIn => {
                    self.reverse_fade();
                    self.stage = Stage::FadingOut;
                }
                Stage::Devocal => {
                    self.fade.start();
                    self.stage = Stage::FadingOut;
                }
                Stage::Passthrough | Stage::FadingOut | Stage::Fallback => {}
            }
        }
    }

    /// Falls back to passthrough (fading out if the accompaniment is audible) and stays in
    /// `Fallback` until `request_devocal(true)` or `set_separator`. No effect while devocal
    /// is off (`Passthrough`). While already falling back, `ModelError` replaces `Overload`
    /// but not the other way round.
    pub fn force_fallback(&mut self, reason: FallbackReason) {
        if self.stage == Stage::Passthrough {
            return;
        }
        self.fallback = match self.fallback {
            Some(FallbackReason::ModelError) => Some(FallbackReason::ModelError),
            _ => Some(reason),
        };
        match self.stage {
            Stage::WarmingUp | Stage::Fallback => self.stage = Stage::Fallback,
            Stage::FadingIn => {
                self.reverse_fade();
                self.stage = Stage::FadingOut;
            }
            Stage::Devocal => {
                self.fade.start();
                self.stage = Stage::FadingOut;
            }
            // Already fading out: it now ends in `Fallback`.
            Stage::FadingOut | Stage::Passthrough => {}
        }
    }

    pub fn fallback_reason(&self) -> Option<FallbackReason> {
        self.fallback
    }

    /// Input discontinuity (seek, glitch): clears the model state and the passthrough delay.
    /// The stage is kept; a warm-up in progress starts over so the fresh model state still
    /// gets the full 200 ms before it becomes audible.
    pub fn on_discontinuity(&mut self) {
        if let Some(sep) = self.separator.as_mut() {
            sep.reset();
        }
        self.delay.reset();
        if self.stage == Stage::WarmingUp {
            self.warm_left = self.warm_frames;
        }
    }

    /// Block size in frames; `process_block` takes `hop() * 2` interleaved samples.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Output delay relative to the input in frames (passthrough and accompaniment alike).
    pub fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    /// Current stage (changes immediately on `request_devocal`/`force_fallback`, and on
    /// `process_block`).
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Processes one block of `hop() * 2` samples. Never allocates, panics or outputs
    /// non-finite samples. A block of another length is passed through the delay (the
    /// remainder of `output` is zeroed); if the model was running this is a model error.
    pub fn process_block(&mut self, input: &[f32], output: &mut [f32]) -> BlockReport {
        let samples = self.hop * 2;
        if input.len() != samples || output.len() != samples {
            return self.process_malformed(input, output);
        }
        for (d, &s) in self.input.iter_mut().zip(input) {
            *d = finite_or_zero(s);
        }
        self.pass.copy_from_slice(&self.input);
        self.delay.process(&mut self.pass);

        let mut ran_model = false;
        if self.model_running() {
            let ok = match self.separator.as_mut() {
                Some(sep) => {
                    ran_model = true;
                    sep.process(&self.input, &mut self.acc).is_ok()
                        && self.acc.iter().all(|s| s.is_finite())
                }
                None => false,
            };
            if !ok {
                output.copy_from_slice(&self.pass);
                self.fail_now();
                return self.report(ran_model);
            }
        }

        match self.stage {
            Stage::Passthrough | Stage::Fallback => output.copy_from_slice(&self.pass),
            Stage::WarmingUp => {
                output.copy_from_slice(&self.pass);
                self.warm_left = self.warm_left.saturating_sub(self.hop);
                if self.warm_left == 0 {
                    self.fade.start();
                    self.stage = Stage::FadingIn;
                }
            }
            Stage::FadingIn => {
                if self.fade.mix(&self.pass, &self.acc, output) {
                    self.stage = Stage::Devocal;
                }
            }
            Stage::Devocal => output.copy_from_slice(&self.acc),
            Stage::FadingOut => {
                if self.fade.mix(&self.acc, &self.pass, output) {
                    self.stage = if self.fallback.is_some() {
                        Stage::Fallback
                    } else {
                        Stage::Passthrough
                    };
                }
            }
        }
        self.report(ran_model)
    }

    fn process_malformed(&mut self, input: &[f32], output: &mut [f32]) -> BlockReport {
        let n = input.len().min(output.len());
        for (o, &s) in output[..n].iter_mut().zip(input) {
            *o = finite_or_zero(s);
        }
        self.delay.process(&mut output[..n]);
        output[n..].fill(0.0);
        if self.model_running() {
            // The model cannot take this block; its stream would be out of step.
            self.fail_now();
        }
        self.report(false)
    }

    fn model_running(&self) -> bool {
        matches!(
            self.stage,
            Stage::WarmingUp | Stage::FadingIn | Stage::Devocal | Stage::FadingOut
        )
    }

    /// Model failure: the current block was passthrough, so go straight to `Fallback`
    /// without a fade (the accompaniment is unusable). The model is reset before it runs
    /// again (`request_devocal(true)` from `Fallback`).
    fn fail_now(&mut self) {
        self.fallback = Some(FallbackReason::ModelError);
        self.stage = Stage::Fallback;
    }

    /// Mirrors the fade position for the opposite direction (`from`/`to` swap).
    fn reverse_fade(&mut self) {
        let frames = self.fade.frames();
        self.fade.start_at(frames - self.fade.position());
    }

    fn report(&self, ran_model: bool) -> BlockReport {
        BlockReport {
            ran_model,
            stage: self.stage,
        }
    }
}

fn finite_or_zero(s: f32) -> f32 {
    if s.is_finite() {
        s
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{equal_power, frames_for_ms, DelayLine};
    use crate::separator::{check_block, DelayOnly};
    use crate::stemgen::alloc_count;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const HOP: usize = 128;
    const ACC: f32 = 0.25;
    /// ceil(8820 / 128)
    const WARM_BLOCKS: usize = 69;
    /// 20 ms at 44.1 kHz
    const FADE_FRAMES: usize = 882;

    /// Accompaniment is constant 0.25; counts `reset` calls.
    struct ConstSep {
        resets: Arc<AtomicUsize>,
    }

    impl Separator for ConstSep {
        fn sample_rate(&self) -> u32 {
            44_100
        }
        fn hop(&self) -> usize {
            HOP
        }
        fn latency_frames(&self) -> usize {
            HOP
        }
        fn process(&mut self, input: &[f32], out: &mut [f32]) -> Result<(), String> {
            check_block(HOP, input, out)?;
            out.fill(ACC);
            Ok(())
        }
        fn reset(&mut self) {
            self.resets.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Like `ConstSep`, but the `fail_at`-th `process` call (1-based) returns `Err`.
    struct FailSep {
        fail_at: usize,
        calls: usize,
        resets: Arc<AtomicUsize>,
    }

    impl Separator for FailSep {
        fn sample_rate(&self) -> u32 {
            44_100
        }
        fn hop(&self) -> usize {
            HOP
        }
        fn latency_frames(&self) -> usize {
            HOP
        }
        fn process(&mut self, input: &[f32], out: &mut [f32]) -> Result<(), String> {
            check_block(HOP, input, out)?;
            self.calls += 1;
            // Leave garbage behind, as a failed model might.
            out.fill(f32::NAN);
            if self.calls == self.fail_at {
                return Err("inference failed".into());
            }
            out.fill(ACC);
            Ok(())
        }
        fn reset(&mut self) {
            self.resets.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Reports success but produces NaN.
    struct NanSep;

    impl Separator for NanSep {
        fn sample_rate(&self) -> u32 {
            44_100
        }
        fn hop(&self) -> usize {
            HOP
        }
        fn latency_frames(&self) -> usize {
            HOP
        }
        fn process(&mut self, _input: &[f32], out: &mut [f32]) -> Result<(), String> {
            out.fill(f32::NAN);
            Ok(())
        }
        fn reset(&mut self) {}
    }

    fn const_sep() -> (Box<dyn Separator>, Arc<AtomicUsize>) {
        let resets = Arc::new(AtomicUsize::new(0));
        (
            Box::new(ConstSep {
                resets: resets.clone(),
            }),
            resets,
        )
    }

    fn neg_half(_n: usize) -> f32 {
        -0.5
    }

    /// Never zero, so delayed zeros are distinguishable from input.
    fn ramp(n: usize) -> f32 {
        ((n % 1000) + 1) as f32 / 1000.0
    }

    /// 5 Hz, amplitude 0.5, starting at 0.
    fn slow_sine(n: usize) -> f32 {
        0.5 * (std::f32::consts::TAU * 5.0 * n as f32 / 44_100.0).sin()
    }

    /// Feeds the processor block by block from a signal (same on both channels) and keeps
    /// the expected passthrough from an independent reference delay line.
    struct Rig {
        p: Processor,
        reference: DelayLine,
        signal: fn(usize) -> f32,
        frame: usize,
        input: Vec<f32>,
        pass: Vec<f32>,
        out: Vec<f32>,
    }

    impl Rig {
        fn new(p: Processor, signal: fn(usize) -> f32) -> Self {
            Self {
                p,
                reference: DelayLine::new(HOP),
                signal,
                frame: 0,
                input: Vec::new(),
                pass: Vec::new(),
                out: Vec::new(),
            }
        }

        fn block(&mut self) -> BlockReport {
            self.input = (0..HOP)
                .flat_map(|i| {
                    let v = (self.signal)(self.frame + i);
                    [v, v]
                })
                .collect();
            self.frame += HOP;
            self.pass = self.input.clone();
            self.reference.process(&mut self.pass);
            self.out = vec![9.0; HOP * 2];
            let r = self.p.process_block(&self.input, &mut self.out);
            assert!(
                self.out.iter().all(|s| s.is_finite()),
                "non-finite output in {r:?}"
            );
            r
        }

        fn left(&self) -> Vec<f32> {
            self.out.iter().step_by(2).copied().collect()
        }

        /// Runs blocks until the reported stage is `stage`; returns the left channel.
        fn run_until(&mut self, stage: Stage) -> Vec<f32> {
            let mut left = Vec::new();
            for _ in 0..500 {
                let r = self.block();
                left.extend(self.left());
                if r.stage == stage {
                    return left;
                }
            }
            panic!("never reached {stage:?}");
        }
    }

    #[test]
    fn passthrough_is_delayed_by_model_latency() {
        for sep in [Some(const_sep().0), None] {
            let mut rig = Rig::new(Processor::new(sep), ramp);
            assert_eq!(rig.p.hop(), HOP);
            assert_eq!(rig.p.latency_frames(), HOP);
            assert_eq!(rig.p.stage(), Stage::Passthrough);
            let r = rig.block();
            assert_eq!(
                r,
                BlockReport {
                    ran_model: false,
                    stage: Stage::Passthrough
                }
            );
            // 128 frames of latency = exactly one hop of silence.
            assert!(rig.out.iter().all(|&s| s == 0.0));
            let first_input = rig.input.clone();
            rig.block();
            assert_eq!(rig.out, first_input);
            for _ in 0..10 {
                let r = rig.block();
                assert!(!r.ran_model);
                assert_eq!(rig.out, rig.pass);
            }
        }
    }

    #[test]
    fn enable_warms_200ms_then_fades_20ms() {
        assert_eq!(frames_for_ms(200.0), 8820);
        assert_eq!(frames_for_ms(200.0).div_ceil(HOP), WARM_BLOCKS);
        assert_eq!(frames_for_ms(20.0), FADE_FRAMES);
        let (sep, resets) = const_sep();
        let mut rig = Rig::new(Processor::new(Some(sep)), neg_half);
        for _ in 0..3 {
            rig.block();
        }
        rig.p.request_devocal(true);
        assert_eq!(
            resets.load(Ordering::SeqCst),
            1,
            "warm-up starts with reset"
        );
        for b in 0..WARM_BLOCKS {
            let r = rig.block();
            assert!(r.ran_model, "warm-up block {b}");
            assert_eq!(rig.out, rig.pass, "warm-up block {b} is passthrough");
            if b + 1 < WARM_BLOCKS {
                assert_eq!(r.stage, Stage::WarmingUp, "block {b}");
            }
        }
        let mut fade = Vec::new();
        let mut stages = Vec::new();
        while fade.len() < FADE_FRAMES + 3 * HOP {
            let r = rig.block();
            assert!(r.ran_model);
            stages.push(r.stage);
            fade.extend(rig.left());
        }
        assert_eq!(fade[0], -0.5, "the fade starts at the passthrough");
        for k in 1..FADE_FRAMES {
            assert!(fade[k] > fade[k - 1], "monotonic at fade frame {k}");
        }
        assert!(fade[FADE_FRAMES - 1] < ACC, "still fading at frame 881");
        assert!(
            fade[FADE_FRAMES..].iter().all(|&s| s == ACC),
            "accompaniment after 882 frames"
        );
        // 882 frames = 6 full blocks + 114 frames of the 7th.
        assert!(stages[..6].iter().all(|&s| s == Stage::FadingIn));
        assert!(stages[6..].iter().all(|&s| s == Stage::Devocal));
    }

    #[test]
    fn disable_fades_then_stops_running_model() {
        let (sep, _) = const_sep();
        let mut rig = Rig::new(Processor::new(Some(sep)), neg_half);
        rig.p.request_devocal(true);
        rig.run_until(Stage::Devocal);
        rig.block();
        rig.p.request_devocal(false);
        let mut fade = Vec::new();
        for b in 0..7 {
            let r = rig.block();
            assert!(r.ran_model, "fade-out block {b} still runs the model");
            let want = if b < 6 {
                Stage::FadingOut
            } else {
                Stage::Passthrough
            };
            assert_eq!(r.stage, want, "block {b}");
            fade.extend(rig.left());
        }
        assert_eq!(fade[0], ACC);
        for k in 1..FADE_FRAMES {
            assert!(fade[k] < fade[k - 1], "monotonic at fade frame {k}");
        }
        assert!(fade[FADE_FRAMES..].iter().all(|&s| s == -0.5));
        for _ in 0..5 {
            let r = rig.block();
            assert_eq!(
                r,
                BlockReport {
                    ran_model: false,
                    stage: Stage::Passthrough
                }
            );
            assert_eq!(rig.out, rig.pass);
        }
    }

    #[test]
    fn model_error_falls_back() {
        let resets = Arc::new(AtomicUsize::new(0));
        // Fail during the fade-in, when the accompaniment is already audible.
        let fail_at = WARM_BLOCKS + 3;
        let sep = FailSep {
            fail_at,
            calls: 0,
            resets: resets.clone(),
        };
        let mut rig = Rig::new(Processor::new(Some(Box::new(sep))), neg_half);
        rig.p.request_devocal(true);
        let mut last = None;
        for _ in 1..fail_at {
            last = Some(rig.block());
        }
        assert_eq!(last.unwrap().stage, Stage::FadingIn);
        let r = rig.block();
        assert_eq!(
            r,
            BlockReport {
                ran_model: true,
                stage: Stage::Fallback
            }
        );
        assert_eq!(rig.out, rig.pass, "the failing block is passthrough");
        assert_eq!(rig.p.fallback_reason(), Some(FallbackReason::ModelError));
        for _ in 0..5 {
            let r = rig.block();
            assert_eq!(
                r,
                BlockReport {
                    ran_model: false,
                    stage: Stage::Fallback
                }
            );
            assert_eq!(rig.out, rig.pass);
        }
        // Only a new "on" request leaves Fallback; it resets the poisoned model first.
        rig.p.request_devocal(false);
        assert_eq!(rig.block().stage, Stage::Fallback);
        assert_eq!(rig.p.fallback_reason(), Some(FallbackReason::ModelError));
        assert_eq!(resets.load(Ordering::SeqCst), 1);
        rig.p.request_devocal(true);
        assert_eq!(resets.load(Ordering::SeqCst), 2);
        assert_eq!(rig.p.fallback_reason(), None);
        assert_eq!(
            rig.block(),
            BlockReport {
                ran_model: true,
                stage: Stage::WarmingUp
            }
        );
    }

    #[test]
    fn non_finite_model_output_is_a_model_error() {
        let mut rig = Rig::new(Processor::new(Some(Box::new(NanSep))), neg_half);
        rig.p.request_devocal(true);
        let r = rig.block();
        assert_eq!(
            r,
            BlockReport {
                ran_model: true,
                stage: Stage::Fallback
            }
        );
        assert_eq!(rig.out, rig.pass);
        assert_eq!(rig.p.fallback_reason(), Some(FallbackReason::ModelError));
    }

    #[test]
    fn non_finite_input_never_reaches_the_output() {
        let (sep, _) = const_sep();
        let mut p = Processor::new(Some(sep));
        p.request_devocal(true);
        let input = [f32::NAN, f32::INFINITY]
            .iter()
            .copied()
            .cycle()
            .take(HOP * 2)
            .collect::<Vec<_>>();
        let mut out = vec![0.0f32; HOP * 2];
        for _ in 0..(WARM_BLOCKS + 20) {
            p.process_block(&input, &mut out);
            assert!(out.iter().all(|s| s.is_finite()));
        }
    }

    /// Largest frame-to-frame change a single equal-power fade step can cause, for a
    /// passthrough bounded by `max_pass` and the constant accompaniment.
    fn fade_step(max_pass: f32) -> f32 {
        (0..FADE_FRAMES)
            .map(|k| {
                let (o0, i0) = equal_power(k as f32 / FADE_FRAMES as f32);
                let (o1, i1) = equal_power((k + 1) as f32 / FADE_FRAMES as f32);
                (o1 - o0).abs() * max_pass + (i1 - i0).abs() * ACC
            })
            .fold(0.0, f32::max)
    }

    /// Review Focus 5: on/off/on/off/on, 3 blocks apart, from passthrough, from the start
    /// of the fade-in and from devocal; output never jumps and ends in devocal.
    #[test]
    fn rapid_toggle_never_jumps() {
        enum Start {
            Passthrough,
            FadeInBegins,
            Devocal,
        }
        for start in [Start::Passthrough, Start::FadeInBegins, Start::Devocal] {
            let (sep, _) = const_sep();
            let mut rig = Rig::new(Processor::new(Some(sep)), slow_sine);
            let mut left = Vec::new();
            let name = match start {
                Start::Passthrough => "passthrough",
                Start::FadeInBegins => {
                    rig.p.request_devocal(true);
                    left.extend(rig.run_until(Stage::FadingIn));
                    "fade-in"
                }
                Start::Devocal => {
                    rig.p.request_devocal(true);
                    left.extend(rig.run_until(Stage::Devocal));
                    "devocal"
                }
            };
            let mut reversed = false;
            for on in [true, false, true, false, true] {
                rig.p.request_devocal(on);
                for _ in 0..3 {
                    let r = rig.block();
                    reversed |= matches!(r.stage, Stage::FadingIn | Stage::FadingOut);
                    left.extend(rig.left());
                }
            }
            let mut last = None;
            for _ in 0..(WARM_BLOCKS + 20) {
                last = Some(rig.block());
                left.extend(rig.left());
            }
            assert_eq!(last.unwrap().stage, Stage::Devocal, "{name}");
            if !matches!(start, Start::Passthrough) {
                assert!(reversed, "{name}: toggles hit a fade");
            }
            let input_diff = (0..left.len())
                .map(|n| (slow_sine(n + 1) - slow_sine(n)).abs())
                .fold(0.0, f32::max);
            let bound = fade_step(0.5) * 2.0 + input_diff;
            let (at, max_diff) = left
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .enumerate()
                .fold((0, 0.0f32), |a, (i, d)| if d > a.1 { (i, d) } else { a });
            assert!(
                max_diff <= bound,
                "{name}: jump {max_diff} at frame {at} exceeds {bound}"
            );
        }
    }

    #[test]
    fn warm_up_cancel_is_immediate_passthrough() {
        let (sep, _) = const_sep();
        let mut rig = Rig::new(Processor::new(Some(sep)), ramp);
        rig.p.request_devocal(true);
        for _ in 0..3 {
            assert_eq!(rig.block().stage, Stage::WarmingUp);
        }
        rig.p.request_devocal(false);
        let r = rig.block();
        assert_eq!(
            r,
            BlockReport {
                ran_model: false,
                stage: Stage::Passthrough
            }
        );
        assert_eq!(rig.out, rig.pass);
    }

    #[test]
    fn overload_fades_out_into_fallback() {
        let (sep, resets) = const_sep();
        let mut rig = Rig::new(Processor::new(Some(sep)), neg_half);
        // Nothing to fall back from while devocal is off.
        rig.p.force_fallback(FallbackReason::Overload);
        assert_eq!(rig.p.fallback_reason(), None);
        assert_eq!(rig.block().stage, Stage::Passthrough);

        rig.p.request_devocal(true);
        rig.run_until(Stage::Devocal);
        rig.p.force_fallback(FallbackReason::Overload);
        assert_eq!(rig.p.fallback_reason(), Some(FallbackReason::Overload));
        let mut fade = Vec::new();
        for b in 0..7 {
            let r = rig.block();
            assert!(r.ran_model);
            let want = if b < 6 {
                Stage::FadingOut
            } else {
                Stage::Fallback
            };
            assert_eq!(r.stage, want, "block {b}");
            fade.extend(rig.left());
        }
        assert_eq!(fade[0], ACC);
        for k in 1..FADE_FRAMES {
            assert!(fade[k] < fade[k - 1], "monotonic at fade frame {k}");
        }
        // Repeated overload reports and "off" requests keep it in Fallback.
        rig.p.force_fallback(FallbackReason::Overload);
        rig.p.request_devocal(false);
        for _ in 0..3 {
            let r = rig.block();
            assert_eq!(
                r,
                BlockReport {
                    ran_model: false,
                    stage: Stage::Fallback
                }
            );
            assert_eq!(rig.out, rig.pass);
        }
        assert_eq!(rig.p.fallback_reason(), Some(FallbackReason::Overload));
        let before = resets.load(Ordering::SeqCst);
        rig.p.request_devocal(true);
        assert_eq!(resets.load(Ordering::SeqCst), before + 1);
        assert_eq!(rig.p.fallback_reason(), None);
        assert_eq!(rig.block().stage, Stage::WarmingUp);
    }

    #[test]
    fn fallback_during_warm_up_is_immediate() {
        let (sep, _) = const_sep();
        let mut rig = Rig::new(Processor::new(Some(sep)), ramp);
        rig.p.request_devocal(true);
        rig.block();
        rig.p.force_fallback(FallbackReason::Overload);
        let r = rig.block();
        assert_eq!(
            r,
            BlockReport {
                ran_model: false,
                stage: Stage::Fallback
            }
        );
        assert_eq!(rig.out, rig.pass);
    }

    #[test]
    fn set_separator_returns_to_passthrough_with_its_latency() {
        let (sep, _) = const_sep();
        let mut rig = Rig::new(Processor::new(Some(sep)), ramp);
        rig.p.request_devocal(true);
        rig.run_until(Stage::Devocal);
        rig.p.force_fallback(FallbackReason::Overload);
        rig.run_until(Stage::Fallback);

        let old = rig.p.set_separator(Box::new(DelayOnly::new(300)));
        assert!(old.is_some(), "the previous model is handed back");
        assert_eq!(rig.p.latency_frames(), 300);
        assert_eq!(rig.p.stage(), Stage::Passthrough);
        assert_eq!(rig.p.fallback_reason(), None);
        assert_eq!(rig.p.hop(), 128);
        rig.reference = DelayLine::new(300);
        for _ in 0..10 {
            let r = rig.block();
            assert_eq!(
                r,
                BlockReport {
                    ran_model: false,
                    stage: Stage::Passthrough
                }
            );
            assert_eq!(rig.out, rig.pass);
        }
        rig.p.request_devocal(true);
        assert_eq!(rig.block().stage, Stage::WarmingUp);
    }

    #[test]
    fn request_without_model_stays_passthrough() {
        let mut rig = Rig::new(Processor::new(None), ramp);
        rig.p.request_devocal(true);
        let r = rig.block();
        assert_eq!(
            r,
            BlockReport {
                ran_model: false,
                stage: Stage::Passthrough
            }
        );
        assert_eq!(rig.out, rig.pass);
    }

    #[test]
    fn discontinuity_resets_model() {
        let (sep, resets) = const_sep();
        let mut rig = Rig::new(Processor::new(Some(sep)), ramp);
        for _ in 0..3 {
            rig.block();
        }
        assert_eq!(resets.load(Ordering::SeqCst), 0);
        rig.p.on_discontinuity();
        assert_eq!(resets.load(Ordering::SeqCst), 1);
        rig.block();
        assert!(
            rig.out.iter().all(|&s| s == 0.0),
            "delay line was cleared too"
        );
    }

    #[test]
    fn discontinuity_restarts_warm_up() {
        let (sep, resets) = const_sep();
        let mut rig = Rig::new(Processor::new(Some(sep)), ramp);
        rig.p.request_devocal(true);
        for _ in 0..10 {
            rig.block();
        }
        rig.p.on_discontinuity();
        assert_eq!(resets.load(Ordering::SeqCst), 2);
        rig.reference.reset();
        for b in 0..WARM_BLOCKS - 1 {
            let r = rig.block();
            assert_eq!(r.stage, Stage::WarmingUp, "block {b} after the reset");
            assert_eq!(rig.out, rig.pass);
        }
        assert_eq!(rig.block().stage, Stage::FadingIn);
    }

    #[test]
    fn wrong_block_length_does_not_panic() {
        let (sep, _) = const_sep();
        let mut p = Processor::new(Some(sep));
        let mut short_out = vec![9.0f32; 100];
        let r = p.process_block(&[0.5; HOP * 2], &mut short_out);
        assert_eq!(r.stage, Stage::Passthrough);
        assert!(short_out.iter().all(|s| s.is_finite()));
        let mut out = vec![9.0f32; HOP * 2];
        p.process_block(&[0.5; 10], &mut out);
        assert!(out.iter().all(|s| s.is_finite()));

        p.request_devocal(true);
        p.process_block(&[0.5; HOP * 2], &mut out);
        let r = p.process_block(&[0.5; 10], &mut out);
        assert_eq!(r.stage, Stage::Fallback);
        assert_eq!(p.fallback_reason(), Some(FallbackReason::ModelError));
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn process_block_does_not_allocate() {
        let (sep, _) = const_sep();
        let mut p = Processor::new(Some(sep));
        let input = vec![0.1f32; HOP * 2];
        let mut out = vec![0.0f32; HOP * 2];
        let before = alloc_count::this_thread();
        p.request_devocal(true);
        for i in 0..(WARM_BLOCKS + 30) {
            if i == WARM_BLOCKS + 2 {
                p.request_devocal(false);
            }
            if i == WARM_BLOCKS + 4 {
                p.request_devocal(true);
            }
            if i == WARM_BLOCKS + 20 {
                p.force_fallback(FallbackReason::Overload);
            }
            p.process_block(&input, &mut out);
        }
        p.on_discontinuity();
        p.process_block(&input, &mut out);
        assert_eq!(alloc_count::this_thread() - before, 0);
    }

    #[test]
    fn take_separator_returns_the_model_and_leaves_a_plain_delay() {
        let mut p = Processor::new(Some(Box::new(DelayOnly::new(256))));
        assert_eq!(p.latency_frames(), 256);
        p.request_devocal(true);
        let model = p.take_separator();
        assert_eq!(model.map(|m| m.latency_frames()), Some(256));
        assert_eq!(p.stage(), Stage::Passthrough);
        assert_eq!(p.latency_frames(), NO_MODEL_LATENCY_FRAMES);
        assert_eq!(p.hop(), NO_MODEL_HOP);
        assert!(p.take_separator().is_none());
    }
}
