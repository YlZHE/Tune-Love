//! Choose the Auto-Tune Key/Scale whose allowed note set best covers the song's
//! accumulated pitch-class evidence, and say when that choice is too uncertain
//! to act on.
//!
//! Two layers (docs/2026-10-04_chromatic-gate-report.md):
//! * The set (Major or Minor, 24 combos covering 12 distinct note sets) comes from
//!   the pilot-frozen cost matcher: the first evidence commits immediately, later
//!   changes need a clear cost improvement.
//! * A gate decides whether to *write* that set or Chromatic ("when uncertain, do
//!   not pull"). Statistic: max out-of-set weight / min in-set weight, with ENTER and
//!   EXIT lines and an exit hold, counted in evidence seconds. It mirrors
//!   `experiments/scale-match/chromatic_gate.py` (`gate_statistic("ratio")`,
//!   `Gate.update`) step for step; the tests pin both against the same sequences.
//!   Chromatic is therefore the starting state and the uncertain state; the chosen
//!   set stays visible as `candidate`.
//!
//! The note sets are the textbook assumption verified against the plugin in stage C.
use super::{ChromaEvidence, Mode, MusicalKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scale {
    Major,
    Minor,
    Chromatic,
}

/// A Major/Minor name for the note set the matcher currently favours.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    /// Pitch class 0..12 (C = 0).
    pub key: u8,
    /// Major or Minor only.
    pub scale: Scale,
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
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoTuneTarget {
    /// Pitch class 0..12 (C = 0) of the current name; None while there is no evidence at all.
    pub key: Option<u8>,
    pub scale: Scale,
    /// The favoured set under its name; also given while `scale` is Chromatic.
    pub candidate: Option<Candidate>,
    /// Pitch classes outside the set whose weight is at least GATE.exit times the
    /// weakest in-set weight; at most 2, strongest first.
    pub uncovered_notes: Vec<u8>,
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

/// Lines of the Chromatic gate, in units of the ratio statistic and evidence seconds.
#[derive(Clone, Copy, Debug)]
pub struct GateParams {
    pub enter: f64,
    pub exit: f64,
    pub exit_hold_seconds: f64,
    pub min_seconds: f64,
}

/// Frozen by the user's choice of the second-round default (chromatic-gate-02, ratio 3.0/1.5/3/3).
pub const GATE: GateParams = GateParams {
    enter: 3.0,
    exit: 1.5,
    exit_hold_seconds: 3.0,
    min_seconds: 3.0,
};

const MAJOR: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
const MINOR: [u8; 7] = [0, 2, 3, 5, 7, 8, 10];
/// Combo order: Major C..B, then Minor C..B. Ties resolve to the earliest
/// entry, so a relative-minor tie picks the Major combo (same notes).
const COMBOS: usize = 24;

fn combo(index: usize) -> (u8, Scale) {
    if index < 12 {
        (index as u8, Scale::Major)
    } else {
        ((index - 12) as u8, Scale::Minor)
    }
}

/// The note set is identified by its major tonic; a minor key's set is that of its
/// relative major, three semitones up.
fn set_of(key: u8, scale: Scale) -> usize {
    match scale {
        Scale::Minor => (key as usize + 3) % 12,
        _ => key as usize,
    }
}

fn mask(index: usize) -> [bool; 12] {
    let (key, scale) = combo(index);
    let intervals = match scale {
        Scale::Minor => &MINOR,
        _ => &MAJOR,
    };
    let mut allowed = [false; 12];
    intervals
        .iter()
        .for_each(|i| allowed[(key + i) as usize % 12] = true);
    allowed
}

/// max out-of-set weight / min in-set weight (the prior keeps the in-set minimum above 0).
fn gate_statistic(weights: &[f64; 12], allowed: &[bool; 12]) -> f64 {
    let top = (0..12)
        .filter(|i| !allowed[*i])
        .map(|i| weights[i])
        .fold(0.0, f64::max);
    let floor = (0..12)
        .filter(|i| allowed[*i])
        .map(|i| weights[i])
        .fold(f64::INFINITY, f64::min);
    top / floor
}

#[derive(Clone, Copy, Debug)]
struct GateState {
    chromatic: bool,
    hold: f64,
}

impl GateState {
    fn new() -> Self {
        Self {
            chromatic: true,
            hold: 0.0,
        }
    }

    /// `seconds`: accumulated evidence time; `step`: evidence time added by this step.
    fn update(&mut self, gate: &GateParams, statistic: f64, seconds: f64, step: f64) {
        if self.chromatic {
            if seconds >= gate.min_seconds && statistic < gate.exit {
                self.hold += step;
                if self.hold >= gate.exit_hold_seconds {
                    self.chromatic = false;
                }
            } else {
                self.hold = 0.0;
            }
        } else if statistic > gate.enter {
            self.chromatic = true;
            self.hold = 0.0;
        }
    }
}

pub struct ScaleMatcher {
    params: Params,
    gate: GateParams,
    state: GateState,
    evidence: [f64; 12],
    seconds: f64,
    current: Option<usize>,
    seeded: bool,
    /// Last name used for each note set (indexed by major tonic).
    names: [Option<(u8, Scale)>; 12],
}

impl ScaleMatcher {
    pub fn new(params: Params, gate: GateParams) -> Self {
        Self {
            params,
            gate,
            state: GateState::new(),
            evidence: [0.0; 12],
            seconds: 0.0,
            current: None,
            seeded: false,
            names: [None; 12],
        }
    }

    /// Forget the song: a new track starts Chromatic with no evidence and no names.
    pub fn reset(&mut self) {
        self.evidence = [0.0; 12];
        self.seconds = 0.0;
        self.current = None;
        self.seeded = false;
        self.state = GateState::new();
        self.names = [None; 12];
    }

    /// Start a song from a remembered Major/Minor choice (before any evidence of this play).
    /// A remembered Chromatic result (with `candidate`) is seeded in Task 3; ignored until then.
    pub fn seed(&mut self, key: u8, scale: Scale, _candidate: Option<Candidate>) {
        if key >= 12 || scale == Scale::Chromatic {
            return;
        }
        let set = set_of(key, scale);
        self.current = Some(match scale {
            Scale::Minor => 12 + key as usize,
            _ => key as usize,
        });
        self.names[set] = Some((key, scale));
        self.state.chromatic = false;
        self.state.hold = 0.0;
        self.seeded = true;
    }

    /// The stable libKeyFinder result decides how a relative major/minor pair is named:
    /// if it belongs to a set, that set uses its name from now on (until the song resets).
    pub fn set_name_hint(&mut self, hint: Option<MusicalKey>) {
        if let Some(MusicalKey { pitch_class, mode }) = hint {
            if pitch_class < 12 {
                let scale = match mode {
                    Mode::Major => Scale::Major,
                    Mode::Minor => Scale::Minor,
                };
                self.names[set_of(pitch_class, scale)] = Some((pitch_class, scale));
            }
        }
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
        let statistic = gate_statistic(&self.weights(), &mask(chosen));
        self.state
            .update(&self.gate, statistic, self.seconds, step.seconds);
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
            // First evidence commits immediately to a candidate set.
            None => best,
            Some(current)
                if best != current && costs[best] < costs[current] - self.params.margin =>
            {
                best
            }
            Some(current) => current,
        }
    }

    /// Always Some: before any evidence it is Chromatic with no key or candidate, so the
    /// song can start unpulled.
    pub fn target(&self) -> Option<AutoTuneTarget> {
        let source = if self.seeded {
            TargetSource::Cache
        } else {
            TargetSource::Analysis
        };
        let Some(index) = self.current else {
            return Some(AutoTuneTarget {
                key: None,
                scale: Scale::Chromatic,
                candidate: None,
                uncovered_notes: Vec::new(),
                evidence_seconds: self.seconds,
                source,
            });
        };
        let (tonic, scale) = combo(index);
        let set = set_of(tonic, scale);
        let (key, named) = self.names[set].unwrap_or((set as u8, Scale::Major));

        let weights = self.weights();
        let allowed = mask(index);
        let floor = (0..12)
            .filter(|i| allowed[*i])
            .map(|i| weights[i])
            .fold(f64::INFINITY, f64::min);
        let mut outside: Vec<(u8, f64)> = (0..12)
            .filter(|i| !allowed[*i] && weights[*i] >= self.gate.exit * floor)
            .map(|i| (i as u8, weights[i]))
            .collect();
        outside.sort_by(|a, b| b.1.total_cmp(&a.1)); // stable: equal weights keep pitch-class order
        outside.truncate(2);

        Some(AutoTuneTarget {
            key: Some(key),
            scale: if self.state.chromatic {
                Scale::Chromatic
            } else {
                named
            },
            candidate: Some(Candidate { key, scale: named }),
            uncovered_notes: outside.into_iter().map(|(pc, _)| pc).collect(),
            evidence_seconds: self.seconds,
            source,
        })
    }
}

/// Player transposition shifts the Key (and the candidate and uncovered notes with
/// it); the Scale is unchanged.
pub fn transpose(target: AutoTuneTarget, semitones: i32) -> AutoTuneTarget {
    let shift = |pc: u8| ((pc as i32 + semitones).rem_euclid(12)) as u8;
    AutoTuneTarget {
        key: target.key.map(shift),
        candidate: target.candidate.map(|c| Candidate {
            key: shift(c.key),
            ..c
        }),
        uncovered_notes: target.uncovered_notes.into_iter().map(shift).collect(),
        ..target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C_MAJOR: [usize; 7] = [0, 2, 4, 5, 7, 9, 11];
    const G_MAJOR: [usize; 7] = [7, 9, 11, 0, 2, 4, 6];
    /// G# major / F minor notes: G# A# C C# D# F G.
    const AB_MAJOR: [usize; 7] = [8, 10, 0, 1, 3, 5, 7];

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

    fn weighted(levels: &[(usize, f64)]) -> ChromaEvidence {
        let mut chroma = [0.0; 12];
        for (pc, level) in levels {
            chroma[*pc] = *level;
        }
        ChromaEvidence {
            chroma,
            seconds: 1.0,
            rms: 0.1,
        }
    }

    fn silent() -> ChromaEvidence {
        ChromaEvidence {
            chroma: [1.0; 12],
            seconds: 1.0,
            rms: 0.0,
        }
    }

    fn feed(matcher: &mut ScaleMatcher, pitch_classes: &[usize], steps: usize) {
        for _ in 0..steps {
            matcher.add(&evidence(pitch_classes, 1.0));
        }
    }

    fn new_matcher() -> ScaleMatcher {
        ScaleMatcher::new(FROZEN, GATE)
    }

    fn pair(matcher: &ScaleMatcher) -> (Option<u8>, Scale) {
        let target = matcher.target().expect("target");
        (target.key, target.scale)
    }

    fn musical(pitch_class: u8, mode: Mode) -> MusicalKey {
        MusicalKey { pitch_class, mode }
    }

    fn candidate(key: u8, scale: Scale) -> Option<Candidate> {
        Some(Candidate { key, scale })
    }

    // ---- frozen values and gate state machine (mirror chromatic_gate.py) ----

    #[test]
    fn gate_values_are_the_frozen_ones() {
        assert_eq!(
            (
                GATE.enter,
                GATE.exit,
                GATE.exit_hold_seconds,
                GATE.min_seconds
            ),
            (3.0, 1.5, 3.0, 3.0)
        );
    }

    /// In-set notes at 0.125 (exact in binary, so ratio 3.0 is exactly 3.0), out-of-set notes at
    /// 0.001 except D-flat, which is `ratio` x 0.125 (the C major set). The Python script used
    /// 0.1, where 0.1 * 3.0 / 0.1 is 3.0000000000000004; flags elsewhere are unaffected.
    fn ratio_weights(ratio: f64) -> ([f64; 12], [bool; 12]) {
        let allowed = mask(0);
        let mut weights = [0.001; 12];
        for pc in 0..12 {
            if allowed[pc] {
                weights[pc] = 0.125;
            }
        }
        weights[1] = 0.125 * ratio;
        (weights, allowed)
    }

    /// One second per value; returns the Chromatic flag after every step like the Python `run`.
    fn run_gate(state: &mut GateState, seconds: &mut f64, ratios: &[f64]) -> String {
        ratios
            .iter()
            .map(|r| {
                let (weights, allowed) = ratio_weights(*r);
                let statistic = gate_statistic(&weights, &allowed);
                *seconds += 1.0;
                state.update(&GATE, statistic, *seconds, 1.0);
                if state.chromatic {
                    'C'
                } else {
                    '.'
                }
            })
            .collect()
    }

    fn settled_gate() -> (GateState, f64) {
        let mut state = GateState::new();
        let mut seconds = 0.0;
        run_gate(&mut state, &mut seconds, &[0.0; 8]);
        assert!(!state.chromatic);
        (state, seconds)
    }

    #[test]
    fn statistic_is_top_out_of_set_over_weakest_in_set() {
        let allowed = mask(0);
        let mut weights = [0.0; 12];
        for pc in 0..12 {
            weights[pc] = if allowed[pc] { 0.12 } else { 0.0 };
        }
        weights[1] = 0.06; // strongest out-of-set note
        weights[0] = 0.03; // weakest in-set note
        assert!((gate_statistic(&weights, &allowed) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn gate_starts_chromatic_until_min_seconds_then_needs_the_hold() {
        // Python: "CCCC...." (qualifying from second 3, hold of 3 s met at second 5)
        let mut state = GateState::new();
        assert_eq!(run_gate(&mut state, &mut 0.0, &[0.0; 8]), "CCCC....");
    }

    #[test]
    fn enter_is_strict_greater_than() {
        let (mut state, mut seconds) = settled_gate();
        assert_eq!(run_gate(&mut state, &mut seconds, &[3.0]), "."); // 3.0 is not > ENTER
        assert_eq!(run_gate(&mut state, &mut seconds, &[3.000001]), "C");
    }

    #[test]
    fn oscillation_between_lines_holds() {
        let (mut state, mut seconds) = settled_gate();
        let between: Vec<f64> = [2.0, 1.6, 2.9, 1.8].repeat(5);
        assert_eq!(run_gate(&mut state, &mut seconds, &between), ".".repeat(20));
        assert_eq!(run_gate(&mut state, &mut seconds, &[4.0]), "C");
        assert_eq!(run_gate(&mut state, &mut seconds, &between), "C".repeat(20));
    }

    #[test]
    fn exit_requires_hold() {
        let (mut state, mut seconds) = settled_gate();
        assert_eq!(run_gate(&mut state, &mut seconds, &[4.0]), "C");
        // a blip above EXIT resets the hold
        assert_eq!(
            run_gate(&mut state, &mut seconds, &[0.1, 0.1, 4.0, 0.1, 0.1]),
            "CCCCC"
        );
        assert_eq!(run_gate(&mut state, &mut seconds, &[4.0]), "C");
        assert_eq!(run_gate(&mut state, &mut seconds, &[0.1, 0.1, 0.1]), "CC.");
    }

    // ---- end-to-end parity with chromatic_gate.production_chromatic (majmin + ratio gate) ----

    enum Step {
        Chroma(ChromaEvidence),
        Silence,
    }

    /// Feeds the steps; returns the matcher, the Chromatic flag after every step and the
    /// gate statistic of the current set after every step (NaN before the first evidence).
    fn replay(steps: Vec<Step>) -> (ScaleMatcher, String, Vec<f64>) {
        let mut matcher = new_matcher();
        let mut flags = String::new();
        let mut ratios = Vec::new();
        for step in steps {
            match step {
                Step::Chroma(e) => matcher.add(&e),
                Step::Silence => matcher.add(&silent()),
            }
            let target = matcher.target().expect("target");
            flags.push(if target.scale == Scale::Chromatic {
                'C'
            } else {
                '.'
            });
            ratios.push(
                matcher
                    .current
                    .map(|c| gate_statistic(&matcher.weights(), &mask(c)))
                    .unwrap_or(f64::NAN),
            );
        }
        (matcher, flags, ratios)
    }

    fn rep(e: ChromaEvidence, n: usize) -> Vec<Step> {
        (0..n).map(|_| Step::Chroma(e.clone())).collect()
    }

    fn near(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-5,
            "ratio {actual} != python {expected}"
        );
    }

    #[test]
    fn parity_clean_c_major() {
        let (m, flags, ratios) = replay(rep(evidence(&C_MAJOR, 1.0), 30));
        assert_eq!(flags, "CCCC..........................");
        for (i, r) in [
            (0, 0.538462),
            (3, 0.225806),
            (4, 0.189189),
            (9, 0.104478),
            (29, 0.037433),
        ] {
            near(ratios[i], r);
        }
        assert_eq!(pair(&m), (Some(0), Scale::Major));
    }

    const D_AND_DFLAT: [(usize, f64); 8] = [
        (5, 1.0),
        (7, 1.0),
        (8, 1.0),
        (10, 1.0),
        (0, 1.0),
        (1, 1.0),
        (3, 0.8),
        (2, 1.0),
    ];

    #[test]
    fn parity_d_and_dflat_strong_stays_chromatic_with_the_set_as_candidate() {
        // F minor notes + D natural; E-flat weaker (0.8). Python: Eb major set, Chromatic all 20 s.
        let (m, flags, ratios) = replay(rep(weighted(&D_AND_DFLAT), 20));
        assert_eq!(flags, "C".repeat(20));
        for (i, r) in [(0, 1.359008), (1, 1.574841), (9, 2.107498), (19, 2.252581)] {
            near(ratios[i], r);
        }
        let target = m.target().unwrap();
        assert_eq!(target.scale, Scale::Chromatic);
        assert_eq!(target.candidate, candidate(3, Scale::Major));
        assert_eq!(target.key, Some(3));
        assert_eq!(target.uncovered_notes, vec![1]); // D-flat: the one note outside Eb major
    }

    #[test]
    fn parity_equal_weight_extra_note_leaves_chromatic() {
        // E-flat also at 1.0: the statistic is exactly 1.0 < EXIT (Python: "CCCC" then clear).
        let mut levels = D_AND_DFLAT;
        levels[6].1 = 1.0;
        let (_, flags, ratios) = replay(rep(weighted(&levels), 20));
        assert_eq!(flags, "CCCC................");
        near(ratios[19], 1.0);
    }

    #[test]
    fn parity_set_change_does_not_reenter_chromatic() {
        let mut steps = rep(evidence(&C_MAJOR, 1.0), 12);
        steps.extend(rep(evidence(&G_MAJOR, 1.0), 25));
        let (m, flags, ratios) = replay(steps);
        assert_eq!(flags, "CCCC.................................");
        for (i, r) in [
            (11, 0.088608),
            (12, 0.164557),
            (29, 1.455696),
            (30, 0.652893),
            (36, 0.503185),
        ] {
            near(ratios[i], r);
        }
        assert_eq!(pair(&m), (Some(7), Scale::Major));
    }

    #[test]
    fn parity_silent_steps_change_nothing_then_a_borrowed_set_enters() {
        let mut steps = rep(evidence(&C_MAJOR, 1.0), 12);
        steps.extend((0..3).map(|_| Step::Silence));
        let f_minor_plus_d = [
            (2, 1.0),
            (1, 1.0),
            (5, 0.5),
            (7, 0.5),
            (8, 0.5),
            (10, 0.5),
            (0, 0.5),
            (3, 0.5),
        ];
        steps.extend(rep(weighted(&f_minor_plus_d), 15));
        let (_, flags, ratios) = replay(steps);
        assert_eq!(flags, "CCCC........................CC");
        for (i, r) in [
            (12, 0.088608),
            (14, 0.088608),
            (15, 0.312458),
            (29, 3.446369),
        ] {
            near(ratios[i], r);
        }
    }

    #[test]
    fn parity_a_borrowed_note_below_the_lines_stays_out_of_chromatic() {
        let mut steps = rep(evidence(&C_MAJOR, 1.0), 10);
        steps.extend(rep(evidence(&[0, 2, 4, 5, 7, 9, 11, 1], 1.0), 12));
        let (_, flags, ratios) = replay(steps);
        assert_eq!(flags, "CCCC..................");
        near(ratios[21], 0.538462);
    }

    // ---- target semantics ----

    #[test]
    fn no_evidence_is_chromatic_without_key() {
        let mut matcher = new_matcher();
        let target = matcher.target().expect("a target exists from the start");
        assert_eq!(target.scale, Scale::Chromatic);
        assert_eq!(target.key, None);
        assert_eq!(target.candidate, None);
        assert!(target.uncovered_notes.is_empty());
        assert_eq!(target.evidence_seconds, 0.0);
        feed(&mut matcher, &C_MAJOR, 10);
        matcher.reset();
        let target = matcher.target().unwrap();
        assert_eq!(
            (target.key, target.scale, target.candidate),
            (None, Scale::Chromatic, None)
        );
    }

    #[test]
    fn chromatic_until_min_seconds_then_commits() {
        let mut matcher = new_matcher();
        feed(&mut matcher, &C_MAJOR, 3);
        let early = matcher.target().unwrap();
        assert_eq!(early.scale, Scale::Chromatic);
        assert_eq!(early.key, Some(0), "key follows the current naming");
        assert_eq!(early.candidate, candidate(0, Scale::Major));
        feed(&mut matcher, &C_MAJOR, 27);
        assert_eq!(pair(&matcher), (Some(0), Scale::Major));
        let done = matcher.target().unwrap();
        assert_eq!(done.evidence_seconds, 30.0);
        assert_eq!(done.candidate, candidate(0, Scale::Major));
    }

    #[test]
    fn first_evidence_picks_a_covering_major_or_minor_candidate() {
        let mut matcher = new_matcher();
        feed(&mut matcher, &[0, 4, 7], 1); // one second of a C major triad
        let target = matcher.target().expect("candidate on first evidence");
        let allowed = mask(matcher.current.unwrap());
        assert!([0, 4, 7].iter().all(|pc| allowed[*pc]), "{target:?}");
        assert!(target.candidate.is_some());
    }

    #[test]
    fn pentatonic_evidence_narrows_to_a_covering_diatonic_set() {
        let mut matcher = new_matcher();
        feed(&mut matcher, &[5, 8, 10, 0, 3], 30);
        let allowed = mask(matcher.current.unwrap());
        assert!([5, 8, 10, 0, 3].iter().all(|pc| allowed[*pc]));
    }

    #[test]
    fn silence_and_invalid_steps_are_ignored() {
        let mut matcher = new_matcher();
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
        let target = matcher.target().unwrap();
        assert_eq!(
            (target.key, target.scale, target.evidence_seconds),
            (None, Scale::Chromatic, 0.0)
        );
    }

    #[test]
    fn hysteresis_requires_a_clear_improvement_to_switch() {
        let mut matcher = new_matcher();
        feed(&mut matcher, &C_MAJOR, 30);
        assert_eq!(pair(&matcher).0, Some(0));
        feed(&mut matcher, &G_MAJOR, 1);
        assert_eq!(pair(&matcher).0, Some(0));
        feed(&mut matcher, &G_MAJOR, 120);
        assert_eq!(pair(&matcher), (Some(7), Scale::Major));
    }

    #[test]
    fn reset_forgets_the_song_and_the_gate_starts_chromatic_again() {
        let mut matcher = new_matcher();
        feed(&mut matcher, &C_MAJOR, 30);
        matcher.reset();
        let target = matcher.target().unwrap();
        assert_eq!((target.key, target.scale), (None, Scale::Chromatic));
        feed(&mut matcher, &C_MAJOR, 2);
        assert_eq!(matcher.target().unwrap().scale, Scale::Chromatic);
    }

    // ---- naming of relative major / minor ----

    #[test]
    fn relative_pair_follows_stable_hint() {
        let mut matcher = new_matcher();
        feed(&mut matcher, &AB_MAJOR, 30);
        let first = matcher.target().unwrap();
        assert_eq!(
            (first.key, first.scale),
            (Some(8), Scale::Major),
            "G# Major"
        );
        matcher.set_name_hint(Some(musical(5, Mode::Minor)));
        let second = matcher.target().unwrap();
        assert_eq!(
            (second.key, second.scale, second.candidate),
            (Some(5), Scale::Minor, candidate(5, Scale::Minor)),
            "F Minor"
        );
        matcher.set_name_hint(Some(musical(0, Mode::Major))); // unrelated: keeps the last name
        assert_eq!(pair(&matcher), (Some(5), Scale::Minor));
        matcher.set_name_hint(None);
        assert_eq!(pair(&matcher), (Some(5), Scale::Minor));
    }

    #[test]
    fn hint_names_the_set_even_before_the_gate_commits() {
        let mut matcher = new_matcher();
        matcher.set_name_hint(Some(musical(5, Mode::Minor)));
        feed(&mut matcher, &AB_MAJOR, 2); // still Chromatic, candidate named by the hint
        let target = matcher.target().unwrap();
        assert_eq!(target.scale, Scale::Chromatic);
        assert_eq!(target.candidate, candidate(5, Scale::Minor));
    }

    #[test]
    fn reset_clears_the_hint() {
        let mut matcher = new_matcher();
        matcher.set_name_hint(Some(musical(5, Mode::Minor)));
        matcher.reset();
        feed(&mut matcher, &AB_MAJOR, 30);
        assert_eq!(pair(&matcher), (Some(8), Scale::Major));
    }

    #[test]
    fn same_hint_twice_does_not_change_target() {
        let mut matcher = new_matcher();
        feed(&mut matcher, &AB_MAJOR, 30);
        matcher.set_name_hint(Some(musical(5, Mode::Minor)));
        let once = matcher.target().unwrap();
        matcher.set_name_hint(Some(musical(5, Mode::Minor)));
        assert_eq!(matcher.target().unwrap(), once);
    }

    // ---- transpose / serialization ----

    #[test]
    fn transpose_shifts_key_and_candidate() {
        let target = AutoTuneTarget {
            key: Some(0),
            scale: Scale::Chromatic,
            candidate: candidate(11, Scale::Minor),
            uncovered_notes: vec![1, 10],
            evidence_seconds: 1.0,
            source: TargetSource::Analysis,
        };
        let up = transpose(target.clone(), 2);
        assert_eq!(up.key, Some(2));
        assert_eq!(up.scale, Scale::Chromatic);
        assert_eq!(up.candidate, candidate(1, Scale::Minor));
        assert_eq!(up.uncovered_notes, vec![3, 0]);
        assert_eq!(transpose(target, -1).key, Some(11));
        let none = transpose(new_matcher().target().unwrap(), 5);
        assert_eq!((none.key, none.candidate), (None, None));
    }

    #[test]
    fn target_serializes_with_profile_compatible_names() {
        let json = serde_json::to_value(AutoTuneTarget {
            key: Some(6),
            scale: Scale::Chromatic,
            candidate: candidate(6, Scale::Minor),
            uncovered_notes: vec![2, 1],
            evidence_seconds: 12.5,
            source: TargetSource::Cache,
        })
        .unwrap();
        assert_eq!(json["key"], 6);
        assert_eq!(json["scale"], "chromatic");
        assert_eq!(json["candidate"]["key"], 6);
        assert_eq!(json["candidate"]["scale"], "minor");
        assert_eq!(json["uncoveredNotes"], serde_json::json!([2, 1]));
        assert_eq!(json["evidenceSeconds"], 12.5);
        assert_eq!(json["source"], "cache");
        assert_eq!(json.as_object().unwrap().len(), 6);
        let bare = serde_json::to_value(new_matcher().target().unwrap()).unwrap();
        assert!(bare["key"].is_null() && bare["candidate"].is_null());
    }

    // ---- seeding (Major/Minor only until Task 3) ----

    #[test]
    fn a_seed_is_reported_immediately_and_held_against_early_contrary_evidence() {
        let mut matcher = new_matcher();
        matcher.seed(6, Scale::Minor, None);
        let seeded = matcher.target().unwrap();
        assert_eq!(
            (seeded.key, seeded.scale, seeded.source),
            (Some(6), Scale::Minor, TargetSource::Cache)
        );
        feed(&mut matcher, &C_MAJOR, 19); // contrary, but under the hold
        assert_eq!(pair(&matcher), (Some(6), Scale::Minor));
        assert_eq!(matcher.target().unwrap().source, TargetSource::Cache);
        feed(&mut matcher, &C_MAJOR, 40); // overwhelming: analysis takes over
        assert_eq!(pair(&matcher), (Some(0), Scale::Major));
        assert_eq!(matcher.target().unwrap().source, TargetSource::Analysis);
    }

    #[test]
    fn a_confirmed_seed_keeps_its_cache_source() {
        let mut matcher = new_matcher();
        matcher.seed(0, Scale::Major, None);
        feed(&mut matcher, &C_MAJOR, 40);
        assert_eq!(pair(&matcher), (Some(0), Scale::Major));
        assert_eq!(matcher.target().unwrap().source, TargetSource::Cache);
        matcher.reset();
        let target = matcher.target().unwrap();
        assert_eq!((target.key, target.scale), (None, Scale::Chromatic));
    }
}
