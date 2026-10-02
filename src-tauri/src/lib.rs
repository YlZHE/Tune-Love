pub mod audio;
pub mod autotune;
pub mod key_detection;
pub mod media;
mod settings;
pub mod source;

pub fn run() {
    let media = media::MediaState::default();
    let audio = audio::AudioState::new(media.clone());
    let key_detection = key_detection::KeyDetectionState::new(audio.clone());
    tauri::Builder::default()
        .manage(media)
        .manage(media::transport::TransportState::default())
        .manage(audio)
        .manage(key_detection)
        .manage(settings::SettingsWindowState::default())
        .manage(autotune::AutotuneState::default())
        .setup(|app| {
            use tauri::Manager;
            app.state::<audio::AudioState>().start();
            // Per-song Key/Scale memory (local only). Without a data dir the
            // feature is simply off; analysis is unaffected.
            if let Ok(dir) = app.path().app_local_data_dir() {
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
            autotune::autotune_command
        ])
        .build(tauri::generate_context!())
        .expect("Could not start Tune Love")
        .run(|app, event| {
            use tauri::Manager;
            if matches!(event, tauri::RunEvent::Exit) {
                app.state::<key_detection::KeyDetectionState>().stop();
                app.state::<audio::AudioState>().stop();
                app.state::<autotune::AutotuneState>().stop();
            }
        });
}
