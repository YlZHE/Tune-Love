//! ONNX Runtime window model (bytesep, HTDemucs) for [`crate::windowed::WindowedSeparator`],
//! on the CPU or on DirectML, plus the device decision of spec 4.4.
//!
//! Model facts: one fixed-shape f32 input `[1, 2, W]` (planar stereo, 44.1 kHz) and output 0
//! either `[1, 2, W]` (the vocals; `vocals_index` 0) or `[1, S, 2, W]` (S stems, the vocals
//! at `vocals_index`). The worker hands over interleaved windows, converted here.

use std::path::Path;

use devocal_core::protocol::{Device, ErrorCode, WindowedSpec};
use ort::ep;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

use crate::dsp::SAMPLE_RATE;
use crate::stemgen::fixed_f32_shape;
use crate::windowed::{WindowModel, WindowedParams, XF};

const CHANNELS: usize = 2;

pub struct OrtWindowModel {
    session: Session,
    input_name: String,
    window: usize,
    /// Output stems (1 for a plain `[1, 2, W]` output).
    stems: usize,
    vocals_index: usize,
}

impl OrtWindowModel {
    /// Loads the model and warms it up with one all-zero window. The GPU session uses
    /// DirectML on the default adapter and fails (never falls back to the CPU) when it cannot
    /// be registered; it always uses one intra-op thread (`threads` only applies to the CPU).
    pub fn load(
        path: &Path,
        device: Device,
        threads: u16,
        vocals_index: u32,
    ) -> Result<Self, String> {
        let err = |stage: &'static str| move |e: ort::Error<_>| format!("{stage}: {e}");
        let mut builder = Session::builder()
            .map_err(|e| format!("session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(err("optimization level"))?
            .with_inter_threads(1)
            .map_err(err("inter-op threads"))?
            // Idle pool threads must not busy-wait between windows (CPU budget).
            .with_intra_op_spinning(false)
            .map_err(err("intra-op spinning"))?
            .with_inter_op_spinning(false)
            .map_err(err("inter-op spinning"))?;
        builder = match device {
            Device::Cpu => builder
                .with_intra_threads(usize::from(threads.max(1)))
                .map_err(err("intra-op threads"))?,
            Device::Gpu => builder
                .with_intra_threads(1)
                .map_err(err("intra-op threads"))?
                .with_memory_pattern(false)
                .map_err(err("memory pattern"))?
                .with_parallel_execution(false)
                .map_err(err("parallel execution"))?
                .with_execution_providers([ep::DirectML::default().build().error_on_failure()])
                .map_err(err("DirectML"))?,
        };
        let session = builder
            .commit_from_file(path)
            .map_err(|e| format!("load {}: {e}", path.display()))?;

        let (inputs, outputs) = (session.inputs(), session.outputs());
        if inputs.len() != 1 || outputs.is_empty() {
            return Err(format!(
                "unexpected model signature: {} inputs, {} outputs",
                inputs.len(),
                outputs.len()
            ));
        }
        let input_name = inputs[0].name().to_string();
        let in_shape = fixed_f32_shape(&input_name, inputs[0].dtype())?;
        let window = match in_shape[..] {
            [1, 2, w] => w as usize,
            _ => return Err(format!("input shape {in_shape:?} is not [1, 2, W]")),
        };
        let out_shape = fixed_f32_shape(outputs[0].name(), outputs[0].dtype())?;
        let stems = match out_shape[..] {
            [1, 2, w] if w as usize == window => 1,
            [1, s, 2, w] if w as usize == window => s as usize,
            _ => {
                return Err(format!(
                    "output shape {out_shape:?} does not match input {in_shape:?}"
                ))
            }
        };
        let vocals_index = vocals_index as usize;
        if vocals_index >= stems {
            return Err(format!(
                "vocals index {vocals_index} but the model has {stems} stem(s)"
            ));
        }
        let mut model = Self {
            session,
            input_name,
            window,
            stems,
            vocals_index,
        };
        let silence = vec![0.0f32; window * CHANNELS];
        model.run(&silence, &mut vec![0.0f32; window * CHANNELS])?;
        Ok(model)
    }

    pub fn window_frames(&self) -> usize {
        self.window
    }
}

impl WindowModel for OrtWindowModel {
    fn run(&mut self, input: &[f32], vocals: &mut [f32]) -> Result<(), String> {
        let w = self.window;
        if input.len() != w * CHANNELS || vocals.len() != w * CHANNELS {
            return Err(format!(
                "window length mismatch: input {} / output {} samples, expected {}",
                input.len(),
                vocals.len(),
                w * CHANNELS
            ));
        }
        let mut planar = vec![0.0f32; w * CHANNELS];
        for (i, f) in input.as_chunks::<CHANNELS>().0.iter().enumerate() {
            planar[i] = f[0];
            planar[w + i] = f[1];
        }
        let tensor = Tensor::from_array(([1usize, CHANNELS, w], planar))
            .map_err(|e| format!("input tensor: {e}"))?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .map_err(|e| format!("run: {e}"))?;
        let (_, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("output tensor: {e}"))?;
        if data.len() != self.stems * CHANNELS * w {
            return Err(format!("output has {} samples", data.len()));
        }
        let stem = &data[self.vocals_index * CHANNELS * w..][..CHANNELS * w];
        for (i, f) in vocals.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            f[0] = stem[i];
            f[1] = stem[w + i];
        }
        Ok(())
    }
}

fn ms_to_frames(ms: u32) -> usize {
    (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize
}

/// Window geometry for the device's hop. Errors (instead of the panic in
/// `WindowedSeparator::new`) on a manifest the separator cannot run.
pub fn window_params(w: &WindowedSpec, device: Device) -> Result<WindowedParams, String> {
    let hop_ms = match device {
        Device::Gpu => w.gpu_hop_ms,
        Device::Cpu => w.cpu_hop_ms,
    }
    .ok_or_else(|| format!("the model has no hop for {device:?}"))?;
    let p = WindowedParams {
        window_frames: ms_to_frames(w.window_ms),
        hop_frames: ms_to_frames(hop_ms),
        lookahead_frames: ms_to_frames(w.lookahead_ms),
    };
    if p.window_frames < p.hop_frames + p.lookahead_frames
        || p.hop_frames < XF
        || p.lookahead_frames < XF
    {
        return Err(format!("unusable window geometry {p:?}"));
    }
    Ok(p)
}

/// Spec 4.4. `gpu_ok` (a DirectML session could be created) is asked only when the GPU is a
/// candidate. The explicit `gpu` request never falls back to the CPU; `auto` does, unless
/// the model has no CPU hop (`GpuRequired`).
pub fn resolve_device(
    requested: &str,
    spec: &Option<WindowedSpec>,
    gpu_ok: impl FnOnce() -> bool,
) -> Result<Device, ErrorCode> {
    if !matches!(requested, "auto" | "cpu" | "gpu") {
        return Err(ErrorCode::BadDevice);
    }
    // StemgenRT has no GPU path.
    let Some(w) = spec else {
        return Ok(Device::Cpu);
    };
    let (gpu_hop, cpu_hop) = (w.gpu_hop_ms.is_some(), w.cpu_hop_ms.is_some());
    if requested == "cpu" {
        return if cpu_hop {
            Ok(Device::Cpu)
        } else {
            Err(ErrorCode::GpuRequired)
        };
    }
    if gpu_hop && gpu_ok() {
        Ok(Device::Gpu)
    } else if requested == "gpu" {
        Err(ErrorCode::ModelLoadFailed)
    } else if cpu_hop {
        Ok(Device::Cpu)
    } else {
        Err(ErrorCode::GpuRequired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(gpu_hop_ms: Option<u32>, cpu_hop_ms: Option<u32>) -> Option<WindowedSpec> {
        Some(WindowedSpec {
            window_ms: 1000,
            lookahead_ms: 100,
            gpu_hop_ms,
            cpu_hop_ms,
            cpu_threads: 2,
            vocals_index: 0,
        })
    }

    fn bytesep() -> Option<WindowedSpec> {
        spec(Some(300), Some(300))
    }

    /// GPU only, like HTDemucs.
    fn htdemucs() -> Option<WindowedSpec> {
        spec(Some(300), None)
    }

    fn never() -> bool {
        panic!("gpu_ok must not be asked here")
    }

    #[test]
    fn stemgenrt_is_always_cpu_and_never_probes_the_gpu() {
        for req in ["auto", "cpu", "gpu"] {
            assert_eq!(resolve_device(req, &None, never), Ok(Device::Cpu), "{req}");
        }
    }

    #[test]
    fn bytesep_table() {
        use Device::*;
        let auto = |ok| resolve_device("auto", &bytesep(), move || ok);
        assert_eq!(auto(true), Ok(Gpu));
        assert_eq!(auto(false), Ok(Cpu));
        assert_eq!(resolve_device("cpu", &bytesep(), never), Ok(Cpu));
        let gpu = |ok| resolve_device("gpu", &bytesep(), move || ok);
        assert_eq!(gpu(true), Ok(Gpu));
        assert_eq!(gpu(false), Err(ErrorCode::ModelLoadFailed));
    }

    #[test]
    fn htdemucs_table() {
        use Device::*;
        let auto = |ok| resolve_device("auto", &htdemucs(), move || ok);
        assert_eq!(auto(true), Ok(Gpu));
        assert_eq!(auto(false), Err(ErrorCode::GpuRequired));
        assert_eq!(
            resolve_device("cpu", &htdemucs(), never),
            Err(ErrorCode::GpuRequired)
        );
        let gpu = |ok| resolve_device("gpu", &htdemucs(), move || ok);
        assert_eq!(gpu(true), Ok(Gpu));
        assert_eq!(gpu(false), Err(ErrorCode::ModelLoadFailed));
    }

    #[test]
    fn a_model_without_a_gpu_hop_never_asks_the_gpu() {
        let cpu_only = spec(None, Some(300));
        assert_eq!(resolve_device("auto", &cpu_only, never), Ok(Device::Cpu));
        assert_eq!(
            resolve_device("gpu", &cpu_only, never),
            Err(ErrorCode::ModelLoadFailed)
        );
    }

    #[test]
    fn unknown_device_is_bad() {
        assert_eq!(
            resolve_device("dml", &bytesep(), never),
            Err(ErrorCode::BadDevice)
        );
        assert_eq!(resolve_device("", &None, never), Err(ErrorCode::BadDevice));
    }

    #[test]
    fn window_params_convert_ms_per_device_and_validate() {
        let w = spec(Some(300), Some(500)).unwrap();
        let gpu = window_params(&w, Device::Gpu).unwrap();
        assert_eq!(
            (gpu.window_frames, gpu.hop_frames, gpu.lookahead_frames),
            (44_100, 13_230, 4_410)
        );
        assert_eq!(window_params(&w, Device::Cpu).unwrap().hop_frames, 22_050);
        // No hop on the device.
        let gpu_only = spec(Some(300), None).unwrap();
        assert!(window_params(&gpu_only, Device::Cpu).is_err());
        // hop + lookahead exceed the window; hop below the crossfade.
        assert!(window_params(&spec(Some(950), Some(950)).unwrap(), Device::Gpu).is_err());
        assert!(window_params(&spec(Some(2), Some(2)).unwrap(), Device::Gpu).is_err());
    }

    /// Real model smoke test: `DEVOCAL_TEST_MODEL=<1 s onnx> cargo test -p devocal-engine
    /// ort_window_smoke -- --ignored --nocapture`. One window per device, no loops.
    #[test]
    #[ignore]
    fn ort_window_smoke() {
        let Ok(path) = std::env::var("DEVOCAL_TEST_MODEL") else {
            return;
        };
        let vocals_index = std::env::var("DEVOCAL_TEST_VOCALS_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        for device in [Device::Cpu, Device::Gpu] {
            let mut m = match OrtWindowModel::load(Path::new(&path), device, 2, vocals_index) {
                Ok(m) => m,
                Err(e) if device == Device::Gpu => {
                    eprintln!("GPU unavailable: {e}");
                    continue;
                }
                Err(e) => panic!("{device:?}: {e}"),
            };
            let n = m.window_frames() * 2;
            let input: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
            let mut vocals = vec![f32::NAN; n];
            m.run(&input, &mut vocals).unwrap();
            assert!(vocals.iter().all(|v| v.is_finite()), "{device:?}");
            eprintln!("{device:?}: ok, window {} frames", n / 2);
        }
    }
}
