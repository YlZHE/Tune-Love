#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Major,
    Minor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicalKey {
    pub pitch_class: u8,
    pub mode: Mode,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct KeyScore {
    pub key: MusicalKey,
    pub score: f64,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyDiagnostics {
    pub score_kind: &'static str,
    pub scores: [KeyScore; 24],
    pub best: KeyScore,
    pub runner_up: KeyScore,
    pub margin: f64,
}

/// Pitch-class evidence from one analysis step: twelve magnitudes (C first)
/// averaged over the audio hops it covers, their duration, and the input level
/// used to skip silence. Consumed by the scale matcher, never shown directly.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChromaEvidence {
    pub chroma: [f64; 12],
    pub seconds: f64,
    pub rms: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct DiagnosticAnalysis {
    pub candidate: Option<MusicalKey>,
    pub diagnostics: Option<KeyDiagnostics>,
    pub evidence: Option<ChromaEvidence>,
}
