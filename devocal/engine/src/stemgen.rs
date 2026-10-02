//! StemgenRT streaming separator on ONNX Runtime (CPU).
//!
//! Model facts (verified against `artifacts/separation-bench/src/quality.py::stemgenrt`):
//! input `audio_chunk` `[1, 2, 128]` (channel-major) plus the recurrent state tensors;
//! output 0 is the stems `[1, 4, 2, 128]`, outputs 1.. are the next states in the same
//! order as the state inputs. Output k belongs to input block k-1 (one hop of latency);
//! vocals are stem 2 and accompaniment = previous input block - vocals.

use std::ffi::{c_char, CStr, CString};
use std::path::Path;
use std::ptr;

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::sys::{OrtApi, OrtValue};
use ort::value::{Tensor, TensorElementType, ValueType};
use ort::AsPointer;

use crate::separator::{check_block, Separator};

pub const STEMGEN_SAMPLE_RATE: u32 = 44_100;
pub const STEMGEN_HOP: usize = 128;
const CHANNELS: usize = 2;
const STEMS: usize = 4;
const VOCALS: usize = 2;
const AUDIO_INPUT: &str = "audio_chunk";
const WARMUP_RUNS: usize = 20;
/// Upper bound on model inputs/outputs, so per-block name/value pointer arrays live on the stack.
const MAX_IO: usize = 16;

/// StemgenRT streaming separator. Inputs and outputs are bound to tensors allocated once at
/// load: the recurrent state lives in two banks, the model reads bank `cur` and writes the
/// next state straight into the other bank, and the banks swap after each block. `process`
/// makes one raw `OrtApi::Run` call with stack-built pointer arrays, so the wrapper does no
/// heap allocation per block.
pub struct StemgenRt {
    api: &'static OrtApi,
    session: Session,
    /// `[1, 2, HOP]`, channel-major copy of the current input block.
    audio: Tensor<f32>,
    /// `[1, STEMS, 2, HOP]` model output, overwritten every block.
    stems: Tensor<f32>,
    /// Two banks of state tensors, each in model state-input order.
    banks: [Vec<Tensor<f32>>; 2],
    /// Bank holding the current state (read by the next run).
    cur: usize,
    /// `audio_chunk` first, then the state inputs in model order.
    input_names: Vec<CString>,
    /// Stems first, then the next-state outputs (same order as the state inputs).
    output_names: Vec<CString>,
    /// Previous input block, interleaved (zeros before the first block).
    prev: Vec<f32>,
}

fn fixed_f32_shape(name: &str, dtype: &ValueType) -> Result<Vec<i64>, String> {
    match dtype {
        ValueType::Tensor { ty, shape, .. } if *ty == TensorElementType::Float32 => {
            if shape.iter().any(|&d| d <= 0) {
                return Err(format!(
                    "{name}: dynamic shape {:?} not supported",
                    &shape[..]
                ));
            }
            Ok(shape.to_vec())
        }
        other => Err(format!("{name}: expected an f32 tensor, found {other}")),
    }
}

fn zeros(shape: &[i64]) -> Result<Tensor<f32>, String> {
    let len = shape.iter().product::<i64>() as usize;
    Tensor::from_array((shape.to_vec(), vec![0.0f32; len])).map_err(|e| e.to_string())
}

fn cstring(name: &str) -> Result<CString, String> {
    CString::new(name).map_err(|_| format!("model io name contains NUL: {name:?}"))
}

impl StemgenRt {
    /// Loads the model on the CPU with `threads` intra-op threads (at least 1) and one
    /// inter-op thread, validates its signature and warms it up with 20 runs.
    pub fn load(path: &Path, threads: u16) -> Result<Self, String> {
        let err = |stage: &'static str| move |e: ort::Error<_>| format!("{stage}: {e}");
        let session = Session::builder()
            .map_err(|e| format!("session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(err("optimization level"))?
            .with_intra_threads(usize::from(threads.max(1)))
            .map_err(err("intra-op threads"))?
            .with_inter_threads(1)
            .map_err(err("inter-op threads"))?
            // Idle pool threads must not busy-wait between blocks (CPU budget).
            .with_intra_op_spinning(false)
            .map_err(err("intra-op spinning"))?
            .with_inter_op_spinning(false)
            .map_err(err("inter-op spinning"))?
            .commit_from_file(path)
            .map_err(|e| format!("load {}: {e}", path.display()))?;

        let inputs = session.inputs();
        let outputs = session.outputs();
        if inputs.len() > MAX_IO || outputs.len() != inputs.len() {
            return Err(format!(
                "unexpected model signature: {} inputs, {} outputs",
                inputs.len(),
                outputs.len()
            ));
        }
        let audio = inputs
            .iter()
            .find(|i| i.name() == AUDIO_INPUT)
            .ok_or_else(|| format!("model has no `{AUDIO_INPUT}` input"))?;
        let audio_shape = vec![1, CHANNELS as i64, STEMGEN_HOP as i64];
        if fixed_f32_shape(AUDIO_INPUT, audio.dtype())? != audio_shape {
            return Err(format!("`{AUDIO_INPUT}` shape is not {audio_shape:?}"));
        }
        let stems_shape = vec![1, STEMS as i64, CHANNELS as i64, STEMGEN_HOP as i64];
        if fixed_f32_shape(outputs[0].name(), outputs[0].dtype())? != stems_shape {
            return Err(format!("output 0 shape is not {stems_shape:?}"));
        }

        let mut input_names = vec![cstring(AUDIO_INPUT)?];
        let mut output_names = vec![cstring(outputs[0].name())?];
        let mut state_shapes = Vec::new();
        let states = inputs.iter().filter(|i| i.name() != AUDIO_INPUT);
        for (state, next) in states.zip(&outputs[1..]) {
            let shape = fixed_f32_shape(state.name(), state.dtype())?;
            if fixed_f32_shape(next.name(), next.dtype())? != shape {
                return Err(format!(
                    "state output `{}` does not match input `{}`",
                    next.name(),
                    state.name()
                ));
            }
            input_names.push(cstring(state.name())?);
            output_names.push(cstring(next.name())?);
            state_shapes.push(shape);
        }

        let bank = || -> Result<Vec<Tensor<f32>>, String> {
            state_shapes.iter().map(|s| zeros(s)).collect()
        };
        let banks = [bank()?, bank()?];
        let mut sep = Self {
            api: ort::api(),
            session,
            audio: zeros(&audio_shape)?,
            stems: zeros(&stems_shape)?,
            banks,
            cur: 0,
            input_names,
            output_names,
            prev: vec![0.0; STEMGEN_HOP * CHANNELS],
        };

        let silence = vec![0.0f32; STEMGEN_HOP * CHANNELS];
        let mut out = vec![0.0f32; STEMGEN_HOP * CHANNELS];
        for _ in 0..WARMUP_RUNS {
            sep.process(&silence, &mut out)
                .map_err(|e| format!("warm-up: {e}"))?;
        }
        sep.reset();
        Ok(sep)
    }

    /// One inference: reads `audio` and bank `cur`, writes `stems` and bank `1 - cur`.
    fn run(&mut self) -> Result<(), String> {
        let n = self.input_names.len();
        let next = 1 - self.cur;
        let mut in_names = [ptr::null::<c_char>(); MAX_IO];
        let mut out_names = [ptr::null::<c_char>(); MAX_IO];
        let mut in_vals = [ptr::null::<OrtValue>(); MAX_IO];
        let mut out_vals = [ptr::null_mut::<OrtValue>(); MAX_IO];
        for (slot, name) in in_names.iter_mut().zip(&self.input_names) {
            *slot = name.as_ptr();
        }
        for (slot, name) in out_names.iter_mut().zip(&self.output_names) {
            *slot = name.as_ptr();
        }
        in_vals[0] = self.audio.ptr();
        out_vals[0] = self.stems.ptr_mut();
        for (k, state) in self.banks[self.cur].iter().enumerate() {
            in_vals[k + 1] = state.ptr();
        }
        for (k, state) in self.banks[next].iter_mut().enumerate() {
            out_vals[k + 1] = state.ptr_mut();
        }
        // SAFETY: every pointer refers to a live CString or OrtValue owned by `self`, which is
        // borrowed mutably for the call. Inputs (bank `cur`, `audio`) and preallocated outputs
        // (bank `next`, `stems`) are distinct CPU tensors whose shapes and types were checked
        // against the model at load, so ONNX Runtime writes the results in place.
        let status = unsafe {
            (self.api.Run)(
                self.session.ptr_mut(),
                ptr::null(),
                in_names.as_ptr(),
                in_vals.as_ptr(),
                n,
                out_names.as_ptr(),
                n,
                out_vals.as_mut_ptr(),
            )
        };
        if status.0.is_null() {
            return Ok(());
        }
        // SAFETY: a non-null status is owned by us; the message is valid until it is released.
        unsafe {
            let msg = CStr::from_ptr((self.api.GetErrorMessage)(status.0))
                .to_string_lossy()
                .into_owned();
            (self.api.ReleaseStatus)(status.0);
            Err(format!("inference: {msg}"))
        }
    }
}

impl Separator for StemgenRt {
    fn sample_rate(&self) -> u32 {
        STEMGEN_SAMPLE_RATE
    }

    fn hop(&self) -> usize {
        STEMGEN_HOP
    }

    fn latency_frames(&self) -> usize {
        STEMGEN_HOP
    }

    fn process(&mut self, input: &[f32], out_accompaniment: &mut [f32]) -> Result<(), String> {
        check_block(STEMGEN_HOP, input, out_accompaniment)?;
        // A non-finite sample would poison the recurrent state; reject the block untouched.
        if !input.iter().all(|s| s.is_finite()) {
            return Err("non-finite input sample".into());
        }
        {
            let (_, audio) = self
                .audio
                .try_extract_tensor_mut::<f32>()
                .map_err(|e| e.to_string())?;
            let (left, right) = audio.split_at_mut(STEMGEN_HOP);
            for ((frame, l), r) in input.chunks_exact(2).zip(left).zip(right) {
                *l = frame[0];
                *r = frame[1];
            }
        }
        self.run()?;
        let (_, stems) = self
            .stems
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        let vocals = &stems[VOCALS * CHANNELS * STEMGEN_HOP..(VOCALS + 1) * CHANNELS * STEMGEN_HOP];
        let (v_left, v_right) = vocals.split_at(STEMGEN_HOP);
        let frames = out_accompaniment
            .chunks_exact_mut(2)
            .zip(self.prev.chunks_exact(2))
            .zip(v_left.iter().zip(v_right));
        for ((out, prev), (vl, vr)) in frames {
            out[0] = prev[0] - vl;
            out[1] = prev[1] - vr;
        }
        if !out_accompaniment.iter().all(|s| s.is_finite()) {
            return Err("model produced a non-finite sample".into());
        }
        // Commit only after a good block: the state written by this run becomes current.
        self.cur = 1 - self.cur;
        self.prev.copy_from_slice(input);
        Ok(())
    }

    fn reset(&mut self) {
        for bank in &mut self.banks {
            for state in bank.iter_mut() {
                if let Ok((_, data)) = state.try_extract_tensor_mut::<f32>() {
                    data.fill(0.0);
                }
            }
        }
        self.cur = 0;
        self.prev.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn model_path() -> PathBuf {
        PathBuf::from(
            std::env::var_os("STEMGENRT_ONNX")
                .expect("set STEMGENRT_ONNX to the absolute path of hop128.onnx"),
        )
    }

    /// Reads a little-endian f32 fixture produced by tools/stemgen_reference.py.
    fn fixture(name: &str) -> Vec<f32> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (generate with tools/stemgen_reference.py)",
                path.display()
            )
        });
        assert_eq!(bytes.len() % 4, 0);
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }

    fn run_blocks(sep: &mut StemgenRt, input: &[f32]) -> Vec<f32> {
        let n = sep.hop() * 2;
        let mut out = vec![0.0f32; input.len()];
        for (i, o) in input.chunks_exact(n).zip(out.chunks_exact_mut(n)) {
            sep.process(i, o).unwrap();
        }
        out
    }

    #[test]
    fn stemgen_load_missing_file_is_err() {
        assert!(StemgenRt::load(Path::new("definitely-missing-model.onnx"), 1).is_err());
    }

    #[test]
    #[ignore = "needs STEMGENRT_ONNX and generated fixtures"]
    fn stemgen_matches_python_reference() {
        let mut sep = StemgenRt::load(&model_path(), 1).unwrap();
        assert_eq!(sep.sample_rate(), 44_100);
        assert_eq!(sep.hop(), 128);
        assert_eq!(sep.latency_frames(), 128);
        let input = fixture("stemgen-input.f32");
        let expected = fixture("stemgen-accompaniment.f32");
        assert_eq!(input.len(), expected.len());
        assert_eq!(input.len() % 256, 0);
        let out = run_blocks(&mut sep, &input);
        let max_diff = out
            .iter()
            .zip(&expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        println!(
            "max |rust - python| = {max_diff:e} over {} samples",
            out.len()
        );
        assert!(max_diff <= 1e-4, "max diff {max_diff}");
    }

    #[test]
    #[ignore = "needs STEMGENRT_ONNX and generated fixtures"]
    fn stemgen_reset_zeroes_state() {
        let mut sep = StemgenRt::load(&model_path(), 1).unwrap();
        let input = fixture("stemgen-input.f32");
        let input = &input[..256 * 200];
        let first = run_blocks(&mut sep, input);
        sep.reset();
        let second = run_blocks(&mut sep, input);
        assert_eq!(first, second);
    }

    #[test]
    #[ignore = "needs STEMGENRT_ONNX"]
    fn stemgen_rejects_bad_blocks_without_panicking() {
        let mut sep = StemgenRt::load(&model_path(), 1).unwrap();
        let mut out = vec![0.0f32; 256];
        assert!(sep.process(&[0.0; 255], &mut out).is_err());
        assert!(sep.process(&[0.0; 256], &mut [0.0; 128]).is_err());
        let mut bad = vec![0.0f32; 256];
        bad[7] = f32::NAN;
        assert!(sep.process(&bad, &mut out).is_err());
        // A rejected block leaves the stream usable and unchanged.
        let input = fixture("stemgen-input.f32");
        let input = &input[..256 * 50];
        let after_errors = run_blocks(&mut sep, input);
        sep.reset();
        assert_eq!(after_errors, run_blocks(&mut sep, input));
    }

    /// Prints the median per-block `process` time with one thread (budget 128/44100 s ≈ 2.9 ms).
    #[test]
    #[ignore = "needs STEMGENRT_ONNX; timing report"]
    fn stemgen_block_timing() {
        use std::time::Instant;
        let mut sep = StemgenRt::load(&model_path(), 1).unwrap();
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        let mut out = vec![0.0f32; 256];
        let mut times: Vec<f64> = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let t = Instant::now();
            sep.process(&input, &mut out).unwrap();
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "stemgen per-block process (1 thread, 1000 blocks): median {:.3} ms, p95 {:.3} ms, max {:.3} ms (budget 2.902 ms)",
            times[500], times[950], times[999]
        );
    }

    #[test]
    #[ignore = "needs STEMGENRT_ONNX"]
    fn stemgen_process_does_not_allocate() {
        let mut sep = StemgenRt::load(&model_path(), 1).unwrap();
        let input = vec![0.1f32; 256];
        let mut out = vec![0.0f32; 256];
        sep.process(&input, &mut out).unwrap();
        let before = alloc_count::this_thread();
        for _ in 0..50 {
            sep.process(&input, &mut out).unwrap();
        }
        assert_eq!(
            alloc_count::this_thread() - before,
            0,
            "Rust heap allocations in process()"
        );
        // The counter does see allocations on this thread.
        let before = alloc_count::this_thread();
        drop(std::hint::black_box(vec![1u8; 16]));
        assert!(alloc_count::this_thread() > before);
    }
}

/// Counts Rust heap allocations per thread in the test binary (ONNX Runtime's own C++
/// allocations are not seen; this guards our wrapper code).
#[cfg(test)]
mod alloc_count {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
            System.alloc(layout)
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
            System.alloc_zeroed(layout)
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
            System.realloc(ptr, layout, new_size)
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            System.dealloc(ptr, layout)
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    pub fn this_thread() -> u64 {
        COUNT.with(|c| c.get())
    }
}
