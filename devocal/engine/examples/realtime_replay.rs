//! Offline real-time replay: drives the production window separator (the `OrtWindowModel` +
//! `WindowedSeparator` the engine's `load_model` builds on the GPU) inside the production
//! `Processor`, devocal on, with audio from a raw file fed in capture-sized packets paced at
//! real time on a dedicated processing thread, in 128-frame blocks as `audio/processing.rs` does.
//!
//! Usage: `realtime_replay <htdemucs|bytesep> <model.onnx> <audio.f32> <out_prefix> [packet_frames]`
//! (audio: interleaved stereo f32le, 44.1 kHz; packets default to 441 frames = 10 ms).
//!
//! Writes, next to `<out_prefix>`:
//! - `.out.f32`     the processor output (interleaved f32le), one frame per input frame;
//! - `.blocks.csv`  per block: index, input frames consumed, start/end us, timeouts, stage;
//! - `.calls.csv`   per model run: call, matching window k, max |window input - true slice|
//!   for k = call, start/end us (worker side, around the inner `run`);
//! - `.vocals.f32`  every run's full vocals window (interleaved f32le).
//!
//! Instrumentation sits outside the product code: the model is wrapped in a recorder that
//! hands copies to a writer thread (allocation on the inference thread only; the measured
//! inference time includes one memcpy). Read-only apart from the output files.

#![allow(dead_code)]

#[path = "../src/dsp.rs"]
mod dsp;
#[path = "../src/ort_window.rs"]
mod ort_window;
#[path = "../src/processor.rs"]
mod processor;
#[path = "../src/separator.rs"]
mod separator;
#[path = "../src/stemgen.rs"]
mod stemgen;
#[path = "../src/windowed.rs"]
mod windowed;

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use devocal_core::protocol::{Device, WindowedSpec};
use windowed::{WindowModel, WindowedSeparator, WINDOWED_HOP};

struct Rec {
    call: usize,
    t0: u64,
    t1: u64,
    input: Vec<f32>,
    vocals: Vec<f32>,
}

/// Times the inner model and ships copies of every window to the writer thread.
struct Recorder {
    inner: ort_window::OrtWindowModel,
    epoch: Instant,
    calls: usize,
    tx: Sender<Rec>,
}

impl WindowModel for Recorder {
    fn run(&mut self, input: &[f32], vocals: &mut [f32]) -> Result<(), String> {
        let t0 = self.epoch.elapsed().as_micros() as u64;
        let r = self.inner.run(input, vocals);
        let t1 = self.epoch.elapsed().as_micros() as u64;
        let _ = self.tx.send(Rec {
            call: self.calls,
            t0,
            t1,
            input: input.to_vec(),
            vocals: vocals.to_vec(),
        });
        self.calls += 1;
        r
    }
}

/// The manifest's (src-tauri/models.json) window geometry, as the supervisor sends it.
fn spec(model: &str) -> Result<WindowedSpec, String> {
    Ok(match model {
        "htdemucs" => WindowedSpec {
            window_ms: 1000,
            lookahead_ms: 100,
            gpu_hop_ms: Some(100),
            cpu_hop_ms: None,
            cpu_threads: 0,
            vocals_index: 3,
        },
        "bytesep" => WindowedSpec {
            window_ms: 1000,
            lookahead_ms: 100,
            gpu_hop_ms: Some(100),
            cpu_hop_ms: Some(200),
            cpu_threads: 2,
            vocals_index: 0,
        },
        _ => return Err(format!("unknown model {model}")),
    })
}

fn main() -> Result<(), String> {
    let a: Vec<String> = std::env::args().collect();
    if !(5..=6).contains(&a.len()) {
        return Err(
            "usage: realtime_replay <htdemucs|bytesep> <model.onnx> <audio.f32> \
                    <out_prefix> [packet_frames]"
                .into(),
        );
    }
    let w = spec(&a[1])?;
    let prefix = a[4].clone();
    let packet: usize = a
        .get(5)
        .map_or(Ok(441), |s| s.parse())
        .map_err(|e| format!("{e}"))?;
    let bytes = std::fs::read(&a[3]).map_err(|e| format!("audio: {e}"))?;
    let audio: Arc<Vec<f32>> = Arc::new(
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
    );
    let frames = audio.len() / 2;

    // Production construction (engine.rs load_model, GPU branch; the cached self-check is
    // skipped, `load` already ran one warm-up window).
    let params = ort_window::window_params(&w, Device::Gpu)?;
    let model = ort_window::OrtWindowModel::load(Path::new(&a[2]), Device::Gpu, 1, w.vocals_index)?;
    if model.window_frames() != params.window_frames {
        return Err("window mismatch".into());
    }
    let (wf, h, l) = (
        params.window_frames,
        params.hop_frames,
        params.lookahead_frames,
    );
    let epoch = Instant::now();
    let (tx, rx) = channel::<Rec>();
    let sep = WindowedSeparator::new(
        Box::new(Recorder {
            inner: model,
            epoch,
            calls: 0,
            tx,
        }),
        params,
    );

    // Writer: checks each window input against the true slice and dumps the vocals.
    let truth = audio.clone();
    let wprefix = prefix.clone();
    let writer = thread::spawn(move || -> Result<(), String> {
        let io = |e: std::io::Error| e.to_string();
        let mut csv =
            BufWriter::new(std::fs::File::create(format!("{wprefix}.calls.csv")).map_err(io)?);
        writeln!(csv, "call,k_match,in_maxdiff_k_eq_call,t0_us,t1_us").map_err(io)?;
        let mut voc =
            BufWriter::new(std::fs::File::create(format!("{wprefix}.vocals.f32")).map_err(io)?);
        let diff = |k: i64, x: &[f32]| -> f32 {
            let start = k * h as i64 + (h + l) as i64 - wf as i64;
            x.iter()
                .enumerate()
                .map(|(i, &v)| {
                    let f = start + (i / 2) as i64;
                    let t = if f < 0 || f as usize >= truth.len() / 2 {
                        0.0
                    } else {
                        truth[f as usize * 2 + i % 2]
                    };
                    (v - t).abs()
                })
                .fold(0.0, f32::max)
        };
        for r in rx {
            let d = diff(r.call as i64, &r.input);
            let k_match = if d == 0.0 {
                r.call as i64
            } else {
                (r.call as i64 - 4..=r.call as i64 + 4)
                    .find(|&k| diff(k, &r.input) == 0.0)
                    .unwrap_or(-1)
            };
            writeln!(csv, "{},{k_match},{d:e},{},{}", r.call, r.t0, r.t1).map_err(io)?;
            for v in &r.vocals {
                voc.write_all(&v.to_le_bytes()).map_err(io)?;
            }
        }
        csv.flush().map_err(io)?;
        voc.flush().map_err(io)
    });

    // Processing thread: one packet per packet-duration of wall clock, then every whole block.
    let feed = audio.clone();
    let pprefix = prefix.clone();
    let proc = thread::Builder::new()
        .name("replay-processing".into())
        .spawn(move || -> Result<(u64, f32, usize), String> {
            let mut p = processor::Processor::new(Some(Box::new(sep)));
            let latency = p.latency_frames();
            p.request_devocal(true);
            let io = |e: std::io::Error| e.to_string();
            let mut blocks =
                BufWriter::new(std::fs::File::create(format!("{pprefix}.blocks.csv")).map_err(io)?);
            writeln!(blocks, "block,pos,t0_us,t1_us,timeouts,stage").map_err(io)?;
            let mut out = Vec::with_capacity(feed.len());
            let mut obuf = vec![0.0f32; WINDOWED_HOP * 2];
            let period = Duration::from_secs_f64(packet as f64 / 44_100.0);
            let start = Instant::now();
            let (mut avail, mut pos, mut b) = (0usize, 0usize, 0usize);
            let mut i = 0u32;
            while avail < frames {
                let due = start + period * i;
                if let Some(d) = due.checked_duration_since(Instant::now()) {
                    thread::sleep(d);
                }
                i += 1;
                avail = (avail + packet).min(frames);
                while pos + WINDOWED_HOP <= avail {
                    let input = &feed[pos * 2..(pos + WINDOWED_HOP) * 2];
                    let t0 = epoch.elapsed().as_micros();
                    p.process_block(input, &mut obuf);
                    let t1 = epoch.elapsed().as_micros();
                    pos += WINDOWED_HOP;
                    out.extend_from_slice(&obuf);
                    let (to, _) = p.window_stats().unwrap_or((0, 0.0));
                    writeln!(blocks, "{b},{pos},{t0},{t1},{to},{:?}", p.stage()).map_err(io)?;
                    b += 1;
                }
            }
            blocks.flush().map_err(io)?;
            let mut f =
                BufWriter::new(std::fs::File::create(format!("{pprefix}.out.f32")).map_err(io)?);
            for v in &out {
                f.write_all(&v.to_le_bytes()).map_err(io)?;
            }
            f.flush().map_err(io)?;
            let (to, duty) = p.window_stats().unwrap_or((0, 0.0));
            drop(p); // joins the worker, closing the recorder channel
            Ok((to, duty, latency))
        })
        .map_err(|e| e.to_string())?;
    let (timeouts, duty, latency) = proc.join().map_err(|_| "processing panicked")??;
    writer.join().map_err(|_| "writer panicked")??;
    println!(
        "{} frames, W {wf} H {h} L {l}, latency {latency}, packet {packet}: timeouts {timeouts}, duty {duty:.3}",
        frames
    );
    Ok(())
}
