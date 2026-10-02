//! Windows implementation of [`SessionVolumes`] over WASAPI audio sessions.
//!
//! Every call enumerates the sessions of all active render endpoints afresh; nothing is cached,
//! so a session that moved, expired or was recreated is always seen as it is now.
//!
//! **COM:** every `WinSessions` method must run on a thread that has already initialised COM in
//! the multithreaded apartment (`CoInitializeEx(None, COINIT_MULTITHREADED)`). `WinSessions`
//! does not initialise COM itself; without it the calls fail with `CO_E_NOTINITIALIZED`.
//! [`process_tree`] and [`tree_from_pairs`] do not use COM.

use crate::sessions::{endpoint_of_instance, SessionInfo, SessionVolumes};
use std::collections::{HashMap, HashSet};
use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE};
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The real per-process session volume control. See the module docs for the COM requirement.
#[derive(Debug, Clone, Copy, Default)]
pub struct WinSessions;

/// `root` plus every live descendant process.
///
/// A process counts as a child only if it was created no earlier than its parent, which rejects
/// stale parent links left behind by pid reuse. If the process list cannot be read, or the
/// root's creation time cannot be queried, the result is just `[root]`.
pub fn process_tree(root: u32) -> Vec<u32> {
    let Some(pairs) = process_pairs() else {
        return vec![root];
    };
    // Narrow to the candidates a time-blind walk reaches; the creation-time check only removes
    // edges, so this loses nothing and avoids opening every process on the system.
    let untimed: Vec<(u32, u32, u64)> = pairs.iter().map(|&(p, pp)| (p, pp, 0)).collect();
    let candidates: HashSet<u32> = tree_from_pairs(root, &untimed).into_iter().collect();
    let timed: Vec<(u32, u32, u64)> = pairs
        .iter()
        .filter(|(pid, _)| candidates.contains(pid))
        .filter_map(|&(pid, parent)| match query_created(pid) {
            Ok(Some(created)) => Some((pid, parent, created)),
            // Gone since the snapshot, or not queryable: cannot be verified, leave it out.
            _ => None,
        })
        .collect();
    tree_from_pairs(root, &timed)
}

/// Pure part of [`process_tree`]: `procs` holds `(pid, parent pid, creation FILETIME)`.
///
/// Returns `root` first, then its descendants. A process is a child of `parent` only if its
/// creation time is not earlier than the parent's (pid-reuse protection); a process listed as
/// its own parent is ignored, and each pid is visited once (cycle protection). If `root` is not
/// in `procs` its creation time is unknown and no children are accepted.
pub fn tree_from_pairs(root: u32, procs: &[(u32, u32, u64)]) -> Vec<u32> {
    let created: HashMap<u32, u64> = procs.iter().map(|&(pid, _, c)| (pid, c)).collect();
    let mut tree = vec![root];
    let mut seen: HashSet<u32> = HashSet::from([root]);
    let mut next = 0;
    while next < tree.len() {
        let parent = tree[next];
        next += 1;
        let Some(&parent_created) = created.get(&parent) else {
            continue;
        };
        for &(pid, ppid, child_created) in procs {
            if ppid == parent && pid != ppid && child_created >= parent_created && seen.insert(pid)
            {
                tree.push(pid);
            }
        }
    }
    tree
}

/// `(pid, parent pid)` for every process in a ToolHelp snapshot; `None` if the snapshot fails.
fn process_pairs() -> Option<Vec<(u32, u32)>> {
    let snap = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut pairs = Vec::new();
    if unsafe { Process32FirstW(snap.0, &mut entry) }.is_ok() {
        loop {
            pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
            if unsafe { Process32NextW(snap.0, &mut entry) }.is_err() {
                break;
            }
        }
    }
    Some(pairs)
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn filetime(t: FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}

/// See [`SessionVolumes::process_created`]. Only `OpenProcess` failing with
/// `ERROR_INVALID_PARAMETER` means "no such pid"; any other failure (e.g. access denied) is an
/// `Err`. The exit time is deliberately not consulted: it is undefined for a running process.
/// A just-exited process still held open elsewhere therefore reports `Some`; its sessions are
/// judged by the session lookups, not here.
fn query_created(pid: u32) -> Result<Option<u64>, String> {
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => Handle(h),
        Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(None),
        Err(e) => return Err(format!("OpenProcess({pid}): {e}")),
    };
    let (mut start, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetProcessTimes(handle.0, &mut start, &mut exit, &mut kernel, &mut user) }
        .map_err(|e| format!("GetProcessTimes({pid}): {e}"))?;
    Ok(Some(filetime(start)))
}

/// Copies a COM-allocated wide string and frees it.
fn take_pwstr(p: PWSTR) -> Result<String, String> {
    let s = unsafe { p.to_string() }.map_err(|e| e.to_string());
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// One session as enumerated, with the id of the render endpoint it lives on.
struct RawSession {
    endpoint_id: String,
    control: IAudioSessionControl2,
}

/// Result of one enumeration pass over the active render endpoints.
struct Enumerated {
    sessions: Vec<RawSession>,
    /// Ids of the active render endpoints that were read.
    active_endpoints: HashSet<String>,
    /// First failure that affected only one endpoint (its sessions are missing from `sessions`).
    error: Option<String>,
}

/// Every session on every active render endpoint. A failure that affects only one endpoint is
/// returned alongside the sessions that were read, so a lookup can still succeed when it found
/// its target, but never reports "absent" when it might not be.
fn enumerate() -> Result<Enumerated, String> {
    let mut out = Vec::new();
    let mut active_endpoints = HashSet::new();
    let mut first_err: Option<String> = None;
    let en: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
    let devices = unsafe { en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;
    let count = unsafe { devices.GetCount() }.map_err(|e| format!("endpoint count: {e}"))?;
    for i in 0..count {
        let mut read = || -> Result<(), String> {
            let dev = unsafe { devices.Item(i) }.map_err(|e| format!("endpoint {i}: {e}"))?;
            let id = unsafe { dev.GetId() }.map_err(|e| format!("endpoint {i} id: {e}"))?;
            let endpoint_id = take_pwstr(id)?;
            active_endpoints.insert(endpoint_id.clone());
            let mgr: IAudioSessionManager2 = unsafe { dev.Activate(CLSCTX_ALL, None) }
                .map_err(|e| format!("session manager on {endpoint_id}: {e}"))?;
            let sessions = unsafe { mgr.GetSessionEnumerator() }
                .map_err(|e| format!("session enumerator on {endpoint_id}: {e}"))?;
            let n = unsafe { sessions.GetCount() }
                .map_err(|e| format!("session count on {endpoint_id}: {e}"))?;
            for j in 0..n {
                let control = unsafe { sessions.GetSession(j) }
                    .and_then(|c| c.cast::<IAudioSessionControl2>())
                    .map_err(|e| format!("session {j} on {endpoint_id}: {e}"))?;
                out.push(RawSession {
                    endpoint_id: endpoint_id.clone(),
                    control,
                });
            }
            Ok(())
        };
        if let Err(e) = read() {
            first_err.get_or_insert(e);
        }
    }
    Ok(Enumerated {
        sessions: out,
        active_endpoints,
        error: first_err,
    })
}

fn instance_id(s: &RawSession) -> Result<String, String> {
    let p = unsafe { s.control.GetSessionInstanceIdentifier() }
        .map_err(|e| format!("session instance id: {e}"))?;
    take_pwstr(p)
}

fn session_identifier(s: &RawSession) -> Result<String, String> {
    let p = unsafe { s.control.GetSessionIdentifier() }
        .map_err(|e| format!("session identifier: {e}"))?;
    take_pwstr(p)
}

fn info(s: &RawSession, instance_id: String) -> Result<SessionInfo, String> {
    let pid = unsafe { s.control.GetProcessId() }
        .map_err(|e| format!("session pid ({instance_id}): {e}"))?;
    let state = unsafe { s.control.GetState() }
        .map_err(|e| format!("session state ({instance_id}): {e}"))?;
    let session_identifier = session_identifier(s).map_err(|e| format!("{e} ({instance_id})"))?;
    Ok(SessionInfo {
        instance_id,
        session_identifier,
        pid,
        endpoint_id: s.endpoint_id.clone(),
        active: state == AudioSessionStateActive,
    })
}

/// Outcome of a lookup that read everything without error and found nothing. The instance id
/// names its endpoint (prefix before the first `|`); only if that endpoint is active is the
/// session really absent. On an unplugged or disabled endpoint its sessions are not enumerated
/// but may well still exist (and come back), so that, like an unparsable id, is an error.
fn not_found<T>(
    instance_id: &str,
    active_endpoints: &HashSet<String>,
) -> Result<Option<T>, String> {
    match endpoint_of_instance(instance_id) {
        Some(ep) if active_endpoints.contains(ep) => Ok(None),
        Some(ep) => Err(format!(
            "endpoint {ep} is not active; session may still exist: {instance_id}"
        )),
        None => Err(format!("no endpoint in session instance id: {instance_id}")),
    }
}

/// The session with this instance identifier. `Ok(None)` only when every endpoint and session
/// was read without error, none matched, and the id's endpoint is active (see [`not_found`]).
fn find(wanted: &str) -> Result<Option<(SessionInfo, RawSession)>, String> {
    let Enumerated {
        sessions,
        active_endpoints,
        error: mut err,
    } = enumerate()?;
    for s in sessions {
        match instance_id(&s) {
            Ok(id) if id == wanted => {
                let info = info(&s, id)?;
                return Ok(Some((info, s)));
            }
            Ok(_) => {}
            Err(e) => {
                err.get_or_insert(e);
            }
        }
    }
    match err {
        Some(e) => Err(e),
        None => not_found(wanted, &active_endpoints),
    }
}

fn volume_control(instance_id: &str) -> Result<ISimpleAudioVolume, String> {
    let (_, s) = find(instance_id)?.ok_or_else(|| format!("no such session: {instance_id}"))?;
    s.control
        .cast::<ISimpleAudioVolume>()
        .map_err(|e| format!("ISimpleAudioVolume ({instance_id}): {e}"))
}

impl SessionVolumes for WinSessions {
    /// Sessions owned by `root_pid` or any of its descendants, on every active render endpoint.
    /// Fails (rather than returning a partial list) if any endpoint could not be read, so the
    /// caller never holds only part of a player. `root_pid == 0` is rejected: pid 0 owns the
    /// system-sounds session.
    fn sessions_for_tree(&self, root_pid: u32) -> Result<Vec<SessionInfo>, String> {
        if root_pid == 0 {
            return Err("pid 0 is not a player process".into());
        }
        let tree: HashSet<u32> = process_tree(root_pid).into_iter().collect();
        let all = enumerate()?;
        if let Some(e) = all.error {
            return Err(e);
        }
        let mut out = Vec::new();
        for s in &all.sessions {
            // A session whose pid cannot be read cannot be attributed to the tree.
            let Ok(pid) = (unsafe { s.control.GetProcessId() }) else {
                continue;
            };
            if pid == 0 || !tree.contains(&pid) {
                continue;
            }
            out.push(info(s, instance_id(s)?)?);
        }
        Ok(out)
    }

    /// Sessions with this pid-free identifier on every active render endpoint. Like
    /// `sessions_for_tree`, fails rather than returning a partial list.
    fn sessions_with_identifier(
        &self,
        session_identifier: &str,
    ) -> Result<Vec<SessionInfo>, String> {
        let all = enumerate()?;
        if let Some(e) = all.error {
            return Err(e);
        }
        let mut out = Vec::new();
        for s in &all.sessions {
            if self::session_identifier(s)? == session_identifier {
                out.push(info(s, instance_id(s)?)?);
            }
        }
        Ok(out)
    }

    fn session(&self, instance_id: &str) -> Result<Option<SessionInfo>, String> {
        Ok(find(instance_id)?.map(|(info, _)| info))
    }

    fn volume(&self, instance_id: &str) -> Result<f32, String> {
        unsafe { volume_control(instance_id)?.GetMasterVolume() }
            .map_err(|e| format!("GetMasterVolume ({instance_id}): {e}"))
    }

    fn set_volume(&self, instance_id: &str, volume: f32) -> Result<(), String> {
        unsafe { volume_control(instance_id)?.SetMasterVolume(volume, std::ptr::null()) }
            .map_err(|e| format!("SetMasterVolume ({instance_id}): {e}"))
    }

    fn muted(&self, instance_id: &str) -> Result<bool, String> {
        unsafe { volume_control(instance_id)?.GetMute() }
            .map(|b| b.as_bool())
            .map_err(|e| format!("GetMute ({instance_id}): {e}"))
    }

    fn process_created(&self, pid: u32) -> Result<Option<u64>, String> {
        query_created(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Far above any pid Windows hands out; `OpenProcess` reports `ERROR_INVALID_PARAMETER`.
    const UNUSED_PID: u32 = 0xFFFF_FFF0;

    fn sorted(mut v: Vec<u32>) -> Vec<u32> {
        v.sort_unstable();
        v
    }

    #[test]
    fn process_tree_contains_root_and_children() {
        // 100 -> 101 -> 102 (grandchild), 100 -> 103; 200 is unrelated, 201 its child.
        let procs = [
            (100, 1, 1_000),
            (101, 100, 1_100),
            (102, 101, 1_200),
            (103, 100, 1_300),
            (200, 1, 900),
            (201, 200, 950),
        ];
        assert_eq!(
            sorted(tree_from_pairs(100, &procs)),
            vec![100, 101, 102, 103]
        );
        assert_eq!(tree_from_pairs(201, &procs), vec![201]);
    }

    #[test]
    fn process_tree_rejects_children_older_than_their_parent() {
        // pid 100 was reused: 101 names pid 100 as its parent but is older than the current
        // process 100, so it descends from an earlier process that had the same pid.
        let procs = [
            (100, 1, 5_000),
            (101, 100, 4_000), // older than 100: stale parent link (pid reuse)
            (102, 101, 4_500), // child of the stale 101: not ours either
            (103, 100, 5_000), // same timestamp as the parent: still a child
            (104, 103, 6_000),
        ];
        assert_eq!(sorted(tree_from_pairs(100, &procs)), vec![100, 103, 104]);
    }

    #[test]
    fn process_tree_survives_cycles_and_self_parents() {
        let procs = [
            (0, 0, 0),         // System Idle: its own parent
            (100, 102, 1_000), // 100 <-> 101 <-> 102 cycle with equal timestamps
            (101, 100, 1_000),
            (102, 101, 1_000),
        ];
        assert_eq!(sorted(tree_from_pairs(100, &procs)), vec![100, 101, 102]);
        assert_eq!(tree_from_pairs(0, &procs), vec![0]);
    }

    #[test]
    fn missing_session_is_absent_only_on_an_active_endpoint() {
        let active: HashSet<String> = HashSet::from(["{ep-a}".to_string()]);
        assert_eq!(
            not_found::<()>("{ep-a}|x.exe%b{0}|1%b10", &active),
            Ok(None)
        );
        // Endpoint unplugged/disabled: the session may still exist there.
        assert!(not_found::<()>("{ep-b}|x.exe%b{0}|1%b10", &active).is_err());
        // No endpoint prefix: cannot tell, so never "absent".
        assert!(not_found::<()>("garbage", &active).is_err());
    }

    #[test]
    fn process_created_reports_this_process() {
        let me = WinSessions.process_created(std::process::id());
        assert!(matches!(me, Ok(Some(t)) if t > 0), "{me:?}");
    }

    #[test]
    fn process_created_reports_an_unused_pid_as_absent() {
        assert_eq!(WinSessions.process_created(UNUSED_PID), Ok(None));
    }

    #[test]
    fn process_tree_without_root_record_has_no_children() {
        // Without the root's creation time no child can be verified.
        let procs = [(101, 100, 1_100)];
        assert_eq!(tree_from_pairs(100, &procs), vec![100]);
    }
}
