//! Session and endpoint notifications, so the engine reacts within milliseconds instead of at
//! the next 0.5 s `Holder::follow`.
//!
//! [`SessionWatcher`] registers an `IMMNotificationClient` on the device enumerator and an
//! `IAudioSessionNotification` on the session manager of every active render endpoint. The
//! COM callbacks run on threads of the audio service's choosing; they only set flags in
//! [`SessionSignals`] (the default-device callback also stores the new endpoint id) and never
//! touch sessions. The engine loop polls the flags every pass (1 ms), so setting one wakes it
//! within a millisecond; all session work stays on the engine thread:
//! - a new session: `Holder::follow` (and the output endpoint check) at once;
//! - a default render device change: the capture is silenced
//!   (`Holder::default_device_changed`) and a follow runs at once;
//! - endpoints added, removed or changing state: [`SessionWatcher::maintain`] (re)registers
//!   on the active endpoints, then asks for a follow (a session may have been created before
//!   the registration).
//!
//! Documented quirk: `IAudioSessionManager2` delivers `OnSessionCreated` only after
//! `GetSessionEnumerator` has been called once on that manager, so registration calls it
//! first. Every registration is undone in `Drop`, which must run before COM is uninitialised
//! on the engine thread (`run` drops the watcher before returning to `main`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{implement, Ref, Result as WinResult, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, EDataFlow, ERole, IAudioSessionControl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

/// Flags set by the notification callbacks and taken by the engine loop.
#[derive(Debug, Default)]
pub struct SessionSignals {
    session_created: AtomicBool,
    endpoints_changed: AtomicBool,
    /// The latest default render device change: `Some(new endpoint id)`; the inner `None`
    /// means there is no default render device any more (or its id was unreadable).
    default_changed: Mutex<Option<Option<String>>>,
}

impl SessionSignals {
    /// A session was created on some render endpoint.
    pub fn notify_session_created(&self) {
        self.session_created.store(true, Ordering::Release);
    }

    /// Render endpoints were added or removed, or changed state.
    pub fn notify_endpoints_changed(&self) {
        self.endpoints_changed.store(true, Ordering::Release);
    }

    /// The default render device changed to `endpoint`.
    pub fn notify_default_changed(&self, endpoint: Option<String>) {
        match self.default_changed.lock() {
            Ok(mut g) => *g = Some(endpoint),
            Err(p) => *p.into_inner() = Some(endpoint),
        }
    }

    pub fn take_session_created(&self) -> bool {
        self.session_created.swap(false, Ordering::AcqRel)
    }

    pub fn take_endpoints_changed(&self) -> bool {
        self.endpoints_changed.swap(false, Ordering::AcqRel)
    }

    /// The latest default render device change since the last call, if any.
    pub fn take_default_changed(&self) -> Option<Option<String>> {
        match self.default_changed.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        }
    }
}

#[implement(IAudioSessionNotification)]
struct SessionCallback {
    signals: Arc<SessionSignals>,
}

impl IAudioSessionNotification_Impl for SessionCallback_Impl {
    fn OnSessionCreated(&self, _new_session: Ref<IAudioSessionControl>) -> WinResult<()> {
        self.signals.notify_session_created();
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct EndpointCallback {
    signals: Arc<SessionSignals>,
}

impl IMMNotificationClient_Impl for EndpointCallback_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> WinResult<()> {
        self.signals.notify_endpoints_changed();
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> WinResult<()> {
        self.signals.notify_endpoints_changed();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> WinResult<()> {
        self.signals.notify_endpoints_changed();
        Ok(())
    }

    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, id: &PCWSTR) -> WinResult<()> {
        // Media players follow the console/multimedia default; a change of the
        // communications default does not move them.
        if flow == eRender && (role == eConsole || role == eMultimedia) {
            let endpoint = if id.is_null() {
                None
            } else {
                unsafe { id.to_string() }.ok()
            };
            self.signals.notify_default_changed(endpoint);
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> WinResult<()> {
        Ok(())
    }
}

/// The notification registrations. COM (MTA) must be initialised on the thread that creates,
/// maintains and drops it.
pub struct SessionWatcher {
    enumerator: IMMDeviceEnumerator,
    endpoint_cb: IMMNotificationClient,
    session_cb: IAudioSessionNotification,
    /// Session managers registered with `session_cb`, by endpoint id.
    managers: Vec<(String, IAudioSessionManager2)>,
    signals: Arc<SessionSignals>,
}

impl SessionWatcher {
    /// Registers for endpoint changes and for new sessions on every active render endpoint.
    /// Fails only if the endpoint notifications cannot be registered; an endpoint whose
    /// session notifications cannot be registered is logged and retried on the next endpoint
    /// change.
    pub fn start(signals: Arc<SessionSignals>) -> Result<Self, String> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("device enumerator: {e}"))?;
        let endpoint_cb: IMMNotificationClient = EndpointCallback {
            signals: signals.clone(),
        }
        .into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&endpoint_cb) }
            .map_err(|e| format!("registering endpoint notifications: {e}"))?;
        let session_cb: IAudioSessionNotification = SessionCallback {
            signals: signals.clone(),
        }
        .into();
        let mut watcher = Self {
            enumerator,
            endpoint_cb,
            session_cb,
            managers: Vec::new(),
            signals,
        };
        if let Err(e) = watcher.refresh() {
            eprintln!("devocal engine: session notifications: {e}");
        }
        Ok(watcher)
    }

    /// Engine loop, every pass: after an endpoint change, (re)registers on the active render
    /// endpoints and asks for a follow pass.
    pub fn maintain(&mut self) {
        if !self.signals.take_endpoints_changed() {
            return;
        }
        if let Err(e) = self.refresh() {
            eprintln!("devocal engine: session notifications: {e}");
        }
        self.signals.notify_session_created();
    }

    /// Endpoint ids currently registered for session notifications.
    #[cfg(test)]
    pub fn registered(&self) -> Vec<String> {
        self.managers.iter().map(|(id, _)| id.clone()).collect()
    }

    /// Unregisters from endpoints that are no longer active and registers on new active ones.
    /// Returns the first error; the other endpoints are still handled.
    fn refresh(&mut self) -> Result<(), String> {
        let mut active = Vec::new();
        let mut first_err: Option<String> = None;
        let devices = unsafe {
            self.enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        }
        .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;
        let count = unsafe { devices.GetCount() }.map_err(|e| format!("endpoint count: {e}"))?;
        for i in 0..count {
            let read = || -> Result<_, String> {
                let dev = unsafe { devices.Item(i) }.map_err(|e| format!("endpoint {i}: {e}"))?;
                let id = unsafe { dev.GetId() }.map_err(|e| format!("endpoint {i} id: {e}"))?;
                let text = unsafe { id.to_string() }.map_err(|e| e.to_string());
                unsafe { CoTaskMemFree(Some(id.0 as *const _)) };
                Ok((text?, dev))
            };
            match read() {
                Ok(d) => active.push(d),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }

        let cb = &self.session_cb;
        self.managers.retain(|(id, mgr)| {
            let keep = active.iter().any(|(a, _)| a == id);
            if !keep {
                // The endpoint may already be gone; nothing more to do then.
                let _ = unsafe { mgr.UnregisterSessionNotification(cb) };
            }
            keep
        });
        for (id, dev) in active {
            if self.managers.iter().any(|(m, _)| *m == id) {
                continue;
            }
            let register = || -> Result<IAudioSessionManager2, String> {
                let mgr: IAudioSessionManager2 = unsafe { dev.Activate(CLSCTX_ALL, None) }
                    .map_err(|e| format!("session manager on {id}: {e}"))?;
                // Notifications start only after the enumerator was requested once.
                unsafe { mgr.GetSessionEnumerator() }
                    .map_err(|e| format!("session enumerator on {id}: {e}"))?;
                unsafe { mgr.RegisterSessionNotification(&self.session_cb) }
                    .map_err(|e| format!("registering session notifications on {id}: {e}"))?;
                Ok(mgr)
            };
            match register() {
                Ok(mgr) => self.managers.push((id, mgr)),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for SessionWatcher {
    fn drop(&mut self) {
        for (_, mgr) in self.managers.drain(..) {
            let _ = unsafe { mgr.UnregisterSessionNotification(&self.session_cb) };
        }
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.endpoint_cb)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ComGuard;
    use std::time::{Duration, Instant};

    #[test]
    fn signals_are_taken_once() {
        let s = SessionSignals::default();
        assert!(!s.take_session_created());
        assert!(!s.take_endpoints_changed());
        assert_eq!(s.take_default_changed(), None);
        s.notify_session_created();
        s.notify_endpoints_changed();
        s.notify_default_changed(Some("ep-a".into()));
        s.notify_default_changed(Some("ep-b".into())); // the latest wins
        assert!(s.take_session_created());
        assert!(!s.take_session_created());
        assert!(s.take_endpoints_changed());
        assert!(!s.take_endpoints_changed());
        assert_eq!(s.take_default_changed(), Some(Some("ep-b".into())));
        assert_eq!(s.take_default_changed(), None);
        s.notify_default_changed(None);
        assert_eq!(s.take_default_changed(), Some(None));
    }

    #[test]
    fn callbacks_only_set_flags() {
        let signals = Arc::new(SessionSignals::default());
        let client: IMMNotificationClient = EndpointCallback {
            signals: signals.clone(),
        }
        .into();
        let id: Vec<u16> = "ep-new\0".encode_utf16().collect();
        let id = PCWSTR(id.as_ptr());
        unsafe {
            client
                .OnDefaultDeviceChanged(eRender, eConsole, id)
                .unwrap();
        }
        assert_eq!(signals.take_default_changed(), Some(Some("ep-new".into())));
        unsafe {
            // Communications default and capture devices: ignored.
            client
                .OnDefaultDeviceChanged(eRender, windows::Win32::Media::Audio::eCommunications, id)
                .unwrap();
            client
                .OnDefaultDeviceChanged(windows::Win32::Media::Audio::eCapture, eConsole, id)
                .unwrap();
        }
        assert_eq!(signals.take_default_changed(), None);
        unsafe {
            client
                .OnDefaultDeviceChanged(eRender, eMultimedia, PCWSTR::null())
                .unwrap();
        }
        assert_eq!(signals.take_default_changed(), Some(None));
        unsafe {
            client.OnDeviceAdded(id).unwrap();
        }
        assert!(signals.take_endpoints_changed());
        unsafe {
            client.OnDeviceRemoved(id).unwrap();
        }
        assert!(signals.take_endpoints_changed());
        unsafe {
            client
                .OnDeviceStateChanged(id, DEVICE_STATE_ACTIVE)
                .unwrap();
        }
        assert!(signals.take_endpoints_changed());
        assert!(!signals.take_session_created());

        let session: IAudioSessionNotification = SessionCallback {
            signals: signals.clone(),
        }
        .into();
        unsafe {
            session.OnSessionCreated(None).unwrap();
        }
        assert!(signals.take_session_created());
    }

    /// Registers on the real endpoints and unregisters again. Read-only: no session is
    /// created and no volume is touched.
    #[test]
    fn watcher_registers_and_unregisters_on_the_real_endpoints() {
        let _com = ComGuard::init_mta().unwrap();
        let signals = Arc::new(SessionSignals::default());
        let mut w = SessionWatcher::start(signals.clone()).expect("start");
        let n = w.registered().len();
        // An endpoint change re-registers without duplicates and asks for a follow.
        signals.notify_endpoints_changed();
        w.maintain();
        assert_eq!(w.registered().len(), n);
        assert!(signals.take_session_created());
        drop(w);
    }

    /// Manual: opens (never starts) a shared-mode render stream on the default endpoint,
    /// which creates an audio session for this test process, and checks that the watcher
    /// sees it. No audio is played and no session volume is changed.
    /// `cargo test -p devocal-engine notify -- --ignored`
    #[test]
    #[ignore = "creates an audio session on the default render endpoint"]
    fn watcher_sees_a_new_session() {
        use windows::Win32::Media::Audio::{IAudioClient, AUDCLNT_SHAREMODE_SHARED};
        let _com = ComGuard::init_mta().unwrap();
        let signals = Arc::new(SessionSignals::default());
        let w = SessionWatcher::start(signals.clone()).expect("start");
        assert!(!w.registered().is_empty(), "no active render endpoint");
        assert!(!signals.take_session_created());
        let dev = crate::audio::endpoint::find_render_device(None).unwrap();
        let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None) }.unwrap();
        let fmt = crate::audio::endpoint::CoTaskFormat(unsafe { client.GetMixFormat() }.unwrap());
        let started = Instant::now();
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, fmt.0, None) }
            .unwrap();
        let mut seen = false;
        while started.elapsed() < Duration::from_secs(2) {
            if signals.take_session_created() {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        eprintln!("session notification after {:?}", started.elapsed());
        assert!(seen, "no OnSessionCreated within 2 s");
        drop(client);
        drop(w);
    }
}
