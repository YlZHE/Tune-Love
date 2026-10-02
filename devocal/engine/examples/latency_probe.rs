//! Passive real-machine latency and dropout probe (Task 14).
//!
//! Records, at the same time, the player's own audio (process loopback of its process tree)
//! and the system output (loopback of a render endpoint, by default the default one), then:
//! - builds a lag timeline: mono mix, windows of 4096 frames every 0.25 s; for each window the
//!   lag `L` in [-300, +300] ms that maximises the normalised correlation of `out[i]` with
//!   `src[i - L]` (coarse search on 8x decimated audio, refined at full rate). Windows where
//!   either side is digital silence, or that match poorly (correlation < 0.5), are skipped;
//! - groups the windows into segments (runs of at least 4 consecutive windows whose lags stay
//!   within +-2 frames of each other) and reports the jump between consecutive segments;
//! - counts dropouts in 10 ms blocks, separately on each stream and relative to that stream's
//!   own level: a block more than 30 dB below the running median of its neighbours (+-10
//!   blocks), where those neighbours are within 30 dB of the stream's median block energy.
//!   This works for the player's process loopback even while it is held at -80 dB.
//!   Output dropouts also present in the player's own audio are reported apart;
//! - prints one JSON object (and writes it to `--out` if given).
//!
//! It only captures: it talks to no engine and never changes any session volume.
//!
//! How to measure latency: the absolute lag between the two recordings is NOT a latency (the
//! two capture paths have their own, different, unknown delays; the content offset between
//! them is merely stable from run to run). What is exact is a lag JUMP inside one recording.
//! Start the probe, keep at least 3 s of steady playback before and after a release (engine
//! detaches: output content skips forward by the added delay, jump = minus the latency,
//! reported in `releaseLatencyMs`) or an attach (jump = plus the latency, `attachLatencyMs`).
//! Segments shorter than 4 windows (1 s) are ignored, so hold each state for a few seconds.
//! A recording with no state change gives one segment and no jumps.
//!
//! Usage:
//!   latency_probe --pid <player root pid> --seconds <s> [--endpoint <render endpoint id>]
//!                 [--out probe.json] [--dump <prefix>]
//!
//! `--dump` also writes the raw recordings (interleaved stereo f32 LE, 44.1 kHz) to
//! `<prefix>-source.f32` and `<prefix>-output.f32`, and their first-packet QPC times to
//! `<prefix>-times.json`, for offline analysis (the times are unreliable for alignment).
//!
//! The analysis is pure functions over the two mono signals, unit-tested with synthetic
//! signals (`cargo test --workspace` runs them: the example is declared with `test = true`).

use std::thread;
use std::time::Instant;

use serde_json::json;
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

const RATE: usize = 44_100;
const MAX_LAG_MS: usize = 300;
const MAX_LAG: usize = MAX_LAG_MS * RATE / 1000;
const DECIMATE: usize = 8;
/// Lag-timeline window length (frames) and hop (0.25 s).
const WINDOW: usize = 4096;
const HOP: usize = RATE / 4;
/// Windows that match worse than this are skipped (no usable alignment there).
const MIN_WINDOW_CORR: f64 = 0.5;
/// Peak amplitude at or below which a stretch counts as digital silence (-140 dB).
const SILENCE_PEAK: f32 = 1e-7;
/// Window lags this close (frames) belong to the same segment.
const SEGMENT_TOLERANCE: i64 = 2;
/// Shortest run of windows reported as a segment.
const MIN_SEGMENT_WINDOWS: usize = 4;
const BLOCK: usize = RATE / 100;
const NEIGHBOURS: usize = 10;
/// -30 dB in energy.
const GAP_RATIO: f64 = 1e-3;
/// Neighbours count as audible when their median energy is within 30 dB of the stream's
/// median block energy.
const AUDIBLE_RELATIVE: f64 = 1e-3;

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

/// Pearson correlation of `out[i]` with `src[i + shift]` over the `out` indices `range`
/// (indices outside either signal are skipped).
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

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, &s| m.max(s.abs()))
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|&s| f64::from(s) * f64::from(s)).sum()
}

/// Normalised correlation (cosine, scale invariant) of `a` with `src[start..start + a.len()]`;
/// `None` when that range is outside `src`, 0 when the source part has no energy.
fn cosine_at(a: &[f32], a_norm: f64, src: &[f32], start: i64) -> Option<f64> {
    if start < 0 || start as usize + a.len() > src.len() {
        return None;
    }
    let b = &src[start as usize..start as usize + a.len()];
    let (mut dot, mut bb) = (0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        dot += f64::from(x) * f64::from(y);
        bb += f64::from(y) * f64::from(y);
    }
    Some(if bb > 0.0 {
        dot / (a_norm * bb.sqrt())
    } else {
        0.0
    })
}

/// Best lag (frames, `out[i] ~ src[i - lag]`) of the output window starting at `start` and
/// its normalised correlation; coarse search on the decimated signals, then a fine search of
/// +-2 decimation steps at full rate. `None` when nothing could be compared.
fn window_lag(
    out: &[f32],
    src: &[f32],
    out8: &[f32],
    src8: &[f32],
    start: usize,
) -> Option<(i64, f64)> {
    let w8 = (start / DECIMATE, (start + WINDOW) / DECIMATE);
    let a8 = &out8[w8.0..w8.1];
    let n8 = energy(a8).sqrt();
    let mut coarse: Option<(i64, f64)> = None;
    for d8 in -((MAX_LAG / DECIMATE) as i64)..=(MAX_LAG / DECIMATE) as i64 {
        if let Some(r) = cosine_at(a8, n8, src8, w8.0 as i64 - d8) {
            if coarse.is_none_or(|(_, best)| r > best) {
                coarse = Some((d8, r));
            }
        }
    }
    let centre = coarse?.0 * DECIMATE as i64;
    let a = &out[start..start + WINDOW];
    let n = energy(a).sqrt();
    let mut fine: Option<(i64, f64)> = None;
    let reach = 2 * DECIMATE as i64;
    for d in (centre - reach).max(-(MAX_LAG as i64))..=(centre + reach).min(MAX_LAG as i64) {
        if let Some(r) = cosine_at(a, n, src, start as i64 - d) {
            if fine.is_none_or(|(_, best)| r > best) {
                fine = Some((d, r));
            }
        }
    }
    fine
}

/// One analysed window: first output frame, best lag in frames, peak correlation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct WindowLag {
    start: usize,
    lag: i64,
    corr: f64,
}

/// Lag of every window (4096 frames, every 0.25 s) in [-300, +300] ms. Windows start at
/// 300 ms so that every candidate lag has source audio; windows where the output window or
/// the whole source search range is digital silence, or whose best correlation is below
/// [`MIN_WINDOW_CORR`], are left out.
fn lag_timeline(out: &[f32], src: &[f32]) -> Vec<WindowLag> {
    let out8 = decimate(out);
    let src8 = decimate(src);
    let mut v = Vec::new();
    let mut start = MAX_LAG;
    while start + WINDOW <= out.len() && start + WINDOW + MAX_LAG <= src.len() {
        let silent = peak(&out[start..start + WINDOW]) <= SILENCE_PEAK
            || peak(&src[start - MAX_LAG..start + WINDOW + MAX_LAG]) <= SILENCE_PEAK;
        if !silent {
            if let Some((lag, corr)) = window_lag(out, src, &out8, &src8, start) {
                if corr >= MIN_WINDOW_CORR {
                    v.push(WindowLag { start, lag, corr });
                }
            }
        }
        start += HOP;
    }
    v
}

/// A run of windows with a steady lag.
#[derive(Debug, Clone, PartialEq)]
struct Segment {
    start_s: f64,
    end_s: f64,
    /// Median window lag, frames.
    lag_frames: i64,
    mean_corr: f64,
    windows: usize,
    /// Output frame range covered (first window start .. last window end).
    span: (usize, usize),
}

impl Segment {
    fn lag_ms(&self) -> f64 {
        self.lag_frames as f64 * 1000.0 / RATE as f64
    }
}

fn close_run(run: &mut Vec<WindowLag>, segments: &mut Vec<Segment>) {
    if run.len() >= MIN_SEGMENT_WINDOWS {
        let mut lags: Vec<i64> = run.iter().map(|w| w.lag).collect();
        lags.sort_unstable();
        let first = run[0].start;
        let end = run[run.len() - 1].start + WINDOW;
        segments.push(Segment {
            start_s: first as f64 / RATE as f64,
            end_s: end as f64 / RATE as f64,
            lag_frames: lags[lags.len() / 2],
            mean_corr: run.iter().map(|w| w.corr).sum::<f64>() / run.len() as f64,
            windows: run.len(),
            span: (first, end),
        });
    }
    run.clear();
}

/// Runs of consecutive windows whose lag stays within +-2 frames of the previous window's,
/// at least [`MIN_SEGMENT_WINDOWS`] long.
fn segments(windows: &[WindowLag]) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut run: Vec<WindowLag> = Vec::new();
    for &w in windows {
        if run
            .last()
            .is_some_and(|l| (w.lag - l.lag).abs() > SEGMENT_TOLERANCE)
        {
            close_run(&mut run, &mut out);
        }
        run.push(w);
    }
    close_run(&mut run, &mut out);
    out
}

/// `next.lag - prev.lag` in ms for each pair of consecutive segments. Across a release the
/// output content skips forward, so the jump is minus the added latency; across an attach it
/// is plus it.
fn jumps_ms(segments: &[Segment]) -> Vec<f64> {
    segments
        .windows(2)
        .map(|p| p[1].lag_ms() - p[0].lag_ms())
        .collect()
}

struct Analysis {
    windows: usize,
    segments: Vec<Segment>,
    jumps_ms: Vec<f64>,
    /// Index of the segment with the most windows, and the whole-span Pearson correlation of
    /// the output with the source at that segment's lag (scale invariant).
    dominant: Option<(usize, f64)>,
}

fn analyse(out: &[f32], src: &[f32]) -> Analysis {
    let windows = lag_timeline(out, src);
    let segments = segments(&windows);
    let jumps_ms = jumps_ms(&segments);
    // The first of equally long segments wins.
    let dominant = segments
        .iter()
        .enumerate()
        .max_by_key(|(i, s)| (s.windows, std::cmp::Reverse(*i)))
        .map(|(i, s)| (i, correlation(out, src, -s.lag_frames, s.span)));
    Analysis {
        windows: windows.len(),
        segments,
        jumps_ms,
        dominant,
    }
}

/// Lag (frames) in force at output frame `at`: that of the segment containing it, else of
/// the nearest segment; 0 when there are none.
fn lag_at(segments: &[Segment], at: usize) -> i64 {
    let dist = |s: &Segment| {
        if at < s.span.0 {
            s.span.0 - at
        } else {
            at.saturating_sub(s.span.1)
        }
    };
    segments
        .iter()
        .min_by_key(|s| dist(s))
        .map_or(0, |s| s.lag_frames)
}

fn segment_json(s: &Segment) -> serde_json::Value {
    json!({
        "startS": s.start_s,
        "endS": s.end_s,
        "lagMs": s.lag_ms(),
        "meanCorr": s.mean_corr,
    })
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
    let analysis = analyse(&out_m, &src_m);

    let out_gaps = dropouts(&out_m);
    let src_gaps = dropouts(&src_m);
    let ours: Vec<usize> = out_gaps
        .iter()
        .copied()
        .filter(|&b| {
            // The source block this output block was playing, at the lag in force there.
            let lag_blocks = lag_at(&analysis.segments, b * BLOCK) / BLOCK as i64;
            let s = b as i64 - lag_blocks;
            !src_gaps.iter().any(|&g| (g as i64 - s).abs() <= 2)
        })
        .collect();

    let release: Vec<f64> = analysis
        .jumps_ms
        .iter()
        .filter(|&&j| j < 0.0)
        .map(|j| -j)
        .collect();
    let attach: Vec<f64> = analysis
        .jumps_ms
        .iter()
        .copied()
        .filter(|&j| j > 0.0)
        .collect();
    let dominant = analysis.dominant.map(|(i, corr)| {
        let mut d = segment_json(&analysis.segments[i]);
        d["windows"] = json!(analysis.segments[i].windows);
        d["correlation"] = json!(corr);
        d
    });

    let report = json!({
        "pid": pid,
        "seconds": secs,
        "windows": analysis.windows,
        "segments": analysis.segments.iter().map(segment_json).collect::<Vec<_>>(),
        "jumpsMs": analysis.jumps_ms,
        "releaseLatencyMs": release,
        "attachLatencyMs": attach,
        "dominant": dominant,
        "correlation": analysis.dominant.map(|(_, c)| c),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white noise in [-0.5, 0.5).
    fn noise(frames: usize, seed: u64) -> Vec<f32> {
        let mut x = seed;
        (0..frames)
            .map(|_| {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((x >> 40) as f32 / (1u64 << 24) as f32) - 0.5
            })
            .collect()
    }

    /// `out[i] = src[i - lag]` for `i` before `switch`, `src[i - lag_after]` from it on.
    fn delayed(src: &[f32], frames: usize, switch: usize, lag: i64, lag_after: i64) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                let l = if i < switch { lag } else { lag_after };
                let j = i as i64 - l;
                if j < 0 {
                    0.0
                } else {
                    src[j as usize]
                }
            })
            .collect()
    }

    const RELEASE_FRAMES: i64 = 3_312; // 75.1 ms

    fn release_pair(scale: f32) -> (Vec<f32>, Vec<f32>) {
        let src: Vec<f32> = noise(14 * RATE, 7).iter().map(|s| s * scale).collect();
        let out = delayed(&src, 12 * RATE, 5 * RATE, 100, 100 - RELEASE_FRAMES);
        // The output is the full-scale signal whatever the source level is.
        let out = out.iter().map(|s| s / scale).collect();
        (src, out)
    }

    fn check_release(src: &[f32], out: &[f32]) {
        let a = analyse(out, src);
        assert_eq!(a.segments.len(), 2, "{:?}", a.segments);
        assert_eq!(a.jumps_ms.len(), 1);
        let want = -(RELEASE_FRAMES as f64) * 1000.0 / RATE as f64;
        assert!(
            (a.jumps_ms[0] - want).abs() <= 0.1,
            "jump {} ms, want {want}",
            a.jumps_ms[0]
        );
        assert!((a.segments[0].lag_ms() - 100.0 * 1000.0 / RATE as f64).abs() < 0.05);
        assert!(a.segments[0].end_s <= a.segments[1].start_s + 0.3);
        assert!(a.segments.iter().all(|s| s.mean_corr > 0.9));
        let (_, corr) = a.dominant.unwrap();
        assert!(corr > 0.99, "{corr}");
    }

    #[test]
    fn release_shows_as_one_negative_jump() {
        let (src, out) = release_pair(1.0);
        check_release(&src, &out);
    }

    #[test]
    fn jump_is_scale_invariant_for_a_held_source() {
        // Source held at -80 dB, output at full scale.
        let (src, out) = release_pair(1e-4);
        check_release(&src, &out);
    }

    #[test]
    fn attach_shows_as_one_positive_jump() {
        let src = noise(14 * RATE, 11);
        let out = delayed(&src, 12 * RATE, 5 * RATE, -200, -200 + RELEASE_FRAMES);
        let a = analyse(&out, &src);
        assert_eq!(a.segments.len(), 2, "{:?}", a.segments);
        let want = RELEASE_FRAMES as f64 * 1000.0 / RATE as f64;
        assert!((a.jumps_ms[0] - want).abs() <= 0.1, "{}", a.jumps_ms[0]);
        assert!(a.jumps_ms[0] > 0.0);
    }

    #[test]
    fn baseline_has_one_segment_and_no_jumps() {
        let src = noise(14 * RATE, 3);
        let out = src[..12 * RATE].to_vec();
        let a = analyse(&out, &src);
        assert_eq!(a.segments.len(), 1, "{:?}", a.segments);
        assert!(a.jumps_ms.is_empty());
        assert_eq!(a.segments[0].lag_frames, 0);
        let (i, corr) = a.dominant.unwrap();
        assert_eq!(i, 0);
        assert!(corr >= 0.999, "{corr}");
    }

    #[test]
    fn silent_stretches_are_skipped_not_matched() {
        let src = noise(14 * RATE, 5);
        let mut out = src[..12 * RATE].to_vec();
        out[4 * RATE..8 * RATE].fill(0.0);
        let a = analyse(&out, &src);
        let w = lag_timeline(&out, &src);
        assert_eq!(a.windows, w.len());
        assert!(w.len() > 20 && w.len() < 40, "{}", w.len());
        // No window touches the silent stretch; matched ones keep the true lag 0.
        assert!(w
            .iter()
            .all(|x| x.start + WINDOW <= 4 * RATE || x.start >= 8 * RATE));
        assert!(w.iter().all(|x| x.lag == 0));
        // All-silent source: nothing to report.
        let silent = vec![0.0; 14 * RATE];
        let none = analyse(&out, &silent);
        assert_eq!(none.windows, 0);
        assert!(none.dominant.is_none());
    }

    fn seg(start: usize, lag: i64, windows: usize) -> Segment {
        Segment {
            start_s: start as f64 / RATE as f64,
            end_s: (start + windows * HOP) as f64 / RATE as f64,
            lag_frames: lag,
            mean_corr: 1.0,
            windows,
            span: (start, start + windows * HOP),
        }
    }

    #[test]
    fn jumps_are_next_minus_previous_in_ms() {
        let s = [
            seg(0, 441, 8),
            seg(RATE * 3, -441, 8),
            seg(RATE * 6, 441, 8),
        ];
        let j = jumps_ms(&s);
        assert_eq!(j.len(), 2);
        assert!(
            (j[0] + 20.0).abs() < 1e-9 && (j[1] - 20.0).abs() < 1e-9,
            "{j:?}"
        );
        assert_eq!(lag_at(&s, RATE * 3 + 10), -441);
        assert_eq!(lag_at(&s, RATE * 5), -441);
        assert_eq!(lag_at(&[], 5), 0);
    }

    #[test]
    fn segments_need_four_windows_and_tolerate_two_frames() {
        let w = |i: usize, lag: i64| WindowLag {
            start: i * HOP,
            lag,
            corr: 0.9,
        };
        // 3 windows then a jump: dropped. Then 5 windows drifting by 1 frame: kept.
        let ws = [
            w(0, 10),
            w(1, 10),
            w(2, 11),
            w(3, 500),
            w(4, 501),
            w(5, 502),
            w(6, 502),
            w(7, 501),
        ];
        let s = segments(&ws);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].windows, 5);
        assert_eq!(s[0].lag_frames, 501);
    }
}
