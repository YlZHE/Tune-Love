//! Processing thread: takes hop-sized blocks from ring A, runs the `Processor`, times each
//! block for the `LoadMonitor`, and pushes the output to ring B. Control messages are applied
//! here, between blocks (caller obligation 4).
//!
//! Positions: `in_pos` counts frames taken from ring A (processed or discarded), `out_pos`
//! frames pushed to ring B. Input frame `x` of a block leaves the processor `latency` frames
//! later, at output frame `x + (out_pos - in_pos) + latency`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use devocal_core::protocol::FallbackReason;
use rtrb::{Consumer, Producer};

use super::{
    edge_fade_frames, is_backlogged, now_us, reason_code, stage_code, AudioStats, FadeIn,
    InputMarker, Mmcss, ProcCommand, Shared, Starvation,
};
use crate::dsp::SAMPLE_RATE;
use crate::load::LoadMonitor;
use crate::processor::{Processor, Stage};
use crate::separator::Separator;

/// LoadMonitor window (about 1 s of 128-frame blocks) and threshold.
pub const LOAD_WINDOW_BLOCKS: usize = 344;
pub const LOAD_THRESHOLD: f64 = 0.70;
/// A pending model swap waits for the fade-out, but not longer than this without input.
const SWAP_IDLE_US: u64 = 100_000;
const WAIT_MS: u32 = 5;

pub(crate) struct ProcessingCtx {
    pub processor: Processor,
    pub input: Consumer<f32>,
    pub in_markers: Consumer<InputMarker>,
    pub output: Producer<f32>,
    pub out_markers: Producer<u64>,
    pub control: Consumer<ProcCommand>,
    pub shared: Arc<Shared>,
    pub stats: Arc<AudioStats>,
}

/// Ruling 18 overload decision for one processed block. Never while the user toggle is off
/// or the model is not running. LoadMonitor overload always forces the fallback; a counted
/// underrun only when, at that starvation, the model ran and ring A held at least one hop
/// (processing was behind). Any other underrun is jitter, absorbed by render headroom.
pub fn should_force_fallback(
    underrun: Option<Starvation>,
    hop: usize,
    user_on: bool,
    ran_model: bool,
    overloaded: bool,
) -> bool {
    if !user_on || !ran_model {
        return false;
    }
    overloaded || underrun.is_some_and(|u| is_backlogged(u.ran_model, u.backlog_frames, hop))
}

fn block_us(hop: usize) -> f64 {
    hop as f64 * 1_000_000.0 / f64::from(SAMPLE_RATE)
}

struct State {
    hop: usize,
    latency: usize,
    in_block: Vec<f32>,
    out_block: Vec<f32>,
    in_pos: u64,
    out_pos: u64,
    load: LoadMonitor,
    /// The user's toggle as last received.
    user_on: bool,
    /// Model waiting for the fade-out before it is installed.
    pending: Option<Box<dyn Separator>>,
    /// Underrun count already accounted for.
    underrun_base: u64,
    /// Fades the output in after a reset (skips the cleared delay line first).
    fade_in: FadeIn,
    last_block_us: u64,
}

pub(crate) fn run(mut ctx: ProcessingCtx) {
    let _mmcss = Mmcss::pro_audio("processing");
    let hop = ctx.processor.hop();
    let mut st = State {
        hop,
        latency: ctx.processor.latency_frames(),
        in_block: vec![0.0; hop * 2],
        out_block: vec![0.0; hop * 2],
        in_pos: 0,
        out_pos: 0,
        load: LoadMonitor::new(block_us(hop), LOAD_WINDOW_BLOCKS, LOAD_THRESHOLD),
        user_on: false,
        pending: None,
        underrun_base: 0,
        fade_in: FadeIn::new(edge_fade_frames()),
        last_block_us: now_us(),
    };
    st.underrun_base = ctx.stats.underruns.load(Ordering::Acquire);
    publish_config(&ctx, &st);
    publish_state(&ctx);

    'outer: while !ctx.shared.stop.load(Ordering::Acquire) {
        while let Ok(cmd) = ctx.control.pop() {
            apply_command(&mut ctx, &mut st, cmd);
            publish_state(&ctx);
        }
        if try_swap(&mut ctx, &mut st) {
            publish_state(&ctx);
        }

        ctx.shared
            .in_ring_frames
            .store((ctx.input.slots() / 2) as u32, Ordering::Relaxed);
        if ctx.input.slots() < st.hop * 2 {
            ctx.shared.wake.wait(WAIT_MS);
            continue;
        }
        // Markers were pushed before their samples, so every marker inside this block is
        // visible now (the acquire in `slots()` above orders the two rings).
        while let Ok(&m) = ctx.in_markers.peek() {
            match m {
                InputMarker::Discontinuity(at) => {
                    if at >= st.in_pos + st.hop as u64 {
                        break;
                    }
                    let _ = ctx.in_markers.pop();
                    // Re-chunk at the discontinuity: drop the frames before it.
                    let skip = at.saturating_sub(st.in_pos) as usize;
                    if skip > 0 {
                        if let Ok(chunk) = ctx.input.read_chunk(skip * 2) {
                            chunk.commit_all();
                        }
                        st.in_pos += skip as u64;
                    }
                    ctx.processor.on_discontinuity();
                    begin_gap(&mut ctx, &mut st);
                    continue 'outer;
                }
                InputMarker::GuardGap(at) => {
                    if at >= st.in_pos + st.hop as u64 {
                        break;
                    }
                    let _ = ctx.in_markers.pop();
                    let out =
                        at as i128 + st.out_pos as i128 - st.in_pos as i128 + st.latency as i128;
                    let out = out.max(st.out_pos as i128) as u64;
                    let _ = ctx.out_markers.push(out);
                }
            }
        }
        if ctx.input.pop_entire_slice(&mut st.in_block).is_err() {
            continue;
        }
        st.in_pos += st.hop as u64;

        let t0 = now_us();
        let report = ctx.processor.process_block(&st.in_block, &mut st.out_block);
        let t1 = now_us();
        st.last_block_us = t1;
        st.fade_in.apply(&mut st.out_block);

        // Load: only blocks that ran the model count; otherwise start a fresh window
        // (caller obligation 2).
        if report.ran_model {
            st.load.record((t1 - t0) as f64);
        } else {
            st.load.reset();
        }
        // The render thread publishes the starvation snapshot before incrementing the count.
        let underruns = ctx.stats.underruns.load(Ordering::Acquire);
        let underrun = (underruns > st.underrun_base).then(|| Starvation {
            ran_model: ctx.shared.underrun_ran_model.load(Ordering::Acquire),
            backlog_frames: ctx.shared.underrun_backlog_frames.load(Ordering::Acquire) as usize,
        });
        let overloaded = report.ran_model && st.load.overloaded();
        if should_force_fallback(underrun, st.hop, st.user_on, report.ran_model, overloaded) {
            ctx.processor.force_fallback(FallbackReason::Overload);
            if overloaded {
                st.load.reset();
            }
        }
        st.underrun_base = underruns;
        publish_state(&ctx);
        let ratio = if report.ran_model {
            st.load.ratio()
        } else {
            0.0
        };
        ctx.stats
            .load_ratio_milli
            .store((ratio * 1000.0).round() as u32, Ordering::Relaxed);

        if ctx.output.slots() >= st.hop * 2 {
            let _ = ctx.output.push_entire_slice(&st.out_block);
            st.out_pos += st.hop as u64;
        } else {
            // Ring B full (render stalled): drop the block, fade around the jump.
            let _ = ctx.out_markers.push(st.out_pos);
            st.fade_in.start(0);
        }
    }
}

/// Output jumps at `out_pos` (delay line / model state cleared): the render thread fades out
/// the frames before it, and the output fades in after the cleared delay.
fn begin_gap(ctx: &mut ProcessingCtx, st: &mut State) {
    let _ = ctx.out_markers.push(st.out_pos);
    st.fade_in.start(st.latency);
}

fn apply_command(ctx: &mut ProcessingCtx, st: &mut State, cmd: ProcCommand) {
    match cmd {
        ProcCommand::SetDevocal(on) => {
            st.user_on = on;
            // While a model swap is pending the request is applied after the swap.
            if st.pending.is_none() {
                ctx.processor.request_devocal(on);
                if on {
                    accept_on(ctx, st);
                }
            }
        }
        ProcCommand::SetSeparator(s) => {
            // Caller obligation 3: off first, swap once the fade-out is over.
            ctx.processor.request_devocal(false);
            if let Some(replaced) = st.pending.replace(s) {
                retire(replaced);
            }
        }
    }
}

/// A user "on" was accepted: fresh load window and underrun baseline (obligation 2), and a
/// one-time render pre-roll (ruling 18).
fn accept_on(ctx: &ProcessingCtx, st: &mut State) {
    st.load.reset();
    st.underrun_base = ctx.stats.underruns.load(Ordering::Acquire);
    ctx.shared.preroll_request.store(true, Ordering::Release);
}

/// Installs a pending model once it is safe; returns true if it did.
fn try_swap(ctx: &mut ProcessingCtx, st: &mut State) -> bool {
    if st.pending.is_none() {
        return false;
    }
    let audible = matches!(
        ctx.processor.stage(),
        Stage::FadingOut | Stage::FadingIn | Stage::Devocal
    );
    let idle = now_us().saturating_sub(st.last_block_us) > SWAP_IDLE_US;
    if audible && !idle {
        return false;
    }
    let Some(s) = st.pending.take() else {
        return false;
    };
    // Allocates (and drops the old model) between blocks; not on the per-block path.
    if let Some(old) = ctx.processor.set_separator(s) {
        retire(old);
    }
    st.hop = ctx.processor.hop();
    st.latency = ctx.processor.latency_frames();
    // Re-chunk at the new hop: nothing partial is held (blocks are read whole from ring A).
    st.in_block = vec![0.0; st.hop * 2];
    st.out_block = vec![0.0; st.hop * 2];
    st.load = LoadMonitor::new(block_us(st.hop), LOAD_WINDOW_BLOCKS, LOAD_THRESHOLD);
    // The new delay line starts empty: fade around the jump.
    begin_gap(ctx, st);
    publish_config(ctx, st);
    if st.user_on {
        ctx.processor.request_devocal(true);
        accept_on(ctx, st);
    }
    true
}

/// Publishes the processor's stage and fallback reason (caller obligation 5).
fn publish_state(ctx: &ProcessingCtx) {
    ctx.stats
        .stage
        .store(stage_code(ctx.processor.stage()), Ordering::Release);
    ctx.stats.fallback_reason.store(
        reason_code(ctx.processor.fallback_reason()),
        Ordering::Release,
    );
}

/// Drops a replaced model on a short-lived thread: tearing down an inference session can
/// take milliseconds, which would starve the output (and, with input flowing, count as an
/// underrun against the new model). If no thread can be spawned, the closure (and the model)
/// is dropped here instead.
fn retire(old: Box<dyn Separator>) {
    let _ = std::thread::Builder::new()
        .name("devocal-model-drop".into())
        .spawn(move || drop(old));
}

fn publish_config(ctx: &ProcessingCtx, st: &State) {
    ctx.shared
        .proc_latency_frames
        .store(st.latency as u32, Ordering::Relaxed);
    ctx.shared.proc_hop.store(st.hop as u32, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOP: usize = 128;

    fn starved(ran_model: bool, backlog_frames: usize) -> Option<Starvation> {
        Some(Starvation {
            ran_model,
            backlog_frames,
        })
    }

    #[test]
    fn jitter_underrun_does_not_force_fallback() {
        // Model running, but ring A held less than a hop: processing kept up.
        assert!(!should_force_fallback(
            starved(true, 0),
            HOP,
            true,
            true,
            false
        ));
        assert!(!should_force_fallback(
            starved(true, HOP - 1),
            HOP,
            true,
            true,
            false
        ));
        // Starved while the model was not running (passthrough).
        assert!(!should_force_fallback(
            starved(false, 4 * HOP),
            HOP,
            true,
            true,
            false
        ));
        assert!(!should_force_fallback(None, HOP, true, true, false));
    }

    #[test]
    fn backlogged_underrun_forces_fallback() {
        assert!(should_force_fallback(
            starved(true, HOP),
            HOP,
            true,
            true,
            false
        ));
        assert!(should_force_fallback(
            starved(true, 10 * HOP),
            HOP,
            true,
            true,
            false
        ));
    }

    #[test]
    fn load_overload_forces_fallback() {
        assert!(should_force_fallback(None, HOP, true, true, true));
        assert!(should_force_fallback(
            starved(true, 0),
            HOP,
            true,
            true,
            true
        ));
    }

    #[test]
    fn nothing_forced_while_user_off_or_model_idle() {
        assert!(!should_force_fallback(
            starved(true, 10 * HOP),
            HOP,
            false,
            true,
            true
        ));
        assert!(!should_force_fallback(
            starved(true, 10 * HOP),
            HOP,
            true,
            false,
            true
        ));
    }
}
