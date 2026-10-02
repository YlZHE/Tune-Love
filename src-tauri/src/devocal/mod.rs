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
pub mod supervisor;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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
/// Default model location under the app data dir (where `npm run fetch:stemgenrt` puts it).
pub const MODEL_FILE: &str = "stemgenrt-hop128.onnx";
pub const RESTORE_FILE: &str = "devocal-restore.json";
pub const ENGINE_EXE: &str = "devocal-engine.exe";
const TICK: Duration = Duration::from_millis(100);
/// Re-resolve a player that could not be resolved, or whose process ended, this often.
const RESOLVE_RETRY_MS: u64 = 2_000;
/// How long app exit waits for the supervisor thread to shut the engine down.
const SHUTDOWN_JOIN: Duration = Duration::from_secs(3);

/// The model file: env `TUNE_LOVE_STEMGENRT_ONNX`, then `<data>/models/stemgenrt-hop128.onnx`;
/// the first that exists.
pub fn model_path(data_dir: &Path) -> Option<PathBuf> {
    model_path_from(std::env::var_os(MODEL_ENV), data_dir)
}

/// [`model_path`] with the env value passed in (testable without touching the process env).
pub fn model_path_from(env: Option<OsString>, data_dir: &Path) -> Option<PathBuf> {
    let from_env = env.filter(|v| !v.is_empty()).map(PathBuf::from);
    let default = data_dir.join("models").join(MODEL_FILE);
    from_env.into_iter().chain([default]).find(|p| p.is_file())
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
        let mut slot = lock(&self.shared.supervisor);
        let Some(sup) = slot.as_mut() else {
            return Err("devocal is not started yet".into());
        };
        apply(sup, action, data_dir.as_deref())?;
        let status = sup.status();
        *lock(&self.shared.status) = status.clone();
        Ok(status)
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

/// One user action on the supervisor.
fn apply<L: link::EngineLink>(
    sup: &mut Supervisor<L>,
    action: &str,
    data_dir: Option<&Path>,
) -> Result<(), String> {
    match action {
        "enable" => sup.enable(data_dir.and_then(model_path)),
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
    let run_restore = Box::new(move || {
        #[cfg(windows)]
        if let Some(path) = &run_path {
            restore_with(path, &devocal_core::sessions_win::WinSessions);
        }
        #[cfg(not(windows))]
        let _ = &run_path;
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    fn model_path_prefers_an_existing_env_file_then_the_data_dir() {
        let dir = temp_dir("model-path");
        assert_eq!(model_path_from(None, &dir), None);
        let env_file = dir.join("custom.onnx");
        // A set but missing env path falls through to the data dir.
        assert_eq!(model_path_from(Some(env_file.clone().into()), &dir), None);
        std::fs::create_dir_all(dir.join("models")).unwrap();
        let data_file = dir.join("models").join("stemgenrt-hop128.onnx");
        std::fs::write(&data_file, b"x").unwrap();
        assert_eq!(model_path_from(None, &dir), Some(data_file.clone()));
        assert_eq!(
            model_path_from(Some(env_file.clone().into()), &dir),
            Some(data_file)
        );
        std::fs::write(&env_file, b"x").unwrap();
        assert_eq!(
            model_path_from(Some(env_file.clone().into()), &dir),
            Some(env_file)
        );
        // An empty env value is ignored.
        assert_eq!(
            model_path_from(Some(OsString::new()), &dir),
            Some(dir.join("models").join("stemgenrt-hop128.onnx"))
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
            Box::new(|| {}),
            Arc::new(AttenuationGate::new()),
        )
        .with_restore_pending(Box::new(|| false));
        (sup, engines)
    }

    #[test]
    fn actions_map_to_supervisor_calls() {
        let dir = temp_dir("actions");
        let (mut sup, engines) = fake_supervisor();
        apply(&mut sup, "enable", Some(&dir)).unwrap();
        assert_eq!(
            sup.status().phase,
            "unavailable",
            "no model in the data dir"
        );
        assert!(apply(&mut sup, "explode", Some(&dir)).is_err());

        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(dir.join("models").join(MODEL_FILE), b"x").unwrap();
        apply(&mut sup, "enable", Some(&dir)).unwrap();
        let player = PlayerProcess {
            source_id: "folia".into(),
            pid: 7,
            created_at: 9,
        };
        sup.tick(0, Some(player), true);
        let engine = engines.lock().unwrap()[0].clone();
        assert_eq!(engine.sent().len(), 4);
        engine.clear_sent();
        apply(&mut sup, "disable", Some(&dir)).unwrap();
        apply(&mut sup, "release", Some(&dir)).unwrap();
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
        state.shutdown(); // no thread: returns at once
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
