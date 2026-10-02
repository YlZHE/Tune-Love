//! Crash-safe restore file. Written atomically before any player session volume is
//! lowered; whoever survives (engine, app, next app start) restores from it.

use crate::sessions::{is_held, SessionInfo, SessionVolumes};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const RESTORE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreEntry {
    pub pid: u32,
    pub created_at: u64,
    pub instance_id: String,
    pub original_volume: f32,
    pub original_mute: bool,
    pub saved_at_ms: u64,
    /// Pid-free session identifier (see `SessionInfo::session_identifier`), used to find the
    /// player's session again after it was restarted. Empty (files written before this field
    /// existed) means the entry cannot be matched after the process is gone.
    #[serde(default)]
    pub session_identifier: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreRecord {
    pub version: u32,
    pub entries: Vec<RestoreEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestoreOutcome {
    pub restored: usize,
    pub left_changed: usize,
    pub gone: usize,
    /// Entries whose player process has exited (or whose recorded session is gone) and whose
    /// session identifier matches no current session. Windows persists the held volume per app,
    /// so they stay in the file until the player's session appears again and can be restored.
    pub awaiting_player: usize,
    /// Entries that could not be evaluated or restored because of an error (process, session or
    /// volume lookup, `set_volume`). They stay in the restore file for a later retry. An unreadable
    /// restore file (I/O error other than corruption) counts as 1 here and is left untouched.
    pub failed: usize,
    pub corrupt: bool,
}

/// Writes `record` to `path` atomically: temp file `<path>.tmp-<pid>`, fsync, then rename over the target.
pub fn write_atomic(path: &Path, record: &RestoreRecord) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);

    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        // On Windows std::fs::rename replaces an existing target (MOVEFILE_REPLACE_EXISTING).
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Reads the restore file. Missing file -> `Ok(None)`; unparsable or unknown version -> `InvalidData`.
pub fn read(path: &Path) -> io::Result<Option<RestoreRecord>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let record: RestoreRecord = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if record.version != RESTORE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported restore file version {}", record.version),
        ));
    }
    Ok(Some(record))
}

enum Relaunched {
    /// No session identifier recorded: nothing can be matched.
    Gone,
    Restored,
    LeftChanged,
    /// No session with the identifier exists yet.
    Awaiting,
    Failed,
}

/// Sessions matching the identifiers of a restore file's entries, looked up once per
/// `restore` call (one enumeration on Windows, however many entries need it) and only if one
/// does. A failed lookup fails every entry that needed it, as a per-entry lookup would have.
struct IdentifierLookup<'a> {
    identifiers: Vec<&'a str>,
    found: Option<Result<Vec<SessionInfo>, String>>,
}

impl<'a> IdentifierLookup<'a> {
    fn new(entries: &'a [RestoreEntry]) -> Self {
        let mut identifiers: Vec<&str> = Vec::new();
        for e in entries {
            let id = e.session_identifier.as_str();
            if !id.is_empty() && !identifiers.contains(&id) {
                identifiers.push(id);
            }
        }
        Self {
            identifiers,
            found: None,
        }
    }

    /// The current sessions with this identifier (`Err` if the one lookup failed).
    fn with(
        &mut self,
        sessions: &dyn SessionVolumes,
        identifier: &str,
    ) -> Result<Vec<SessionInfo>, ()> {
        let ids = &self.identifiers;
        match self
            .found
            .get_or_insert_with(|| sessions.sessions_with_identifiers(ids))
        {
            Ok(all) => Ok(all
                .iter()
                .filter(|s| s.session_identifier == identifier)
                .cloned()
                .collect()),
            Err(_) => Err(()),
        }
    }
}

/// The entry's process has exited, or it is alive but its recorded session is gone. Windows
/// persists per-app session volume (keyed by endpoint, executable and session GUID, not pid), so
/// a relaunched player, or a session the live player recreated, comes back at the held volume.
/// Find those sessions by the pid-free session identifier and restore the ones still held.
fn restore_relaunched(
    e: &RestoreEntry,
    sessions: &dyn SessionVolumes,
    lookup: &mut IdentifierLookup,
) -> Relaunched {
    if e.session_identifier.is_empty() {
        return Relaunched::Gone;
    }
    let found = match lookup.with(sessions, &e.session_identifier) {
        Ok(found) => found,
        Err(()) => return Relaunched::Failed,
    };
    if found.is_empty() {
        return Relaunched::Awaiting;
    }
    let (mut restored, mut failed) = (false, false);
    for s in &found {
        match sessions.volume(&s.instance_id) {
            Ok(v) if is_held(v) => match sessions.set_volume(&s.instance_id, e.original_volume) {
                Ok(()) => restored = true,
                Err(_) => failed = true,
            },
            Ok(_) => {}
            Err(_) => failed = true,
        }
    }
    if failed {
        Relaunched::Failed
    } else if restored {
        Relaunched::Restored
    } else {
        Relaunched::LeftChanged
    }
}

/// Runs [`restore_relaunched`] and records its result in `out` (and `retry` for kept entries).
fn count_relaunched(
    e: &RestoreEntry,
    sessions: &dyn SessionVolumes,
    lookup: &mut IdentifierLookup,
    out: &mut RestoreOutcome,
    retry: &mut Vec<RestoreEntry>,
) {
    match restore_relaunched(e, sessions, lookup) {
        Relaunched::Gone => out.gone += 1,
        Relaunched::Restored => out.restored += 1,
        Relaunched::LeftChanged => out.left_changed += 1,
        Relaunched::Awaiting => {
            out.awaiting_player += 1;
            retry.push(e.clone());
        }
        Relaunched::Failed => {
            out.failed += 1;
            retry.push(e.clone());
        }
    }
}

/// Restores every entry whose process (pid + creation time) and session still exist and whose
/// volume is still the held value; anything else is left as the user has it.
///
/// If the entry's process has exited (or its pid was reused), or the process is alive but the
/// recorded session no longer exists, the player's sessions are looked up by the entry's
/// pid-free `session_identifier` instead, because Windows re-applies the persisted held volume
/// to a relaunched player or a recreated session: held sessions are restored (counted in
/// `restored`), sessions the user changed are left alone (`left_changed`), and with no matching
/// session yet the entry is kept (`awaiting_player`). Entries without an identifier count as
/// `gone`.
///
/// Entries that hit an error (process/session/volume lookup, `set_volume`) are counted in
/// `failed` and kept: the file is rewritten atomically with only those entries, and deleted only
/// when none remain. A file that does not parse (or has an unknown version) is renamed to
/// `<path>.corrupt` (left in place if the rename fails) and no volume is touched. Any other
/// I/O error reading the file leaves it untouched and is reported as `failed = 1`.
pub fn restore(path: &Path, sessions: &dyn SessionVolumes) -> RestoreOutcome {
    let mut out = RestoreOutcome::default();
    let record = match read(path) {
        Ok(None) => return out,
        Ok(Some(r)) => r,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            out.corrupt = true;
            let mut name = path.as_os_str().to_owned();
            name.push(".corrupt");
            // If this fails the file stays where it is; it is never deleted unprocessed.
            let _ = std::fs::rename(path, PathBuf::from(name));
            return out;
        }
        Err(_) => {
            out.failed = 1;
            return out;
        }
    };

    let mut retry: Vec<RestoreEntry> = Vec::new();
    let mut lookup = IdentifierLookup::new(&record.entries);
    for e in &record.entries {
        match sessions.process_created(e.pid) {
            Ok(Some(created)) if created == e.created_at => {}
            Ok(_) => {
                // Process gone, or the pid now belongs to a different process.
                count_relaunched(e, sessions, &mut lookup, &mut out, &mut retry);
                continue;
            }
            Err(_) => {
                out.failed += 1;
                retry.push(e.clone());
                continue;
            }
        }
        match sessions.session(&e.instance_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                // Process alive, recorded session gone: it may have been recreated.
                count_relaunched(e, sessions, &mut lookup, &mut out, &mut retry);
                continue;
            }
            Err(_) => {
                out.failed += 1;
                retry.push(e.clone());
                continue;
            }
        }
        match sessions.volume(&e.instance_id) {
            Ok(v) if is_held(v) => match sessions.set_volume(&e.instance_id, e.original_volume) {
                Ok(()) => out.restored += 1,
                Err(_) => {
                    out.failed += 1;
                    retry.push(e.clone());
                }
            },
            Ok(_) => out.left_changed += 1,
            Err(_) => {
                out.failed += 1;
                retry.push(e.clone());
            }
        }
    }

    if retry.is_empty() {
        let _ = std::fs::remove_file(path);
    } else {
        let remaining = RestoreRecord {
            version: RESTORE_VERSION,
            entries: retry,
        };
        // If the rewrite fails the original (complete) file stays; entries already restored
        // are then seen as "not held" next time and counted as left_changed.
        let _ = write_atomic(path, &remaining);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{FakeSessions, SessionInfo};
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "devocal-restore-test-{}-{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("devocal-restore.json")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(pid: u32, created_at: u64, id: &str, vol: f32) -> RestoreEntry {
        RestoreEntry {
            pid,
            created_at,
            instance_id: id.into(),
            original_volume: vol,
            original_mute: false,
            saved_at_ms: 1_700_000_000_000,
            session_identifier: String::new(),
        }
    }

    fn add(f: &FakeSessions, id: &str, pid: u32, created: u64, volume: f32) {
        f.add_session(
            SessionInfo {
                instance_id: id.into(),
                session_identifier: String::new(),
                pid,
                endpoint_id: "ep".into(),
                active: true,
            },
            volume,
            false,
        );
        f.set_process_created(pid, Some(created));
    }

    const PLAYER: &str = "ep|\\Device\\HarddiskVolume3\\Player\\player.exe%b{00000000}";

    /// Entry for a player session (pid 10, created 111) that carries the pid-free identifier.
    fn player_entry(vol: f32) -> RestoreEntry {
        RestoreEntry {
            session_identifier: PLAYER.into(),
            ..entry(10, 111, "ep|player|1%b10", vol)
        }
    }

    /// The relaunched player: new pid 20, new instance id, same session identifier.
    fn add_relaunched(f: &FakeSessions, volume: f32) {
        f.add_session(
            SessionInfo {
                instance_id: "ep|player|1%b20".into(),
                session_identifier: PLAYER.into(),
                pid: 20,
                endpoint_id: "ep".into(),
                active: false,
            },
            volume,
            false,
        );
        f.set_process_created(20, Some(222));
    }

    fn write_entries(dir: &TempDir, entries: Vec<RestoreEntry>) {
        let rec = RestoreRecord {
            version: 1,
            entries,
        };
        write_atomic(&dir.file(), &rec).unwrap();
    }

    #[test]
    fn exited_player_is_restored_when_relaunched() {
        let dir = TempDir::new("relaunch");
        let f = FakeSessions::new(); // pid 10 is gone
        add_relaunched(&f, 1.0e-4); // Windows re-applied the persisted held volume
        write_entries(&dir, vec![player_entry(0.8)]);

        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.gone, out.awaiting_player), (1, 0, 0));
        assert_eq!(f.volume("ep|player|1%b20").unwrap(), 0.8);
        assert!(!dir.file().exists());
    }

    #[test]
    fn exited_player_is_matched_by_identifier_after_pid_reuse_too() {
        let dir = TempDir::new("relaunch-reuse");
        let f = FakeSessions::new();
        f.set_process_created(10, Some(999)); // pid 10 now belongs to another process
        add_relaunched(&f, 1.0e-4);
        write_entries(&dir, vec![player_entry(0.8)]);

        let out = restore(&dir.file(), &f);
        assert_eq!(out.restored, 1);
        assert_eq!(f.volume("ep|player|1%b20").unwrap(), 0.8);
    }

    #[test]
    fn exited_player_entry_waits_until_relaunch() {
        let dir = TempDir::new("await");
        let f = FakeSessions::new();
        write_entries(&dir, vec![player_entry(0.8)]);

        let out = restore(&dir.file(), &f);
        assert_eq!(
            (out.restored, out.gone, out.failed, out.awaiting_player),
            (0, 0, 0, 1)
        );
        assert_eq!(f.set_volume_calls(), 0);
        let kept = read(&dir.file()).unwrap().expect("file must be kept");
        assert_eq!(kept.entries, vec![player_entry(0.8)]);

        add_relaunched(&f, 1.0e-4);
        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.awaiting_player), (1, 0));
        assert!(!dir.file().exists());
    }

    #[test]
    fn relaunched_player_changed_by_user_is_left_alone() {
        let dir = TempDir::new("relaunch-changed");
        let f = FakeSessions::new();
        add_relaunched(&f, 0.4);
        write_entries(&dir, vec![player_entry(0.8)]);

        let out = restore(&dir.file(), &f);
        assert_eq!(
            (out.restored, out.left_changed, out.awaiting_player),
            (0, 1, 0)
        );
        assert_eq!(f.set_volume_calls(), 0);
        assert_eq!(f.volume("ep|player|1%b20").unwrap(), 0.4);
        assert!(!dir.file().exists());
    }

    #[test]
    fn live_player_with_recreated_session_is_restored_by_identifier() {
        // pid 10 is still the same process, but its old session is gone and a new one (same
        // identifier, new instance id) came back at the persisted held volume.
        let dir = TempDir::new("recreated");
        let f = FakeSessions::new();
        f.set_process_created(10, Some(111));
        f.add_session(
            SessionInfo {
                instance_id: "ep|player|2%b10".into(),
                session_identifier: PLAYER.into(),
                pid: 10,
                endpoint_id: "ep".into(),
                active: true,
            },
            1.0e-4,
            false,
        );
        write_entries(&dir, vec![player_entry(0.8)]);

        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.gone, out.awaiting_player), (1, 0, 0));
        assert_eq!(f.volume("ep|player|2%b10").unwrap(), 0.8);
        assert!(!dir.file().exists());
    }

    #[test]
    fn live_player_without_any_session_waits_for_it() {
        let dir = TempDir::new("live-await");
        let f = FakeSessions::new();
        f.set_process_created(10, Some(111)); // alive, but no session at all yet
        write_entries(&dir, vec![player_entry(0.8)]);

        let out = restore(&dir.file(), &f);
        assert_eq!(
            (out.restored, out.gone, out.failed, out.awaiting_player),
            (0, 0, 0, 1)
        );
        assert_eq!(f.set_volume_calls(), 0);
        let kept = read(&dir.file()).unwrap().expect("file must be kept");
        assert_eq!(kept.entries, vec![player_entry(0.8)]);
    }

    #[test]
    fn exited_player_without_identifier_is_gone() {
        // Files written before the identifier existed parse with an empty one.
        let dir = TempDir::new("legacy");
        let json = r#"{"version":1,"entries":[{"pid":10,"createdAt":111,
            "instanceId":"ep|player|1%b10","originalVolume":0.8,"originalMute":false,
            "savedAtMs":1700000000000}]}"#;
        std::fs::write(dir.file(), json).unwrap();
        let rec = read(&dir.file()).unwrap().unwrap();
        assert_eq!(rec.entries[0].session_identifier, "");

        let f = FakeSessions::new();
        add_relaunched(&f, 1.0e-4);
        let out = restore(&dir.file(), &f);
        assert_eq!((out.gone, out.restored, out.awaiting_player), (1, 0, 0));
        assert_eq!(f.set_volume_calls(), 0);
        assert!(!dir.file().exists());
    }

    #[test]
    fn awaiting_entries_cost_one_identifier_lookup_per_restore() {
        let dir = TempDir::new("one-lookup");
        let f = FakeSessions::new();
        // Five exited players' entries, on five devices the player never returns to.
        let entries: Vec<RestoreEntry> = (0..5)
            .map(|i| RestoreEntry {
                session_identifier: format!("ep{i}|player.exe%b{{0}}"),
                instance_id: format!("ep{i}|player|1%b10"),
                ..player_entry(0.8)
            })
            .collect();
        write_entries(&dir, entries);
        let out = restore(&dir.file(), &f);
        assert_eq!(out.awaiting_player, 5);
        assert_eq!(f.identifier_lookups(), 1);
        restore(&dir.file(), &f);
        assert_eq!(f.identifier_lookups(), 2, "one per call");

        // One entry's lookup failing fails every entry that needed it (all kept).
        f.fail_sessions_with_identifier("ep3|player.exe%b{0}");
        let out = restore(&dir.file(), &f);
        assert_eq!((out.failed, out.awaiting_player), (5, 0));
        assert_eq!(f.identifier_lookups(), 3);
        assert_eq!(read(&dir.file()).unwrap().unwrap().entries.len(), 5);

        // Entries whose process and session are alive need no lookup at all.
        let dir = TempDir::new("no-lookup");
        let f = FakeSessions::new();
        f.set_process_created(10, Some(111));
        f.add_session(
            SessionInfo {
                instance_id: "ep|s".into(),
                session_identifier: "ep|player".into(),
                pid: 10,
                endpoint_id: "ep".into(),
                active: true,
            },
            1.0e-4,
            false,
        );
        write_entries(
            &dir,
            vec![RestoreEntry {
                pid: 10,
                created_at: 111,
                instance_id: "ep|s".into(),
                session_identifier: "ep|player".into(),
                ..player_entry(0.8)
            }],
        );
        assert_eq!(restore(&dir.file(), &f).restored, 1);
        assert_eq!(f.identifier_lookups(), 0);
    }

    #[test]
    fn identifier_lookup_error_keeps_the_entry_for_retry() {
        let dir = TempDir::new("ident-err");
        let f = FakeSessions::new();
        add_relaunched(&f, 1.0e-4);
        f.fail_sessions_with_identifier(PLAYER);
        write_entries(&dir, vec![player_entry(0.8)]);

        let out = restore(&dir.file(), &f);
        assert_eq!((out.failed, out.restored, out.awaiting_player), (1, 0, 0));
        assert_eq!(f.set_volume_calls(), 0);
        assert!(dir.file().exists());

        f.clear_failures();
        assert_eq!(restore(&dir.file(), &f).restored, 1);
        assert!(!dir.file().exists());
    }

    #[test]
    fn inactive_endpoint_session_is_kept_for_retry() {
        // The player is alive but its session's endpoint is unplugged/disabled right now.
        let dir = TempDir::new("inactive-ep");
        let f = FakeSessions::new();
        f.add_session(
            SessionInfo {
                instance_id: "ep2|player|1%b10".into(),
                session_identifier: "ep2|player.exe%b{0}".into(),
                pid: 10,
                endpoint_id: "ep2".into(),
                active: false,
            },
            1.0e-4,
            false,
        );
        f.set_process_created(10, Some(111));
        f.set_endpoint_active("ep2", false);
        write_entries(&dir, vec![entry(10, 111, "ep2|player|1%b10", 0.8)]);

        let out = restore(&dir.file(), &f);
        assert_eq!((out.failed, out.gone, out.restored), (1, 0, 0));
        assert_eq!(f.set_volume_calls(), 0);
        let kept = read(&dir.file()).unwrap().expect("file must be kept");
        assert_eq!(kept.entries.len(), 1);

        f.set_endpoint_active("ep2", true);
        assert_eq!(restore(&dir.file(), &f).restored, 1);
        assert!(!dir.file().exists());
    }

    #[test]
    fn write_is_atomic_and_round_trips() {
        let dir = TempDir::new("roundtrip");
        let rec = RestoreRecord {
            version: 1,
            entries: vec![entry(10, 111, "a", 0.8), entry(11, 222, "b", 0.25)],
        };
        write_atomic(&dir.file(), &rec).unwrap();
        // overwrite once more to exercise the replace path
        write_atomic(&dir.file(), &rec).unwrap();

        let names: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["devocal-restore.json"]);
        assert_eq!(read(&dir.file()).unwrap(), Some(rec));
    }

    #[test]
    fn restores_only_when_still_held() {
        let dir = TempDir::new("held");
        let f = FakeSessions::new();
        add(&f, "a", 10, 111, 1.0e-4);
        add(&f, "b", 11, 222, 0.5);
        let rec = RestoreRecord {
            version: 1,
            entries: vec![entry(10, 111, "a", 0.8), entry(11, 222, "b", 0.3)],
        };
        write_atomic(&dir.file(), &rec).unwrap();

        let out = restore(&dir.file(), &f);
        assert_eq!(out.restored, 1);
        assert_eq!(out.left_changed, 1);
        assert_eq!(out.gone, 0);
        assert!(!out.corrupt);
        assert_eq!(f.volume("a").unwrap(), 0.8);
        assert_eq!(f.volume("b").unwrap(), 0.5);
        assert!(!dir.file().exists());
    }

    #[test]
    fn pid_reuse_is_ignored() {
        let dir = TempDir::new("pidreuse");
        let f = FakeSessions::new();
        add(&f, "a", 10, 999, 1.0e-4); // same pid, different creation time
        let rec = RestoreRecord {
            version: 1,
            entries: vec![entry(10, 111, "a", 0.8)],
        };
        write_atomic(&dir.file(), &rec).unwrap();

        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.left_changed, out.gone), (0, 0, 1));
        assert_eq!(f.volume("a").unwrap(), 1.0e-4);
        assert_eq!(f.set_volume_calls(), 0);
        assert!(!dir.file().exists());
    }

    #[test]
    fn missing_session_or_process_counts_as_gone() {
        let dir = TempDir::new("gone");
        let f = FakeSessions::new();
        add(&f, "a", 10, 111, 1.0e-4);
        f.remove_session("a"); // process alive, session gone
        let rec = RestoreRecord {
            version: 1,
            entries: vec![entry(10, 111, "a", 0.8), entry(12, 5, "c", 0.4)],
        };
        write_atomic(&dir.file(), &rec).unwrap();
        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.left_changed, out.gone), (0, 0, 2));
        assert_eq!(f.set_volume_calls(), 0);
    }

    #[test]
    fn missing_file_is_noop() {
        let dir = TempDir::new("missing");
        let f = FakeSessions::new();
        let out = restore(&dir.file(), &f);
        assert_eq!(out, RestoreOutcome::default());
        assert_eq!(read(&dir.file()).unwrap(), None);
    }

    #[test]
    fn corrupt_file_is_quarantined() {
        let dir = TempDir::new("corrupt");
        let f = FakeSessions::new();
        add(&f, "a", 10, 111, 1.0e-4);
        std::fs::write(dir.file(), "{oops").unwrap();

        let out = restore(&dir.file(), &f);
        assert!(out.corrupt);
        assert_eq!((out.restored, out.left_changed, out.gone), (0, 0, 0));
        assert!(!dir.file().exists());
        assert!(dir.0.join("devocal-restore.json.corrupt").exists());
        assert_eq!(f.set_volume_calls(), 0);
        assert_eq!(f.volume("a").unwrap(), 1.0e-4);
    }

    fn two_held(dir: &TempDir) -> FakeSessions {
        let f = FakeSessions::new();
        add(&f, "a", 10, 111, 1.0e-4);
        add(&f, "b", 11, 222, 1.0e-4);
        let rec = RestoreRecord {
            version: 1,
            entries: vec![entry(10, 111, "a", 0.8), entry(11, 222, "b", 0.6)],
        };
        write_atomic(&dir.file(), &rec).unwrap();
        f
    }

    #[test]
    fn volume_lookup_error_keeps_the_entry_for_retry() {
        let dir = TempDir::new("volerr");
        let f = two_held(&dir);
        f.fail_volume("a");

        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.failed, out.left_changed), (1, 1, 0));
        assert_eq!(f.set_volume_calls(), 1); // only b was written
        let kept = read(&dir.file()).unwrap().expect("file must still exist");
        assert_eq!(kept.entries, vec![entry(10, 111, "a", 0.8)]);

        f.clear_failures();
        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.failed), (1, 0));
        assert_eq!(f.volume("a").unwrap(), 0.8);
        assert_eq!(f.volume("b").unwrap(), 0.6);
        assert!(!dir.file().exists());
    }

    #[test]
    fn set_volume_error_keeps_the_entry_for_retry() {
        let dir = TempDir::new("seterr");
        let f = two_held(&dir);
        f.fail_set_volume("b");

        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.failed), (1, 1));
        assert_eq!(f.volume("a").unwrap(), 0.8);
        assert_eq!(f.volume("b").unwrap(), 1.0e-4);
        let kept = read(&dir.file()).unwrap().expect("file must still exist");
        assert_eq!(kept.entries, vec![entry(11, 222, "b", 0.6)]);

        f.clear_failures();
        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.failed), (1, 0));
        assert_eq!(f.volume("b").unwrap(), 0.6);
        assert!(!dir.file().exists());
    }

    #[test]
    fn session_lookup_error_keeps_the_entry_for_retry() {
        let dir = TempDir::new("sesserr");
        let f = two_held(&dir);
        f.fail_session("a");

        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.failed), (1, 1));
        let kept = read(&dir.file()).unwrap().expect("file must still exist");
        assert_eq!(kept.entries, vec![entry(10, 111, "a", 0.8)]);

        f.clear_failures();
        assert_eq!(restore(&dir.file(), &f).restored, 1);
        assert!(!dir.file().exists());
    }

    #[test]
    fn process_query_error_keeps_the_entry_for_retry() {
        let dir = TempDir::new("procerr");
        let f = two_held(&dir);
        f.fail_process_created(10);

        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.failed, out.gone), (1, 1, 0));
        assert_eq!(f.writes(), vec![("b".to_string(), 0.6)]);
        assert_eq!(f.volume("a").unwrap(), 1.0e-4);
        let kept = read(&dir.file()).unwrap().expect("file must still exist");
        assert_eq!(kept.entries, vec![entry(10, 111, "a", 0.8)]);

        f.clear_failures();
        let out = restore(&dir.file(), &f);
        assert_eq!((out.restored, out.failed), (1, 0));
        assert_eq!(f.volume("a").unwrap(), 0.8);
        assert!(!dir.file().exists());
    }

    #[test]
    fn unreadable_file_is_left_untouched_and_not_quarantined() {
        // A directory at the restore path makes fs::read fail with a non-InvalidData error.
        let dir = TempDir::new("unreadable");
        std::fs::create_dir(dir.file()).unwrap();
        let f = FakeSessions::new();
        add(&f, "a", 10, 111, 1.0e-4);

        let out = restore(&dir.file(), &f);
        assert!(!out.corrupt);
        assert_eq!(out.failed, 1);
        assert!(dir.file().is_dir());
        assert!(!dir.0.join("devocal-restore.json.corrupt").exists());
        assert_eq!(f.set_volume_calls(), 0);
    }

    #[test]
    fn failed_quarantine_rename_never_deletes_the_file() {
        let dir = TempDir::new("quarantine-fail");
        std::fs::write(dir.file(), "{oops").unwrap();
        // A non-empty directory at the quarantine path makes the rename fail.
        let q = dir.0.join("devocal-restore.json.corrupt");
        std::fs::create_dir(&q).unwrap();
        std::fs::write(q.join("keep"), "x").unwrap();
        let f = FakeSessions::new();

        let out = restore(&dir.file(), &f);
        assert!(out.corrupt);
        assert!(dir.file().exists());
        assert_eq!(f.set_volume_calls(), 0);
    }
}
