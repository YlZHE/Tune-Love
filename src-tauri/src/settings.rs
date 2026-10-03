use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

#[derive(Default)]
pub struct SettingsWindowState(tokio::sync::Mutex<()>);

/// Settings sections another window may ask the page to jump to.
const SECTIONS: &[&str] = &["devocal-model"];

/// The settings page address, optionally asking it to jump to a known section. Anything else is
/// refused, so the value can be put into the address as is.
pub fn settings_url(section: Option<&str>) -> Result<String, String> {
    match section {
        None => Ok("index.html?view=settings".into()),
        Some(section) if SECTIONS.contains(&section) => Ok(format!("index.html?view=settings&section={section}")),
        Some(section) => Err(format!("Unknown settings section: {section}")),
    }
}

// Only the main window can request this one locally configured settings page.
// Serializing creation makes even concurrent requests reuse the same window.
// A new window reads `section` from its address; an open one gets it as a
// `settings-section` event.
#[tauri::command]
pub async fn open_settings(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, SettingsWindowState>,
    section: Option<String>,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("Settings can only be opened from the main window".into());
    }
    let url = settings_url(section.as_deref())?;
    let _guard = state.0.lock().await;
    let settings = match app.get_webview_window("settings") {
        Some(settings) => {
            if let Some(section) = &section {
                app.emit_to("settings", "settings-section", section)
                    .map_err(|error| error.to_string())?;
            }
            settings
        }
        None => {
            let mut config = app
                .config()
                .app
                .windows
                .iter()
                .find(|config| config.label == "settings")
                .ok_or("Settings window configuration is missing")?
                .clone();
            config.url = WebviewUrl::App(url.into());
            WebviewWindowBuilder::from_config(&app, &config)
                .and_then(|builder| builder.parent(&window))
                .and_then(|builder| builder.build())
                .map_err(|error| error.to_string())?
        }
    };
    settings.unminimize().map_err(|error| error.to_string())?;
    settings.show().map_err(|error| error.to_string())?;
    settings.set_focus().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::settings_url;

    #[test]
    fn settings_url_accepts_only_known_sections() {
        assert_eq!(settings_url(None).unwrap(), "index.html?view=settings");
        assert_eq!(
            settings_url(Some("devocal-model")).unwrap(),
            "index.html?view=settings&section=devocal-model"
        );
        assert!(settings_url(Some("other")).is_err());
        assert!(settings_url(Some("devocal-model&view=main")).is_err());
        assert!(settings_url(Some("")).is_err());
    }
}
