//! Processing thread: takes hop-sized blocks from ring A, runs the `Processor`, times each
//! block for the `LoadMonitor`, and pushes the output to ring B, signalling
//! `Shared::render_wake` after each block so the render thread writes it on arrival (O2).
//! Control messages are applied here, between blocks (caller obligation 4).
//!
//! The block being processed is published as in flight (`Shared::proc_in_flight_frames`) for
//! the latency estimate: with the model running it is out of both rings for most of a hop.
//!
//! Positions: `in_pos` counts frames taken from ring A (processed or discarded), `out_pos`
//! frames pushed to ring B. Input frame `x` of a block leaves the processor `latency` frames
//! later, at output frame `x + (out_pos - in_pos) + latency`; `latency` is the audible path's
//! (`Processor::output_latency_frames`: passthrough or model, by stage).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use devocal_core::protocol::FallbackReason;
use rtrb::{Consumer, Producer};

use super::{
    edge_fade_frames, now_us, reason_code, stage_code, unpack_starvation, AudioStats, FadeIn,
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

/// Overload decision for one processed block (rulings 18 and 24). Never while the user
/// toggle is off, the model is not running, or the processor is warming up (its output is
/// the passthrough). LoadMonitor overload forces the fallback; a counted underrun only when
/// the render thread judged it forcing (`Starvation::force`: processing behind again within
/// `render::OVERLOAD_REPEAT_US`, or with the jitter headroom at its cap). Any other underrun
/// is absorbed by render headroom.
pub fn should_force_fallback(
    underrun: Option<Starvation>,
    user_on: bool,
    ran_model: bool,
    overloaded: bool,
    stage: Stage,
) -> bool {
    if !user_on || !ran_model || stage == Stage::WarmingUp {
        return false;
    }
    overloaded || underrun.is_some_and(|u| u.force)
}

/// Ruling 24: an overload fallback is retried (devocal on again, with a pre-roll) this long
/// after it was forced...
pub const OVERLOAD_RETRY_DELAY_US: u64 = 5_000_000;
/// ...at most this many times per user "on"; once spent, the fallback stays.
pub const OVERLOAD_RETRIES: u32 = 1;

/// Ruling 24: when to retry devocal after a forced overload fallback. Pure; times in us.
#[derive(Debug)]
pub(crate) struct OverloadRetry {
    due_us: Option<u64>,
    left: u32,
}

impl OverloadRetry {
    pub fn new() -> Self {
        Self {
            due_us: None,
            left: OVERLOAD_RETRIES,
        }
    }

    /// A user "on": a fresh retry budget, nothing scheduled.
    pub fn user_on(&mut self) {
        *self = Self::new();
    }

    /// A user "off": nothing scheduled.
    pub fn user_off(&mut self) {
        self.due_us = None;
    }

    /// An overload fallback was forced at `now_us`; true if a retry is scheduled.
    pub fn forced(&mut self, now_us: u64) -> bool {
        self.due_us = (self.left > 0).then_some(now_us + OVERLOAD_RETRY_DELAY_US);
        self.due_us.is_some()
    }

    pub fn pending(&self) -> bool {
        self.due_us.is_some()
    }

    /// True once when the scheduled retry is due and the processor is still in the overload
    /// fallback with the user toggle on (spends one retry). Anything else at the due time
    /// drops the schedule.
    pub fn poll(
        &mut self,
        now_us: u64,
        stage: Stage,
        reason: Option<FallbackReason>,
        user_on: bool,
    ) -> bool {
        let Some(due) = self.due_us else {
            return false;
        };
        if now_us < due {
            return false;
        }
        self.due_us = None;
        if user_on && stage == Stage::Fallback && reason == Some(FallbackReason::Overload) {
            self.left -= 1;
            return true;
        }
        false
    }
}

/// Pops one block from ring A into `block` and publishes the input still waiting behind it.
/// The published backlog therefore never includes the block about to be processed, so a
/// starvation during `process_block` sees only genuinely unprocessed input (ruling 18).
pub(crate) fn pop_block(input: &mut Consumer<f32>, block: &mut [f32], backlog: &AtomicU32) -> bool {
    if input.pop_entire_slice(block).is_err() {
        return false;
    }
    backlog.store((input.slots() / 2) as u32, Ordering::Release);
    true
}

fn block_us(hop: usize) -> f64 {
    hop as f64 * 1_000_000.0 / f64::from(SAMPLE_RATE)
}

struct State {
    hop: usize,
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
    /// Ruling 24: retry after a forced overload fallback.
    retry: OverloadRetry,
}

/// Runs until the stop flag; returns the model (the newest one handed over: a pending swap
/// wins over the installed model) so the engine can reuse it for the next run.
pub(crate) fn run(mut ctx: ProcessingCtx) -> Option<Box<dyn Separator>> {
    let _mmcss = Mmcss::pro_audio("processing");
    let hop = ctx.processor.hop();
    let mut st = State {
        hop,
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
        retry: OverloadRetry::new(),
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
        if st.retry.pending() && retry_overload(&mut ctx, &mut st, now_us()) {
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
                    ctx.stats.diag.proc_resets.fetch_add(1, Ordering::Relaxed);
                    begin_gap(&mut ctx, &mut st);
                    continue 'outer;
                }
                InputMarker::GuardGap(at) => {
                    if at >= st.in_pos + st.hop as u64 {
                        break;
                    }
                    let _ = ctx.in_markers.pop();
                    let out = at as i128 + st.out_pos as i128 - st.in_pos as i128
                        + ctx.processor.output_latency_frames() as i128;
                    let out = out.max(st.out_pos as i128) as u64;
                    let _ = ctx.out_markers.push(out);
                }
            }
        }
        // From here until it is pushed to ring B the block is in neither ring; publish it as
        // in flight for the latency estimate. Set before the pop and cleared after the push,
        // so it is never missing from the estimate (briefly counted twice instead).
        ctx.shared
            .proc_in_flight_frames
            .store(st.hop as u32, Ordering::Relaxed);
        if !pop_block(&mut ctx.input, &mut st.in_block, &ctx.shared.in_ring_frames) {
            ctx.shared.proc_in_flight_frames.store(0, Ordering::Relaxed);
            continue;
        }
        st.in_pos += st.hop as u64;
        ctx.stats.diag.proc_blocks.fetch_add(1, Ordering::Relaxed);

        let t0 = now_us();
        ctx.shared.block_start_us.store(t0, Ordering::Relaxed);
        let report = ctx.processor.process_block(&st.in_block, &mut st.out_block);
        let t1 = now_us();
        st.last_block_us = t1;
        ctx.stats.diag.note_block_time(t1 - t0);
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
        let underrun = (underruns > st.underrun_base)
            .then(|| unpack_starvation(ctx.shared.underrun_snapshot.load(Ordering::Acquire)));
        let overloaded = report.ran_model && st.load.overloaded();
        let window = window_overloaded(&ctx, &st, report.ran_model);
        if should_force_fallback(
            underrun,
            st.user_on,
            report.ran_model,
            overloaded || window,
            ctx.processor.stage(),
        ) {
            force_overload(&mut ctx, &mut st, overloaded, window, t1);
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
            // Cleared only once the block is in ring B: for a moment it counts twice (never
            // zero times) in the latency estimate (ruling 23).
            ctx.shared.proc_in_flight_frames.store(0, Ordering::Relaxed);
            ctx.shared.last_push_us.store(t1, Ordering::Relaxed);
            ctx.shared.block_start_us.store(0, Ordering::Relaxed);
            st.out_pos += st.hop as u64;
            // O2: the render thread writes the block to the device on arrival.
            ctx.shared.render_wake.set();
        } else {
            // Ring B full (render stalled): drop the block, fade around the jump.
            ctx.shared.proc_in_flight_frames.store(0, Ordering::Relaxed);
            ctx.shared.block_start_us.store(0, Ordering::Relaxed);
            ctx.stats
                .diag
                .proc_ring_b_drops
                .fetch_add(1, Ordering::Relaxed);
            let _ = ctx.out_markers.push(st.out_pos);
            st.fade_in.start(0);
        }
    }
    let installed = ctx.processor.take_separator();
    match st.pending.take() {
        Some(newest) => {
            if let Some(old) = installed {
                retire(old);
            }
            Some(newest)
        }
        None => installed,
    }
}

/// Output jumps at `out_pos` (delay line / model state cleared): the render thread fades out
/// the frames before it, and the output fades in after the cleared delay.
fn begin_gap(ctx: &mut ProcessingCtx, st: &mut State) {
    let _ = ctx.out_markers.push(st.out_pos);
    st.fade_in.start(ctx.processor.output_latency_frames());
}

/// Spec 6: the window model's own timeouts / duty (an atomic read, no lock), for a block that
/// ran the model. Not while a swap is pending: the engine already holds the new model's spec,
/// and the old model fading out is about to be dropped.
fn window_overloaded(ctx: &ProcessingCtx, st: &State, ran_model: bool) -> bool {
    ran_model && st.pending.is_none() && ctx.processor.model_overloaded()
}

/// Forces `Fallback(Overload)` (load, underrun or `window`: the window model's own overload),
/// records it for the engine log and schedules the ruling 24 retry. A window overload is
/// flagged for the engine (spec 6) and never retried: retrying would put the model that
/// just fell behind back on (on the GPU, against "not back to the GPU this session"). A
/// fallback already under way is not recorded again.
fn force_overload(
    ctx: &mut ProcessingCtx,
    st: &mut State,
    overloaded: bool,
    window: bool,
    now: u64,
) {
    let fresh = ctx.processor.fallback_reason().is_none();
    let stage = ctx.processor.stage();
    ctx.processor.force_fallback(FallbackReason::Overload);
    if overloaded || window {
        st.load.reset();
    }
    if window {
        st.retry.user_off();
        ctx.shared
            .fallback_log
            .window_overload
            .store(true, Ordering::Release);
    }
    if !fresh {
        return;
    }
    let log = &ctx.shared.fallback_log;
    let trigger = if window {
        3
    } else if overloaded {
        2
    } else {
        1
    };
    log.trigger.store(trigger, Ordering::Relaxed);
    log.forced_at_us.store(now, Ordering::Relaxed);
    log.load_milli
        .store((st.load.ratio() * 1000.0).round() as u32, Ordering::Relaxed);
    log.stage.store(stage_code(stage), Ordering::Relaxed);
    log.retry_armed
        .store(!window && st.retry.forced(now), Ordering::Relaxed);
    if trigger == 1 {
        log.keep_forced_detail();
    }
    log.forced.fetch_add(1, Ordering::Release);
}

/// Ruling 24: devocal on again after an overload fallback, through the same path as a user
/// "on" (model reset, warm-up, fade-in, render pre-roll). Not while a model swap is pending
/// (the swap re-enables devocal itself). True if it did.
fn retry_overload(ctx: &mut ProcessingCtx, st: &mut State, now: u64) -> bool {
    if st.pending.is_some()
        || !st.retry.poll(
            now,
            ctx.processor.stage(),
            ctx.processor.fallback_reason(),
            st.user_on,
        )
    {
        return false;
    }
    ctx.processor.request_devocal(true);
    accept_on(ctx, st);
    ctx.shared
        .fallback_log
        .retries
        .fetch_add(1, Ordering::Release);
    true
}

fn apply_command(ctx: &mut ProcessingCtx, st: &mut State, cmd: ProcCommand) {
    match cmd {
        ProcCommand::SetDevocal(on) => {
            st.user_on = on;
            if on {
                st.retry.user_on();
            } else {
                st.retry.user_off();
            }
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
    // The audible path's delay changes with the stage (passthrough vs model).
    ctx.shared.proc_latency_frames.store(
        ctx.processor.output_latency_frames() as u32,
        Ordering::Relaxed,
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
    ctx.shared.proc_hop.store(st.hop as u32, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::super::is_backlogged;
    use super::*;

    const HOP: usize = 128;

    fn starved(ran_model: bool, backlog_frames: usize) -> Option<Starvation> {
        Some(Starvation {
            ran_model,
            backlog_frames,
            ..Starvation::default()
        })
    }

    #[test]
    fn backlog_excludes_the_block_being_processed() {
        let (mut tx, mut rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 8);
        let backlog = AtomicU32::new(u32::MAX);
        let mut block = vec![0.0f32; HOP * 2];
        // Exactly one block waiting: once it is taken for processing, nothing is behind.
        tx.push_entire_slice(&[0.5f32; HOP * 2]).unwrap();
        assert!(pop_block(&mut rx, &mut block, &backlog));
        assert_eq!(backlog.load(Ordering::Acquire), 0);
        assert!(!is_backlogged(
            true,
            backlog.load(Ordering::Acquire) as usize,
            HOP
        ));
        // Three blocks waiting: two remain behind the one in flight.
        tx.push_entire_slice(&[0.5f32; 3 * HOP * 2]).unwrap();
        assert!(pop_block(&mut rx, &mut block, &backlog));
        assert_eq!(backlog.load(Ordering::Acquire), 2 * HOP as u32);
        // Less than a block: nothing popped, backlog untouched.
        tx.push_entire_slice(&[0.5f32; 10]).unwrap();
        rx.pop_entire_slice(&mut [0.0; 4 * HOP]).unwrap();
        assert!(!pop_block(&mut rx, &mut block, &backlog));
    }

    fn forcing(backlog_frames: usize) -> Option<Starvation> {
        Some(Starvation {
            ran_model: true,
            backlog_frames,
            warming: false,
            force: true,
        })
    }

    /// Ruling 24: an underrun forces the fallback only when the render thread judged it
    /// forcing (`force`), whatever the backlog it saw.
    #[test]
    fn only_a_forcing_underrun_forces_fallback() {
        let s = Stage::Devocal;
        assert!(!should_force_fallback(
            starved(true, 0),
            true,
            true,
            false,
            s
        ));
        assert!(
            !should_force_fallback(starved(true, 10 * HOP), true, true, false, s),
            "a backlogged hiccup alone is absorbed as jitter"
        );
        assert!(!should_force_fallback(None, true, true, false, s));
        assert!(should_force_fallback(forcing(HOP), true, true, false, s));
        assert!(should_force_fallback(
            forcing(0),
            true,
            true,
            false,
            Stage::FadingIn
        ));
    }

    #[test]
    fn load_overload_forces_fallback() {
        assert!(should_force_fallback(
            None,
            true,
            true,
            true,
            Stage::Devocal
        ));
        assert!(should_force_fallback(
            starved(true, 0),
            true,
            true,
            true,
            Stage::Devocal
        ));
    }

    #[test]
    fn nothing_forced_while_user_off_model_idle_or_warming_up() {
        let s = Stage::Devocal;
        assert!(!should_force_fallback(forcing(HOP), false, true, true, s));
        assert!(!should_force_fallback(forcing(HOP), true, false, true, s));
        assert!(!should_force_fallback(
            forcing(HOP),
            true,
            true,
            true,
            Stage::WarmingUp
        ));
    }

    /// Ruling 24 (2): one retry, `OVERLOAD_RETRY_DELAY_US` after the forced fallback, only
    /// while still in the overload fallback with the user on; a user "on" restores the
    /// budget, a user "off" cancels.
    #[test]
    fn overload_retry_fires_once_after_the_delay() {
        use FallbackReason::{ModelError, Overload};
        let t = 10_000_000;
        let fb = Stage::Fallback;
        let mut r = OverloadRetry::new();
        assert!(!r.poll(t, fb, Some(Overload), true), "nothing scheduled");
        assert!(r.forced(t));
        assert!(!r.poll(t + OVERLOAD_RETRY_DELAY_US - 1, fb, Some(Overload), true));
        assert!(r.poll(t + OVERLOAD_RETRY_DELAY_US, fb, Some(Overload), true));
        assert!(!r.pending());
        // The retry fell back again: the budget is spent, the fallback stays.
        let t2 = t + 2 * OVERLOAD_RETRY_DELAY_US;
        assert!(!r.forced(t2));
        assert!(!r.poll(t2 + OVERLOAD_RETRY_DELAY_US, fb, Some(Overload), true));
        // A user "on" starts a fresh budget.
        r.user_on();
        assert!(r.forced(t2));
        assert!(r.poll(t2 + OVERLOAD_RETRY_DELAY_US, fb, Some(Overload), true));
        // Off before it is due: cancelled.
        let mut r = OverloadRetry::new();
        r.forced(t);
        r.user_off();
        assert!(!r.poll(t + OVERLOAD_RETRY_DELAY_US, fb, Some(Overload), true));
        // Not in the overload fallback any more, a model error, or user off: dropped,
        // nothing spent.
        let mut r = OverloadRetry::new();
        r.forced(t);
        assert!(!r.poll(t + OVERLOAD_RETRY_DELAY_US, Stage::Devocal, None, true));
        r.forced(t);
        assert!(!r.poll(t + OVERLOAD_RETRY_DELAY_US, fb, Some(ModelError), true));
        r.forced(t);
        assert!(!r.poll(t + OVERLOAD_RETRY_DELAY_US, fb, Some(Overload), false));
        assert!(r.forced(t), "budget unspent");
    }

    /// A processing context on rings only (no thread) around `processor`.
    fn ctx_for(processor: Processor) -> (ProcessingCtx, State) {
        use super::super::OwnedEvent;
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let (_in_tx, in_rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 4);
        let (_mk_tx, mk_rx) = rtrb::RingBuffer::<InputMarker>::new(8);
        let (out_tx, _out_rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 4);
        let (omk_tx, _omk_rx) = rtrb::RingBuffer::<u64>::new(8);
        let (_ctl_tx, ctl_rx) = rtrb::RingBuffer::<ProcCommand>::new(8);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            start_us: now_us(),
            run_id: 1,
            last_input_us: AtomicU64::new(0),
            capture_packet_frames: AtomicU32::new(0),
            in_ring_frames: AtomicU32::new(0),
            proc_in_flight_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(0),
            proc_hop: AtomicU32::new(0),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
            last_push_us: AtomicU64::new(0),
            block_start_us: AtomicU64::new(0),
            fallback_log: crate::audio::FallbackLog::default(),
            wake: OwnedEvent::new().unwrap(),
            render_wake: OwnedEvent::new().unwrap(),
        });
        let hop = processor.hop();
        let st = State {
            hop,
            in_block: vec![0.0; hop * 2],
            out_block: vec![0.0; hop * 2],
            in_pos: 0,
            out_pos: 0,
            load: LoadMonitor::new(block_us(hop), LOAD_WINDOW_BLOCKS, LOAD_THRESHOLD),
            user_on: false,
            pending: None,
            underrun_base: 0,
            fade_in: FadeIn::new(edge_fade_frames()),
            last_block_us: 0,
            retry: OverloadRetry::new(),
        };
        let ctx = ProcessingCtx {
            processor,
            input: in_rx,
            in_markers: mk_rx,
            output: out_tx,
            out_markers: omk_tx,
            control: ctl_rx,
            shared,
            stats: Arc::new(AudioStats::default()),
        };
        (ctx, st)
    }

    /// Ruling 24 (2): the retry goes through the user "on" path (warm-up, fade-in, render
    /// pre-roll request, fallback reason cleared), once; a second forced fallback stays.
    /// The forced fallbacks and the retry are recorded for the engine log.
    #[test]
    fn a_forced_overload_fallback_is_retried_once_through_the_enable_path() {
        use crate::separator::DelayOnly;
        let (mut ctx, mut st) = ctx_for(Processor::new(Some(Box::new(DelayOnly::new(HOP)))));
        apply_command(&mut ctx, &mut st, ProcCommand::SetDevocal(true));
        assert_eq!(ctx.processor.stage(), Stage::WarmingUp);
        assert!(ctx.shared.preroll_request.swap(false, Ordering::AcqRel));
        let t = 10_000_000;
        force_overload(&mut ctx, &mut st, false, false, t);
        assert_eq!(ctx.processor.stage(), Stage::Fallback);
        let log = &ctx.shared.fallback_log;
        assert_eq!(log.forced.load(Ordering::Acquire), 1);
        assert_eq!(log.trigger.load(Ordering::Relaxed), 1, "underrun");
        assert!(log.retry_armed.load(Ordering::Relaxed));
        // Forcing again while already falling back records nothing new.
        force_overload(&mut ctx, &mut st, false, false, t + 1);
        assert_eq!(ctx.shared.fallback_log.forced.load(Ordering::Acquire), 1);

        assert!(!retry_overload(
            &mut ctx,
            &mut st,
            t + OVERLOAD_RETRY_DELAY_US - 1
        ));
        assert!(retry_overload(
            &mut ctx,
            &mut st,
            t + OVERLOAD_RETRY_DELAY_US
        ));
        assert_eq!(ctx.processor.stage(), Stage::WarmingUp);
        assert_eq!(ctx.processor.fallback_reason(), None);
        assert!(
            ctx.shared.preroll_request.load(Ordering::Acquire),
            "pre-roll"
        );
        assert_eq!(ctx.shared.fallback_log.retries.load(Ordering::Acquire), 1);

        // The retry falls back again: no further retry.
        let t2 = t + 2 * OVERLOAD_RETRY_DELAY_US;
        force_overload(&mut ctx, &mut st, true, false, t2);
        let log = &ctx.shared.fallback_log;
        assert_eq!(log.forced.load(Ordering::Acquire), 2);
        assert_eq!(log.trigger.load(Ordering::Relaxed), 2, "load");
        assert!(!log.retry_armed.load(Ordering::Relaxed));
        assert!(!retry_overload(
            &mut ctx,
            &mut st,
            t2 + 10 * OVERLOAD_RETRY_DELAY_US
        ));
        assert_eq!(ctx.processor.stage(), Stage::Fallback);

        // A user off/on leaves the fallback as before, with a fresh retry budget.
        apply_command(&mut ctx, &mut st, ProcCommand::SetDevocal(false));
        apply_command(&mut ctx, &mut st, ProcCommand::SetDevocal(true));
        assert_eq!(ctx.processor.stage(), Stage::WarmingUp);
        let t3 = t2 + 20 * OVERLOAD_RETRY_DELAY_US;
        force_overload(&mut ctx, &mut st, false, false, t3);
        assert!(retry_overload(
            &mut ctx,
            &mut st,
            t3 + OVERLOAD_RETRY_DELAY_US
        ));
    }

    /// Overload reported by a test model.
    struct Overloaded;
    impl Separator for Overloaded {
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
            out.fill(0.0);
            Ok(())
        }
        fn reset(&mut self) {}
        fn overloaded(&self) -> bool {
            true
        }
    }

    /// Fix round 1: while a swap is pending the engine already reports the new model, so the
    /// fading-out old model's overload is not flagged (it is about to be dropped).
    #[test]
    fn no_window_overload_while_a_swap_is_pending() {
        use crate::separator::DelayOnly;
        let (ctx, mut st) = ctx_for(Processor::new(Some(Box::new(Overloaded))));
        assert!(window_overloaded(&ctx, &st, true));
        assert!(!window_overloaded(&ctx, &st, false));
        st.pending = Some(Box::new(DelayOnly::new(HOP)));
        assert!(!window_overloaded(&ctx, &st, true));
    }

    /// Spec 6: a window model's own overload forces `Fallback(Overload)`, is flagged for the
    /// engine (which decides by device) and is never retried here (a retry would put the
    /// model that just fell behind back on, e.g. on the GPU).
    #[test]
    fn a_window_model_overload_is_flagged_for_the_engine_and_not_retried() {
        use crate::separator::DelayOnly;
        let (mut ctx, mut st) = ctx_for(Processor::new(Some(Box::new(DelayOnly::new(HOP)))));
        apply_command(&mut ctx, &mut st, ProcCommand::SetDevocal(true));
        let t = 10_000_000;
        force_overload(&mut ctx, &mut st, false, true, t);
        assert_eq!(ctx.processor.stage(), Stage::Fallback);
        assert_eq!(
            ctx.processor.fallback_reason(),
            Some(FallbackReason::Overload)
        );
        let log = &ctx.shared.fallback_log;
        assert!(log.window_overload.load(Ordering::Acquire));
        assert_eq!(log.trigger.load(Ordering::Relaxed), 3);
        assert!(!log.retry_armed.load(Ordering::Relaxed));
        assert!(!st.retry.pending());
        assert!(!retry_overload(
            &mut ctx,
            &mut st,
            t + 10 * OVERLOAD_RETRY_DELAY_US
        ));
        assert_eq!(ctx.processor.stage(), Stage::Fallback);

        // An ordinary overload already scheduled a retry: a window overload during its
        // fade-out still cancels it.
        let (mut ctx, mut st) = ctx_for(Processor::new(Some(Box::new(DelayOnly::new(HOP)))));
        apply_command(&mut ctx, &mut st, ProcCommand::SetDevocal(true));
        force_overload(&mut ctx, &mut st, true, false, t);
        assert!(st.retry.pending());
        force_overload(&mut ctx, &mut st, false, true, t + 1);
        assert!(!st.retry.pending());
        assert!(ctx
            .shared
            .fallback_log
            .window_overload
            .load(Ordering::Acquire));
    }

    /// Runs the processing thread without devices (rings only) and stops it.
    fn run_and_stop(
        processor: Processor,
        commands: Vec<ProcCommand>,
    ) -> Option<Box<dyn Separator>> {
        use super::super::OwnedEvent;
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let (_in_tx, in_rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 4);
        let (_mk_tx, mk_rx) = rtrb::RingBuffer::<InputMarker>::new(8);
        let (out_tx, _out_rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 4);
        let (omk_tx, _omk_rx) = rtrb::RingBuffer::<u64>::new(8);
        let (mut ctl_tx, ctl_rx) = rtrb::RingBuffer::<ProcCommand>::new(8);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            start_us: now_us(),
            run_id: 1,
            last_input_us: AtomicU64::new(0),
            capture_packet_frames: AtomicU32::new(0),
            in_ring_frames: AtomicU32::new(0),
            proc_in_flight_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(0),
            proc_hop: AtomicU32::new(0),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
            last_push_us: AtomicU64::new(0),
            block_start_us: AtomicU64::new(0),
            fallback_log: crate::audio::FallbackLog::default(),
            wake: OwnedEvent::new().unwrap(),
            render_wake: OwnedEvent::new().unwrap(),
        });
        let ctx = ProcessingCtx {
            processor,
            input: in_rx,
            in_markers: mk_rx,
            output: out_tx,
            out_markers: omk_tx,
            control: ctl_rx,
            shared: shared.clone(),
            stats: Arc::new(AudioStats::default()),
        };
        let thread = std::thread::spawn(move || run(ctx));
        for c in commands {
            assert!(ctl_tx.push(c).is_ok());
        }
        shared.wake.set();
        // Stop once the thread has taken every command (it applies a command in the pass that
        // pops it, before it looks at the stop flag again); generous bound for loaded machines.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while ctl_tx.slots() < 8 && std::time::Instant::now() < deadline {
            shared.wake.set();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(ctl_tx.slots(), 8, "commands taken within 2 s");
        shared.stop.store(true, Ordering::Release);
        shared.wake.set();
        thread.join().unwrap()
    }

    /// O2: every block pushed to ring B wakes the render thread (no audio device: rings and
    /// kernel events only).
    #[test]
    fn processing_wakes_render_after_each_pushed_block() {
        use super::super::OwnedEvent;
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let (mut in_tx, in_rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 4);
        let (_mk_tx, mk_rx) = rtrb::RingBuffer::<InputMarker>::new(8);
        let (out_tx, out_rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 4);
        let (omk_tx, _omk_rx) = rtrb::RingBuffer::<u64>::new(8);
        let (_ctl_tx, ctl_rx) = rtrb::RingBuffer::<ProcCommand>::new(8);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            start_us: now_us(),
            run_id: 1,
            last_input_us: AtomicU64::new(0),
            capture_packet_frames: AtomicU32::new(0),
            in_ring_frames: AtomicU32::new(0),
            proc_in_flight_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(0),
            proc_hop: AtomicU32::new(0),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
            last_push_us: AtomicU64::new(0),
            block_start_us: AtomicU64::new(0),
            fallback_log: crate::audio::FallbackLog::default(),
            wake: OwnedEvent::new().unwrap(),
            render_wake: OwnedEvent::new().unwrap(),
        });
        let ctx = ProcessingCtx {
            processor: Processor::new(None),
            input: in_rx,
            in_markers: mk_rx,
            output: out_tx,
            out_markers: omk_tx,
            control: ctl_rx,
            shared: shared.clone(),
            stats: Arc::new(AudioStats::default()),
        };
        let thread = std::thread::spawn(move || run(ctx));
        assert!(
            !shared.render_wake.wait(0),
            "not signalled before any block"
        );
        in_tx.push_entire_slice(&[0.25f32; 2 * HOP * 2]).unwrap();
        shared.wake.set();
        assert!(shared.render_wake.wait(2_000), "woken by the pushed blocks");
        // Both blocks reach ring B.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while out_rx.slots() < 2 * HOP * 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(out_rx.slots(), 2 * HOP * 2);
        shared.stop.store(true, Ordering::Release);
        shared.wake.set();
        thread.join().unwrap();
    }

    /// Ruling 21 (b): while the model works on a block, that block is in neither ring; the
    /// thread publishes it as in flight (one hop), so the latency estimate does not read one
    /// hop low during inference, and clears it once the block is in ring B.
    #[test]
    fn the_block_being_processed_is_published_as_in_flight() {
        use super::super::OwnedEvent;
        use std::sync::atomic::{AtomicBool, AtomicU64};
        /// Records the in-flight frames it sees during each `process` call.
        struct Probe {
            shared: Arc<Shared>,
            min: Arc<AtomicU32>,
            max: Arc<AtomicU32>,
        }
        impl Separator for Probe {
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
                let f = self.shared.proc_in_flight_frames.load(Ordering::Relaxed);
                self.min.fetch_min(f, Ordering::Relaxed);
                self.max.fetch_max(f, Ordering::Relaxed);
                out.fill(0.0);
                Ok(())
            }
            fn reset(&mut self) {}
        }
        let (mut in_tx, in_rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 4);
        let (_mk_tx, mk_rx) = rtrb::RingBuffer::<InputMarker>::new(8);
        let (out_tx, out_rx) = rtrb::RingBuffer::<f32>::new(HOP * 2 * 4);
        let (omk_tx, _omk_rx) = rtrb::RingBuffer::<u64>::new(8);
        let (mut ctl_tx, ctl_rx) = rtrb::RingBuffer::<ProcCommand>::new(8);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            start_us: now_us(),
            run_id: 1,
            last_input_us: AtomicU64::new(0),
            capture_packet_frames: AtomicU32::new(0),
            in_ring_frames: AtomicU32::new(0),
            proc_in_flight_frames: AtomicU32::new(0),
            proc_latency_frames: AtomicU32::new(0),
            proc_hop: AtomicU32::new(0),
            output_failed: AtomicBool::new(false),
            preroll_request: AtomicBool::new(false),
            underrun_snapshot: AtomicU64::new(0),
            last_push_us: AtomicU64::new(0),
            block_start_us: AtomicU64::new(0),
            fallback_log: crate::audio::FallbackLog::default(),
            wake: OwnedEvent::new().unwrap(),
            render_wake: OwnedEvent::new().unwrap(),
        });
        let (min, max) = (
            Arc::new(AtomicU32::new(u32::MAX)),
            Arc::new(AtomicU32::new(0)),
        );
        let probe = Probe {
            shared: shared.clone(),
            min: min.clone(),
            max: max.clone(),
        };
        let ctx = ProcessingCtx {
            processor: Processor::new(Some(Box::new(probe))),
            input: in_rx,
            in_markers: mk_rx,
            output: out_tx,
            out_markers: omk_tx,
            control: ctl_rx,
            shared: shared.clone(),
            stats: Arc::new(AudioStats::default()),
        };
        let thread = std::thread::spawn(move || run(ctx));
        assert!(ctl_tx.push(ProcCommand::SetDevocal(true)).is_ok());
        shared.wake.set();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while ctl_tx.slots() < 8 && std::time::Instant::now() < deadline {
            shared.wake.set();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        in_tx.push_entire_slice(&[0.25f32; 4 * HOP * 2]).unwrap();
        shared.wake.set();
        while out_rx.slots() < 4 * HOP * 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(out_rx.slots(), 4 * HOP * 2, "all four blocks in ring B");
        assert_eq!(
            min.load(Ordering::Relaxed),
            HOP as u32,
            "in flight on every call"
        );
        assert_eq!(max.load(Ordering::Relaxed), HOP as u32);
        // Cleared right after the last push.
        while shared.proc_in_flight_frames.load(Ordering::Relaxed) != 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            shared.proc_in_flight_frames.load(Ordering::Relaxed),
            0,
            "nothing in flight once the blocks are in ring B"
        );
        shared.stop.store(true, Ordering::Release);
        shared.wake.set();
        thread.join().unwrap();
    }

    /// Ruling 24 fix round 1: a retry coming due while a model swap is pending waits; the swap
    /// turns devocal on again itself, and the retry is then dropped without spending the
    /// budget. The forced fallback keeps the detail of the underrun that forced it.
    #[test]
    fn a_retry_due_during_a_pending_swap_is_left_to_the_swap() {
        use super::super::StarvationDetail;
        use crate::separator::DelayOnly;
        let (mut ctx, mut st) = ctx_for(Processor::new(Some(Box::new(DelayOnly::new(HOP)))));
        apply_command(&mut ctx, &mut st, ProcCommand::SetDevocal(true));
        let t = 10_000_000;
        ctx.shared.fallback_log.set_detail(StarvationDetail {
            shortfall: 58,
            ..StarvationDetail::default()
        });
        force_overload(&mut ctx, &mut st, false, false, t);
        assert_eq!(ctx.shared.fallback_log.forced_detail().shortfall, 58);
        assert_eq!(ctx.processor.stage(), Stage::Fallback);
        st.pending = Some(Box::new(DelayOnly::new(HOP)));
        let due = t + OVERLOAD_RETRY_DELAY_US;
        assert!(!retry_overload(&mut ctx, &mut st, due));
        assert!(st.retry.pending(), "still scheduled");
        assert_eq!(ctx.processor.stage(), Stage::Fallback);
        assert!(try_swap(&mut ctx, &mut st));
        assert_eq!(
            ctx.processor.stage(),
            Stage::WarmingUp,
            "the swap turned devocal on again"
        );
        assert!(!retry_overload(&mut ctx, &mut st, due + 1));
        assert!(!st.retry.pending(), "dropped");
        assert_eq!(ctx.shared.fallback_log.retries.load(Ordering::Acquire), 0);
        assert!(st.retry.forced(due + 2), "budget unspent");
    }

    #[test]
    fn processing_thread_hands_its_model_back_at_stop() {
        use crate::separator::DelayOnly;
        let back = run_and_stop(Processor::new(Some(Box::new(DelayOnly::new(256)))), vec![]);
        assert_eq!(back.map(|m| m.latency_frames()), Some(256));
        assert!(run_and_stop(Processor::new(None), vec![]).is_none());
        // The newest model handed over is the one returned.
        let back = run_and_stop(
            Processor::new(Some(Box::new(DelayOnly::new(256)))),
            vec![ProcCommand::SetSeparator(Box::new(DelayOnly::new(512)))],
        );
        assert_eq!(back.map(|m| m.latency_frames()), Some(512));
    }
}
