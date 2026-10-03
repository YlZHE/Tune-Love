//! The single model manifest (`src-tauri/models.json`), shared with `scripts/fetch-stemgenrt.mjs`
//! and the licence text, plus the ordered download sources for each file.

use std::collections::HashSet;
use std::sync::OnceLock;

use serde::Deserialize;

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
    pub sample_rate: u32,
    pub latency_ms: f64,
    pub runtime: Runtime,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Cpu,
    Cuda,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseInfo {
    pub code: String,
    pub weights: String,
    pub training_data: Vec<String>,
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
    for prefix in mirrors {
        let host = prefix["https://".len()..].split('/').next().unwrap_or_default();
        out.push(Candidate { kind: SourceKind::Mirror(host.into()), url: format!("{prefix}{}", file.origin) });
    }
    out
}

/// A download-acceleration prefix: `https://` + non-empty host, ends with `/`, no whitespace or
/// control characters, at most 200 characters.
pub fn valid_prefix(prefix: &str) -> bool {
    let Some(rest) = prefix.strip_prefix("https://") else { return false };
    prefix.len() <= MAX_PREFIX_LEN
        && prefix.ends_with('/')
        && !prefix.chars().any(|c| c.is_whitespace() || c.is_control())
        && rest.split('/').next().is_some_and(|host| !host.is_empty())
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
        assert_eq!(s.runtime, Runtime::Cpu);
        assert_eq!(s.sample_rate, 44100);
        assert_eq!(s.license.weights, "pending");
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
        let c = candidates(&f, Some("http://bad/"), &[]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].kind, SourceKind::Origin);
    }

    #[test]
    fn non_mirrorable_files_only_use_origin() {
        let f = spec("m.onnx", "https://zenodo.org/x/m.onnx", false);
        let c = candidates(&f, Some("https://my.proxy/"), &["https://ghfast.top/".into()]);
        assert_eq!(c, vec![Candidate { kind: SourceKind::Origin, url: "https://zenodo.org/x/m.onnx".into() }]);
    }

    #[test]
    fn valid_prefix_rules() {
        for ok in ["https://ghfast.top/", "https://a.b/c/"] {
            assert!(valid_prefix(ok), "{ok}");
        }
        let long = format!("https://a.b/{}/", "x".repeat(200));
        for bad in [
            "", "http://a.b/", "https://a.b", "https:///", "https://a b/", " https://a.b/",
            "javascript:alert(1)/", "https://a.b/\n", long.as_str(),
        ] {
            assert!(!valid_prefix(bad), "{bad:?}");
        }
    }
}
