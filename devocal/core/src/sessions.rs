//! Abstraction over the Windows per-process audio-session volume.
//! The real implementation is `sessions_win::WinSessions` (Windows only); `FakeSessions` is
//! the test double (available with the `fake` feature).

/// The volume the engine holds a player session at while it plays the audio itself (-80 dB).
pub const HELD_VOLUME: f32 = 1.0e-4;

/// True when `v` is (still) the volume the engine set: `|v - 1e-4| <= 1e-6`.
pub fn is_held(v: f32) -> bool {
    (v - HELD_VOLUME).abs() <= 1.0e-6
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Session instance identifier: unique per session, contains the owning pid.
    pub instance_id: String,
    /// Session identifier (`IAudioSessionControl2::GetSessionIdentifier`): pid-free, the same for
    /// every launch of the same executable on the same endpoint. Windows persists per-app
    /// session volume under this kind of key, so it survives a player restart.
    pub session_identifier: String,
    pub pid: u32,
    pub endpoint_id: String,
    pub active: bool,
}

/// Endpoint id of a session instance identifier: the non-empty prefix before the first `|`.
pub fn endpoint_of_instance(instance_id: &str) -> Option<&str> {
    instance_id
        .split_once('|')
        .map(|(ep, _)| ep)
        .filter(|ep| !ep.is_empty())
}

pub trait SessionVolumes {
    fn sessions_for_tree(&self, root_pid: u32) -> Result<Vec<SessionInfo>, String>;
    /// All current sessions whose (pid-free) session identifier equals `session_identifier`.
    fn sessions_with_identifier(
        &self,
        session_identifier: &str,
    ) -> Result<Vec<SessionInfo>, String>;
    fn session(&self, instance_id: &str) -> Result<Option<SessionInfo>, String>;
    fn volume(&self, instance_id: &str) -> Result<f32, String>;
    fn set_volume(&self, instance_id: &str, volume: f32) -> Result<(), String>;
    fn muted(&self, instance_id: &str) -> Result<bool, String>;
    /// Process creation time as a FILETIME.
    ///
    /// `Ok(Some(t))`: the process is running and was created at `t`. `Ok(None)`: there is no
    /// such process, or it has exited (even if its process object is still held open
    /// somewhere and still reports a creation time). `Err`: the query failed for another
    /// reason (e.g. access denied); callers must not treat that as "gone".
    fn process_created(&self, pid: u32) -> Result<Option<u64>, String>;
    /// Ids of the current default render endpoint for the console and multimedia roles
    /// (usually one id; two when the roles differ). Empty when there is no default render
    /// endpoint. Callers treat an empty list or an `Err` as "unknown".
    fn default_render_endpoints(&self) -> Result<Vec<String>, String>;
}

#[cfg(any(test, feature = "fake"))]
pub use fake::{FakeSessions, SetVolumeHook};

#[cfg(any(test, feature = "fake"))]
mod fake {
    use super::{endpoint_of_instance, SessionInfo, SessionVolumes};
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

    struct FakeSession {
        info: SessionInfo,
        volume: f32,
        muted: bool,
    }

    #[derive(Default)]
    struct State {
        sessions: Vec<FakeSession>,
        created: HashMap<u32, u64>,
        /// child pid -> parent pid, used by `sessions_for_tree`.
        parents: HashMap<u32, u32>,
        /// Ordered log of every `set_volume` attempt as (instance id, volume).
        writes: Vec<(String, f32)>,
        fail_session: HashSet<String>,
        fail_volume: HashSet<String>,
        fail_set_volume: HashSet<String>,
        fail_process_created: HashSet<u32>,
        fail_sessions_with_identifier: HashSet<String>,
        /// Endpoints marked unplugged/disabled: their sessions are not enumerated.
        inactive_endpoints: HashSet<String>,
        /// `default_render_endpoints`; empty (unknown) unless a test sets it.
        default_endpoints: Vec<String>,
        fail_default_endpoints: bool,
    }

    impl State {
        /// The session, if it exists and its endpoint is active (i.e. it would be enumerated).
        fn visible(&self, instance_id: &str) -> Option<&FakeSession> {
            self.sessions.iter().find(|s| {
                s.info.instance_id == instance_id
                    && !self.inactive_endpoints.contains(&s.info.endpoint_id)
            })
        }
    }

    /// Hook called as `(instance_id, volume)` at the start of every `set_volume`.
    pub type SetVolumeHook = Box<dyn Fn(&str, f32)>;

    /// In-memory `SessionVolumes` for tests. Interior mutability keeps the trait `&self`.
    #[derive(Default)]
    pub struct FakeSessions {
        state: RefCell<State>,
        hook: RefCell<Option<SetVolumeHook>>,
    }

    impl FakeSessions {
        pub fn new() -> Self {
            Self::default()
        }

        /// Adds a session. An empty `session_identifier` is replaced by a unique default.
        pub fn add_session(&self, mut info: SessionInfo, volume: f32, muted: bool) {
            if info.session_identifier.is_empty() {
                info.session_identifier = format!("fake-identifier:{}", info.instance_id);
            }
            self.state.borrow_mut().sessions.push(FakeSession {
                info,
                volume,
                muted,
            });
        }

        pub fn remove_session(&self, instance_id: &str) {
            self.state
                .borrow_mut()
                .sessions
                .retain(|s| s.info.instance_id != instance_id);
        }

        pub fn set_active(&self, instance_id: &str, active: bool) {
            if let Some(s) = self
                .state
                .borrow_mut()
                .sessions
                .iter_mut()
                .find(|s| s.info.instance_id == instance_id)
            {
                s.info.active = active;
            }
        }

        pub fn set_muted(&self, instance_id: &str, muted: bool) {
            if let Some(s) = self
                .state
                .borrow_mut()
                .sessions
                .iter_mut()
                .find(|s| s.info.instance_id == instance_id)
            {
                s.muted = muted;
            }
        }

        /// Sets (or with `None` removes) the creation time of `pid`.
        pub fn set_process_created(&self, pid: u32, created: Option<u64>) {
            let mut st = self.state.borrow_mut();
            match created {
                Some(c) => {
                    st.created.insert(pid, c);
                }
                None => {
                    st.created.remove(&pid);
                }
            }
        }

        /// Declares `child` as a child process of `parent` for `sessions_for_tree`.
        pub fn set_parent(&self, child: u32, parent: u32) {
            self.state.borrow_mut().parents.insert(child, parent);
        }

        /// Number of `set_volume` calls attempted so far (including injected failures).
        pub fn set_volume_calls(&self) -> usize {
            self.state.borrow().writes.len()
        }

        /// Ordered log of every `set_volume` attempt as (instance id, volume),
        /// including ones that failed through `fail_set_volume`.
        pub fn writes(&self) -> Vec<(String, f32)> {
            self.state.borrow().writes.clone()
        }

        /// Installs a hook called at the start of every `set_volume`, before the write is
        /// applied and with no internal borrow held (the hook may call back into the fake or
        /// inspect the file system, e.g. to assert that a restore file already exists).
        pub fn on_set_volume(&self, hook: SetVolumeHook) {
            *self.hook.borrow_mut() = Some(hook);
        }

        /// Makes `session(instance_id)` return an error until `clear_failures`.
        pub fn fail_session(&self, instance_id: &str) {
            self.state
                .borrow_mut()
                .fail_session
                .insert(instance_id.into());
        }

        /// Makes `volume(instance_id)` return an error until `clear_failures`.
        pub fn fail_volume(&self, instance_id: &str) {
            self.state
                .borrow_mut()
                .fail_volume
                .insert(instance_id.into());
        }

        /// Makes `set_volume(instance_id, ..)` return an error (still logged) until `clear_failures`.
        pub fn fail_set_volume(&self, instance_id: &str) {
            self.state
                .borrow_mut()
                .fail_set_volume
                .insert(instance_id.into());
        }

        /// Makes `process_created(pid)` return an error until `clear_failures`.
        pub fn fail_process_created(&self, pid: u32) {
            self.state.borrow_mut().fail_process_created.insert(pid);
        }

        /// Makes `sessions_with_identifier(id)` return an error until `clear_failures`.
        pub fn fail_sessions_with_identifier(&self, session_identifier: &str) {
            self.state
                .borrow_mut()
                .fail_sessions_with_identifier
                .insert(session_identifier.into());
        }

        /// Marks an endpoint unplugged/disabled (`false`) or active again (`true`). Like
        /// `WinSessions`, sessions on an inactive endpoint are not enumerated, and looking up an
        /// instance id on such an endpoint is an error rather than "absent".
        pub fn set_endpoint_active(&self, endpoint_id: &str, active: bool) {
            let mut st = self.state.borrow_mut();
            if active {
                st.inactive_endpoints.remove(endpoint_id);
            } else {
                st.inactive_endpoints.insert(endpoint_id.into());
            }
        }

        /// Sets the default render endpoint(s) (`&[]`: none, i.e. unknown).
        pub fn set_default_endpoints(&self, ids: &[&str]) {
            self.state.borrow_mut().default_endpoints = ids.iter().map(|s| s.to_string()).collect();
        }

        /// Makes `default_render_endpoints` return an error until `clear_failures`.
        pub fn fail_default_endpoints(&self) {
            self.state.borrow_mut().fail_default_endpoints = true;
        }

        /// Clears all injected failures (not endpoint states).
        pub fn clear_failures(&self) {
            let mut st = self.state.borrow_mut();
            st.fail_default_endpoints = false;
            st.fail_session.clear();
            st.fail_volume.clear();
            st.fail_set_volume.clear();
            st.fail_process_created.clear();
            st.fail_sessions_with_identifier.clear();
        }
    }

    impl SessionVolumes for FakeSessions {
        fn sessions_for_tree(&self, root_pid: u32) -> Result<Vec<SessionInfo>, String> {
            let st = self.state.borrow();
            let in_tree = |mut pid: u32| {
                // Bounded walk up the parent chain (guards against cycles).
                for _ in 0..64 {
                    if pid == root_pid {
                        return true;
                    }
                    match st.parents.get(&pid) {
                        Some(&p) => pid = p,
                        None => return false,
                    }
                }
                false
            };
            Ok(st
                .sessions
                .iter()
                .filter(|s| !st.inactive_endpoints.contains(&s.info.endpoint_id))
                .filter(|s| in_tree(s.info.pid))
                .map(|s| s.info.clone())
                .collect())
        }

        fn sessions_with_identifier(
            &self,
            session_identifier: &str,
        ) -> Result<Vec<SessionInfo>, String> {
            let st = self.state.borrow();
            if st
                .fail_sessions_with_identifier
                .contains(session_identifier)
            {
                return Err(format!(
                    "injected sessions_with_identifier failure: {session_identifier}"
                ));
            }
            Ok(st
                .sessions
                .iter()
                .filter(|s| !st.inactive_endpoints.contains(&s.info.endpoint_id))
                .filter(|s| s.info.session_identifier == session_identifier)
                .map(|s| s.info.clone())
                .collect())
        }

        fn session(&self, instance_id: &str) -> Result<Option<SessionInfo>, String> {
            let st = self.state.borrow();
            if st.fail_session.contains(instance_id) {
                return Err(format!("injected session failure: {instance_id}"));
            }
            if let Some(s) = st.visible(instance_id) {
                return Ok(Some(s.info.clone()));
            }
            let stored_endpoint = st
                .sessions
                .iter()
                .find(|s| s.info.instance_id == instance_id)
                .map(|s| s.info.endpoint_id.as_str());
            match stored_endpoint.or_else(|| endpoint_of_instance(instance_id)) {
                Some(ep) if st.inactive_endpoints.contains(ep) => {
                    Err(format!("endpoint {ep} is not active: {instance_id}"))
                }
                _ => Ok(None),
            }
        }

        fn volume(&self, instance_id: &str) -> Result<f32, String> {
            let st = self.state.borrow();
            if st.fail_volume.contains(instance_id) {
                return Err(format!("injected volume failure: {instance_id}"));
            }
            st.visible(instance_id)
                .map(|s| s.volume)
                .ok_or_else(|| format!("no such session: {instance_id}"))
        }

        fn set_volume(&self, instance_id: &str, volume: f32) -> Result<(), String> {
            if let Some(h) = self.hook.borrow().as_ref() {
                h(instance_id, volume);
            }
            let mut st = self.state.borrow_mut();
            st.writes.push((instance_id.to_string(), volume));
            if st.fail_set_volume.contains(instance_id) {
                return Err(format!("injected set_volume failure: {instance_id}"));
            }
            if st.visible(instance_id).is_none() {
                return Err(format!("no such session: {instance_id}"));
            }
            let s = st
                .sessions
                .iter_mut()
                .find(|s| s.info.instance_id == instance_id)
                .expect("visible session exists");
            s.volume = volume;
            Ok(())
        }

        fn muted(&self, instance_id: &str) -> Result<bool, String> {
            self.state
                .borrow()
                .visible(instance_id)
                .map(|s| s.muted)
                .ok_or_else(|| format!("no such session: {instance_id}"))
        }

        fn process_created(&self, pid: u32) -> Result<Option<u64>, String> {
            let st = self.state.borrow();
            if st.fail_process_created.contains(&pid) {
                return Err(format!("injected process_created failure: {pid}"));
            }
            Ok(st.created.get(&pid).copied())
        }

        fn default_render_endpoints(&self) -> Result<Vec<String>, String> {
            let st = self.state.borrow();
            if st.fail_default_endpoints {
                return Err("injected default_render_endpoints failure".into());
            }
            Ok(st.default_endpoints.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_held_tolerance() {
        assert!(is_held(1.0e-4));
        assert!(is_held(1.0e-4 + 0.9e-6));
        assert!(is_held(1.0e-4 - 0.9e-6));
        assert!(!is_held(1.0e-4 + 2.0e-6));
        assert!(!is_held(0.5));
        assert!(!is_held(0.0));
    }

    fn info(id: &str, pid: u32) -> SessionInfo {
        SessionInfo {
            instance_id: id.into(),
            session_identifier: String::new(),
            pid,
            endpoint_id: "ep".into(),
            active: true,
        }
    }

    #[test]
    fn endpoint_of_instance_takes_the_prefix_before_the_first_bar() {
        let id = "{0.0.0.00000000}.{667b}|\\Device\\HarddiskVolume3\\x.exe%b{0000}|1%b19376";
        assert_eq!(endpoint_of_instance(id), Some("{0.0.0.00000000}.{667b}"));
        assert_eq!(endpoint_of_instance("ep|a|b"), Some("ep"));
        assert_eq!(endpoint_of_instance("no-bar"), None);
        assert_eq!(endpoint_of_instance("|leading"), None);
        assert_eq!(endpoint_of_instance(""), None);
    }

    #[test]
    fn fake_session_identifier_lookup_and_injected_failure() {
        let f = FakeSessions::new();
        let mut a = info("ep|a|1%b10", 10);
        a.session_identifier = "ep|player.exe%b{0}".into();
        f.add_session(a.clone(), 0.5, false);
        f.add_session(info("ep|b|1%b11", 11), 0.5, false); // gets a default identifier
        let b = f.session("ep|b|1%b11").unwrap().unwrap();
        assert!(!b.session_identifier.is_empty());
        assert_ne!(b.session_identifier, a.session_identifier);

        assert_eq!(
            f.sessions_with_identifier("ep|player.exe%b{0}"),
            Ok(vec![a])
        );
        assert_eq!(f.sessions_with_identifier("nothing"), Ok(vec![]));
        f.fail_sessions_with_identifier("ep|player.exe%b{0}");
        assert!(f.sessions_with_identifier("ep|player.exe%b{0}").is_err());
        f.clear_failures();
        assert_eq!(
            f.sessions_with_identifier("ep|player.exe%b{0}")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn fake_inactive_endpoint_hides_sessions_and_errors_lookups() {
        let f = FakeSessions::new();
        let mut a = info("ep2|a|1%b10", 10);
        a.endpoint_id = "ep2".into();
        a.session_identifier = "ep2|player.exe%b{0}".into();
        f.add_session(a.clone(), 1.0e-4, false);
        f.set_endpoint_active("ep2", false);
        // Not visible, but not reported as absent either.
        assert!(f.session("ep2|a|1%b10").is_err());
        assert!(f.session("ep2|unknown").is_err());
        assert!(f.volume("ep2|a|1%b10").is_err());
        assert_eq!(f.sessions_for_tree(10), Ok(vec![]));
        assert_eq!(
            f.sessions_with_identifier("ep2|player.exe%b{0}"),
            Ok(vec![])
        );
        // Ids on active (or unknown) endpoints still report absence normally.
        assert_eq!(f.session("ep|missing"), Ok(None));
        f.set_endpoint_active("ep2", true);
        assert_eq!(f.session("ep2|a|1%b10"), Ok(Some(a)));
    }

    #[test]
    fn fake_reads_writes_and_counts() {
        let f = FakeSessions::new();
        f.add_session(info("a", 10), 0.8, false);
        assert_eq!(f.volume("a").unwrap(), 0.8);
        f.set_volume("a", 0.1).unwrap();
        assert_eq!(f.volume("a").unwrap(), 0.1);
        assert_eq!(f.set_volume_calls(), 1);
        assert!(f.set_volume("zz", 0.1).is_err());
        assert!(f.session("zz").unwrap().is_none());
        assert!(!f.muted("a").unwrap());
        f.remove_session("a");
        assert!(f.volume("a").is_err());
    }

    #[test]
    fn fake_injects_errors_logs_writes_and_calls_hook() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let f = FakeSessions::new();
        f.add_session(info("a", 10), 0.8, false);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s2 = seen.clone();
        f.on_set_volume(Box::new(move |id, v| {
            s2.borrow_mut().push((id.to_string(), v))
        }));

        f.fail_session("a");
        f.fail_volume("a");
        f.fail_set_volume("a");
        assert!(f.session("a").is_err());
        assert!(f.volume("a").is_err());
        assert!(f.set_volume("a", 0.1).is_err());
        f.clear_failures();
        assert_eq!(f.volume("a").unwrap(), 0.8); // failed write did not apply

        f.set_volume("a", 0.2).unwrap();
        assert_eq!(f.volume("a").unwrap(), 0.2);
        assert_eq!(
            f.writes(),
            vec![("a".to_string(), 0.1), ("a".to_string(), 0.2)]
        );
        assert_eq!(f.set_volume_calls(), 2);
        assert_eq!(*seen.borrow(), f.writes());
    }

    #[test]
    fn fake_default_endpoints_are_settable_and_fail_on_request() {
        let f = FakeSessions::new();
        assert_eq!(f.default_render_endpoints(), Ok(vec![]));
        f.set_default_endpoints(&["ep"]);
        assert_eq!(f.default_render_endpoints(), Ok(vec!["ep".to_string()]));
        f.fail_default_endpoints();
        assert!(f.default_render_endpoints().is_err());
        f.clear_failures();
        assert_eq!(f.default_render_endpoints(), Ok(vec!["ep".to_string()]));
    }

    #[test]
    fn fake_process_created_and_tree() {
        let f = FakeSessions::new();
        f.set_process_created(10, Some(111));
        assert_eq!(f.process_created(10), Ok(Some(111)));
        f.fail_process_created(10);
        assert!(f.process_created(10).is_err());
        f.clear_failures();
        assert_eq!(f.process_created(10), Ok(Some(111)));
        f.set_process_created(10, None);
        assert_eq!(f.process_created(10), Ok(None));

        f.add_session(info("root", 10), 1.0, false);
        f.add_session(info("child", 11), 1.0, false);
        f.add_session(info("other", 99), 1.0, false);
        f.set_parent(11, 10);
        let ids: Vec<_> = f
            .sessions_for_tree(10)
            .unwrap()
            .into_iter()
            .map(|s| s.instance_id)
            .collect();
        assert_eq!(ids, vec!["root", "child"]);
    }
}
