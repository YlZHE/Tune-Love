//! Streaming visualization analysis; does not alter the retained PCM or capture routing.
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::{collections::VecDeque, sync::Arc};

const SIZE: usize = 2048;
const HOP: usize = 512;
// 96 hops ~= 1.024 s, in audio time (independent of packet size or IPC polling).
const HISTORY: usize = 96;
const NOISE_FLOOR: f64 = 0.0001; // -80 dBFS RMS; do not amplify digital near-silence.

pub struct BandAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    ring: Vec<[f32; 2]>,
    window: Vec<f32>,
    buffer: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    write: usize,
    filled: usize,
    since_fft: usize,
    bands: [f64; 3],
    normalization: f64,
    energy_history: VecDeque<[f64; 3]>,
    reference_scratch: Vec<f64>,
}

impl Default for BandAnalyzer {
    fn default() -> Self {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(SIZE);
        let window: Vec<f32> = (0..SIZE)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / SIZE as f32).cos())
            .collect();
        let normalization = SIZE as f64 * window.iter().map(|w| (*w as f64).powi(2)).sum::<f64>();
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        Self {
            fft,
            ring: vec![[0.0; 2]; SIZE],
            window,
            buffer: vec![Complex::default(); SIZE],
            scratch,
            write: 0,
            filled: 0,
            since_fft: 0,
            bands: [0.0; 3],
            normalization,
            energy_history: VecDeque::with_capacity(HISTORY),
            reference_scratch: Vec::with_capacity(HISTORY),
        }
    }
}

impl BandAnalyzer {
    pub fn reset(&mut self) {
        self.ring.fill([0.0; 2]);
        self.write = 0;
        self.filled = 0;
        self.since_fft = 0;
        self.bands = [0.0; 3];
        self.energy_history.clear();
        self.reference_scratch.clear();
    }

    pub fn push(&mut self, samples: &[f32]) -> [f64; 3] {
        for frame in samples.chunks_exact(2) {
            self.ring[self.write] =
                [frame[0], frame[1]].map(|v| if v.is_finite() { v } else { 0.0 });
            self.write = (self.write + 1) % SIZE;
            self.filled = (self.filled + 1).min(SIZE);
            self.since_fft += 1;
            if self.filled == SIZE && self.since_fft >= HOP {
                self.analyze();
                self.since_fft = 0;
            }
        }
        self.bands
    }

    fn analyze(&mut self) {
        let mut power = [0.0f64; 3];
        // Analyze channel power separately: averaging waveforms cancels anti-phase stereo.
        for channel in 0..2 {
            let mean = self.ring.iter().map(|v| v[channel]).sum::<f32>() / SIZE as f32;
            for i in 0..SIZE {
                self.buffer[i] = Complex::new(
                    (self.ring[(self.write + i) % SIZE][channel] - mean) * self.window[i],
                    0.0,
                );
            }
            self.fft
                .process_with_scratch(&mut self.buffer, &mut self.scratch);
            for bin in 1..SIZE / 2 {
                let hz = bin as f64 * super::signal::SAMPLE_RATE as f64 / SIZE as f64;
                let band = if (20.0..250.0).contains(&hz) {
                    0
                } else if (250.0..2_000.0).contains(&hz) {
                    1
                } else if (2_000.0..=20_000.0).contains(&hz) {
                    2
                } else {
                    continue;
                };
                // One-sided factor 2 and stereo mean /2 cancel.
                power[band] += self.buffer[bin].norm_sqr() as f64 / self.normalization;
            }
        }
        let rms = power.map(f64::sqrt);
        let strongest = rms.iter().copied().fold(0.0, f64::max);
        // The relative gate prevents FFT leakage from being normalized into another
        // full-strength band. The absolute floor also works when every band is quiet.
        let energy = rms.map(|v| {
            if v.is_finite() && v >= NOISE_FLOOR && v >= strongest * 0.01 {
                v
            } else {
                0.0
            }
        });
        if self.energy_history.len() == HISTORY {
            self.energy_history.pop_front();
        }
        self.energy_history.push_back(energy);
        for band in 0..3 {
            if energy[band] == 0.0 {
                self.bands[band] = 0.0;
                continue;
            }
            self.reference_scratch.clear();
            self.reference_scratch.extend(
                self.energy_history
                    .iter()
                    .map(|v| v[band])
                    .filter(|v| *v > 0.0),
            );
            self.reference_scratch.sort_unstable_by(f64::total_cmp);
            // Recent 80th percentile, not the current frame's maximum: short beats
            // can rise above the reference and quiet passages remain distinguishable.
            let reference =
                self.reference_scratch[(self.reference_scratch.len() - 1) * 4 / 5].max(NOISE_FLOOR);
            let sustained = 1.0 - (-1.6 * energy[band] / reference).exp();
            self.bands[band] = if band < 2 {
                // Bass underneath a kick and compressed midrange often never fall
                // silent. Normalizing only against a high percentile puts both
                // the bed and its accents near 0.8, concealing their differences.
                // Expand the measured short-term range while retaining a visible
                // sustain. High-frequency response intentionally stays unchanged.
                let last = self.reference_scratch.len() - 1;
                let floor = self.reference_scratch[last / 5];
                let ceiling = self.reference_scratch[last * 19 / 20];
                let span = (ceiling - floor).max(0.0);
                // Do not expand almost-constant FFT ripple. A relative minimum
                // range also preserves the same response after a volume change.
                let contrast_weight = ((span / reference - 0.08) / 0.12).clamp(0.0, 1.0);
                let excursion = (energy[band] - floor).max(0.0) / span.max(reference * 0.2);
                // Reserve fixed headroom rather than crossfading the sustain away:
                // when an old accent leaves the percentile window, unchanged PCM
                // must not suddenly jump back up and look like another accent.
                let base = sustained * 0.8;
                let accent = (1.0 - (-4.0 * excursion).exp()) * contrast_weight;
                (base + (1.0 - base) * accent).clamp(0.0, 1.0)
            } else {
                sustained.clamp(0.0, 1.0)
            };
        }
    }
}
