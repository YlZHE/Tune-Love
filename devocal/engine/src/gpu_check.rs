//! GPU parity self-check (spec 6): before a window model is used on the GPU, one built-in
//! test window runs on the GPU and on the CPU; the GPU is usable only if its vocals match the
//! CPU's (SDR >= 40 dB). Results are cached in `<models_dir>/gpu-check.json` by adapter
//! description + driver version + model SHA-256; without an adapter key nothing is cached.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use crate::dsp::SAMPLE_RATE;
use crate::windowed::WindowModel;

pub const PASS_SDR_DB: f64 = 40.0;
const CACHE_FILE: &str = "gpu-check.json";

/// Runs (or looks up) the self-check. `run_gpu` / `run_cpu` take the interleaved stereo test
/// window and return the vocals; `run_cpu` is only called on a cache miss.
pub fn gpu_check(
    models_dir: &Path,
    model_path: &Path,
    run_gpu: impl FnOnce(&[f32]) -> Result<Vec<f32>, String>,
    run_cpu: impl FnOnce(&[f32]) -> Result<Vec<f32>, String>,
) -> bool {
    check_with_adapter(adapter_key(), models_dir, model_path, run_gpu, run_cpu)
}

fn check_with_adapter(
    adapter: Option<String>,
    models_dir: &Path,
    model_path: &Path,
    run_gpu: impl FnOnce(&[f32]) -> Result<Vec<f32>, String>,
    run_cpu: impl FnOnce(&[f32]) -> Result<Vec<f32>, String>,
) -> bool {
    let cache_path = models_dir.join(CACHE_FILE);
    let key = adapter.and_then(|a| Some(format!("{a}|{}", model_sha256(model_path)?)));
    // Missing or corrupt: start over (and rewrite it below).
    let mut cache: HashMap<String, bool> = fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Some(&pass) = key.as_ref().and_then(|k| cache.get(k)) {
        return pass;
    }
    let input = test_signal();
    let Ok(gpu) = run_gpu(&input) else {
        return false;
    };
    let Ok(cpu) = run_cpu(&input) else {
        return false;
    };
    let pass = sdr_db(&cpu, &gpu) >= PASS_SDR_DB;
    if let Some(k) = key {
        cache.insert(k, pass);
        if let Ok(json) = serde_json::to_vec_pretty(&cache) {
            // A write failure only means checking again next time.
            let _ = fs::write(&cache_path, json);
        }
    }
    pass
}

/// SDR of `test` against `reference` in dB; -inf for a length mismatch, non-finite output or
/// a silent reference (proves nothing), +inf for identical outputs.
fn sdr_db(reference: &[f32], test: &[f32]) -> f64 {
    if reference.len() != test.len() || !test.iter().all(|v| v.is_finite()) {
        return f64::NEG_INFINITY;
    }
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&r, &t) in reference.iter().zip(test) {
        sig += f64::from(r) * f64::from(r);
        err += (f64::from(r) - f64::from(t)).powi(2);
    }
    if sig == 0.0 {
        f64::NEG_INFINITY
    } else if err == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (sig / err).log10()
    }
}

fn model_sha256(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hash = hmac_sha256::Hash::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut buf).ok()? {
            0 => break,
            n => hash.update(&buf[..n]),
        }
    }
    Some(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Description and UMD driver version of DXGI adapter 0 (the one DirectML uses by default).
#[cfg(windows)]
fn adapter_key() -> Option<String> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1};
    // SAFETY: plain DXGI queries on interfaces this function owns.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        let version = adapter.CheckInterfaceSupport(&IDXGIDevice::IID).ok()? as u64;
        let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
        Some(format!(
            "{} {:04x}:{:04x}|{}.{}.{}.{}",
            String::from_utf16_lossy(&desc.Description[..len]),
            desc.VendorId,
            desc.DeviceId,
            version >> 48,
            (version >> 32) & 0xffff,
            (version >> 16) & 0xffff,
            version & 0xffff
        ))
    }
}

#[cfg(not(windows))]
fn adapter_key() -> Option<String> {
    None
}

/// The built-in test window: 1 s of deterministic stereo, a voice-like harmonic tone with
/// vibrato over a chord and a little noise (so a separator puts energy in its vocals).
fn test_signal() -> Vec<f32> {
    use std::f32::consts::TAU;
    let sr = SAMPLE_RATE as f32;
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
        // xorshift32
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32 - 0.5
    };
    let mut phase = 0.0f32;
    let mut out = Vec::with_capacity(SAMPLE_RATE as usize * 2);
    for i in 0..SAMPLE_RATE as usize {
        let t = i as f32 / sr;
        phase += TAU * 220.0 * (1.0 + 0.01 * (TAU * 5.5 * t).sin()) / sr;
        let voice: f32 = (1..=6).map(|h| (phase * h as f32).sin() / h as f32).sum();
        let chord: f32 = [110.0, 164.8, 261.6]
            .iter()
            .map(|f| (TAU * f * t).sin())
            .sum();
        let n = 0.02 * noise();
        out.push(0.25 * voice + 0.08 * chord + n);
        out.push(0.25 * voice - 0.08 * chord + n);
    }
    out
}

/// One window through `model`, as the self-check's runner.
pub fn run_window(model: &mut dyn WindowModel, input: &[f32]) -> Result<Vec<f32>, String> {
    let mut out = vec![0.0; input.len()];
    model.run(input, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "devocal-gpu-check-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("model.onnx"), b"model bytes v1").unwrap();
            TempDir(p)
        }
        fn model(&self) -> PathBuf {
            self.0.join("model.onnx")
        }
        fn cache(&self) -> PathBuf {
            self.0.join("gpu-check.json")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ADAPTER: &str = "Fake GPU|31.0.101.5186";

    /// Runs the check with a GPU runner that returns the reference plus `noise` (relative
    /// amplitude) and counts its calls.
    fn check(dir: &TempDir, adapter: Option<&str>, noise: f32, gpu_calls: &Cell<u32>) -> bool {
        check_with_adapter(
            adapter.map(str::to_string),
            &dir.0,
            &dir.model(),
            |x| {
                gpu_calls.set(gpu_calls.get() + 1);
                Ok(x.iter()
                    .enumerate()
                    .map(|(i, v)| v + noise * if i % 2 == 0 { 0.5 } else { -0.5 })
                    .collect())
            },
            |x| Ok(x.to_vec()),
        )
    }

    /// Reads DXGI adapter 0 (no GPU work): `cargo test -p devocal-engine adapter_key_reads_dxgi
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn adapter_key_reads_dxgi() {
        let key = adapter_key().expect("DXGI adapter 0");
        eprintln!("adapter key: {key}");
        assert!(key.contains('|'));
    }

    #[test]
    fn test_signal_is_one_second_of_stereo_deterministic_and_not_silent() {
        let s = test_signal();
        assert_eq!(s.len(), 44_100 * 2);
        assert_eq!(s, test_signal());
        assert!(s.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
        let rms = (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt();
        assert!(rms > 0.05, "rms {rms}");
    }

    #[test]
    fn check_passes_and_caches() {
        let dir = TempDir::new("pass");
        let calls = Cell::new(0);
        assert!(check(&dir, Some(ADAPTER), 0.0, &calls));
        assert_eq!(calls.get(), 1);
        assert!(dir.cache().exists());
        // Cached: the GPU (and the CPU) do not run again.
        let again = check_with_adapter(
            Some(ADAPTER.into()),
            &dir.0,
            &dir.model(),
            |_| panic!("run_gpu on a cache hit"),
            |_| panic!("run_cpu on a cache hit"),
        );
        assert!(again);
    }

    #[test]
    fn check_fails_below_40db() {
        let dir = TempDir::new("fail");
        let calls = Cell::new(0);
        // Error 0.5 * 0.05 against a signal of rms > 0.05: well under 40 dB.
        assert!(!check(&dir, Some(ADAPTER), 0.05, &calls));
        // A small error (about 60 dB down) passes.
        let dir = TempDir::new("near");
        assert!(check(&dir, Some(ADAPTER), 1e-4, &calls));
        // The failure is cached too.
        let dir = TempDir::new("fail-cached");
        assert!(!check(&dir, Some(ADAPTER), 0.05, &calls));
        let n = calls.get();
        assert!(!check(&dir, Some(ADAPTER), 0.0, &calls));
        assert_eq!(calls.get(), n, "cached failure");
    }

    #[test]
    fn corrupt_cache_rechecks() {
        let dir = TempDir::new("corrupt");
        std::fs::write(dir.cache(), b"{ not json").unwrap();
        let calls = Cell::new(0);
        assert!(check(&dir, Some(ADAPTER), 0.0, &calls));
        assert_eq!(calls.get(), 1);
        // Rewritten: a hit now.
        assert!(check(&dir, Some(ADAPTER), 0.0, &calls));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn new_driver_rechecks() {
        let dir = TempDir::new("driver");
        let calls = Cell::new(0);
        assert!(check(&dir, Some(ADAPTER), 0.0, &calls));
        assert!(!check(&dir, Some("Fake GPU|32.0.0.1"), 0.05, &calls));
        assert_eq!(calls.get(), 2, "new driver: checked again");
        // Each key keeps its own result.
        assert!(check(&dir, Some(ADAPTER), 0.05, &calls));
        assert!(!check(&dir, Some("Fake GPU|32.0.0.1"), 0.0, &calls));
        assert_eq!(calls.get(), 2);
        // A changed model file is a new key as well.
        std::fs::write(dir.model(), b"model bytes v2").unwrap();
        assert!(check(&dir, Some(ADAPTER), 0.0, &calls));
        assert_eq!(calls.get(), 3);
    }

    /// A silent reference proves nothing: identical all-zero outputs fail.
    #[test]
    fn a_silent_reference_fails() {
        let dir = TempDir::new("silent");
        let zeros = |x: &[f32]| Ok(vec![0.0; x.len()]);
        assert!(!check_with_adapter(
            Some(ADAPTER.into()),
            &dir.0,
            &dir.model(),
            zeros,
            zeros
        ));
    }

    #[test]
    fn unknown_adapter_rechecks_every_time() {
        let dir = TempDir::new("unknown");
        let calls = Cell::new(0);
        assert!(check(&dir, None, 0.0, &calls));
        assert!(check(&dir, None, 0.0, &calls));
        assert_eq!(calls.get(), 2);
        assert!(
            !dir.cache().exists(),
            "nothing cached without an adapter key"
        );
    }

    #[test]
    fn runner_errors_and_bad_output_fail_without_caching_errors() {
        let dir = TempDir::new("errors");
        let (a, m) = (Some(ADAPTER.to_string()), dir.model());
        assert!(!check_with_adapter(
            a.clone(),
            &dir.0,
            &m,
            |_| Err("dml".into()),
            |_| panic!("no CPU run after a GPU error")
        ));
        assert!(!check_with_adapter(
            a.clone(),
            &dir.0,
            &m,
            |x| Ok(x.to_vec()),
            |_| Err("cpu".into())
        ));
        assert!(!dir.cache().exists(), "runner errors are not cached");
        // Non-finite or wrong-length output is a (cached) failure.
        assert!(!check_with_adapter(
            a.clone(),
            &dir.0,
            &m,
            |x| Ok(x.iter().map(|_| f32::NAN).collect()),
            |x| Ok(x.to_vec())
        ));
        let dir = TempDir::new("short");
        assert!(!check_with_adapter(
            a,
            &dir.0,
            &dir.model(),
            |x| Ok(x[1..].to_vec()),
            |x| Ok(x.to_vec())
        ));
    }
}
