//! Real-time vocal removal: the app side of `devocal-engine.exe`.
//!
//! - [`gate`]: how the app's own player capture must be scaled while the engine holds it;
//! - [`link`]: starting the engine and talking to it over its named pipe;
//! - [`supervisor`]: what the user asked for, restarts, restore, player changes.
//!
//! [`DevocalState`] runs the supervisor on its own thread (COM MTA, needed by `WinSessions`
//! for the restore and by the player resolve) every 100 ms; the Tauri commands only change
//! the user's wish and read the last published status.

pub mod gate;
pub mod link;
pub mod model;
pub mod supervisor;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use devocal_core::restore::{restore, RestoreOutcome};
use devocal_core::sessions::SessionVolumes;
use serde::Deserialize;

use crate::media::{AudioTarget, MediaState};
use gate::AttenuationGate;
use link::ProcessLink;
use supervisor::{DevocalStatus, PlayerProcess, Supervisor};

/// Env override for the StemgenRT ONNX file.
pub const MODEL_ENV: &str = "TUNE_LOVE_STEMGENRT_ONNX";
pub const RESTORE_FILE: &str = "devocal-restore.json";
pub const ENGINE_EXE: &str = "devocal-engine.exe";
const TICK: Duration = Duration::from_millis(100);
/// Re-resolve a player that could not be resolved, or whose process ended, this often.
const RESOLVE_RETRY_MS: u64 = 2_000;
/// How long app exit waits for the supervisor thread to shut the engine down.
const SHUTDOWN_JOIN: Duration = Duration::from_secs(3);

/// The model file: env `TUNE_LOVE_STEMGENRT_ONNX` (an existing file, used as is), then the
/// verified `<data>/models/stemgenrt-hop128/<file>`. May hash the file (cached), so keep it off
/// hot paths.
pub fn model_path(data_dir: &Path) -> Option<PathBuf> {
    model_path_from(
        std::env::var_os(MODEL_ENV),
        data_dir,
        model::manifest::bundled(),
    )
}

/// [`model_path`] with the env value and manifest passed in (testable without the process env).
pub fn model_path_from(
    env: Option<OsString>,
    data_dir: &Path,
    manifest: &model::manifest::Manifest,
) -> Option<PathBuf> {
    let from_env = env.filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(path) = from_env.filter(|p| p.is_file()) {
        return Some(path);
    }
    model::verify::installed_files(
        manifest,
        &data_dir.join("models"),
        model::manifest::STEMGENRT_ID,
    )
    .and_then(|files| files.into_iter().next())
}

pub fn restore_file(data_dir: &Path) -> PathBuf {
    data_dir.join(RESTORE_FILE)
}

pub fn engine_log(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("devocal-engine.log")
}

/// `devocal-engine.exe` next to the app executable.
pub fn engine_exe() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map(|exe| exe.with_file_name(ENGINE_EXE))
        .map_err(|e| format!("locating the app executable: {e}"))
}

/// Runs the restore file if it exists and logs anything noteworthy. The caller must be on a
/// COM MTA thread when `sessions` is `WinSessions`.
pub fn restore_with(path: &Path, sessions: &dyn SessionVolumes) -> Option<RestoreOutcome> {
    if !path.exists() {
        return None;
    }
    let out = restore(path, sessions);
    if out != RestoreOutcome::default() {
        eprintln!("devocal: restore {}: {out:?}", path.display());
    }
    Some(out)
}

/// App start: restore a file left by a crashed app/engine before anything else runs.
pub fn restore_at_startup(path: &Path) {
    if !path.exists() {
        return;
    }
    #[cfg(windows)]
    {
        let path = path.to_path_buf();
        let joined = std::thread::Builder::new()
            .name("devocal-startup-restore".into())
            .spawn(move || {
                let Ok(_apartment) = crate::audio::capture::Apartment::enter() else {
                    eprintln!("devocal: startup restore skipped: COM unavailable");
                    return;
                };
                restore_with(&path, &devocal_core::sessions_win::WinSessions);
            })
            .map(|h| h.join());
        if !matches!(joined, Ok(Ok(()))) {
            eprintln!("devocal: startup restore did not complete");
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DevocalRequest {
    /// enable | disable | release
    pub action: String,
}

struct Shared {
    supervisor: Mutex<Option<Supervisor<ProcessLink>>>,
    status: Mutex<DevocalStatus>,
    data_dir: Mutex<Option<PathBuf>>,
    stop: Mutex<Option<Sender<()>>>,
    done: Mutex<Option<Receiver<()>>>,
    /// Model installs so far (see `resolve_then_lock`).
    installs: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
pub struct DevocalState {
    gate: Arc<AttenuationGate>,
    shared: Arc<Shared>,
}

impl DevocalState {
    pub fn new(gate: Arc<AttenuationGate>) -> Self {
        Self {
            gate,
            shared: Arc::new(Shared {
                supervisor: Mutex::new(None),
                status: Mutex::new(DevocalStatus::off()),
                data_dir: Mutex::new(None),
                stop: Mutex::new(None),
                done: Mutex::new(None),
                installs: AtomicU64::new(0),
            }),
        }
    }

    pub fn gate(&self) -> Arc<AttenuationGate> {
        self.gate.clone()
    }

    /// Creates the supervisor and starts its 100 ms thread. Without a data dir there is no
    /// model and no restore file, so devocal reports `unavailable`.
    pub fn start(&self, data_dir: Option<PathBuf>, media: MediaState) {
        let mut slot = lock(&self.shared.supervisor);
        if slot.is_some() {
            return;
        }
        *lock(&self.shared.data_dir) = data_dir.clone();
        *slot = Some(build_supervisor(data_dir, self.gate.clone()));
        drop(slot);
        let (stop_tx, stop_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("devocal-supervisor".into())
            .spawn(move || worker(&shared, &media, &stop_rx, &done_tx));
        match spawned {
            Ok(_) => {
                *lock(&self.shared.stop) = Some(stop_tx);
                *lock(&self.shared.done) = Some(done_rx);
            }
            Err(e) => eprintln!("devocal: supervisor thread unavailable: {e}"),
        }
    }

    pub fn status(&self) -> DevocalStatus {
        lock(&self.shared.status).clone()
    }

    pub fn command(&self, action: &str) -> Result<DevocalStatus, String> {
        let data_dir = lock(&self.shared.data_dir).clone();
        // Worked out before taking the supervisor lock: the first check hashes the model file.
        let (model, mut slot) = if action == "enable" {
            resolve_then_lock(&self.shared.installs, &self.shared.supervisor, || {
                data_dir.as_deref().and_then(model_path)
            })
        } else {
            (None, lock(&self.shared.supervisor))
        };
        let Some(sup) = slot.as_mut() else {
            return Err("devocal is not started yet".into());
        };
        apply(sup, action, model)?;
        let status = sup.status();
        *lock(&self.shared.status) = status.clone();
        Ok(status)
    }

    /// A model install finished (any install, whether or not it auto-enables): clears an
    /// `unavailable` caused only by the missing model, so the main window stops offering the
    /// download. Does not enable devocal. Call it after the file is in place.
    pub fn model_installed(&self) {
        // Counted before the lock: an enable that looked for the model before the file landed
        // and takes the lock after this point looks again (see `resolve_then_lock`).
        self.shared.installs.fetch_add(1, Ordering::SeqCst);
        let mut slot = lock(&self.shared.supervisor);
        if let Some(sup) = slot.as_mut() {
            sup.model_installed();
            *lock(&self.shared.status) = sup.status();
        }
    }

    /// App exit: the supervisor thread shuts the engine down (and restores); waits up to 3 s.
    /// If it does not finish, the engine still releases by itself when the app is gone.
    pub fn shutdown(&self) {
        let stop = lock(&self.shared.stop).take();
        let done = lock(&self.shared.done).take();
        if let (Some(stop), Some(done)) = (stop, done) {
            let _ = stop.send(());
            if done.recv_timeout(SHUTDOWN_JOIN).is_err() {
                eprintln!("devocal: engine shutdown did not finish in time");
            }
        }
    }
}

/// Looks up the model for an enable (may hash, so outside the lock), then takes the supervisor
/// lock. If the lookup found nothing but an install finished meanwhile, it looks once more:
/// otherwise this stale enable would mark devocal `unavailable` (model_not_found) right after
/// [`DevocalState::model_installed`] cleared it.
fn resolve_then_lock<'a, T>(
    installs: &AtomicU64,
    slot: &'a Mutex<T>,
    resolve: impl Fn() -> Option<PathBuf>,
) -> (Option<PathBuf>, MutexGuard<'a, T>) {
    let seen = installs.load(Ordering::SeqCst);
    let model = resolve();
    let guard = lock(slot);
    if model.is_some() || installs.load(Ordering::SeqCst) == seen {
        return (model, guard);
    }
    drop(guard);
    let model = resolve();
    (model, lock(slot))
}

/// One user action on the supervisor.
fn apply<L: link::EngineLink>(
    sup: &mut Supervisor<L>,
    action: &str,
    model: Option<PathBuf>,
) -> Result<(), String> {
    match action {
        "enable" => sup.enable(model),
        "disable" => sup.disable(),
        "release" => sup.release(),
        other => return Err(format!("unknown devocal action {other:?}")),
    }
    Ok(())
}

fn build_supervisor(
    data_dir: Option<PathBuf>,
    gate: Arc<AttenuationGate>,
) -> Supervisor<ProcessLink> {
    let restore_path = data_dir.as_deref().map(restore_file);
    let log = data_dir.as_deref().map(engine_log);
    let spawn_restore = restore_path.clone();
    let spawn = Box::new(move || -> Result<ProcessLink, String> {
        let (Some(restore), Some(log)) = (&spawn_restore, &log) else {
            return Err("the app data directory is unavailable".into());
        };
        ProcessLink::spawn(&engine_exe()?, restore, log)
    });
    let run_path = restore_path.clone();
    let run_restore = Box::new(move || -> Option<RestoreOutcome> {
        #[cfg(windows)]
        {
            run_path
                .as_deref()
                .and_then(|path| restore_with(path, &devocal_core::sessions_win::WinSessions))
        }
        #[cfg(not(windows))]
        {
            let _ = &run_path;
            None
        }
    });
    let pending = Box::new(move || restore_path.as_deref().is_some_and(Path::exists));
    Supervisor::new(spawn, run_restore, gate).with_restore_pending(pending)
}

fn worker(shared: &Shared, media: &MediaState, stop: &Receiver<()>, done: &Sender<()>) {
    #[cfg(windows)]
    let _apartment = match crate::audio::capture::Apartment::enter() {
        Ok(a) => Some(a),
        Err(e) => {
            eprintln!("devocal: COM unavailable on the supervisor thread: {e}");
            None
        }
    };
    let start = Instant::now();
    let mut players = PlayerCache::default();
    loop {
        let now_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let target = media.audio_target();
        let playing = target.as_ref().is_some_and(|t| t.playing);
        let player = players.player(target.as_ref(), now_ms);
        if let Some(sup) = lock(&shared.supervisor).as_mut() {
            sup.tick(now_ms, player, playing);
            *lock(&shared.status) = sup.status();
        }
        match stop.recv_timeout(TICK) {
            Err(RecvTimeoutError::Timeout) => continue,
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(sup) = lock(&shared.supervisor).as_mut() {
        sup.shutdown();
        *lock(&shared.status) = sup.status();
    }
    let _ = done.send(());
}

/// The player process for the media session's source, resolved only when the source
/// changes, or (every 2 s) while it is unresolved or its process has ended.
#[derive(Default)]
struct PlayerCache {
    source_id: Option<String>,
    #[cfg(windows)]
    identity: Option<crate::audio::process::native::ProcessIdentity>,
    last_attempt_ms: u64,
    last_error: Option<String>,
}

impl PlayerCache {
    #[cfg(windows)]
    fn player(&mut self, target: Option<&AudioTarget>, now_ms: u64) -> Option<PlayerProcess> {
        let t = target?;
        let changed = self.source_id.as_deref() != Some(t.source_id.as_str());
        let stale = self.identity.as_ref().is_none_or(|i| !i.is_alive());
        if changed || (stale && now_ms.saturating_sub(self.last_attempt_ms) >= RESOLVE_RETRY_MS) {
            self.source_id = Some(t.source_id.clone());
            self.last_attempt_ms = now_ms;
            self.identity = match crate::audio::process::native::resolve(&t.source_id) {
                Ok(identity) => {
                    self.last_error = None;
                    Some(identity)
                }
                Err(e) => {
                    if self.last_error.as_deref() != Some(e.as_str()) {
                        eprintln!("devocal: player {} unresolved: {e}", t.source_id);
                        self.last_error = Some(e);
                    }
                    None
                }
            };
        }
        let identity = self.identity.as_ref().filter(|i| i.is_alive())?;
        Some(PlayerProcess {
            source_id: t.source_id.clone(),
            pid: identity.record.pid,
            created_at: identity.record.created,
        })
    }

    #[cfg(not(windows))]
    fn player(&mut self, _target: Option<&AudioTarget>, _now_ms: u64) -> Option<PlayerProcess> {
        let _ = (&self.source_id, self.last_attempt_ms, &self.last_error);
        None
    }
}

#[tauri::command]
pub fn get_devocal_status(state: tauri::State<'_, DevocalState>) -> DevocalStatus {
    state.status()
}

#[tauri::command]
pub async fn devocal_command(
    request: DevocalRequest,
    state: tauri::State<'_, DevocalState>,
) -> Result<DevocalStatus, String> {
    // Off the main thread: the supervisor may be busy starting the engine.
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.command(&request.action))
        .await
        .map_err(|e| format!("devocal command failed: {e}"))?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::devocal::link::fake::{FakeEngine, FakeLink};
    use devocal_core::protocol::Command;
    use devocal_core::restore::{write_atomic, RestoreEntry, RestoreRecord, RESTORE_VERSION};
    use devocal_core::sessions::{FakeSessions, SessionInfo, HELD_VOLUME};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// A fresh, empty directory under the system temp dir.
    pub fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tune-love-devocal-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn model_path_prefers_an_existing_env_file_then_the_verified_data_dir_file() {
        use crate::devocal::model::verify::tests::fake_manifest;
        let manifest = fake_manifest(b"good model bytes");
        let dir = temp_dir("model-path");
        assert_eq!(model_path_from(None, &dir, &manifest), None);
        let env_file = dir.join("custom.onnx");
        // A set but missing env path falls through to the data dir.
        assert_eq!(
            model_path_from(Some(env_file.clone().into()), &dir, &manifest),
            None
        );
        let data_file = dir
            .join("models")
            .join("stemgenrt-hop128")
            .join("model.onnx");
        std::fs::create_dir_all(data_file.parent().unwrap()).unwrap();
        // Wrong content does not verify.
        std::fs::write(&data_file, b"x").unwrap();
        assert_eq!(model_path_from(None, &dir, &manifest), None);
        std::fs::write(&data_file, b"good model bytes").unwrap();
        assert_eq!(
            model_path_from(None, &dir, &manifest),
            Some(data_file.clone())
        );
        assert_eq!(
            model_path_from(Some(env_file.clone().into()), &dir, &manifest),
            Some(data_file.clone())
        );
        // An existing env file wins and is not verified.
        std::fs::write(&env_file, b"x").unwrap();
        assert_eq!(
            model_path_from(Some(env_file.clone().into()), &dir, &manifest),
            Some(env_file)
        );
        // An empty env value is ignored.
        assert_eq!(
            model_path_from(Some(OsString::new()), &dir, &manifest),
            Some(data_file)
        );
    }

    #[test]
    fn paths_live_under_the_data_dir() {
        let dir = Path::new(r"C:\data");
        assert_eq!(restore_file(dir), dir.join("devocal-restore.json"));
        assert_eq!(engine_log(dir), dir.join("logs").join("devocal-engine.log"));
        assert_eq!(
            engine_exe().unwrap().file_name().unwrap(),
            "devocal-engine.exe"
        );
    }

    #[test]
    fn restore_with_restores_a_held_session_and_removes_the_file() {
        let dir = temp_dir("restore");
        let path = restore_file(&dir);
        assert_eq!(restore_with(&path, &FakeSessions::new()), None, "no file");

        let sessions = FakeSessions::new();
        sessions.add_session(
            SessionInfo {
                instance_id: "{e}|s1".into(),
                session_identifier: "{e}|player".into(),
                pid: 40,
                endpoint_id: "{e}".into(),
                active: true,
            },
            HELD_VOLUME,
            false,
        );
        sessions.set_process_created(40, Some(77));
        write_atomic(
            &path,
            &RestoreRecord {
                version: RESTORE_VERSION,
                entries: vec![RestoreEntry {
                    pid: 40,
                    created_at: 77,
                    instance_id: "{e}|s1".into(),
                    original_volume: 0.3,
                    original_mute: false,
                    saved_at_ms: 1,
                    session_identifier: "{e}|player".into(),
                }],
            },
        )
        .unwrap();
        let out = restore_with(&path, &sessions).unwrap();
        assert_eq!(out.restored, 1);
        assert_eq!(sessions.writes(), vec![("{e}|s1".to_string(), 0.3)]);
        assert!(!path.exists());
    }

    fn fake_supervisor() -> (Supervisor<FakeLink>, Arc<Mutex<Vec<FakeEngine>>>) {
        let engines: Arc<Mutex<Vec<FakeEngine>>> = Arc::default();
        let e = engines.clone();
        let sup = Supervisor::new(
            Box::new(move || {
                let engine = FakeEngine::default();
                e.lock().unwrap().push(engine.clone());
                Ok(engine.link())
            }),
            Box::new(|| None),
            Arc::new(AttenuationGate::new()),
        )
        .with_restore_pending(Box::new(|| false));
        (sup, engines)
    }

    #[test]
    fn actions_map_to_supervisor_calls() {
        let dir = temp_dir("actions");
        let (mut sup, engines) = fake_supervisor();
        apply(&mut sup, "enable", None).unwrap();
        assert_eq!(sup.status().phase, "unavailable", "no verified model");
        assert!(apply(&mut sup, "explode", None).is_err());

        let model = dir.join("model.onnx");
        apply(&mut sup, "enable", Some(model)).unwrap();
        let player = PlayerProcess {
            source_id: "folia".into(),
            pid: 7,
            created_at: 9,
        };
        sup.tick(0, Some(player), true);
        let engine = engines.lock().unwrap()[0].clone();
        assert_eq!(engine.sent().len(), 4);
        engine.clear_sent();
        apply(&mut sup, "disable", None).unwrap();
        apply(&mut sup, "release", None).unwrap();
        assert_eq!(
            engine.sent(),
            vec![Command::SetMode { devocal: false }, Command::Release]
        );
    }

    #[test]
    fn state_without_a_started_supervisor_reports_off_and_rejects_commands() {
        let state = DevocalState::new(Arc::new(AttenuationGate::new()));
        assert_eq!(state.status(), DevocalStatus::off());
        assert!(state.command("enable").is_err());
        state.model_installed(); // no supervisor: nothing to clear
        assert_eq!(state.status(), DevocalStatus::off());
        state.shutdown(); // no thread: returns at once
    }

    #[test]
    fn an_enable_that_found_no_model_looks_again_after_an_install_landed() {
        let installs = AtomicU64::new(0);
        let slot = Mutex::new(());
        let calls = AtomicUsize::new(0);
        // The install lands between the lookup and the lock.
        let (model, guard) = resolve_then_lock(&installs, &slot, || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                installs.fetch_add(1, Ordering::SeqCst);
                None
            } else {
                Some(PathBuf::from("model.onnx"))
            }
        });
        drop(guard);
        assert_eq!(model, Some(PathBuf::from("model.onnx")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // No install meanwhile: one lookup, its answer kept.
        let calls = AtomicUsize::new(0);
        let (model, _guard) = resolve_then_lock(&installs, &slot, || {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        });
        assert_eq!(model, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_enable_that_found_the_model_does_not_look_again() {
        let installs = AtomicU64::new(0);
        let slot = Mutex::new(());
        let calls = AtomicUsize::new(0);
        let (model, _guard) = resolve_then_lock(&installs, &slot, || {
            calls.fetch_add(1, Ordering::SeqCst);
            installs.fetch_add(1, Ordering::SeqCst);
            Some(PathBuf::from("model.onnx"))
        });
        assert_eq!(model, Some(PathBuf::from("model.onnx")));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn status_serialises_camel_case() {
        let v = serde_json::to_value(DevocalStatus::off()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "phase": "off", "held": false, "latencyMs": null, "loadRatio": null,
                "fallbackReason": null, "sessionOverridden": false, "inputSilent": false,
                "waitingForPlayer": false, "error": null
            })
        );
    }
}
