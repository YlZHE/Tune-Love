//! In-app model download. The Tauri commands live here: `get_model_status` and `model_command`
//! (download, cancel, import, delete).
//!
//! Every `ModelState` call may hash a model file the first time, so each one runs on a blocking
//! thread; the job future it returns is spawned on Tauri's async runtime.

pub mod download;
#[cfg(test)]
pub mod fake;
pub mod fetch;
pub mod manifest;
pub mod state;
pub mod verify;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use tauri::{AppHandle, Manager, State, WebviewWindow};

use download::{ModelError, RetryPolicy};
use fetch::ReqwestFetcher;
use manifest::{valid_prefix, Manifest, STEMGENRT_ID};
use state::{ModelPhase, ModelState, ModelStatus};

/// Debug builds only: when set (non-empty), the origin source is pointed at a closed port so the
/// download has to fall back to the mirrors (for the on-device test).
#[cfg(debug_assertions)]
const BREAK_ORIGIN_ENV: &str = "TUNE_LOVE_MODEL_BREAK_ORIGIN";

const ACTIONS: &[&str] = &["download", "cancel", "import", "delete"];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRequest {
    pub id: String,
    /// download | cancel | import | delete
    pub action: String,
    /// Turn de-vocal on once the model is in place (StemgenRT only).
    #[serde(default)]
    pub auto_enable: bool,
    /// A user-entered download-acceleration prefix; empty means none.
    #[serde(default)]
    pub mirror_prefix: Option<String>,
}

pub fn validate_request(req: &ModelRequest, manifest: &Manifest) -> Result<(), ModelError> {
    if manifest.model(&req.id).is_none() || !ACTIONS.contains(&req.action.as_str()) {
        return Err(ModelError::UnknownModel);
    }
    if custom_prefix(req).is_some_and(|p| !valid_prefix(p)) {
        return Err(ModelError::InvalidPrefix);
    }
    Ok(())
}

pub fn wants_auto_enable(req: &ModelRequest) -> bool {
    req.auto_enable && req.id == STEMGENRT_ID
}

/// What a download or import request does, given whether the model is already installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Not installed: start the job (for import, open the file dialog first).
    Start,
    /// Installed and auto-enable asked: enable de-vocal now. `begin_*` would finish at once
    /// without calling `on_installed`, so the install callback would never do it.
    EnableNow,
    /// Installed: nothing to do, and no file dialog.
    Nothing,
}

fn next_step(req: &ModelRequest, already_installed: bool) -> Next {
    if !already_installed {
        Next::Start
    } else if wants_auto_enable(req) && matches!(req.action.as_str(), "download" | "import") {
        Next::EnableNow
    } else {
        Next::Nothing
    }
}

/// The one import dialog that may be open.
static IMPORT_DIALOG_OPEN: AtomicBool = AtomicBool::new(false);

/// The slot of the open import dialog; dropping it frees the slot.
struct DialogSlot(&'static AtomicBool);

impl DialogSlot {
    /// `None` if a dialog already holds the slot.
    fn take(flag: &'static AtomicBool) -> Option<DialogSlot> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| DialogSlot(flag))
    }
}

impl Drop for DialogSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The user's prefix, an empty one counting as not filled in.
fn custom_prefix(req: &ModelRequest) -> Option<&str> {
    req.mirror_prefix.as_deref().filter(|p| !p.is_empty())
}

/// The on-device test switch; release builds have no such switch.
fn break_origin() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var_os(BREAK_ORIGIN_ENV).is_some_and(|v| !v.is_empty())
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

/// A new HTTP client for each download job. reqwest reads the system and environment proxy when
/// the client is built, so a proxy the user turns on (or off) after a failure takes effect on the
/// next try, without restarting the app.
fn fetcher() -> Result<Arc<ReqwestFetcher>, ModelError> {
    new_fetcher(ReqwestFetcher::standard)
}

/// Builds a client; a build failure is logged and reported as `network_unreachable`.
fn new_fetcher<F>(build: impl FnOnce() -> Result<F, String>) -> Result<Arc<F>, ModelError> {
    build().map(Arc::new).map_err(|e| {
        eprintln!("models: {e}");
        ModelError::Network
    })
}

/// Takes the same path as `devocal_command("enable")`, on a blocking thread; a failure is only
/// logged.
fn enable_devocal(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = app.state::<super::DevocalState>().command("enable") {
            eprintln!("models: auto-enable de-vocal failed: {e}");
        }
    });
}

/// What a finished install does, on a blocking thread: always clear a de-vocal `unavailable`
/// caused by the missing model (so the main-window hint does not go stale), then enable de-vocal
/// if the request asked for it.
fn on_installed(app: &AppHandle, req: &ModelRequest) -> Box<dyn FnOnce() + Send> {
    let app = app.clone();
    let enable = wants_auto_enable(req);
    Box::new(move || {
        tauri::async_runtime::spawn_blocking(move || {
            let devocal = app.state::<super::DevocalState>();
            devocal.model_installed();
            if enable {
                if let Err(e) = devocal.command("enable") {
                    eprintln!("models: auto-enable de-vocal failed: {e}");
                }
            }
        });
    })
}

/// May hash (cached).
fn is_installed(state: &ModelState, id: &str) -> bool {
    state.statuses().iter().any(|s| s.id == id && s.phase == ModelPhase::Installed)
}

/// Blocking. `Some(statuses)` if the model is already installed, after enabling de-vocal when
/// the request asks for it; `None` if the job is still to be started.
fn settle_installed(app: &AppHandle, state: &ModelState, req: &ModelRequest) -> Option<Vec<ModelStatus>> {
    let next = next_step(req, is_installed(state, &req.id));
    if next == Next::Start {
        return None;
    }
    // The model is in place (perhaps put there outside the app, by the fetch script or a manual
    // copy): a missing-model state of de-vocal no longer applies, as after an install here.
    app.state::<super::DevocalState>().model_installed();
    if next == Next::EnableNow {
        enable_devocal(app);
    }
    Some(state.statuses())
}

/// Runs `f` (which may hash files) on a blocking thread; errors become machine codes. A panic in
/// `f` is logged and reported as `internal_error`.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ModelError> + Send + 'static) -> Result<T, String> {
    match tauri::async_runtime::spawn_blocking(f).await {
        Ok(result) => result.map_err(|e| e.code()),
        Err(e) => {
            eprintln!("models: command task failed: {e}");
            Err(ModelError::Internal.code())
        }
    }
}

/// Blocking.
fn start_download(app: &AppHandle, state: &ModelState, req: &ModelRequest) -> Result<Vec<ModelStatus>, ModelError> {
    if let Some(statuses) = settle_installed(app, state, req) {
        return Ok(statuses);
    }
    let job = state.begin_download(
        fetcher()?,
        &req.id,
        custom_prefix(req),
        RetryPolicy::standard(),
        break_origin(),
        on_installed(app, req),
    )?;
    tauri::async_runtime::spawn(job);
    Ok(state.statuses())
}

/// Blocking.
fn start_import(
    app: &AppHandle,
    state: &ModelState,
    req: &ModelRequest,
    file: PathBuf,
) -> Result<Vec<ModelStatus>, ModelError> {
    // Installed meanwhile (another import or a download finished while the dialog was open)?
    if let Some(statuses) = settle_installed(app, state, req) {
        return Ok(statuses);
    }
    // Still the final guard: a job may have started while the dialog was open.
    let job = state.begin_import(&req.id, file, on_installed(app, req))?;
    tauri::async_runtime::spawn(job);
    Ok(state.statuses())
}

async fn import(
    app: AppHandle,
    window: WebviewWindow,
    state: ModelState,
    req: ModelRequest,
) -> Result<Vec<ModelStatus>, String> {
    // One dialog at a time: a second click on Import while one is open is refused.
    let slot = DialogSlot::take(&IMPORT_DIALOG_OPEN).ok_or_else(|| ModelError::AlreadyRunning.code())?;
    // Settled before the dialog opens, not after the user has picked a file: an installed model
    // needs no file, and a running job would refuse the import anyway.
    let settled = blocking({
        let (app, state, req) = (app.clone(), state.clone(), req.clone());
        move || {
            if let Some(statuses) = settle_installed(&app, &state, &req) {
                return Ok(Some(statuses));
            }
            let busy = state
                .statuses()
                .iter()
                .any(|s| matches!(s.phase, ModelPhase::Downloading | ModelPhase::Verifying));
            if busy {
                return Err(ModelError::AlreadyRunning);
            }
            Ok(None)
        }
    })
    .await?;
    if let Some(statuses) = settled {
        return Ok(statuses);
    }
    // The slot travels with the dialog, so it stays taken until the dialog has closed even if
    // this future were dropped meanwhile.
    let (slot, picked) = blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        // Parented to the calling window, so the picker is not hidden behind an always-on-top one.
        let picked = window
            .dialog()
            .file()
            .set_parent(&window)
            .add_filter("ONNX 模型", &["onnx"])
            .blocking_pick_file();
        Ok((slot, picked))
    })
    .await?;
    drop(slot);
    let Some(picked) = picked else {
        // Dialog closed without a choice.
        return blocking(move || Ok(state.statuses())).await;
    };
    let file = picked.into_path().map_err(|_| ModelError::ImportUnsupported.code())?;
    blocking(move || start_import(&app, &state, &req, file)).await
}

#[tauri::command]
pub async fn get_model_status(state: State<'_, ModelState>) -> Result<Vec<ModelStatus>, String> {
    // The first query hashes installed files.
    let state = state.inner().clone();
    blocking(move || Ok(state.statuses())).await
}

#[tauri::command]
pub async fn model_command(
    app: AppHandle,
    window: WebviewWindow,
    request: ModelRequest,
    state: State<'_, ModelState>,
) -> Result<Vec<ModelStatus>, String> {
    validate_request(&request, manifest::bundled()).map_err(|e| e.code())?;
    let state = state.inner().clone();
    match request.action.as_str() {
        "download" => blocking(move || start_download(&app, &state, &request)).await,
        "import" => import(app, window, state, request).await,
        "cancel" => {
            blocking(move || {
                state.cancel(&request.id);
                Ok(state.statuses())
            })
            .await
        }
        "delete" => {
            blocking(move || {
                state.delete(&request.id)?;
                Ok(state.statuses())
            })
            .await
        }
        _ => Err(ModelError::UnknownModel.code()),
    }
}

#[cfg(test)]
mod tests {
    use super::manifest::bundled;
    use super::*;

    fn req(id: &str, action: &str, auto_enable: bool, mirror_prefix: Option<&str>) -> ModelRequest {
        ModelRequest {
            id: id.into(),
            action: action.into(),
            auto_enable,
            mirror_prefix: mirror_prefix.map(str::to_owned),
        }
    }

    #[test]
    fn request_parses_camel_case_with_defaults() {
        let r: ModelRequest =
            serde_json::from_value(serde_json::json!({ "id": "stemgenrt-hop128", "action": "download" })).unwrap();
        assert!(!r.auto_enable && r.mirror_prefix.is_none());
        let r: ModelRequest = serde_json::from_value(
            serde_json::json!({ "id": "x", "action": "import", "autoEnable": true, "mirrorPrefix": "https://p/" }),
        )
        .unwrap();
        assert!(r.auto_enable);
        assert_eq!(r.mirror_prefix.as_deref(), Some("https://p/"));
    }

    #[test]
    fn validate_request_rejects_unknown_ids_actions_and_bad_prefixes() {
        let m = bundled();
        assert_eq!(validate_request(&req("x", "download", false, None), m), Err(ModelError::UnknownModel));
        assert_eq!(validate_request(&req(STEMGENRT_ID, "frobnicate", false, None), m), Err(ModelError::UnknownModel));
        assert_eq!(
            validate_request(&req(STEMGENRT_ID, "download", false, Some("http://p/")), m),
            Err(ModelError::InvalidPrefix)
        );
        // An empty prefix is "not filled in".
        assert_eq!(validate_request(&req(STEMGENRT_ID, "download", false, Some("")), m), Ok(()));
        for action in ["download", "cancel", "import", "delete"] {
            assert_eq!(validate_request(&req(STEMGENRT_ID, action, false, Some("https://p.example/")), m), Ok(()));
        }
    }

    #[test]
    fn auto_enable_only_for_stemgenrt() {
        assert!(wants_auto_enable(&req(STEMGENRT_ID, "download", true, None)));
        assert!(!wants_auto_enable(&req("other", "download", true, None)));
        assert!(!wants_auto_enable(&req(STEMGENRT_ID, "download", false, None)));
    }

    #[test]
    fn installed_model_needs_no_job_and_enables_only_on_request() {
        // Not installed: start the job (import: open the dialog), whatever was asked.
        assert_eq!(next_step(&req(STEMGENRT_ID, "download", true, None), false), Next::Start);
        assert_eq!(next_step(&req(STEMGENRT_ID, "import", false, None), false), Next::Start);
        // Installed + auto-enable: `begin_*` would finish at once without `on_installed`.
        assert_eq!(next_step(&req(STEMGENRT_ID, "download", true, None), true), Next::EnableNow);
        assert_eq!(next_step(&req(STEMGENRT_ID, "import", true, None), true), Next::EnableNow);
        // Installed, no auto-enable (or not StemgenRT): nothing to do, no dialog.
        assert_eq!(next_step(&req(STEMGENRT_ID, "download", false, None), true), Next::Nothing);
        assert_eq!(next_step(&req(STEMGENRT_ID, "import", false, None), true), Next::Nothing);
        assert_eq!(next_step(&req("other", "import", true, None), true), Next::Nothing);
        // Cancel and delete never enable.
        assert_eq!(next_step(&req(STEMGENRT_ID, "cancel", true, None), true), Next::Nothing);
        assert_eq!(next_step(&req(STEMGENRT_ID, "delete", true, None), true), Next::Nothing);
    }

    #[test]
    fn dialog_slot_admits_one_and_frees_on_drop() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let first = DialogSlot::take(&FLAG).expect("free slot");
        assert!(DialogSlot::take(&FLAG).is_none(), "second dialog refused while one is open");
        drop(first);
        let again = DialogSlot::take(&FLAG).expect("freed on drop");
        // Freed on unwind too.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = again;
            panic!("dialog task panicked");
        }));
        assert!(r.is_err());
        assert!(DialogSlot::take(&FLAG).is_some());
    }

    #[test]
    fn empty_mirror_prefix_counts_as_none() {
        assert_eq!(custom_prefix(&req(STEMGENRT_ID, "download", false, Some(""))), None);
        assert_eq!(custom_prefix(&req(STEMGENRT_ID, "download", false, None)), None);
        assert_eq!(custom_prefix(&req(STEMGENRT_ID, "download", false, Some("https://p/"))), Some("https://p/"));
    }

    #[test]
    fn each_download_job_gets_its_own_client() {
        // reqwest reads the system/env proxy when a client is built; a shared client would ignore
        // a proxy the user turned on after a failure until the app restarts.
        let a = fetcher().expect("client builds");
        let b = fetcher().expect("client builds");
        assert!(!Arc::ptr_eq(&a, &b), "two jobs must not share one client");
    }

    #[test]
    fn a_client_that_fails_to_build_is_network_unreachable() {
        let r = new_fetcher(|| Err::<(), String>("tls backend unavailable".into()));
        assert_eq!(r.map(|_| ()), Err(ModelError::Network));
        assert_eq!(ModelError::Network.code(), "network_unreachable");
    }
}
