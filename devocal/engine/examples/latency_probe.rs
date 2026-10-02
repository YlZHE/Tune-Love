//! Passive real-machine latency and dropout probe (Task 14).
//!
//! Records, at the same time, the player's own audio (process loopback of its process tree)
//! and the system output (loopback of a render endpoint, by default the default one), then:
//! - finds the added latency by cross-correlation (search 0..300 ms; coarse search on 8x
//!   decimated audio, refined at full rate) and reports the correlation coefficient;
//! - counts dropouts in 10 ms blocks, separately on each stream and relative to that stream's
//!   own level: a block more than 30 dB below the running median of its neighbours (±10
//!   blocks), where those neighbours are within 30 dB of the stream's median block energy.
//!   This works for the player's process loopback even while it is held at -80 dB.
//!   Output dropouts also present in the player's own audio are reported apart;
//! - prints one JSON object (and writes it to `--out` if given).
//!
//! It only captures: it talks to no engine and never changes any session volume.
//!
//! Usage:
//!   latency_probe --pid <player root pid> --seconds <s> [--endpoint <render endpoint id>]
//!                 [--out probe.json] [--dump <prefix>]
//!
//! `--dump` also writes the raw recordings (interleaved stereo f32 LE, 44.1 kHz) to
//! `<prefix>-source.f32` and `<prefix>-output.f32`, and their first-packet QPC times to
//! `<prefix>-times.json`, for offline analysis.

use std::thread;
use std::time::Instant;

use serde_json::json;
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

const RATE: usize = 44_100;
const MAX_LAG_MS: usize = 300;
const DECIMATE: usize = 8;
const BLOCK: usize = RATE / 100;
const NEIGHBOURS: usize = 10;
/// -30 dB in energy.
const GAP_RATIO: f64 = 1e-3;
/// Neighbours count as audible when their median energy is within 30 dB of the stream's
/// median block energy.
const AUDIBLE_RELATIVE: f64 = 1e-3;
/// Longest window used for the lag search.
const SEARCH_SECONDS: usize = 10;

struct Args {
    pid: u32,
    endpoint: Option<String>,
    seconds: f64,
    out: Option<String>,
    dump: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let (mut pid, mut seconds, mut endpoint, mut out, mut dump) = (None, None, None, None, None);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--pid" => pid = Some(value()?.parse().map_err(|e| format!("--pid: {e}"))?),
            "--seconds" => {
                seconds = Some(
                    value()?
                        .parse::<f64>()
                        .map_err(|e| format!("--seconds: {e}"))?,
                )
            }
            "--endpoint" => endpoint = Some(value()?),
            "--out" => out = Some(value()?),
            "--dump" => dump = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let pid = pid.ok_or("--pid <player root pid> is required")?;
    let seconds = seconds.ok_or("--seconds is required")?;
    if !(seconds > 0.0 && seconds <= 600.0) {
        return Err("--seconds must be in (0, 600]".into());
    }
    Ok(Args {
        pid,
        endpoint,
        seconds,
        out,
        dump,
    })
}

enum Source {
    Process(u32),
    Endpoint(Option<String>),
}

struct Recording {
    /// Interleaved stereo.
    samples: Vec<f32>,
    /// QPC time of the first frame, in 100 ns units.
    first_qpc: u64,
    discontinuities: usize,
    /// Packets flagged AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR / SILENT, and packets read.
    timestamp_errors: usize,
    silent_packets: usize,
    packets: usize,
}

fn record(src: Source, seconds: f64) -> Result<Recording, String> {
    wasapi::initialize_mta()
        .ok()
        .map_err(|e| format!("COM: {e}"))?;
    let result = (|| {
        let mut client = match &src {
            Source::Process(pid) => AudioClient::new_application_loopback_client(*pid, true)
                .map_err(|e| format!("process loopback {pid}: {e}"))?,
            Source::Endpoint(id) => {
                let en = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
                let dev = match id {
                    Some(id) => en.get_device(id),
                    None => en.get_default_device(&Direction::Render),
                }
                .map_err(|e| format!("render endpoint: {e}"))?;
                dev.get_iaudioclient()
                    .map_err(|e| format!("endpoint client: {e}"))?
            }
        };
        let fmt = WaveFormat::new(32, 32, &SampleType::Float, RATE, 2, None);
        client
            .initialize_client(
                &fmt,
                &Direction::Capture,
                &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: 0,
                },
            )
            .map_err(|e| format!("initialise capture: {e}"))?;
        let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
        let cap = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
        client.start_stream().map_err(|e| e.to_string())?;
        let mut rec = Recording {
            samples: Vec::with_capacity((seconds * RATE as f64) as usize * 2 + RATE),
            first_qpc: 0,
            discontinuities: 0,
            timestamp_errors: 0,
            silent_packets: 0,
            packets: 0,
        };
        let mut bytes = vec![0u8; RATE * 8];
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < seconds {
            let _ = event.wait_for_event(50);
            loop {
                let frames = cap.get_next_packet_size().map_err(|e| e.to_string())?;
                if frames.unwrap_or(0) == 0 {
                    break;
                }
                let (n, info) = cap
                    .read_from_device(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if rec.samples.is_empty() {
                    rec.first_qpc = info.timestamp;
                }
                rec.packets += 1;
                if info.flags.timestamp_error {
                    rec.timestamp_errors += 1;
                }
                if info.flags.silent {
                    rec.silent_packets += 1;
                }
                if info.flags.data_discontinuity && !rec.samples.is_empty() {
                    rec.discontinuities += 1;
                }
                rec.samples.extend(
                    bytes[..n as usize * 8]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                );
            }
        }
        let _ = client.stop_stream();
        drop(cap);
        drop(client);
        drop(event);
        Ok(rec)
    })();
    wasapi::deinitialize();
    result
}

fn dump(prefix: &str, src: &Recording, out: &Recording) -> Result<(), String> {
    for (name, rec) in [("source", src), ("output", out)] {
        let path = format!("{prefix}-{name}.f32");
        let bytes: Vec<u8> = rec.samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(&path, bytes).map_err(|e| format!("write {path}: {e}"))?;
    }
    let times =
        json!({ "sourceFirstQpc": src.first_qpc, "outputFirstQpc": out.first_qpc, "rate": RATE });
    let path = format!("{prefix}-times.json");
    std::fs::write(&path, times.to_string()).map_err(|e| format!("write {path}: {e}"))
}

fn mono(stereo: &[f32]) -> Vec<f32> {
    stereo
        .chunks_exact(2)
        .map(|f| 0.5 * (f[0] + f[1]))
        .collect()
}

fn decimate(x: &[f32]) -> Vec<f32> {
    x.chunks_exact(DECIMATE)
        .map(|c| c.iter().sum::<f32>() / DECIMATE as f32)
        .collect()
}

/// Pearson correlation of `out[i]` with `src[i + shift]` over the overlapping range of
/// `out` indices `range`.
fn correlation(out: &[f32], src: &[f32], shift: i64, range: (usize, usize)) -> f64 {
    let (mut sx, mut sy, mut sxx, mut syy, mut sxy, mut n) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for i in range.0..range.1 {
        let j = i as i64 + shift;
        if j < 0 || j as usize >= src.len() || i >= out.len() {
            continue;
        }
        let x = f64::from(src[j as usize]);
        let y = f64::from(out[i]);
        sx += x;
        sy += y;
        sxx += x * x;
        syy += y * y;
        sxy += x * y;
        n += 1.0;
    }
    if n < 2.0 {
        return 0.0;
    }
    let cov = sxy - sx * sy / n;
    let vx = sxx - sx * sx / n;
    let vy = syy - sy * sy / n;
    if vx <= 0.0 || vy <= 0.0 {
        return 0.0;
    }
    cov / (vx * vy).sqrt()
}

/// Best lag `d` (samples, output later than source) for `out[i] ~ src[i + offset - d]`.
fn find_lag(out: &[f32], src: &[f32], offset: i64) -> (usize, f64) {
    let max_lag = MAX_LAG_MS * RATE / 1000;
    // Search window: up to 10 s in the middle of the output recording.
    let len = out.len().min(SEARCH_SECONDS * RATE);
    let first = (out.len() - len) / 2;
    let window = (first, first + len);

    let out8 = decimate(out);
    let src8 = decimate(src);
    let w8 = (window.0 / DECIMATE, window.1 / DECIMATE);
    let off8 = offset / DECIMATE as i64;
    let mut best = (0usize, f64::MIN);
    for d8 in 0..=max_lag / DECIMATE {
        let r = correlation(&out8, &src8, off8 - d8 as i64, w8);
        if r > best.1 {
            best = (d8, r);
        }
    }
    let centre = best.0 * DECIMATE;
    let mut fine = (centre, f64::MIN);
    for d in centre.saturating_sub(2 * DECIMATE)..=(centre + 2 * DECIMATE).min(max_lag) {
        let r = correlation(out, src, offset - d as i64, window);
        if r > fine.1 {
            fine = (d, r);
        }
    }
    fine
}

/// Dropout runs (first block index of each) in 10 ms blocks, relative to the stream's own
/// level: energies are normalised by the stream's median block energy, a block is a gap when
/// it is more than 30 dB below the running median of its ±10 neighbours and those neighbours
/// are within 30 dB of the stream's median. A stream that is mostly digital silence (median
/// energy 0) reports none.
fn dropouts(x: &[f32]) -> Vec<usize> {
    let mut energy: Vec<f64> = x
        .chunks_exact(BLOCK)
        .map(|b| b.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / BLOCK as f64)
        .collect();
    let level = median(&energy);
    if level <= 0.0 || !level.is_finite() {
        return Vec::new();
    }
    for e in energy.iter_mut() {
        *e /= level;
    }
    let mut runs = Vec::new();
    let mut in_gap = false;
    let mut neighbours = Vec::with_capacity(2 * NEIGHBOURS);
    for b in 0..energy.len() {
        neighbours.clear();
        let lo = b.saturating_sub(NEIGHBOURS);
        let hi = (b + NEIGHBOURS + 1).min(energy.len());
        neighbours.extend((lo..hi).filter(|&k| k != b).map(|k| energy[k]));
        let local = median(&neighbours);
        let gap = local > AUDIBLE_RELATIVE && energy[b] < local * GAP_RATIO;
        if gap && !in_gap {
            runs.push(b);
        }
        in_gap = gap;
    }
    runs
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / x.len() as f64).sqrt()
}

fn main() {
    if let Err(e) = run() {
        eprintln!("latency_probe: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let pid = args.pid;
    let secs = args.seconds;
    let endpoint = args.endpoint.clone();
    let src = thread::spawn(move || record(Source::Process(pid), secs));
    let out = thread::spawn(move || record(Source::Endpoint(endpoint), secs));
    let src = src.join().map_err(|_| "source capture panicked")??;
    let out = out.join().map_err(|_| "output capture panicked")??;

    if let Some(prefix) = &args.dump {
        dump(prefix, &src, &out)?;
    }
    let src_m = mono(&src.samples);
    let out_m = mono(&out.samples);
    if src_m.len() < RATE || out_m.len() < RATE {
        return Err("less than 1 s recorded; is the player playing?".into());
    }
    // Source index of the output's first frame, from the packets' QPC times.
    let offset =
        ((out.first_qpc as i128 - src.first_qpc as i128) * RATE as i128 / 10_000_000) as i64;
    let (lag, _) = find_lag(&out_m, &src_m, offset);
    let corr = correlation(&out_m, &src_m, offset - lag as i64, (0, out_m.len()));

    let out_gaps = dropouts(&out_m);
    let src_gaps = dropouts(&src_m);
    let lag_blocks = (lag / BLOCK) as i64;
    let offset_blocks = offset / BLOCK as i64;
    let ours: Vec<usize> = out_gaps
        .iter()
        .copied()
        .filter(|&b| {
            let s = b as i64 + offset_blocks - lag_blocks;
            !src_gaps.iter().any(|&g| (g as i64 - s).abs() <= 2)
        })
        .collect();

    let report = json!({
        "pid": pid,
        "seconds": secs,
        "latencyMs": lag as f64 * 1000.0 / RATE as f64,
        "correlation": corr,
        "lagSearchMs": [0, MAX_LAG_MS],
        "dropouts": ours.len(),
        "dropoutTimesMs": ours.iter().map(|&b| b * 10).collect::<Vec<_>>(),
        "outputGapsTotal": out_gaps.len(),
        "sourceGaps": src_gaps.len(),
        "sourceRms": rms(&src_m),
        "outputRms": rms(&out_m),
        "sourceDiscontinuities": src.discontinuities,
        "outputDiscontinuities": out.discontinuities,
        "sourcePackets": src.packets,
        "sourceTimestampErrors": src.timestamp_errors,
        "sourceSilentPackets": src.silent_packets,
        "outputPackets": out.packets,
        "outputTimestampErrors": out.timestamp_errors,
        "outputSilentPackets": out.silent_packets,
        "endpoint": args.endpoint,
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    println!("{text}");
    if let Some(path) = &args.out {
        std::fs::write(path, &text).map_err(|e| format!("write {path}: {e}"))?;
    }
    Ok(())
}
