//! Per-song memory of the analysed Key/Scale, so a song played again can be
//! written before its vocal starts. Local only: a small JSON file under the
//! app's local data directory, keyed by normalized title/artist/album (not by
//! player). Bounded, written atomically, and tolerant of a damaged file.
//!
//! File version 2 adds the candidate name and allows Chromatic ("uncertain")
//! results. Version 1 files are still read (entries get no candidate) and are
//! rewritten as version 2 on the next change. Downgrade: an older app that only
//! knows version 1 treats a version 2 file as damaged, moves it aside as
//! `.json.damaged` and starts empty, so going back loses the remembered songs.
use super::scale_match::{AutoTuneTarget, Candidate, Scale};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

/// Only remember a result backed by this much non-silent analysis.
pub const MIN_CACHE_EVIDENCE_SECONDS: f64 = 30.0;
const MAX_ENTRIES: usize = 5_000;
const FILE_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedKey {
    pub key: u8,
    pub scale: Scale,
    /// The set's Major/Minor name; always present for Chromatic, None in version 1 entries.
    #[serde(default)]
    pub candidate: Option<Candidate>,
    pub evidence_seconds: f64,
    pub updated_ms: u64,
}

impl CachedKey {
    fn is_valid(&self) -> bool {
        self.key < 12
            && self.evidence_seconds.is_finite()
            && self
                .candidate
                .map_or(true, |c| c.key < 12 && c.scale != Scale::Chromatic)
            && (self.scale != Scale::Chromatic || self.candidate.is_some())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FileEntry {
    id: String,
    #[serde(flatten)]
    value: CachedKey,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheFile {
    version: u32,
    entries: Vec<FileEntry>,
}

/// Player-independent song identity from the media `track_key`
/// (`["sourceId","title","artist","album"]`). None without a title.
pub fn song_id(track_key: &str) -> Option<String> {
    let parts: Vec<String> = serde_json::from_str(track_key).ok()?;
    if parts.len() != 4 {
        return None;
    }
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let (title, artist, album) = (norm(&parts[1]), norm(&parts[2]), norm(&parts[3]));
    if title.is_empty() {
        return None;
    }
    Some(format!("{title}\u{1f}{artist}\u{1f}{album}"))
}

#[derive(Default)]
pub struct SongCache {
    path: Option<PathBuf>,
    entries: HashMap<String, CachedKey>,
}

impl SongCache {
    /// Load from `path`; a missing file starts empty, a damaged one is moved aside.
    pub fn load(path: PathBuf) -> Self {
        let entries = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<CacheFile>(&bytes) {
                Ok(file) if (1..=FILE_VERSION).contains(&file.version) => file
                    .entries
                    .into_iter()
                    .filter(|e| e.value.is_valid())
                    .map(|e| (e.id, e.value))
                    .collect(),
                _ => {
                    let _ = fs::rename(&path, path.with_extension("json.damaged"));
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };
        Self {
            path: Some(path),
            entries,
        }
    }

    pub fn get(&self, id: &str) -> Option<CachedKey> {
        self.entries.get(id).copied()
    }

    /// Remember `target` for `id` if it is well supported and differs from what
    /// is stored. Returns the serialized file to persist, or None if unchanged.
    pub fn put(&mut self, id: &str, target: &AutoTuneTarget, now_ms: u64) -> Option<Vec<u8>> {
        // A key-less target (no evidence yet) is never stored; Chromatic keeps its candidate.
        let Some(key) = target.key else {
            return None;
        };
        if target.scale == Scale::Chromatic && target.candidate.is_none() {
            return None;
        }
        if target.evidence_seconds < MIN_CACHE_EVIDENCE_SECONDS
            || !target.evidence_seconds.is_finite()
        {
            return None;
        }
        if self.entries.get(id).is_some_and(|e| {
            e.key == key && e.scale == target.scale && e.candidate == target.candidate
        }) {
            return None;
        }
        self.entries.insert(
            id.to_owned(),
            CachedKey {
                key,
                scale: target.scale,
                candidate: target.candidate,
                evidence_seconds: target.evidence_seconds,
                updated_ms: now_ms,
            },
        );
        if self.entries.len() > MAX_ENTRIES {
            let mut ages: Vec<(u64, String)> = self
                .entries
                .iter()
                .map(|(k, v)| (v.updated_ms, k.clone()))
                .collect();
            ages.sort();
            for (_, old) in ages.into_iter().take(self.entries.len() - MAX_ENTRIES) {
                self.entries.remove(&old);
            }
        }
        self.path.as_ref()?;
        let mut entries: Vec<FileEntry> = self
            .entries
            .iter()
            .map(|(id, value)| FileEntry {
                id: id.clone(),
                value: *value,
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        serde_json::to_vec(&CacheFile {
            version: FILE_VERSION,
            entries,
        })
        .ok()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// Atomic replace: write a sibling temp file, then rename over the target.
pub fn persist(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(key: u8, scale: Scale, seconds: f64) -> AutoTuneTarget {
        AutoTuneTarget {
            key: Some(key),
            scale,
            candidate: None,
            uncovered_notes: Vec::new(),
            evidence_seconds: seconds,
            source: super::super::scale_match::TargetSource::Analysis,
        }
    }

    fn track(source: &str, title: &str, artist: &str, album: &str) -> String {
        serde_json::to_string(&[source, title, artist, album]).unwrap()
    }

    #[test]
    fn song_id_ignores_player_case_and_spacing_and_needs_a_title() {
        let a = song_id(&track("NetEase", "泣", "SASIOVERLXRD", "Album")).unwrap();
        let b = song_id(&track("Spotify", " 泣 ", "sasioverlxrd", "album")).unwrap();
        assert_eq!(a, b);
        assert!(song_id(&track("x", "  ", "a", "b")).is_none());
        assert!(song_id("not json").is_none());
        assert!(song_id("[\"a\",\"b\"]").is_none());
    }

    #[test]
    fn put_requires_evidence_and_only_reports_changes() {
        let mut cache = SongCache {
            path: Some(PathBuf::from("unused.json")),
            ..Default::default()
        };
        assert!(cache.put("s", &target(6, Scale::Minor, 10.0), 1).is_none());
        assert!(cache.get("s").is_none());
        assert!(cache.put("s", &target(6, Scale::Minor, 40.0), 2).is_some());
        assert!(
            cache.put("s", &target(6, Scale::Minor, 90.0), 3).is_none(),
            "same pair is not rewritten"
        );
        assert!(cache.put("s", &target(1, Scale::Major, 90.0), 4).is_some());
        assert_eq!(
            cache.get("s").map(|c| (c.key, c.scale)),
            Some((1, Scale::Major))
        );
    }

    fn chromatic_target(key: u8, candidate: Option<Candidate>, seconds: f64) -> AutoTuneTarget {
        AutoTuneTarget {
            scale: Scale::Chromatic,
            candidate,
            ..target(key, Scale::Chromatic, seconds)
        }
    }

    fn eb_major() -> Option<Candidate> {
        Some(Candidate {
            key: 3,
            scale: Scale::Major,
        })
    }

    #[test]
    fn put_stores_chromatic_with_candidate() {
        let mut cache = SongCache {
            path: Some(PathBuf::from("unused.json")),
            ..Default::default()
        };
        assert!(cache
            .put("s", &chromatic_target(3, eb_major(), 40.0), 1)
            .is_some());
        let stored = cache.get("s").unwrap();
        assert_eq!((stored.key, stored.scale), (3, Scale::Chromatic));
        assert_eq!(stored.candidate, eb_major());
        assert!(
            cache
                .put("s", &chromatic_target(3, eb_major(), 90.0), 2)
                .is_none(),
            "same key, scale and candidate is not rewritten"
        );
        let renamed = Some(Candidate {
            key: 0,
            scale: Scale::Minor,
        });
        assert!(
            cache
                .put("s", &chromatic_target(0, renamed, 90.0), 3)
                .is_some(),
            "a different candidate is a change"
        );
        let mut keyless = chromatic_target(3, None, 90.0);
        keyless.key = None;
        assert!(cache.put("t", &keyless, 4).is_none());
        assert!(cache.get("t").is_none());
    }

    #[test]
    fn reads_version_1_files() {
        let dir = std::env::temp_dir().join(format!("song-cache-v1-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song-keys-v1.json");
        fs::write(
            &path,
            br#"{"version":1,"entries":[{"id":"old","key":6,"scale":"minor","evidenceSeconds":40.0,"updatedMs":3}]}"#,
        )
        .unwrap();
        let cache = SongCache::load(path.clone());
        let hit = cache.get("old").expect("v1 entry is kept");
        assert_eq!((hit.key, hit.scale, hit.candidate), (6, Scale::Minor, None));
        assert!(!dir.join("song-keys-v1.json.damaged").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_version_2() {
        let dir = std::env::temp_dir().join(format!("song-cache-v2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("song-keys-v1.json");
        let mut cache = SongCache::load(path.clone());
        cache.put("a", &chromatic_target(3, eb_major(), 45.0), 7);
        let bytes = cache.put("b", &target(9, Scale::Minor, 45.0), 8).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("\"version\":2"));
        persist(&path, &bytes).unwrap();
        let loaded = SongCache::load(path);
        let a = loaded.get("a").unwrap();
        assert_eq!(
            (a.key, a.scale, a.candidate, a.updated_ms),
            (3, Scale::Chromatic, eb_major(), 7)
        );
        assert_eq!(loaded.get("b").unwrap().candidate, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_through_disk_and_moves_a_damaged_file_aside() {
        let dir = std::env::temp_dir().join(format!("song-cache-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("song-keys-v1.json");
        let mut cache = SongCache::load(path.clone());
        let bytes = cache
            .put("song", &target(9, Scale::Minor, 45.0), 7)
            .unwrap();
        persist(&path, &bytes).unwrap();
        let loaded = SongCache::load(path.clone());
        assert_eq!(
            loaded.get("song").map(|c| (c.key, c.scale, c.updated_ms)),
            Some((9, Scale::Minor, 7))
        );

        fs::write(&path, b"{broken").unwrap();
        let recovered = SongCache::load(path.clone());
        assert!(recovered.get("song").is_none());
        assert!(dir.join("song-keys-v1.json.damaged").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evicts_the_oldest_entries_beyond_the_bound() {
        let mut cache = SongCache {
            path: Some(PathBuf::from("unused.json")),
            ..Default::default()
        };
        for i in 0..(MAX_ENTRIES as u64 + 3) {
            cache.put(&format!("s{i}"), &target(0, Scale::Major, 40.0), i);
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(cache.get("s0").is_none() && cache.get("s2").is_none());
        assert!(cache.get(&format!("s{}", MAX_ENTRIES + 2)).is_some());
    }
}
