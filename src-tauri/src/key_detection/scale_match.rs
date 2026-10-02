//! Choose the Auto-Tune Key/Scale whose allowed note set best covers the song's
//! accumulated pitch-class evidence. Only Major and Minor are ever chosen: the
//! user's experience is that Chromatic barely corrects a voice, so there is no
//! "safe" Chromatic state. Before the first evidence there is no target (nothing
//! is written); the first evidence commits immediately, later changes need a
//! clear cost improvement. Parameters are the ones frozen by the offline pilot
//! (docs/2026-10-02_scale-match-pilot-report.md); the note sets are the textbook
//! assumption pending stage C verification against the plugin.
use super::ChromaEvidence;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scale {
    Major,
    Minor,
}

/// Where the current choice came from: this play's analysis, or the result
/// remembered from an earlier play of the same song (kept until analysis of
/// this play has enough evidence to overrule it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetSource {
    Analysis,
    Cache,
}

/// The current recommendation, keyed exactly like the profile's option tables.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoTuneTarget {
    /// Pitch class 0..12 (C = 0).
    pub key: u8,
    pub scale: Scale,
    /// Seconds of non-silent evidence analysed during this play.
    pub evidence_seconds: f64,
    pub source: TargetSource,
}

/// A cached choice is not overruled before this much evidence from the current
/// play (an early, noisy analysis must not undo a well-supported earlier result).
pub const SEED_HOLD_SECONDS: f64 = 20.0;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub w_extra: f64,
    pub tau: f64,
    pub alpha: f64,
    pub margin: f64,
    pub gamma: f64,
    pub silence_rms: f64,
}

/// Frozen from the five-song pilot (S1, diatonic family). Not tuned on holdout data.
pub const FROZEN: Params = Params {
    w_extra: 1.0,
    tau: 0.04,
    alpha: 2.0,
    margin: 0.03,
    gamma: 4.0,
    silence_rms: 1.0e-3,
};

const MAJOR: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
const MINOR: [u8; 7] = [0, 2, 3, 5, 7, 8, 10];
/// Combo order: Major C..B, then Minor C..B. Ties resolve to the earliest
/// entry, so a relative-minor tie reports the Major name (same notes).
const COMBOS: usize = 24;

fn combo(index: usize) -> (u8, Scale) {
    if index < 12 {
        (index as u8, Scale::Major)
    } else {
        ((index - 12) as u8, Scale::Minor)
    }
}

fn mask(index: usize) -> [bool; 12] {
    let (key, scale) = combo(index);
    let intervals = match scale {
        Scale::Major => &MAJOR,
        Scale::Minor => &MINOR,
    };
    let mut allowed = [false; 12];
    intervals
        .iter()
        .for_each(|i| allowed[(key + i) as usize % 12] = true);
    allowed
}

pub struct ScaleMatcher {
    params: Params,
    evidence: [f64; 12],
    seconds: f64,
    current: Option<usize>,
    seeded: bool,
}

impl ScaleMatcher {
    pub fn new(params: Params) -> Self {
        Self {
            params,
            evidence: [0.0; 12],
            seconds: 0.0,
            current: None,
            seeded: false,
        }
    }

    /// Forget the song: a new track starts undecided with no evidence.
    pub fn reset(&mut self) {
        self.evidence = [0.0; 12];
        self.seconds = 0.0;
        self.current = None;
        self.seeded = false;
    }

    /// Start a song from a remembered choice (before any evidence of this play).
    pub fn seed(&mut self, key: u8, scale: Scale) {
        if key >= 12 {
            return;
        }
        self.current = Some(match scale {
            Scale::Major => key as usize,
            Scale::Minor => 12 + key as usize,
        });
        self.seeded = true;
    }

    /// Accumulate one step of evidence; silent or empty steps are ignored.
    pub fn add(&mut self, step: &ChromaEvidence) {
        if !(step.seconds > 0.0) || !(step.rms >= self.params.silence_rms) {
            return;
        }
        let peak = step.chroma.iter().cloned().fold(0.0_f64, f64::max);
        if !(peak > 0.0) || step.chroma.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return;
        }
        let shaped: Vec<f64> = step
            .chroma
            .iter()
            .map(|v| (v / peak).powf(self.params.gamma))
            .collect();
        let total: f64 = shaped.iter().sum();
        for (slot, value) in self.evidence.iter_mut().zip(shaped) {
            *slot += value / total * step.seconds;
        }
        self.seconds += step.seconds;
        if self.seeded && self.seconds < SEED_HOLD_SECONDS {
            return;
        }
        let chosen = self.choose();
        if self.current != Some(chosen) {
            self.seeded = false;
        }
        self.current = Some(chosen);
    }

    fn weights(&self) -> [f64; 12] {
        let alpha = self.params.alpha;
        let total = self.seconds + alpha;
        std::array::from_fn(|i| (self.evidence[i] + alpha / 12.0) / total)
    }

    fn cost(&self, index: usize, weights: &[f64; 12]) -> f64 {
        let allowed = mask(index);
        let mut miss = 0.0;
        let mut extra = 0.0;
        for pc in 0..12 {
            if allowed[pc] {
                extra += (1.0 - weights[pc] / self.params.tau).clamp(0.0, 1.0);
            } else {
                miss += weights[pc];
            }
        }
        miss + self.params.w_extra * extra / 12.0
    }

    fn choose(&self) -> usize {
        let weights = self.weights();
        let costs: Vec<f64> = (0..COMBOS).map(|i| self.cost(i, &weights)).collect();
        let best = (0..COMBOS)
            .min_by(|a, b| costs[*a].total_cmp(&costs[*b]))
            .unwrap_or(0);
        match self.current {
            // First evidence commits immediately: there is no neutral fallback.
            None => best,
            Some(current)
                if best != current && costs[best] < costs[current] - self.params.margin =>
            {
                best
            }
            Some(current) => current,
        }
    }

    /// None until the first non-silent evidence of the current song.
    pub fn target(&self) -> Option<AutoTuneTarget> {
        self.current.map(|index| {
            let (key, scale) = combo(index);
            AutoTuneTarget {
                key,
                scale,
                evidence_seconds: self.seconds,
                source: if self.seeded {
                    TargetSource::Cache
                } else {
                    TargetSource::Analysis
                },
            }
        })
    }
}

/// Player transposition shifts the Key only; the Scale is unchanged.
pub fn transpose(target: AutoTuneTarget, semitones: i32) -> AutoTuneTarget {
    AutoTuneTarget {
        key: ((target.key as i32 + semitones).rem_euclid(12)) as u8,
        ..target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(pitch_classes: &[usize], seconds: f64) -> ChromaEvidence {
        let mut chroma = [0.0; 12];
        for pc in pitch_classes {
            chroma[*pc] = 1.0;
        }
        ChromaEvidence {
            chroma,
            seconds,
            rms: 0.1,
        }
    }

    fn feed(matcher: &mut ScaleMatcher, pitch_classes: &[usize], steps: usize) {
        for _ in 0..steps {
            matcher.add(&evidence(pitch_classes, 1.0));
        }
    }

    fn pair(matcher: &ScaleMatcher) -> (u8, Scale) {
        let target = matcher.target().expect("decided");
        (target.key, target.scale)
    }

    #[test]
    fn undecided_until_evidence_then_c_major() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        assert!(matcher.target().is_none());
        feed(&mut matcher, &[0, 2, 4, 5, 7, 9, 11], 30);
        assert_eq!(pair(&matcher), (0, Scale::Major));
        assert_eq!(matcher.target().unwrap().evidence_seconds, 30.0);
    }

    #[test]
    fn first_evidence_commits_to_a_covering_major_or_minor() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        feed(&mut matcher, &[0, 4, 7], 1); // one second of a C major triad
        let target = matcher.target().expect("commits on first evidence");
        let allowed = mask(matcher.current.unwrap());
        assert!([0, 4, 7].iter().all(|pc| allowed[*pc]), "{target:?}");
    }

    #[test]
    fn relative_minor_tie_reports_major_name() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        feed(&mut matcher, &[9, 11, 0, 2, 4, 5, 7], 30); // A natural minor == C major notes
        assert_eq!(pair(&matcher), (0, Scale::Major));
    }

    #[test]
    fn pentatonic_evidence_narrows_to_a_covering_diatonic_set() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        feed(&mut matcher, &[5, 8, 10, 0, 3], 30);
        let allowed = mask(matcher.current.unwrap());
        assert!([5, 8, 10, 0, 3].iter().all(|pc| allowed[*pc]));
    }

    #[test]
    fn ambiguous_evidence_still_chooses_major_or_minor() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        feed(&mut matcher, &(0..12).collect::<Vec<_>>(), 60);
        assert!(matcher.target().is_some());
    }

    #[test]
    fn silence_and_invalid_steps_are_ignored() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        let mut quiet = evidence(&[0, 4, 7], 1.0);
        quiet.rms = 1.0e-4;
        matcher.add(&quiet);
        matcher.add(&ChromaEvidence {
            chroma: [0.0; 12],
            seconds: 1.0,
            rms: 0.1,
        });
        matcher.add(&ChromaEvidence {
            chroma: [f64::NAN; 12],
            seconds: 1.0,
            rms: 0.1,
        });
        matcher.add(&evidence(&[0, 4, 7], 0.0));
        assert!(matcher.target().is_none());
    }

    #[test]
    fn hysteresis_requires_a_clear_improvement_to_switch() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        feed(&mut matcher, &[0, 2, 4, 5, 7, 9, 11], 30);
        assert_eq!(pair(&matcher).0, 0);
        feed(&mut matcher, &[7, 9, 11, 0, 2, 4, 6], 1);
        assert_eq!(pair(&matcher).0, 0);
        feed(&mut matcher, &[7, 9, 11, 0, 2, 4, 6], 120);
        assert_eq!(pair(&matcher), (7, Scale::Major));
    }

    #[test]
    fn reset_forgets_the_song() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        feed(&mut matcher, &[0, 2, 4, 5, 7, 9, 11], 30);
        matcher.reset();
        assert!(matcher.target().is_none());
    }

    #[test]
    fn transpose_shifts_key_only() {
        let c_major = AutoTuneTarget {
            key: 0,
            scale: Scale::Major,
            evidence_seconds: 1.0,
            source: TargetSource::Analysis,
        };
        assert_eq!(transpose(c_major, 2).key, 2);
        assert_eq!(transpose(c_major, -1).key, 11);
        assert_eq!(transpose(c_major, 2).scale, Scale::Major);
    }

    #[test]
    fn target_serializes_with_profile_compatible_names() {
        let json = serde_json::to_value(AutoTuneTarget {
            key: 6,
            scale: Scale::Minor,
            evidence_seconds: 12.5,
            source: TargetSource::Cache,
        })
        .unwrap();
        assert_eq!(json["key"], 6);
        assert_eq!(json["scale"], "minor");
        assert_eq!(json["evidenceSeconds"], 12.5);
        assert_eq!(json["source"], "cache");
        assert_eq!(json.as_object().unwrap().len(), 4);
    }

    #[test]
    fn a_seed_is_reported_immediately_and_held_against_early_contrary_evidence() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        matcher.seed(6, Scale::Minor);
        let seeded = matcher.target().unwrap();
        assert_eq!(
            (seeded.key, seeded.scale, seeded.source),
            (6, Scale::Minor, TargetSource::Cache)
        );
        feed(&mut matcher, &[0, 2, 4, 5, 7, 9, 11], 19); // contrary, but under the hold
        assert_eq!(pair(&matcher), (6, Scale::Minor));
        assert_eq!(matcher.target().unwrap().source, TargetSource::Cache);
        feed(&mut matcher, &[0, 2, 4, 5, 7, 9, 11], 40); // overwhelming: analysis takes over
        assert_eq!(pair(&matcher), (0, Scale::Major));
        assert_eq!(matcher.target().unwrap().source, TargetSource::Analysis);
    }

    #[test]
    fn a_confirmed_seed_keeps_its_cache_source() {
        let mut matcher = ScaleMatcher::new(FROZEN);
        matcher.seed(0, Scale::Major);
        feed(&mut matcher, &[0, 2, 4, 5, 7, 9, 11], 40);
        assert_eq!(pair(&matcher), (0, Scale::Major));
        assert_eq!(matcher.target().unwrap().source, TargetSource::Cache);
        matcher.reset();
        assert!(matcher.target().is_none());
    }
}
