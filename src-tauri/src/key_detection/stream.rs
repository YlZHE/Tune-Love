//! Bounded, cursor-aware adapter around upstream libkeyfinder's progressive API.
//! The native context is owned by one worker; no pointer crosses a thread boundary.
use super::{engine, ChromaEvidence, KeyDiagnostics, MusicalKey};
use crate::audio::PcmWindow;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::NonNull;

#[cfg(windows)]
extern "C" {
    fn keyfinder_stream_create(error: *mut c_char, capacity: usize) -> *mut c_void;
    fn keyfinder_stream_destroy(context: *mut c_void);
    fn keyfinder_stream_feed(
        context: *mut c_void,
        samples: *const f64,
        count: usize,
        reset: c_int,
        gain: f64,
        key: *mut c_int,
        stats: *mut u32,
        scores: *mut f64,
        chroma: *mut f64,
        error: *mut c_char,
        capacity: usize,
    ) -> c_int;
}

#[derive(Default, Debug, serde::Serialize)]
pub struct StreamAnalysis {
    pub candidate: Option<MusicalKey>,
    pub fed_frames: usize,
    pub retained_hops: u32,
    pub buffered_samples: u32,
    pub reset: bool,
    pub duplicate: bool,
    pub batch_fallback: bool,
    /// Pitch-class evidence for the audio newly covered by this step, if any.
    pub evidence: Option<ChromaEvidence>,
}

#[derive(PartialEq, Eq)]
struct Cursor {
    source: String,
    track: String,
    target: u64,
    capture: u64,
    end: u64,
}

impl Cursor {
    fn same_epoch(&self, other: &Self) -> bool {
        self.source == other.source
            && self.track == other.track
            && self.target == other.target
            && self.capture == other.capture
    }
}

#[derive(Default)]
pub struct RollingDetector {
    native: Option<NonNull<c_void>>,
    cursor: Option<Cursor>,
    mix: Option<engine::Mix>,
    /// Shortest accepted window in seconds; None keeps the 6 s default.
    min_seconds: Option<usize>,
}

impl RollingDetector {
    /// Accept windows from `seconds` (clamped to 3..=8) instead of the 6 s default.
    pub fn with_min_seconds(seconds: usize) -> Self {
        Self {
            native: None,
            cursor: None,
            mix: None,
            min_seconds: Some(seconds),
        }
    }

    fn min_seconds(&self) -> usize {
        self.min_seconds.unwrap_or(engine::MIN_SECONDS)
    }

    /// Forget continuity, while retaining the reusable native factories/FFT plans.
    pub fn reset(&mut self) {
        self.cursor = None;
        self.mix = None;
    }

    pub fn analyze(&mut self, window: &PcmWindow) -> Result<StreamAnalysis, String> {
        self.analyze_requested(window, false).map(|r| r.0)
    }

    pub fn analyze_with_diagnostics(
        &mut self,
        window: &PcmWindow,
    ) -> Result<(StreamAnalysis, Option<KeyDiagnostics>), String> {
        self.analyze_requested(window, true)
    }

    fn analyze_requested(
        &mut self,
        window: &PcmWindow,
        requested: bool,
    ) -> Result<(StreamAnalysis, Option<KeyDiagnostics>), String> {
        let result = self.analyze_inner(window, requested);
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn analyze_inner(
        &mut self,
        window: &PcmWindow,
        requested: bool,
    ) -> Result<(StreamAnalysis, Option<KeyDiagnostics>), String> {
        engine::validate_input_min(
            &window.samples,
            window.sample_rate,
            window.channels,
            self.min_seconds(),
        )?;
        let frames = window.samples.len() / 2;
        let start = window
            .sample_end_sequence
            .checked_sub(frames as u64)
            .ok_or("analysis cursor is shorter than its PCM window")?;
        let current = Cursor {
            source: window.source_id.clone(),
            track: window.track_key.clone(),
            target: window.target_generation,
            capture: window.capture_generation,
            end: window.sample_end_sequence,
        };
        if self.cursor.as_ref() == Some(&current) {
            return Ok((
                StreamAnalysis {
                    duplicate: true,
                    ..Default::default()
                },
                None,
            ));
        }
        let Some(profile) = engine::mono_profile(&window.samples) else {
            // Silence must never reclassify a previously retained chromagram.
            self.mix = None;
            self.cursor = Some(current);
            return Ok((StreamAnalysis::default(), None));
        };
        if profile.clips {
            // Clipping is nonlinear: scaled chroma is not equivalent. Preserve the
            // existing normalization/clamp behavior for this uncommon window.
            self.mix = None;
            let result = engine::detect_with_diagnostics_min(
                &window.samples,
                window.sample_rate,
                window.channels,
                self.min_seconds(),
            )?;
            self.cursor = Some(current);
            return Ok((
                StreamAnalysis {
                    candidate: result.candidate,
                    fed_frames: frames,
                    batch_fallback: true,
                    reset: true,
                    evidence: result.evidence,
                    ..Default::default()
                },
                if requested { result.diagnostics } else { None },
            ));
        }
        let offset = self
            .cursor
            .as_ref()
            .filter(|last| {
                last.same_epoch(&current)
                    && last.end >= start
                    && last.end < current.end
                    && self.mix == Some(profile.mix)
            })
            .map(|last| (last.end - start) as usize);
        let reset = offset.is_none();
        let samples: Vec<f64> = window.samples[offset.unwrap_or(0) * 2..]
            .chunks_exact(2)
            .map(|frame| profile.mix.sample(frame))
            .collect();
        // Feed unscaled samples consistently. Uniform window gain is applied to
        // the chroma at classification, never independently to successive chunks.
        let (candidate, stats, diagnostics, chroma) =
            self.feed(&samples, reset, profile.gain, requested)?;
        self.cursor = Some(current);
        self.mix = Some(profile.mix);
        let evidence = (stats[2] > 0).then(|| ChromaEvidence {
            chroma,
            seconds: stats[2] as f64 * stats[3] as f64 / window.sample_rate as f64,
            rms: engine::rms(samples.iter().copied(), samples.len()),
        });
        Ok((
            StreamAnalysis {
                candidate,
                fed_frames: samples.len(),
                retained_hops: stats[0],
                buffered_samples: stats[1],
                reset,
                evidence,
                ..Default::default()
            },
            diagnostics,
        ))
    }

    #[cfg(windows)]
    fn feed(
        &mut self,
        samples: &[f64],
        reset: bool,
        gain: f64,
        requested: bool,
    ) -> Result<
        (
            Option<MusicalKey>,
            [u32; 4],
            Option<KeyDiagnostics>,
            [f64; 12],
        ),
        String,
    > {
        // Serialize FFTW planning, execution and destruction, including baseline calls.
        let _guard = engine::NATIVE_ANALYSIS
            .lock()
            .map_err(|_| "native keyfinder lock poisoned")?;
        let mut error = [0_i8; 512];
        let ptr = match self.native {
            Some(ptr) => ptr,
            None => {
                let ptr = NonNull::new(unsafe {
                    keyfinder_stream_create(error.as_mut_ptr(), error.len())
                })
                .ok_or_else(|| native_error(&error))?;
                self.native = Some(ptr);
                ptr
            }
        };
        let mut key = 24;
        let mut stats = [0_u32; 4];
        let mut scores = [0.0; 24];
        let mut chroma = [0.0; 12];
        let status = unsafe {
            keyfinder_stream_feed(
                ptr.as_ptr(),
                samples.as_ptr(),
                samples.len(),
                reset.into(),
                gain,
                &mut key,
                stats.as_mut_ptr(),
                if requested {
                    scores.as_mut_ptr()
                } else {
                    std::ptr::null_mut()
                },
                chroma.as_mut_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            return Err(native_error(&error));
        }
        let candidate = engine::map_upstream(key);
        let diagnostics = if requested && candidate.is_some() {
            Some(engine::diagnostics(scores)?)
        } else {
            None
        };
        Ok((candidate, stats, diagnostics, chroma))
    }

    #[cfg(not(windows))]
    fn feed(
        &mut self,
        _: &[f64],
        _: bool,
        _: f64,
        _: bool,
    ) -> Result<
        (
            Option<MusicalKey>,
            [u32; 4],
            Option<KeyDiagnostics>,
            [f64; 12],
        ),
        String,
    > {
        Err("libkeyfinder native adapter is currently available only on Windows".into())
    }
}

#[cfg(windows)]
fn native_error(error: &[i8; 512]) -> String {
    format!(
        "libkeyfinder stream failed: {}",
        unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
    )
}

impl Drop for RollingDetector {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(ptr) = self.native.take() {
            let _guard = engine::NATIVE_ANALYSIS
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            unsafe {
                keyfinder_stream_destroy(ptr.as_ptr());
            }
        }
    }
}
