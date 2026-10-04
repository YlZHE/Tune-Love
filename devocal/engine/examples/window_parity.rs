//! Offline correctness probe: does the engine's own ONNX Runtime (DirectML) path compute the
//! same vocals as its CPU path, window by window?
//!
//! Usage: `window_parity <model.onnx> <vocals_index> <audio.f32> <out.csv>`
//! The audio is raw interleaved stereo f32le at 44.1 kHz. Windows of W frames end every
//! H = 4410 frames (the product hop); for each the GPU and CPU vocals are compared (SDR of GPU
//! against CPU) over the whole window and over the product slice [W-L-H, W-L+XF), L = 4410.
//! GPU runs are paced at 4x their run time (duty <= 20 %), CPU runs at 1x with 2 threads.
//! The GPU window vocals are also written (interleaved f32le) to `<out.csv>.gpu.f32` so they
//! can be compared with other runtimes; the lowest-SDR windows are re-run to test determinism.
//! Read-only: touches nothing but the files named on the command line.

#![allow(dead_code)]

#[path = "../src/dsp.rs"]
mod dsp;
#[path = "../src/ort_window.rs"]
mod ort_window;
#[path = "../src/separator.rs"]
mod separator;
#[path = "../src/stemgen.rs"]
mod stemgen;
#[path = "../src/windowed.rs"]
mod windowed;

use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant};

use devocal_core::protocol::Device;
use windowed::{WindowModel, XF};

const H: usize = 4410;
const L: usize = 4410;

fn sdr(reference: &[f32], test: &[f32]) -> f64 {
    let (mut s, mut e) = (0f64, 0f64);
    for (&r, &t) in reference.iter().zip(test) {
        s += f64::from(r) * f64::from(r);
        e += (f64::from(r) - f64::from(t)).powi(2);
    }
    10.0 * (s.max(1e-30) / e.max(1e-30)).log10()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64).sqrt()
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() -> Result<(), String> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 5 {
        return Err("usage: window_parity <model.onnx> <vocals_index> <audio.f32> <out.csv>".into());
    }
    let idx: u32 = a[2].parse().map_err(|e| format!("vocals_index: {e}"))?;
    let bytes = std::fs::read(&a[3]).map_err(|e| format!("audio: {e}"))?;
    let audio: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let model = std::path::Path::new(&a[1]);
    let mut gpu = ort_window::OrtWindowModel::load(model, Device::Gpu, 1, idx)?;
    let mut cpu = ort_window::OrtWindowModel::load(model, Device::Cpu, 2, idx)?;
    let w = gpu.window_frames();
    let frames = audio.len() / 2;
    let (s0, s1) = (w - L - H, w - L + XF);
    let n_win = (frames - w) / H + 1;
    eprintln!("{} frames, W {w}, {n_win} windows", frames);

    let mut csv = std::fs::File::create(&a[4]).map_err(|e| e.to_string())?;
    writeln!(csv, "idx,end_frame,in_rms,voc_rms,sdr_full,sdr_slice,max_abs_diff,nonfinite_gpu,nonfinite_cpu,gpu_ms")
        .map_err(|e| e.to_string())?;
    let mut dump = std::io::BufWriter::new(
        std::fs::File::create(format!("{}.gpu.f32", a[4])).map_err(|e| e.to_string())?,
    );
    let (mut vg, mut vc) = (vec![0f32; w * 2], vec![0f32; w * 2]);
    let mut rows: Vec<(usize, f64, f64)> = Vec::new(); // (idx, sdr_full, sdr_slice)
    let mut gpu_out: Vec<Vec<f32>> = Vec::new();
    let mut cpu_out: Vec<Vec<f32>> = Vec::new();
    for m in 0..n_win {
        let end = w + m * H;
        let input = &audio[(end - w) * 2..end * 2];
        let t = Instant::now();
        gpu.run(input, &mut vg)?;
        let gdt = t.elapsed();
        sleep(gdt * 4 + Duration::from_millis(5));
        let t = Instant::now();
        cpu.run(input, &mut vc)?;
        sleep(t.elapsed());
        let nf_g = vg.iter().filter(|v| !v.is_finite()).count();
        let nf_c = vc.iter().filter(|v| !v.is_finite()).count();
        let full = sdr(&vc, &vg);
        let slice = sdr(&vc[s0 * 2..s1 * 2], &vg[s0 * 2..s1 * 2]);
        let mad = vg.iter().zip(&vc).map(|(g, c)| (g - c).abs()).fold(0f32, f32::max);
        writeln!(
            csv,
            "{m},{end},{:.6},{:.6},{full:.2},{slice:.2},{mad:.6},{nf_g},{nf_c},{:.2}",
            rms(input),
            rms(&vc),
            gdt.as_secs_f64() * 1e3
        )
        .map_err(|e| e.to_string())?;
        for v in &vg {
            dump.write_all(&v.to_le_bytes()).map_err(|e| e.to_string())?;
        }
        rows.push((m, full, slice));
        gpu_out.push(vg.clone());
        cpu_out.push(vc.clone());
        if m % 50 == 0 {
            eprintln!("window {m}/{n_win}");
        }
    }
    dump.flush().map_err(|e| e.to_string())?;

    for (name, col) in [("full", 1usize), ("slice", 2)] {
        let mut v: Vec<f64> = rows.iter().map(|r| if col == 1 { r.1 } else { r.2 }).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "{name}: n {} min {:.1} p1 {:.1} median {:.1}  <40 dB: {}  <20 dB: {}",
            v.len(),
            v[0],
            pct(&v, 0.01),
            pct(&v, 0.5),
            v.iter().filter(|&&x| x < 40.0).count(),
            v.iter().filter(|&&x| x < 20.0).count()
        );
    }

    // Determinism: re-run the 5 worst windows (by full SDR) twice on the GPU.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&x, &y| rows[x].1.partial_cmp(&rows[y].1).unwrap());
    for &m in order.iter().take(5) {
        let end = w + m * H;
        let input = &audio[(end - w) * 2..end * 2];
        let first = &gpu_out[m];
        let mut line = format!("rerun window {m} (sdr_full {:.1}):", rows[m].1);
        for _ in 0..2 {
            let t = Instant::now();
            gpu.run(input, &mut vg)?;
            sleep(t.elapsed() * 4 + Duration::from_millis(5));
                line += &format!(
                    " vs-first-gpu {:.1} vs-cpu {:.1};",
                    sdr(first, &vg),
                    sdr(&cpu_out[m], &vg)
                );
        }
        println!("{line}");
    }
    Ok(())
}
