use super::{audio_target_from_session, MediaState, Snapshot};
use nowplaying::{MediaController, MediaSession};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Controls {
    pub session_id: String,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_previous: bool,
    pub can_next: bool,
}

impl Controls {
    pub fn unavailable(session_id: &str) -> Self {
        Self {
            session_id: session_id.into(),
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Play,
    Pause,
    Previous,
    Next,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub action: Action,
    pub session_id: String,
    pub source_id: String,
    pub track_key: String,
    pub target_generation: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ControlError {
    code: &'static str,
}
impl ControlError {
    fn new(code: &'static str) -> Self {
        Self { code }
    }
}

impl From<nowplaying::Error> for ControlError {
    fn from(error: nowplaying::Error) -> Self {
        Self::new(match error {
            nowplaying::Error::SessionNotFound { .. } => "stale_target",
            nowplaying::Error::UnsupportedOperation { .. } => "unsupported",
            nowplaying::Error::UnsupportedPlatform
            | nowplaying::Error::BackendUnavailable { .. } => "unavailable",
            _ => "failed",
        })
    }
}

#[derive(Default)]
pub struct TransportState(std::sync::Arc<tokio::sync::Mutex<()>>);

impl TransportState {
    async fn run_guarded<F>(&self, operation: F, wait: Duration) -> Result<(), ControlError>
    where
        F: std::future::Future<Output = Result<(), ControlError>> + Send + 'static,
    {
        let guard = self
            .0
            .clone()
            .try_lock_owned()
            .map_err(|_| ControlError::new("busy"))?;
        let task = tokio::spawn(async move {
            let _guard = guard;
            operation.await
        });
        // Dropping a JoinHandle detaches it; it does not cancel the native
        // operation. Keep its owned gate until it really completes, even if
        // the caller times out or closes its window.
        tokio::time::timeout(wait, task)
            .await
            .map_err(|_| ControlError::new("timeout"))?
            .map_err(|_| ControlError::new("failed"))?
    }
}

trait Backend {
    async fn current(&self) -> Result<Option<MediaSession>, ControlError>;
    async fn sessions(&self) -> Result<Vec<MediaSession>, ControlError>;
    async fn send(&self, action: Action, id: &str) -> Result<(), ControlError>;
}

struct NativeBackend(MediaController);
impl Backend for NativeBackend {
    async fn current(&self) -> Result<Option<MediaSession>, ControlError> {
        self.0.current().await.map_err(Into::into)
    }
    async fn sessions(&self) -> Result<Vec<MediaSession>, ControlError> {
        self.0.sessions().await.map_err(Into::into)
    }
    async fn send(&self, action: Action, id: &str) -> Result<(), ControlError> {
        match action {
            Action::Play => self.0.play(id).await,
            Action::Pause => self.0.pause(id).await,
            Action::Previous => self.0.previous(id).await,
            Action::Next => self.0.next(id).await,
        }
        .map_err(Into::into)
    }
}

fn validate_snapshot(snapshot: &Snapshot, request: &Request) -> Result<(), ControlError> {
    let stale = || ControlError::new("stale_target");
    if snapshot.status != "ready"
        || snapshot.target_generation != request.target_generation
        || super::now_ms().saturating_sub(snapshot.captured_at_ms) > 8_000
    {
        return Err(stale());
    }
    let track = snapshot.track.as_ref().ok_or_else(stale)?;
    let key = serde_json::to_string(&[&track.source_id, &track.title, &track.artist, &track.album])
        .map_err(|_| stale())?;
    if request.session_id.is_empty()
        || request.source_id.is_empty()
        || request.session_id != track.transport.session_id
        || request.source_id != track.source_id
        || request.track_key != key
    {
        return Err(stale());
    }
    let enabled = match request.action {
        Action::Play => track.transport.can_play,
        Action::Pause => track.transport.can_pause,
        Action::Previous => track.transport.can_previous,
        Action::Next => track.transport.can_next,
    };
    if !enabled {
        return Err(ControlError::new("unsupported"));
    }
    Ok(())
}

fn matches_request(session: &MediaSession, request: &Request) -> bool {
    session.id == request.session_id
        && audio_target_from_session(session).is_some_and(|target| {
            target.source_id == request.source_id && target.track_key == request.track_key
        })
}

async fn execute(
    state: &MediaState,
    request: &Request,
    backend: &impl Backend,
) -> Result<(), ControlError> {
    // Discovery is read-only and can safely time out before any write starts.
    tokio::time::timeout(
        Duration::from_secs(3),
        validate_native(state, request, backend),
    )
    .await
    .map_err(|_| ControlError::new("timeout"))??;
    backend.send(request.action, &request.session_id).await
}

async fn validate_native(
    state: &MediaState,
    request: &Request,
    backend: &impl Backend,
) -> Result<(), ControlError> {
    validate_snapshot(&state.snapshot(), request)?;
    let sessions = backend.sessions().await?;
    let matching: Vec<_> = sessions
        .iter()
        .filter(|session| session.id == request.session_id)
        .collect();
    if matching.len() > 1 {
        return Err(ControlError::new("ambiguous_target"));
    }
    if !matching
        .first()
        .is_some_and(|session| matches_request(session, request))
    {
        return Err(ControlError::new("stale_target"));
    }
    if !backend
        .current()
        .await?
        .as_ref()
        .is_some_and(|session| matches_request(session, request))
    {
        return Err(ControlError::new("stale_target"));
    }
    // The read awaits may have overlapped a source/track change in the watcher.
    validate_snapshot(&state.snapshot(), request)?;
    Ok(())
}

#[tauri::command]
pub async fn control_media(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, MediaState>,
    transport: tauri::State<'_, TransportState>,
    request: Request,
) -> Result<(), ControlError> {
    if window.label() != "main" {
        return Err(ControlError::new("unavailable"));
    }
    let shared = state.inner().clone();
    transport
        .run_guarded(
            async move {
                let backend = NativeBackend(
                    tokio::time::timeout(Duration::from_secs(3), MediaController::new())
                        .await
                        .map_err(|_| ControlError::new("timeout"))??,
                );
                execute(&shared, &request, &backend).await
            },
            Duration::from_secs(5),
        )
        .await
}

#[cfg(windows)]
pub async fn capabilities(id: &str) -> Option<Controls> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager;
    let manager = Manager::RequestAsync().ok()?.await.ok()?;
    let sessions = manager.GetSessions().ok()?;
    let mut selected = None;
    for index in 0..sessions.Size().ok()? {
        let session = sessions.GetAt(index).ok()?;
        if session.SourceAppUserModelId().ok()?.to_string() == id {
            // The upstream Windows backend addresses by AUMID, not by instance.
            // Ambiguous instances must not inherit first-match write semantics.
            if selected.is_some() {
                return None;
            }
            selected = Some(session);
        }
    }
    let controls = selected?.GetPlaybackInfo().ok()?.Controls().ok()?;
    Some(Controls {
        session_id: id.into(),
        can_play: controls.IsPlayEnabled().ok()?,
        can_pause: controls.IsPauseEnabled().ok()?,
        can_previous: controls.IsPreviousEnabled().ok()?,
        can_next: controls.IsNextEnabled().ok()?,
    })
}

#[cfg(not(windows))]
pub async fn capabilities(_: &str) -> Option<Controls> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn response_timeout_keeps_the_gate_until_the_native_operation_finishes() {
        let transport = TransportState::default();
        let (finish, pending_native) = tokio::sync::oneshot::channel::<()>();
        assert_eq!(
            transport
                .run_guarded(
                    async move {
                        let _ = pending_native.await;
                        Ok(())
                    },
                    Duration::from_millis(10)
                )
                .await,
            Err(ControlError::new("timeout"))
        );
        assert_eq!(
            transport
                .run_guarded(async { Ok(()) }, Duration::from_secs(1))
                .await,
            Err(ControlError::new("busy"))
        );
        finish.send(()).unwrap();
        let guard = tokio::time::timeout(Duration::from_secs(1), transport.0.lock())
            .await
            .unwrap();
        drop(guard);
        assert_eq!(
            transport
                .run_guarded(async { Ok(()) }, Duration::from_secs(1))
                .await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_target_that_left_and_returned_rejects_its_old_request() {
        for invalidation in ["song", "offline"] {
            let (state, request, backend) = fixture(Action::Next);
            let original = state.snapshot();
            if invalidation == "song" {
                let mut other = original.clone();
                other.track.as_mut().unwrap().title = "Other".into();
                state.set(other);
            } else {
                state.set(Snapshot::empty("idle"));
            }
            state.set(original);
            assert_eq!(
                execute(&state, &request, &backend).await,
                Err(ControlError::new("stale_target"))
            );
            assert!(backend.sent.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn playback_progress_cover_and_display_name_do_not_invalidate_the_target() {
        let (state, request, backend) = fixture(Action::Next);
        let mut snapshot = state.snapshot();
        let track = snapshot.track.as_mut().unwrap();
        track.position_seconds = 20.0;
        track.playback_status = "paused";
        track.artwork_data_url = Some("delayed cover".into());
        track.source = "Friendly name".into();
        state.set(snapshot);
        assert_eq!(
            state.snapshot().target_generation,
            request.target_generation
        );
        assert_eq!(execute(&state, &request, &backend).await, Ok(()));
    }

    struct Fake {
        current: Option<MediaSession>,
        sessions: Vec<MediaSession>,
        sent: Mutex<Vec<(Action, String)>>,
        error: Option<&'static str>,
    }
    impl Backend for Fake {
        async fn current(&self) -> Result<Option<MediaSession>, ControlError> {
            Ok(self.current.clone())
        }
        async fn sessions(&self) -> Result<Vec<MediaSession>, ControlError> {
            Ok(self.sessions.clone())
        }
        async fn send(&self, action: Action, id: &str) -> Result<(), ControlError> {
            self.sent.lock().unwrap().push((action, id.into()));
            self.error
                .map_or(Ok(()), |code| Err(ControlError::new(code)))
        }
    }
    fn fixture(action: Action) -> (MediaState, Request, Fake) {
        let session = super::super::tests::session("player.exe", None);
        let mut track = super::super::track_from(&session).unwrap();
        track.transport = Controls {
            session_id: "player.exe".into(),
            can_play: true,
            can_pause: true,
            can_previous: true,
            can_next: true,
        };
        let state = MediaState::default();
        state.set(Snapshot {
            status: "ready",
            track: Some(track),
            captured_at_ms: super::super::now_ms(),
            target_generation: 0,
        });
        let request = Request {
            target_generation: state.snapshot().target_generation,
            action,
            session_id: "player.exe".into(),
            source_id: "player.exe".into(),
            track_key: r#"["player.exe","Example","",""]"#.into(),
        };
        let backend = Fake {
            current: Some(session.clone()),
            sessions: vec![session],
            sent: Mutex::new(vec![]),
            error: None,
        };
        (state, request, backend)
    }

    #[tokio::test]
    async fn routes_each_action_once_to_the_verified_session() {
        for action in [Action::Play, Action::Pause, Action::Previous, Action::Next] {
            let (state, request, backend) = fixture(action);
            assert_eq!(execute(&state, &request, &backend).await, Ok(()));
            assert_eq!(
                *backend.sent.lock().unwrap(),
                vec![(action, "player.exe".into())]
            );
        }
    }

    #[tokio::test]
    async fn stale_or_mismatched_requests_never_reach_the_player() {
        for condition in [
            "source",
            "session",
            "song",
            "expired",
            "missing",
            "unavailable",
        ] {
            let (state, mut request, backend) = fixture(Action::Next);
            match condition {
                "source" => request.source_id = "other.exe".into(),
                "session" => request.session_id = "other.exe".into(),
                "song" => request.track_key = "old song".into(),
                "expired" => {
                    let mut snapshot = state.snapshot();
                    snapshot.captured_at_ms -= 8001;
                    state.set(snapshot);
                }
                "missing" => state.set(Snapshot::empty("idle")),
                _ => state.set(Snapshot::empty("unavailable")),
            }
            assert_eq!(
                execute(&state, &request, &backend).await,
                Err(ControlError::new("stale_target")),
                "{condition}"
            );
            assert!(backend.sent.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn changed_native_selection_or_duplicate_ids_fail_closed() {
        for condition in ["current", "song", "gone", "duplicate"] {
            let (state, request, mut backend) = fixture(Action::Next);
            match condition {
                "current" => {
                    backend.current = Some(super::super::tests::session("other.exe", None))
                }
                "song" => {
                    backend
                        .current
                        .as_mut()
                        .unwrap()
                        .track
                        .as_mut()
                        .unwrap()
                        .title = "New song".into()
                }
                "gone" => backend.sessions.clear(),
                _ => backend.sessions.push(backend.sessions[0].clone()),
            }
            let code = if condition == "duplicate" {
                "ambiguous_target"
            } else {
                "stale_target"
            };
            assert_eq!(
                execute(&state, &request, &backend).await,
                Err(ControlError::new(code)),
                "{condition}"
            );
            assert!(backend.sent.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn unsupported_action_does_not_dispatch_and_native_errors_are_not_retried() {
        let (state, request, mut backend) = fixture(Action::Next);
        let mut snapshot = state.snapshot();
        snapshot.track.as_mut().unwrap().transport.can_next = false;
        state.set(snapshot);
        assert_eq!(
            execute(&state, &request, &backend).await,
            Err(ControlError::new("unsupported"))
        );
        assert!(backend.sent.lock().unwrap().is_empty());
        let (state, request, _) = fixture(Action::Next);
        backend.error = Some("failed");
        assert_eq!(
            execute(&state, &request, &backend).await,
            Err(ControlError::new("failed"))
        );
        assert_eq!(backend.sent.lock().unwrap().len(), 1);
    }
}
