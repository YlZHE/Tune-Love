use crate::{
    audio::PcmWindow,
    key_detection::{self, Mode, MusicalKey, RollingDetector},
};

fn pcm() -> Vec<f32> {
    (0..384_000)
        .flat_map(|i| {
            let t = i as f64 / 48_000.0;
            let x = ([261.625565, 329.627557, 391.995436]
                .iter()
                .map(|f| (std::f64::consts::TAU * f * t).sin())
                .sum::<f64>()
                * 0.16) as f32;
            [x, x]
        })
        .collect()
}

#[test]
fn diagnostics_match_native_candidate_and_are_c_ordered() {
    let samples = pcm();
    let normal = key_detection::detect(&samples, 48_000, 2).unwrap();
    let result = key_detection::detect_with_diagnostics(&samples, 48_000, 2).unwrap();
    assert_eq!(normal, result.candidate);
    let d = result.diagnostics.unwrap();
    assert_eq!(d.score_kind, "cosine_similarity");
    assert_eq!(d.scores.len(), 24);
    for (i, s) in d.scores.iter().enumerate() {
        assert_eq!(
            s.key,
            MusicalKey {
                pitch_class: (i / 2) as u8,
                mode: if i % 2 == 0 { Mode::Major } else { Mode::Minor }
            }
        );
        assert!(s.score.is_finite());
    }
    let mut sorted = d.scores.to_vec();
    sorted.sort_by(|a, b| b.score.total_cmp(&a.score));
    assert_eq!(d.best.score, sorted[0].score);
    assert_eq!(d.runner_up.score, sorted[1].score);
    assert_eq!(d.margin, d.best.score - d.runner_up.score);
    assert_eq!(Some(d.best.key), normal);
}

#[test]
fn diagnostics_are_optional_and_silence_duplicates_stay_empty() {
    let samples = pcm();
    let w = PcmWindow {
        source_id: "lab".into(),
        track_key: "one".into(),
        target_generation: 1,
        capture_generation: 1,
        sample_end_sequence: 384_000,
        captured_at_ms: 8000,
        sample_rate: 48_000,
        channels: 2,
        samples,
    };
    let mut normal = RollingDetector::default();
    let mut diagnostic = RollingDetector::default();
    let a = normal.analyze(&w).unwrap();
    let (b, d) = diagnostic.analyze_with_diagnostics(&w).unwrap();
    assert_eq!(a.candidate, b.candidate);
    assert_eq!(a.fed_frames, b.fed_frames);
    assert!(d.is_some());
    let (repeat, d) = diagnostic.analyze_with_diagnostics(&w).unwrap();
    assert!(repeat.duplicate);
    assert!(d.is_none());
    let silent = vec![0.0; 576_000];
    let d = key_detection::detect_with_diagnostics(&silent, 48_000, 2).unwrap();
    assert!(d.candidate.is_none() && d.diagnostics.is_none());
}

#[test]
fn diagnostic_path_rejects_invalid_audio() {
    assert!(key_detection::detect_with_diagnostics(&[0.0], 48_000, 2).is_err());
    let mut samples = vec![0.0; 576_000];
    samples[0] = f32::NAN;
    assert!(key_detection::detect_with_diagnostics(&samples, 48_000, 2).is_err());
    samples[0] = 0.0;
    assert!(key_detection::detect_with_diagnostics(&samples, 44_100, 2).is_err());
}

#[test]
fn diagnostic_scores_reorder_values_and_preserve_upstream_tie_priority() {
    let input = std::array::from_fn(|i| i as f64 / 24.0);
    let d = super::engine::diagnostics(input).unwrap();
    for (i, s) in d.scores.iter().enumerate() {
        assert_eq!(s.score, input[(i + 6) % 24]);
    }
    assert_eq!(
        d.best.key,
        MusicalKey {
            pitch_class: 8,
            mode: Mode::Minor
        }
    );
    assert_eq!(d.best.score, input[23]);
    let tied = super::engine::diagnostics([0.5; 24]).unwrap();
    assert_eq!(
        tied.best.key,
        MusicalKey {
            pitch_class: 9,
            mode: Mode::Major
        }
    );
    assert_eq!(
        tied.runner_up.key,
        MusicalKey {
            pitch_class: 9,
            mode: Mode::Minor
        }
    );
    assert_eq!(tied.margin, 0.0);
    let mut bad = input;
    bad[0] = f64::NAN;
    assert!(super::engine::diagnostics(bad).is_err());
}

#[test]
fn diagnostic_batch_matches_original_for_transposed_and_edge_fixtures() {
    let mut fixtures = Vec::new();
    for root in 0..12 {
        for minor in [false, true] {
            fixtures.push(super::tests::cadence(root, minor, 0.65));
        }
    }
    for amplitude in [0.00065, 0.000001, 4.0] {
        fixtures.push(super::tests::cadence(0, false, amplitude));
    }
    let mut anti = super::tests::cadence(9, true, 0.65);
    for frame in anti.chunks_exact_mut(2) {
        frame[1] = -frame[0];
    }
    fixtures.push(anti);
    fixtures.push(vec![0.0; 576_000]);
    for (i, samples) in fixtures.iter().enumerate() {
        let ordinary = key_detection::detect(samples, 48_000, 2).unwrap();
        let diagnostic = key_detection::detect_with_diagnostics(samples, 48_000, 2).unwrap();
        assert_eq!(ordinary, diagnostic.candidate, "fixture {i}");
        if let Some(d) = diagnostic.diagnostics {
            assert_eq!(Some(d.best.key), ordinary, "fixture {i}");
        }
    }
}
