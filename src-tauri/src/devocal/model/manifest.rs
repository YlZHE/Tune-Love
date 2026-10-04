//! The single model manifest (`src-tauri/models.json`), shared with `scripts/fetch-stemgenrt.mjs`
//! and the licence text, plus the ordered download sources for each file.

use std::collections::HashSet;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub const STEMGENRT_ID: &str = "stemgenrt-hop128";

const GITHUB_PREFIX: &str = "https://github.com/";
const MAX_PREFIX_LEN: usize = 200;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: u32,
    pub mirrors: Vec<String>,
    pub models: Vec<ModelSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSpec {
    pub id: String,
    pub name: String,
    pub tier: Tier,
    pub files: Vec<FileSpec>,
    pub kind: ModelKind,
    pub sample_rate: u32,
    /// Index of the vocals stem in the model output. Streaming models ignore it (0).
    pub vocals_index: u32,
    pub devices: Devices,
    pub license: LicenseInfo,
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSpec {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    pub origin: String,
    pub mirrorable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Realtime,
    Quality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    /// Frame-by-frame model with its own state (StemgenRT): no window, hop or lookahead.
    Streaming,
    /// Fixed-length window model run on overlapping hops by the windowed separator.
    Windowed,
}

/// Per-device run parameters; a missing device means the model does not run there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Devices {
    #[serde(default)]
    pub cpu: Option<DeviceParams>,
    #[serde(default)]
    pub gpu: Option<DeviceParams>,
}

/// Windowed models: `latency_ms = hop + lookahead + 0.3 * hop`. Streaming models use 0 for the window fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceParams {
    pub window_ms: u32,
    pub hop_ms: u32,
    pub lookahead_ms: u32,
    pub threads: u16,
    pub latency_ms: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseInfo {
    pub code: String,
    pub weights: String,
    pub training_data: Vec<String>,
    /// The weights were converted and modified by this project (shown in the consent box).
    #[serde(default)]
    pub converted: bool,
    /// Extra credits shown with the conversion note (authors, tools); empty if none.
    #[serde(default)]
    pub credit: String,
}

impl Manifest {
    /// Parses and validates a manifest; any unsafe or malformed entry is an error.
    pub fn parse(json: &str) -> Result<Manifest, String> {
        let m: Manifest = serde_json::from_str(json).map_err(|e| format!("manifest JSON: {e}"))?;
        m.validate()?;
        Ok(m)
    }

    pub fn model(&self, id: &str) -> Option<&ModelSpec> {
        self.models.iter().find(|m| m.id == id)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!("unsupported manifest version {}", self.version));
        }
        for mirror in &self.mirrors {
            if !valid_prefix(mirror) {
                return Err(format!("invalid mirror prefix {mirror:?}"));
            }
        }
        if self.models.is_empty() {
            return Err("manifest lists no models".into());
        }
        let mut ids = HashSet::new();
        for model in &self.models {
            let id = &model.id;
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
                return Err(format!("invalid model id {id:?}"));
            }
            if !ids.insert(id.as_str()) {
                return Err(format!("duplicate model id {id:?}"));
            }
            validate_devices(model)?;
            if model.files.is_empty() {
                return Err(format!("model {id} has no files"));
            }
            let mut names = HashSet::new();
            for f in &model.files {
                validate_file(id, f)?;
                if !names.insert(f.file.as_str()) {
                    return Err(format!("model {id}: duplicate file {:?}", f.file));
                }
            }
        }
        Ok(())
    }
}

fn validate_devices(model: &ModelSpec) -> Result<(), String> {
    let id = &model.id;
    // The vocals stem is one of the four stems (drums, bass, other, vocals); streaming models ignore it.
    if model.vocals_index >= 4 || (model.kind == ModelKind::Streaming && model.vocals_index != 0) {
        return Err(format!("model {id}: invalid vocals index {}", model.vocals_index));
    }
    let devices = [("cpu", &model.devices.cpu), ("gpu", &model.devices.gpu)];
    if devices.iter().all(|(_, d)| d.is_none()) {
        return Err(format!("model {id} has no usable device"));
    }
    // The engine gets one window and one lookahead per model (only the hop differs by device).
    if let (ModelKind::Windowed, Some(cpu), Some(gpu)) = (model.kind, &model.devices.cpu, &model.devices.gpu) {
        if (cpu.window_ms, cpu.lookahead_ms) != (gpu.window_ms, gpu.lookahead_ms) {
            return Err(format!("model {id}: cpu and gpu must use the same window and lookahead"));
        }
    }
    for (name, d) in devices {
        let Some(d) = d else { continue };
        if d.threads == 0 {
            return Err(format!("model {id}: {name} threads must be at least 1"));
        }
        if !(d.latency_ms.is_finite() && d.latency_ms >= 0.0) {
            return Err(format!("model {id}: {name} latency is not a valid number"));
        }
        match model.kind {
            ModelKind::Streaming if (d.window_ms, d.hop_ms, d.lookahead_ms) != (0, 0, 0) => {
                return Err(format!("model {id}: streaming {name} must not set window, hop or lookahead"));
            }
            ModelKind::Windowed => {
                if d.window_ms == 0 || d.hop_ms == 0 || d.hop_ms > d.window_ms {
                    return Err(format!("model {id}: {name} needs 0 < hop <= window"));
                }
                let want = d.hop_ms as f64 * 1.3 + d.lookahead_ms as f64;
                if (d.latency_ms - want).abs() > 1e-6 {
                    return Err(format!("model {id}: {name} latency {} must be hop + lookahead + 0.3*hop = {want}", d.latency_ms));
                }
            }
            ModelKind::Streaming => {}
        }
    }
    Ok(())
}

fn validate_file(model: &str, f: &FileSpec) -> Result<(), String> {
    let name = f.file.as_str();
    if name.is_empty() || name == "." || name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(format!("model {model}: unsafe file name {name:?}"));
    }
    if f.bytes == 0 {
        return Err(format!("model {model}: file {name} has zero size"));
    }
    if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(format!("model {model}: file {name} sha256 must be 64 lowercase hex digits"));
    }
    if !f.origin.starts_with("https://") {
        return Err(format!("model {model}: file {name} origin must be https"));
    }
    if f.mirrorable && !f.origin.starts_with(GITHUB_PREFIX) {
        return Err(format!("model {model}: file {name} is mirrorable but not hosted on GitHub"));
    }
    Ok(())
}

/// The manifest compiled into the app. Panics (with the reason) if it is invalid, which a test prevents.
pub fn bundled() -> &'static Manifest {
    static BUNDLED: OnceLock<Manifest> = OnceLock::new();
    BUNDLED.get_or_init(|| {
        Manifest::parse(include_str!("../../../models.json"))
            .unwrap_or_else(|e| panic!("bundled models.json is invalid: {e}"))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    Origin,
    Custom,
    /// A built-in mirror, identified by its host.
    Mirror(String),
}

impl SourceKind {
    pub fn label(&self) -> String {
        match self {
            SourceKind::Origin => "origin".into(),
            SourceKind::Custom => "custom".into(),
            SourceKind::Mirror(host) => format!("mirror:{host}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub kind: SourceKind,
    pub url: String,
}

/// Download sources for one file in trial order: origin, the user's custom prefix, then the
/// built-in mirrors. Prefixes apply only to mirrorable files; an invalid custom prefix is ignored.
pub fn candidates(file: &FileSpec, custom_prefix: Option<&str>, mirrors: &[String]) -> Vec<Candidate> {
    let mut out = vec![Candidate { kind: SourceKind::Origin, url: file.origin.clone() }];
    if !file.mirrorable {
        return out;
    }
    if let Some(prefix) = custom_prefix.filter(|p| valid_prefix(p)) {
        out.push(Candidate { kind: SourceKind::Custom, url: format!("{prefix}{}", file.origin) });
    }
    for prefix in mirrors.iter().filter(|p| valid_prefix(p)) {
        let Some(rest) = prefix.strip_prefix("https://") else { continue };
        let host = rest.split('/').next().unwrap_or_default();
        out.push(Candidate { kind: SourceKind::Mirror(host.into()), url: format!("{prefix}{}", file.origin) });
    }
    out
}

/// A download-acceleration prefix: `https://` + host (`[A-Za-z0-9.-]+`, optional `:port`), ends
/// with `/`, no whitespace or control characters, at most 200 characters. The host rule keeps
/// userinfo (`a@evil`), queries and fragments out, since the origin URL is appended verbatim.
pub fn valid_prefix(prefix: &str) -> bool {
    let Some(rest) = prefix.strip_prefix("https://") else { return false };
    prefix.len() <= MAX_PREFIX_LEN
        && prefix.ends_with('/')
        && !prefix.chars().any(|c| c.is_whitespace() || c.is_control())
        && rest.split('/').next().is_some_and(valid_authority)
}

/// `host` or `host:port`: a non-empty host of ASCII letters, digits, `.` and `-`; a port 1-65535.
fn valid_authority(authority: &str) -> bool {
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && port.is_none_or(|p| p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = include_str!("../../../models.json");

    fn good_value() -> serde_json::Value {
        serde_json::from_str(GOOD).unwrap()
    }

    fn spec(file: &str, origin: &str, mirrorable: bool) -> FileSpec {
        FileSpec {
            file: file.into(),
            bytes: 1,
            sha256: "0".repeat(64),
            origin: origin.into(),
            mirrorable,
        }
    }

    #[test]
    fn bundled_manifest_is_valid_and_lists_stemgenrt() {
        let m = bundled();
        assert_eq!(m.version, 1);
        let s = m.model(STEMGENRT_ID).unwrap();
        assert_eq!(s.files.len(), 1);
        let f = &s.files[0];
        assert_eq!((f.file.as_str(), f.bytes), ("model.onnx", 37_529_132));
        assert_eq!(f.sha256, "77164d6a581fafb2a31f53fd8ffde44c07cf618472952a4cdba14e68dda3b8b9");
        assert!(f.origin.starts_with("https://github.com/sweetspotsoundsystem/stemgen-rt/raw/61df8f4aa1555ef110308d01ea92b54ace770979/"));
        assert_eq!(m.mirrors, ["https://ghfast.top/", "https://ghproxy.net/", "https://ghproxy.vip/"]);
        assert_eq!(s.tier, Tier::Realtime);
        assert_eq!(s.kind, ModelKind::Streaming);
        assert_eq!(s.sample_rate, 44100);
        assert_eq!(s.license.weights, "pending");
        assert!(!s.license.converted);
    }

    fn cpu(m: &Manifest, id: &str) -> DeviceParams {
        m.model(id).unwrap().devices.cpu.clone().unwrap()
    }

    #[test]
    fn bundled_manifest_has_three_models() {
        let m = bundled();
        let ids: Vec<&str> = m.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, [STEMGENRT_ID, "bytesep-mobilenet-1s", "htdemucs-ft-vocals-1s"]);

        let s = m.model(STEMGENRT_ID).unwrap();
        assert!(s.devices.gpu.is_none());
        // Streaming models ignore the window parameters and `vocals_index`.
        assert_eq!(
            cpu(m, STEMGENRT_ID),
            DeviceParams { window_ms: 0, hop_ms: 0, lookahead_ms: 0, threads: 1, latency_ms: 5.8 }
        );
        assert_eq!(s.vocals_index, 0);

        let b = m.model("bytesep-mobilenet-1s").unwrap();
        assert_eq!((b.kind, b.tier, b.sample_rate, b.vocals_index), (ModelKind::Windowed, Tier::Quality, 44100, 0));
        assert_eq!(b.files.len(), 1);
        let f = &b.files[0];
        assert_eq!((f.file.as_str(), f.bytes, f.mirrorable), ("bytesep-mobilenet-1s.onnx", 9_600_617, true));
        assert_eq!(f.sha256, "d70b6ba65e9627b6bc0f3e02efb6d678030d44af74b9de1ed0c7bc60a80d4885");
        assert_eq!(f.origin, "https://github.com/YlZHE/Tune-Love/releases/download/models-v1/bytesep-mobilenet-1s.onnx");
        assert_eq!(b.source, "https://zenodo.org/records/5804160");
        assert_eq!((b.license.code.as_str(), b.license.weights.as_str()), ("Apache-2.0", "CC BY 4.0"));
        assert_eq!(b.license.training_data, ["MUSDB18（仅教育用途）"]);
        assert!(b.license.converted);
        assert_eq!(
            b.devices.gpu,
            Some(DeviceParams { window_ms: 1000, hop_ms: 100, lookahead_ms: 100, threads: 1, latency_ms: 230.0 })
        );
        assert_eq!(
            b.devices.cpu,
            Some(DeviceParams { window_ms: 1000, hop_ms: 200, lookahead_ms: 100, threads: 2, latency_ms: 360.0 })
        );

        let h = m.model("htdemucs-ft-vocals-1s").unwrap();
        assert_eq!((h.kind, h.tier, h.sample_rate, h.vocals_index), (ModelKind::Windowed, Tier::Quality, 44100, 3));
        let f = &h.files[0];
        assert_eq!((f.file.as_str(), f.bytes, f.mirrorable), ("htdemucs-ft-vocals-1s.onnx", 304_759_764, true));
        assert_eq!(f.sha256, "fb173f3fdffd43d298c5ab26a9945a6c17845b9b81cce99df5ebcc8022dd5ab4");
        assert_eq!(f.origin, "https://github.com/YlZHE/Tune-Love/releases/download/models-v1/htdemucs-ft-vocals-1s.onnx");
        assert_eq!(h.source, "https://github.com/facebookresearch/demucs");
        assert_eq!(h.license.code, "MIT");
        assert!(h.license.converted);
        assert_eq!(h.license.weights, "MIT（Demucs 官方发布；训练数据来源不明，仅限非商业使用）");
        assert!(h.license.credit.contains("StemSplit demucs-onnx"));
        assert!(b.license.credit.contains("Kong 等人") && b.license.credit.contains("zenodo.org/records/5513378"));
        assert!(h.license.training_data.iter().any(|t| t.contains("来源不明") && t.contains("仅限非商业使用")));
        assert!(h.devices.cpu.is_none(), "HTDemucs runs on GPU only");
        assert_eq!(
            h.devices.gpu,
            Some(DeviceParams { window_ms: 1000, hop_ms: 100, lookahead_ms: 100, threads: 1, latency_ms: 230.0 })
        );
    }

    #[test]
    fn windowed_latency_matches_rule() {
        // latency = hop + lookahead + 0.3 * hop
        let mut windowed = 0;
        for model in &bundled().models {
            for d in [&model.devices.cpu, &model.devices.gpu].into_iter().flatten() {
                if model.kind == ModelKind::Windowed {
                    windowed += 1;
                    let want = d.hop_ms as f64 + d.lookahead_ms as f64 + 0.3 * d.hop_ms as f64;
                    assert!((d.latency_ms - want).abs() < 1e-6, "{}: {} vs {want}", model.id, d.latency_ms);
                }
            }
        }
        assert_eq!(windowed, 3);
        assert_eq!(cpu(bundled(), "bytesep-mobilenet-1s").latency_ms, 360.0);
    }

    #[test]
    fn rejects_windowed_without_device() {
        let mut v = good_value();
        v["models"][1]["devices"] = serde_json::json!({ "cpu": null, "gpu": null });
        assert!(Manifest::parse(&v.to_string()).is_err());
        let mut v = good_value();
        v["models"][1]["devices"] = serde_json::json!({});
        assert!(Manifest::parse(&v.to_string()).is_err());
        let mut v = good_value();
        v["models"][1].as_object_mut().unwrap().remove("devices");
        assert!(Manifest::parse(&v.to_string()).is_err());
    }

    #[test]
    fn rejects_cpu_threads_zero() {
        let mut v = good_value();
        v["models"][0]["devices"]["cpu"]["threads"] = 0.into();
        assert!(Manifest::parse(&v.to_string()).is_err());
        let mut v = good_value();
        v["models"][1]["devices"]["cpu"]["threads"] = 0.into();
        assert!(Manifest::parse(&v.to_string()).is_err());
    }

    #[test]
    fn rejects_inconsistent_device_params() {
        type Edit = Box<dyn Fn(&mut serde_json::Value)>;
        let cases: Vec<(&str, Edit)> = vec![
            ("latency off the rule", Box::new(|v| v["models"][1]["devices"]["gpu"]["latencyMs"] = 200.0.into())),
            ("zero hop", Box::new(|v| v["models"][1]["devices"]["gpu"]["hopMs"] = 0.into())),
            ("zero window", Box::new(|v| v["models"][1]["devices"]["gpu"]["windowMs"] = 0.into())),
            ("streaming with a window", Box::new(|v| v["models"][0]["devices"]["cpu"]["windowMs"] = 1000.into())),
            // The WindowedSpec has one window and one lookahead for both devices.
            ("devices disagree on the window", Box::new(|v| v["models"][1]["devices"]["cpu"]["windowMs"] = 2000.into())),
            ("devices disagree on the lookahead", Box::new(|v| {
                let cpu = &mut v["models"][1]["devices"]["cpu"];
                cpu["lookaheadMs"] = 50.into();
                cpu["latencyMs"] = 310.0.into(); // still on the latency rule
            })),
            ("vocals index 4", Box::new(|v| v["models"][2]["vocalsIndex"] = 4.into())),
            ("streaming vocals index 1", Box::new(|v| v["models"][0]["vocalsIndex"] = 1.into())),
            ("bad kind", Box::new(|v| v["models"][0]["kind"] = "batch".into())),
            ("old runtime-only entry", Box::new(|v| {
                let m = v["models"][0].as_object_mut().unwrap();
                m.remove("kind");
                m.remove("devices");
                m.insert("runtime".into(), "cpu".into());
            })),
        ];
        for (name, edit) in cases {
            let mut v = good_value();
            edit(&mut v);
            assert!(Manifest::parse(&v.to_string()).is_err(), "should reject: {name}");
        }
    }

    #[test]
    fn parse_rejects_unsafe_or_malformed_entries() {
        assert!(Manifest::parse(GOOD).is_ok());
        type Edit = Box<dyn Fn(&mut serde_json::Value)>;
        let mut cases: Vec<(&str, Edit)> = vec![
            ("version 2", Box::new(|v| v["version"] = 2.into())),
            ("duplicate id", Box::new(|v| {
                let m = v["models"][0].clone();
                v["models"].as_array_mut().unwrap().push(m);
            })),
            ("uppercase id", Box::new(|v| v["models"][0]["id"] = "Abc".into())),
            ("slash id", Box::new(|v| v["models"][0]["id"] = "a/b".into())),
            ("empty id", Box::new(|v| v["models"][0]["id"] = "".into())),
            ("short sha", Box::new(|v| v["models"][0]["files"][0]["sha256"] = "abcd".into())),
            ("uppercase sha", Box::new(|v| {
                let s = v["models"][0]["files"][0]["sha256"].as_str().unwrap().to_uppercase();
                v["models"][0]["files"][0]["sha256"] = s.into();
            })),
            ("non-hex sha", Box::new(|v| v["models"][0]["files"][0]["sha256"] = "z".repeat(64).into())),
            ("http origin", Box::new(|v| v["models"][0]["files"][0]["origin"] = "http://github.com/a/b".into())),
            ("mirrorable non-github", Box::new(|v| {
                v["models"][0]["files"][0]["origin"] = "https://zenodo.org/x/model.onnx".into()
            })),
            ("mirror not https", Box::new(|v| v["mirrors"][0] = "http://ghfast.top/".into())),
            ("mirror no trailing slash", Box::new(|v| v["mirrors"][0] = "https://ghfast.top".into())),
            ("empty models", Box::new(|v| v["models"] = serde_json::json!([]))),
            ("empty files", Box::new(|v| v["models"][0]["files"] = serde_json::json!([]))),
            ("zero bytes", Box::new(|v| v["models"][0]["files"][0]["bytes"] = 0.into())),
            ("bad tier", Box::new(|v| v["models"][0]["tier"] = "fast".into())),
        ];
        for bad in ["../x.onnx", "a/b", "a\\b", "", ".", ".."] {
            let bad = bad.to_string();
            cases.push(("bad file name", Box::new(move |v| v["models"][0]["files"][0]["file"] = bad.clone().into())));
        }
        for (name, edit) in cases {
            let mut v = good_value();
            edit(&mut v);
            assert!(Manifest::parse(&v.to_string()).is_err(), "should reject: {name}");
        }
        assert!(Manifest::parse("not json").is_err());
    }

    #[test]
    fn candidates_follow_origin_custom_mirrors_order() {
        let f = spec("m.onnx", "https://github.com/a/b/raw/c/m.onnx", true);
        let c = candidates(&f, Some("https://my.proxy/"), &["https://ghfast.top/".into(), "https://ghproxy.net/".into()]);
        assert_eq!(c.iter().map(|c| (c.kind.label(), c.url.clone())).collect::<Vec<_>>(), vec![
            ("origin".into(), "https://github.com/a/b/raw/c/m.onnx".into()),
            ("custom".into(), "https://my.proxy/https://github.com/a/b/raw/c/m.onnx".into()),
            ("mirror:ghfast.top".into(), "https://ghfast.top/https://github.com/a/b/raw/c/m.onnx".into()),
            ("mirror:ghproxy.net".into(), "https://ghproxy.net/https://github.com/a/b/raw/c/m.onnx".into()),
        ]);
    }

    #[test]
    fn invalid_custom_prefix_is_ignored() {
        let f = spec("m.onnx", "https://github.com/a/b/raw/c/m.onnx", true);
        for bad in ["http://bad/", "https://a@evil/", "https://a.b?x/"] {
            let c = candidates(&f, Some(bad), &[]);
            assert_eq!(c.len(), 1, "{bad}");
            assert_eq!(c[0].kind, SourceKind::Origin);
        }
    }

    #[test]
    fn invalid_mirrors_are_skipped() {
        let f = spec("m.onnx", "https://github.com/a/b/raw/c/m.onnx", true);
        let mirrors = ["x", "http://ghfast.top/", "https://a@evil/", "https://ghproxy.net/"].map(String::from);
        let c = candidates(&f, None, &mirrors);
        assert_eq!(c.iter().map(|c| c.kind.label()).collect::<Vec<_>>(), ["origin", "mirror:ghproxy.net"]);
    }

    #[test]
    fn non_mirrorable_files_only_use_origin() {
        let f = spec("m.onnx", "https://zenodo.org/x/m.onnx", false);
        let c = candidates(&f, Some("https://my.proxy/"), &["https://ghfast.top/".into()]);
        assert_eq!(c, vec![Candidate { kind: SourceKind::Origin, url: "https://zenodo.org/x/m.onnx".into() }]);
    }

    #[test]
    fn valid_prefix_rules() {
        for ok in ["https://ghfast.top/", "https://a.b/c/", "https://my-proxy.example:8443/", "https://a.b:1/x/"] {
            assert!(valid_prefix(ok), "{ok}");
        }
        let long = format!("https://a.b/{}/", "x".repeat(200));
        for bad in [
            "", "http://a.b/", "https://a.b", "https:///", "https://a b/", " https://a.b/",
            "javascript:alert(1)/", "https://a.b/\n", long.as_str(),
            "https://a@evil/", "https://user:pw@evil/", "https://a.b?x/", "https://a.b#x/", "https://a_b/",
            "https://a.b:/", "https://a.b:0/", "https://a.b:65536/", "https://a.b:80x/", "https://:443/",
            "https://a.b:1:2/", "https://[::1]/", "https://a.b\\c/", "https://a.b:+80/",
        ] {
            assert!(!valid_prefix(bad), "{bad:?}");
        }
    }
}
