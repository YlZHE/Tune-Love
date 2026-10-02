//! Abstraction over the Windows per-process audio-session volume.
//! The real implementation lives in the engine crate; `FakeSessions` is the
//! test double (available with the `fake` feature).

/// The volume the engine holds a player session at while it plays the audio itself (-80 dB).
pub const HELD_VOLUME: f32 = 1.0e-4;

/// True when `v` is (still) the volume the engine set: `|v - 1e-4| <= 1e-6`.
pub fn is_held(v: f32) -> bool {
    (v - HELD_VOLUME).abs() <= 1.0e-6
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub instance_id: String,
    pub pid: u32,
    pub endpoint_id: String,
    pub active: bool,
}

pub trait SessionVolumes {
    fn sessions_for_tree(&self, root_pid: u32) -> Result<Vec<SessionInfo>, String>;
    fn session(&self, instance_id: &str) -> Result<Option<SessionInfo>, String>;
    fn volume(&self, instance_id: &str) -> Result<f32, String>;
    fn set_volume(&self, instance_id: &str, volume: f32) -> Result<(), String>;
    fn muted(&self, instance_id: &str) -> Result<bool, String>;
    /// Process creation time as a FILETIME; `None` when the process does not exist.
    fn process_created(&self, pid: u32) -> Option<u64>;
}

#[cfg(any(test, feature = "fake"))]
pub use fake::{FakeSessions, SetVolumeHook};

#[cfg(any(test, feature = "fake"))]
mod fake {
    use super::{SessionInfo, SessionVolumes};
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

        pub fn add_session(&self, info: SessionInfo, volume: f32, muted: bool) {
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

        /// Clears all injected failures.
        pub fn clear_failures(&self) {
            let mut st = self.state.borrow_mut();
            st.fail_session.clear();
            st.fail_volume.clear();
            st.fail_set_volume.clear();
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
                .filter(|s| in_tree(s.info.pid))
                .map(|s| s.info.clone())
                .collect())
        }

        fn session(&self, instance_id: &str) -> Result<Option<SessionInfo>, String> {
            if self.state.borrow().fail_session.contains(instance_id) {
                return Err(format!("injected session failure: {instance_id}"));
            }
            Ok(self
                .state
                .borrow()
                .sessions
                .iter()
                .find(|s| s.info.instance_id == instance_id)
                .map(|s| s.info.clone()))
        }

        fn volume(&self, instance_id: &str) -> Result<f32, String> {
            if self.state.borrow().fail_volume.contains(instance_id) {
                return Err(format!("injected volume failure: {instance_id}"));
            }
            self.state
                .borrow()
                .sessions
                .iter()
                .find(|s| s.info.instance_id == instance_id)
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
            match st
                .sessions
                .iter_mut()
                .find(|s| s.info.instance_id == instance_id)
            {
                Some(s) => {
                    s.volume = volume;
                    Ok(())
                }
                None => Err(format!("no such session: {instance_id}")),
            }
        }

        fn muted(&self, instance_id: &str) -> Result<bool, String> {
            self.state
                .borrow()
                .sessions
                .iter()
                .find(|s| s.info.instance_id == instance_id)
                .map(|s| s.muted)
                .ok_or_else(|| format!("no such session: {instance_id}"))
        }

        fn process_created(&self, pid: u32) -> Option<u64> {
            self.state.borrow().created.get(&pid).copied()
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
            pid,
            endpoint_id: "ep".into(),
            active: true,
        }
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
    fn fake_process_created_and_tree() {
        let f = FakeSessions::new();
        f.set_process_created(10, Some(111));
        assert_eq!(f.process_created(10), Some(111));
        f.set_process_created(10, None);
        assert_eq!(f.process_created(10), None);

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
