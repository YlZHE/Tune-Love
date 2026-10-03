//! "Installed" means present with the manifest's size and SHA-256. Hashing a 37.5 MB file is
//! noticeable, so a result is remembered per path (valid while the file's modified time and
//! size are unchanged); a size mismatch is rejected without hashing at all.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use super::manifest::{FileSpec, Manifest};

/// Models that used to live as one flat file directly under `models/`: (model id, file name).
pub const LEGACY_FLAT: &[(&str, &str)] = &[("stemgenrt-hop128", "stemgenrt-hop128.onnx")];

type Cache = HashMap<PathBuf, (SystemTime, u64, String)>;

fn lock_cache() -> MutexGuard<'static, Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Lowercase hex SHA-256 of a file, streamed.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// True if the file has the spec's size and SHA-256 (hash cached by path + mtime + size).
///
/// The cache trusts an unchanged (mtime, size). Anything that puts new bytes at a path must
/// therefore either rename a file into place (which gives a fresh entry) or call
/// [`note_verified`] right after; the downloader does both.
pub fn file_verified(path: &Path, spec: &FileSpec) -> bool {
    verified_sha256(path, spec).is_some()
}

/// [`file_verified`], returning the hash that was computed over the file (now or, for an
/// unchanged file, earlier) when it matches the spec.
pub fn verified_sha256(path: &Path, spec: &FileSpec) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() != spec.bytes {
        return None;
    }
    let Ok(mtime) = meta.modified() else {
        return sha256_file(path).ok().filter(|h| *h == spec.sha256);
    };
    if let Some((m, len, sha)) = lock_cache().get(path) {
        if *m == mtime && *len == meta.len() {
            return (*sha == spec.sha256).then(|| sha.clone());
        }
    }
    // Not holding the cache lock while hashing.
    let sha = sha256_file(path).ok()?;
    let ok = sha == spec.sha256;
    lock_cache().insert(path.to_path_buf(), (mtime, meta.len(), sha.clone()));
    ok.then_some(sha)
}

/// Remembers a hash that was just computed while installing, so the next check does not redo it.
///
/// `sha256` must be the hash the caller computed over the bytes now at `path`, never the
/// manifest's expected value: passing the expected value would let an unverified file pass.
pub fn note_verified(path: &Path, sha256: &str) {
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(mtime) = meta.modified() {
            lock_cache().insert(path.to_path_buf(), (mtime, meta.len(), sha256.to_string()));
        }
    }
}

pub fn model_dir(models_dir: &Path, id: &str) -> PathBuf {
    models_dir.join(id)
}

/// The model's files in manifest order, only if every one is present and verified.
pub fn installed_files(manifest: &Manifest, models_dir: &Path, id: &str) -> Option<Vec<PathBuf>> {
    let spec = manifest.model(id)?;
    let dir = model_dir(models_dir, id);
    spec.files
        .iter()
        .map(|f| {
            let path = dir.join(&f.file);
            file_verified(&path, f).then_some(path)
        })
        .collect()
}

/// Moves an old flat download into its model directory, but only a file that verifies and only
/// onto an empty spot. Anything else is left exactly where it is. Returns the migrated ids.
pub fn migrate_legacy(manifest: &Manifest, models_dir: &Path) -> Vec<String> {
    let mut migrated = Vec::new();
    for (id, flat_name) in LEGACY_FLAT {
        let Some(spec) = manifest.model(id) else {
            continue;
        };
        let [file] = spec.files.as_slice() else {
            continue;
        };
        let flat = models_dir.join(flat_name);
        let dir = model_dir(models_dir, id);
        let target = dir.join(&file.file);
        if !flat.is_file() || target.exists() {
            continue;
        }
        let Some(sha256) = verified_sha256(&flat, file) else {
            continue;
        };
        if std::fs::create_dir_all(&dir).is_err() || std::fs::rename(&flat, &target).is_err() {
            continue;
        }
        // The hash computed over the file, as `note_verified` requires (not the manifest value).
        note_verified(&target, &sha256);
        migrated.push((*id).to_string());
    }
    migrated
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::devocal::tests::temp_dir;

    /// A one-model manifest (`stemgenrt-hop128`, file `model.onnx`) whose file is `content`.
    pub fn fake_manifest(content: &[u8]) -> Manifest {
        let sha = format!("{:x}", Sha256::digest(content));
        let json = serde_json::json!({
            "version": 1,
            "mirrors": [],
            "models": [{
                "id": "stemgenrt-hop128", "name": "t", "tier": "realtime",
                "files": [{
                    "file": "model.onnx", "bytes": content.len(), "sha256": sha,
                    "origin": "https://example.com/model.onnx", "mirrorable": false
                }],
                "sampleRate": 44100, "latencyMs": 5.8, "runtime": "cpu",
                "license": { "code": "MIT", "weights": "pending", "trainingData": [] },
                "source": "https://example.com"
            }]
        });
        Manifest::parse(&json.to_string()).unwrap()
    }

    /// Writes `content` and moves the modified time `secs` ahead, so a cached hash of the
    /// previous content (same size, possibly the same coarse timestamp) cannot be reused.
    fn rewrite(path: &Path, content: &[u8], secs: u64) {
        std::fs::write(path, content).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let later = std::fs::metadata(path).unwrap().modified().unwrap()
            + std::time::Duration::from_secs(secs);
        f.set_modified(later).unwrap();
    }

    const GOOD: &[u8] = b"good model bytes";
    const ID: &str = "stemgenrt-hop128";

    #[test]
    fn installed_requires_size_and_hash() {
        let m = fake_manifest(GOOD);
        let models = temp_dir("verify-installed");
        assert_eq!(installed_files(&m, &models, ID), None, "missing");
        let file = model_dir(&models, ID).join("model.onnx");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"short").unwrap();
        assert_eq!(installed_files(&m, &models, ID), None, "wrong size");
        let mut bad = GOOD.to_vec();
        bad[3] ^= 1;
        rewrite(&file, &bad, 2);
        assert_eq!(installed_files(&m, &models, ID), None, "wrong content");
        rewrite(&file, GOOD, 4);
        assert_eq!(installed_files(&m, &models, ID), Some(vec![file]));
        assert_eq!(installed_files(&m, &models, "unknown"), None);
    }

    #[test]
    fn cache_rehashes_when_mtime_changes() {
        let m = fake_manifest(GOOD);
        let models = temp_dir("verify-cache");
        let file = model_dir(&models, ID).join("model.onnx");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, GOOD).unwrap();
        assert!(installed_files(&m, &models, ID).is_some());
        let mut bad = GOOD.to_vec();
        bad[0] ^= 1;
        rewrite(&file, &bad, 2);
        assert_eq!(installed_files(&m, &models, ID), None);
    }

    #[test]
    fn migrate_moves_a_verified_flat_file_into_the_model_dir() {
        let m = fake_manifest(GOOD);
        let models = temp_dir("verify-migrate");
        let flat = models.join("stemgenrt-hop128.onnx");
        std::fs::write(&flat, GOOD).unwrap();
        assert_eq!(migrate_legacy(&m, &models), vec![ID.to_string()]);
        assert!(!flat.exists());
        let target = models.join(ID).join("model.onnx");
        assert_eq!(std::fs::read(&target).unwrap(), GOOD);
        assert_eq!(installed_files(&m, &models, ID), Some(vec![target]));
        assert!(migrate_legacy(&m, &models).is_empty(), "nothing left to migrate");
    }

    #[test]
    fn migrate_leaves_bad_or_conflicting_files_alone() {
        let m = fake_manifest(GOOD);
        let models = temp_dir("verify-migrate-bad");
        let flat = models.join("stemgenrt-hop128.onnx");
        let target = models.join(ID).join("model.onnx");
        let mut bad = GOOD.to_vec();
        bad[2] ^= 1;
        std::fs::write(&flat, &bad).unwrap();
        assert!(migrate_legacy(&m, &models).is_empty());
        assert_eq!(std::fs::read(&flat).unwrap(), bad, "bad file stays put");
        assert!(!target.exists());

        std::fs::write(&flat, GOOD).unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"existing").unwrap();
        assert!(migrate_legacy(&m, &models).is_empty());
        assert_eq!(std::fs::read(&flat).unwrap(), GOOD);
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
    }

    #[test]
    fn verified_sha256_returns_the_hash_it_computed() {
        let m = fake_manifest(GOOD);
        let spec = &m.model(ID).unwrap().files[0];
        let dir = temp_dir("verify-sha");
        let file = dir.join("model.onnx");
        std::fs::write(&file, GOOD).unwrap();
        let computed = format!("{:x}", Sha256::digest(GOOD));
        assert_eq!(verified_sha256(&file, spec), Some(computed.clone()));
        // Cached the second time; still the computed value.
        assert_eq!(verified_sha256(&file, spec), Some(computed));
        let mut bad = GOOD.to_vec();
        bad[1] ^= 1;
        rewrite(&file, &bad, 2);
        assert_eq!(verified_sha256(&file, spec), None);
        assert_eq!(verified_sha256(&dir.join("missing.onnx"), spec), None);
    }
}
