mod engine;
pub mod scale_match;
pub mod song_cache;
mod stability;
mod stream;
mod types;

#[cfg(test)]
mod diagnostics_tests;
#[cfg(test)]
mod stream_tests;

pub use engine::{detect, detect_with_diagnostics};
pub use stream::{RollingDetector, StreamAnalysis};
pub use scale_match::{AutoTuneTarget, Scale, TargetSource};
pub use types::{ChromaEvidence, DiagnosticAnalysis, KeyDiagnostics, KeyScore, Mode, MusicalKey};

use crate::audio::{AnalysisContext, AudioState, PcmWindow};
use stability::{Identity, Observation, Stabilizer};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

// Analysis starts once 3 s of the song are captured, so Key/Scale evidence can
// arrive ~4 s in (libkeyfinder's first hop needs ~3.75 s). The key *label* keeps
// its original 6 s rule: shorter windows feed the scale matcher but never vote.
// Evidence: docs/2026-10-02_scale-match-stage-b-report.md (early start section).
const MIN_SECONDS: usize = 3;
const LABEL_MIN_SAMPLES: usize = 48_000 * 2 * 6;
const MIN_SAMPLES: usize = 48_000 * 2 * MIN_SECONDS;
const MAX_SAMPLES: usize = 48_000 * 2 * 8;
const CAPTURE_FRESH_MS: u64 = 1_000;
const IDLE_POLL: Duration = Duration::from_millis(50);
const POST_ANALYSIS_DELAY: Duration = Duration::from_secs(1);

/// Short opening windows contribute scale evidence only, never a key-label vote.
fn label_vote(candidate: Option<MusicalKey>, window_samples: usize) -> Option<MusicalKey> {
    candidate.filter(|_| window_samples >= LABEL_MIN_SAMPLES)
}

fn next_analysis_delay(elapsed: Duration) -> Duration {
    POST_ANALYSIS_DELAY.saturating_sub(elapsed).max(IDLE_POLL)
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionSnapshot {
    pub source_id: Option<String>,
    pub track_key: Option<String>,
    pub target_generation: u64,
    pub status: &'static str,
    pub key: Option<MusicalKey>,
    pub updated_at_ms: u64,
    /// Recommended Auto-Tune Key/Scale (Major/Minor only) for the current song;
    /// None until the first non-silent evidence of that song.
    pub autotune_target: Option<AutoTuneTarget>,
}

#[derive(Clone)]
pub struct KeyDetectionState {
    audio: AudioState,
    inner: Arc<Mutex<Stabilizer>>,
    stop: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    cache: Arc<Mutex<song_cache::SongCache>>,
}

fn observation(context: &AnalysisContext) -> Observation {
    Observation {
        identity: context
            .source_id
            .as_ref()
            .zip(context.track_key.as_ref())
            .map(|(source_id, track_key)| Identity {
                source_id: source_id.clone(),
                track_key: track_key.clone(),
                target_generation: context.target_generation,
            }),
        playing: context.playing,
        capture_generation: context.capture_generation,
        sample_end_sequence: context.sample_end_sequence,
    }
}

fn window_observation(window: &PcmWindow) -> Observation {
    Observation {
        identity: Some(Identity {
            source_id: window.source_id.clone(),
            track_key: window.track_key.clone(),
            target_generation: window.target_generation,
        }),
        playing: true,
        capture_generation: window.capture_generation,
        sample_end_sequence: window.sample_end_sequence,
    }
}

fn analysis_ready(context: &AnalysisContext, now: u64) -> bool {
    context.playing
        && context.source_id.is_some()
        && context.track_key.is_some()
        && (MIN_SAMPLES..=MAX_SAMPLES).contains(&context.sample_count)
        && context.captured_at_ms > 0
        && now.saturating_sub(context.captured_at_ms) <= CAPTURE_FRESH_MS
}

fn window_is_current(window: &PcmWindow, current: &AnalysisContext, now: u64) -> bool {
    analysis_ready(current, now)
        && current.source_id.as_deref() == Some(window.source_id.as_str())
        && current.track_key.as_deref() == Some(window.track_key.as_str())
        && current.target_generation == window.target_generation
        && current.capture_generation == window.capture_generation
        && current.sample_end_sequence >= window.sample_end_sequence
        && current.captured_at_ms >= window.captured_at_ms
        && (MIN_SAMPLES..=MAX_SAMPLES).contains(&window.samples.len())
}

impl KeyDetectionState {
    pub fn new(audio: AudioState) -> Self {
        Self {
            audio,
            inner: Arc::new(Mutex::new(Stabilizer::new())),
            stop: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
            wake: Arc::new((Mutex::new(()), Condvar::new())),
            cache: Arc::new(Mutex::new(song_cache::SongCache::default())),
        }
    }

    /// Load the per-song Key/Scale memory from `path` (call before `start`).
    pub fn use_song_cache(&self, path: std::path::PathBuf) {
        let loaded = song_cache::SongCache::load(path);
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = loaded;
        self.lock().set_cache(self.cache.clone());
    }

    /// Remember the current song's target once it is well supported. Disk I/O
    /// happens here, on the analysis worker, with no other lock held.
    fn remember(&self, track_key: &str, target: &AutoTuneTarget) {
        let Some(id) = song_cache::song_id(track_key) else { return };
        let (bytes, path) = {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            (cache.put(&id, target, crate::audio::now_ms()), cache.path().map(|p| p.to_path_buf()))
        };
        if let (Some(bytes), Some(path)) = (bytes, path) {
            if let Err(error) = song_cache::persist(&path, &bytes) {
                eprintln!("Song key cache not saved: {error}");
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Stabilizer> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn snapshot(&self) -> DetectionSnapshot {
        // Preserve the established audio -> media ordering, then acquire key state.
        // This prevents a delayed key-state refresh from restoring an older target.
        let context = self.audio.analysis_context();
        let observed = observation(&context);
        let stable = {
            let mut state = self.lock();
            state.observe(&observed, crate::audio::now_ms());
            state.snapshot()
        };
        DetectionSnapshot {
            source_id: stable
                .identity
                .as_ref()
                .map(|value| value.source_id.clone()),
            track_key: stable
                .identity
                .as_ref()
                .map(|value| value.track_key.clone()),
            target_generation: stable
                .identity
                .as_ref()
                .map_or(0, |value| value.target_generation),
            status: stable.status,
            key: stable.key,
            updated_at_ms: stable.updated_at_ms,
            autotune_target: stable.autotune_target,
        }
    }

    pub fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) || self.stop.load(Ordering::Acquire) {
            return;
        }
        let state = self.clone();
        if std::thread::Builder::new()
            .name("key-detection-worker".into())
            .spawn(move || state.worker())
            .is_err()
        {
            self.stop();
        }
    }

    pub fn stop(&self) {
        // Transition the predicate while holding the same mutex used by wait().
        // This closes the notify-before-sleep window of a condition variable.
        let wake_guard = self
            .wake
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.stop.store(true, Ordering::Release);
        self.wake.1.notify_all();
        drop(wake_guard);
        // Never hold the wake mutex while acquiring analysis state.
        self.lock().stop(crate::audio::now_ms());
    }

    fn wait(&self, duration: Duration) -> bool {
        if self.stop.load(Ordering::Acquire) {
            return false;
        }
        let guard = self
            .wake
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = self
            .wake
            .1
            .wait_timeout_while(guard, duration, |_| !self.stop.load(Ordering::Acquire));
        !self.stop.load(Ordering::Acquire)
    }

    fn worker(&self) {
        let mut detector = RollingDetector::with_min_seconds(MIN_SECONDS);
        while !self.stop.load(Ordering::Acquire) {
            let now = crate::audio::now_ms();
            let context = self.audio.analysis_context();
            let observed = observation(&context);
            let should_copy = {
                let mut state = self.lock();
                state.observe(&observed, now);
                analysis_ready(&context, now) && state.can_begin(&observed)
            };
            if !should_copy {
                if !analysis_ready(&context, now) {
                    detector.reset();
                }
                if !self.wait(IDLE_POLL) {
                    break;
                }
                continue;
            }

            let Some(window) = self.audio.analysis_window() else {
                if !self.wait(IDLE_POLL) {
                    break;
                }
                continue;
            };
            let current = self.audio.analysis_context();
            let commit_now = crate::audio::now_ms();
            if !window_is_current(&window, &current, commit_now) {
                if !self.wait(IDLE_POLL) {
                    break;
                }
                continue;
            }
            let window_observed = window_observation(&window);
            let current_observed = observation(&current);
            let token = {
                let mut state = self.lock();
                state.begin_if_current(&window_observed, &current_observed, commit_now)
            };
            let Some(token) = token else {
                if !self.wait(IDLE_POLL) {
                    break;
                }
                continue;
            };

            let analysis_started = Instant::now();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                detector.analyze(&window)
            }));
            let elapsed_ms = analysis_started.elapsed().as_millis();
            let (candidate, evidence, failed, result_label) = match outcome {
                Ok(Ok(result)) => {
                    eprintln!("Key stream: fed_frames={} retained_hops={} buffered_samples={} reset={} batch_fallback={} evidence_seconds={:.2}",
                        result.fed_frames, result.retained_hops, result.buffered_samples, result.reset, result.batch_fallback,
                        result.evidence.map_or(0.0, |e| e.seconds));
                    (label_vote(result.candidate, window.samples.len()), result.evidence, false,
                        format!("{:?}", result.candidate))
                }
                Ok(Err(error)) => {
                    eprintln!("Key analysis unavailable: {error}");
                    (None, None, true, "error".to_owned())
                }
                Err(_) => (None, None, true, "panic".to_owned()),
            };
            if failed {
                detector.reset();
            }

            // Re-read audio/media identity after the heavyweight native call and only
            // then acquire key state. No capture or state mutex is held by analysis.
            let current = self.audio.analysis_context();
            let finished_at = crate::audio::now_ms();
            let current_observed = observation(&current);
            let remembered = {
                let mut state = self.lock();
                state.complete_with_evidence(
                    &token,
                    &current_observed,
                    candidate,
                    evidence,
                    failed,
                    finished_at,
                );
                let stable = state.snapshot();
                stable.identity.zip(stable.autotune_target)
            };
            if let Some((identity, target)) = remembered {
                self.remember(&identity.track_key, &target);
            }
            eprintln!(
                "Key analysis: window_ms={} capture_generation={} sample_end_sequence={} elapsed_ms={} result={}",
                window.samples.len() as u64 * 1_000
                    / (window.sample_rate as u64 * window.channels as u64),
                window.capture_generation,
                window.sample_end_sequence,
                elapsed_ms,
                result_label
            );

            if !self.wait(next_analysis_delay(analysis_started.elapsed())) {
                break;
            }
        }
    }
}

#[tauri::command]
pub fn get_key_detection(state: tauri::State<'_, KeyDetectionState>) -> DetectionSnapshot {
    state.snapshot()
}

#[cfg(test)]
mod tests {
    use super::{
        analysis_ready, detect, engine::map_upstream, window_is_current, AudioState,
        DetectionSnapshot, Duration, KeyDetectionState, Mode, MusicalKey, Ordering, PcmWindow,
    };
    use crate::audio::AnalysisContext;
    use std::f32::consts::TAU;

    const RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    const SECONDS: usize = 8;

    fn key(pitch_class: u8, mode: Mode) -> MusicalKey {
        MusicalKey { pitch_class, mode }
    }

    #[test]
    fn detection_snapshot_serializes_the_exact_read_only_ipc_contract() {
        let json = serde_json::to_value(DetectionSnapshot {
            source_id: Some("player".into()),
            track_key: Some("track".into()),
            target_generation: 7,
            status: "detected",
            key: Some(key(1, Mode::Minor)),
            updated_at_ms: 99,
            autotune_target: Some(super::AutoTuneTarget {
                key: 1,
                scale: super::Scale::Minor,
                evidence_seconds: 20.0,
                source: super::TargetSource::Analysis,
            }),
        })
        .unwrap();
        let fields = json.as_object().unwrap();
        assert_eq!(fields.len(), 7);
        assert_eq!(json["autotuneTarget"]["key"], 1);
        assert_eq!(json["autotuneTarget"]["scale"], "minor");
        assert_eq!(json["autotuneTarget"]["evidenceSeconds"], 20.0);
        assert_eq!(json["autotuneTarget"]["source"], "analysis");
        assert_eq!(json["sourceId"], "player");
        assert_eq!(json["trackKey"], "track");
        assert_eq!(json["targetGeneration"], 7);
        assert_eq!(json["status"], "detected");
        assert_eq!(json["key"]["pitchClass"], 1);
        assert_eq!(json["key"]["mode"], "minor");
        assert_eq!(json["updatedAtMs"], 99);
    }

    #[test]
    fn opening_windows_shorter_than_six_seconds_never_vote_for_the_label() {
        let c = Some(key(0, Mode::Major));
        assert_eq!(super::label_vote(c, 48_000 * 2 * 3), None);
        assert_eq!(super::label_vote(c, 48_000 * 2 * 6 - 2), None);
        assert_eq!(super::label_vote(c, 48_000 * 2 * 6), c);
        assert_eq!(super::label_vote(None, 48_000 * 2 * 8), None);
    }

    #[test]
    fn scheduler_requires_fresh_three_to_eight_second_stereo_window() {
        let mut context = AnalysisContext {
            source_id: Some("player".into()),
            track_key: Some("track".into()),
            target_generation: 1,
            capture_generation: 1,
            sample_end_sequence: 1,
            captured_at_ms: 1_000,
            sample_count: 48_000 * 2 * 3,
            playing: true,
        };
        assert!(analysis_ready(&context, 1_999));
        context.sample_count -= 2;
        assert!(!analysis_ready(&context, 1_999));
        context.sample_count = 48_000 * 2 * 8 + 2;
        assert!(!analysis_ready(&context, 1_999));
        context.sample_count = 48_000 * 2 * 6;
        assert!(!analysis_ready(&context, 2_001));
        context.captured_at_ms = 2_000;
        context.playing = false;
        assert!(!analysis_ready(&context, 2_001));
    }

    #[test]
    fn analysis_cadence_does_not_add_compute_time_to_every_interval() {
        assert_eq!(
            super::next_analysis_delay(Duration::from_millis(250)),
            Duration::from_millis(750)
        );
        assert_eq!(
            super::next_analysis_delay(Duration::from_secs(2)),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn slow_analysis_window_remains_current_when_capture_epoch_is_fresh() {
        let window = PcmWindow {
            source_id: "player".into(),
            track_key: "track".into(),
            target_generation: 1,
            capture_generation: 2,
            sample_end_sequence: 100,
            captured_at_ms: 1_000,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![0.0; 48_000 * 2 * 6],
        };
        let current = AnalysisContext {
            source_id: Some("player".into()),
            track_key: Some("track".into()),
            target_generation: 1,
            capture_generation: 2,
            sample_end_sequence: 200,
            captured_at_ms: 5_000,
            sample_count: 48_000 * 2 * 8,
            playing: true,
        };
        assert!(window_is_current(&window, &current, 5_001));
    }

    #[test]
    fn stop_coordinates_predicate_transition_with_wait_mutex() {
        use crate::media::MediaState;
        use std::sync::mpsc;

        let state = KeyDetectionState::new(AudioState::new(MediaState::default()));
        let wake_guard = state
            .wake
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let worker = state.clone();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (done_tx, done_rx) = mpsc::sync_channel(0);
        let thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            worker.stop();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(!state.stop.load(Ordering::Acquire));
        drop(wake_guard);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        thread.join().unwrap();
        assert!(state.stop.load(Ordering::Acquire));
        assert!(!state.wait(Duration::from_secs(1)));
    }

    pub(super) fn cadence(root: u8, minor: bool, amplitude: f32) -> Vec<f32> {
        let major = [[0, 4, 7], [5, 9, 0], [7, 11, 2], [0, 4, 7]];
        let minor_chords = [[0, 3, 7], [5, 8, 0], [7, 11, 2], [0, 3, 7]];
        let chords = if minor { minor_chords } else { major };
        let frames = RATE as usize * SECONDS;
        let chord_frames = frames / chords.len();
        let mut output = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let chord_index = (frame / chord_frames).min(chords.len() - 1);
            let local = frame % chord_frames;
            let edge = (RATE as usize / 100).max(1);
            let envelope = ((local.min(chord_frames - local - 1) as f32) / edge as f32).min(1.0);
            let time = frame as f32 / RATE as f32;
            let mut sample = 0.0;
            for interval in chords[chord_index] {
                let midi = 48 + ((root as i32 + interval as i32) % 12);
                let frequency = 440.0 * 2.0_f32.powf((midi as f32 - 69.0) / 12.0);
                sample += (TAU * frequency * time).sin();
                sample += 0.20 * (TAU * frequency * 2.0 * time).sin();
            }
            sample *= amplitude * envelope / 3.6;
            output.extend_from_slice(&[sample, sample]);
        }
        output
    }

    #[test]
    fn maps_every_upstream_key_literal() {
        let expected = [
            key(9, Mode::Major),
            key(9, Mode::Minor),
            key(10, Mode::Major),
            key(10, Mode::Minor),
            key(11, Mode::Major),
            key(11, Mode::Minor),
            key(0, Mode::Major),
            key(0, Mode::Minor),
            key(1, Mode::Major),
            key(1, Mode::Minor),
            key(2, Mode::Major),
            key(2, Mode::Minor),
            key(3, Mode::Major),
            key(3, Mode::Minor),
            key(4, Mode::Major),
            key(4, Mode::Minor),
            key(5, Mode::Major),
            key(5, Mode::Minor),
            key(6, Mode::Major),
            key(6, Mode::Minor),
            key(7, Mode::Major),
            key(7, Mode::Minor),
            key(8, Mode::Major),
            key(8, Mode::Minor),
        ];
        for (upstream, expected) in expected.into_iter().enumerate() {
            assert_eq!(
                map_upstream(upstream as i32),
                Some(expected),
                "upstream {upstream}"
            );
        }
        assert_eq!(map_upstream(24), None);
        assert_eq!(map_upstream(-1), None);
        assert_eq!(map_upstream(25), None);
    }

    #[test]
    fn rejects_invalid_shape_rate_channels_and_nonfinite_samples() {
        assert!(detect(&[0.0; 32], 44_100, 2).is_err());
        assert!(detect(&[0.0; 32], RATE, 1).is_err());
        assert!(detect(&[0.0; 31], RATE, CHANNELS).is_err());
        let mut nan_samples = vec![0.0; RATE as usize * CHANNELS as usize * 6];
        nan_samples[17] = f32::NAN;
        assert!(detect(&nan_samples, RATE, CHANNELS)
            .unwrap_err()
            .contains("non-finite"));
        let mut infinite_samples = vec![0.0; RATE as usize * CHANNELS as usize * 6];
        infinite_samples[31] = f32::INFINITY;
        assert!(detect(&infinite_samples, RATE, CHANNELS)
            .unwrap_err()
            .contains("non-finite"));
    }

    #[test]
    fn enforces_six_to_eight_second_analysis_window() {
        assert!(detect(&vec![0.0; RATE as usize * 2 * 6 - 2], RATE, CHANNELS).is_err());
        assert!(detect(&vec![0.0; RATE as usize * 2 * 8 + 2], RATE, CHANNELS).is_err());
    }

    #[test]
    fn silence_is_not_a_key() {
        assert_eq!(
            detect(&vec![0.0; RATE as usize * 2 * 8], RATE, CHANNELS).unwrap(),
            None
        );
    }

    #[test]
    fn extremely_low_noise_is_not_amplified_into_a_key() {
        let mut state = 0x9e37_79b9_u32;
        let mut samples = Vec::with_capacity(RATE as usize * 2 * 8);
        for _ in 0..RATE as usize * 8 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (((state >> 8) as f32 / 16_777_215.0) * 2.0 - 1.0) * 0.00002;
            samples.extend_from_slice(&[noise, noise]);
        }
        assert_eq!(detect(&samples, RATE, CHANNELS).unwrap(), None);
    }

    #[test]
    fn detects_labeled_c_major_cadence() {
        assert_eq!(
            detect(&cadence(0, false, 0.65), RATE, CHANNELS).unwrap(),
            Some(key(0, Mode::Major))
        );
    }

    #[test]
    fn detects_labeled_a_minor_cadence() {
        assert_eq!(
            detect(&cadence(9, true, 0.65), RATE, CHANNELS).unwrap(),
            Some(key(9, Mode::Minor))
        );
    }

    #[test]
    fn low_volume_copy_keeps_the_labeled_key_and_input_is_unchanged() {
        let loud = cadence(0, false, 0.65);
        let quiet: Vec<f32> = loud.iter().map(|sample| sample * 0.001).collect();
        let before = quiet.clone();
        assert_eq!(
            detect(&quiet, RATE, CHANNELS).unwrap(),
            Some(key(0, Mode::Major))
        );
        assert_eq!(quiet, before);
    }

    #[test]
    fn anti_phase_stereo_uses_a_non_cancelled_channel() {
        let mono = cadence(0, false, 0.65);
        let mut anti_phase = mono.clone();
        for frame in anti_phase.chunks_exact_mut(2) {
            frame[1] = -frame[0];
        }
        assert_eq!(
            detect(&anti_phase, RATE, CHANNELS).unwrap(),
            Some(key(0, Mode::Major))
        );
    }

    #[test]
    fn records_negative_fixture_outputs_without_claiming_rejection() {
        let frames = RATE as usize * SECONDS;
        let mut tone = Vec::with_capacity(frames * 2);
        let mut noise = Vec::with_capacity(frames * 2);
        let mut percussion = Vec::with_capacity(frames * 2);
        let mut state = 0x1234_5678_u32;
        for frame in 0..frames {
            let t = frame as f32 / RATE as f32;
            let single = 0.35 * (TAU * 440.0 * t).sin();
            tone.extend_from_slice(&[single, single]);
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (((state >> 8) as f32 / 16_777_215.0) * 2.0 - 1.0) * 0.2;
            noise.extend_from_slice(&[n, n]);
            let within_beat = frame % (RATE as usize / 2);
            let click = if within_beat < 400 {
                0.7 * (-(within_beat as f32) / 80.0).exp()
            } else {
                0.0
            };
            percussion.extend_from_slice(&[click, click]);
        }
        println!(
            "negative single-tone result: {:?}",
            detect(&tone, RATE, CHANNELS).unwrap()
        );
        println!(
            "negative noise result: {:?}",
            detect(&noise, RATE, CHANNELS).unwrap()
        );
        println!(
            "negative percussion result: {:?}",
            detect(&percussion, RATE, CHANNELS).unwrap()
        );
    }
}
