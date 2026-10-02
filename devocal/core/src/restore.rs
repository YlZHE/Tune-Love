//! Crash-safe restore file. Written atomically before any player session volume is
//! lowered; whoever survives (engine, app, next app start) restores from it.

use crate::sessions::{is_held, SessionVolumes};
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
    /// Entries that could not be evaluated or restored because of an error (session or volume
    /// lookup, `set_volume`). They stay in the restore file for a later retry. An unreadable
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

/// Restores every entry whose process (pid + creation time) and session still exist and whose
/// volume is still the held value; anything else is left as the user has it.
///
/// Entries that hit an error (session/volume lookup, `set_volume`) are counted in `failed` and
/// kept: the file is rewritten atomically with only those entries, and deleted only when none
/// remain. A file that does not parse (or has an unknown version) is renamed to
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
    for e in &record.entries {
        if sessions.process_created(e.pid) != Some(e.created_at) {
            out.gone += 1;
            continue;
        }
        match sessions.session(&e.instance_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                out.gone += 1;
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
        }
    }

    fn add(f: &FakeSessions, id: &str, pid: u32, created: u64, volume: f32) {
        f.add_session(
            SessionInfo {
                instance_id: id.into(),
                pid,
                endpoint_id: "ep".into(),
                active: true,
            },
            volume,
            false,
        );
        f.set_process_created(pid, Some(created));
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
