//! Engine core and main loop of `devocal-engine.exe`.
//!
//! [`EngineCore`] owns the session [`Holder`], the audio side (behind [`AudioPort`], so tests
//! use a fake) and the phase machine ([`state::next`](crate::state::next)). It does no I/O of
//! its own: [`EngineCore::handle`] takes one command, [`EngineCore::tick`] advances time, and
//! both return the events to send. [`loop_once`] is one pass of the main loop; [`run`] is the
//! real process: the app pipe, the watchdog and `loop_once` every millisecond.
//!
//! Timestamps: every step reads the clock again (each command, the watchdog's exit request,
//! the tick). A step that blocked (an audio start takes up to tens of milliseconds) must not
//! leave the following steps with a stale time: the Holder's ramp and its gain history are
//! stamped with these times, and a stale stamp both collapses the 30 ms ramp and ages the
//! loud steps out of the conservative gain window.
//!
//! Models: loading never blocks the loop. `set_model` starts a load on a worker thread
//! ([`ModelLoader`]) and every tick polls for the result; a newer `set_model` supersedes a
//! pending one (the older result is dropped on its worker). A loaded model goes to the
//! running audio (`set_separator`) or is staged for the next audio start. When the audio
//! stops, the processing thread hands its model back; the engine `reset()`s it and stages it,
//! so a re-attach does not load again. Only if nothing came back (a thread left detached, a
//! failed start) does the next attach reload the model in the background.
//!
//! Attach order: `attach` stages the model (or starts that background reload), starts the
//! audio threads (process loopback of the player tree, output on the player's endpoint) and
//! only then, on the next tick, calls `Holder::begin_attach` with that tick's time, so the
//! 30 ms volume ramp is not compressed by the audio start-up and nothing is lowered when the
//! audio cannot start.
//!
//! Every tick (1 ms in [`run`]):
//! 1. a finished model load (installed or reported);
//! 2. a pending `begin_attach` (see above);
//! 3. audio thread failure (`capture_failed` -> `CaptureFailed`, other `failed` ->
//!    `RenderFailed`) while attaching/active: release first, then report;
//! 4. `Holder::tick`: `Held` while attaching -> `AttachDone`; `Failed` while attaching ->
//!    `Failure` + `begin_release`; `Failed` while releasing is one-shot (logged, the entry
//!    stays in the restore file) and the next `Idle` is `ReleaseDone`;
//! 5. every 0.5 s, and at once when the safety guard asks (`take_follow_request`) or a
//!    notification arrived ([`SessionSignals`]: a new session, a default render device
//!    change): `Holder::follow` while active; every 0.5 s and after a notification also the
//!    output endpoint check. A default render device change while attaching or active first
//!    silences the capture (`Holder::default_device_changed`);
//! 6. `SharedGains` from the Holder (after the tick and follow, so the audio threads see the
//!    gain that matches the volumes just set);
//! 7. `Metrics` once per second;
//! 8. a `State` event whenever phase, mode, fallback reason or attached pid changed.
//!
//! Mode reporting (caller obligation 5), while `Active`:
//! - user toggle off: `Passthrough` whatever the processor stage (an overload fallback while
//!   the user has devocal off is plain passthrough, never "overload");
//! - user toggle on: `Passthrough`/`WarmingUp`/`FadingIn`/`Devocal` report `Devocal`
//!   (pending until the fade-in is done, or until a pending model load finishes);
//!   `FadingOut` and `Fallback` with a `fallback_reason` report `Fallback(reason)`;
//!   `FadingOut` without a reason is a model swap in progress and stays `Devocal` (pending);
//! - for up to 100 ms after a forwarded toggle, until the processor stage moves, the
//!   requested mode is reported, so a stale stage (e.g. the old `Fallback`) does not flash.
//!
//! Toggle forwarding (caller obligation 1): the engine remembers what the running audio was
//! told. "On" is sent only when the user wants it, the audio has a model and the audio was
//! not already told "on"; a repeated "on" (state sync) therefore never undoes an overload
//! fallback. "On" requested while a load is pending is sent after the model is installed;
//! a failed load clears it. A new audio run starts "off" and gets the user's toggle once.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use devocal_core::protocol::{
    decode_command, encode, Command, ErrorCode, Event, FallbackReason, Metrics, Mode, Phase,
    ProtocolError, PROTOCOL,
};
use devocal_core::sessions::SessionVolumes;
use devocal_core::sessions_win::WinSessions;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

use crate::audio::endpoint::session_endpoint;
use crate::audio::{now_us, AudioConfig, AudioHandle, SharedGains};
use crate::dsp::SAMPLE_RATE;
use crate::holder::{Holder, HolderPhase};
use crate::notify::{SessionSignals, SessionWatcher};
use crate::pipe::{pipe_name, Accepted, PipeServer};
use crate::processor::{Processor, Stage};
use crate::separator::Separator;
use crate::state::{next, Input};
use crate::stemgen::StemgenRt;

/// Main loop period.
pub const TICK_US: u64 = 1_000;
/// Holder follow and output endpoint check.
pub const CHECK_INTERVAL_US: u64 = 500_000;
pub const METRICS_INTERVAL_US: u64 = 1_000_000;
/// After a forwarded toggle the requested mode is reported until the stage moves, at most
/// this long.
pub const TOGGLE_SETTLE_US: u64 = 100_000;
/// The audio threads keep running this long after the release (output gain already 0), so
/// the render side is silent before it stops.
pub const AUDIO_STOP_DELAY_US: u64 = 20_000;
/// Watchdog: how long the release may take before the engine exits anyway (the restore file
/// then stays for the app).
pub const EXIT_RELEASE_TIMEOUT_US: u64 = 2_000_000;
/// How long the engine waits for the app to connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the writer may take to flush the last events at exit.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(200);

/// Audio statistics as plain values.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AudioSnapshot {
    pub underruns: u64,
    pub input_silent_ms: u64,
    pub load_ratio: f32,
    pub latency_ms: f32,
    /// Blocks silenced by the safety guard.
    pub unattenuated_blocks: u64,
    pub headroom_frames: u32,
}

/// The audio side as the engine core sees it: [`RealAudio`] wraps [`AudioHandle`]; tests use
/// a fake. `start` replaces nothing (the core stops a previous run first); every other call
/// is a no-op (or a neutral value) while stopped.
pub trait AudioPort {
    fn start(
        &mut self,
        cfg: AudioConfig,
        gains: Arc<SharedGains>,
        processor: Processor,
    ) -> Result<(), String>;
    /// The user toggle (send only on a genuine change, caller obligation 1).
    fn set_devocal(&self, on: bool);
    fn set_separator(&self, s: Box<dyn Separator>);
    fn rebind_output(&self, endpoint: Option<String>);
    fn output_failed(&self) -> bool;
    fn capture_failed(&self) -> bool;
    /// Any audio thread ended without being stopped.
    fn failed(&self) -> bool;
    fn stage(&self) -> Stage;
    fn fallback_reason(&self) -> Option<FallbackReason>;
    /// The safety guard asks for an immediate `Holder::follow` (cleared by the call).
    fn take_follow_request(&self) -> bool;
    fn stats(&self) -> AudioSnapshot;
    /// Stops the audio and returns the model it held (if it came back).
    fn stop(&mut self) -> Option<Box<dyn Separator>>;
}

/// Result channel of one model load.
pub type LoadResult = mpsc::Receiver<Result<Box<dyn Separator>, String>>;

/// Starts loading a model (path, inference threads) off the main loop; the result arrives on
/// the returned channel. A receiver dropped before the result arrives abandons the load.
pub type ModelLoader = Box<dyn FnMut(PathBuf, u16) -> LoadResult>;

/// Loads StemgenRT models, each on its own worker thread.
pub fn stemgen_loader() -> ModelLoader {
    Box::new(|path: PathBuf, threads: u16| {
        let (tx, rx) = mpsc::channel();
        let on_spawn_error = tx.clone();
        let spawned = thread::Builder::new()
            .name("devocal-model-load".into())
            .spawn(move || {
                let model =
                    StemgenRt::load(&path, threads).map(|m| Box::new(m) as Box<dyn Separator>);
                // A superseded load: the receiver is gone and the model is dropped here.
                let _ = tx.send(model);
            });
        if let Err(e) = spawned {
            let _ = on_spawn_error.send(Err(format!("starting the model loader: {e}")));
        }
        rx
    })
}

/// Drops a model on a short-lived thread (tearing down an inference session can take
/// milliseconds; the main loop must not stall). Without a thread it is dropped here.
fn retire(model: Box<dyn Separator>) {
    let _ = thread::Builder::new()
        .name("devocal-model-drop".into())
        .spawn(move || drop(model));
}

/// Reported mode for the user's toggle and the processor's stage (see the module docs).
pub fn mode_for(
    user_on: bool,
    stage: Stage,
    reason: Option<FallbackReason>,
) -> (Mode, Option<FallbackReason>) {
    if !user_on {
        return (Mode::Passthrough, None);
    }
    match (stage, reason) {
        (Stage::Fallback, r) => (Mode::Fallback, r),
        (Stage::FadingOut, Some(r)) => (Mode::Fallback, Some(r)),
        (
            Stage::FadingOut
            | Stage::Passthrough
            | Stage::WarmingUp
            | Stage::FadingIn
            | Stage::Devocal,
            _,
        ) => (Mode::Devocal, None),
    }
}

fn error(code: ErrorCode, message: impl Into<String>) -> Event {
    Event::Error {
        code,
        message: message.into(),
    }
}

struct ModelSpec {
    id: String,
    path: PathBuf,
    threads: u16,
}

struct PendingLoad {
    spec: ModelSpec,
    /// Reloading the current model (its separator did not come back), not a new `set_model`.
    reload: bool,
    rx: LoadResult,
}

type StateKey = (Phase, Option<Mode>, Option<FallbackReason>, Option<u32>);

pub struct EngineCore<S: SessionVolumes, A: AudioPort> {
    holder: Holder<S>,
    audio: A,
    audio_running: bool,
    /// Set after a release: stop the audio at this time.
    audio_stop_at: Option<u64>,
    /// The running audio has a model (started with one, or one was sent).
    audio_model: bool,
    /// The toggle the running audio was last told.
    audio_toggle: bool,
    gains: Arc<SharedGains>,
    loader: ModelLoader,
    /// The model last loaded successfully (in the audio, staged, or being reloaded).
    model: Option<ModelSpec>,
    /// A loaded model waiting for the next audio start.
    staged: Option<Box<dyn Separator>>,
    pending_load: Option<PendingLoad>,
    phase: Phase,
    pid: Option<u32>,
    /// `begin_attach` runs on the next tick (after the audio started).
    pending_attach: Option<(u32, u64)>,
    /// The user's last requested toggle.
    user_devocal: bool,
    /// (stage before the last forwarded toggle, until when the requested mode is reported).
    toggle_settle: Option<(Stage, u64)>,
    bound_endpoint: Option<String>,
    next_check_us: Option<u64>,
    next_metrics_us: Option<u64>,
    overridden_total: u64,
    last_state: StateKey,
    last_follow_error: Option<String>,
    unattenuated_seen: u64,
    exit_requested: bool,
    /// Set by the session/endpoint notification callbacks (see [`crate::notify`]).
    signals: Arc<SessionSignals>,
    /// A notification asked for a follow pass that has not run yet (it arrived while
    /// attaching).
    event_follow: bool,
}

impl<S: SessionVolumes, A: AudioPort> EngineCore<S, A> {
    pub fn new(sessions: S, restore_path: PathBuf, audio: A, loader: ModelLoader) -> Self {
        Self {
            holder: Holder::new(sessions, restore_path),
            audio,
            audio_running: false,
            audio_stop_at: None,
            audio_model: false,
            audio_toggle: false,
            gains: Arc::new(SharedGains::new(1.0, 0.0)),
            loader,
            model: None,
            staged: None,
            pending_load: None,
            phase: Phase::Idle,
            pid: None,
            pending_attach: None,
            user_devocal: false,
            toggle_settle: None,
            bound_endpoint: None,
            next_check_us: None,
            next_metrics_us: None,
            overridden_total: 0,
            last_state: (Phase::Idle, None, None, None),
            last_follow_error: None,
            unattenuated_seen: 0,
            exit_requested: false,
            signals: Arc::new(SessionSignals::default()),
            event_follow: false,
        }
    }

    /// The flags the notification callbacks set ([`SessionWatcher`] in [`run`]).
    pub fn signals(&self) -> Arc<SessionSignals> {
        self.signals.clone()
    }

    #[cfg(test)]
    pub fn sessions(&self) -> &S {
        self.holder.sessions()
    }

    #[cfg(test)]
    pub fn audio(&self) -> &A {
        &self.audio
    }

    #[cfg(test)]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// `shutdown`, a protocol mismatch or the watchdog asked the engine to exit.
    pub fn exit_requested(&self) -> bool {
        self.exit_requested
    }

    /// The exit was requested, the release is done and the audio is stopped.
    pub fn exit_ready(&self) -> bool {
        self.exit_requested && self.phase == Phase::Idle && !self.audio_running
    }

    /// Watchdog: release and exit (as `shutdown`).
    pub fn request_exit(&mut self, now_us: u64) -> Vec<Event> {
        let mut ev = Vec::new();
        self.begin_exit(now_us, &mut ev);
        self.sync_state(now_us, &mut ev);
        ev
    }

    /// One line from the pipe. Another protocol version releases and exits (as a `hello`
    /// with another version); a malformed line is reported and ignored.
    pub fn handle_line(&mut self, line: &str, now_us: u64) -> Vec<Event> {
        match decode_command(line) {
            Ok(cmd) => self.handle(cmd, now_us),
            Err(e @ ProtocolError::Version(_)) => {
                let mut ev = vec![error(
                    ErrorCode::Protocol,
                    format!("{e}; releasing and exiting"),
                )];
                self.begin_exit(now_us, &mut ev);
                self.sync_state(now_us, &mut ev);
                ev
            }
            Err(e) => vec![error(ErrorCode::Protocol, e.to_string())],
        }
    }

    pub fn handle(&mut self, cmd: Command, now_us: u64) -> Vec<Event> {
        let mut ev = Vec::new();
        match cmd {
            Command::Hello { version } => {
                if version != PROTOCOL {
                    ev.push(error(
                        ErrorCode::Protocol,
                        format!(
                            "unsupported protocol version {version} (engine speaks {PROTOCOL}); \
                             releasing and exiting"
                        ),
                    ));
                    self.begin_exit(now_us, &mut ev);
                }
            }
            Command::Attach { pid, created_at } => self.attach(pid, created_at, now_us, &mut ev),
            Command::SetMode { devocal } => self.set_mode(devocal, now_us, &mut ev),
            Command::SetModel {
                id,
                path,
                device,
                threads,
            } => self.set_model(id, path, &device, threads, now_us, &mut ev),
            Command::Release => self.release(now_us, &mut ev),
            Command::Shutdown => self.begin_exit(now_us, &mut ev),
        }
        self.sync_state(now_us, &mut ev);
        ev
    }

    /// Advances the engine to `now_us` (see the module docs for the order).
    pub fn tick(&mut self, now_us: u64) -> Vec<Event> {
        let mut ev = Vec::new();
        self.poll_model(now_us, &mut ev);
        self.start_pending_attach(now_us, &mut ev);
        self.check_audio(now_us, &mut ev);
        self.step_holder(now_us, &mut ev);
        self.periodic(now_us, &mut ev);
        self.gains
            .set(self.holder.capture_gain(now_us), self.holder.output_gain());
        if self.audio_stop_at.is_some_and(|t| now_us >= t) {
            self.stop_audio();
        }
        if let Some((before, until)) = self.toggle_settle {
            if !self.audio_live() || self.audio.stage() != before || now_us >= until {
                self.toggle_settle = None;
            }
        }
        self.metrics(now_us, &mut ev);
        self.sync_state(now_us, &mut ev);
        ev
    }

    /// Stops the audio threads now (no-op when stopped) and keeps the model they hand back
    /// for the next run (reset; a newer staged model wins).
    pub fn stop_audio(&mut self) {
        if self.audio_running {
            if let Some(mut model) = self.audio.stop() {
                model.reset();
                if self.staged.is_none() {
                    self.staged = Some(model);
                } else {
                    retire(model);
                }
            }
            self.audio_running = false;
        }
        self.audio_model = false;
        self.audio_toggle = false;
        self.audio_stop_at = None;
        self.toggle_settle = None;
    }

    /// Audio is running for an attach in progress or a hold (not releasing or about to
    /// stop): toggles and models go to the audio side only then.
    fn audio_live(&self) -> bool {
        self.audio_running
            && self.audio_stop_at.is_none()
            && matches!(self.phase, Phase::Attaching | Phase::Active)
    }

    fn begin_exit(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        self.exit_requested = true;
        self.release(now_us, ev);
    }

    fn attach(&mut self, pid: u32, created_at: u64, now_us: u64, ev: &mut Vec<Event>) {
        if self.exit_requested {
            ev.push(error(
                ErrorCode::AttachFailed,
                "the engine is shutting down",
            ));
            return;
        }
        if self.phase != Phase::Idle {
            ev.push(error(
                ErrorCode::AttachFailed,
                format!(
                    "cannot attach to {pid} while {:?} (release first)",
                    self.phase
                ),
            ));
            return;
        }
        // The previous run may still be in its stop delay; its model comes back here.
        self.stop_audio();
        if self.staged.is_none() && self.pending_load.is_none() {
            // Nothing came back from the last run: reload in the background.
            if let Some(spec) = self.model.take() {
                self.start_load(spec, true);
            }
        }
        self.pid = Some(pid);
        self.transition(Input::Attach, now_us, ev);

        let endpoint = self
            .holder
            .sessions()
            .sessions_for_tree(pid)
            .ok()
            .and_then(|t| session_endpoint(&t));
        let with_model = self.staged.is_some();
        let processor = Processor::new(self.staged.take());
        let cfg = AudioConfig {
            pid,
            sample_rate: SAMPLE_RATE,
            hop: processor.hop(),
            output_endpoint: endpoint.clone(),
        };
        match self.audio.start(cfg, self.gains.clone(), processor) {
            Ok(()) => {
                self.audio_running = true;
                self.audio_model = with_model;
                self.audio_toggle = false;
                self.bound_endpoint = endpoint;
                self.pending_attach = Some((pid, created_at));
                // A fresh processor: apply the user's current toggle once.
                self.sync_toggle(now_us);
            }
            Err(e) => {
                ev.push(error(
                    ErrorCode::AttachFailed,
                    format!("starting the audio for {pid} failed: {e}"),
                ));
                self.abort_to_idle(now_us, ev);
            }
        }
    }

    fn set_mode(&mut self, devocal: bool, now_us: u64, ev: &mut Vec<Event>) {
        if self.exit_requested {
            ev.push(error(
                ErrorCode::Protocol,
                "the engine is shutting down; set_mode ignored",
            ));
            return;
        }
        if devocal && self.model.is_none() && self.pending_load.is_none() {
            ev.push(error(
                ErrorCode::NoModel,
                "no model loaded (send set_model first)",
            ));
            return;
        }
        self.user_devocal = devocal;
        self.sync_toggle(now_us);
    }

    /// Tells the running audio the user's toggle if it differs from what it was told; "on"
    /// only once the audio has a model (caller obligation 1).
    fn sync_toggle(&mut self, now_us: u64) {
        if !self.audio_live() {
            return;
        }
        let want = self.user_devocal && self.audio_model;
        if want != self.audio_toggle {
            let before = self.audio.stage();
            self.audio.set_devocal(want);
            self.audio_toggle = want;
            self.toggle_settle = Some((before, now_us + TOGGLE_SETTLE_US));
        }
    }

    fn set_model(
        &mut self,
        id: String,
        path: PathBuf,
        device: &str,
        threads: u16,
        now_us: u64,
        ev: &mut Vec<Event>,
    ) {
        if self.exit_requested {
            ev.push(error(
                ErrorCode::Protocol,
                "the engine is shutting down; set_model ignored",
            ));
            return;
        }
        if device != "cpu" {
            ev.push(error(
                ErrorCode::BadDevice,
                format!("device {device:?} is not supported (only \"cpu\")"),
            ));
            return;
        }
        self.start_load(ModelSpec { id, path, threads }, false);
        // A loader that finished at once is installed now.
        self.poll_model(now_us, ev);
    }

    /// Starts a load; a pending older one is abandoned (its result is dropped on its worker).
    fn start_load(&mut self, spec: ModelSpec, reload: bool) {
        let rx = (self.loader)(spec.path.clone(), spec.threads);
        self.pending_load = Some(PendingLoad { spec, reload, rx });
    }

    /// Installs or reports a finished load.
    fn poll_model(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        let Some(pending) = &self.pending_load else {
            return;
        };
        let result = match pending.rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("the model loader ended without a result".to_string())
            }
        };
        let Some(PendingLoad { spec, reload, .. }) = self.pending_load.take() else {
            return;
        };
        match result {
            Ok(model) => {
                self.model = Some(spec);
                if self.audio_live() {
                    // The audio side fades out, swaps and turns back on if it was on (caller
                    // obligation 3); a pending user "on" is sent after the swap.
                    self.audio.set_separator(model);
                    self.audio_model = true;
                    self.sync_toggle(now_us);
                } else if let Some(old) = self.staged.replace(model) {
                    retire(old);
                }
            }
            Err(e) => {
                ev.push(error(
                    ErrorCode::ModelLoadFailed,
                    format!(
                        "{} model {} from {} failed: {e}",
                        if reload { "reloading" } else { "loading" },
                        spec.id,
                        spec.path.display()
                    ),
                ));
                if reload {
                    self.model = None;
                }
                // A previously loaded model stays in use; without one a pending "on" is
                // dropped.
                if self.model.is_none() {
                    self.user_devocal = false;
                }
            }
        }
    }

    fn release(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        if matches!(self.phase, Phase::Attaching | Phase::Active) {
            self.pending_attach = None;
            self.holder.begin_release(now_us);
            self.transition(Input::Release, now_us, ev);
        }
    }

    /// A failure while attaching or active: release (the holder ramps back), phase
    /// `Releasing`.
    fn fail(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        self.pending_attach = None;
        self.holder.begin_release(now_us);
        self.transition(Input::Failure, now_us, ev);
    }

    /// The attach failed before anything was lowered: straight back to `Idle`
    /// (`Failure` then `ReleaseDone`, reported as one state change) with the audio stopped.
    fn abort_to_idle(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        self.pending_attach = None;
        self.stop_audio();
        let phase = next(self.phase, Input::Failure).and_then(|p| next(p, Input::ReleaseDone));
        self.phase = phase.unwrap_or(Phase::Idle);
        self.pid = None;
        self.bound_endpoint = None;
        self.sync_state(now_us, ev);
    }

    fn transition(&mut self, input: Input, now_us: u64, ev: &mut Vec<Event>) {
        if let Ok(phase) = next(self.phase, input) {
            self.phase = phase;
            if phase == Phase::Idle {
                self.pid = None;
            }
            self.sync_state(now_us, ev);
        }
    }

    fn start_pending_attach(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        if self.phase != Phase::Attaching {
            return;
        }
        let Some((pid, created_at)) = self.pending_attach.take() else {
            return;
        };
        match self.holder.begin_attach(pid, created_at, now_us) {
            Ok(report) => {
                if report.restore_failed > 0 || report.restore_corrupt {
                    ev.push(error(
                        ErrorCode::AttachFailed,
                        format!(
                            "attach continues, but the restore before it was incomplete: {} \
                             entries not restored (kept in the restore file){}",
                            report.restore_failed,
                            if report.restore_corrupt {
                                "; the restore file was corrupt and has been set aside"
                            } else {
                                ""
                            }
                        ),
                    ));
                }
            }
            Err(e) => {
                ev.push(error(ErrorCode::AttachFailed, e));
                self.abort_to_idle(now_us, ev);
            }
        }
    }

    fn check_audio(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        if !self.audio_live() {
            return;
        }
        let (code, message) = if self.audio.capture_failed() {
            (
                ErrorCode::CaptureFailed,
                "capturing the player's audio failed; released",
            )
        } else if self.audio.failed() {
            (
                ErrorCode::RenderFailed,
                "an audio processing or output thread stopped; released",
            )
        } else {
            return;
        };
        self.fail(now_us, ev);
        ev.push(error(code, message));
    }

    fn step_holder(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        match (self.phase, self.holder.tick(now_us)) {
            (Phase::Attaching, HolderPhase::Held) => self.transition(Input::AttachDone, now_us, ev),
            (Phase::Attaching, HolderPhase::Failed(m)) => {
                self.fail(now_us, ev);
                ev.push(error(ErrorCode::AttachFailed, format!("{m}; released")));
            }
            (Phase::Releasing, HolderPhase::Failed(m)) => {
                // One-shot; the release continues and the entry stays in the restore file
                // (the app restores it later). The next `Idle` ends the release.
                eprintln!("devocal engine: release: {m} (kept in the restore file)");
            }
            (Phase::Releasing, HolderPhase::Idle) => {
                self.transition(Input::ReleaseDone, now_us, ev);
                self.schedule_audio_stop(now_us);
            }
            _ => {}
        }
    }

    fn schedule_audio_stop(&mut self, now_us: u64) {
        if self.audio_running && self.audio_stop_at.is_none() {
            self.audio_stop_at = Some(now_us + AUDIO_STOP_DELAY_US);
        }
        self.bound_endpoint = None;
    }

    fn periodic(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        let due = self.next_check_us.is_none_or(|t| now_us >= t);
        if due {
            self.next_check_us = Some(now_us + CHECK_INTERVAL_US);
        }
        // Notifications are taken every pass; they matter only while holding.
        let created = self.signals.take_session_created();
        let default_changed = self.signals.take_default_changed();
        let holding = matches!(self.phase, Phase::Attaching | Phase::Active);
        if holding {
            if created || default_changed.is_some() {
                self.event_follow = true;
            }
            if let Some(endpoint) = default_changed {
                // Silent (never amplified) until the player is held on the new endpoint.
                self.holder.default_device_changed(now_us, endpoint);
            }
        } else {
            self.event_follow = false;
        }
        let requested = self.audio_live() && self.audio.take_follow_request();
        let event = self.event_follow && self.phase == Phase::Active;
        if (due || requested || event) && self.phase == Phase::Active {
            self.event_follow = false;
            self.follow(now_us, ev);
        }
        if (due || event) && self.audio_live() {
            self.follow_endpoint();
        }
    }

    fn follow(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        let r = self.holder.follow(now_us);
        self.overridden_total += r.overridden as u64;
        if r.player_exited {
            self.transition(Input::PlayerExited, now_us, ev);
            self.schedule_audio_stop(now_us);
            if r.restore_failed > 0 || r.restore_corrupt {
                ev.push(error(
                    ErrorCode::AttachFailed,
                    format!(
                        "the player exited; {}",
                        r.error.as_deref().unwrap_or("restore incomplete")
                    ),
                ));
            }
            self.last_follow_error = None;
            return;
        }
        match r.error {
            Some(e) => {
                if self.last_follow_error.as_deref() != Some(e.as_str()) {
                    eprintln!(
                        "devocal engine: follow: {e} ({} errors in this pass)",
                        r.failed
                    );
                }
                self.last_follow_error = Some(e);
            }
            None => self.last_follow_error = None,
        }
    }

    /// Rebinds the output when the player's endpoint changed or the output failed. A tree
    /// without sessions (or an enumeration error) gives no information: the binding is kept
    /// unless the output failed, which then goes to the session's endpoint or, with no
    /// session, the default endpoint (`None`).
    fn follow_endpoint(&mut self) {
        let Some(pid) = self.pid else {
            return;
        };
        let failed = self.audio.output_failed();
        let target = match self.holder.sessions().sessions_for_tree(pid) {
            Ok(tree) => match session_endpoint(&tree) {
                Some(ep) => Some(ep),
                None if failed => None,
                None => self.bound_endpoint.clone(),
            },
            Err(_) => self.bound_endpoint.clone(),
        };
        if failed || target != self.bound_endpoint {
            self.audio.rebind_output(target.clone());
            self.bound_endpoint = target;
        }
    }

    fn current_mode(&self, now_us: u64) -> (Option<Mode>, Option<FallbackReason>) {
        if self.phase != Phase::Active {
            return (None, None);
        }
        if !self.audio_live() {
            return (Some(Mode::Passthrough), None);
        }
        let stage = self.audio.stage();
        if let Some((before, until)) = self.toggle_settle {
            if stage == before && now_us < until {
                let requested = if self.user_devocal {
                    Mode::Devocal
                } else {
                    Mode::Passthrough
                };
                return (Some(requested), None);
            }
        }
        let (mode, reason) = mode_for(self.user_devocal, stage, self.audio.fallback_reason());
        (Some(mode), reason)
    }

    fn sync_state(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        let (mode, reason) = self.current_mode(now_us);
        let pid = if self.phase == Phase::Idle {
            None
        } else {
            self.pid
        };
        let key = (self.phase, mode, reason, pid);
        if key != self.last_state {
            self.last_state = key;
            ev.push(Event::State {
                phase: self.phase,
                mode,
                fallback_reason: reason,
                attached_pid: pid,
            });
        }
    }

    fn metrics(&mut self, now_us: u64, ev: &mut Vec<Event>) {
        match self.next_metrics_us {
            None => {
                self.next_metrics_us = Some(now_us + METRICS_INTERVAL_US);
                return;
            }
            Some(t) if now_us < t => return,
            Some(_) => self.next_metrics_us = Some(now_us + METRICS_INTERVAL_US),
        }
        let s = if self.audio_running {
            self.audio.stats()
        } else {
            AudioSnapshot::default()
        };
        if s.unattenuated_blocks > self.unattenuated_seen {
            eprintln!(
                "devocal engine: safety guard silenced {} block(s) so far",
                s.unattenuated_blocks
            );
        }
        self.unattenuated_seen = s.unattenuated_blocks;
        let (mode, reason) = self.current_mode(now_us);
        ev.push(Event::Metrics(Metrics {
            mode,
            latency_ms: s.latency_ms,
            load_ratio: s.load_ratio,
            underruns: s.underruns,
            fallback_reason: reason,
            attenuation: self.holder.attenuation(),
            attenuation_epoch: self.holder.attenuation_epoch(),
            session_overridden: self.overridden_total,
            input_silent_ms: s.input_silent_ms,
        }));
    }
}

/// [`AudioPort`] over the real audio threads.
#[derive(Default)]
pub struct RealAudio {
    handle: Option<AudioHandle>,
}

impl AudioPort for RealAudio {
    fn start(
        &mut self,
        cfg: AudioConfig,
        gains: Arc<SharedGains>,
        processor: Processor,
    ) -> Result<(), String> {
        if let Some(old) = self.stop() {
            retire(old);
        }
        self.handle = Some(AudioHandle::start(cfg, gains, processor)?);
        Ok(())
    }

    fn set_devocal(&self, on: bool) {
        if let Some(h) = &self.handle {
            h.set_devocal(on);
        }
    }

    fn set_separator(&self, s: Box<dyn Separator>) {
        if let Some(h) = &self.handle {
            h.set_separator(s);
        }
    }

    fn rebind_output(&self, endpoint: Option<String>) {
        if let Some(h) = &self.handle {
            h.rebind_output(endpoint);
        }
    }

    fn output_failed(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.output_failed())
    }

    fn capture_failed(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.capture_failed())
    }

    fn failed(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.failed())
    }

    fn stage(&self) -> Stage {
        self.handle
            .as_ref()
            .map_or(Stage::Passthrough, |h| h.stage())
    }

    fn fallback_reason(&self) -> Option<FallbackReason> {
        self.handle.as_ref().and_then(|h| h.fallback_reason())
    }

    fn take_follow_request(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|h| h.take_follow_request())
    }

    fn stats(&self) -> AudioSnapshot {
        let Some(h) = &self.handle else {
            return AudioSnapshot::default();
        };
        let s = &h.stats;
        AudioSnapshot {
            underruns: s.underruns.load(Ordering::Relaxed),
            input_silent_ms: s.input_silent_ms.load(Ordering::Relaxed),
            load_ratio: s.load_ratio_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            latency_ms: s.latency_ms_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            unattenuated_blocks: s.unattenuated_blocks.load(Ordering::Relaxed),
            headroom_frames: s.headroom_frames.load(Ordering::Relaxed),
        }
    }

    fn stop(&mut self) -> Option<Box<dyn Separator>> {
        self.handle.take().and_then(|h| h.stop())
    }
}

/// What the main loop saw since the previous pass.
#[derive(Debug, Default)]
pub struct LoopInput {
    /// Command lines received, in order.
    pub lines: Vec<String>,
    /// The pipe reached end of stream, or a read or write failed.
    pub link_closed: bool,
    /// The app process has exited.
    pub app_exited: bool,
}

/// Main-loop state kept between passes.
#[derive(Debug)]
pub struct LoopState {
    link_up: bool,
    exit_deadline: Option<u64>,
}

impl Default for LoopState {
    fn default() -> Self {
        Self {
            link_up: true,
            exit_deadline: None,
        }
    }
}

impl LoopState {
    /// Events may still be sent to the app.
    pub fn link_up(&self) -> bool {
        self.link_up
    }
}

/// One pass of the main loop: the commands, the watchdog (pipe closed or app gone ->
/// release and exit), the tick, and the exit decision (released and stopped, or 2 s after
/// the exit request). `clock` is read again for every step. Appends the events to send to
/// `out`; returns true when the engine should exit.
pub fn loop_once<S: SessionVolumes, A: AudioPort>(
    core: &mut EngineCore<S, A>,
    st: &mut LoopState,
    input: LoopInput,
    clock: &mut dyn FnMut() -> u64,
    out: &mut Vec<Event>,
) -> bool {
    for line in &input.lines {
        out.extend(core.handle_line(line, clock()));
    }
    if input.link_closed {
        st.link_up = false;
    }
    if !core.exit_requested() {
        let why = if !st.link_up {
            Some("the app pipe closed")
        } else if input.app_exited {
            Some("the app process exited")
        } else {
            None
        };
        if let Some(why) = why {
            eprintln!("devocal engine: {why}; releasing and exiting");
            out.extend(core.request_exit(clock()));
        }
    }
    out.extend(core.tick(clock()));
    if core.exit_requested() {
        let now = clock();
        let deadline = *st
            .exit_deadline
            .get_or_insert(now + EXIT_RELEASE_TIMEOUT_US);
        if core.exit_ready() {
            return true;
        }
        if now >= deadline {
            eprintln!(
                "devocal engine: the release did not finish within 2 s; exiting and leaving \
                 the restore file for the app"
            );
            return true;
        }
    }
    false
}

/// Command line: `--app-pid <u32> --restore-file <path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub app_pid: u32,
    pub restore_file: PathBuf,
}

pub const USAGE: &str = "usage: devocal-engine --app-pid <u32> --restore-file <path>";

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let (mut app_pid, mut restore_file) = (None, None);
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--app-pid" => {
                let v = value()?;
                app_pid = Some(
                    v.parse::<u32>()
                        .map_err(|e| format!("--app-pid {v:?}: {e}"))?,
                );
            }
            "--restore-file" => restore_file = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Args {
        app_pid: app_pid.ok_or("--app-pid is required")?,
        restore_file: restore_file.ok_or("--restore-file is required")?,
    })
}

/// The app process, watched with `WaitForSingleObject(handle, 0)`.
struct AppProcess(HANDLE);

impl AppProcess {
    fn open(pid: u32) -> Result<Self, String> {
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
            .map(AppProcess)
            .map_err(|e| format!("opening the app process {pid}: {e}"))
    }

    fn exited(&self) -> bool {
        let r = unsafe { WaitForSingleObject(self.0, 0) };
        r == WAIT_OBJECT_0
    }
}

impl Drop for AppProcess {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The engine process: pipe, watchdog and the 1 ms main loop. Returns the exit code: 0 after
/// a normal exit (shutdown, app gone, pipe closed, protocol mismatch), 1 when it could not
/// start. The calling thread must be in the COM MTA (`WinSessions`).
pub fn run(args: Args) -> i32 {
    let app = match AppProcess::open(args.app_pid) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("devocal engine: {e}");
            return 1;
        }
    };
    let name = pipe_name(args.app_pid);
    let server = match PipeServer::create(&name) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("devocal engine: {e}");
            return 1;
        }
    };
    match server.accept(args.app_pid, CONNECT_TIMEOUT, &mut || !app.exited()) {
        Ok(Accepted::Connected) => {}
        Ok(other) => {
            // Nothing is held yet: just leave.
            eprintln!("devocal engine: no app connection ({other:?}); exiting");
            return 0;
        }
        Err(e) => {
            eprintln!("devocal engine: accepting the app connection: {e}");
            return 1;
        }
    }

    // Reader: lines in, `None` at end of stream or error.
    let (line_tx, line_rx) = mpsc::channel::<Option<String>>();
    let reader = server.clone();
    let spawned = thread::Builder::new()
        .name("devocal-pipe-read".into())
        .spawn(move || loop {
            match reader.read_line() {
                Ok(Some(line)) => {
                    if line_tx.send(Some(line)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = line_tx.send(None);
                    break;
                }
                Err(e) => {
                    eprintln!("devocal engine: pipe read: {e}");
                    let _ = line_tx.send(None);
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("devocal engine: spawn pipe reader: {e}");
        return 1;
    }

    // Writer: the main loop never blocks on the pipe.
    let (out_tx, out_rx) = mpsc::channel::<String>();
    let (flushed_tx, flushed_rx) = mpsc::channel::<()>();
    let write_failed = Arc::new(AtomicBool::new(false));
    let writer = server.clone();
    let failed_flag = write_failed.clone();
    let spawned = thread::Builder::new()
        .name("devocal-pipe-write".into())
        .spawn(move || {
            for line in out_rx {
                if let Err(e) = writer.write_line(&line) {
                    eprintln!("devocal engine: pipe write: {e}");
                    failed_flag.store(true, Ordering::Release);
                    break;
                }
            }
            let _ = flushed_tx.send(());
        });
    if let Err(e) = spawned {
        eprintln!("devocal engine: spawn pipe writer: {e}");
        return 1;
    }

    let mut core = EngineCore::new(
        WinSessions,
        args.restore_file,
        RealAudio::default(),
        stemgen_loader(),
    );
    // Without notifications the engine still follows every 0.5 s.
    let mut watcher = match SessionWatcher::start(core.signals()) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("devocal engine: no session notifications ({e}); polling only");
            None
        }
    };
    let mut st = LoopState::default();
    let mut clock = now_us;
    let mut reader_done = false;
    loop {
        let mut input = LoopInput::default();
        while !reader_done {
            match line_rx.try_recv() {
                Ok(Some(line)) => input.lines.push(line),
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => reader_done = true,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        input.link_closed = reader_done || write_failed.load(Ordering::Acquire);
        input.app_exited = app.exited();
        if let Some(w) = watcher.as_mut() {
            w.maintain();
        }
        let mut events = Vec::new();
        let exit = loop_once(&mut core, &mut st, input, &mut clock, &mut events);
        for e in &events {
            match encode(e) {
                Ok(line) => {
                    if st.link_up() {
                        let _ = out_tx.send(line);
                    }
                }
                Err(err) => eprintln!("devocal engine: cannot encode {e:?}: {err}"),
            }
        }
        if exit {
            break;
        }
        thread::sleep(Duration::from_micros(TICK_US));
    }
    core.stop_audio();
    // Unregister the notifications while COM is still initialised on this thread.
    drop(watcher);
    drop(out_tx);
    let _ = flushed_rx.recv_timeout(FLUSH_TIMEOUT);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::separator::DelayOnly;
    use devocal_core::protocol::{ErrorCode, PROTOCOL};
    use devocal_core::sessions::{FakeSessions, SessionInfo, HELD_VOLUME};
    use std::cell::{Cell, RefCell};
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PID: u32 = 100;
    const CREATED: u64 = 555;
    const MS: u64 = 1_000;
    const T0: u64 = 1_000_000;
    const SESSION: &str = "ep1|player|1%b100";
    const SESSION2: &str = "ep2|player|1%b100";

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "devocal-engine-test-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("devocal-restore.json")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct FakeState {
        running: bool,
        /// `set_devocal` leaves the stage alone (the processing thread has not applied it yet).
        lazy_stage: bool,
        start_error: Option<String>,
        /// Output endpoint of every start.
        starts: Vec<Option<String>>,
        stops: usize,
        devocal_calls: Vec<bool>,
        separators: usize,
        rebinds: Vec<Option<String>>,
        output_failed: bool,
        capture_failed: bool,
        failed: bool,
        stage: Option<Stage>,
        reason: Option<FallbackReason>,
        follow_request: bool,
        stats: AudioSnapshot,
        /// The gains of the latest start (what the engine publishes).
        gains: Option<Arc<SharedGains>>,
        /// The model the running audio holds (from the start or `set_separator`).
        model: Option<Box<dyn Separator>>,
        /// Whether each start came with a model.
        start_had_model: Vec<bool>,
        /// Ordered `set_separator` / `set_devocal` calls.
        log: Vec<String>,
        /// `stop` hands no model back (a thread left detached).
        lose_model_on_stop: bool,
        /// `start` advances this clock by `start_delay_us` (a slow audio start).
        clock: Option<Rc<Cell<u64>>>,
        start_delay_us: u64,
    }

    /// Audio double: records every call; the test sets stage, failures and stats.
    #[derive(Default)]
    struct FakeAudio(RefCell<FakeState>);

    impl FakeAudio {
        fn st(&self) -> std::cell::Ref<'_, FakeState> {
            self.0.borrow()
        }
        fn set_stage(&self, stage: Stage, reason: Option<FallbackReason>) {
            let mut s = self.0.borrow_mut();
            s.stage = Some(stage);
            s.reason = reason;
        }
        fn set_capture_failed(&self) {
            let mut s = self.0.borrow_mut();
            s.capture_failed = true;
            s.failed = true;
        }
        fn set_render_failed(&self) {
            self.0.borrow_mut().failed = true;
        }
        fn set_output_failed(&self, v: bool) {
            self.0.borrow_mut().output_failed = v;
        }
        fn request_follow(&self) {
            self.0.borrow_mut().follow_request = true;
        }
        fn fail_next_start(&self, msg: &str) {
            self.0.borrow_mut().start_error = Some(msg.into());
        }
        fn set_lazy_stage(&self) {
            self.0.borrow_mut().lazy_stage = true;
        }
        fn set_stats(&self, stats: AudioSnapshot) {
            self.0.borrow_mut().stats = stats;
        }
    }

    impl AudioPort for FakeAudio {
        fn start(
            &mut self,
            cfg: AudioConfig,
            gains: Arc<SharedGains>,
            mut processor: Processor,
        ) -> Result<(), String> {
            let mut s = self.0.borrow_mut();
            assert!(!s.running, "start while running");
            assert_eq!(cfg.pid, PID);
            assert_eq!(cfg.hop, processor.hop());
            if let Some(c) = &s.clock {
                c.set(c.get() + s.start_delay_us);
            }
            if let Some(e) = s.start_error.take() {
                return Err(e);
            }
            let model = processor.take_separator();
            s.start_had_model.push(model.is_some());
            s.model = model;
            s.gains = Some(gains);
            s.running = true;
            s.starts.push(cfg.output_endpoint);
            s.stage = Some(Stage::Passthrough);
            s.reason = None;
            Ok(())
        }
        fn set_devocal(&self, on: bool) {
            let mut s = self.0.borrow_mut();
            assert!(s.running, "set_devocal while stopped");
            s.devocal_calls.push(on);
            s.log.push(format!("devocal:{on}"));
            if !s.lazy_stage {
                s.stage = Some(if on {
                    Stage::Devocal
                } else {
                    Stage::Passthrough
                });
                s.reason = None;
            }
        }
        fn set_separator(&self, model: Box<dyn Separator>) {
            let mut s = self.0.borrow_mut();
            assert!(s.running, "set_separator while stopped");
            s.separators += 1;
            s.log.push("separator".into());
            s.model = Some(model);
        }
        fn rebind_output(&self, endpoint: Option<String>) {
            self.0.borrow_mut().rebinds.push(endpoint);
        }
        fn output_failed(&self) -> bool {
            self.st().output_failed
        }
        fn capture_failed(&self) -> bool {
            self.st().capture_failed
        }
        fn failed(&self) -> bool {
            self.st().failed
        }
        fn stage(&self) -> Stage {
            self.st().stage.unwrap_or(Stage::Passthrough)
        }
        fn fallback_reason(&self) -> Option<FallbackReason> {
            self.st().reason
        }
        fn take_follow_request(&self) -> bool {
            std::mem::take(&mut self.0.borrow_mut().follow_request)
        }
        fn stats(&self) -> AudioSnapshot {
            self.st().stats
        }
        fn stop(&mut self) -> Option<Box<dyn Separator>> {
            let mut s = self.0.borrow_mut();
            if s.running {
                s.running = false;
                s.stops += 1;
            }
            s.capture_failed = false;
            s.failed = false;
            s.output_failed = false;
            let model = s.model.take();
            if s.lose_model_on_stop {
                None
            } else {
                model
            }
        }
    }

    fn session(id: &str, endpoint: &str) -> SessionInfo {
        SessionInfo {
            instance_id: id.into(),
            session_identifier: format!("{endpoint}|player.exe%b{{0}}"),
            pid: PID,
            endpoint_id: endpoint.into(),
            active: true,
        }
    }

    fn fake_sessions() -> FakeSessions {
        let f = FakeSessions::new();
        f.set_process_created(PID, Some(CREATED));
        f.add_session(session(SESSION, "ep1"), 0.8, false);
        f
    }

    /// `DelayOnly` that counts `reset` calls.
    struct TestSep {
        inner: DelayOnly,
        resets: Arc<AtomicUsize>,
    }

    impl Separator for TestSep {
        fn sample_rate(&self) -> u32 {
            self.inner.sample_rate()
        }
        fn hop(&self) -> usize {
            self.inner.hop()
        }
        fn latency_frames(&self) -> usize {
            self.inner.latency_frames()
        }
        fn process(&mut self, input: &[f32], out: &mut [f32]) -> Result<(), String> {
            self.inner.process(input, out)
        }
        fn reset(&mut self) {
            self.resets.fetch_add(1, Ordering::Relaxed);
            self.inner.reset();
        }
    }

    type LoadSender = mpsc::Sender<Result<Box<dyn Separator>, String>>;

    /// Loads finish at once, or (`manual`) when the test says so.
    #[derive(Default)]
    struct FakeLoads {
        calls: usize,
        manual: bool,
        pending: Vec<(PathBuf, LoadSender)>,
        resets: Arc<AtomicUsize>,
    }

    /// A `TestSep` for any path except `*bad.onnx`.
    fn fake_load(path: &Path, resets: &Arc<AtomicUsize>) -> Result<Box<dyn Separator>, String> {
        if path.to_string_lossy().ends_with("bad.onnx") {
            Err("cannot load model".into())
        } else {
            Ok(Box::new(TestSep {
                inner: DelayOnly::new(128),
                resets: resets.clone(),
            }))
        }
    }

    fn loader(loads: Rc<RefCell<FakeLoads>>) -> ModelLoader {
        Box::new(move |path: PathBuf, _threads: u16| {
            let (tx, rx) = mpsc::channel();
            let mut l = loads.borrow_mut();
            l.calls += 1;
            if l.manual {
                l.pending.push((path, tx));
            } else {
                let _ = tx.send(fake_load(&path, &l.resets));
            }
            rx
        })
    }

    type Core = EngineCore<FakeSessions, FakeAudio>;

    struct Rig {
        core: Core,
        loads: Rc<RefCell<FakeLoads>>,
        now: u64,
        restore: PathBuf,
        _dir: TempDir,
    }

    impl Rig {
        fn new(tag: &str) -> Self {
            let dir = TempDir::new(tag);
            let path = dir.file();
            Self::with_path(dir, path)
        }

        fn with_path(dir: TempDir, restore: PathBuf) -> Self {
            let loads = Rc::new(RefCell::new(FakeLoads::default()));
            let core = EngineCore::new(
                fake_sessions(),
                restore.clone(),
                FakeAudio::default(),
                loader(loads.clone()),
            );
            Rig {
                core,
                loads,
                now: T0,
                restore,
                _dir: dir,
            }
        }

        fn send(&mut self, cmd: Command) -> Vec<Event> {
            self.core.handle(cmd, self.now)
        }

        /// Ticks every millisecond for `ms` milliseconds.
        fn run(&mut self, ms: u64) -> Vec<Event> {
            let mut ev = Vec::new();
            for _ in 0..ms {
                ev.extend(self.core.tick(self.now));
                self.now += MS;
            }
            ev
        }

        fn audio(&self) -> &FakeAudio {
            self.core.audio()
        }

        fn loads(&self) -> usize {
            self.loads.borrow().calls
        }

        fn manual_loads(&self) {
            self.loads.borrow_mut().manual = true;
        }

        /// Completes manual load `i`; false if the engine had abandoned it.
        fn finish_load(&self, i: usize) -> bool {
            let l = self.loads.borrow();
            let (path, tx) = &l.pending[i];
            tx.send(fake_load(path, &l.resets)).is_ok()
        }

        fn resets(&self) -> usize {
            self.loads.borrow().resets.load(Ordering::Relaxed)
        }

        fn volume(&self, id: &str) -> f32 {
            self.core.sessions().volume(id).unwrap()
        }

        /// hello, set_model, attach and 100 ms of ticks: Active(Passthrough).
        fn attached(tag: &str) -> (Self, Vec<Event>) {
            let mut r = Rig::new(tag);
            let mut ev = r.send(Command::Hello { version: PROTOCOL });
            ev.extend(r.send(set_model("model.onnx", "cpu")));
            ev.extend(r.send(attach()));
            ev.extend(r.run(100));
            assert_eq!(r.core.phase(), Phase::Active, "{ev:?}");
            (r, ev)
        }
    }

    fn set_model(path: &str, device: &str) -> Command {
        Command::SetModel {
            id: "fake".into(),
            path: PathBuf::from(path),
            device: device.into(),
            threads: 1,
        }
    }

    fn attach() -> Command {
        Command::Attach {
            pid: PID,
            created_at: CREATED,
        }
    }

    type StateTuple = (Phase, Option<Mode>, Option<FallbackReason>);

    fn states(ev: &[Event]) -> Vec<StateTuple> {
        ev.iter()
            .filter_map(|e| match e {
                Event::State {
                    phase,
                    mode,
                    fallback_reason,
                    ..
                } => Some((*phase, *mode, *fallback_reason)),
                _ => None,
            })
            .collect()
    }

    fn errors(ev: &[Event]) -> Vec<ErrorCode> {
        ev.iter()
            .filter_map(|e| match e {
                Event::Error { code, .. } => Some(*code),
                _ => None,
            })
            .collect()
    }

    fn error_messages(ev: &[Event]) -> Vec<String> {
        ev.iter()
            .filter_map(|e| match e {
                Event::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    fn metrics(ev: &[Event]) -> Vec<devocal_core::protocol::Metrics> {
        ev.iter()
            .filter_map(|e| match e {
                Event::Metrics(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    const ACTIVE_PASS: StateTuple = (Phase::Active, Some(Mode::Passthrough), None);
    const ACTIVE_DEVOCAL: StateTuple = (Phase::Active, Some(Mode::Devocal), None);
    const ATTACHING: StateTuple = (Phase::Attaching, None, None);
    const RELEASING: StateTuple = (Phase::Releasing, None, None);
    const IDLE: StateTuple = (Phase::Idle, None, None);

    #[test]
    fn full_lifecycle_emits_states_in_order() {
        let mut r = Rig::new("lifecycle");
        let mut ev = r.send(Command::Hello { version: PROTOCOL });
        ev.extend(r.send(set_model("model.onnx", "cpu")));
        ev.extend(r.send(attach()));
        ev.extend(r.run(100));
        assert_eq!(r.volume(SESSION), HELD_VOLUME);
        ev.extend(r.send(Command::SetMode { devocal: true }));
        ev.extend(r.run(100));
        ev.extend(r.send(Command::Release));
        ev.extend(r.run(100));

        assert_eq!(
            states(&ev),
            vec![ATTACHING, ACTIVE_PASS, ACTIVE_DEVOCAL, RELEASING, IDLE]
        );
        assert!(errors(&ev).is_empty(), "{ev:?}");
        let attached_pids: Vec<Option<u32>> = ev
            .iter()
            .filter_map(|e| match e {
                Event::State { attached_pid, .. } => Some(*attached_pid),
                _ => None,
            })
            .collect();
        assert_eq!(
            attached_pids,
            vec![Some(PID), Some(PID), Some(PID), Some(PID), None]
        );
        assert_eq!(r.volume(SESSION), 0.8);
        assert!(!r.core.sessions().writes().is_empty());
        let st = r.audio().st();
        assert_eq!(st.starts, vec![Some("ep1".to_string())]);
        assert_eq!(st.devocal_calls, vec![true]);
        assert_eq!(st.stops, 1);
        assert!(!st.running);
        drop(st);
        assert_eq!(r.loads(), 1);
        assert!(!r.core.exit_requested());
    }

    #[test]
    fn bad_device_rejected() {
        let mut r = Rig::new("bad-device");
        let ev = r.send(set_model("model.onnx", "dml"));
        assert_eq!(errors(&ev), vec![ErrorCode::BadDevice]);
        assert_eq!(r.loads(), 0, "the model is not loaded");
        // Still no model.
        let ev = r.send(Command::SetMode { devocal: true });
        assert_eq!(errors(&ev), vec![ErrorCode::NoModel]);
    }

    #[test]
    fn model_load_failure_reported() {
        let mut r = Rig::new("load-fail");
        let ev = r.send(set_model("bad.onnx", "cpu"));
        assert_eq!(errors(&ev), vec![ErrorCode::ModelLoadFailed]);
        assert!(error_messages(&ev)[0].contains("cannot load model"));
        let ev = r.send(Command::SetMode { devocal: true });
        assert_eq!(errors(&ev), vec![ErrorCode::NoModel]);
    }

    #[test]
    fn devocal_without_model_rejected() {
        let mut r = Rig::new("no-model");
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        let rejected = r.send(Command::SetMode { devocal: true });
        assert_eq!(errors(&rejected), vec![ErrorCode::NoModel]);
        ev.extend(rejected);
        ev.extend(r.run(100));
        assert_eq!(states(&ev), vec![ATTACHING, ACTIVE_PASS]);
        assert!(r.audio().st().devocal_calls.is_empty());
    }

    #[test]
    fn model_loaded_while_active_goes_to_the_audio_side() {
        let mut r = Rig::new("model-live");
        r.send(attach());
        r.run(100);
        let ev = r.send(set_model("model.onnx", "cpu"));
        assert!(errors(&ev).is_empty());
        assert_eq!(r.audio().st().separators, 1);
        let ev = r.send(Command::SetMode { devocal: true });
        assert!(errors(&ev).is_empty());
        assert_eq!(states(&ev), vec![ACTIVE_DEVOCAL]);
    }

    #[test]
    fn protocol_mismatch_releases() {
        let (mut r, _) = Rig::attached("mismatch-hello");
        let mut ev = r.send(Command::Hello {
            version: PROTOCOL + 1,
        });
        assert_eq!(errors(&ev), vec![ErrorCode::Protocol]);
        assert!(r.core.exit_requested());
        ev.extend(r.run(100));
        assert_eq!(states(&ev), vec![RELEASING, IDLE]);
        assert_eq!(r.volume(SESSION), 0.8);
        assert!(r.core.exit_ready());
        assert!(!r.audio().st().running);

        // A line with another protocol number is handled the same way.
        let (mut r, _) = Rig::attached("mismatch-line");
        let mut ev = r
            .core
            .handle_line("{\"protocol\":2,\"cmd\":\"release\"}", r.now);
        assert_eq!(errors(&ev), vec![ErrorCode::Protocol]);
        ev.extend(r.run(100));
        assert_eq!(states(&ev), vec![RELEASING, IDLE]);
        assert!(r.core.exit_ready());
    }

    #[test]
    fn malformed_line_is_reported_and_ignored() {
        let (mut r, _) = Rig::attached("malformed");
        let ev = r
            .core
            .handle_line("{\"protocol\":1,\"cmd\":\"explode\"}", r.now);
        assert_eq!(errors(&ev), vec![ErrorCode::Protocol]);
        assert!(!r.core.exit_requested());
        assert_eq!(r.core.phase(), Phase::Active);
        let ev = r
            .core
            .handle_line("{\"protocol\":1,\"cmd\":\"release\"}\n", r.now);
        assert_eq!(states(&ev), vec![RELEASING]);
    }

    #[test]
    fn shutdown_releases_then_exit_is_ready() {
        let (mut r, _) = Rig::attached("shutdown");
        let mut ev = r.send(Command::Shutdown);
        assert!(r.core.exit_requested());
        assert!(!r.core.exit_ready());
        ev.extend(r.run(100));
        assert_eq!(states(&ev), vec![RELEASING, IDLE]);
        assert!(r.core.exit_ready());
        assert_eq!(r.volume(SESSION), 0.8);
        // Idle already: exit is ready at once.
        let mut r = Rig::new("shutdown-idle");
        r.send(Command::Shutdown);
        assert!(r.core.exit_ready());
    }

    #[test]
    fn metrics_once_per_second() {
        let mut r = Rig::new("metrics");
        let ev = r.run(2_500);
        assert_eq!(metrics(&ev).len(), 2);

        let (mut r, _) = Rig::attached("metrics-active");
        r.audio().set_stats(AudioSnapshot {
            underruns: 3,
            input_silent_ms: 40,
            load_ratio: 0.25,
            latency_ms: 21.5,
            unattenuated_blocks: 0,
            headroom_frames: 0,
        });
        let m = metrics(&r.run(1_000));
        assert_eq!(m.len(), 1);
        let m = &m[0];
        assert_eq!(m.mode, Some(Mode::Passthrough));
        assert_eq!(m.underruns, 3);
        assert_eq!(m.input_silent_ms, 40);
        assert_eq!(m.load_ratio, 0.25);
        assert_eq!(m.latency_ms, 21.5);
        assert!((m.attenuation - HELD_VOLUME / 0.8).abs() < 1e-9);
        assert!(m.attenuation_epoch >= 1);
    }

    #[test]
    fn overload_reported_as_fallback() {
        let (mut r, _) = Rig::attached("overload");
        let mut ev = r.send(Command::SetMode { devocal: true });
        ev.extend(r.run(100));
        assert_eq!(states(&ev), vec![ACTIVE_DEVOCAL]);

        r.audio()
            .set_stage(Stage::Fallback, Some(FallbackReason::Overload));
        let mut ev = r.run(2_000);
        // State sync from the app repeats "on": never forwarded again.
        ev.extend(r.send(Command::SetMode { devocal: true }));
        ev.extend(r.run(1_000));
        assert_eq!(
            states(&ev),
            vec![(
                Phase::Active,
                Some(Mode::Fallback),
                Some(FallbackReason::Overload)
            )]
        );
        assert_eq!(r.audio().st().devocal_calls, vec![true]);
        assert_eq!(r.audio().stage(), Stage::Fallback);
        let m = metrics(&ev);
        assert!(!m.is_empty());
        assert!(m.iter().all(|m| m.mode == Some(Mode::Fallback)
            && m.fallback_reason == Some(FallbackReason::Overload)));

        // A user toggle off/on is forwarded and leaves the fallback.
        let mut ev = r.send(Command::SetMode { devocal: false });
        ev.extend(r.send(Command::SetMode { devocal: true }));
        ev.extend(r.run(10));
        assert_eq!(r.audio().st().devocal_calls, vec![true, false, true]);
        assert_eq!(states(&ev), vec![ACTIVE_PASS, ACTIVE_DEVOCAL]);
    }

    #[test]
    fn fallback_while_user_off_is_passthrough() {
        let (mut r, _) = Rig::attached("fallback-off");
        r.audio()
            .set_stage(Stage::Fallback, Some(FallbackReason::Overload));
        let ev = r.run(100);
        assert!(states(&ev).is_empty(), "{ev:?}");
    }

    #[test]
    fn set_mode_forwarded_only_on_change() {
        let (mut r, _) = Rig::attached("toggle");
        for on in [true, true, false, false, true] {
            r.send(Command::SetMode { devocal: on });
            r.run(5);
        }
        assert_eq!(r.audio().st().devocal_calls, vec![true, false, true]);
    }

    #[test]
    fn user_toggle_before_attach_applies_to_the_new_audio_once() {
        let mut r = Rig::new("toggle-early");
        r.send(set_model("model.onnx", "cpu"));
        let ev = r.send(Command::SetMode { devocal: true });
        assert!(errors(&ev).is_empty());
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        ev.extend(r.send(Command::SetMode { devocal: true }));
        ev.extend(r.run(10));
        assert_eq!(r.audio().st().devocal_calls, vec![true]);
        assert_eq!(states(&ev), vec![ATTACHING, ACTIVE_DEVOCAL]);
    }

    #[test]
    fn stage_mapping_reports_pending_and_fallback_reasons() {
        use FallbackReason::*;
        use Stage::*;
        // User on: everything before the fallback is "devocal (pending)".
        for stage in [Passthrough, WarmingUp, FadingIn, Devocal] {
            assert_eq!(
                mode_for(true, stage, None),
                (Mode::Devocal, None),
                "{stage:?}"
            );
        }
        // FadingOut: the reason decides (a model swap fades out without one).
        assert_eq!(mode_for(true, FadingOut, None), (Mode::Devocal, None));
        assert_eq!(
            mode_for(true, FadingOut, Some(Overload)),
            (Mode::Fallback, Some(Overload))
        );
        assert_eq!(
            mode_for(true, Fallback, Some(ModelError)),
            (Mode::Fallback, Some(ModelError))
        );
        // User off: plain passthrough whatever the stage says.
        for stage in [
            Passthrough,
            WarmingUp,
            FadingIn,
            Devocal,
            FadingOut,
            Fallback,
        ] {
            assert_eq!(
                mode_for(false, stage, Some(Overload)),
                (Mode::Passthrough, None),
                "{stage:?}"
            );
        }
    }

    #[test]
    fn endpoint_change_rebinds() {
        let (mut r, _) = Rig::attached("endpoint");
        r.run(1_000);
        assert!(r.audio().st().rebinds.is_empty());

        // The player moves to another endpoint.
        r.core.sessions().remove_session(SESSION);
        r.core
            .sessions()
            .add_session(session(SESSION2, "ep2"), 0.8, false);
        r.run(600);
        assert_eq!(r.audio().st().rebinds, vec![Some("ep2".to_string())]);
        r.run(1_500);
        assert_eq!(r.audio().st().rebinds.len(), 1, "rebinds exactly once");

        // The endpoint failed: rebind at the next check.
        r.audio().set_output_failed(true);
        r.run(600);
        assert_eq!(
            r.audio().st().rebinds,
            vec![Some("ep2".to_string()), Some("ep2".to_string())]
        );
        r.audio().set_output_failed(false);
        r.run(1_000);
        assert_eq!(r.audio().st().rebinds.len(), 2);

        // The player has no session any more and the output still works: keep the binding.
        r.core.sessions().remove_session(SESSION2);
        r.run(1_000);
        assert_eq!(r.audio().st().rebinds.len(), 2);
        // ...but a failed output then goes to the default endpoint.
        r.audio().set_output_failed(true);
        r.run(600);
        assert_eq!(r.audio().st().rebinds.last(), Some(&None));
    }

    #[test]
    fn attach_failure_returns_to_idle() {
        // The restore file cannot be written: nothing is lowered.
        let dir = TempDir::new("attach-fail");
        let path = dir.0.join("missing").join("devocal-restore.json");
        let mut r = Rig::with_path(dir, path);
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        assert_eq!(errors(&ev), vec![ErrorCode::AttachFailed]);
        assert_eq!(states(&ev), vec![ATTACHING, IDLE]);
        assert_eq!(r.core.phase(), Phase::Idle);
        assert_eq!(r.volume(SESSION), 0.8);
        assert_eq!(r.core.sessions().set_volume_calls(), 0);
        assert!(!r.audio().st().running);
        // A later attach works.
        let ev = r.send(attach());
        assert_eq!(states(&ev), vec![ATTACHING]);
    }

    #[test]
    fn audio_start_failure_returns_to_idle_without_lowering() {
        let mut r = Rig::new("start-fail");
        r.audio().fail_next_start("process loopback unavailable");
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        assert_eq!(errors(&ev), vec![ErrorCode::AttachFailed]);
        assert!(error_messages(&ev)[0].contains("process loopback unavailable"));
        assert_eq!(states(&ev), vec![ATTACHING, IDLE]);
        assert_eq!(r.core.sessions().set_volume_calls(), 0);
        assert!(!r.restore.exists());
    }

    #[test]
    fn holder_failure_during_attach_releases() {
        let mut r = Rig::new("holder-fail");
        r.core.sessions().fail_set_volume(SESSION);
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        assert_eq!(errors(&ev), vec![ErrorCode::AttachFailed]);
        assert_eq!(states(&ev), vec![ATTACHING, RELEASING, IDLE]);
        assert!(!r.audio().st().running);
    }

    #[test]
    fn unclean_attach_report_is_surfaced_but_attach_continues() {
        let dir = TempDir::new("corrupt");
        std::fs::write(dir.file(), b"not json").unwrap();
        let path = dir.file();
        let mut r = Rig::with_path(dir, path);
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        assert_eq!(errors(&ev), vec![ErrorCode::AttachFailed]);
        assert!(error_messages(&ev)[0].contains("restore"), "{ev:?}");
        assert_eq!(states(&ev), vec![ATTACHING, ACTIVE_PASS]);
        assert_eq!(r.volume(SESSION), HELD_VOLUME);
    }

    #[test]
    fn capture_failure_releases() {
        let (mut r, _) = Rig::attached("capture-fail");
        r.audio().set_capture_failed();
        let mut ev = r.run(1);
        assert_eq!(errors(&ev), vec![ErrorCode::CaptureFailed]);
        ev.extend(r.run(100));
        assert_eq!(states(&ev), vec![RELEASING, IDLE]);
        assert_eq!(r.volume(SESSION), 0.8);
        assert!(!r.audio().st().running);
        assert!(!r.core.exit_requested());
    }

    #[test]
    fn render_failure_releases() {
        let (mut r, _) = Rig::attached("render-fail");
        r.audio().set_render_failed();
        let mut ev = r.run(100);
        assert_eq!(errors(&ev), vec![ErrorCode::RenderFailed]);
        ev.extend(r.run(10));
        assert_eq!(states(&ev), vec![RELEASING, IDLE]);
        assert_eq!(r.volume(SESSION), 0.8);
    }

    #[test]
    fn release_failure_still_ends_idle_and_keeps_the_restore_file() {
        let (mut r, _) = Rig::attached("release-fail");
        r.core.sessions().fail_set_volume(SESSION);
        let mut ev = r.send(Command::Release);
        ev.extend(r.run(200));
        assert_eq!(states(&ev), vec![RELEASING, IDLE]);
        assert!(r.restore.exists());
        assert!(!r.audio().st().running);
    }

    #[test]
    fn player_exit_goes_idle() {
        let (mut r, _) = Rig::attached("player-exit");
        r.core.sessions().set_process_created(PID, None);
        let ev = r.run(600);
        assert_eq!(states(&ev), vec![IDLE]);
        assert_eq!(r.volume(SESSION), 0.8);
        assert!(!r.audio().st().running);
    }

    #[test]
    fn guard_request_runs_follow_at_once() {
        let (mut r, _) = Rig::attached("guard");
        // Something turned the session back up right after a follow pass.
        r.run(450);
        r.core.sessions().set_volume(SESSION, 0.8).unwrap();
        r.run(1);
        assert_eq!(r.volume(SESSION), 0.8, "no follow due yet");
        r.audio().request_follow();
        r.run(1);
        assert_eq!(r.volume(SESSION), HELD_VOLUME);
    }

    /// The capture gain the engine last published to the audio side.
    fn published_capture_gain(r: &Rig) -> f32 {
        r.audio()
            .st()
            .gains
            .as_ref()
            .expect("audio started")
            .capture_gain()
    }

    #[test]
    fn session_notification_runs_follow_at_once() {
        let (mut r, _) = Rig::attached("session-event");
        r.run(450);
        assert!((published_capture_gain(&r) - 8000.0).abs() < 0.5);
        // The player opens a second stream; Windows starts its session at 1.0.
        const NEW: &str = "ep1|player|2%b100";
        r.core
            .sessions()
            .add_session(session(NEW, "ep1"), 1.0, false);
        r.run(1);
        assert_eq!(r.volume(NEW), 1.0, "no follow due yet");
        r.core.signals().notify_session_created();
        r.run(1);
        assert_eq!(r.volume(NEW), HELD_VOLUME);
        // It played at 1.0 until now: the capture gain is at most 0.8 / 1.0 (no x8000).
        let g = published_capture_gain(&r);
        assert!(g > 0.0 && g <= 0.8 + 1e-6, "{g}");
        r.run(100);
        assert!(published_capture_gain(&r) <= 0.8 + 1e-6);
        r.run(1);
        assert!((published_capture_gain(&r) - 8000.0).abs() < 0.5);
    }

    #[test]
    fn session_notification_while_attaching_is_followed_once_active() {
        let mut r = Rig::new("session-event-attaching");
        r.send(Command::Hello { version: PROTOCOL });
        r.send(set_model("model.onnx", "cpu"));
        r.send(attach());
        r.run(5);
        assert_eq!(r.core.phase(), Phase::Attaching);
        const NEW: &str = "ep1|player|2%b100";
        r.core
            .sessions()
            .add_session(session(NEW, "ep1"), 1.0, false);
        r.core.signals().notify_session_created();
        r.run(40);
        assert_eq!(r.core.phase(), Phase::Active);
        assert_eq!(r.volume(NEW), HELD_VOLUME, "followed without waiting 0.5 s");
    }

    #[test]
    fn default_device_change_mutes_the_capture_until_the_player_is_held_there() {
        let (mut r, _) = Rig::attached("device-change");
        r.run(300);
        assert!((published_capture_gain(&r) - 8000.0).abs() < 0.5);
        r.core.signals().notify_default_changed(Some("ep2".into()));
        r.run(1);
        assert_eq!(published_capture_gain(&r), 0.0, "silent at once");
        r.run(300);
        assert_eq!(
            published_capture_gain(&r),
            0.0,
            "the player has not moved yet (a 0.5 s follow ran meanwhile)"
        );
        // The player recreates its stream on the new device, where Windows starts it at 1.0.
        r.core.sessions().set_active(SESSION, false);
        r.core
            .sessions()
            .add_session(session(SESSION2, "ep2"), 1.0, false);
        r.core.signals().notify_session_created();
        r.run(1);
        assert_eq!(r.volume(SESSION2), HELD_VOLUME);
        assert_eq!(r.audio().st().rebinds, vec![Some("ep2".to_string())]);
        // Back with the conservative gain: original / 1.0 for the window, then 8000.
        let g = published_capture_gain(&r);
        assert!(g > 0.0 && g <= 0.8 + 1e-6, "{g}");
        r.run(100);
        assert!(published_capture_gain(&r) <= 0.8 + 1e-6);
        r.run(1);
        assert!((published_capture_gain(&r) - 8000.0).abs() < 0.5);
    }

    #[test]
    fn default_device_mute_times_out_after_one_second() {
        let (mut r, _) = Rig::attached("device-change-timeout");
        r.run(300);
        r.core.signals().notify_default_changed(Some("ep2".into()));
        r.run(1);
        assert_eq!(published_capture_gain(&r), 0.0);
        // The player stays on its endpoint (e.g. pinned to it): 1 s of silence, then normal.
        r.run(999);
        assert_eq!(published_capture_gain(&r), 0.0);
        r.run(1);
        assert!((published_capture_gain(&r) - 8000.0).abs() < 0.5);
        assert!(r.audio().st().rebinds.is_empty());
    }

    #[test]
    fn notifications_while_idle_are_dropped() {
        let mut r = Rig::new("events-idle");
        r.send(Command::Hello { version: PROTOCOL });
        r.send(set_model("model.onnx", "cpu"));
        r.core.signals().notify_default_changed(Some("ep2".into()));
        r.core.signals().notify_session_created();
        r.run(5);
        r.send(attach());
        r.run(200);
        assert_eq!(r.core.phase(), Phase::Active);
        assert!(
            (published_capture_gain(&r) - 8000.0).abs() < 0.5,
            "a stale device change does not mute a new hold"
        );
    }

    #[test]
    fn reattach_reuses_the_returned_separator() {
        let (mut r, _) = Rig::attached("reuse");
        assert_eq!(r.loads(), 1);
        r.send(Command::Release);
        r.run(100);
        assert!(r.resets() >= 1, "the returned model is reset");
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        assert_eq!(r.loads(), 1, "no second load");
        assert_eq!(r.core.phase(), Phase::Active);
        assert_eq!(r.audio().st().start_had_model, vec![true, true]);
        let ev = r.send(Command::SetMode { devocal: true });
        assert!(errors(&ev).is_empty());
        assert_eq!(r.audio().st().devocal_calls, vec![true]);
    }

    #[test]
    fn reattach_reloads_in_background_when_nothing_came_back() {
        let (mut r, _) = Rig::attached("lost-model");
        r.audio().0.borrow_mut().lose_model_on_stop = true;
        r.send(Command::Release);
        r.run(100);
        r.manual_loads();
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        assert_eq!(r.loads(), 2, "reloaded in the background");
        assert_eq!(
            r.core.phase(),
            Phase::Active,
            "the attach does not wait for it"
        );
        assert_eq!(r.audio().st().start_had_model, vec![true, false]);
        let ev = r.send(Command::SetMode { devocal: true });
        assert!(
            errors(&ev).is_empty(),
            "accepted while the reload is pending"
        );
        assert!(r.audio().st().log.is_empty());
        assert!(r.finish_load(0));
        r.run(1);
        assert_eq!(r.audio().st().log, vec!["separator", "devocal:true"]);
    }

    #[test]
    fn model_load_does_not_block_the_loop() {
        let mut r = Rig::new("async-load");
        r.manual_loads();
        r.send(attach());
        r.run(100);
        let ev = r.send(set_model("model.onnx", "cpu"));
        assert!(errors(&ev).is_empty());
        assert_eq!(r.loads(), 1);
        // The player is turned up while the load takes 1.2 s of (fake) time.
        r.core.sessions().set_volume(SESSION, 0.8).unwrap();
        let during = r.run(1_200);
        assert_eq!(r.volume(SESSION), HELD_VOLUME, "follow ran during the load");
        assert!(!metrics(&during).is_empty(), "metrics kept coming");
        assert_eq!(r.audio().st().separators, 0);
        assert!(r.finish_load(0));
        let ev = r.run(1);
        assert!(errors(&ev).is_empty());
        assert_eq!(r.audio().st().separators, 1);
    }

    #[test]
    fn set_mode_during_pending_load_is_applied_after_load() {
        let mut r = Rig::new("pending-on");
        r.manual_loads();
        let mut ev = r.send(attach());
        ev.extend(r.run(100));
        ev.extend(r.send(set_model("model.onnx", "cpu")));
        let on = r.send(Command::SetMode { devocal: true });
        assert!(errors(&on).is_empty(), "{on:?}");
        ev.extend(on);
        ev.extend(r.run(300));
        assert!(
            r.audio().st().log.is_empty(),
            "nothing sent before the model"
        );
        assert!(r.finish_load(0));
        ev.extend(r.run(10));
        assert_eq!(r.audio().st().log, vec!["separator", "devocal:true"]);
        // Devocal (pending) from the request on.
        assert_eq!(states(&ev), vec![ATTACHING, ACTIVE_PASS, ACTIVE_DEVOCAL]);

        // A failed load drops the pending "on".
        let mut r = Rig::new("pending-on-fail");
        r.manual_loads();
        r.send(attach());
        r.run(100);
        r.send(set_model("bad.onnx", "cpu"));
        r.send(Command::SetMode { devocal: true });
        assert!(r.finish_load(0));
        let ev = r.run(10);
        assert_eq!(errors(&ev), vec![ErrorCode::ModelLoadFailed]);
        assert_eq!(states(&ev), vec![ACTIVE_PASS]);
        assert!(r.audio().st().log.is_empty());
        let ev = r.send(Command::SetMode { devocal: true });
        assert_eq!(errors(&ev), vec![ErrorCode::NoModel]);
    }

    #[test]
    fn newer_set_model_supersedes_a_pending_load() {
        let mut r = Rig::new("supersede");
        r.manual_loads();
        r.send(attach());
        r.run(100);
        r.send(set_model("old.onnx", "cpu"));
        r.send(set_model("new.onnx", "cpu"));
        assert!(!r.finish_load(0), "the older load was abandoned");
        r.run(5);
        assert_eq!(r.audio().st().separators, 0);
        assert!(r.finish_load(1));
        r.run(1);
        assert_eq!(r.audio().st().separators, 1);
    }

    #[test]
    fn commands_after_exit_request_are_rejected() {
        let (mut r, _) = Rig::attached("after-exit");
        r.send(Command::Shutdown);
        let ev = r.send(set_model("model.onnx", "cpu"));
        assert_eq!(errors(&ev), vec![ErrorCode::Protocol]);
        assert_eq!(r.loads(), 1);
        let ev = r.send(Command::SetMode { devocal: true });
        assert_eq!(errors(&ev), vec![ErrorCode::Protocol]);
        assert!(r.audio().st().devocal_calls.is_empty());
    }

    #[test]
    fn attach_ramp_survives_a_slow_handshake() {
        let mut r = Rig::new("slow-handshake");
        let clock = Rc::new(Cell::new(T0));
        {
            let mut s = r.audio().0.borrow_mut();
            s.clock = Some(clock.clone());
            s.start_delay_us = 500 * MS; // the audio start blocks for 500 ms
        }
        // Every volume write with the (real) time it happened.
        let writes: Rc<RefCell<Vec<(u64, f32)>>> = Rc::default();
        {
            let (w, c) = (writes.clone(), clock.clone());
            r.core
                .sessions()
                .on_set_volume(Box::new(move |_, v| w.borrow_mut().push((c.get(), v))));
        }
        let original = 0.8_f32;
        // Highest volume in effect during [from, to]: the loopback still carries audio made
        // at these volumes.
        let max_volume = |from: u64, to: u64| -> f32 {
            let w = writes.borrow();
            let at_from = w
                .iter()
                .rev()
                .find(|(t, _)| *t <= from)
                .map_or(original, |&(_, v)| v);
            w.iter()
                .filter(|(t, _)| *t > from && *t <= to)
                .map(|&(_, v)| v)
                .fold(at_from, f32::max)
        };
        let handshake = [
            Command::Hello { version: PROTOCOL },
            set_model("model.onnx", "cpu"),
            attach(),
        ];
        let mut st = LoopState::default();
        let mut out = Vec::new();
        let mut attach_start = None;
        for pass in 0..140 {
            let input = LoopInput {
                lines: if pass == 0 {
                    handshake.iter().map(|c| encode(c).unwrap()).collect()
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            let c = clock.clone();
            assert!(!loop_once(
                &mut r.core,
                &mut st,
                input,
                &mut move || c.get(),
                &mut out
            ));
            let now = clock.get();
            if pass == 0 {
                attach_start = Some(now);
            }
            if let Some(g) = r.audio().st().gains.as_ref() {
                let gain = g.capture_gain();
                let loudest = max_volume(now.saturating_sub(20 * MS), now);
                assert!(
                    gain * loudest <= original * 1.001,
                    "pass {pass}: gain {gain} with volume {loudest} in the last 20 ms"
                );
            }
            clock.set(now + MS);
        }
        let t1 = attach_start.unwrap();
        assert!(t1 >= T0 + 500 * MS);
        let times: Vec<u64> = writes.borrow().iter().map(|w| w.0).collect();
        assert_eq!(
            times,
            vec![t1, t1 + 10 * MS, t1 + 20 * MS, t1 + 30 * MS],
            "four ramp steps 10 ms apart"
        );
        assert_eq!(r.volume(SESSION), HELD_VOLUME);
        assert_eq!(r.core.phase(), Phase::Active);
        assert!(errors(&out).is_empty(), "{out:?}");
    }

    #[test]
    fn attach_while_active_is_rejected() {
        let (mut r, _) = Rig::attached("double-attach");
        let ev = r.send(attach());
        assert_eq!(errors(&ev), vec![ErrorCode::AttachFailed]);
        assert_eq!(r.core.phase(), Phase::Active);
        assert_eq!(r.audio().st().starts.len(), 1);
    }

    #[test]
    fn stale_fallback_stage_does_not_hide_a_user_toggle() {
        let (mut r, _) = Rig::attached("settle");
        r.send(Command::SetMode { devocal: true });
        r.run(10);
        r.audio()
            .set_stage(Stage::Fallback, Some(FallbackReason::Overload));
        let mut ev = r.run(10);
        // The processing thread applies the toggles later than they are sent.
        r.audio().set_lazy_stage();
        ev.extend(r.send(Command::SetMode { devocal: false }));
        ev.extend(r.run(5));
        ev.extend(r.send(Command::SetMode { devocal: true }));
        ev.extend(r.run(5));
        r.audio().set_stage(Stage::WarmingUp, None);
        ev.extend(r.run(200));
        let fallback = (
            Phase::Active,
            Some(Mode::Fallback),
            Some(FallbackReason::Overload),
        );
        assert_eq!(states(&ev), vec![fallback, ACTIVE_PASS, ACTIVE_DEVOCAL]);

        // A toggle the processor never acts on is reported truthfully after the settle time.
        r.audio()
            .set_stage(Stage::Fallback, Some(FallbackReason::Overload));
        let mut ev = r.run(10);
        ev.extend(r.send(Command::SetMode { devocal: false }));
        ev.extend(r.send(Command::SetMode { devocal: true }));
        ev.extend(r.run(200));
        assert_eq!(
            states(&ev),
            vec![fallback, ACTIVE_PASS, ACTIVE_DEVOCAL, fallback]
        );
    }

    #[test]
    fn watchdog_exit_while_audio_starts_releases_without_lowering() {
        let mut r = Rig::new("watchdog-early");
        let mut ev = r.send(attach());
        // The pipe closed before the first tick: begin_attach never runs.
        ev.extend(r.core.request_exit(r.now));
        ev.extend(r.run(100));
        assert_eq!(states(&ev), vec![ATTACHING, RELEASING, IDLE]);
        assert_eq!(r.core.sessions().set_volume_calls(), 0);
        assert!(r.core.exit_ready());
        assert!(!r.restore.exists());
    }

    #[test]
    fn overridden_sessions_are_counted_in_metrics() {
        let (mut r, _) = Rig::attached("overridden");
        r.core.sessions().set_volume(SESSION, 0.5).unwrap();
        let m = metrics(&r.run(1_000));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].session_overridden, 1);
        assert_eq!(r.volume(SESSION), HELD_VOLUME);
    }

    #[test]
    fn args_parse() {
        let args = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
        assert_eq!(
            args(&["--app-pid", "42", "--restore-file", r"C:\x\r.json"]),
            Ok(Args {
                app_pid: 42,
                restore_file: PathBuf::from(r"C:\x\r.json"),
            })
        );
        assert!(args(&["--app-pid", "42"]).is_err());
        assert!(args(&["--restore-file", "r.json"]).is_err());
        assert!(args(&["--app-pid", "-1", "--restore-file", "r.json"]).is_err());
        assert!(args(&["--app-pid"]).is_err());
        assert!(args(&["--verbose"]).is_err());
    }

    #[test]
    fn requests_while_releasing_wait_for_the_next_attach() {
        let (mut r, _) = Rig::attached("releasing");
        r.send(Command::Release);
        r.run(5);
        assert_eq!(r.core.phase(), Phase::Releasing);
        r.send(set_model("model2.onnx", "cpu"));
        r.send(Command::SetMode { devocal: true });
        assert_eq!(
            r.audio().st().separators,
            0,
            "not sent to the stopping audio"
        );
        assert!(r.audio().st().devocal_calls.is_empty());
        r.run(100);
        r.send(attach());
        r.run(100);
        assert_eq!(r.loads(), 2, "the staged model is used, not reloaded");
        assert_eq!(r.audio().st().devocal_calls, vec![true]);
    }
}
