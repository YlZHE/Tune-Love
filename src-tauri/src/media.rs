use nowplaying::{MediaController, MediaSession, PlaybackStatus};
pub mod transport;
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    title: String,
    artist: String,
    album: String,
    artwork_data_url: Option<String>,
    position_seconds: f64,
    duration_seconds: f64,
    playback_status: &'static str,
    playback_rate: f64,
    source: String,
    source_id: String,
    source_icon_data_url: Option<String>,
    updated_at_ms: u64,
    transport: transport::Controls,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub status: &'static str,
    pub track: Option<Track>,
    pub captured_at_ms: u64,
    pub target_generation: u64,
}

impl Snapshot {
    fn empty(status: &'static str) -> Self {
        Self {
            status,
            track: None,
            captured_at_ms: now_ms(),
            target_generation: 0,
        }
    }
}

#[derive(Clone)]
pub struct MediaState(Arc<Mutex<Snapshot>>);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTarget {
    pub source_id: String,
    pub track_key: String,
    pub playing: bool,
    pub target_generation: u64,
}

impl Default for MediaState {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Snapshot::empty("loading"))))
    }
}

impl MediaState {
    pub fn audio_target(&self) -> Option<AudioTarget> {
        let snapshot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if snapshot.status != "ready" || now_ms().saturating_sub(snapshot.captured_at_ms) > 8_000 {
            return None;
        }
        let track = snapshot.track.as_ref()?;
        let key = serde_json::to_string(&[
            track.source_id.as_str(),
            track.title.as_str(),
            track.artist.as_str(),
            track.album.as_str(),
        ])
        .ok()?;
        Some(AudioTarget {
            source_id: track.source_id.clone(),
            track_key: key,
            playing: track.playback_status == "playing",
            target_generation: snapshot.target_generation,
        })
    }
    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set(&self, mut value: Snapshot) {
        let mut previous = self.0.lock().unwrap_or_else(|e| e.into_inner());
        value.target_generation = previous.target_generation;
        if target_identity(&previous) != target_identity(&value) {
            value.target_generation = value.target_generation.saturating_add(1);
        }
        *previous = value;
    }
}

fn target_identity(snapshot: &Snapshot) -> Option<(&str, &str, &str, &str, &str)> {
    if snapshot.status != "ready" {
        return None;
    }
    let track = snapshot.track.as_ref()?;
    Some((
        &track.transport.session_id,
        &track.source_id,
        &track.title,
        &track.artist,
        &track.album,
    ))
}

#[tauri::command]
pub fn get_now_playing(state: tauri::State<'_, MediaState>) -> Snapshot {
    state.snapshot()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn display_source(session: &MediaSession) -> String {
    crate::source::fallback_name(&session.source.id, session.source.name.as_deref())
}

fn track_from(session: &MediaSession) -> Option<Track> {
    let track = session.track.as_ref()?;
    if track.title.trim().is_empty() {
        return None;
    }
    Some(Track {
        title: track.title.clone(),
        artist: track
            .artist
            .clone()
            .or_else(|| track.album_artist.clone())
            .unwrap_or_default(),
        album: track.album.clone().unwrap_or_default(),
        artwork_data_url: None,
        position_seconds: session.playback.position_ms as f64 / 1000.0,
        duration_seconds: track.duration_ms.unwrap_or(0) as f64 / 1000.0,
        playback_status: match session.playback.status {
            PlaybackStatus::Playing => "playing",
            PlaybackStatus::Paused => "paused",
            PlaybackStatus::Stopped => "stopped",
            PlaybackStatus::Unknown => "unknown",
        },
        playback_rate: if session.playback.playback_rate.is_finite() {
            session.playback.playback_rate
        } else {
            1.0
        },
        source: display_source(session),
        source_id: session.source.id.clone(),
        source_icon_data_url: None,
        updated_at_ms: now_ms(),
        transport: transport::Controls::unavailable(&session.id),
    })
}

// The open-source nowplaying crate owns discovery, metadata, state and clock correction.
// Artwork and native application identity are adapted without changing its selection rules.
pub async fn watch(state: MediaState) {
    let mut sources = crate::source::SourceCache::default();
    loop {
        let controller =
            match tokio::time::timeout(Duration::from_secs(4), MediaController::new()).await {
                Ok(Ok(controller)) => controller,
                _ => {
                    state.set(Snapshot::empty("unavailable"));
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
        let mut cache_key = None;
        let mut cover = None;
        let mut cover_checked = 0;
        let mut timer = tokio::time::interval(Duration::from_millis(700));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            let session =
                match tokio::time::timeout(Duration::from_secs(3), controller.current()).await {
                    Ok(Ok(session)) => session,
                    _ => {
                        state.set(Snapshot::empty("unavailable"));
                        break;
                    }
                };
            let Some(session) = session else {
                cache_key = None;
                cover = None;
                state.set(Snapshot::empty("idle"));
                continue;
            };
            let Some(mut track) = track_from(&session) else {
                cache_key = None;
                cover = None;
                state.set(Snapshot::empty("idle"));
                continue;
            };
            track.transport = tokio::time::timeout(
                Duration::from_millis(500),
                transport::capabilities(&session.id),
            )
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| transport::Controls::unavailable(&session.id));
            let source = sources.current(&track.source_id, &track.source).await;
            track.source = source.name;
            track.source_icon_data_url = source.icon_data_url;
            let key = (
                session.id.clone(),
                track.title.clone(),
                track.artist.clone(),
                track.album.clone(),
            );
            let changed = cache_key.as_ref() != Some(&key);
            if changed {
                cover = None;
                cache_key = Some(key);
            }
            track.artwork_data_url = cover.clone();
            let captured_at_ms = track.updated_at_ms;
            state.set(Snapshot {
                status: "ready",
                track: Some(track.clone()),
                captured_at_ms,
                target_generation: 0,
            });
            // Recheck missing/delayed covers, without reading artwork on every timeline update.
            let retry_after = if cover.is_some() { 15000 } else { 3000 };
            if changed || now_ms().saturating_sub(cover_checked) >= retry_after {
                cover_checked = now_ms();
                cover = tokio::time::timeout(
                    Duration::from_secs(2),
                    artwork(&session.id, &track.title),
                )
                .await
                .ok()
                .flatten();
                track.artwork_data_url = cover.clone();
                state.set(Snapshot {
                    status: "ready",
                    track: Some(track),
                    captured_at_ms,
                    target_generation: 0,
                });
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn image_data_url(bytes: &[u8]) -> Option<String> {
    use base64::Engine;
    if bytes.len() > 8 * 1024 * 1024 {
        return None;
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        return None;
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

pub(crate) fn audio_target_from_session(session: &MediaSession) -> Option<AudioTarget> {
    let track = track_from(session)?;
    Some(AudioTarget {
        source_id: track.source_id.clone(),
        track_key: serde_json::to_string(&[
            &track.source_id,
            &track.title,
            &track.artist,
            &track.album,
        ])
        .ok()?,
        playing: track.playback_status == "playing",
        target_generation: 0,
    })
}

#[cfg(windows)]
async fn artwork(source_id: &str, title: &str) -> Option<String> {
    use windows::{
        Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager,
        Storage::Streams::{DataReader, InputStreamOptions},
    };
    let manager = Manager::RequestAsync().ok()?.await.ok()?;
    let sessions = manager.GetSessions().ok()?;
    for i in 0..sessions.Size().ok()? {
        let Ok(session) = sessions.GetAt(i) else {
            continue;
        };
        if session.SourceAppUserModelId().ok()?.to_string() != source_id {
            continue;
        }
        let properties = session.TryGetMediaPropertiesAsync().ok()?.await.ok()?;
        // Avoid showing the next song's cover beside metadata from the previous one.
        if properties.Title().ok()?.to_string() != title {
            return None;
        }
        let stream = properties
            .Thumbnail()
            .ok()?
            .OpenReadAsync()
            .ok()?
            .await
            .ok()?;
        let size = stream.Size().ok()?;
        if size == 0 || size > 8 * 1024 * 1024 {
            let _ = stream.Close();
            return None;
        }
        let input = stream.GetInputStreamAt(0).ok()?;
        let reader = DataReader::CreateDataReader(&input).ok()?;
        reader
            .SetInputStreamOptions(InputStreamOptions::None)
            .ok()?;
        let result = async {
            let loaded = reader.LoadAsync(size as u32).ok()?.await.ok()?;
            if loaded as u64 != size {
                return None;
            }
            let mut bytes = vec![0; loaded as usize];
            reader.ReadBytes(&mut bytes).ok()?;
            image_data_url(&bytes)
        }
        .await;
        let _ = reader.Close();
        let _ = stream.Close();
        return result;
    }
    None
}

#[cfg(not(windows))]
async fn artwork(_: &str, _: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nowplaying::{MediaSource, MediaTrack, PlaybackState};
    #[test]
    fn audio_target_uses_frontend_identity_and_rejects_expired_metadata() {
        let state = MediaState::default();
        let mut track = track_from(&session("player.exe", None)).unwrap();
        track.playback_status = "playing";
        state.set(Snapshot {
            status: "ready",
            track: Some(track.clone()),
            captured_at_ms: now_ms(),
            target_generation: 0,
        });
        let target = state.audio_target().unwrap();
        assert_eq!(target.track_key, r#"["player.exe","Example","",""]"#);
        assert!(target.playing);
        assert_eq!(target.target_generation, state.snapshot().target_generation);
        state.set(Snapshot {
            status: "ready",
            track: Some(track),
            captured_at_ms: now_ms() - 8001,
            target_generation: 0,
        });
        assert!(state.audio_target().is_none());
    }

    #[test]
    fn target_generation_rejects_observed_a_to_b_to_a_identity_reuse() {
        let state = MediaState::default();
        let mut a = track_from(&session("player.exe", None)).unwrap();
        a.playback_status = "playing";
        state.set(Snapshot {
            status: "ready",
            track: Some(a.clone()),
            captured_at_ms: now_ms(),
            target_generation: 0,
        });
        let first_a = state.audio_target().unwrap();

        let mut b = a.clone();
        b.title = "Other".into();
        state.set(Snapshot {
            status: "ready",
            track: Some(b),
            captured_at_ms: now_ms(),
            target_generation: 0,
        });
        let observed_b = state.audio_target().unwrap();

        state.set(Snapshot {
            status: "ready",
            track: Some(a),
            captured_at_ms: now_ms(),
            target_generation: 0,
        });
        let second_a = state.audio_target().unwrap();

        assert_eq!(first_a.source_id, second_a.source_id);
        assert_eq!(first_a.track_key, second_a.track_key);
        assert!(first_a.target_generation < observed_b.target_generation);
        assert!(observed_b.target_generation < second_a.target_generation);
        assert_ne!(first_a, second_a);
    }

    pub(super) fn session(id: &str, name: Option<&str>) -> MediaSession {
        MediaSession {
            id: id.into(),
            source: MediaSource {
                id: id.into(),
                name: name.map(str::to_owned),
            },
            track: Some(MediaTrack {
                title: "Example".into(),
                artist: None,
                album: None,
                album_artist: None,
                duration_ms: Some(180000),
                track_number: None,
                genres: vec![],
            }),
            playback: PlaybackState::default(),
        }
    }

    #[test]
    fn desktop_source_names_omit_paths_and_exe_extensions() {
        for id in ["Folia.exe", r"C:\Apps\Folia\Folia.EXE"] {
            assert_eq!(display_source(&session(id, Some(id))), "Folia");
        }
    }

    #[test]
    fn unrecognized_source_ids_do_not_leak_internal_identifiers() {
        assert_eq!(
            display_source(&session("top.vendor.player", Some("top.vendor.player"))),
            "媒体播放器"
        );
        assert_eq!(display_source(&session("", Some("  "))), "媒体播放器");
    }

    #[test]
    fn media_snapshots_include_stable_source_identity_and_nullable_icon() {
        let track = track_from(&session("Folia.exe", Some("Folia"))).unwrap();
        let json = serde_json::to_value(track).unwrap();
        assert_eq!(json["sourceId"], "Folia.exe");
        assert!(json.as_object().unwrap().contains_key("sourceIconDataUrl"));
        assert!(json["sourceIconDataUrl"].is_null());
    }

    #[test]
    fn unprobed_sessions_do_not_advertise_transport_actions() {
        let track = track_from(&session("Folia.exe", Some("Folia"))).unwrap();
        let json = serde_json::to_value(track).unwrap();
        assert_eq!(json["transport"]["sessionId"], "Folia.exe");
        for capability in ["canPlay", "canPause", "canPrevious", "canNext"] {
            assert_eq!(json["transport"][capability], false);
        }
    }
}
