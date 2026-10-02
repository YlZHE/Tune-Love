use tauri::{AppHandle, Manager, State, WebviewWindow, WebviewWindowBuilder};

#[derive(Default)]
pub struct SettingsWindowState(tokio::sync::Mutex<()>);

// Only the main window can request this one locally configured settings page.
// Serializing creation makes even concurrent requests reuse the same window.
#[tauri::command]
pub async fn open_settings(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, SettingsWindowState>,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("Settings can only be opened from the main window".into());
    }
    let _guard = state.0.lock().await;
    let settings = match app.get_webview_window("settings") {
        Some(settings) => settings,
        None => {
            let config = app
                .config()
                .app
                .windows
                .iter()
                .find(|config| config.label == "settings")
                .ok_or("Settings window configuration is missing")?;
            WebviewWindowBuilder::from_config(&app, config)
                .and_then(|builder| builder.parent(&window))
                .and_then(|builder| builder.build())
                .map_err(|error| error.to_string())?
        }
    };
    settings.unminimize().map_err(|error| error.to_string())?;
    settings.show().map_err(|error| error.to_string())?;
    settings.set_focus().map_err(|error| error.to_string())
}
