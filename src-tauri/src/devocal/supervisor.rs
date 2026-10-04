//! App-side supervisor of `devocal-engine.exe`.
//!
//! The [`Supervisor`] keeps what the user asked for (hold the player, devocal on or off), and
//! drives one engine process towards it over an [`EngineLink`]. It does no I/O of its own:
//! the engine process, the restore and the clock are injected, so tests run with a fake link.
//! The app calls [`Supervisor::tick`] every 100 ms with the current player.
//!
//! Rules (caller obligations 1, 9, 16–26 and the controller rulings of task 10):
//! - The engine's phase is taken only from `State` events. An `attach` is sent only while the
//!   last State is `Idle` (a fresh engine starts `Idle`), so a player change sends `release`
//!   and waits for `Idle` first.
//! - `set_mode(true)` is sent only for a user `enable`, and to a freshly started engine
//!   (restart) while the user still wants devocal; never on state sync. The engine ignores a
//!   repeated "on" (obligation 1), so a user `enable` while the engine reports `Fallback` is
//!   sent as `set_mode(false)` then `set_mode(true)`: that is the user's retry.
//! - `set_model` (the selected model, device and manifest window geometry) is sent to a new
//!   engine, when the model, its file or the device changes, once per user
//!   action after `Error{ModelLoadFailed}` (followed by `set_mode(true)` if still wanted), and
//!   on the next user `enable` after the engine gave up on the model (a second
//!   `ModelLoadFailed`, `NoModel` or `GpuRequired`). When a GPU-only model falls behind
//!   (`device_note: gpu_overloaded`) the app sends StemgenRT and keeps that, across engine
//!   restarts, until the selection changes.
//! - `Error{Protocol}` means a version mismatch only during the hello exchange (from the
//!   spawn until the first State or Metrics); the engine is then shut down (killed after
//!   2 s) and not restarted.
//! - After every engine exit (any exit code): gate drops, `restore` runs, gate back to 1.0;
//!   then, if the hold is still wanted, a restart within the [`RestartBudget`] (phase
//!   `restarting`), otherwise phase `failed` with `engine_crashed`.
//! - While nothing is held (no engine, or last State `Idle` with no attach in flight) and the
//!   restore file exists, `restore` runs every 2 s, or every 200 ms while the last restore
//!   kept entries awaiting the player (a player relaunched at the persisted held volume is
//!   fixed within about 200 ms instead of staying silent for up to 2 s). The fast cadence
//!   lasts at most 60 s without progress (each restore enumerates the sessions once), then
//!   falls back to 2 s; a different restore outcome, an entry restored or removed, or a hold
//!   or engine exit (which may have changed the awaiting entries) starts it again. At most
//!   one restore per tick.
//! - Attenuation gate: drop while attaching/releasing and around an exit; `1/attenuation`
//!   only from Metrics received while the last State is `Active` (a fresh Metrics after
//!   entering Active, so a stale value from before the hold is never used), set again only
//!   when `attenuationEpoch` changes; 1.0 on `Idle` and after a restore.
//! - No hang detection from missing Metrics (a model load may block events; obligation 23).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use devocal_core::protocol::{
    encode, Command, Device, DeviceNote, ErrorCode, Event, FallbackReason, Metrics, Mode, Phase,
    WindowedSpec, PROTOCOL,
};
use devocal_core::restore::RestoreOutcome;
use serde::Serialize;

use super::gate::AttenuationGate;
use super::link::EngineLink;
use super::model::manifest::{bundled, ModelKind, ModelSpec, STEMGENRT_ID};

/// What the user picked in the settings: a manifest model id and `auto` | `cpu` | `gpu`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub model_id: String,
    pub device: String,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            model_id: STEMGENRT_ID.into(),
            device: "auto".into(),
        }
    }
}

/// The `set_model` the engine has been sent: which model, from where, on which device.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SentModel {
    id: String,
    path: PathBuf,
    device: String,
}

/// The window geometry the engine needs for a windowed model; `None` for a streaming one.
/// Window and lookahead are the same for both devices (the manifest validation checks it).
fn windowed_spec(model: &ModelSpec) -> Option<WindowedSpec> {
    if model.kind != ModelKind::Windowed {
        return None;
    }
    let (cpu, gpu) = (model.devices.cpu.as_ref(), model.devices.gpu.as_ref());
    let any = gpu.or(cpu)?;
    Some(WindowedSpec {
        window_ms: any.window_ms,
        lookahead_ms: any.lookahead_ms,
        gpu_hop_ms: gpu.map(|d| d.hop_ms),
        cpu_hop_ms: cpu.map(|d| d.hop_ms),
        cpu_threads: cpu.map_or(0, |d| d.threads),
        vocals_index: model.vocals_index,
    })
}

/// Restore retry interval while nothing is held (obligation 9).
pub const RESTORE_RETRY_MS: u64 = 2_000;
/// Restore retry interval while nothing is held and the restore file has entries awaiting the
/// player (it may be relaunched at the persisted held volume any moment).
pub const AWAITING_RESTORE_RETRY_MS: u64 = 200;
/// The fast cadence falls back to [`RESTORE_RETRY_MS`] after this long without progress.
pub const AWAITING_RESTORE_FAST_FOR_MS: u64 = 60_000;
/// Wait before re-attaching the same player after the engine went idle on its own.
pub const ATTACH_RETRY_MS: u64 = 3_000;
/// Attaches to one player (pid + creation time) before giving up (`failed`, `attach_failed`).
pub const MAX_ATTACH_ATTEMPTS: u32 = 3;
/// `session_overridden` stays true this long after the engine's counter increased.
pub const OVERRIDE_WINDOW_MS: u64 = 10_000;
/// `input_silent` once the engine has seen no input for this long while media plays.
pub const INPUT_SILENT_MS: u64 = 3_000;
/// A version-mismatched engine that has not exited after `shutdown` is killed after this.
pub const MISMATCH_KILL_MS: u64 = 2_000;
/// An engine that has not reported `Idle` this long after the app sent `release` is killed
/// (then restored and, if the hold is still wanted, restarted within the budget).
pub const RELEASE_DEADLINE_MS: u64 = 20_000;
/// How long [`Supervisor::shutdown`] waits for the engine to exit before killing it.
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

/// The player the engine should hold: pid plus process creation time (FILETIME).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerProcess {
    pub source_id: String,
    pub pid: u32,
    pub created_at: u64,
}

impl PlayerProcess {
    fn key(&self) -> (u32, u64) {
        (self.pid, self.created_at)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevocalStatus {
    /// off | attaching | passthrough | devocal | fallback | releasing | restarting | failed |
    /// unavailable
    pub phase: &'static str,
    pub held: bool,
    pub latency_ms: Option<f32>,
    pub load_ratio: Option<f32>,
    /// overload | model_error
    pub fallback_reason: Option<&'static str>,
    pub session_overridden: bool,
    pub input_silent: bool,
    /// `attaching`/`restarting` only because there is no player to hold yet (nothing
    /// playing). A separate flag, not a phase, so a status reader that does not know it still
    /// sees `attaching`.
    pub waiting_for_player: bool,
    pub error: Option<String>,
    /// The model the engine is told to run: the selected one, or StemgenRT after a GPU-only
    /// model fell behind (the selection itself is unchanged then). `None` before the first status.
    pub model_id: Option<String>,
    /// cpu | gpu: what the loaded model runs on (from the engine's Metrics while active).
    pub device: Option<&'static str>,
    /// gpu_unavailable | gpu_check_failed | gpu_overloaded: why it is not on the GPU.
    pub device_note: Option<&'static str>,
}

impl DevocalStatus {
    pub fn off() -> Self {
        Self {
            phase: "off",
            held: false,
            latency_ms: None,
            load_ratio: None,
            fallback_reason: None,
            session_overridden: false,
            input_silent: false,
            waiting_for_player: false,
            error: None,
            model_id: None,
            device: None,
            device_note: None,
        }
    }
}

/// At most `max` restarts within any `window_ms`.
pub struct RestartBudget {
    max: usize,
    window_ms: u64,
    restarts: VecDeque<u64>,
}

impl Default for RestartBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl RestartBudget {
    /// 3 restarts per 60 s.
    pub fn new() -> Self {
        Self {
            max: 3,
            window_ms: 60_000,
            restarts: VecDeque::new(),
        }
    }

    /// Records a restart at `now_ms` and returns true if the budget allows it.
    pub fn allow(&mut self, now_ms: u64) -> bool {
        while let Some(&t) = self.restarts.front() {
            if now_ms.saturating_sub(t) >= self.window_ms {
                self.restarts.pop_front();
            } else {
                break;
            }
        }
        if self.restarts.len() < self.max {
            self.restarts.push_back(now_ms);
            true
        } else {
            false
        }
    }
}

/// What the gate was last set to, so it is set (and its epoch bumped) only on a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateKey {
    Unity,
    Drop,
    Scaled { link: u64, epoch: u64 },
}

struct EngineState {
    phase: Phase,
    mode: Option<Mode>,
    fallback_reason: Option<FallbackReason>,
}

/// One running engine process and what we know about it.
struct Linked<L> {
    link: L,
    generation: u64,
    /// Last State event; `None` until the first one (a fresh engine is idle).
    state: Option<EngineState>,
    /// From the spawn until the first State or Metrics.
    hello_exchange: bool,
    /// The player of the last `attach`, until the engine reports Idle.
    attached: Option<PlayerProcess>,
    /// `attach` sent, no State (or `AttachFailed`) seen for it yet.
    attach_in_flight: bool,
    /// `release` sent, Idle not seen yet.
    release_sent: bool,
    /// When the app sent that `release` (deadline [`RELEASE_DEADLINE_MS`]).
    release_sent_at: Option<u64>,
    sent_model: Option<SentModel>,
    /// The GPU-overload fallback was already looked at for this engine (once is enough).
    overload_handled: bool,
    /// The engine has no usable model: it gave up loading `sent_model` (a `ModelLoadFailed`
    /// with no retry left) or answered `NoModel`. The next user `enable` sends `set_model`.
    model_failed: bool,
    /// Last Metrics received while Active (cleared when leaving Active).
    metrics: Option<Metrics>,
    /// Baseline of the engine's `session_overridden` counter.
    overridden_seen: Option<u64>,
    /// Version mismatch detected at this time; the engine is being shut down.
    mismatch_at: Option<u64>,
}

impl<L> Linked<L> {
    fn new(link: L, generation: u64) -> Self {
        Self {
            link,
            generation,
            state: None,
            hello_exchange: true,
            attached: None,
            attach_in_flight: false,
            release_sent: false,
            release_sent_at: None,
            sent_model: None,
            overload_handled: false,
            model_failed: false,
            metrics: None,
            overridden_seen: None,
            mismatch_at: None,
        }
    }

    fn phase(&self) -> Phase {
        self.state.as_ref().map_or(Phase::Idle, |s| s.phase)
    }

    /// The last State says the engine fell back to the original sound while devocal was on.
    fn in_fallback(&self) -> bool {
        !self.release_sent
            && self
                .state
                .as_ref()
                .is_some_and(|s| s.phase == Phase::Active && s.mode == Some(Mode::Fallback))
    }

    /// Sessions may be held, or are about to be: restore must not run.
    fn holding(&self) -> bool {
        self.attach_in_flight
            || matches!(
                self.phase(),
                Phase::Attaching | Phase::Active | Phase::Releasing
            )
    }
}

/// Runs the restore file; `None` when there was no file.
pub type RestoreFn = Box<dyn FnMut() -> Option<RestoreOutcome> + Send>;

pub struct Supervisor<L: EngineLink> {
    spawn: Box<dyn FnMut() -> Result<L, String> + Send>,
    restore: RestoreFn,
    restore_pending: Box<dyn Fn() -> bool + Send>,
    gate: Arc<AttenuationGate>,
    gate_key: GateKey,
    budget: RestartBudget,
    shutdown_wait: Duration,
    engine: Option<Linked<L>>,
    generation: u64,

    // What the user asked for.
    want_hold: bool,
    want_devocal: bool,
    model: Option<PathBuf>,
    /// The model and device the user picked; `model` is that model's file.
    selection: Selection,
    /// StemgenRT, loaded because a GPU-only model fell behind on the GPU. Kept until the
    /// selection changes: the engine's GPU ban does not survive an engine restart.
    fallback: Option<SentModel>,
    /// Where StemgenRT is (verified install, or the env override).
    stemgenrt_path: Box<dyn Fn() -> Option<PathBuf> + Send>,
    /// One `ModelLoadFailed` retry left for the current user action (ruling 5).
    model_retry: bool,
    /// Send `set_mode(true)` once the (new) engine has its handshake.
    user_on_pending: bool,

    // Conditions.
    restarting: bool,
    failed: bool,
    unavailable: bool,
    error: Option<String>,
    shut_down: bool,

    // Re-attach bookkeeping for one player.
    failure_target: Option<(u32, u64)>,
    attach_failures: u32,
    retry_after_ms: u64,
    attach_gave_up: bool,

    last_restore_ms: Option<u64>,
    /// The last restore kept entries awaiting the player: retry every 200 ms...
    restore_awaiting: bool,
    /// ...for 60 s from this time (the last progress or change).
    fast_restore_since: Option<u64>,
    /// The outcome of the last restore (a different one restarts the fast cadence).
    last_restore_outcome: Option<RestoreOutcome>,
    /// Something may have changed the awaiting entries (a hold, an engine exit): the next
    /// restore restarts the fast cadence.
    restore_reset: bool,
    overridden_at: Option<u64>,
    now_ms: u64,
    media_playing: bool,
    /// The last tick had a player to hold.
    has_target: bool,
}

impl<L: EngineLink> Supervisor<L> {
    /// `spawn` starts an engine and connects to it; `restore` runs the restore file (it must
    /// not panic and is a no-op returning `None` without a file).
    pub fn new(
        spawn: Box<dyn FnMut() -> Result<L, String> + Send>,
        restore: RestoreFn,
        gate: Arc<AttenuationGate>,
    ) -> Self {
        Self {
            spawn,
            restore,
            restore_pending: Box::new(|| true),
            gate,
            gate_key: GateKey::Unity,
            budget: RestartBudget::new(),
            shutdown_wait: SHUTDOWN_WAIT,
            engine: None,
            generation: 0,
            want_hold: false,
            want_devocal: false,
            model: None,
            selection: Selection::default(),
            fallback: None,
            stemgenrt_path: Box::new(|| None),
            model_retry: false,
            user_on_pending: false,
            restarting: false,
            failed: false,
            unavailable: false,
            error: None,
            shut_down: false,
            failure_target: None,
            attach_failures: 0,
            retry_after_ms: 0,
            attach_gave_up: false,
            last_restore_ms: None,
            restore_awaiting: false,
            fast_restore_since: None,
            last_restore_outcome: None,
            restore_reset: false,
            overridden_at: None,
            now_ms: 0,
            media_playing: false,
            has_target: false,
        }
    }

    /// Whether the restore file exists (checked before the periodic retry). Defaults to
    /// "always", which only costs a no-op restore every 2 s.
    pub fn with_restore_pending(mut self, pending: Box<dyn Fn() -> bool + Send>) -> Self {
        self.restore_pending = pending;
        self
    }

    pub fn with_shutdown_wait(mut self, wait: Duration) -> Self {
        self.shutdown_wait = wait;
        self
    }

    /// Where to find StemgenRT for the switch after a GPU-only model fell behind. Without it
    /// that switch reports `model_not_found:stemgenrt-hop128` and leaves the engine as it is.
    pub fn with_stemgenrt_path(mut self, path: Box<dyn Fn() -> Option<PathBuf> + Send>) -> Self {
        self.stemgenrt_path = path;
        self
    }

    /// User action: hold the player and turn devocal on with the selected model (`model` is its
    /// file). `None` (no such model installed) only reports `unavailable`.
    pub fn enable(&mut self, model: Option<PathBuf>, selection: Selection) {
        if self.shut_down {
            return;
        }
        if selection != self.selection {
            // A new choice ends the GPU-overload fallback, and an old file is not this model's.
            self.selection = selection;
            self.fallback = None;
            self.model = None;
        }
        self.error = None;
        self.failed = false;
        self.unavailable = false;
        self.failure_target = None;
        self.attach_failures = 0;
        self.attach_gave_up = false;
        let Some(model) = model else {
            self.unavailable = true;
            self.error = Some(format!("model_not_found:{}", self.selection.model_id));
            // With no engine running, a wish left from an earlier enable must not survive this
            // failed one: otherwise a later install (which clears `unavailable`) would spawn an
            // engine, take over the player and turn devocal on with nobody asking. A running
            // engine is left as it is; it already holds the player.
            if self.engine.is_none() {
                self.want_hold = false;
                self.want_devocal = false;
                self.user_on_pending = false;
                self.model_retry = false;
                self.restarting = false;
            }
            return;
        };
        self.want_hold = true;
        self.want_devocal = true;
        self.model_retry = true;
        self.model = Some(model);
        let wanted = self.wanted();
        let live = self
            .engine
            .as_ref()
            .filter(|e| e.mismatch_at.is_none())
            .map(|e| (e.sent_model != wanted || e.model_failed, e.in_fallback()));
        match live {
            Some((reload, fallback)) => {
                self.user_on_pending = false;
                if reload && !self.send_set_model() {
                    return;
                }
                // The engine still has devocal "on" after a fallback and ignores a repeated
                // "on" (obligation 1): off-then-on is the user's retry. Only in Fallback: in
                // Devocal the "off" would start a fade-out, and in Passthrough the toggle is
                // already off so a plain "on" is forwarded.
                if fallback && !self.send(&Command::SetMode { devocal: false }) {
                    return;
                }
                self.send(&Command::SetMode { devocal: true });
            }
            // The next tick spawns the engine and sends the whole handshake.
            None => self.user_on_pending = true,
        }
    }

    /// A model was installed (downloaded or imported): an `unavailable` caused only by the missing
    /// model no longer applies. Does not enable devocal; any other condition is left as it is.
    pub fn model_installed(&mut self) {
        if self.unavailable
            && self
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("model_not_found:"))
        {
            self.unavailable = false;
            self.error = None;
        }
    }

    /// User action: devocal off; the player stays held (passthrough).
    pub fn disable(&mut self) {
        if self.shut_down {
            return;
        }
        self.want_devocal = false;
        self.user_on_pending = false;
        self.model_retry = false;
        if self.engine.is_some() {
            self.send(&Command::SetMode { devocal: false });
        }
    }

    /// User action: give the player back.
    pub fn release(&mut self) {
        if self.shut_down {
            return;
        }
        self.want_hold = false;
        self.want_devocal = false;
        self.user_on_pending = false;
        self.model_retry = false;
        self.failed = false;
        self.restarting = false;
        self.attach_gave_up = false;
        self.error = None;
        self.send_release();
    }

    pub fn tick(&mut self, now_ms: u64, target: Option<PlayerProcess>, media_playing: bool) {
        if self.shut_down {
            return;
        }
        self.now_ms = now_ms;
        self.media_playing = media_playing;
        self.has_target = target.is_some();
        self.pump(now_ms);
        self.check_exit(now_ms);
        self.ensure_engine(target.as_ref());
        self.drive_attach(now_ms, target.as_ref());
        self.flush_user_on();
        self.retry_restore(now_ms);
    }

    pub fn status(&self) -> DevocalStatus {
        let e = self.engine.as_ref();
        let active_metrics = e
            .filter(|e| e.phase() == Phase::Active && !e.release_sent)
            .and_then(|e| e.metrics.as_ref());
        let phase = if self.unavailable {
            "unavailable"
        } else if self.failed || self.attach_gave_up {
            "failed"
        } else if self.restarting {
            "restarting"
        } else {
            match e {
                None if self.want_hold && self.want_devocal => "attaching",
                None => "off",
                Some(e) if e.release_sent => "releasing",
                Some(e) => match e.phase() {
                    Phase::Idle if e.attach_in_flight || (self.want_hold && self.want_devocal) => {
                        "attaching"
                    }
                    Phase::Idle => "off",
                    Phase::Attaching => "attaching",
                    Phase::Active => match e.state.as_ref().and_then(|s| s.mode) {
                        Some(Mode::Devocal) => "devocal",
                        Some(Mode::Fallback) => "fallback",
                        Some(Mode::Passthrough) | None => "passthrough",
                    },
                    Phase::Releasing => "releasing",
                },
            }
        };
        let held = self.want_hold
            && e.is_some_and(|e| {
                !e.release_sent && matches!(e.phase(), Phase::Attaching | Phase::Active)
            });
        let fallback_reason = e
            .filter(|e| e.phase() == Phase::Active)
            .and_then(|e| e.state.as_ref())
            .and_then(|s| s.fallback_reason)
            .map(|r| match r {
                FallbackReason::Overload => "overload",
                FallbackReason::ModelError => "model_error",
            });
        // Nothing to attach to (no engine is spawned without a target, and an idle engine
        // is sent no `attach`): say so instead of "taking over the player".
        let attach_pending =
            e.is_some_and(|e| e.attach_in_flight || e.phase() != Phase::Idle || e.release_sent);
        let waiting_for_player = matches!(phase, "attaching" | "restarting")
            && self.want_hold
            && !self.has_target
            && !attach_pending;
        DevocalStatus {
            phase,
            held,
            latency_ms: active_metrics.map(|m| m.latency_ms),
            load_ratio: active_metrics.map(|m| m.load_ratio),
            fallback_reason,
            session_overridden: self
                .overridden_at
                .is_some_and(|t| self.now_ms.saturating_sub(t) < OVERRIDE_WINDOW_MS),
            input_silent: self.media_playing
                && active_metrics.is_some_and(|m| m.input_silent_ms >= INPUT_SILENT_MS),
            waiting_for_player,
            error: self.error.clone(),
            model_id: Some(self.wanted_id().to_string()),
            device: active_metrics.and_then(|m| m.device).map(|d| match d {
                Device::Cpu => "cpu",
                Device::Gpu => "gpu",
            }),
            // After the switch to StemgenRT the engine no longer carries the note; keep the reason.
            device_note: active_metrics
                .and_then(|m| m.device_note)
                .map(|n| match n {
                    DeviceNote::GpuUnavailable => "gpu_unavailable",
                    DeviceNote::GpuCheckFailed => "gpu_check_failed",
                    DeviceNote::GpuOverloaded => "gpu_overloaded",
                })
                .or(self.fallback.as_ref().map(|_| "gpu_overloaded")),
        }
    }

    /// App exit: `shutdown`, wait up to 1 s, kill if still running, then restore. Every later
    /// call is a no-op.
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        self.want_hold = false;
        self.want_devocal = false;
        match self.engine.take() {
            Some(mut e) => {
                if encode(&Command::Shutdown).is_ok() {
                    let _ = e.link.send(&Command::Shutdown);
                }
                let deadline = Instant::now() + self.shutdown_wait;
                while !e.link.exited() {
                    if Instant::now() >= deadline {
                        e.link.kill();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                self.apply_gate(GateKey::Drop, None);
                self.run_restore(self.now_ms);
                self.apply_gate(GateKey::Unity, Some(1.0));
            }
            None => {
                if (self.restore_pending)() {
                    self.run_restore(self.now_ms);
                }
            }
        }
    }

    // ---- tick steps ----

    fn pump(&mut self, now_ms: u64) {
        loop {
            let Some(e) = self.engine.as_mut() else {
                return;
            };
            let Some(ev) = e.link.try_recv() else {
                return;
            };
            match ev {
                Event::State {
                    phase,
                    mode,
                    fallback_reason,
                    ..
                } => self.on_state(phase, mode, fallback_reason, now_ms),
                Event::Metrics(m) => self.on_metrics(m, now_ms),
                Event::Error { code, message } => self.on_error(code, message, now_ms),
            }
        }
    }

    fn on_state(
        &mut self,
        phase: Phase,
        mode: Option<Mode>,
        fallback_reason: Option<FallbackReason>,
        now_ms: u64,
    ) {
        let Some(e) = self.engine.as_mut() else {
            return;
        };
        e.hello_exchange = false;
        let was_active = e.phase() == Phase::Active;
        e.state = Some(EngineState {
            phase,
            mode,
            fallback_reason,
        });
        if phase != Phase::Active || !was_active {
            e.metrics = None;
        }
        match phase {
            Phase::Idle => {
                e.attach_in_flight = false;
                let requested = std::mem::replace(&mut e.release_sent, false);
                e.release_sent_at = None;
                let lost = e.attached.take();
                self.apply_gate(GateKey::Unity, Some(1.0));
                if let (false, Some(p)) = (requested, lost) {
                    // The engine let go on its own (player gone, capture failed, ...).
                    self.note_attach_failure(&p, now_ms);
                }
            }
            Phase::Attaching => {
                e.attach_in_flight = false;
                self.apply_gate(GateKey::Drop, None);
            }
            Phase::Active => {
                e.attach_in_flight = false;
                self.restarting = false;
                if !was_active {
                    // Wait for a Metrics received while Active.
                    self.apply_gate(GateKey::Drop, None);
                }
            }
            Phase::Releasing => self.apply_gate(GateKey::Drop, None),
        }
    }

    fn on_metrics(&mut self, m: Metrics, now_ms: u64) {
        let Some(e) = self.engine.as_mut() else {
            return;
        };
        e.hello_exchange = false;
        let increased = e
            .overridden_seen
            .is_some_and(|seen| m.session_overridden > seen);
        e.overridden_seen = Some(m.session_overridden);
        if increased {
            self.overridden_at = Some(now_ms);
        }
        let Some(e) = self.engine.as_mut() else {
            return;
        };
        if e.phase() != Phase::Active || e.release_sent {
            // Obligation 22; and Metrics the engine sent before it handled our `release`
            // must not reopen the gate.
            return;
        }
        let key = GateKey::Scaled {
            link: e.generation,
            epoch: m.attenuation_epoch,
        };
        let gain = (m.attenuation.is_finite() && m.attenuation > 0.0).then(|| 1.0 / m.attenuation);
        let overloaded = m.device_note == Some(DeviceNote::GpuOverloaded);
        e.metrics = Some(m);
        match gain {
            Some(g) => self.apply_gate(key, Some(g)),
            None => self.apply_gate(GateKey::Drop, None),
        }
        if overloaded {
            self.on_gpu_overloaded();
        }
    }

    fn on_error(&mut self, code: ErrorCode, message: String, now_ms: u64) {
        match code {
            ErrorCode::Protocol => {
                let in_hello = self
                    .engine
                    .as_ref()
                    .is_some_and(|e| e.hello_exchange && e.mismatch_at.is_none());
                if !in_hello {
                    // Also sent for commands after an exit request (obligation 26).
                    eprintln!("devocal: engine protocol error: {message}");
                    return;
                }
                self.failed = true;
                self.restarting = false;
                self.error = Some(format!("engine_version_mismatch: {message}"));
                self.send(&Command::Shutdown);
                if let Some(e) = self.engine.as_mut() {
                    e.mismatch_at = Some(now_ms);
                }
            }
            ErrorCode::ModelLoadFailed => {
                if self.model_retry && self.model.is_some() {
                    self.model_retry = false;
                    if self.send_set_model() && self.want_devocal {
                        self.send(&Command::SetMode { devocal: true });
                    }
                } else {
                    self.error = Some(format!("model_load_failed: {message}"));
                    if let Some(e) = self.engine.as_mut() {
                        e.model_failed = true;
                    }
                }
            }
            ErrorCode::AttachFailed => {
                self.error = Some(format!("attach_failed: {message}"));
                let Some(e) = self.engine.as_mut() else {
                    return;
                };
                // Phase stays as the last State said; but an attach that was rejected or
                // could not start leaves the engine Idle without any State.
                if e.attach_in_flight && e.phase() == Phase::Idle {
                    e.attach_in_flight = false;
                    if let Some(p) = e.attached.take() {
                        self.note_attach_failure(&p, now_ms);
                    }
                }
            }
            ErrorCode::CaptureFailed => self.error = Some(format!("capture_failed: {message}")),
            ErrorCode::RenderFailed => self.error = Some(format!("render_failed: {message}")),
            ErrorCode::NoModel => {
                self.error = Some(format!("no_model: {message}"));
                if let Some(e) = self.engine.as_mut() {
                    e.model_failed = true;
                }
            }
            ErrorCode::BadDevice => self.error = Some(format!("bad_device: {message}")),
            // A GPU-only model on a machine or choice without a usable GPU: say so, keep the
            // original sound, never swap models behind the user's back. No retry (it would fail
            // the same way); the user's next enable sends the model again.
            ErrorCode::GpuRequired => {
                self.error = Some(format!("gpu_required: {message}"));
                if let Some(e) = self.engine.as_mut() {
                    e.model_failed = true;
                }
            }
        }
    }

    fn check_exit(&mut self, now_ms: u64) {
        let exited = match self.engine.as_mut() {
            None => return,
            Some(e) => {
                let mismatch_due = e
                    .mismatch_at
                    .is_some_and(|t| now_ms.saturating_sub(t) >= MISMATCH_KILL_MS);
                // A release that never reaches Idle (engine stalled with the pipe open).
                let release_due = e
                    .release_sent_at
                    .is_some_and(|t| now_ms.saturating_sub(t) >= RELEASE_DEADLINE_MS);
                if !e.link.exited() && (mismatch_due || release_due) {
                    if release_due {
                        eprintln!("devocal: engine did not finish releasing in 20 s; killing it");
                    }
                    e.link.kill();
                }
                e.link.exited()
            }
        };
        if !exited {
            return;
        }
        // Whatever the engine said last (it may have released before exiting).
        self.pump(now_ms);
        self.engine = None;
        self.apply_gate(GateKey::Drop, None);
        self.restore_reset = true;
        self.run_restore(now_ms);
        self.last_restore_ms = Some(now_ms);
        self.apply_gate(GateKey::Unity, Some(1.0));
        if self.failed || !self.want_hold {
            self.restarting = false;
        } else if self.budget.allow(now_ms) {
            self.restarting = true;
        } else {
            self.restarting = false;
            self.failed = true;
            self.error = Some("engine_crashed".into());
        }
    }

    fn ensure_engine(&mut self, target: Option<&PlayerProcess>) {
        if self.engine.is_some()
            || !self.want_hold
            || self.failed
            || self.unavailable
            || target.is_none()
        {
            return;
        }
        match (self.spawn)() {
            Err(e) => {
                self.unavailable = true;
                self.restarting = false;
                self.error = Some(format!("engine_unavailable: {e}"));
            }
            Ok(link) => {
                self.generation += 1;
                self.engine = Some(Linked::new(link, self.generation));
                if !self.send(&Command::Hello { version: PROTOCOL }) {
                    return;
                }
                if self.model.is_some() {
                    self.send_set_model();
                }
                // A new engine starts with devocal off: give it the user's standing choice.
                self.user_on_pending = self.want_devocal;
            }
        }
    }

    fn drive_attach(&mut self, now_ms: u64, target: Option<&PlayerProcess>) {
        if !self.want_hold || self.failed {
            return;
        }
        let Some(t) = target else {
            return;
        };
        let Some(e) = self.engine.as_ref() else {
            return;
        };
        if e.mismatch_at.is_some() || e.release_sent {
            return;
        }
        let phase = e.phase();
        if phase == Phase::Idle && !e.attach_in_flight {
            if self.failure_target == Some(t.key()) {
                if self.attach_failures >= MAX_ATTACH_ATTEMPTS {
                    if !self.attach_gave_up {
                        self.attach_gave_up = true;
                        self.error = Some("attach_failed".into());
                    }
                    return;
                }
                if now_ms < self.retry_after_ms {
                    return;
                }
            }
            if self.attach_gave_up {
                self.attach_gave_up = false;
                self.error = None;
            }
            let cmd = Command::Attach {
                pid: t.pid,
                created_at: t.created_at,
            };
            if self.send(&cmd) {
                if let Some(e) = self.engine.as_mut() {
                    e.attached = Some(t.clone());
                    e.attach_in_flight = true;
                }
                self.apply_gate(GateKey::Drop, None);
            }
        } else if e.attach_in_flight || matches!(phase, Phase::Attaching | Phase::Active) {
            let changed = e.attached.as_ref().is_some_and(|a| a.key() != t.key());
            if changed {
                // Another player: release first, attach once Idle (obligation 18).
                self.send_release();
            }
        }
    }

    fn flush_user_on(&mut self) {
        if !self.user_on_pending || self.engine.as_ref().is_none_or(|e| e.mismatch_at.is_some()) {
            return;
        }
        self.user_on_pending = false;
        if self.want_devocal {
            self.send(&Command::SetMode { devocal: true });
        }
    }

    fn retry_restore(&mut self, now_ms: u64) {
        if self.engine.as_ref().is_some_and(|e| e.holding()) {
            // Never while holding; and the engine may change the restore file meanwhile.
            self.restore_reset = true;
            return;
        }
        let fast = self.restore_awaiting
            && (self.restore_reset
                || self
                    .fast_restore_since
                    .is_some_and(|t| now_ms.saturating_sub(t) < AWAITING_RESTORE_FAST_FOR_MS));
        let interval = if fast {
            AWAITING_RESTORE_RETRY_MS
        } else {
            RESTORE_RETRY_MS
        };
        if self
            .last_restore_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < interval)
        {
            return;
        }
        self.last_restore_ms = Some(now_ms);
        if (self.restore_pending)() {
            self.run_restore(now_ms);
        } else {
            self.restore_awaiting = false;
            self.fast_restore_since = None;
            self.last_restore_outcome = None;
        }
    }

    /// Runs the restore and updates the fast-retry state: entries still await the player;
    /// progress (an entry restored or removed), a different outcome or a pending reset
    /// restarts the 60 s fast period.
    fn run_restore(&mut self, now_ms: u64) {
        let out = (self.restore)();
        let awaiting = out.is_some_and(|o| o.awaiting_player > 0);
        if awaiting {
            let progress = out.is_some_and(|o| o.restored + o.left_changed + o.gone > 0);
            let changed = out != self.last_restore_outcome;
            if self.restore_reset || progress || changed || self.fast_restore_since.is_none() {
                self.fast_restore_since = Some(now_ms);
            }
        } else {
            self.fast_restore_since = None;
        }
        self.restore_awaiting = awaiting;
        self.last_restore_outcome = out;
        self.restore_reset = false;
    }

    // ---- helpers ----

    fn note_attach_failure(&mut self, p: &PlayerProcess, now_ms: u64) {
        if self.failure_target == Some(p.key()) {
            self.attach_failures += 1;
        } else {
            self.failure_target = Some(p.key());
            self.attach_failures = 1;
        }
        self.retry_after_ms = now_ms + ATTACH_RETRY_MS;
    }

    fn send_release(&mut self) {
        let needed = self.engine.as_ref().is_some_and(|e| {
            e.mismatch_at.is_none()
                && !e.release_sent
                && (e.attach_in_flight || matches!(e.phase(), Phase::Attaching | Phase::Active))
        });
        if needed && self.send(&Command::Release) {
            if let Some(e) = self.engine.as_mut() {
                e.release_sent = true;
                e.release_sent_at = Some(self.now_ms);
            }
            self.apply_gate(GateKey::Drop, None);
        }
    }

    /// What the engine should be running: the GPU-overload fallback if there is one, else the
    /// selected model on the selected device. `None` while there is no model file.
    fn wanted(&self) -> Option<SentModel> {
        self.fallback.clone().or_else(|| {
            Some(SentModel {
                id: self.selection.model_id.clone(),
                path: self.model.clone()?,
                device: self.selection.device.clone(),
            })
        })
    }

    fn wanted_id(&self) -> &str {
        self.fallback
            .as_ref()
            .map_or(self.selection.model_id.as_str(), |f| f.id.as_str())
    }

    /// Sends `set_model` for [`Self::wanted`], with the geometry from the manifest. An
    /// unencodable path or a model the manifest does not list makes devocal unavailable.
    fn send_set_model(&mut self) -> bool {
        let Some(wanted) = self.wanted() else {
            return false;
        };
        let Some(spec) = bundled().model(&wanted.id) else {
            self.unavailable = true;
            self.want_devocal = false;
            self.user_on_pending = false;
            self.error = Some(format!("model_not_found:{}", wanted.id));
            return false;
        };
        let devices = &spec.devices;
        let cmd = Command::SetModel {
            id: wanted.id.clone(),
            path: wanted.path.clone(),
            device: wanted.device.clone(),
            // Only StemgenRT reads this; a window model has `cpu_threads` in `windowed`.
            threads: devices
                .cpu
                .as_ref()
                .or(devices.gpu.as_ref())
                .map_or(1, |d| d.threads),
            windowed: windowed_spec(spec),
        };
        if let Err(e) = encode(&cmd) {
            self.unavailable = true;
            self.want_devocal = false;
            self.user_on_pending = false;
            self.error = Some(format!("model_path_unencodable: {e}"));
            return false;
        }
        if !self.send(&cmd) {
            return false;
        }
        if let Some(e) = self.engine.as_mut() {
            e.sent_model = Some(wanted);
            e.overload_handled = false;
            e.model_failed = false;
        }
        true
    }

    /// A GPU-only model fell behind on the GPU: the engine stays in `Fallback(Overload)` and
    /// waits for the app. Load StemgenRT (the engine fades out and swaps; devocal stays on) and
    /// keep it until the selection changes. A model with a CPU path is moved by the engine itself.
    fn on_gpu_overloaded(&mut self) {
        let Some(e) = self.engine.as_mut() else {
            return;
        };
        if std::mem::replace(&mut e.overload_handled, true) {
            return;
        }
        let Some(sent) = e.sent_model.as_ref() else {
            return;
        };
        let has_cpu = bundled()
            .model(&sent.id)
            .is_none_or(|m| m.devices.cpu.is_some());
        if has_cpu {
            return;
        }
        let Some(path) = (self.stemgenrt_path)() else {
            self.error = Some(format!("model_not_found:{STEMGENRT_ID}"));
            return;
        };
        self.fallback = Some(SentModel {
            id: STEMGENRT_ID.into(),
            path,
            device: "cpu".into(),
        });
        self.send_set_model();
    }

    /// Sends `cmd`; an unencodable command never reaches the link. A link error means the
    /// link is broken: the engine is killed and the next tick handles it as an exit.
    fn send(&mut self, cmd: &Command) -> bool {
        if self.engine.is_none() {
            return false;
        }
        if let Err(e) = encode(cmd) {
            self.error = Some(format!("encode_failed: {e}"));
            return false;
        }
        let Some(e) = self.engine.as_mut() else {
            return false;
        };
        match e.link.send(cmd) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("devocal: engine link broken: {err}");
                e.link.kill();
                false
            }
        }
    }

    fn apply_gate(&mut self, key: GateKey, value: Option<f32>) {
        if self.gate_key != key {
            self.gate_key = key;
            self.gate.set(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devocal::link::fake::{FakeEngine, FakeLink};
    use devocal_core::protocol::{
        Device, DeviceNote, ErrorCode, FallbackReason, Metrics, Mode, Phase, WindowedSpec, PROTOCOL,
    };
    use std::sync::Mutex;

    struct Rig {
        sup: Supervisor<FakeLink>,
        engines: Arc<Mutex<Vec<FakeEngine>>>,
        /// The gate value seen by each restore call.
        restores: Arc<Mutex<Vec<Option<f32>>>>,
        gate: Arc<AttenuationGate>,
        spawn_error: Arc<Mutex<Option<String>>>,
        /// "spawn" / "restore" in call order.
        order: Arc<Mutex<Vec<&'static str>>>,
        /// What each restore call reports.
        outcome: Arc<Mutex<Option<RestoreOutcome>>>,
    }

    fn rig() -> Rig {
        rig_with(false)
    }

    fn rig_with(restore_pending: bool) -> Rig {
        let engines: Arc<Mutex<Vec<FakeEngine>>> = Arc::default();
        let restores: Arc<Mutex<Vec<Option<f32>>>> = Arc::default();
        let gate = Arc::new(AttenuationGate::new());
        let spawn_error: Arc<Mutex<Option<String>>> = Arc::default();
        let (e, s) = (engines.clone(), spawn_error.clone());
        let (r, g) = (restores.clone(), gate.clone());
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let (o1, o2) = (order.clone(), order.clone());
        let outcome: Arc<Mutex<Option<RestoreOutcome>>> =
            Arc::new(Mutex::new(Some(RestoreOutcome::default())));
        let out = outcome.clone();
        let sup = Supervisor::new(
            Box::new(move || {
                if let Some(err) = s.lock().unwrap().clone() {
                    return Err(err);
                }
                let engine = FakeEngine::default();
                e.lock().unwrap().push(engine.clone());
                o1.lock().unwrap().push("spawn");
                Ok(engine.link())
            }),
            Box::new(move || {
                r.lock().unwrap().push(g.read().1);
                o2.lock().unwrap().push("restore");
                out.lock().unwrap().clone()
            }),
            gate.clone(),
        )
        .with_restore_pending(Box::new(move || restore_pending))
        .with_stemgenrt_path(Box::new(|| Some(model())))
        .with_shutdown_wait(Duration::from_millis(50));
        Rig {
            sup,
            engines,
            restores,
            gate,
            spawn_error,
            order,
            outcome,
        }
    }

    impl Rig {
        fn engine(&self, i: usize) -> FakeEngine {
            self.engines.lock().unwrap()[i].clone()
        }
        fn last(&self) -> FakeEngine {
            self.engines.lock().unwrap().last().unwrap().clone()
        }
        fn spawns(&self) -> usize {
            self.engines.lock().unwrap().len()
        }
        fn restores(&self) -> usize {
            self.restores.lock().unwrap().len()
        }
        fn phase(&self) -> &'static str {
            self.sup.status().phase
        }
        /// enable + tick with `p` + Attaching + Active(Devocal), at time `t`.
        fn hold(&mut self, t: u64, p: &PlayerProcess) {
            self.sup.enable(Some(model()), Selection::default());
            self.sup.tick(t, Some(p.clone()), true);
            let e = self.last();
            e.push(state(Phase::Attaching, None));
            e.push(state(Phase::Active, Some(Mode::Devocal)));
            self.sup.tick(t + 100, Some(p.clone()), true);
        }
    }

    fn model() -> PathBuf {
        PathBuf::from(r"C:\models\stemgenrt-hop128.onnx")
    }

    fn player(pid: u32) -> PlayerProcess {
        PlayerProcess {
            source_id: "folia".into(),
            pid,
            created_at: 1_000 + u64::from(pid),
        }
    }

    fn state(phase: Phase, mode: Option<Mode>) -> Event {
        Event::State {
            phase,
            mode,
            fallback_reason: None,
            attached_pid: None,
        }
    }

    fn metrics(attenuation: f32, epoch: u64) -> Event {
        Event::Metrics(Metrics {
            mode: Some(Mode::Devocal),
            latency_ms: 42.0,
            load_ratio: 0.3,
            underruns: 0,
            fallback_reason: None,
            attenuation,
            attenuation_epoch: epoch,
            session_overridden: 0,
            input_silent_ms: 0,
            device: None,
            device_note: None,
        })
    }

    fn error(code: ErrorCode) -> Event {
        Event::Error {
            code,
            message: "x".into(),
        }
    }

    fn set_model() -> Command {
        Command::SetModel {
            id: "stemgenrt-hop128".into(),
            path: model(),
            device: "auto".into(),
            threads: 1,
            windowed: None,
        }
    }

    fn attach(p: &PlayerProcess) -> Command {
        Command::Attach {
            pid: p.pid,
            created_at: p.created_at,
        }
    }

    fn handshake(p: &PlayerProcess) -> Vec<Command> {
        vec![
            Command::Hello { version: PROTOCOL },
            set_model(),
            attach(p),
            Command::SetMode { devocal: true },
        ]
    }

    #[test]
    fn enable_sends_handshake_in_order() {
        let mut r = rig();
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, None, true);
        assert_eq!(r.spawns(), 0, "no target: no spawn");
        assert_eq!(r.phase(), "attaching");
        let p = player(7);
        r.sup.tick(100, Some(p.clone()), true);
        assert_eq!(r.spawns(), 1);
        assert_eq!(r.engine(0).sent(), handshake(&p));
        // Nothing is re-sent on later ticks.
        r.sup.tick(200, Some(p.clone()), true);
        assert_eq!(r.engine(0).sent().len(), 4);
    }

    #[test]
    fn enable_while_held_only_sends_set_mode() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.sup.disable();
        r.engine(0).clear_sent();
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(300, Some(p), true);
        assert_eq!(r.engine(0).sent(), vec![Command::SetMode { devocal: true }]);
        assert_eq!(r.spawns(), 1);
    }

    fn fallback(reason: FallbackReason) -> Event {
        Event::State {
            phase: Phase::Active,
            mode: Some(Mode::Fallback),
            fallback_reason: Some(reason),
            attached_pid: Some(7),
        }
    }

    #[test]
    fn enable_after_a_fallback_sends_off_then_on() {
        for reason in [FallbackReason::Overload, FallbackReason::ModelError] {
            let mut r = rig();
            let p = player(7);
            r.hold(0, &p);
            r.last().push(fallback(reason));
            r.sup.tick(200, Some(p.clone()), true);
            assert_eq!(r.phase(), "fallback");
            r.last().clear_sent();
            // The engine ignores a repeated "on" (obligation 1): the retry is off-then-on.
            r.sup.enable(Some(model()), Selection::default());
            assert_eq!(
                r.last().sent(),
                vec![
                    Command::SetMode { devocal: false },
                    Command::SetMode { devocal: true }
                ]
            );
            // The engine leaves the fallback; a later enable is a plain "on" again.
            r.last().push(state(Phase::Active, Some(Mode::Passthrough)));
            r.last().push(state(Phase::Active, Some(Mode::Devocal)));
            r.sup.tick(300, Some(p.clone()), true);
            assert_eq!(r.phase(), "devocal");
            r.last().clear_sent();
            r.sup.enable(Some(model()), Selection::default());
            assert_eq!(r.last().sent(), vec![Command::SetMode { devocal: true }]);
        }
    }

    #[test]
    fn enable_while_passthrough_or_devocal_sends_no_off() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.sup.disable();
        r.last().push(state(Phase::Active, Some(Mode::Passthrough)));
        r.sup.tick(200, Some(p), true);
        r.last().clear_sent();
        r.sup.enable(Some(model()), Selection::default());
        assert_eq!(r.last().sent(), vec![Command::SetMode { devocal: true }]);
    }

    #[test]
    fn enable_after_the_engine_gave_up_on_the_model_reloads_it() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        // The automatic retry, then the engine gives up and clears its toggle.
        r.last().push(error(ErrorCode::ModelLoadFailed));
        r.sup.tick(200, Some(p.clone()), true);
        r.last().push(error(ErrorCode::ModelLoadFailed));
        r.last().push(state(Phase::Active, Some(Mode::Passthrough)));
        r.sup.tick(300, Some(p.clone()), true);
        let s = r.sup.status();
        assert_eq!(s.phase, "passthrough");
        assert!(s.error.unwrap().starts_with("model_load_failed"));
        r.last().clear_sent();
        r.sup.enable(Some(model()), Selection::default());
        assert_eq!(
            r.last().sent(),
            vec![set_model(), Command::SetMode { devocal: true }]
        );
        assert_eq!(r.sup.status().error, None);
        // Loaded this time: a later enable does not reload.
        r.sup.disable();
        r.last().clear_sent();
        r.sup.enable(Some(model()), Selection::default());
        assert_eq!(r.last().sent(), vec![Command::SetMode { devocal: true }]);
    }

    #[test]
    fn enable_after_no_model_reloads_it() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().push(error(ErrorCode::NoModel));
        r.sup.tick(200, Some(p), true);
        assert!(r.sup.status().error.unwrap().starts_with("no_model"));
        r.last().clear_sent();
        r.sup.enable(Some(model()), Selection::default());
        assert_eq!(
            r.last().sent(),
            vec![set_model(), Command::SetMode { devocal: true }]
        );
    }

    #[test]
    fn waiting_for_player_while_there_is_nothing_to_hold() {
        let mut r = rig();
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, None, false);
        let s = r.sup.status();
        assert_eq!(s.phase, "attaching", "phase unchanged for older readers");
        assert!(s.waiting_for_player);
        // A player appears: the attach starts and the wait is over.
        let p = player(7);
        r.sup.tick(100, Some(p.clone()), true);
        assert!(!r.sup.status().waiting_for_player);
        r.last().push(state(Phase::Attaching, None));
        r.last().push(state(Phase::Active, Some(Mode::Devocal)));
        r.sup.tick(200, Some(p.clone()), true);
        assert!(!r.sup.status().waiting_for_player);
        // Held with no player reported: still held, not waiting.
        r.sup.tick(300, None, false);
        assert_eq!(r.phase(), "devocal");
        assert!(!r.sup.status().waiting_for_player);
        // The engine lets go and there is no player: waiting again.
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(400, None, false);
        let s = r.sup.status();
        assert_eq!(s.phase, "attaching");
        assert!(s.waiting_for_player);
        // Off is not waiting.
        r.sup.release();
        assert!(!r.sup.status().waiting_for_player);
    }

    #[test]
    fn player_exit_while_held_waits_for_a_player_and_keeps_the_attach_budget() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        assert_eq!(r.phase(), "devocal");
        // The player exits: the app's target goes away (its process identity is no longer
        // alive) and the engine, having seen the exit, lets go on its own.
        r.last().clear_sent();
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(1_000, None, false);
        let s = r.sup.status();
        assert_eq!(s.phase, "attaching");
        assert!(s.waiting_for_player, "{s:?}");
        assert!(!s.held);
        assert_eq!(s.error, None);
        // However long it takes, nothing is attached and nothing is given up.
        for t in (1_100..40_000).step_by(100) {
            r.sup.tick(t, None, false);
        }
        assert!(r.last().sent().is_empty(), "{:?}", r.last().sent());
        let s = r.sup.status();
        assert_eq!((s.phase, s.waiting_for_player), ("attaching", true));
        // The relaunched player (a new process) is attached at once.
        let relaunched = player(8);
        r.sup.tick(40_000, Some(relaunched.clone()), true);
        assert_eq!(r.last().sent(), vec![attach(&relaunched)]);
        assert!(!r.sup.status().waiting_for_player);
        assert_eq!(r.spawns(), 1);
    }

    #[test]
    fn crash_restores_then_restarts_up_to_three_times_per_minute() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(p.clone()), true);
        assert_eq!(r.spawns(), 1);
        for (i, t) in [1_000u64, 2_000, 3_000].into_iter().enumerate() {
            r.last().exit();
            r.sup.tick(t, Some(p.clone()), true);
            assert_eq!(r.restores(), i + 1);
            assert_eq!(r.spawns(), i + 2);
            assert_eq!(r.phase(), "restarting");
            // The restarted engine gets the whole handshake again.
            assert_eq!(r.last().sent(), handshake(&p));
        }
        r.last().exit();
        r.sup.tick(4_000, Some(p.clone()), true);
        assert_eq!(r.restores(), 4);
        assert_eq!(r.spawns(), 4, "budget exhausted: no 4th restart");
        assert_eq!(r.phase(), "failed");
        assert_eq!(r.sup.status().error.as_deref(), Some("engine_crashed"));
        assert_eq!(r.gate.read().1, Some(1.0));
        r.sup.tick(5_000, Some(p.clone()), true);
        assert_eq!(r.spawns(), 4, "failed stays failed without a user action");

        // 61 s: a user enable spawns again and the restart budget has recovered.
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(61_000, Some(p.clone()), true);
        assert_eq!(r.spawns(), 5);
        r.last().exit();
        r.sup.tick(62_000, Some(p.clone()), true);
        assert_eq!(r.spawns(), 6);
        assert_eq!(r.phase(), "restarting");
    }

    #[test]
    fn restart_reaches_active_and_leaves_restarting() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().exit();
        r.sup.tick(1_000, Some(p.clone()), true);
        assert_eq!(r.phase(), "restarting");
        r.last().push(state(Phase::Attaching, None));
        r.last().push(state(Phase::Active, Some(Mode::Devocal)));
        r.sup.tick(1_100, Some(p), true);
        assert_eq!(r.phase(), "devocal");
    }

    #[test]
    fn any_exit_restores_even_without_a_hold_and_does_not_restart() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.sup.release();
        r.last().push(state(Phase::Releasing, None));
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(300, Some(p.clone()), true);
        assert_eq!(r.phase(), "off");
        r.last().exit();
        r.sup.tick(400, Some(p), true);
        assert_eq!(r.restores(), 1);
        assert_eq!(r.spawns(), 1);
        assert_eq!(r.phase(), "off");
    }

    #[test]
    fn gate_drops_during_attach_and_scales_when_active() {
        let mut r = rig();
        let p = player(7);
        assert_eq!(r.gate.read(), (0, Some(1.0)));
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().push(state(Phase::Attaching, None));
        r.sup.tick(100, Some(p.clone()), true);
        let (e1, g) = r.gate.read();
        assert_eq!(g, None, "attaching drops the capture");

        // Metrics are ignored unless the last State was Active (obligation 22).
        r.last().push(metrics(1e-4 / 0.3, 1));
        r.sup.tick(200, Some(p.clone()), true);
        assert_eq!(r.gate.read().1, None);

        r.last().push(state(Phase::Active, Some(Mode::Devocal)));
        r.last().push(metrics(1e-4 / 0.3, 1));
        r.sup.tick(300, Some(p.clone()), true);
        let (e2, g) = r.gate.read();
        let g = g.expect("scaled when active");
        assert!((g - 3000.0).abs() <= 0.1, "gain {g}");
        assert!(e2 > e1);

        // Same epoch: no new set.
        r.last().push(metrics(1e-4 / 0.3, 1));
        r.sup.tick(400, Some(p.clone()), true);
        assert_eq!(r.gate.read().0, e2);

        // New epoch: set again.
        r.last().push(metrics(1e-4 / 0.5, 2));
        r.sup.tick(500, Some(p.clone()), true);
        let (e3, g) = r.gate.read();
        assert!(e3 > e2);
        assert!((g.unwrap() - 5000.0).abs() <= 0.1);

        r.last().push(state(Phase::Releasing, None));
        r.sup.tick(600, Some(p.clone()), true);
        let (e4, g) = r.gate.read();
        assert!(e4 > e3);
        assert_eq!(g, None);
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(700, Some(p), true);
        let (e5, g) = r.gate.read();
        assert!(e5 > e4);
        assert_eq!(g, Some(1.0));
    }

    #[test]
    fn engine_exit_drops_gate_restores_then_unity() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().push(metrics(1e-4, 1));
        r.sup.tick(200, Some(p.clone()), true);
        assert_eq!(r.gate.read().1, Some(10_000.0));
        // The restart's spawn fails, so nothing re-attaches after the restore.
        *r.spawn_error.lock().unwrap() = Some("gone".into());
        r.last().exit();
        r.sup.tick(300, Some(p), true);
        assert_eq!(
            *r.restores.lock().unwrap(),
            vec![None],
            "restore runs while dropping"
        );
        assert_eq!(r.gate.read().1, Some(1.0), "unity after the restore");
    }

    #[test]
    fn player_change_releases_then_attaches_new() {
        let mut r = rig();
        let (a, b) = (player(7), player(8));
        r.hold(0, &a);
        r.last().clear_sent();
        r.sup.tick(300, Some(b.clone()), true);
        assert_eq!(r.last().sent(), vec![Command::Release]);
        assert_eq!(r.gate.read().1, None);
        r.last().push(state(Phase::Releasing, None));
        r.sup.tick(400, Some(b.clone()), true);
        assert_eq!(r.last().sent(), vec![Command::Release], "wait for Idle");
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(500, Some(b.clone()), true);
        assert_eq!(r.last().sent(), vec![Command::Release, attach(&b)]);
        assert_eq!(r.spawns(), 1);
        // Same pid with a new creation time is also a different player.
        let mut b2 = b.clone();
        b2.created_at += 1;
        r.last().push(state(Phase::Attaching, None));
        r.last().push(state(Phase::Active, Some(Mode::Devocal)));
        r.sup.tick(600, Some(b.clone()), true);
        r.last().clear_sent();
        r.sup.tick(700, Some(b2), true);
        assert_eq!(r.last().sent(), vec![Command::Release]);
    }

    #[test]
    fn unrequested_idle_retries_the_same_player_with_backoff_then_fails() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        for i in 0..2u64 {
            r.last().clear_sent();
            let t = 1_000 + i * 10_000;
            r.last().push(state(Phase::Releasing, None));
            r.last().push(state(Phase::Idle, None));
            r.sup.tick(t, Some(p.clone()), true);
            r.sup.tick(t + 1_000, Some(p.clone()), true);
            assert!(r.last().sent().is_empty(), "backoff before retrying");
            r.sup.tick(t + ATTACH_RETRY_MS, Some(p.clone()), true);
            assert_eq!(r.last().sent(), vec![attach(&p)]);
            r.last().push(state(Phase::Attaching, None));
            r.last().push(state(Phase::Active, Some(Mode::Devocal)));
            r.sup.tick(t + ATTACH_RETRY_MS + 100, Some(p.clone()), true);
        }
        r.last().clear_sent();
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(40_000, Some(p.clone()), true);
        r.sup.tick(50_000, Some(p.clone()), true);
        assert!(r.last().sent().is_empty());
        assert_eq!(r.phase(), "failed");
        assert_eq!(r.sup.status().error.as_deref(), Some("attach_failed"));
        // A different player is tried again.
        r.sup.tick(50_100, Some(player(9)), true);
        assert_eq!(r.last().sent(), vec![attach(&player(9))]);
    }

    #[test]
    fn attach_failed_while_idle_counts_as_a_failed_attempt() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().push(error(ErrorCode::AttachFailed));
        r.sup.tick(100, Some(p.clone()), true);
        r.last().clear_sent();
        r.sup.tick(200, Some(p.clone()), true);
        assert!(r.last().sent().is_empty());
        r.sup.tick(100 + ATTACH_RETRY_MS, Some(p.clone()), true);
        assert_eq!(r.last().sent(), vec![attach(&p)]);
    }

    #[test]
    fn disable_keeps_hold() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().clear_sent();
        r.sup.disable();
        assert_eq!(r.last().sent(), vec![Command::SetMode { devocal: false }]);
        r.last().push(state(Phase::Active, Some(Mode::Passthrough)));
        r.sup.tick(300, Some(p), true);
        assert_eq!(r.last().sent(), vec![Command::SetMode { devocal: false }]);
        let s = r.sup.status();
        assert!(s.held);
        assert_eq!(s.phase, "passthrough");
    }

    #[test]
    fn release_sends_release_and_clears_held() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        assert!(r.sup.status().held);
        r.last().clear_sent();
        r.sup.release();
        assert_eq!(r.last().sent(), vec![Command::Release]);
        assert!(!r.sup.status().held);
        r.last().push(state(Phase::Releasing, None));
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(300, Some(p), true);
        assert_eq!(r.last().sent(), vec![Command::Release], "no re-attach");
        assert_eq!(r.phase(), "off");
    }

    #[test]
    fn missing_model_reports_unavailable() {
        let dir = crate::devocal::tests::temp_dir("missing-model");
        let model = crate::devocal::model_path_from(
            None,
            &dir,
            crate::devocal::model::manifest::bundled(),
            STEMGENRT_ID,
        );
        assert_eq!(model, None);
        let mut r = rig();
        r.sup.enable(model, Selection::default());
        r.sup.tick(0, Some(player(7)), true);
        assert_eq!(r.spawns(), 0);
        assert_eq!(r.phase(), "unavailable");
    }

    #[test]
    fn an_installed_model_clears_model_not_found_without_enabling() {
        let mut r = rig();
        r.sup.enable(None, Selection::default());
        assert_eq!(r.phase(), "unavailable");
        r.sup.model_installed();
        let s = r.sup.status();
        assert_eq!(s.phase, "off");
        assert_eq!(s.error, None);
        // Not enabled: a player appearing spawns no engine.
        r.sup.tick(0, Some(player(7)), true);
        assert_eq!(r.spawns(), 0);
        assert_eq!(r.phase(), "off");
        // A later enable with the model goes ahead as usual.
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(100, Some(player(7)), true);
        assert_eq!(r.spawns(), 1);
    }

    #[test]
    fn an_install_after_a_failed_enable_does_not_start_devocal_by_itself() {
        // Devocal was on, the engine crashed past its restart budget, the model file went away,
        // and the user's enable found no model.
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(p.clone()), true);
        for t in [1_000u64, 2_000, 3_000, 4_000] {
            r.last().exit();
            r.sup.tick(t, Some(p.clone()), true);
        }
        assert_eq!(r.phase(), "failed");
        assert_eq!(r.spawns(), 4);
        r.sup.enable(None, Selection::default());
        assert_eq!(r.phase(), "unavailable");
        // An install without auto-enable: nothing starts or takes over the player.
        r.sup.model_installed();
        r.sup.tick(70_000, Some(p.clone()), true);
        r.sup.tick(70_100, Some(p.clone()), true);
        assert_eq!(r.spawns(), 4, "no engine spawned");
        let s = r.sup.status();
        assert_eq!(s.phase, "off");
        assert!(!s.held);
        // The user's next enable works as usual.
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(70_200, Some(p.clone()), true);
        assert_eq!(r.spawns(), 5);
    }

    #[test]
    fn a_failed_enable_while_restarting_stops_the_restart() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().exit();
        r.sup.tick(1_000, None, true);
        assert_eq!(r.phase(), "restarting");
        r.sup.enable(None, Selection::default());
        r.sup.model_installed();
        r.sup.tick(1_100, Some(p.clone()), true);
        assert_eq!(r.spawns(), 1, "no restart after the failed enable");
        assert_eq!(r.phase(), "off");
    }

    #[test]
    fn an_installed_model_leaves_other_conditions_alone() {
        // Another unavailable reason stays.
        let mut r = rig();
        *r.spawn_error.lock().unwrap() = Some("devocal-engine.exe not found".into());
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(player(7)), true);
        r.sup.model_installed();
        let s = r.sup.status();
        assert_eq!(s.phase, "unavailable");
        assert!(s.error.unwrap().contains("devocal-engine.exe not found"));
        // A running devocal is not touched.
        let mut r = rig();
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(player(7)), true);
        let before = r.sup.status();
        r.sup.model_installed();
        assert_eq!(r.sup.status(), before);
        assert_eq!(r.spawns(), 1);
    }

    #[test]
    fn missing_engine_reports_unavailable_without_retrying() {
        let mut r = rig();
        *r.spawn_error.lock().unwrap() = Some("devocal-engine.exe not found".into());
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(player(7)), true);
        r.sup.tick(100, Some(player(7)), true);
        let s = r.sup.status();
        assert_eq!(s.phase, "unavailable");
        assert!(s.error.unwrap().contains("devocal-engine.exe not found"));
    }

    #[test]
    fn shutdown_kills_and_restores_if_engine_hangs() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        let e = r.last();
        r.sup.shutdown();
        assert_eq!(e.sent().last(), Some(&Command::Shutdown));
        assert!(e.killed());
        assert_eq!(r.restores(), 1);
        assert_eq!(r.gate.read().1, Some(1.0));
        // Ticks and commands after shutdown do nothing.
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(10_000, Some(p), true);
        assert_eq!(r.spawns(), 1);
    }

    #[test]
    fn shutdown_of_a_well_behaved_engine_does_not_kill_but_still_restores() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        let e = r.last();
        e.lock().exit_on_shutdown = true;
        r.sup.shutdown();
        assert!(!e.killed());
        assert_eq!(r.restores(), 1);
    }

    #[test]
    fn restore_retries_every_two_seconds_only_while_nothing_is_held() {
        let mut r = rig_with(true);
        r.sup.tick(0, None, false);
        assert_eq!(r.restores(), 1, "nothing held: retry at once");
        r.sup.tick(1_000, None, false);
        assert_eq!(r.restores(), 1);
        r.sup.tick(2_000, None, false);
        assert_eq!(r.restores(), 2);
        let p = player(7);
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(3_000, Some(p.clone()), true);
        // The attach is in flight (no State yet): no restore.
        r.sup.tick(6_100, Some(p.clone()), true);
        assert_eq!(r.restores(), 2);
        r.last().push(state(Phase::Attaching, None));
        r.last().push(state(Phase::Active, Some(Mode::Devocal)));
        for t in [8_200u64, 10_300, 12_400] {
            r.sup.tick(t, Some(p.clone()), true);
        }
        assert_eq!(r.restores(), 2, "held: no restore");
        r.sup.release();
        r.last().push(state(Phase::Releasing, None));
        r.sup.tick(14_500, Some(p.clone()), true);
        assert_eq!(r.restores(), 2, "releasing still holds");
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(16_600, Some(p), true);
        assert_eq!(r.restores(), 3);
    }

    fn awaiting(n: usize) -> Option<RestoreOutcome> {
        Some(RestoreOutcome {
            awaiting_player: n,
            ..Default::default()
        })
    }

    #[test]
    fn restore_retries_every_200_ms_while_entries_await_the_player() {
        let mut r = rig_with(true);
        *r.outcome.lock().unwrap() = awaiting(2);
        // Ticks every 100 ms, as the app does: one restore every other tick.
        let mut counts = Vec::new();
        for t in (0..=800u64).step_by(100) {
            r.sup.tick(t, None, false);
            counts.push(r.restores());
        }
        assert_eq!(counts, vec![1, 1, 2, 2, 3, 3, 4, 4, 5]);
        // The player came back and everything was restored: back to every 2 s.
        *r.outcome.lock().unwrap() = Some(RestoreOutcome {
            restored: 2,
            ..Default::default()
        });
        r.sup.tick(1_000, None, false);
        assert_eq!(r.restores(), 6);
        for t in (1_100..3_000u64).step_by(100) {
            r.sup.tick(t, None, false);
        }
        assert_eq!(r.restores(), 6);
        r.sup.tick(3_000, None, false);
        assert_eq!(r.restores(), 7);
    }

    /// Restore times in `[from, to)` with a tick every 100 ms and no player.
    fn restore_ticks(r: &mut Rig, from: u64, to: u64) -> Vec<u64> {
        let mut at = Vec::new();
        for t in (from..to).step_by(100) {
            let before = r.restores();
            r.sup.tick(t, None, false);
            if r.restores() > before {
                at.push(t);
            }
        }
        at
    }

    #[test]
    fn fast_restore_backs_off_after_60_s_without_progress_and_restarts_on_a_change() {
        let mut r = rig_with(true);
        *r.outcome.lock().unwrap() = awaiting(2);
        let fast = restore_ticks(&mut r, 0, 60_000);
        assert_eq!(fast.len(), 300, "every 200 ms for 60 s");
        assert_eq!(fast.last(), Some(&59_800));
        // No progress for 60 s: every 2 s.
        assert_eq!(
            restore_ticks(&mut r, 60_000, 70_000),
            vec![61_800, 63_800, 65_800, 67_800, 69_800]
        );
        // One entry was restored (progress, different outcome): fast again for 60 s.
        *r.outcome.lock().unwrap() = Some(RestoreOutcome {
            restored: 1,
            awaiting_player: 1,
            ..Default::default()
        });
        assert_eq!(restore_ticks(&mut r, 70_000, 72_000), vec![71_800]);
        *r.outcome.lock().unwrap() = awaiting(1);
        assert_eq!(
            restore_ticks(&mut r, 72_000, 73_000),
            vec![72_000, 72_200, 72_400, 72_600, 72_800]
        );
        // 60 s after the last change (72.0 s) it backs off again.
        let rest = restore_ticks(&mut r, 73_000, 140_000);
        assert!(
            rest.ends_with(&[131_800, 133_800, 135_800, 137_800, 139_800]),
            "{:?}",
            &rest[rest.len() - 6..]
        );
        // Same outcome, nothing new: stays at 2 s.
        assert_eq!(restore_ticks(&mut r, 140_000, 144_000).len(), 2);
    }

    #[test]
    fn a_hold_restarts_the_fast_restore_cadence() {
        let mut r = rig_with(true);
        *r.outcome.lock().unwrap() = awaiting(1);
        restore_ticks(&mut r, 0, 62_000);
        assert!(
            restore_ticks(&mut r, 62_000, 63_000).is_empty(),
            "backed off"
        );
        // A hold and its release may have changed the awaiting entries.
        let p = player(7);
        r.hold(63_000, &p);
        r.sup.release();
        r.last().push(state(Phase::Releasing, None));
        r.last().push(state(Phase::Idle, None));
        let before = r.restores();
        for t in (63_200..64_000u64).step_by(100) {
            r.sup.tick(t, Some(p.clone()), true);
        }
        assert_eq!(
            r.restores() - before,
            4,
            "fast again: 63.2, 63.4, 63.6, 63.8 s"
        );
    }

    #[test]
    fn fast_restore_retry_never_runs_while_holding() {
        let mut r = rig_with(true);
        *r.outcome.lock().unwrap() = awaiting(1);
        r.sup.tick(0, None, false);
        assert_eq!(r.restores(), 1);
        let p = player(7);
        r.hold(100, &p);
        let held = r.restores();
        for t in (300..3_000u64).step_by(100) {
            r.sup.tick(t, Some(p.clone()), true);
        }
        assert_eq!(r.restores(), held, "held: no restore");
        // Released: the fast cadence resumes at once.
        r.sup.release();
        r.last().push(state(Phase::Releasing, None));
        r.last().push(state(Phase::Idle, None));
        r.sup.tick(3_000, Some(p.clone()), true);
        assert_eq!(r.restores(), held + 1);
        r.sup.tick(3_100, Some(p.clone()), true);
        r.sup.tick(3_200, Some(p), true);
        assert_eq!(r.restores(), held + 2);
    }

    #[test]
    fn no_restore_retry_without_a_restore_file() {
        let mut r = rig_with(false);
        for t in [0u64, 2_000, 4_000] {
            r.sup.tick(t, None, false);
        }
        assert_eq!(r.restores(), 0);
    }

    #[test]
    fn model_load_failure_resends_set_model_once_per_user_action() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().clear_sent();
        r.last().push(error(ErrorCode::ModelLoadFailed));
        r.sup.tick(300, Some(p.clone()), true);
        assert_eq!(
            r.last().sent(),
            vec![set_model(), Command::SetMode { devocal: true }]
        );
        r.last().clear_sent();
        r.last().push(error(ErrorCode::ModelLoadFailed));
        r.sup.tick(400, Some(p.clone()), true);
        assert!(r.last().sent().is_empty(), "no loop on a bad model");
        assert!(r
            .sup
            .status()
            .error
            .unwrap()
            .starts_with("model_load_failed"));
        // A new user action allows one more retry; disable ends that allowance.
        r.sup.enable(Some(model()), Selection::default());
        r.sup.disable();
        r.last().clear_sent();
        r.last().push(error(ErrorCode::ModelLoadFailed));
        r.sup.tick(500, Some(p), true);
        assert!(r.last().sent().is_empty(), "disable ends the user action");
    }

    #[test]
    fn model_load_failure_after_disable_then_enable_retries_with_set_mode() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.sup.disable();
        r.sup.enable(Some(model()), Selection::default());
        r.last().clear_sent();
        r.last().push(error(ErrorCode::ModelLoadFailed));
        r.sup.tick(300, Some(p), true);
        assert_eq!(
            r.last().sent(),
            vec![set_model(), Command::SetMode { devocal: true }]
        );
    }

    #[test]
    fn protocol_error_during_hello_is_a_version_mismatch() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().clear_sent();
        r.last().push(error(ErrorCode::Protocol));
        r.sup.tick(100, Some(p.clone()), true);
        assert_eq!(r.phase(), "failed");
        assert!(r
            .sup
            .status()
            .error
            .unwrap()
            .starts_with("engine_version_mismatch"));
        assert_eq!(r.last().sent(), vec![Command::Shutdown]);
        r.last().exit();
        r.sup.tick(200, Some(p.clone()), true);
        assert_eq!(r.restores(), 1);
        assert_eq!(r.spawns(), 1, "no restart after a version mismatch");
        assert_eq!(r.phase(), "failed");
    }

    #[test]
    fn version_mismatched_engine_that_does_not_exit_is_killed() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(model()), Selection::default());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().push(error(ErrorCode::Protocol));
        r.sup.tick(100, Some(p.clone()), true);
        r.sup.tick(1_000, Some(p.clone()), true);
        assert!(!r.last().killed());
        r.sup.tick(2_200, Some(p), true);
        assert!(r.last().killed());
        assert_eq!(r.restores(), 1);
        assert_eq!(r.spawns(), 1);
    }

    #[test]
    fn protocol_error_after_the_hello_exchange_is_not_a_mismatch() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().push(error(ErrorCode::Protocol));
        r.sup.tick(300, Some(p), true);
        assert_eq!(r.phase(), "devocal");
    }

    #[test]
    fn status_reports_metrics_fallback_override_and_silence() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().push(Event::Metrics(Metrics {
            mode: Some(Mode::Devocal),
            latency_ms: 45.4,
            load_ratio: 0.25,
            underruns: 0,
            fallback_reason: None,
            attenuation: 1e-4,
            attenuation_epoch: 1,
            session_overridden: 2,
            input_silent_ms: 3_500,
            device: None,
            device_note: None,
        }));
        r.sup.tick(1_000, Some(p.clone()), true);
        let s = r.sup.status();
        assert_eq!(s.latency_ms, Some(45.4));
        assert_eq!(s.load_ratio, Some(0.25));
        assert!(s.input_silent);
        assert!(!s.session_overridden, "first sample is the baseline");
        r.sup.tick(1_100, Some(p.clone()), false);
        assert!(!r.sup.status().input_silent, "not playing: not silent");

        let mut m = match metrics(1e-4, 1) {
            Event::Metrics(m) => m,
            _ => unreachable!(),
        };
        m.session_overridden = 3;
        r.last().push(Event::Metrics(m));
        r.sup.tick(2_000, Some(p.clone()), true);
        assert!(r.sup.status().session_overridden);
        r.sup.tick(11_900, Some(p.clone()), true);
        assert!(r.sup.status().session_overridden);
        r.sup.tick(12_100, Some(p.clone()), true);
        assert!(!r.sup.status().session_overridden, "older than 10 s");

        r.last().push(Event::State {
            phase: Phase::Active,
            mode: Some(Mode::Fallback),
            fallback_reason: Some(FallbackReason::Overload),
            attached_pid: Some(7),
        });
        r.sup.tick(12_200, Some(p), true);
        let s = r.sup.status();
        assert_eq!(s.phase, "fallback");
        assert_eq!(s.fallback_reason, Some("overload"));
    }

    #[test]
    fn link_send_failure_is_handled_as_an_engine_exit() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        // The engine died between ticks; the next command fails.
        r.last().exit();
        r.sup.disable();
        r.sup.tick(300, Some(p), true);
        assert_eq!(r.restores(), 1);
        assert_eq!(r.spawns(), 2);
    }

    #[test]
    fn stuck_release_is_killed_and_restored_after_20s() {
        let mut r = rig();
        let (a, b) = (player(7), player(8));
        r.hold(0, &a);
        let first = r.last();
        // Player change at 300: release sent; the engine says Releasing and then stalls.
        r.sup.tick(300, Some(b.clone()), true);
        assert_eq!(first.sent().last(), Some(&Command::Release));
        first.push(state(Phase::Releasing, None));
        for t in [1_000u64, 10_000, 20_299] {
            r.sup.tick(t, Some(b.clone()), true);
        }
        assert!(!first.killed(), "deadline not reached yet");
        assert_eq!(r.restores(), 0);
        assert_eq!(r.phase(), "releasing");
        assert_eq!(r.gate.read().1, None);

        r.sup.tick(20_300, Some(b.clone()), true);
        assert!(first.killed());
        assert_eq!(*r.order.lock().unwrap(), vec!["spawn", "restore", "spawn"]);
        assert_eq!(*r.restores.lock().unwrap(), vec![None]);
        // The hold is still wanted: the restarted engine attaches the new player.
        assert_eq!(r.last().sent(), handshake(&b));
        assert_eq!(r.phase(), "restarting");
    }

    #[test]
    fn stuck_user_release_is_killed_and_restored_without_restart() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.sup.tick(1_000, Some(p.clone()), true);
        r.sup.release();
        r.last().push(state(Phase::Releasing, None));
        r.sup.tick(20_999, Some(p.clone()), true);
        assert!(!r.last().killed());
        r.sup.tick(21_000, Some(p), true);
        assert!(r.last().killed());
        assert_eq!(*r.order.lock().unwrap(), vec!["spawn", "restore"]);
        assert_eq!(r.gate.read().1, Some(1.0));
        assert_eq!(r.phase(), "off");
    }

    #[test]
    fn metrics_after_release_sent_keep_the_gate_closed() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().push(metrics(1e-4, 1));
        r.sup.tick(200, Some(p.clone()), true);
        assert_eq!(r.gate.read().1, Some(10_000.0));
        r.sup.release();
        let (epoch, g) = r.gate.read();
        assert_eq!(g, None);
        // Sent by the engine before it handled the release (still Active, new epoch).
        r.last().push(metrics(1e-4 / 0.5, 2));
        r.sup.tick(300, Some(p), true);
        assert_eq!(r.gate.read(), (epoch, None));
    }

    #[test]
    fn restart_budget_allows_three_per_window() {
        let mut b = RestartBudget::new();
        assert!(b.allow(0));
        assert!(b.allow(1));
        assert!(b.allow(2));
        assert!(!b.allow(59_999));
        assert!(b.allow(60_000));
    }

    fn bytesep() -> Selection {
        Selection {
            model_id: "bytesep-mobilenet-1s".into(),
            device: "gpu".into(),
        }
    }

    fn htdemucs() -> Selection {
        Selection {
            model_id: "htdemucs-ft-vocals-1s".into(),
            device: "auto".into(),
        }
    }

    fn bytesep_path() -> PathBuf {
        PathBuf::from(r"C:\models\bytesep-mobilenet-1s.onnx")
    }

    fn htdemucs_path() -> PathBuf {
        PathBuf::from(r"C:\models\htdemucs-ft-vocals-1s.onnx")
    }

    fn set_model_for(
        id: &str,
        path: PathBuf,
        device: &str,
        threads: u16,
        windowed: Option<WindowedSpec>,
    ) -> Command {
        Command::SetModel {
            id: id.into(),
            path,
            device: device.into(),
            threads,
            windowed,
        }
    }

    fn bytesep_set_model(device: &str) -> Command {
        set_model_for(
            "bytesep-mobilenet-1s",
            bytesep_path(),
            device,
            2,
            Some(WindowedSpec {
                window_ms: 1000,
                lookahead_ms: 100,
                gpu_hop_ms: Some(100),
                cpu_hop_ms: Some(200),
                cpu_threads: 2,
                vocals_index: 0,
            }),
        )
    }

    fn htdemucs_set_model() -> Command {
        set_model_for(
            "htdemucs-ft-vocals-1s",
            htdemucs_path(),
            "auto",
            1,
            Some(WindowedSpec {
                window_ms: 1000,
                lookahead_ms: 100,
                gpu_hop_ms: Some(100),
                cpu_hop_ms: None,
                cpu_threads: 0,
                vocals_index: 3,
            }),
        )
    }

    fn device_metrics(device: Device, note: Option<DeviceNote>) -> Event {
        let Event::Metrics(mut m) = metrics(1e-4, 1) else {
            unreachable!()
        };
        m.device = Some(device);
        m.device_note = note;
        Event::Metrics(m)
    }

    #[test]
    fn enable_with_selection_sends_windowed_set_model() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(bytesep_path()), bytesep());
        r.sup.tick(0, Some(p.clone()), true);
        assert_eq!(
            r.engine(0).sent(),
            vec![
                Command::Hello { version: PROTOCOL },
                bytesep_set_model("gpu"),
                attach(&p),
                Command::SetMode { devocal: true },
            ]
        );
        assert_eq!(
            r.sup.status().model_id.as_deref(),
            Some("bytesep-mobilenet-1s")
        );

        // A GPU-only model has no CPU hop and cpu_threads 0 (engine default).
        let mut r = rig();
        r.sup.enable(Some(htdemucs_path()), htdemucs());
        r.sup.tick(0, Some(p), true);
        assert_eq!(r.engine(0).sent()[1], htdemucs_set_model());
    }

    #[test]
    fn enable_resends_set_model_only_when_the_selection_changes() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().clear_sent();
        // Same selection: no SetModel, only the usual "on".
        r.sup.enable(Some(model()), Selection::default());
        assert_eq!(r.last().sent(), vec![Command::SetMode { devocal: true }]);
        r.last().clear_sent();
        // Same model, another device: reload.
        r.sup.enable(
            Some(model()),
            Selection {
                model_id: "stemgenrt-hop128".into(),
                device: "cpu".into(),
            },
        );
        assert_eq!(
            r.last().sent(),
            vec![
                set_model_for("stemgenrt-hop128", model(), "cpu", 1, None),
                Command::SetMode { devocal: true }
            ]
        );
        r.last().clear_sent();
        // Another model.
        r.sup.enable(Some(bytesep_path()), bytesep());
        assert_eq!(
            r.last().sent(),
            vec![bytesep_set_model("gpu"), Command::SetMode { devocal: true }]
        );
    }

    #[test]
    fn restart_keeps_selection() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(bytesep_path()), bytesep());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().exit();
        r.sup.tick(1_000, Some(p.clone()), true);
        assert_eq!(r.spawns(), 2);
        r.sup.tick(1_100, Some(p.clone()), true);
        assert_eq!(
            r.engine(1).sent(),
            vec![
                Command::Hello { version: PROTOCOL },
                bytesep_set_model("gpu"),
                attach(&p),
                Command::SetMode { devocal: true },
            ]
        );
    }

    #[test]
    fn gpu_only_overload_switches_to_stemgenrt() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(htdemucs_path()), htdemucs());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().push(state(Phase::Attaching, None));
        r.last().push(state(Phase::Active, Some(Mode::Devocal)));
        r.sup.tick(100, Some(p.clone()), true);
        r.last().clear_sent();
        // Fell behind: the engine stays in Fallback(Overload) and says why.
        r.last().push(Event::State {
            phase: Phase::Active,
            mode: Some(Mode::Fallback),
            fallback_reason: Some(FallbackReason::Overload),
            attached_pid: None,
        });
        r.last()
            .push(device_metrics(Device::Gpu, Some(DeviceNote::GpuOverloaded)));
        r.sup.tick(200, Some(p.clone()), true);
        assert_eq!(
            r.last().sent(),
            vec![set_model_for("stemgenrt-hop128", model(), "cpu", 1, None)]
        );
        // Further metrics with the note do not resend.
        r.last()
            .push(device_metrics(Device::Gpu, Some(DeviceNote::GpuOverloaded)));
        r.sup.tick(300, Some(p.clone()), true);
        assert_eq!(r.last().sent().len(), 1);
        // The selection is unchanged; the status says what runs and why.
        let s = r.sup.status();
        assert_eq!(s.model_id.as_deref(), Some("stemgenrt-hop128"));
        assert_eq!(s.device_note, Some("gpu_overloaded"));
        // An engine restart in the meantime loads StemgenRT as well.
        r.last().exit();
        r.sup.tick(1_000, Some(p.clone()), true);
        r.sup.tick(1_100, Some(p.clone()), true);
        assert_eq!(
            r.engine(1).sent()[1],
            set_model_for("stemgenrt-hop128", model(), "cpu", 1, None)
        );
        // The user picking again (a changed selection) ends the fallback.
        r.sup.enable(Some(bytesep_path()), bytesep());
        assert_eq!(
            r.engine(1).sent().iter().rev().nth(1).cloned(),
            Some(bytesep_set_model("gpu"))
        );
        assert_eq!(
            r.sup.status().model_id.as_deref(),
            Some("bytesep-mobilenet-1s")
        );
    }

    #[test]
    fn overload_of_a_model_with_a_cpu_path_is_left_to_the_engine() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(bytesep_path()), bytesep());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().push(state(Phase::Attaching, None));
        r.last().push(state(Phase::Active, Some(Mode::Devocal)));
        r.sup.tick(100, Some(p.clone()), true);
        r.last().clear_sent();
        r.last()
            .push(device_metrics(Device::Cpu, Some(DeviceNote::GpuOverloaded)));
        r.sup.tick(200, Some(p), true);
        assert_eq!(r.last().sent(), vec![]);
        let s = r.sup.status();
        assert_eq!(
            (s.device, s.device_note),
            (Some("cpu"), Some("gpu_overloaded"))
        );
        assert_eq!(s.model_id.as_deref(), Some("bytesep-mobilenet-1s"));
    }

    #[test]
    fn missing_selected_model_reports_its_id() {
        let mut r = rig();
        r.sup.enable(None, bytesep());
        let s = r.sup.status();
        assert_eq!(s.phase, "unavailable");
        assert_eq!(
            s.error.as_deref(),
            Some("model_not_found:bytesep-mobilenet-1s")
        );
        // Installing a model clears it.
        r.sup.model_installed();
        assert_eq!(r.sup.status().error, None);
    }

    #[test]
    fn gpu_required_does_not_swap_the_model_and_a_later_enable_retries() {
        let mut r = rig();
        let p = player(7);
        r.sup.enable(Some(htdemucs_path()), htdemucs());
        r.sup.tick(0, Some(p.clone()), true);
        r.last().clear_sent();
        r.last().push(error(ErrorCode::GpuRequired));
        r.sup.tick(100, Some(p), true);
        assert_eq!(r.last().sent(), vec![], "no silent model change, no retry");
        assert_eq!(r.sup.status().error.as_deref(), Some("gpu_required: x"));
        r.sup.enable(Some(htdemucs_path()), htdemucs());
        assert_eq!(r.last().sent()[0], htdemucs_set_model());
    }

    #[test]
    fn status_reports_device_and_note_while_active() {
        let mut r = rig();
        let p = player(7);
        r.hold(0, &p);
        r.last().push(device_metrics(
            Device::Cpu,
            Some(DeviceNote::GpuUnavailable),
        ));
        r.sup.tick(200, Some(p.clone()), true);
        let s = r.sup.status();
        assert_eq!(
            (s.device, s.device_note),
            (Some("cpu"), Some("gpu_unavailable"))
        );
        r.last().push(device_metrics(Device::Gpu, None));
        r.sup.tick(300, Some(p), true);
        let s = r.sup.status();
        assert_eq!((s.device, s.device_note), (Some("gpu"), None));
    }
}
