//! One-time migration of the per-song key cache from the pre-rename data
//! folder (`dev.autotunehelper.nowplaying`) to the Tune Love data folder.

use std::fs;
use std::path::Path;

pub const LEGACY_IDENTIFIER: &str = "dev.autotunehelper.nowplaying";
pub const SONG_CACHE_FILE: &str = "song-keys-v1.json";

#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    Copied,
    AlreadyPresent,
    NoLegacy,
    Failed(String),
}

/// Copy the legacy cache into `new_dir` when the new location has none.
/// The legacy folder is a sibling of `new_dir`. Never overwrites, never
/// deletes the legacy file, and never validates the content.
pub fn migrate_song_cache(new_dir: &Path) -> Migration {
    let target = new_dir.join(SONG_CACHE_FILE);
    if target.exists() {
        return Migration::AlreadyPresent;
    }
    let Some(parent) = new_dir.parent() else {
        return Migration::NoLegacy;
    };
    let legacy = parent.join(LEGACY_IDENTIFIER).join(SONG_CACHE_FILE);
    if !legacy.is_file() {
        return Migration::NoLegacy;
    }
    if let Err(e) = fs::create_dir_all(new_dir) {
        return Migration::Failed(format!("create {}: {e}", new_dir.display()));
    }
    // Copy to a temp name first so a crash never leaves a half-written cache
    // under the real file name.
    let temp = new_dir.join(format!("{SONG_CACHE_FILE}.migrating"));
    let result = fs::copy(&legacy, &temp)
        .map_err(|e| format!("copy {}: {e}", legacy.display()))
        .and_then(|_| {
            fs::rename(&temp, &target).map_err(|e| format!("rename {}: {e}", temp.display()))
        });
    match result {
        Ok(()) => Migration::Copied,
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Migration::Failed(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Sandbox {
        base: PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("tunelove-migrate-{}-{}", std::process::id(), name));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }
        fn new_dir(&self) -> PathBuf {
            self.base.join("io.github.ylzhe.tunelove")
        }
        fn legacy_dir(&self) -> PathBuf {
            self.base.join(LEGACY_IDENTIFIER)
        }
        fn write_legacy(&self, content: &str) {
            fs::create_dir_all(self.legacy_dir()).unwrap();
            fs::write(self.legacy_dir().join(SONG_CACHE_FILE), content).unwrap();
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn copies_when_new_is_empty() {
        let sb = Sandbox::new("copies");
        sb.write_legacy(r#"{"v":1}"#);
        assert_eq!(migrate_song_cache(&sb.new_dir()), Migration::Copied);
        assert_eq!(
            fs::read_to_string(sb.new_dir().join(SONG_CACHE_FILE)).unwrap(),
            r#"{"v":1}"#
        );
        assert!(sb.legacy_dir().join(SONG_CACHE_FILE).exists());
        assert!(!sb
            .new_dir()
            .join(format!("{SONG_CACHE_FILE}.migrating"))
            .exists());
    }

    #[test]
    fn never_overwrites_existing_new() {
        let sb = Sandbox::new("nooverwrite");
        sb.write_legacy(r#"{"v":1}"#);
        fs::create_dir_all(sb.new_dir()).unwrap();
        fs::write(sb.new_dir().join(SONG_CACHE_FILE), r#"{"v":2}"#).unwrap();
        assert_eq!(migrate_song_cache(&sb.new_dir()), Migration::AlreadyPresent);
        assert_eq!(
            fs::read_to_string(sb.new_dir().join(SONG_CACHE_FILE)).unwrap(),
            r#"{"v":2}"#
        );
    }

    #[test]
    fn no_legacy_is_noop() {
        let sb = Sandbox::new("nolegacy");
        assert_eq!(migrate_song_cache(&sb.new_dir()), Migration::NoLegacy);
        assert!(!sb.new_dir().exists());
    }

    #[test]
    fn corrupt_legacy_is_copied_verbatim() {
        let sb = Sandbox::new("corrupt");
        sb.write_legacy("{oops");
        assert_eq!(migrate_song_cache(&sb.new_dir()), Migration::Copied);
        assert_eq!(
            fs::read_to_string(sb.new_dir().join(SONG_CACHE_FILE)).unwrap(),
            "{oops"
        );
    }

    #[test]
    fn unwritable_new_dir_fails_cleanly() {
        let sb = Sandbox::new("unwritable");
        sb.write_legacy(r#"{"v":1}"#);
        // A plain file occupies the new directory's path, so creating it fails.
        fs::write(sb.new_dir(), "not a directory").unwrap();
        assert!(matches!(migrate_song_cache(&sb.new_dir()), Migration::Failed(_)));
        assert!(sb.new_dir().is_file());
        assert!(!sb
            .new_dir()
            .join(format!("{SONG_CACHE_FILE}.migrating"))
            .exists());
    }
}
