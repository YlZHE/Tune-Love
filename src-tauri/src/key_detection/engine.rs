use super::{ChromaEvidence, DiagnosticAnalysis, KeyDiagnostics, KeyScore, Mode, MusicalKey};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

const REQUIRED_SAMPLE_RATE: u32 = 48_000;
const REQUIRED_CHANNELS: u16 = 2;
pub(super) const MIN_SECONDS: usize = 6;
/// Lowest window a caller may opt into (libkeyfinder needs ~3.75 s for one hop).
pub(super) const SHORTEST_SECONDS: usize = 3;
const MAX_SECONDS: usize = 8;
// Measured by the deterministic low-level-noise fixture: 20 micro-scale noise
// stays below this floor, while the 0.001-amplitude labeled cadence remains usable.
const NOISE_FLOOR_RMS: f64 = 5.0e-5;
const TARGET_RMS: f64 = 0.10;
const MAX_GAIN: f64 = 64.0;
pub(super) static NATIVE_ANALYSIS: Mutex<()> = Mutex::new(());

#[cfg(windows)]
extern "C" {
    fn keyfinder_detect_mono(
        samples: *const f64,
        sample_count: usize,
        sample_rate: u32,
        output_key: *mut c_int,
        scores: *mut f64,
        chroma: *mut f64,
        hops: *mut u32,
        error: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
}

pub(super) fn map_upstream(value: i32) -> Option<MusicalKey> {
    if !(0..24).contains(&value) {
        return None;
    }
    const PITCH_CLASSES: [u8; 12] = [9, 10, 11, 0, 1, 2, 3, 4, 5, 6, 7, 8];
    Some(MusicalKey {
        pitch_class: PITCH_CLASSES[value as usize / 2],
        mode: if value % 2 == 0 {
            Mode::Major
        } else {
            Mode::Minor
        },
    })
}

pub(super) fn diagnostics(upstream: [f64; 24]) -> Result<KeyDiagnostics, String> {
    if upstream.iter().any(|score| !score.is_finite()) {
        return Err("native diagnostics contain non-finite scores".into());
    }
    let scores: [KeyScore; 24] = std::array::from_fn(|i| {
        let source = (i + 6) % 24; // upstream starts at A, public diagnostics at C
        KeyScore {
            key: map_upstream(source as i32).unwrap(),
            score: upstream[source],
        }
    });
    // Ties must retain the classifier's upstream A-major-first preference,
    // even though the public score array is deliberately C-major-first.
    let mut ranked: [KeyScore; 24] = std::array::from_fn(|i| KeyScore {
        key: map_upstream(i as i32).unwrap(),
        score: upstream[i],
    });
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(KeyDiagnostics {
        score_kind: "cosine_similarity",
        scores,
        best: ranked[0],
        runner_up: ranked[1],
        margin: ranked[0].score - ranked[1].score,
    })
}

pub(super) fn rms(samples: impl Iterator<Item = f64>, count: usize) -> f64 {
    if count == 0 {
        return 0.0;
    }
    (samples.map(|sample| sample * sample).sum::<f64>() / count as f64).sqrt()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mix {
    Average,
    Left,
    Right,
}

impl Mix {
    pub fn sample(self, frame: &[f32]) -> f64 {
        match self {
            Self::Average => (frame[0] as f64 + frame[1] as f64) * 0.5,
            Self::Left => frame[0] as f64,
            Self::Right => frame[1] as f64,
        }
    }
}

pub(super) struct MonoProfile {
    pub mix: Mix,
    pub gain: f64,
    pub clips: bool,
}

pub(super) fn mono_profile(samples: &[f32]) -> Option<MonoProfile> {
    let frames = samples.len() / 2;
    let left_rms = rms(samples.chunks_exact(2).map(|frame| frame[0] as f64), frames);
    let right_rms = rms(samples.chunks_exact(2).map(|frame| frame[1] as f64), frames);
    let average_rms = rms(
        samples
            .chunks_exact(2)
            .map(|frame| (frame[0] as f64 + frame[1] as f64) * 0.5),
        frames,
    );
    let strongest = left_rms.max(right_rms);
    let choose_channel = strongest > 0.0 && average_rms < strongest * 0.25;
    let mix = if choose_channel {
        if right_rms > left_rms {
            Mix::Right
        } else {
            Mix::Left
        }
    } else {
        Mix::Average
    };
    let level = rms(
        samples.chunks_exact(2).map(|frame| mix.sample(frame)),
        frames,
    );
    if level <= NOISE_FLOOR_RMS {
        return None;
    }
    let gain = (TARGET_RMS / level).clamp(1.0, MAX_GAIN);
    let clips = samples
        .chunks_exact(2)
        .any(|frame| mix.sample(frame).abs() * gain > 1.0);
    Some(MonoProfile { mix, gain, clips })
}

/// Same checks with an explicit minimum length (clamped to SHORTEST..=MAX).
pub(super) fn validate_input_min(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    min_seconds: usize,
) -> Result<(), String> {
    let min_seconds = min_seconds.clamp(SHORTEST_SECONDS, MAX_SECONDS);
    if sample_rate != REQUIRED_SAMPLE_RATE {
        return Err(format!("sample rate must be {REQUIRED_SAMPLE_RATE} Hz"));
    }
    if channels != REQUIRED_CHANNELS {
        return Err(format!("channel count must be {REQUIRED_CHANNELS}"));
    }
    if samples.len() % channels as usize != 0 {
        return Err("interleaved sample count is not frame-aligned".into());
    }
    let frames = samples.len() / channels as usize;
    let minimum = sample_rate as usize * min_seconds;
    let maximum = sample_rate as usize * MAX_SECONDS;
    if !(minimum..=maximum).contains(&frames) {
        return Err(format!(
            "analysis input must contain {min_seconds} to {MAX_SECONDS} seconds"
        ));
    }
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err("analysis input contains a non-finite sample".into());
    }
    Ok(())
}

pub fn detect(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<Option<MusicalKey>, String> {
    detect_inner(samples, sample_rate, channels, false, MIN_SECONDS).map(|r| r.candidate)
}

pub fn detect_with_diagnostics(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<DiagnosticAnalysis, String> {
    detect_inner(samples, sample_rate, channels, true, MIN_SECONDS)
}

pub(super) fn detect_with_diagnostics_min(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    min_seconds: usize,
) -> Result<DiagnosticAnalysis, String> {
    detect_inner(samples, sample_rate, channels, true, min_seconds)
}

fn detect_inner(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    requested: bool,
    min_seconds: usize,
) -> Result<DiagnosticAnalysis, String> {
    validate_input_min(samples, sample_rate, channels, min_seconds)?;
    let Some(profile) = mono_profile(samples) else {
        return Ok(DiagnosticAnalysis {
            candidate: None,
            diagnostics: None,
            evidence: None,
        });
    };
    let level = rms(
        samples.chunks_exact(2).map(|frame| profile.mix.sample(frame)),
        samples.len() / 2,
    );
    let mono: Vec<f64> = samples
        .chunks_exact(2)
        .map(|frame| (profile.mix.sample(frame) * profile.gain).clamp(-1.0, 1.0))
        .collect();

    #[cfg(windows)]
    {
        // libkeyfinder constructs and destroys FFTW plans during this call. FFTW's
        // planner is not assumed thread-safe, so the lock spans the full native lifetime.
        let _native_guard = NATIVE_ANALYSIS
            .lock()
            .map_err(|_| "native keyfinder lock poisoned")?;
        let mut upstream = 24_i32;
        let mut scores = [0.0; 24];
        let mut chroma = [0.0; 12];
        let mut hops = [0_u32; 2];
        let mut error = [0_i8; 512];
        let status = unsafe {
            keyfinder_detect_mono(
                mono.as_ptr(),
                mono.len(),
                sample_rate,
                &mut upstream,
                if requested {
                    scores.as_mut_ptr()
                } else {
                    std::ptr::null_mut()
                },
                chroma.as_mut_ptr(),
                hops.as_mut_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            let message = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
            return Err(format!("libkeyfinder failed: {message}"));
        }
        let candidate = map_upstream(upstream);
        let evidence = (hops[0] > 0).then(|| ChromaEvidence {
            chroma,
            seconds: hops[0] as f64 * hops[1] as f64 / sample_rate as f64,
            rms: level,
        });
        return Ok(DiagnosticAnalysis {
            candidate,
            diagnostics: if requested && candidate.is_some() {
                Some(diagnostics(scores)?)
            } else {
                None
            },
            evidence,
        });
    }

    #[cfg(not(windows))]
    {
        let _ = (mono, requested, level);
        Err("libkeyfinder native adapter is currently available only on Windows".into())
    }
}
