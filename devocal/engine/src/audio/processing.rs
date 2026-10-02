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
    edge_fade_frames, now_us, AudioStats, FadeIn, InputMarker, Mmcss, ProcCommand, Shared,
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

    'outer: while !ctx.shared.stop.load(Ordering::Acquire) {
        while let Ok(cmd) = ctx.control.pop() {
            apply_command(&mut ctx, &mut st, cmd);
        }
        try_swap(&mut ctx, &mut st);

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
        let underruns = ctx.stats.underruns.load(Ordering::Acquire);
        if report.ran_model && st.user_on {
            if st.load.overloaded() {
                ctx.processor.force_fallback(FallbackReason::Overload);
                st.load.reset();
            } else if underruns > st.underrun_base {
                // A dropout while input was flowing (counted by the render thread).
                ctx.processor.force_fallback(FallbackReason::Overload);
            }
        }
        st.underrun_base = underruns;
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
            st.pending = Some(s);
        }
    }
}

/// A user "on" was accepted: fresh load window and underrun baseline (obligation 2).
fn accept_on(ctx: &ProcessingCtx, st: &mut State) {
    st.load.reset();
    st.underrun_base = ctx.stats.underruns.load(Ordering::Acquire);
}

fn try_swap(ctx: &mut ProcessingCtx, st: &mut State) {
    if st.pending.is_none() {
        return;
    }
    let audible = matches!(
        ctx.processor.stage(),
        Stage::FadingOut | Stage::FadingIn | Stage::Devocal
    );
    let idle = now_us().saturating_sub(st.last_block_us) > SWAP_IDLE_US;
    if audible && !idle {
        return;
    }
    let Some(s) = st.pending.take() else {
        return;
    };
    // Allocates (and drops the old model) between blocks; not on the per-block path.
    ctx.processor.set_separator(s);
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
}

fn publish_config(ctx: &ProcessingCtx, st: &State) {
    ctx.shared
        .proc_latency_frames
        .store(st.latency as u32, Ordering::Relaxed);
    ctx.shared.proc_hop.store(st.hop as u32, Ordering::Relaxed);
}
