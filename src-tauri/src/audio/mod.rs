mod bands;
pub mod capture;
pub mod process;
pub mod signal;
use crate::devocal::gate::AttenuationGate;
use crate::media::{AudioTarget, MediaState};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioLevel {
    pub source_id: Option<String>,
    pub track_key: Option<String>,
    pub status: &'static str,
    pub rms: f64,
    pub peak: f64,
    pub level: f64,
    pub bands: [f64; 3],
    pub updated_at_ms: u64,
}
impl Default for AudioLevel {
    fn default() -> Self {
        Self {
            source_id: None,
            track_key: None,
            status: "idle",
            rms: 0.0,
            peak: 0.0,
            level: 0.0,
            bands: [0.0; 3],
            updated_at_ms: 0,
        }
    }
}
#[derive(Clone, Debug)]
pub struct PcmWindow {
    pub source_id: String,
    pub track_key: String,
    pub target_generation: u64,
    pub capture_generation: u64,
    pub sample_end_sequence: u64,
    pub captured_at_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AnalysisContext {
    pub source_id: Option<String>,
    pub track_key: Option<String>,
    pub target_generation: u64,
    pub capture_generation: u64,
    pub sample_end_sequence: u64,
    pub captured_at_ms: u64,
    pub sample_count: usize,
    pub playing: bool,
}

#[derive(Default)]
struct Inner {
    target: Option<AudioTarget>,
    worker_generation: u64,
    capture_generation: u64,
    sample_end_sequence: u64,
    captured_at_ms: u64,
    stalled: bool,
    level: AudioLevel,
    samples: VecDeque<f32>,
    analyzer: bands::BandAnalyzer,
    /// Undoes the player attenuation while devocal holds its sessions.
    gate: Arc<AttenuationGate>,
    gate_epoch: u64,
}
impl Inner {
    fn with_gate(gate: Arc<AttenuationGate>) -> Self {
        Self {
            gate_epoch: gate.read().0,
            gate,
            ..Self::default()
        }
    }

    fn invalidate_pcm(&mut self) {
        self.capture_generation = self.capture_generation.wrapping_add(1);
        self.samples.clear();
        self.analyzer.reset();
        self.captured_at_ms = 0;
        self.stalled = true;
    }

    fn sync(&mut self, target: Option<AudioTarget>, _now: u64) -> u64 {
        let changed = self.target.as_ref() != target.as_ref();
        self.target = target;
        if changed {
            self.worker_generation = self.worker_generation.wrapping_add(1);
            self.invalidate_pcm();
            self.level = AudioLevel::default();
        }
        self.worker_generation
    }
    fn publish(&mut self, worker_generation: u64, packet: signal::Packet, now: u64) -> bool {
        if worker_generation != self.worker_generation
            || self.target.as_ref().is_none_or(|t| !t.playing)
        {
            return false;
        }
        // History recorded under another attenuation is useless: invalidate before anything
        // else, even when this packet is then dropped.
        let (epoch, gain) = self.gate.read();
        if epoch != self.gate_epoch {
            self.invalidate_pcm();
            self.gate_epoch = epoch;
        }
        let Some(gain) = gain else {
            return false;
        };
        let mut packet = packet;
        if gain != 1.0 {
            for v in &mut packet.samples {
                let scaled = (*v * gain).clamp(-4.0, 4.0);
                *v = if scaled.is_finite() { scaled } else { 0.0 };
            }
        }
        if packet.reset {
            self.invalidate_pcm();
        }
        self.sample_end_sequence = self
            .sample_end_sequence
            .saturating_add((packet.samples.len() / signal::CHANNELS as usize) as u64);
        signal::retain_samples(&mut self.samples, &packet.samples);
        self.captured_at_ms = now;
        self.stalled = false;
        let (rms, peak) = signal::measure(&packet.samples);
        let bands = self.analyzer.push(&packet.samples);
        let t = self.target.as_ref().unwrap();
        self.level = AudioLevel {
            source_id: Some(t.source_id.clone()),
            track_key: Some(t.track_key.clone()),
            status: "capturing",
            rms: rms.clamp(0.0, 1.0),
            peak: peak.clamp(0.0, 1.0),
            level: peak.clamp(0.0, 1.0),
            bands,
            updated_at_ms: now,
        };
        true
    }
    fn expire(&mut self, now: u64) {
        if self.level.updated_at_ms == 0 {
            return;
        }
        if now.saturating_sub(self.level.updated_at_ms) > 400 {
            self.level.rms = 0.0;
            self.level.peak = 0.0;
            self.level.level = 0.0;
            self.level.bands = [0.0; 3];
            self.analyzer.reset();
        }
        if now.saturating_sub(self.level.updated_at_ms) > 1_000 {
            if !self.stalled {
                self.invalidate_pcm();
            }
        }
    }

    fn analysis_context(&self) -> AnalysisContext {
        AnalysisContext {
            source_id: self.target.as_ref().map(|target| target.source_id.clone()),
            track_key: self.target.as_ref().map(|target| target.track_key.clone()),
            target_generation: self
                .target
                .as_ref()
                .map_or(0, |target| target.target_generation),
            capture_generation: self.capture_generation,
            sample_end_sequence: self.sample_end_sequence,
            captured_at_ms: self.captured_at_ms,
            sample_count: self.samples.len(),
            playing: self.target.as_ref().is_some_and(|target| target.playing),
        }
    }

    fn pcm_window(&self) -> Option<PcmWindow> {
        let target = self.target.as_ref()?;
        if !target.playing || self.samples.is_empty() {
            return None;
        }
        Some(PcmWindow {
            source_id: target.source_id.clone(),
            track_key: target.track_key.clone(),
            target_generation: target.target_generation,
            capture_generation: self.capture_generation,
            sample_end_sequence: self.sample_end_sequence,
            captured_at_ms: self.captured_at_ms,
            sample_rate: signal::SAMPLE_RATE,
            channels: signal::CHANNELS,
            samples: self.samples.iter().copied().collect(),
        })
    }
}
#[derive(Clone)]
pub struct AudioState {
    inner: Arc<Mutex<Inner>>,
    media: MediaState,
    stop: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
}
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
impl AudioState {
    pub fn new(media: MediaState, gate: Arc<AttenuationGate>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::with_gate(gate))),
            media,
            stop: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn snapshot(&self) -> AudioLevel {
        self.refresh();
        self.lock().level.clone()
    }
    pub fn analysis_window(&self) -> Option<PcmWindow> {
        self.refresh();
        self.lock().pcm_window()
    }

    pub(crate) fn analysis_context(&self) -> AnalysisContext {
        self.refresh();
        self.lock().analysis_context()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn refresh(&self) -> u64 {
        self.refresh_from(|| self.media.audio_target())
    }
    fn refresh_from(&self, read_target: impl FnOnce() -> Option<AudioTarget>) -> u64 {
        // Always acquire audio before observing media. Media writers never acquire
        // audio, so concurrent refreshes cannot commit observations in reverse order.
        let mut i = self.lock();
        let target = if self.stop.load(Ordering::Acquire) {
            None
        } else {
            read_target()
        };
        let generation = i.sync(target, now_ms());
        i.expire(now_ms());
        generation
    }
    fn current(&self, generation: u64) -> bool {
        self.refresh() == generation
            && !self.stop.load(Ordering::Acquire)
            && self.lock().target.as_ref().is_some_and(|t| t.playing)
    }
    fn status(&self, generation: u64, status: &'static str) {
        let mut i = self.lock();
        if i.worker_generation != generation {
            return;
        }
        i.invalidate_pcm();
        let t = i.target.clone();
        i.level = AudioLevel {
            source_id: t.as_ref().map(|t| t.source_id.clone()),
            track_key: t.as_ref().map(|t| t.track_key.clone()),
            status,
            ..Default::default()
        };
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.refresh();
    }
    pub fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) || self.stop.load(Ordering::Acquire) {
            return;
        }
        let state = self.clone();
        if std::thread::Builder::new()
            .name("process-audio-worker".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.worker()))
                    .is_err()
                {
                    let g = state.refresh();
                    state.status(g, "unavailable");
                }
            })
            .is_err()
        {
            let g = self.refresh();
            self.status(g, "unavailable");
        }
    }
    fn worker(&self) {
        #[cfg(windows)]
        {
            let Ok(_apartment) = capture::Apartment::enter() else {
                let g = self.refresh();
                self.status(g, "unavailable");
                return;
            };
            while !self.stop.load(Ordering::Acquire) {
                let generation = self.refresh();
                let target = self.lock().target.clone();
                let Some(target) = target.filter(|t| t.playing) else {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                };
                self.status(generation, "resolving");
                let result = process::native::resolve(&target.source_id).and_then(|identity| {
                    capture::run(
                        identity,
                        None,
                        || self.current(generation),
                        |packet| {
                            // Read current media again immediately before committing any result.
                            if self.current(generation) {
                                self.lock().publish(generation, packet, now_ms());
                            }
                        },
                    )
                });
                if self.current(generation) {
                    self.status(generation, "unavailable");
                    if let Err(error) = result {
                        eprintln!("Current-player audio unavailable: {error}");
                    }
                    for _ in 0..40 {
                        if !self.current(generation) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
        }
        #[cfg(not(windows))]
        {
            let g = self.refresh();
            self.status(g, "unavailable");
        }
    }
}
#[tauri::command]
pub fn get_audio_level(state: tauri::State<'_, AudioState>) -> AudioLevel {
    state.snapshot()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn target(source: &str, track: &str) -> AudioTarget {
        AudioTarget {
            source_id: source.into(),
            track_key: track.into(),
            playing: true,
            target_generation: 1,
        }
    }
    fn packet() -> signal::Packet {
        signal::Packet {
            samples: vec![0.25, -0.25],
            reset: false,
        }
    }
    fn tone(frequencies: &[f32], inverted_right: bool) -> signal::Packet {
        let samples = (0..9_600)
            .flat_map(|n| {
                let x = frequencies
                    .iter()
                    .map(|f| (std::f32::consts::TAU * f * n as f32 / 48_000.0).sin() * 0.25)
                    .sum::<f32>();
                [x, if inverted_right { -x } else { x }]
            })
            .collect();
        signal::Packet {
            samples,
            reset: false,
        }
    }
    fn bands(state: &Inner) -> Vec<f64> {
        let json = serde_json::to_value(&state.level).unwrap();
        assert!(
            json["bands"].is_array(),
            "IPC must contain three independently measured frequency bands"
        );
        json["bands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect()
    }
    #[test]
    fn frequency_bands_isolate_low_mid_high_even_for_opposite_phase_stereo() {
        for inverted in [false, true] {
            for (selected, frequency) in [100.0, 1_000.0, 8_000.0].iter().enumerate() {
                let mut s = Inner::default();
                let g = s.sync(Some(target("a", "one")), 1);
                // Exercise packet boundaries, not just a single complete FFT input.
                for block in tone(&[*frequency], inverted).samples.chunks(960) {
                    s.publish(
                        g,
                        signal::Packet {
                            samples: block.to_vec(),
                            reset: false,
                        },
                        2,
                    );
                }
                let measured = bands(&s);
                assert_eq!(measured.len(), 3);
                assert!(
                    measured[selected] > 0.3,
                    "{frequency}Hz must drive band {selected}: {measured:?}"
                );
                for (other, value) in measured.iter().enumerate() {
                    if other != selected {
                        assert!(*value < 0.03, "cross-band leakage: {measured:?}");
                    }
                }
            }
        }
    }
    #[test]
    fn frequency_bands_keep_mixed_energy_and_clear_on_every_lifecycle_boundary() {
        let mut s = Inner::default();
        let mut g = s.sync(Some(target("a", "one")), 1);
        s.publish(g, tone(&[100.0, 1_000.0, 8_000.0], false), 2);
        assert!(bands(&s).iter().all(|v| *v > 0.3 && *v <= 1.0));
        s.expire(403);
        assert_eq!(bands(&s), [0.0; 3]);
        // A short fresh silent packet must not resurrect the expired FFT history.
        s.publish(g, packet(), 404);
        assert_eq!(bands(&s), [0.0; 3]);
        s.publish(g, tone(&[1_000.0], false), 405);
        let mut reset = packet();
        reset.reset = true;
        s.publish(g, reset, 406);
        assert_eq!(bands(&s), [0.0; 3]);
        s.publish(g, tone(&[100.0], false), 407);
        g = s.sync(Some(target("a", "two")), 408);
        assert_eq!(bands(&s), [0.0; 3]);
        s.publish(g, tone(&[8_000.0], false), 409);
        let mut paused = target("a", "two");
        paused.playing = false;
        s.sync(Some(paused), 410);
        assert_eq!(bands(&s), [0.0; 3]);
    }
    #[test]
    fn frequency_bands_preserve_short_transients_instead_of_normalizing_every_frame_to_full() {
        let mut s = Inner::default();
        let g = s.sync(Some(target("a", "one")), 1);
        let mut medium = tone(&[100.0], false);
        medium.samples.iter_mut().for_each(|v| *v *= 2.0);
        s.publish(g, medium, 2);
        let before = bands(&s)[0];
        let mut loud = tone(&[100.0], false);
        loud.samples.iter_mut().for_each(|v| *v *= 3.6);
        let mut after: f64 = 0.0;
        for block in loud.samples[..7_680].chunks(960) {
            s.publish(
                g,
                signal::Packet {
                    samples: block.to_vec(),
                    reset: false,
                },
                3,
            );
            after = after.max(bands(&s)[0]);
        }
        assert!(
            after > before + 0.04,
            "louder clean audio must not sit on the same hard ceiling: {before} -> {after}"
        );
        assert!(after < 1.0);
    }
    #[test]
    fn normalization_keeps_the_same_rhythm_visible_at_one_percent_volume() {
        fn curve(gain: f32) -> Vec<Vec<f64>> {
            let mut s = Inner::default();
            let g = s.sync(Some(target("a", "one")), 1);
            let mut curve = Vec::new();
            for block in 0..240 {
                let samples = (block * 480..(block + 1) * 480)
                    .flat_map(|n| {
                        let t = n as f32 / 48_000.0;
                        let x = [100.0, 1_000.0, 8_000.0]
                            .iter()
                            .enumerate()
                            .map(|(band, hz)| {
                                let envelope = if (block / (12 + band * 5)) % 2 == 0 {
                                    0.22
                                } else {
                                    0.06
                                };
                                gain * envelope * (std::f32::consts::TAU * hz * t).sin()
                            })
                            .sum::<f32>();
                        [x, x]
                    })
                    .collect();
                s.publish(
                    g,
                    signal::Packet {
                        samples,
                        reset: false,
                    },
                    block as u64 + 2,
                );
                if block > 110 {
                    curve.push(bands(&s));
                }
            }
            curve
        }
        let normal = curve(1.0);
        let quiet = curve(0.01);
        for band in 0..3 {
            let peak = quiet.iter().map(|v| v[band]).fold(0.0f64, f64::max);
            let low = quiet.iter().map(|v| v[band]).fold(1.0f64, f64::min);
            assert!(
                peak > 0.6 && peak - low > 0.2,
                "quiet band {band} must retain visible rhythm: {low}..{peak}"
            );
        }
        for (a, b) in normal.iter().zip(quiet.iter()) {
            for i in 0..3 {
                assert!(
                    (a[i] - b[i]).abs() < 0.03,
                    "volume changed visual envelope: {a:?} vs {b:?}"
                );
            }
        }
    }
    #[test]
    fn sustained_bass_does_not_hide_kicks_in_the_low_band() {
        for gain in [1.0, 0.01] {
            let mut analyzer = bands::BandAnalyzer::default();
            let mut idle = Vec::new();
            let mut hits = Vec::new();
            for block in 0..400 {
                let samples = (block * 480..(block + 1) * 480)
                    .flat_map(|n| {
                        let t = n as f32 / 48_000.0;
                        let beat = t % 0.5;
                        let bass = 0.18 * (std::f32::consts::TAU * 55.0 * t).sin();
                        let kick =
                            0.16 * (-beat / 0.055).exp() * (std::f32::consts::TAU * 95.0 * t).sin();
                        let x = gain * (bass + kick);
                        [x, x]
                    })
                    .collect::<Vec<_>>();
                let measured = analyzer.push(&samples);
                if block >= 150 {
                    match block % 50 {
                        3..=8 => hits.push(measured[0]),
                        25..=45 => idle.push(measured[0]),
                        _ => {}
                    }
                }
            }
            let peak = hits.iter().copied().fold(0.0f64, f64::max);
            let baseline = idle.iter().sum::<f64>() / idle.len() as f64;
            eprintln!(
                "bass gain={gain}: baseline={baseline:.4}, kick peak={peak:.4}, contrast={:.4}",
                peak - baseline
            );
            assert!(baseline > 0.1, "sustained 808 must remain visible");
            assert!(peak - baseline > 0.25,
                "kick must visibly rise above sustained bass at gain {gain}: baseline={baseline}, peak={peak}");
        }
    }

    #[test]
    fn mid_band_preserves_syllable_contrast_in_compressed_harmonic_audio() {
        for gain in [1.0, 0.01] {
            let mut analyzer = bands::BandAnalyzer::default();
            let mut curve = Vec::new();
            for block in 0..400 {
                let samples = (block * 480..(block + 1) * 480)
                    .flat_map(|n| {
                        let t = n as f32 / 48_000.0;
                        // A modest +/- 12% envelope, without silent gaps. Harmonic
                        // fixture only: not a claim of vocal source separation.
                        let envelope =
                            0.15 * (1.0 + 0.12 * (std::f32::consts::TAU * 3.0 * t).sin());
                        let voiced = (std::f32::consts::TAU * 600.0 * t).sin()
                            + 0.5 * (std::f32::consts::TAU * 1_200.0 * t).sin();
                        let x = gain * envelope * voiced;
                        [x, x]
                    })
                    .collect::<Vec<_>>();
                let measured = analyzer.push(&samples);
                if block >= 150 {
                    curve.push(measured[1]);
                }
            }
            curve.sort_unstable_by(f64::total_cmp);
            let low = curve[curve.len() / 10];
            let high = curve[curve.len() * 9 / 10];
            eprintln!(
                "mid gain={gain}: p10={low:.4}, p90={high:.4}, contrast={:.4}",
                high - low
            );
            assert!(
                high - low > 0.3,
                "mid-band articulation was flattened at gain {gain}: {low}..{high}"
            );
        }
    }

    #[test]
    fn steady_low_and_mid_tones_do_not_generate_false_rhythmic_accents() {
        let mut failures = Vec::new();
        for frequency in [20.0, 25.0, 30.0, 40.0, 55.0, 100.0, 600.0] {
            let mut analyzer = bands::BandAnalyzer::default();
            let selected = if frequency < 250.0 { 0 } else { 1 };
            let mut curve = Vec::new();
            for block in 0..300 {
                let samples = (block * 480..(block + 1) * 480)
                    .flat_map(|n| {
                        let x =
                            0.2 * (std::f32::consts::TAU * frequency * n as f32 / 48_000.0).sin();
                        [x, x]
                    })
                    .collect::<Vec<_>>();
                let measured = analyzer.push(&samples);
                if block > 150 {
                    curve.push(measured[selected]);
                }
            }
            let low = curve.iter().copied().fold(1.0f64, f64::min);
            let high = curve.iter().copied().fold(0.0f64, f64::max);
            eprintln!("steady {frequency}Hz: {low:.4}..{high:.4}");
            if !(low > 0.5 && high - low < 0.08) {
                failures.push(format!(
                    "steady {frequency}Hz acquired false accents: {low}..{high}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[test]
    fn an_accent_ending_does_not_create_a_later_phantom_accent() {
        for frequency in [55.0, 600.0] {
            let mut analyzer = bands::BandAnalyzer::default();
            let selected = if frequency < 250.0 { 0 } else { 1 };
            let mut tail = Vec::new();
            for block in 0..360 {
                let amplitude = if (150..158).contains(&block) {
                    0.27
                } else {
                    0.18
                };
                let samples = (block * 480..(block + 1) * 480)
                    .flat_map(|n| {
                        let x = amplitude
                            * (std::f32::consts::TAU * frequency * n as f32 / 48_000.0).sin();
                        [x, x]
                    })
                    .collect::<Vec<_>>();
                let measured = analyzer.push(&samples);
                // Only steady PCM remains here. Include the history-expiry point,
                // not just the eventual steady state after all adaptation finishes.
                if block >= 185 {
                    tail.push(measured[selected]);
                }
            }
            let largest_jump = tail.windows(2).map(|v| v[1] - v[0]).fold(0.0f64, f64::max);
            let low = tail.iter().copied().fold(1.0f64, f64::min);
            let high = tail.iter().copied().fold(0.0f64, f64::max);
            assert!(largest_jump < 0.05 && high - low < 0.08,
                "expired accent caused false {frequency}Hz motion: {low}..{high}, jump={largest_jump}");
        }
    }

    #[test]
    fn normalization_adapts_after_volume_drop_but_never_amplifies_silence_or_tiny_noise() {
        let mut s = Inner::default();
        let g = s.sync(Some(target("a", "one")), 1);
        for _ in 0..6 {
            s.publish(g, tone(&[100.0, 1_000.0, 8_000.0], false), 2);
        }
        for _ in 0..7 {
            let mut quiet = tone(&[100.0, 1_000.0, 8_000.0], false);
            quiet.samples.iter_mut().for_each(|v| *v *= 0.01);
            s.publish(g, quiet, 3);
        }
        assert!(bands(&s).iter().all(|v| *v > 0.6));
        for _ in 0..8 {
            let mut noise = tone(&[100.0, 1_000.0, 8_000.0], false);
            noise.samples.iter_mut().for_each(|v| *v *= 0.00001);
            s.publish(g, noise, 4);
            assert_eq!(bands(&s), [0.0; 3]);
        }
        s.publish(
            g,
            signal::Packet {
                samples: vec![0.0; 19_200],
                reset: false,
            },
            5,
        );
        assert_eq!(bands(&s), [0.0; 3]);
    }
    #[test]
    fn full_scale_overflow_clamps_ipc_without_changing_analysis_pcm() {
        let mut state = Inner::default();
        let generation = state.sync(Some(target("a", "one")), 1);
        state.publish(
            generation,
            signal::Packet {
                samples: vec![2.0, -2.0],
                reset: false,
            },
            2,
        );
        assert_eq!(
            (state.level.rms, state.level.peak, state.level.level),
            (1.0, 1.0, 1.0)
        );
        assert_eq!(
            state.samples.iter().copied().collect::<Vec<_>>(),
            vec![2.0, -2.0]
        );
    }
    #[test]
    fn concurrent_refresh_cannot_restore_an_older_observed_source() {
        use std::sync::mpsc;
        use std::time::Duration;
        let state = AudioState::new(MediaState::default(), Arc::new(AttenuationGate::new()));
        let (observed_tx, observed_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = state.clone();
        let first_thread = std::thread::spawn(move || {
            first.refresh_from(|| {
                observed_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Some(target("old", "old-song"))
            })
        });
        observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = state.clone();
        let (second_tx, second_rx) = mpsc::channel();
        let second_thread = std::thread::spawn(move || {
            let generation = second.refresh_from(|| Some(target("new", "new-song")));
            second_tx.send(generation).unwrap();
        });
        // With the broken ordering this commits the new source before the older
        // observation is released, deterministically exposing the rollback.
        let _ = second_rx.recv_timeout(Duration::from_millis(250));
        release_tx.send(()).unwrap();
        first_thread.join().unwrap();
        second_thread.join().unwrap();
        assert_eq!(state.lock().target.as_ref().unwrap().source_id, "new");
    }
    #[test]
    fn generation_change_rejects_late_old_frames_and_clears_pcm() {
        let mut s = Inner::default();
        let a = s.sync(Some(target("a", "one")), 1);
        assert!(s.publish(a, packet(), 2));
        assert_eq!(s.samples.len(), 2);
        let b = s.sync(Some(target("b", "two")), 3);
        assert_ne!(a, b);
        assert!(s.samples.is_empty());
        assert!(!s.publish(a, packet(), 4));
        assert_eq!(s.level.level, 0.0);
        assert!(s.publish(b, packet(), 5));
    }
    #[test]
    fn same_source_new_track_and_pause_each_clear_samples() {
        let mut s = Inner::default();
        let a = s.sync(Some(target("a", "one")), 1);
        s.publish(a, packet(), 2);
        let b = s.sync(Some(target("a", "two")), 3);
        assert_ne!(a, b);
        assert!(s.samples.is_empty());
        s.publish(b, packet(), 4);
        let mut paused = target("a", "two");
        paused.playing = false;
        s.sync(Some(paused), 5);
        assert!(s.samples.is_empty());
        assert_eq!(s.level.status, "idle");
        assert!(!s.publish(b, packet(), 6));
    }
    #[test]
    fn timeout_expires_levels_without_immediate_failure_and_long_stall_clears_history() {
        let mut s = Inner::default();
        let g = s.sync(Some(target("a", "one")), 1);
        s.publish(g, packet(), 2);
        s.expire(403);
        assert_eq!(s.level.level, 0.0);
        assert_eq!(s.level.status, "capturing");
        assert_eq!(s.samples.len(), 2);
        s.expire(2003);
        assert!(s.samples.is_empty());
    }
    #[test]
    fn discontinuity_clears_previous_samples_before_adding_new_packet() {
        let mut s = Inner::default();
        let g = s.sync(Some(target("a", "one")), 1);
        s.publish(g, packet(), 2);
        s.publish(
            g,
            signal::Packet {
                samples: vec![0.5, 0.5],
                reset: true,
            },
            3,
        );
        assert_eq!(
            s.samples.iter().copied().collect::<Vec<_>>(),
            vec![0.5, 0.5]
        );
    }

    fn const_packet(value: f32) -> signal::Packet {
        signal::Packet {
            samples: vec![value; 4],
            reset: false,
        }
    }

    #[test]
    fn gate_scales_samples() {
        let gate = Arc::new(AttenuationGate::new());
        let mut s = Inner::with_gate(gate.clone());
        let worker = s.sync(Some(target("a", "one")), 1);
        gate.set(Some(3000.0));
        assert!(s.publish(worker, const_packet(1e-4), 2));
        assert_eq!(s.samples.len(), 4);
        for v in &s.samples {
            assert!((v - 0.3).abs() < 1e-6, "{v}");
        }
    }

    #[test]
    fn gate_clamps_and_sanitises_scaled_samples() {
        let gate = Arc::new(AttenuationGate::new());
        let mut s = Inner::with_gate(gate.clone());
        let worker = s.sync(Some(target("a", "one")), 1);
        gate.set(Some(1e6));
        let packet = signal::Packet {
            samples: vec![1.0, -1.0, f32::NAN, f32::INFINITY],
            reset: false,
        };
        assert!(s.publish(worker, packet, 2));
        assert_eq!(
            s.samples.iter().copied().collect::<Vec<_>>(),
            vec![4.0, -4.0, 0.0, 4.0]
        );
    }

    #[test]
    fn gate_epoch_change_invalidates_history() {
        let gate = Arc::new(AttenuationGate::new());
        let mut s = Inner::with_gate(gate.clone());
        let worker = s.sync(Some(target("a", "one")), 1);
        assert!(s.publish(worker, const_packet(0.5), 2));
        let before = s.analysis_context().capture_generation;
        assert_eq!(s.samples.len(), 4);
        gate.set(Some(1.0));
        assert!(s.publish(
            worker,
            signal::Packet {
                samples: vec![0.1, 0.1],
                reset: false,
            },
            3
        ));
        assert_eq!(s.analysis_context().capture_generation, before + 1);
        assert_eq!(
            s.samples.iter().copied().collect::<Vec<_>>(),
            vec![0.1, 0.1]
        );
        // The same epoch no longer invalidates.
        assert!(s.publish(worker, packet(), 4));
        assert_eq!(s.analysis_context().capture_generation, before + 1);
    }

    #[test]
    fn gate_none_drops_packet() {
        let gate = Arc::new(AttenuationGate::new());
        let mut s = Inner::with_gate(gate.clone());
        let worker = s.sync(Some(target("a", "one")), 1);
        assert!(s.publish(worker, packet(), 2));
        let end = s.sample_end_sequence;
        let level = s.level.clone();
        gate.set(None);
        assert!(!s.publish(worker, packet(), 3));
        assert_eq!(s.sample_end_sequence, end);
        assert_eq!(s.level.updated_at_ms, level.updated_at_ms);
        assert_eq!(s.level.peak, level.peak);
    }

    #[test]
    fn discontinuity_invalidates_old_analysis_but_keeps_accepting_current_capture_packets() {
        let mut s = Inner::default();
        let worker_generation = s.sync(Some(target("a", "one")), 1);
        assert!(s.publish(worker_generation, packet(), 2));
        let before = s.analysis_context();

        let mut reset = packet();
        reset.reset = true;
        assert!(s.publish(worker_generation, reset, 3));
        let after_reset = s.analysis_context();
        assert_ne!(before.capture_generation, after_reset.capture_generation);
        assert!(s.publish(worker_generation, packet(), 4));
        assert_eq!(
            s.analysis_context().capture_generation,
            after_reset.capture_generation
        );
    }

    #[test]
    fn sample_cursor_advances_monotonically_across_reset_and_target_change() {
        let mut s = Inner::default();
        let first_worker = s.sync(Some(target("a", "one")), 1);
        s.publish(first_worker, packet(), 2);
        let first = s.analysis_context().sample_end_sequence;
        let mut reset = packet();
        reset.reset = true;
        s.publish(first_worker, reset, 3);
        let second = s.analysis_context().sample_end_sequence;
        let second_worker = s.sync(Some(target("b", "two")), 4);
        s.publish(second_worker, packet(), 5);
        let third = s.analysis_context().sample_end_sequence;
        assert!(
            first < second && second < third,
            "{first}, {second}, {third}"
        );
    }

    #[test]
    fn stall_reconnect_pause_and_stop_each_invalidate_pending_analysis() {
        let media = MediaState::default();
        let state = AudioState::new(media, Arc::new(AttenuationGate::new()));
        let worker = state.refresh_from(|| Some(target("a", "one")));
        state.lock().publish(worker, packet(), 10);
        let initial = state.lock().analysis_context().capture_generation;

        state.lock().expire(1_011);
        let stalled = state.lock().analysis_context().capture_generation;
        assert_ne!(initial, stalled);

        state.status(worker, "unavailable");
        let reconnecting = state.lock().analysis_context().capture_generation;
        assert_ne!(stalled, reconnecting);

        let mut paused = target("a", "one");
        paused.playing = false;
        state.refresh_from(|| Some(paused));
        let paused_generation = state.lock().analysis_context().capture_generation;
        assert_ne!(reconnecting, paused_generation);

        state.stop();
        let stopped = state.lock().analysis_context().capture_generation;
        assert_ne!(paused_generation, stopped);
    }

    #[test]
    fn analysis_window_carries_media_capture_cursor_and_timestamp_identity() {
        let mut state = Inner::default();
        let mut target = target("a", "one");
        target.target_generation = 42;
        let worker = state.sync(Some(target), 1);
        state.publish(worker, packet(), 123);
        let window = state.pcm_window().unwrap();
        assert_eq!(window.target_generation, 42);
        assert!(window.capture_generation > 0);
        assert!(window.sample_end_sequence > 0);
        assert_eq!(window.captured_at_ms, 123);
    }
}
