pub mod audio;
pub mod autotune;
pub mod devocal;
pub mod key_detection;
pub mod media;
mod migrate;
mod settings;
pub mod source;

pub fn run() {
    let media = media::MediaState::default();
    // One gate: devocal publishes the hold attenuation, key-detection capture undoes it.
    let gate = std::sync::Arc::new(devocal::gate::AttenuationGate::new());
    let audio = audio::AudioState::new(media.clone(), gate.clone());
    let key_detection = key_detection::KeyDetectionState::new(audio.clone());
    let devocal = devocal::DevocalState::new(gate);
    tauri::Builder::default()
        // First plugin, as the plugin requires. A second launch exits here, before setup:
        // otherwise its startup and periodic restore would undo this instance's player hold
        // and delete its restore record (both share `devocal-restore.json`).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .manage(media)
        .manage(media::transport::TransportState::default())
        .manage(audio)
        .manage(key_detection)
        .manage(settings::SettingsWindowState::default())
        .manage(autotune::AutotuneState::default())
        .manage(devocal)
        .setup(|app| {
            use tauri::Manager;
            // First: give back any player volume a crashed app/engine left lowered.
            let data_dir = app.path().app_local_data_dir().ok();
            if let Some(dir) = &data_dir {
                devocal::restore_at_startup(&devocal::restore_file(dir));
            }
            app.state::<audio::AudioState>().start();
            // Installed builds ship the bridge and Python under the resource dir.
            if let Ok(resource_dir) = app.path().resource_dir() {
                app.state::<autotune::AutotuneState>()
                    .set_resource_dir(resource_dir);
            }
            // Per-song Key/Scale memory (local only). Without a data dir the
            // feature is simply off; analysis is unaffected.
            if let Ok(dir) = app.path().app_local_data_dir() {
                // The app was renamed; carry the old data folder's cache over.
                if let migrate::Migration::Failed(e) = migrate::migrate_song_cache(&dir) {
                    eprintln!("song cache migration failed: {e}");
                }
                app.state::<key_detection::KeyDetectionState>()
                    .use_song_cache(dir.join("song-keys-v1.json"));
            }
            app.state::<key_detection::KeyDetectionState>().start();
            let shared = app.state::<media::MediaState>().inner().clone();
            std::thread::Builder::new()
                .name("media-session-worker".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("media worker runtime");
                    runtime.block_on(media::watch(shared));
                })
                .expect("media worker thread");
            let media = app.state::<media::MediaState>().inner().clone();
            app.state::<devocal::DevocalState>().start(data_dir, media);
            Ok(())
        })
        .on_window_event(|window, event| {
            use tauri::Manager;
            if window.label() == "settings" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            } else if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed) {
                window.state::<key_detection::KeyDetectionState>().stop();
                window.state::<audio::AudioState>().stop();
                window.state::<autotune::AutotuneState>().stop();
                window.state::<devocal::DevocalState>().shutdown();
                // A hidden settings window must never keep the app alive after closing the widget.
                window.app_handle().exit(0);
            }
        })
        .invoke_handler(tauri::generate_handler![
            media::get_now_playing,
            media::transport::control_media,
            audio::get_audio_level,
            key_detection::get_key_detection,
            settings::open_settings,
            autotune::autotune_command,
            devocal::get_devocal_status,
            devocal::devocal_command
        ])
        .build(tauri::generate_context!())
        .expect("Could not start Tune Love")
        .run(|app, event| {
            use tauri::Manager;
            if matches!(event, tauri::RunEvent::Exit) {
                app.state::<key_detection::KeyDetectionState>().stop();
                app.state::<audio::AudioState>().stop();
                app.state::<autotune::AutotuneState>().stop();
                app.state::<devocal::DevocalState>().shutdown();
            }
        });
}
