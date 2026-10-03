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
use std::sync::{Arc, OnceLock};

use serde::Deserialize;
use tauri::{AppHandle, Manager, State};

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

/// Whether to enable de-vocal right away rather than on install: the model is already installed,
/// so `begin_*` would finish at once without calling `on_installed`.
fn enable_now(req: &ModelRequest, already_installed: bool) -> bool {
    already_installed && wants_auto_enable(req) && matches!(req.action.as_str(), "download" | "import")
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

/// One HTTP client for the app's lifetime.
fn fetcher() -> Result<Arc<ReqwestFetcher>, ModelError> {
    static FETCHER: OnceLock<Arc<ReqwestFetcher>> = OnceLock::new();
    if let Some(f) = FETCHER.get() {
        return Ok(f.clone());
    }
    let created = ReqwestFetcher::standard().map_err(|e| {
        eprintln!("models: {e}");
        ModelError::Network
    })?;
    Ok(FETCHER.get_or_init(|| Arc::new(created)).clone())
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

/// What a finished install does: enable de-vocal if the request asked for it, else nothing.
fn on_installed(app: &AppHandle, req: &ModelRequest) -> Box<dyn FnOnce() + Send> {
    if wants_auto_enable(req) {
        let app = app.clone();
        Box::new(move || enable_devocal(&app))
    } else {
        Box::new(|| {})
    }
}

/// May hash (cached).
fn is_installed(state: &ModelState, id: &str) -> bool {
    state.statuses().iter().any(|s| s.id == id && s.phase == ModelPhase::Installed)
}

/// Runs `f` (which may hash files) on a blocking thread; errors become machine codes.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ModelError> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("model command failed: {e}"))?
        .map_err(|e| e.code())
}

/// Blocking.
fn start_download(app: &AppHandle, state: &ModelState, req: &ModelRequest) -> Result<Vec<ModelStatus>, ModelError> {
    if enable_now(req, is_installed(state, &req.id)) {
        enable_devocal(app);
        return Ok(state.statuses());
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
    if enable_now(req, is_installed(state, &req.id)) {
        enable_devocal(app);
        return Ok(state.statuses());
    }
    // Still the final guard: a job may have started while the dialog was open.
    let job = state.begin_import(&req.id, file, on_installed(app, req))?;
    tauri::async_runtime::spawn(job);
    Ok(state.statuses())
}

async fn import(app: AppHandle, state: ModelState, req: ModelRequest) -> Result<Vec<ModelStatus>, String> {
    // Refused before the dialog opens, not after the user has picked a file.
    let busy = blocking({
        let state = state.clone();
        move || {
            Ok(state
                .statuses()
                .iter()
                .any(|s| matches!(s.phase, ModelPhase::Downloading | ModelPhase::Verifying)))
        }
    })
    .await?;
    if busy {
        return Err(ModelError::AlreadyRunning.code());
    }
    let picked = blocking({
        let app = app.clone();
        move || {
            use tauri_plugin_dialog::DialogExt;
            Ok(app.dialog().file().add_filter("ONNX 模型", &["onnx"]).blocking_pick_file())
        }
    })
    .await?;
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
    request: ModelRequest,
    state: State<'_, ModelState>,
) -> Result<Vec<ModelStatus>, String> {
    validate_request(&request, manifest::bundled()).map_err(|e| e.code())?;
    let state = state.inner().clone();
    match request.action.as_str() {
        "download" => blocking(move || start_download(&app, &state, &request)).await,
        "import" => import(app, state, request).await,
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
    fn enable_now_only_when_auto_enable_meets_an_installed_model() {
        // Already installed: `begin_*` would finish at once without `on_installed`.
        assert!(enable_now(&req(STEMGENRT_ID, "download", true, None), true));
        assert!(enable_now(&req(STEMGENRT_ID, "import", true, None), true));
        // Not installed yet: the install callback does it.
        assert!(!enable_now(&req(STEMGENRT_ID, "download", true, None), false));
        // No auto-enable asked, or not the StemgenRT model.
        assert!(!enable_now(&req(STEMGENRT_ID, "download", false, None), true));
        assert!(!enable_now(&req("other", "download", true, None), true));
        // Cancel and delete never enable.
        assert!(!enable_now(&req(STEMGENRT_ID, "cancel", true, None), true));
        assert!(!enable_now(&req(STEMGENRT_ID, "delete", true, None), true));
    }

    #[test]
    fn empty_mirror_prefix_counts_as_none() {
        assert_eq!(custom_prefix(&req(STEMGENRT_ID, "download", false, Some(""))), None);
        assert_eq!(custom_prefix(&req(STEMGENRT_ID, "download", false, None)), None);
        assert_eq!(custom_prefix(&req(STEMGENRT_ID, "download", false, Some("https://p/"))), Some("https://p/"));
    }
}
