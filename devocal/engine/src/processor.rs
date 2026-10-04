//! Block processor on the engine's processing thread: one hop of interleaved stereo `f32`
//! per call. The dry (passthrough) path reads one ring of the input at the model's
//! `passthrough_latency_frames()` `P` or at its `latency_frames()` `M`. For StemgenRT
//! `P == M`, so switching never shifts the audio. Window models keep `P` at one block; then
//! (spec 4.2) switching neither repeats nor skips:
//!
//! - On: after the warm-up the dry path fades out, `M - D` frames of silence follow (`D`:
//!   the dry path's current delay), and the accompaniment fades in from the input frame
//!   after the last dry one. With `M == D` it is a plain crossfade; with `M < D` (ruling 14)
//!   too, skipping `D - M`.
//! - Off (and fallback): crossfade to the dry path at `M`, which then stays held at `M`
//!   (model idle) until the `M - P` frames a drop to `P` would skip, and the frame before
//!   them, are silent (below -80 dBFS), or until a discontinuity.
//!
//! Stages (`fade` = 20 ms equal-power crossfade between dry path and accompaniment):
//!
//! ```text
//!   Passthrough --on--> WarmingUp --200 ms--> FadingIn ------[gap]--fade--> Devocal
//!   (dry at P, or  <--off (gain is 0)--'        |  ^    (gap if M > D: dry   |
//!    held at M)                             off |  | on  fades out, M - D    | off
//!        ^                                      v  |    frames of silence)   |
//!        '------------- fade done ---------- FadingOut <---------------------'
//!                                               |  (crossfade to dry at M;
//!   Fallback --on (reset model)--> WarmingUp    v   force_fallback: -> Fallback)
//!   Fallback <------------------------- fade done
//!
//!   Passthrough / Fallback / WarmingUp: dry held at M --silence or discontinuity--> P
//! ```
//!
//! Reversing direction during a fade continues from the current gain; "off" during the gap
//! lets the silence run out and fades the dry path in at `M`. Any model error (or non-finite
//! model output, or a malformed block while the model runs) outputs that block as dry and
//! enters `Fallback` at once; the model is `reset()` before it runs again.

use devocal_core::protocol::FallbackReason;

use crate::dsp::{equal_power, frames_for_ms, Crossfade};
use crate::separator::Separator;

/// Hop and passthrough latency without a model, matching StemgenRT so that loading the
/// model later does not change the timing.
pub const NO_MODEL_HOP: usize = 128;
pub const NO_MODEL_LATENCY_FRAMES: usize = 128;
/// The model runs (in parallel with the passthrough) this long before the fade-in (longer
/// if its latency is longer).
pub const WARM_UP_MS: f32 = 200.0;
/// Equal-power crossfade between passthrough and accompaniment.
pub const FADE_MS: f32 = 20.0;
/// Peak below -80 dBFS on every channel: silent enough to drop a held delay in.
const SILENCE: f32 = 1e-4;

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

/// The dry path: a ring of the input read at `delay` (`p`, or `m` while held), with its own
/// equal-power gain ramp and the enable gap. Allocates only in `new`.
struct DryPath {
    ring: Vec<f32>,
    /// Ring slot (frame) of the next input frame.
    pos: usize,
    p: usize,
    m: usize,
    delay: usize,
    /// Gain position `0..=fade`; gain `sin(pi/2 * gain / fade)`, moving towards `up`.
    gain: usize,
    fade: usize,
    up: bool,
    /// Enable gap: the gain falls to 0, then this many silent frames, then `delay = m`.
    gap: Option<usize>,
    /// Consecutive silent frames read at tap `p`, up to the previous frame.
    quiet: usize,
}

impl DryPath {
    fn new(p: usize, m: usize, fade: usize) -> Self {
        Self {
            ring: vec![0.0; (p.max(m) + 1) * 2],
            pos: 0,
            p,
            m,
            delay: p,
            gain: fade,
            fade,
            up: true,
            gap: None,
            quiet: 0,
        }
    }

    /// Clears the ring; full gain at `delay`.
    fn reset(&mut self, delay: usize) {
        self.ring.fill(0.0);
        self.pos = 0;
        self.delay = delay;
        self.gain = self.fade;
        self.up = true;
        self.gap = None;
        self.quiet = 0;
    }

    /// The accompaniment is about to fade in: if it is later than the dry path, pause for
    /// the difference instead of repeating it. During a gap the dry path stays muted after it.
    fn before_fade_in(&mut self) {
        if self.gap.is_none() && self.m > self.delay {
            self.gap = Some(self.m - self.delay);
        }
        if self.gap.is_some() {
            self.up = false;
        }
    }

    /// The accompaniment is audible: the dry path waits at its delay, at full gain.
    fn hold_model(&mut self) {
        self.delay = self.m;
        self.gain = self.fade;
        self.up = true;
        self.gap = None;
    }

    fn read(&self, delay: usize) -> [f32; 2] {
        let cap = self.ring.len() / 2;
        let i = (self.pos + cap - delay) % cap * 2;
        [self.ring[i], self.ring[i + 1]]
    }

    /// Replaces `buf` (whole frames) with the dry path; returns how many leading frames were
    /// in the gap. `may_drop`: the dry path alone is audible, so a held delay may drop.
    fn process(&mut self, buf: &mut [f32], may_drop: bool) -> usize {
        let cap = self.ring.len() / 2;
        let mut gap_frames = 0;
        for f in buf.as_chunks_mut::<2>().0 {
            self.ring[self.pos * 2..self.pos * 2 + 2].copy_from_slice(f);
            if self.gap == Some(0) && self.gain == 0 {
                self.gap = None;
                self.delay = self.m;
            }
            // The skipped frames and the one played before them are silent, so the output
            // goes from silence into the next input frame, as the input itself does.
            if may_drop
                && self.gap.is_none()
                && self.delay > self.p
                && self.quiet > self.delay - self.p
            {
                self.delay = self.p;
            }
            let at_p = self.read(self.p);
            self.quiet = if at_p.iter().all(|s| s.abs() < SILENCE) {
                self.quiet.saturating_add(1)
            } else {
                0
            };
            let g = self.gain_now();
            match self.gap {
                Some(left) => {
                    gap_frames += 1;
                    if self.gain > 0 {
                        self.gain -= 1;
                    } else {
                        self.gap = Some(left - 1);
                    }
                }
                None if self.up => self.gain = (self.gain + 1).min(self.fade),
                None => self.gain = self.gain.saturating_sub(1),
            }
            let x = self.read(self.delay);
            f[0] = x[0] * g;
            f[1] = x[1] * g;
            self.pos = (self.pos + 1) % cap;
        }
        gap_frames
    }

    fn gain_now(&self) -> f32 {
        if self.gain >= self.fade {
            1.0
        } else {
            equal_power(self.gain as f32 / self.fade as f32).1
        }
    }
}

pub struct Processor {
    separator: Option<Box<dyn Separator>>,
    hop: usize,
    /// The model's latency in frames (the accompaniment's delay).
    latency_frames: usize,
    dry: DryPath,
    stage: Stage,
    /// Set while falling back (fading out towards, or in, `Fallback`).
    fallback: Option<FallbackReason>,
    warm_frames: usize,
    warm_left: usize,
    /// Fade position in the current direction: `FadingIn` mixes dry -> accompaniment,
    /// `FadingOut` mixes accompaniment -> dry. Held at its start during the enable gap.
    fade: Crossfade,
    /// Sanitised copy of the input block (model input).
    input: Vec<f32>,
    /// Dry path block.
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
            dry: DryPath::new(
                NO_MODEL_LATENCY_FRAMES,
                NO_MODEL_LATENCY_FRAMES,
                fade_frames,
            ),
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
            None => p.configure(
                NO_MODEL_HOP,
                NO_MODEL_LATENCY_FRAMES,
                NO_MODEL_LATENCY_FRAMES,
            ),
        }
        p
    }

    /// Installs a new model: back to `Passthrough` (fallback cleared) with the dry path at
    /// the model's passthrough latency. Allocates; call off the audio path or between blocks.
    /// Returns the previous model so the caller can drop it off the audio thread (tearing
    /// down an inference session can take milliseconds).
    pub fn set_separator(&mut self, s: Box<dyn Separator>) -> Option<Box<dyn Separator>> {
        let hop = s.hop();
        let latency = s.latency_frames();
        let passthrough = s.passthrough_latency_frames();
        let old = self.separator.replace(s);
        self.configure(hop, latency, passthrough);
        old
    }

    /// Removes the model so it can be reused (e.g. after the processing thread ends); the
    /// processor is the no-model delay again.
    pub fn take_separator(&mut self) -> Option<Box<dyn Separator>> {
        let s = self.separator.take();
        if s.is_some() {
            self.configure(
                NO_MODEL_HOP,
                NO_MODEL_LATENCY_FRAMES,
                NO_MODEL_LATENCY_FRAMES,
            );
        }
        s
    }

    fn configure(&mut self, hop: usize, latency_frames: usize, passthrough_frames: usize) {
        self.hop = hop.max(1);
        self.latency_frames = latency_frames;
        self.dry = DryPath::new(passthrough_frames, latency_frames, self.fade.frames());
        self.stage = Stage::Passthrough;
        self.fallback = None;
        // At least the model's latency: until then a window model's output is still its
        // initial silence and must not be faded in.
        self.warm_frames = frames_for_ms(WARM_UP_MS)
            .max(latency_frames)
            .div_ceil(self.hop)
            * self.hop;
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
                    self.dry.before_fade_in();
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
                    self.dry.up = true;
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
                self.dry.up = true;
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

    /// The installed model reports that it cannot keep up ([`Separator::overloaded`]).
    pub fn model_overloaded(&self) -> bool {
        self.separator.as_ref().is_some_and(|s| s.overloaded())
    }

    /// The installed model's [`Separator::window_stats`].
    pub fn window_stats(&self) -> Option<(u64, f32)> {
        self.separator.as_ref().and_then(|s| s.window_stats())
    }

    /// Input discontinuity (seek, glitch): clears the model state and the dry path, which
    /// drops a held delay to `P` (it stays at `M` while the accompaniment is audible, so a
    /// later fade-out stays aligned). The stage is kept, except that a warm-up or fade-in in
    /// progress (re)starts the warm-up, so the fresh model state still gets the full warm-up
    /// before it becomes audible.
    pub fn on_discontinuity(&mut self) {
        if let Some(sep) = self.separator.as_mut() {
            sep.reset();
        }
        let delay = match self.stage {
            Stage::Devocal | Stage::FadingOut => self.latency_frames,
            _ => self.dry.p,
        };
        self.dry.reset(delay);
        // A fade-in in progress goes back to warm-up too: the reset window model is silent
        // again and the fade would complete into silence.
        if matches!(self.stage, Stage::WarmingUp | Stage::FadingIn) {
            self.warm_left = self.warm_frames;
            self.stage = Stage::WarmingUp;
        }
    }

    /// Block size in frames; `process_block` takes `hop() * 2` interleaved samples.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// The model's delay relative to the input in frames (the accompaniment's; see
    /// `passthrough_latency_frames` and `output_latency_frames`).
    pub fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    /// The dry path's delay relative to the input in frames when it is not held.
    pub fn passthrough_latency_frames(&self) -> usize {
        self.dry.p
    }

    /// Delay of what is audible now: the model's in `Devocal`/`FadingOut` and during the
    /// enable gap, otherwise the dry path's (`P`, or `M` while held).
    pub fn output_latency_frames(&self) -> usize {
        match self.stage {
            Stage::Devocal | Stage::FadingOut => self.latency_frames,
            _ if self.dry.gap.is_some() => self.latency_frames,
            _ => self.dry.delay,
        }
    }

    /// Current stage (changes immediately on `request_devocal`/`force_fallback`, and on
    /// `process_block`).
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Processes one block of `hop() * 2` samples. Never allocates, panics or outputs
    /// non-finite samples. A block of another length is passed through the dry path (the
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
        let dry_alone = self.dry_alone();
        let gap = self.dry.process(&mut self.pass, dry_alone) * 2;

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
                    self.dry.before_fade_in();
                    self.stage = Stage::FadingIn;
                }
            }
            Stage::FadingIn => {
                // The fade waits for the gap (the dry path fading out, then silence).
                output[..gap].copy_from_slice(&self.pass[..gap]);
                if self
                    .fade
                    .mix(&self.pass[gap..], &self.acc[gap..], &mut output[gap..])
                {
                    self.dry.hold_model();
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
        let n = input.len().min(output.len()) / 2 * 2;
        for (o, &s) in output[..n].iter_mut().zip(input) {
            *o = finite_or_zero(s);
        }
        let dry_alone = self.dry_alone();
        self.dry.process(&mut output[..n], dry_alone);
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

    /// Only the dry path is audible, so a held delay may drop to `P` (in silence).
    fn dry_alone(&self) -> bool {
        matches!(
            self.stage,
            Stage::Passthrough | Stage::Fallback | Stage::WarmingUp
        )
    }

    /// Model failure: the current block was the dry path, so go straight to `Fallback`
    /// without a fade (the accompaniment is unusable). The model is reset before it runs
    /// again (`request_devocal(true)` from `Fallback`).
    fn fail_now(&mut self) {
        self.fallback = Some(FallbackReason::ModelError);
        self.dry.up = true;
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

    /// Reports `Separator::overloaded` from a shared flag.
    struct OverloadSep(Arc<std::sync::atomic::AtomicBool>);

    impl Separator for OverloadSep {
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
            out.fill(ACC);
            Ok(())
        }
        fn reset(&mut self) {}
        fn overloaded(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn model_overload_is_read_from_the_installed_model() {
        assert!(!Processor::new(None).model_overloaded());
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = Processor::new(Some(Box::new(OverloadSep(flag.clone()))));
        assert!(!p.model_overloaded());
        flag.store(true, Ordering::Relaxed);
        assert!(p.model_overloaded());
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

    fn zero(_n: usize) -> f32 {
        0.0
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

    /// Window-model stand-in: latency 10000, passthrough 128. The accompaniment is silent
    /// for the first 10000 frames after a reset, then constant.
    #[derive(Default)]
    struct SplitSep {
        fed: usize,
    }

    impl Separator for SplitSep {
        fn sample_rate(&self) -> u32 {
            44_100
        }
        fn hop(&self) -> usize {
            HOP
        }
        fn latency_frames(&self) -> usize {
            10_000
        }
        fn passthrough_latency_frames(&self) -> usize {
            HOP
        }
        fn process(&mut self, input: &[f32], out: &mut [f32]) -> Result<(), String> {
            check_block(HOP, input, out)?;
            out.fill(if self.fed >= 10_000 { ACC } else { 0.0 });
            self.fed += HOP;
            Ok(())
        }
        fn reset(&mut self) {
            self.fed = 0;
        }
    }

    #[test]
    fn warm_up_covers_the_model_latency() {
        let mut rig = Rig::new(Processor::new(Some(Box::<SplitSep>::default())), zero);
        rig.p.request_devocal(true);
        // 10000 frames are fed after 78.125 blocks; the stage must outlast them (79 blocks).
        for _ in 0..78 {
            assert_eq!(rig.block().stage, Stage::WarmingUp);
        }
        assert_eq!(rig.block().stage, Stage::FadingIn);
        // The fade-in never fades into the model's initial silence: once the accompaniment
        // is audible it stays.
        let left = rig.run_until(Stage::Devocal);
        let first = left.iter().position(|&s| s != 0.0).unwrap();
        assert!(left[first..].iter().all(|&s| s != 0.0));
    }

    #[test]
    fn discontinuity_during_fade_in_rearms_the_warm_up() {
        let mut rig = Rig::new(Processor::new(Some(Box::<SplitSep>::default())), zero);
        rig.p.request_devocal(true);
        rig.run_until(Stage::FadingIn);
        rig.block();
        // The model is reset: a window model is silent again, so the fade must not go on.
        rig.p.on_discontinuity();
        assert_eq!(rig.p.stage(), Stage::WarmingUp);
        for _ in 0..78 {
            assert_eq!(rig.block().stage, Stage::WarmingUp);
        }
        assert_eq!(rig.block().stage, Stage::FadingIn);
        let left = rig.run_until(Stage::Devocal);
        let first = left.iter().position(|&s| s != 0.0).unwrap();
        assert!(left[first..].iter().all(|&s| s != 0.0));
    }

    #[test]
    fn passthrough_uses_its_own_latency() {
        let mut rig = Rig::new(Processor::new(Some(Box::<SplitSep>::default())), ramp);
        assert_eq!(rig.p.latency_frames(), 10_000);
        assert_eq!(rig.p.passthrough_latency_frames(), HOP);
        rig.block();
        // One hop of silence, then the input delayed by 128 frames.
        assert!(rig.out.iter().all(|&s| s == 0.0));
        let first_input = rig.input.clone();
        rig.block();
        assert_eq!(rig.out, first_input);
        for _ in 0..10 {
            rig.block();
            assert_eq!(rig.out, rig.pass);
        }
        // Warm-up is passthrough as well.
        rig.p.request_devocal(true);
        for _ in 0..WARM_BLOCKS - 1 {
            let r = rig.block();
            assert_eq!(r.stage, Stage::WarmingUp);
            assert_eq!(rig.out, rig.pass);
        }
    }

    #[test]
    fn devocal_after_fade_uses_model_latency() {
        let mut rig = Rig::new(Processor::new(Some(Box::<SplitSep>::default())), ramp);
        rig.p.request_devocal(true);
        rig.run_until(Stage::Devocal);
        rig.block();
        assert!(rig.out.iter().all(|&s| s == ACC));
        assert_eq!(rig.p.output_latency_frames(), 10_000);
        // Back to passthrough, held at the model's delay (the ramp input is never silent).
        rig.p.request_devocal(false);
        rig.run_until(Stage::Passthrough);
        rig.block();
        assert_eq!(rig.p.output_latency_frames(), 10_000);
    }

    #[test]
    fn published_latency_follows_stage() {
        let mut rig = Rig::new(Processor::new(Some(Box::<SplitSep>::default())), ramp);
        let model = 10_000;
        assert_eq!(rig.p.stage(), Stage::Passthrough);
        assert_eq!(rig.p.output_latency_frames(), HOP);
        rig.p.request_devocal(true);
        assert_eq!(rig.p.stage(), Stage::WarmingUp);
        assert_eq!(rig.p.output_latency_frames(), HOP);
        // The enable gap already plays at the model's delay.
        rig.run_until(Stage::FadingIn);
        assert_eq!(rig.p.output_latency_frames(), model);
        rig.run_until(Stage::Devocal);
        assert_eq!(rig.p.output_latency_frames(), model);
        rig.p.request_devocal(false);
        assert_eq!(rig.p.stage(), Stage::FadingOut);
        assert_eq!(rig.p.output_latency_frames(), model);
        // Passthrough holds the model's delay until silence or a discontinuity.
        rig.run_until(Stage::Passthrough);
        assert_eq!(rig.p.output_latency_frames(), model);
        rig.p.request_devocal(true);
        rig.run_until(Stage::Devocal);
        rig.p.force_fallback(FallbackReason::Overload);
        rig.run_until(Stage::Fallback);
        assert_eq!(rig.p.output_latency_frames(), model);
        rig.p.on_discontinuity();
        assert_eq!(rig.p.output_latency_frames(), HOP);
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

    /// Window-model latency for the content checks (passthrough stays at one hop).
    const M: usize = 10_000;

    /// Window-model stand-in for content checks: the accompaniment is the input delayed by
    /// `m`, so it is aligned with the dry path held at `m`. `process` errs while `fail` is set.
    struct IndexSep {
        m: usize,
        p: usize,
        delay: DelayLine,
        fail: Arc<AtomicBool>,
    }

    impl Separator for IndexSep {
        fn sample_rate(&self) -> u32 {
            44_100
        }
        fn hop(&self) -> usize {
            HOP
        }
        fn latency_frames(&self) -> usize {
            self.m
        }
        fn passthrough_latency_frames(&self) -> usize {
            self.p
        }
        fn process(&mut self, input: &[f32], out: &mut [f32]) -> Result<(), String> {
            check_block(HOP, input, out)?;
            if self.fail.load(Ordering::Relaxed) {
                return Err("inference failed".into());
            }
            out.copy_from_slice(input);
            self.delay.process(out);
            Ok(())
        }
        fn reset(&mut self) {
            self.delay.reset();
        }
    }

    fn index_sep(m: usize, p: usize) -> (Box<dyn Separator>, Arc<AtomicBool>) {
        let fail = Arc::new(AtomicBool::new(false));
        let sep = IndexSep {
            m,
            p,
            delay: DelayLine::new(m),
            fail: fail.clone(),
        };
        (Box::new(sep), fail)
    }

    /// Feeds input frame `n` as `(n + 1, 1)`, or silence inside `quiet`, so an output frame
    /// `(l, r)` carries input frame `l / r - 1` at any fade gain (as long as the frames mixed
    /// into it are the same input frame).
    struct Probe {
        p: Processor,
        quiet: Vec<std::ops::Range<usize>>,
        out: Vec<[f32; 2]>,
    }

    impl Probe {
        fn new(p: Processor) -> Self {
            Self {
                p,
                quiet: Vec::new(),
                out: Vec::new(),
            }
        }

        /// Next input frame = number of output frames so far.
        fn frame(&self) -> usize {
            self.out.len()
        }

        fn is_quiet(&self, n: usize) -> bool {
            self.quiet.iter().any(|q| q.contains(&n))
        }

        fn block(&mut self) -> BlockReport {
            let start = self.frame();
            let input: Vec<f32> = (start..start + HOP)
                .flat_map(|n| {
                    if self.is_quiet(n) {
                        [0.0, 0.0]
                    } else {
                        [(n + 1) as f32, 1.0]
                    }
                })
                .collect();
            let mut out = vec![9.0; HOP * 2];
            let r = self.p.process_block(&input, &mut out);
            self.out.extend(out.as_chunks::<2>().0.iter().copied());
            r
        }

        fn run(&mut self, blocks: usize) -> BlockReport {
            (0..blocks).map(|_| self.block()).last().unwrap()
        }

        fn run_until(&mut self, stage: Stage) {
            for _ in 0..1000 {
                if self.block().stage == stage {
                    return;
                }
            }
            panic!("never reached {stage:?}");
        }

        /// Input frame carried by output frame `o`; `None` if it is silent.
        fn content(&self, o: usize) -> Option<usize> {
            let [l, r] = self.out[o];
            (r != 0.0).then(|| (l / r).round() as usize - 1)
        }

        /// Audible output frames from `from` on, as `(output frame, input frame)`.
        fn audible(&self, from: usize) -> Vec<(usize, usize)> {
            (from..self.frame())
                .filter_map(|o| self.content(o).map(|n| (o, n)))
                .collect()
        }

        /// From output frame `from` on, input frames play in order, none twice, and none
        /// is skipped unless it is quiet.
        fn assert_continuous(&self, from: usize) {
            for w in self.audible(from).windows(2) {
                let ((o0, n0), (o1, n1)) = (w[0], w[1]);
                assert!(n1 > n0, "output frame {o1} plays input {n1} after {n0}");
                for n in n0 + 1..n1 {
                    assert!(
                        self.is_quiet(n),
                        "loud input frame {n} skipped between output frames {o0} and {o1}"
                    );
                }
            }
        }
    }

    /// Devocal on, then off: passthrough with the dry path held at the model's delay.
    fn held_probe() -> Probe {
        let (sep, _) = index_sep(M, HOP);
        let mut pr = Probe::new(Processor::new(Some(sep)));
        pr.p.request_devocal(true);
        pr.run_until(Stage::Devocal);
        pr.p.request_devocal(false);
        pr.run_until(Stage::Passthrough);
        pr
    }

    #[test]
    fn enable_with_larger_model_latency_pauses_instead_of_repeating() {
        let (sep, _) = index_sep(M, HOP);
        let mut pr = Probe::new(Processor::new(Some(sep)));
        pr.run(20);
        pr.p.request_devocal(true);
        let on_at = pr.frame();
        pr.run_until(Stage::FadingIn);
        assert_eq!(pr.p.output_latency_frames(), M, "the gap publishes M");
        pr.run_until(Stage::Devocal);
        pr.run(10);
        assert_eq!(pr.p.output_latency_frames(), M);

        let a = pr.audible(on_at);
        for &(o, n) in &a {
            assert!(
                o - n == HOP || o - n == M,
                "output frame {o} plays input {n}: neither the dry path nor the model"
            );
        }
        for w in a.windows(2) {
            assert!(w[1].1 > w[0].1, "output frame {} repeats input", w[1].0);
        }
        let (last_dry_o, last_dry) = *a.iter().rfind(|&&(o, n)| o - n == HOP).unwrap();
        let (first_model_o, first_model) = *a.iter().find(|&&(o, n)| o - n == M).unwrap();
        // The dry path fades out over FADE (its first frame still at full gain)...
        let fading = a
            .iter()
            .filter(|&&(o, n)| o - n == HOP && pr.out[o][1] < 1.0)
            .count();
        assert_eq!(fading, FADE_FRAMES - 1);
        // ...then M - P frames of silence. The next frame starts the model fade-in (at
        // equal-power gain 0, so it is silent too) with the input frame right after the last
        // dry one.
        assert_eq!(first_model_o - last_dry_o - 1, M - HOP + 1);
        assert_eq!(first_model_o - 1 - M, last_dry + 1);
        assert_eq!(first_model, last_dry + 2);
        // The fade-in completes into the model alone.
        assert!(a.iter().all(|&(o, n)| o < first_model_o || o - n == M));
        assert_eq!(pr.out[first_model_o + FADE_FRAMES][1], 1.0);
    }

    /// "Off" during the gap: the silence runs out and the dry path fades in at M, from the
    /// input frame after the last one played.
    #[test]
    fn disable_during_the_gap_resumes_the_dry_path_at_the_model_delay() {
        let (sep, _) = index_sep(M, HOP);
        let mut pr = Probe::new(Processor::new(Some(sep)));
        pr.run(20);
        pr.p.request_devocal(true);
        let on_at = pr.frame();
        pr.run_until(Stage::FadingIn);
        pr.run(3);
        pr.p.request_devocal(false);
        assert_eq!(pr.p.stage(), Stage::FadingOut);
        let r = pr.run(100);
        assert_eq!(
            r,
            BlockReport {
                ran_model: false,
                stage: Stage::Passthrough
            }
        );
        assert_eq!(pr.p.output_latency_frames(), M);
        let a = pr.audible(on_at);
        assert!(a.iter().all(|&(o, n)| o - n == HOP || o - n == M));
        let (last_dry_o, last_dry) = *a.iter().rfind(|&&(o, n)| o - n == HOP).unwrap();
        let (first_o, first) = *a.iter().find(|&&(o, n)| o - n == M).unwrap();
        assert_eq!(first_o - last_dry_o - 1, M - HOP + 1);
        assert_eq!(first, last_dry + 2);
        assert!(a.iter().all(|&(o, n)| o < first_o || o - n == M));
    }

    #[test]
    fn disable_holds_the_model_delay_without_skipping() {
        let (sep, _) = index_sep(M, HOP);
        let mut pr = Probe::new(Processor::new(Some(sep)));
        pr.p.request_devocal(true);
        pr.run_until(Stage::Devocal);
        pr.run(5);
        let off_at = pr.frame();
        pr.p.request_devocal(false);
        assert_eq!(pr.p.output_latency_frames(), M);
        pr.run_until(Stage::Passthrough);
        let r = pr.run(20);
        assert_eq!(
            r,
            BlockReport {
                ran_model: false,
                stage: Stage::Passthrough
            }
        );
        assert_eq!(pr.p.output_latency_frames(), M);
        let a = pr.audible(off_at);
        assert_eq!(a.len(), pr.frame() - off_at, "no silent frame");
        assert!(a.iter().all(|&(o, n)| o - n == M), "no input frame skipped");
    }

    #[test]
    fn delay_drops_to_passthrough_after_silence() {
        let mut pr = held_probe();
        let start = pr.frame();
        // The frames a drop skips plus the output frame before them must be silent.
        let k = M - HOP + 1;
        let blocks = (M + k + 2000) / HOP;
        // One loud frame inside the window restarts the count: no drop.
        let a0 = pr.frame() + 1000;
        pr.quiet.push(a0..a0 + k / 2);
        pr.quiet.push(a0 + k / 2 + 1..a0 + k);
        pr.run(blocks);
        assert_eq!(pr.p.output_latency_frames(), M);
        assert!(pr.audible(start).iter().all(|&(o, n)| o - n == M));

        // A clean window: the delay drops to P inside it.
        let b = pr.frame() + 1000;
        pr.quiet.push(b..b + k);
        pr.run(blocks);
        assert_eq!(pr.p.output_latency_frames(), HOP);
        let a = pr.audible(b);
        let first = *a.iter().find(|&&(o, n)| o - n == HOP).unwrap();
        // Silence, then the first loud frame after the window at once.
        assert_eq!(first, (b + M + 1, b + k));
        assert_eq!(pr.content(b + M), None);
        assert!(a
            .iter()
            .all(|&(o, n)| o - n == if o < b + M { M } else { HOP }));
        pr.assert_continuous(start);
    }

    #[test]
    fn discontinuity_drops_the_held_delay() {
        let mut pr = held_probe();
        assert_eq!(pr.p.output_latency_frames(), M);
        pr.p.on_discontinuity();
        assert_eq!(pr.p.output_latency_frames(), HOP);
        let at = pr.frame();
        pr.run(10);
        let a = pr.audible(at);
        assert_eq!(a[0].0, at + HOP, "the cleared passthrough delay first");
        assert!(a.iter().all(|&(o, n)| o - n == HOP));
    }

    #[test]
    fn reenable_while_delay_is_held_is_an_aligned_crossfade() {
        let mut pr = held_probe();
        let on_at = pr.frame();
        pr.p.request_devocal(true);
        pr.run_until(Stage::Devocal);
        pr.run(5);
        assert_eq!(pr.p.output_latency_frames(), M);
        let a = pr.audible(on_at);
        assert_eq!(a.len(), pr.frame() - on_at, "no gap");
        assert!(a.iter().all(|&(o, n)| o - n == M), "no repeat, no skip");
    }

    #[test]
    fn fallback_lands_on_the_model_aligned_dry_signal() {
        for error in [false, true] {
            let (sep, fail) = index_sep(M, HOP);
            let mut pr = Probe::new(Processor::new(Some(sep)));
            pr.p.request_devocal(true);
            pr.run_until(Stage::Devocal);
            pr.run(5);
            let at = pr.frame();
            if error {
                fail.store(true, Ordering::Relaxed);
                assert_eq!(pr.block().stage, Stage::Fallback);
            } else {
                pr.p.force_fallback(FallbackReason::Overload);
                pr.run_until(Stage::Fallback);
            }
            let r = pr.run(20);
            assert_eq!(
                r,
                BlockReport {
                    ran_model: false,
                    stage: Stage::Fallback
                }
            );
            assert_eq!(pr.p.output_latency_frames(), M, "error: {error}");
            let a = pr.audible(at);
            assert_eq!(a.len(), pr.frame() - at, "error: {error}");
            assert!(a.iter().all(|&(o, n)| o - n == M), "error: {error}");
        }
    }

    /// Ruling 14: a model with less latency than the dry path crossfades as before (and
    /// skips the difference).
    #[test]
    fn smaller_model_latency_keeps_the_crossfade() {
        let (sep, _) = index_sep(HOP, 300);
        let mut pr = Probe::new(Processor::new(Some(sep)));
        pr.run(10);
        pr.p.request_devocal(true);
        let on_at = pr.frame();
        pr.run_until(Stage::FadingIn);
        assert_eq!(pr.p.output_latency_frames(), 300);
        let fade_at = pr.frame();
        pr.run_until(Stage::Devocal);
        let devocal_at = pr.frame();
        pr.run(5);
        assert_eq!(pr.p.output_latency_frames(), HOP);
        assert!((on_at..pr.frame()).all(|o| pr.out[o][1] != 0.0), "no gap");
        assert!(pr
            .audible(on_at)
            .iter()
            .all(|&(o, n)| o >= fade_at || o - n == 300));
        assert!(pr.audible(devocal_at).iter().all(|&(o, n)| o - n == HOP));
    }

    #[test]
    fn gap_hold_and_drop_do_not_allocate() {
        let (sep, _) = index_sep(M, HOP);
        let mut p = Processor::new(Some(sep));
        let loud = vec![0.1f32; HOP * 2];
        let quiet = vec![0.0f32; HOP * 2];
        let mut out = vec![0.0f32; HOP * 2];
        let before = alloc_count::this_thread();
        p.request_devocal(true);
        for _ in 0..200 {
            p.process_block(&loud, &mut out);
        }
        assert_eq!(p.stage(), Stage::Devocal);
        p.request_devocal(false);
        for _ in 0..20 {
            p.process_block(&loud, &mut out);
        }
        assert_eq!(p.output_latency_frames(), M);
        for _ in 0..90 {
            p.process_block(&quiet, &mut out);
        }
        assert_eq!(p.output_latency_frames(), HOP);
        p.on_discontinuity();
        p.process_block(&loud, &mut out);
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
