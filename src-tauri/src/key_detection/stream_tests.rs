use super::{tests::cadence, Mode, MusicalKey, RollingDetector};
use crate::audio::PcmWindow;

const SECOND: usize = 48_000 * 2;

fn window(samples: &[f32], end: usize) -> PcmWindow {
    PcmWindow {
        source_id: "test-player".into(),
        track_key: "test-track".into(),
        target_generation: 1,
        capture_generation: 1,
        sample_end_sequence: (end / 2) as u64,
        captured_at_ms: (end * 1_000 / SECOND) as u64,
        sample_rate: 48_000,
        channels: 2,
        samples: samples[end.saturating_sub(8 * SECOND)..end].to_vec(),
    }
}

fn c_major() -> Option<MusicalKey> {
    Some(MusicalKey {
        pitch_class: 0,
        mode: Mode::Major,
    })
}

#[test]
fn rolling_consumes_only_new_frames_and_does_not_vote_twice() {
    let pcm = cadence(0, false, 0.65);
    let mut detector = RollingDetector::default();
    let first = detector.analyze(&window(&pcm, 6 * SECOND)).unwrap();
    assert_eq!(first.fed_frames, 288_000);
    assert!(first.reset);
    let next = detector.analyze(&window(&pcm, 7 * SECOND)).unwrap();
    assert_eq!(next.fed_frames, 48_000);
    assert!(!next.reset);
    let last = window(&pcm, 8 * SECOND);
    assert_eq!(detector.analyze(&last).unwrap().candidate, c_major());
    let duplicate = detector.analyze(&last).unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.fed_frames, 0);
    assert_eq!(duplicate.candidate, None);
}

#[test]
fn rolling_keeps_low_volume_and_antiphase_keys_without_changing_input() {
    for (scale, antiphase) in [(1.0, false), (0.001, false), (1.0, true)] {
        let mut pcm = cadence(0, false, 0.65);
        for frame in pcm.chunks_exact_mut(2) {
            frame[0] *= scale;
            frame[1] = frame[0] * if antiphase { -1.0 } else { 1.0 };
        }
        let before = pcm.clone();
        let mut detector = RollingDetector::default();
        for second in 6..=8 {
            let result = detector.analyze(&window(&pcm, second * SECOND)).unwrap();
            if second == 8 {
                assert_eq!(result.candidate, c_major());
            }
        }
        assert_eq!(pcm, before);
    }
}

#[test]
fn rolling_is_bounded_and_forgets_old_harmony() {
    let major = cadence(0, false, 0.65);
    let minor = cadence(9, true, 0.65);
    let pcm: Vec<_> = major
        .iter()
        .chain(minor.iter().cycle().take(SECOND * 80))
        .copied()
        .collect();
    let mut detector = RollingDetector::default();
    for second in 6..=88 {
        let result = detector.analyze(&window(&pcm, second * SECOND)).unwrap();
        assert!(result.retained_hops > 0 && result.retained_hops <= 9);
        assert!(result.buffered_samples > 0 && result.buffered_samples <= 16_394);
        if second >= 24 && second % 8 == 0 {
            assert_eq!(
                result.candidate,
                Some(MusicalKey {
                    pitch_class: 9,
                    mode: Mode::Minor
                })
            );
        }
    }
}

#[test]
fn rolling_resets_for_epoch_source_track_rewind_and_missing_audio() {
    let pcm: Vec<_> = cadence(0, false, 0.65)
        .into_iter()
        .cycle()
        .take(SECOND * 20)
        .collect();
    let mut detector = RollingDetector::default();
    let first = window(&pcm, 8 * SECOND);
    detector.analyze(&first).unwrap();
    let mut next = window(&pcm, 9 * SECOND);
    assert!(!detector.analyze(&next).unwrap().reset);
    for field in 0..4 {
        match field {
            0 => next.capture_generation += 1,
            1 => next.target_generation += 1,
            2 => next.source_id.push('2'),
            _ => next.track_key.push('2'),
        }
        let result = detector.analyze(&next).unwrap();
        assert!(result.reset);
        assert_eq!(result.fed_frames, 384_000);
    }
    detector.reset();
    detector.analyze(&first).unwrap();
    assert!(detector.analyze(&window(&pcm, 7 * SECOND)).unwrap().reset);
    let result = detector.analyze(&window(&pcm, 20 * SECOND)).unwrap();
    assert!(result.reset);
    assert_eq!(result.fed_frames, 384_000);
}

#[test]
fn rolling_rejects_bad_inputs_and_recovers_without_reusing_partial_state() {
    let pcm = cadence(0, false, 0.65);
    let mut detector = RollingDetector::default();
    let mut bad = window(&pcm, 6 * SECOND);
    bad.samples[0] = f32::NAN;
    assert!(detector.analyze(&bad).is_err());
    bad = window(&pcm, 6 * SECOND);
    bad.sample_end_sequence = 1;
    assert!(detector.analyze(&bad).is_err());
    bad = window(&pcm, 6 * SECOND);
    bad.samples.truncate(SECOND * 5);
    assert!(detector.analyze(&bad).is_err());
    assert_eq!(
        detector
            .analyze(&window(&pcm, 8 * SECOND))
            .unwrap()
            .candidate,
        c_major()
    );
}

#[test]
fn rolling_silence_does_not_return_old_candidate() {
    let pcm = cadence(0, false, 0.65);
    let mut detector = RollingDetector::default();
    detector.analyze(&window(&pcm, 8 * SECOND)).unwrap();
    let silence = vec![0.0; SECOND * 16];
    let result = detector.analyze(&window(&silence, 16 * SECOND)).unwrap();
    assert_eq!(result.candidate, None);
    let mut resumed = window(&pcm, 8 * SECOND);
    resumed.sample_end_sequence = 24 * 48_000;
    let result = detector.analyze(&resumed).unwrap();
    assert!(result.reset);
    assert_eq!(result.candidate, c_major());
}

#[test]
fn rolling_handles_irregular_deltas_and_downsample_remainders() {
    let pcm: Vec<_> = cadence(0, false, 0.65)
        .into_iter()
        .cycle()
        .take(SECOND * 24)
        .collect();
    let mut detector = RollingDetector::default();
    let mut end = 6 * 48_000;
    let mut fed = detector.analyze(&window(&pcm, end * 2)).unwrap().fed_frames;
    // 45,056 raw frames per FFT hop for the pinned configuration; probe both sides.
    let deltas = [1, 10, 45_055, 1, 2, 48_791, 30_013, 11, 73_937];
    let mut i = 0;
    while end < 24 * 48_000 {
        let delta = deltas[i % deltas.len()].min(24 * 48_000 - end);
        end += delta;
        let result = detector.analyze(&window(&pcm, end * 2)).unwrap();
        assert_eq!(result.fed_frames, delta);
        assert!(!result.reset);
        assert!(result.retained_hops <= 9);
        assert!(result.buffered_samples <= 16_394);
        fed += result.fed_frames;
        if end == 24 * 48_000 {
            assert_eq!(result.candidate, c_major());
        }
        i += 1;
    }
    assert_eq!(fed, 24 * 48_000);
}

#[test]
fn rolling_clipping_fallback_matches_batch_then_recovers_incremental_feed() {
    let pcm: Vec<_> = cadence(0, false, 0.65)
        .into_iter()
        .cycle()
        .take(SECOND * 12)
        .collect();
    let mut detector = RollingDetector::default();
    detector.analyze(&window(&pcm, 8 * SECOND)).unwrap();
    let mut clipped = window(&pcm, 9 * SECOND);
    // Deliberate over-range sample models an input requiring the original clamp.
    clipped.samples[200] = 4.0;
    clipped.samples[201] = 4.0;
    let expected = super::detect(&clipped.samples, 48_000, 2).unwrap();
    let result = detector.analyze(&clipped).unwrap();
    assert!(result.batch_fallback);
    assert_eq!(result.candidate, expected);
    let next = detector.analyze(&window(&pcm, 10 * SECOND)).unwrap();
    assert!(next.reset);
    assert_eq!(next.fed_frames, 384_000);
    let warm = detector.analyze(&window(&pcm, 11 * SECOND)).unwrap();
    assert!(!warm.reset);
    assert_eq!(warm.fed_frames, 48_000);
}

#[test]
fn rolling_channel_selection_change_reseeds_instead_of_mixing_histories() {
    let pcm: Vec<_> = cadence(0, false, 0.65)
        .into_iter()
        .cycle()
        .take(SECOND * 12)
        .collect();
    let mut detector = RollingDetector::default();
    detector.analyze(&window(&pcm, 8 * SECOND)).unwrap();
    for (second, right_gain) in [(9, -1.0), (10, -1.1), (11, 1.0)] {
        let mut input = window(&pcm, second * SECOND);
        for frame in input.samples.chunks_exact_mut(2) {
            frame[1] *= right_gain;
        }
        let result = detector.analyze(&input).unwrap();
        assert!(result.reset);
        assert_eq!(result.fed_frames, 384_000);
    }
    assert!(!detector.analyze(&window(&pcm, 12 * SECOND)).unwrap().reset);
}

#[test]
fn an_opted_in_shorter_window_is_accepted_and_the_default_still_rejects_it() {
    let pcm = cadence(0, false, 0.65);
    let mut default = RollingDetector::default();
    assert!(default.analyze(&window(&pcm, 4 * SECOND)).is_err());
    let mut early = RollingDetector::with_min_seconds(3);
    assert!(early.analyze(&window(&pcm, 2 * SECOND)).is_err());
    let at_three = early.analyze(&window(&pcm, 3 * SECOND)).unwrap();
    assert!(at_three.evidence.is_none(), "no complete libkeyfinder hop before ~3.75 s");
    let at_four = early.analyze(&window(&pcm, 4 * SECOND)).unwrap();
    assert!(at_four.evidence.is_some_and(|e| e.seconds > 0.0));
}

#[test]
fn rolling_reports_pitch_class_evidence_oriented_at_c_for_new_hops() {
    // C major cadence: evidence must concentrate on the seven diatonic bins
    // (C first), cover roughly the audio each feed adds, and skip silence.
    let pcm: Vec<_> = cadence(0, false, 0.65)
        .into_iter()
        .cycle()
        .take(SECOND * 20)
        .collect();
    let mut detector = RollingDetector::default();
    let mut total_seconds = 0.0;
    let mut summed = [0.0; 12];
    for second in 6..=20 {
        let result = detector.analyze(&window(&pcm, second * SECOND)).unwrap();
        if let Some(step) = result.evidence {
            assert!(step.seconds > 0.5 && step.seconds < 9.0, "{}", step.seconds);
            assert!(step.rms > 1.0e-3);
            assert!(step.chroma.iter().all(|v| v.is_finite() && *v >= 0.0));
            total_seconds += step.seconds;
            for (slot, v) in summed.iter_mut().zip(step.chroma) {
                *slot += v;
            }
        }
    }
    // Evidence should cover most of the 20 s of audio (the first window is one step).
    assert!(total_seconds > 12.0 && total_seconds < 22.0, "{total_seconds}");
    let diatonic: f64 = [0, 2, 4, 5, 7, 9, 11].iter().map(|pc| summed[*pc]).sum();
    let chromatic_rest: f64 = [1, 3, 6, 8, 10].iter().map(|pc| summed[*pc]).sum();
    assert!(diatonic > chromatic_rest * 3.0, "{summed:?}");
    assert!(summed[0] > summed[1] && summed[0] > summed[11], "{summed:?}");

    let silent = PcmWindow {
        samples: vec![0.0; 8 * SECOND],
        ..window(&pcm, 20 * SECOND)
    };
    let mut quiet = RollingDetector::default();
    assert!(quiet.analyze(&silent).unwrap().evidence.is_none());
}
