//! Passive probe of the process-loopback delivery offset ("split probe").
//!
//! Records, at the same time, the player's own audio (process loopback of its process tree)
//! and the system output (loopback of a render endpoint, by default the default one). Both are
//! opened at the endpoint's mix-format sample rate as float32 stereo, so the endpoint loopback
//! does no rate conversion. Every packet is stamped with the QPC time at which it was read
//! (`arrival_us`, taken right after `read_from_device` returns), its device QPC timestamp and
//! its position in the recording. The two recordings are aligned by content (not by the
//! unreliable absolute timestamps) and the report says how much later (positive) or earlier
//! (negative) the process loopback delivers a given piece of audio than the endpoint loopback.
//!
//! It only captures: it connects to no engine and never changes any session volume.
//!
//! `--small-period` additionally opens, on the same endpoint, a render stream that writes only
//! silence (`AUDCLNT_BUFFERFLAGS_SILENT`) at the minimum shared-mode engine period
//! (`IAudioClient3::InitializeSharedAudioStream`) for the length of the recording. That changes
//! the engine period of the WHOLE endpoint (every program playing to it), so the probe refuses
//! to run unless `--confirm-period-change` is given as well. The report lists the default,
//! fundamental, minimum and maximum periods and the period `GetCurrentSharedModeEnginePeriod`
//! says is in force.
//!
//! Reading the report (for the runbook): quote `deliveryOffsetUs`, which is corrected for the
//! frames that follow the matched one inside the output packet (a packet is read after its last
//! frame, so the raw difference `deliveryOffsetRawUs` is biased by up to one output packet);
//! the raw one is kept for comparison only. Only trust a run with `runValid: true`. It is false
//! (reasons in `invalidReasons`) when the content lag is not constant (`lagUnstable`: per-window
//! lag p05..p95 wider than 4 frames, e.g. a pause or a dropped packet on one stream), when
//! either stream reported data discontinuities, when no alignment was found, or when the silent
//! small-period stream died during the recording (`smallPeriod.endedEarly`, with the error and
//! the time). A failing capture stops the other capture and the silent stream at once.
//!
//! Usage:
//!   split_probe --pid <player root pid> --seconds <s> [--endpoint <render endpoint id>]
//!               [--small-period --confirm-period-change] [--out <json>] [--dump <prefix>]
//!   split_probe --analyze <prefix> [--out <json>]
//!
//! `--dump` writes the raw recordings (interleaved stereo f32 LE) to `<prefix>-source.f32` and
//! `<prefix>-output.f32`, the packet records to `<prefix>-packets.csv`
//! (`stream,arrival_us,device_qpc_100ns,first_frame,frames`; `first_frame` is the index of the
//! packet's first frame in the dumped recording, not the device position) and the sample rate
//! and small-period information to `<prefix>-meta.json`. `--analyze <prefix>` reads them back
//! and prints the same report without capturing (no `--pid`, no devices).
//!
//! The analysis is pure functions, unit-tested with synthetic signals (`cargo test --workspace`
//! runs them: the example is declared with `test = true`). The lag method (4096-frame windows
//! every 0.25 s, coarse search on 8x decimated audio, refined at full rate, windows below 0.5
//! correlation skipped) is that of `latency_probe`, rewritten here because examples cannot share
//! code.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Instant;

use serde_json::{json, Value};
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Lag search range of the content alignment.
const MAX_LAG_MS: u32 = 500;
const DECIMATE: usize = 8;
/// Alignment window length (frames); the hop is a quarter of a second.
const WINDOW: usize = 4096;
/// Windows that match worse than this are skipped (no usable alignment there).
const MIN_WINDOW_CORR: f64 = 0.5;
/// Peak amplitude at or below which a stretch counts as digital silence (-140 dB).
const SILENCE_PEAK: f32 = 1e-7;

/// One captured packet. `first_frame` is the index of its first frame in the recording
/// (interleaved-stereo sample index / 2), so packets of a stream are contiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PacketTime {
    /// QPC time (microseconds) at which the packet was read.
    arrival_us: u64,
    /// The device's QPC timestamp of the first frame, in 100 ns units.
    device_qpc_100ns: u64,
    first_frame: u64,
    frames: u32,
}

#[derive(Debug)]
struct Args {
    /// Capture mode: the player's root pid and the recording length.
    pid: Option<u32>,
    seconds: Option<f64>,
    endpoint: Option<String>,
    small_period: bool,
    confirm_period_change: bool,
    out: Option<String>,
    dump: Option<String>,
    /// Offline mode: analyse the files `--dump` wrote with this prefix.
    analyze: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        pid: None,
        seconds: None,
        endpoint: None,
        small_period: false,
        confirm_period_change: false,
        out: None,
        dump: None,
        analyze: None,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--pid" => a.pid = Some(value()?.parse().map_err(|e| format!("--pid: {e}"))?),
            "--seconds" => {
                a.seconds = Some(
                    value()?
                        .parse::<f64>()
                        .map_err(|e| format!("--seconds: {e}"))?,
                )
            }
            "--endpoint" => a.endpoint = Some(value()?),
            "--small-period" => a.small_period = true,
            "--confirm-period-change" => a.confirm_period_change = true,
            "--out" => a.out = Some(value()?),
            "--dump" => a.dump = Some(value()?),
            "--analyze" => a.analyze = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.analyze.is_some() {
        if a.pid.is_some()
            || a.seconds.is_some()
            || a.endpoint.is_some()
            || a.small_period
            || a.confirm_period_change
            || a.dump.is_some()
        {
            return Err("--analyze takes only --out".into());
        }
        return Ok(a);
    }
    if a.pid.is_none() {
        return Err("--pid <player root pid> is required".into());
    }
    let secs = a.seconds.ok_or("--seconds is required")?;
    if !(secs > 0.0 && secs <= 600.0) {
        return Err("--seconds must be in (0, 600]".into());
    }
    if a.small_period && !a.confirm_period_change {
        return Err(
            "--small-period opens a minimum-period stream, which changes the engine period of \
             the whole endpoint (every program playing to it); add --confirm-period-change to \
             accept that"
                .into(),
        );
    }
    Ok(a)
}

// ---------------------------------------------------------------------------------------------
// Pure analysis.
// ---------------------------------------------------------------------------------------------

/// Result of [`global_lag`]: `out[i] ~ src[i - frames]`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LagEstimate {
    frames: i64,
    /// Mean normalised correlation of the windows that voted.
    corr: f64,
    /// Number of windows that voted.
    windows: usize,
    /// Spread of the voting windows' own lags (frames): a constant lag has a spread of a frame
    /// or two; a pause or a dropped packet on one stream steps it by whole packets.
    lag_spread: Spread,
}

/// `p95 - p05` of the per-window lags above which the lag is not constant: 4 frames, twice the
/// +-2 frame tolerance `latency_probe` uses for "same lag", about 0.09 ms at 44.1 kHz. Real
/// content aligns within that (the fine search is exact to the frame); a dropped or inserted
/// packet shifts one recording by at least a packet (hundreds of frames), far above it. Using
/// the 5-95 percentile range lets the odd stray window (repeating music matching better at a
/// wrong lag) through, while `min`/`max` are reported too so a late short step is still visible.
const LAG_UNSTABLE_FRAMES: f64 = 4.0;

fn decimate(x: &[f32]) -> Vec<f32> {
    x.chunks_exact(DECIMATE)
        .map(|c| c.iter().sum::<f32>() / DECIMATE as f32)
        .collect()
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
    Some(if bb > 0.0 && a_norm > 0.0 {
        dot / (a_norm * bb.sqrt())
    } else {
        0.0
    })
}

/// Best lag (frames, `out[i] ~ src[i - lag]`) of the output window starting at `start` and its
/// normalised correlation; coarse search on the decimated signals, then a fine search of +-2
/// decimation steps at full rate.
fn window_lag(
    out: &[f32],
    src: &[f32],
    out8: &[f32],
    src8: &[f32],
    start: usize,
    max_lag: usize,
) -> Option<(i64, f64)> {
    let w8 = (start / DECIMATE, (start + WINDOW) / DECIMATE);
    let a8 = &out8[w8.0..w8.1];
    let n8 = energy(a8).sqrt();
    let reach8 = (max_lag / DECIMATE) as i64;
    let mut coarse: Option<(i64, f64)> = None;
    for d8 in -reach8..=reach8 {
        if let Some(r) = cosine_at(a8, n8, src8, w8.0 as i64 - d8) {
            if coarse.is_none_or(|(_, best)| r > best) {
                coarse = Some((d8, r));
            }
        }
    }
    let centre = coarse?.0 * DECIMATE as i64;
    let a = &out[start..start + WINDOW];
    let n = energy(a).sqrt();
    let reach = 2 * DECIMATE as i64;
    let mut fine: Option<(i64, f64)> = None;
    for d in (centre - reach).max(-(max_lag as i64))..=(centre + reach).min(max_lag as i64) {
        if let Some(r) = cosine_at(a, n, src, start as i64 - d) {
            if fine.is_none_or(|(_, best)| r > best) {
                fine = Some((d, r));
            }
        }
    }
    fine
}

/// Content lag of `out` behind `src` (both mono, same sample rate): positive when `out` is the
/// later copy. Windows of 4096 frames every 0.25 s, each searched over +-`max_lag_ms`; only
/// windows with correlation >= 0.5 (and neither side digital silence) vote, and the median of
/// their lags is returned. `None` when no window qualifies.
fn global_lag(out: &[f32], src: &[f32], rate: u32, max_lag_ms: u32) -> Option<LagEstimate> {
    let max_lag = max_lag_ms as usize * rate as usize / 1000;
    let hop = (rate as usize / 4).max(1);
    let out8 = decimate(out);
    let src8 = decimate(src);
    let mut votes: Vec<(i64, f64)> = Vec::new();
    // Start at max_lag so that every candidate lag has source audio before the window.
    let mut start = max_lag;
    while start + WINDOW <= out.len() && start + WINDOW + max_lag <= src.len() {
        let silent = peak(&out[start..start + WINDOW]) <= SILENCE_PEAK
            || peak(&src[start - max_lag..start + WINDOW + max_lag]) <= SILENCE_PEAK;
        if !silent {
            if let Some((lag, corr)) = window_lag(out, src, &out8, &src8, start, max_lag) {
                if corr >= MIN_WINDOW_CORR {
                    votes.push((lag, corr));
                }
            }
        }
        start += hop;
    }
    if votes.is_empty() {
        return None;
    }
    let mut lags: Vec<i64> = votes.iter().map(|v| v.0).collect();
    lags.sort_unstable();
    let all: Vec<f64> = lags.iter().map(|&l| l as f64).collect();
    Some(LagEstimate {
        frames: lags[lags.len() / 2],
        corr: votes.iter().map(|v| v.1).sum::<f64>() / votes.len() as f64,
        windows: votes.len(),
        lag_spread: spread(&all)?,
    })
}

/// One source packet matched with the output packet holding the same content.
struct Match {
    /// Source arrival minus output arrival, microseconds (raw).
    raw_us: i64,
    /// Frames of the output packet that come after the matched frame `y`.
    out_tail_frames: u64,
}

fn match_packets(src: &[PacketTime], out: &[PacketTime], lag_frames: i64) -> Vec<Match> {
    let mut v = Vec::with_capacity(src.len());
    for s in src {
        if s.frames == 0 {
            continue;
        }
        let x = s.first_frame as i64 + i64::from(s.frames) - 1;
        let y = x + lag_frames;
        if y < 0 {
            continue;
        }
        let y = y as u64;
        // Packets are ordered by first_frame: the one holding y is the last that starts at or
        // before it.
        let idx = out.partition_point(|p| p.first_frame <= y);
        if idx == 0 {
            continue;
        }
        let p = &out[idx - 1];
        let end = p.first_frame + u64::from(p.frames);
        if y >= end {
            continue;
        }
        v.push(Match {
            raw_us: s.arrival_us as i64 - p.arrival_us as i64,
            out_tail_frames: end - 1 - y,
        });
    }
    v
}

/// For every source packet: the QPC arrival time of the source packet holding its last frame
/// `x` minus the arrival time of the output packet holding the same content, output frame
/// `y = x + lag_frames`. Positive: the process loopback (source) delivered later. Source
/// packets whose `y` is outside the output are skipped. RAW: see
/// [`delivery_offsets_corrected_us`] for the one to use.
fn delivery_offsets_us(src: &[PacketTime], out: &[PacketTime], lag_frames: i64) -> Vec<i64> {
    match_packets(src, out, lag_frames)
        .into_iter()
        .map(|m| m.raw_us)
        .collect()
}

/// The raw offsets corrected for the frames that follow `y` in its output packet. A packet is
/// read after its LAST frame was captured, so an output packet arriving at `t` delivers `y`
/// already `(last - y) / rate` old; the source side needs no term because `x` is the last
/// frame of its packet. Corrected = raw + `(last - y) / rate`, the offset between the two
/// deliveries of the same instant of audio. This is the one to quote (reported as
/// `deliveryOffsetUs`; the raw one is `deliveryOffsetRawUs`).
fn delivery_offsets_corrected_us(
    src: &[PacketTime],
    out: &[PacketTime],
    lag_frames: i64,
    rate: u32,
) -> Vec<f64> {
    match_packets(src, out, lag_frames)
        .into_iter()
        .map(|m| m.raw_us as f64 + m.out_tail_frames as f64 * 1e6 / f64::from(rate))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Spread {
    n: usize,
    median: f64,
    p05: f64,
    p95: f64,
    min: f64,
    max: f64,
}

/// Percentile `q` in [0, 1] of sorted values, linear interpolation between ranks.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    let pos = q * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

fn spread(values: &[f64]) -> Option<Spread> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(Spread {
        n: v.len(),
        median: percentile(&v, 0.5),
        p05: percentile(&v, 0.05),
        p95: percentile(&v, 0.95),
        min: v[0],
        max: v[v.len() - 1],
    })
}

fn spread_json(s: Option<Spread>) -> Value {
    match s {
        Some(s) => json!({
            "n": s.n, "median": s.median, "p05": s.p05, "p95": s.p95, "min": s.min, "max": s.max,
        }),
        None => Value::Null,
    }
}

/// Packet-size histogram (frames -> count) and the median and 95th percentile of the gaps
/// between consecutive arrivals (microseconds).
fn packet_stats(p: &[PacketTime]) -> Value {
    let mut sizes: BTreeMap<String, usize> = BTreeMap::new();
    for pk in p {
        *sizes.entry(pk.frames.to_string()).or_default() += 1;
    }
    let gaps: Vec<f64> = p
        .windows(2)
        .map(|w| w[1].arrival_us as f64 - w[0].arrival_us as f64)
        .collect();
    let g = spread(&gaps);
    json!({
        "packets": p.len(),
        "frames": p.iter().map(|k| u64::from(k.frames)).sum::<u64>(),
        "sizeHistogram": sizes,
        "arrivalGapUsMedian": g.map(|s| s.median),
        "arrivalGapUsP95": g.map(|s| s.p95),
    })
}

fn mono(stereo: &[f32]) -> Vec<f32> {
    stereo
        .chunks_exact(2)
        .map(|f| 0.5 * (f[0] + f[1]))
        .collect()
}

/// Reasons the run cannot be trusted (empty: it can). Pure: `small_period` is the report's
/// `smallPeriod` value, `discontinuities` the data-discontinuity counts of (source, output).
fn invalid_reasons(
    lag: Option<LagEstimate>,
    discontinuities: (usize, usize),
    small_period: &Value,
) -> Vec<String> {
    let mut r = Vec::new();
    match lag {
        None => r.push("no content alignment between the two recordings".to_string()),
        Some(l) => {
            let width = l.lag_spread.p95 - l.lag_spread.p05;
            if width > LAG_UNSTABLE_FRAMES {
                r.push(format!(
                    "lag not constant: per-window lag p05..p95 spans {width:.1} frames \
                     (limit {LAG_UNSTABLE_FRAMES})"
                ));
            }
        }
    }
    if discontinuities.0 + discontinuities.1 > 0 {
        r.push(format!(
            "data discontinuities: source {}, output {} (a stream lost frames)",
            discontinuities.0, discontinuities.1
        ));
    }
    if small_period["endedEarly"] == json!(true) {
        r.push(format!(
            "the silent small-period stream ended early: {}",
            small_period["endedEarlyError"]
                .as_str()
                .unwrap_or("unknown")
        ));
    }
    r
}

/// Marks the small-period report with whether the silent stream ran for the whole recording.
/// `failure` is its error text and the time (seconds after the stream started) it ended at.
fn finish_small_period(mut info: Value, failure: Option<(String, f64)>) -> Value {
    if info.is_null() {
        return info;
    }
    info["endedEarly"] = json!(failure.is_some());
    if let Some((err, at_s)) = failure {
        info["endedEarlyError"] = json!(err);
        info["endedEarlyAfterS"] = json!(at_s);
    }
    info
}

/// The report for two stereo recordings and their packet records. `discontinuities` are the
/// (source, output) data-discontinuity counts (0 when unknown).
fn report(
    rate: u32,
    src: &[f32],
    out: &[f32],
    src_packets: &[PacketTime],
    out_packets: &[PacketTime],
    small_period: Value,
    discontinuities: (usize, usize),
) -> Value {
    let lag = global_lag(&mono(out), &mono(src), rate, MAX_LAG_MS);
    let corrected =
        lag.map(|l| delivery_offsets_corrected_us(src_packets, out_packets, l.frames, rate));
    let raw = lag.map(|l| {
        delivery_offsets_us(src_packets, out_packets, l.frames)
            .into_iter()
            .map(|o| o as f64)
            .collect::<Vec<f64>>()
    });
    let reasons = invalid_reasons(lag, discontinuities, &small_period);
    let lag_unstable = lag
        .is_some_and(|l| l.lag_spread.p95 - l.lag_spread.p05 > LAG_UNSTABLE_FRAMES)
        || discontinuities.0 + discontinuities.1 > 0;
    json!({
        "rate": rate,
        "lagFrames": lag.map(|l| l.frames),
        "lagMs": lag.map(|l| l.frames as f64 * 1000.0 / f64::from(rate)),
        "lagCorr": lag.map(|l| l.corr),
        "lagWindows": lag.map(|l| l.windows),
        "lagSpreadFrames": lag.map(|l| spread_json(Some(l.lag_spread))),
        "lagUnstable": lag_unstable,
        "sourceDiscontinuities": discontinuities.0,
        "outputDiscontinuities": discontinuities.1,
        // Corrected for the frames after the matched one in the output packet: quote this.
        "deliveryOffsetUs": spread_json(corrected.and_then(|o| spread(&o))),
        "deliveryOffsetRawUs": spread_json(raw.and_then(|o| spread(&o))),
        "sourcePackets": packet_stats(src_packets),
        "outputPackets": packet_stats(out_packets),
        "smallPeriod": small_period,
        "runValid": reasons.is_empty(),
        "invalidReasons": reasons,
    })
}

// ---------------------------------------------------------------------------------------------
// Capture.
// ---------------------------------------------------------------------------------------------

/// Microseconds on the QueryPerformanceCounter clock (shared by all threads).
fn now_us() -> u64 {
    static FREQ: OnceLock<u64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1) as u64
    });
    let mut c = 0i64;
    let _ = unsafe { QueryPerformanceCounter(&mut c) };
    (c.max(0) as u128 * 1_000_000 / u128::from(freq)) as u64
}

/// Flags shared by the capture threads and the silent stream.
#[derive(Default)]
struct Control {
    /// Set by a capture that failed: the other capture and the silent stream stop at once.
    abort: AtomicBool,
    /// Set by the main thread when the recording is over.
    stop: AtomicBool,
    /// Why (and when, QPC microseconds) the silent stream died after it had started.
    silent_failure: Mutex<Option<(String, u64)>>,
}

enum Source {
    Process(u32),
    Endpoint(Option<String>),
}

struct Recording {
    /// Interleaved stereo.
    samples: Vec<f32>,
    packets: Vec<PacketTime>,
    discontinuities: usize,
    timestamp_errors: usize,
    silent_packets: usize,
}

fn open_endpoint_client(id: &Option<String>) -> Result<AudioClient, String> {
    let en = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
    let dev = match id {
        Some(id) => en.get_device(id),
        None => en.get_default_device(&Direction::Render),
    }
    .map_err(|e| format!("render endpoint: {e}"))?;
    dev.get_iaudioclient()
        .map_err(|e| format!("endpoint client: {e}"))
}

/// The endpoint's shared-mode mix-format sample rate.
fn endpoint_rate(id: &Option<String>) -> Result<u32, String> {
    wasapi::initialize_mta()
        .ok()
        .map_err(|e| format!("COM: {e}"))?;
    let result = (|| {
        let client = open_endpoint_client(id)?;
        let fmt = client
            .get_mixformat()
            .map_err(|e| format!("mix format: {e}"))?;
        Ok(fmt.get_samplespersec())
    })();
    wasapi::deinitialize();
    result
}

/// Records for `seconds` or until another capture fails; sets `abort` if this one fails.
fn record(src: Source, seconds: f64, rate: u32, ctl: &Control) -> Result<Recording, String> {
    let r = record_inner(src, seconds, rate, ctl);
    if r.is_err() {
        ctl.abort.store(true, Ordering::SeqCst);
    }
    r
}

fn record_inner(src: Source, seconds: f64, rate: u32, ctl: &Control) -> Result<Recording, String> {
    wasapi::initialize_mta()
        .ok()
        .map_err(|e| format!("COM: {e}"))?;
    let result = (|| {
        let mut client = match &src {
            Source::Process(pid) => AudioClient::new_application_loopback_client(*pid, true)
                .map_err(|e| format!("process loopback {pid}: {e}"))?,
            Source::Endpoint(id) => open_endpoint_client(id)?,
        };
        let fmt = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, 2, None);
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
            samples: Vec::with_capacity((seconds * f64::from(rate)) as usize * 2 + rate as usize),
            packets: Vec::new(),
            discontinuities: 0,
            timestamp_errors: 0,
            silent_packets: 0,
        };
        let mut bytes = vec![0u8; rate as usize * 8];
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < seconds && !ctl.abort.load(Ordering::SeqCst) {
            let _ = event.wait_for_event(50);
            loop {
                let frames = cap.get_next_packet_size().map_err(|e| e.to_string())?;
                if frames.unwrap_or(0) == 0 {
                    break;
                }
                let (n, info) = cap
                    .read_from_device(&mut bytes)
                    .map_err(|e| e.to_string())?;
                let arrival_us = now_us();
                if n == 0 {
                    continue;
                }
                rec.packets.push(PacketTime {
                    arrival_us,
                    device_qpc_100ns: info.timestamp,
                    first_frame: (rec.samples.len() / 2) as u64,
                    frames: n,
                });
                if info.flags.timestamp_error {
                    rec.timestamp_errors += 1;
                }
                if info.flags.silent {
                    rec.silent_packets += 1;
                }
                if info.flags.data_discontinuity && rec.packets.len() > 1 {
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

// ---------------------------------------------------------------------------------------------
// Silent minimum-period render stream (--small-period).
// ---------------------------------------------------------------------------------------------

unsafe fn read_period_info(client: &IAudioClient3, fmt: *const WAVEFORMATEX) -> Value {
    let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
    let r =
        client.GetSharedModeEnginePeriod(fmt, &mut default, &mut fundamental, &mut min, &mut max);
    match r {
        Ok(()) => json!({
            "defaultFrames": default, "fundamentalFrames": fundamental,
            "minFrames": min, "maxFrames": max,
        }),
        Err(e) => json!({ "error": format!("GetSharedModeEnginePeriod: {e}") }),
    }
}

/// Opens the silent render stream and feeds it until `stop` is set. Sends the period report (or
/// the error) on `ready` once the stream runs.
fn silent_stream(
    endpoint: Option<String>,
    ctl: Arc<Control>,
    ready: mpsc::Sender<Result<Value, String>>,
) {
    wasapi::initialize_mta().ok().ok();
    // An error here happened before `ready` was sent (later ones go to `ctl.silent_failure`).
    if let Err(e) = silent_stream_inner(&endpoint, &ctl, &ready) {
        let _ = ready.send(Err(e));
    }
    wasapi::deinitialize();
}

fn silent_stream_inner(
    endpoint: &Option<String>,
    ctl: &Control,
    ready: &mpsc::Sender<Result<Value, String>>,
) -> Result<(), String> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("device enumerator: {e}"))?;
        let dev: IMMDevice = match endpoint {
            Some(id) => en
                .GetDevice(&HSTRING::from(id.as_str()))
                .map_err(|e| format!("endpoint {id}: {e}"))?,
            None => en
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("default render endpoint: {e}"))?,
        };
        let client: IAudioClient3 = dev
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("IAudioClient3 unavailable on this endpoint: {e}"))?;
        let fmt = client
            .GetMixFormat()
            .map_err(|e| format!("mix format: {e}"))?;
        let mut info = read_period_info(&client, fmt);
        // `fmt` is freed on every path, including the one without period info.
        let init = match info["minFrames"].as_u64() {
            Some(min) => client
                .InitializeSharedAudioStream(
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    min as u32,
                    fmt,
                    None,
                )
                .map_err(|e| format!("InitializeSharedAudioStream({min} frames): {e}"))
                .map(|()| min as u32),
            None => Err(info["error"]
                .as_str()
                .unwrap_or("no period info")
                .to_string()),
        };
        CoTaskMemFree(Some(fmt.cast()));
        let min = init?;
        let event: HANDLE = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| format!("CreateEventW: {e}"))?;
        let result = (|| {
            client
                .SetEventHandle(event)
                .map_err(|e| format!("render event: {e}"))?;
            let render: IAudioRenderClient = client
                .GetService()
                .map_err(|e| format!("render client: {e}"))?;
            let buffer = client
                .GetBufferSize()
                .map_err(|e| format!("buffer size: {e}"))?;
            let silence = |n: u32| -> Result<(), String> {
                if n == 0 {
                    return Ok(());
                }
                render
                    .GetBuffer(n)
                    .map_err(|e| format!("render GetBuffer: {e}"))?;
                render
                    .ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                    .map_err(|e| format!("render ReleaseBuffer: {e}"))
            };
            silence(buffer)?;
            client.Start().map_err(|e| format!("start render: {e}"))?;
            // The period actually in force, once the stream runs.
            let mut cur_fmt: *mut WAVEFORMATEX = std::ptr::null_mut();
            let mut current = 0u32;
            match client.GetCurrentSharedModeEnginePeriod(&mut cur_fmt, &mut current) {
                Ok(()) => {
                    if !cur_fmt.is_null() {
                        let hz = std::ptr::addr_of!((*cur_fmt).nSamplesPerSec).read_unaligned();
                        info["currentPeriodSampleRate"] = json!(hz);
                        CoTaskMemFree(Some(cur_fmt.cast()));
                    }
                    info["currentFrames"] = json!(current);
                }
                Err(e) => {
                    info["currentError"] = json!(format!("GetCurrentSharedModeEnginePeriod: {e}"))
                }
            }
            info["requestedFrames"] = json!(min);
            info["bufferFrames"] = json!(buffer);
            let _ = ready.send(Ok(info));
            // From here on a failure cannot reach `ready`: it is recorded for the report
            // (the small period no longer held), and the stream ends.
            let feed = || -> Result<(), String> {
                while !ctl.stop.load(Ordering::SeqCst) && !ctl.abort.load(Ordering::SeqCst) {
                    WaitForSingleObject(event, 100);
                    let padding = client
                        .GetCurrentPadding()
                        .map_err(|e| format!("render padding: {e}"))?;
                    silence(buffer.saturating_sub(padding))?;
                }
                Ok(())
            };
            if let Err(e) = feed() {
                if let Ok(mut f) = ctl.silent_failure.lock() {
                    *f = Some((e, now_us()));
                }
            }
            let _ = client.Stop();
            drop(render);
            Ok(())
        })();
        drop(client);
        let _ = CloseHandle(event);
        result
    }
}

// ---------------------------------------------------------------------------------------------
// Files.
// ---------------------------------------------------------------------------------------------

fn packets_csv(src: &[PacketTime], out: &[PacketTime]) -> String {
    let mut s = String::from("stream,arrival_us,device_qpc_100ns,first_frame,frames\n");
    for (name, packets) in [("source", src), ("output", out)] {
        for p in packets {
            s.push_str(&format!(
                "{name},{},{},{},{}\n",
                p.arrival_us, p.device_qpc_100ns, p.first_frame, p.frames
            ));
        }
    }
    s
}

fn parse_packets_csv(text: &str) -> Result<(Vec<PacketTime>, Vec<PacketTime>), String> {
    let (mut src, mut out) = (Vec::new(), Vec::new());
    for (n, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        if f.len() != 5 {
            return Err(format!("packets.csv line {}: expected 5 fields", n + 1));
        }
        let num = |i: usize| {
            f[i].trim()
                .parse::<u64>()
                .map_err(|e| format!("packets.csv line {}: {e}", n + 1))
        };
        let p = PacketTime {
            arrival_us: num(1)?,
            device_qpc_100ns: num(2)?,
            first_frame: num(3)?,
            frames: u32::try_from(num(4)?)
                .map_err(|e| format!("packets.csv line {}: {e}", n + 1))?,
        };
        match f[0] {
            "source" => src.push(p),
            "output" => out.push(p),
            other => return Err(format!("packets.csv line {}: stream {other}", n + 1)),
        }
    }
    Ok((src, out))
}

fn write_f32(path: &str, samples: &[f32]) -> Result<(), String> {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(path, bytes).map_err(|e| format!("write {path}: {e}"))
}

/// Reads interleaved stereo f32 LE as written by `--dump`.
fn read_f32(path: &str) -> Result<Vec<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

fn dump(
    prefix: &str,
    rate: u32,
    src: &Recording,
    out: &Recording,
    small_period: &Value,
) -> Result<(), String> {
    write_f32(&format!("{prefix}-source.f32"), &src.samples)?;
    write_f32(&format!("{prefix}-output.f32"), &out.samples)?;
    let path = format!("{prefix}-packets.csv");
    std::fs::write(&path, packets_csv(&src.packets, &out.packets))
        .map_err(|e| format!("write {path}: {e}"))?;
    let meta = json!({
        "rate": rate,
        "smallPeriod": small_period,
        "sourceDiscontinuities": src.discontinuities,
        "outputDiscontinuities": out.discontinuities,
    });
    let path = format!("{prefix}-meta.json");
    std::fs::write(&path, meta.to_string()).map_err(|e| format!("write {path}: {e}"))
}

fn emit(report: Value, out: &Option<String>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    println!("{text}");
    if let Some(path) = out {
        std::fs::write(path, &text).map_err(|e| format!("write {path}: {e}"))?;
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("split_probe: {e}");
        std::process::exit(1);
    }
}

fn analyze(prefix: &str, out_path: &Option<String>) -> Result<(), String> {
    let meta_path = format!("{prefix}-meta.json");
    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(&meta_path).map_err(|e| format!("read {meta_path}: {e}"))?,
    )
    .map_err(|e| format!("{meta_path}: {e}"))?;
    let rate = meta["rate"]
        .as_u64()
        .and_then(|r| u32::try_from(r).ok())
        .filter(|&r| r > 0)
        .ok_or_else(|| format!("{meta_path}: no usable rate"))?;
    let src = read_f32(&format!("{prefix}-source.f32"))?;
    let out = read_f32(&format!("{prefix}-output.f32"))?;
    let csv_path = format!("{prefix}-packets.csv");
    let (sp, op) = parse_packets_csv(
        &std::fs::read_to_string(&csv_path).map_err(|e| format!("read {csv_path}: {e}"))?,
    )?;
    let count = |k: &str| meta[k].as_u64().unwrap_or(0) as usize;
    let mut r = report(
        rate,
        &src,
        &out,
        &sp,
        &op,
        meta["smallPeriod"].clone(),
        (
            count("sourceDiscontinuities"),
            count("outputDiscontinuities"),
        ),
    );
    r["analyzed"] = json!(prefix);
    emit(r, out_path)
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv)?;
    if let Some(prefix) = &args.analyze {
        return analyze(prefix, &args.out);
    }
    let pid = args.pid.ok_or("--pid is required")?;
    let secs = args.seconds.ok_or("--seconds is required")?;
    let rate = endpoint_rate(&args.endpoint)?;

    // The silent stream first, so that the whole recording runs at the small period.
    let ctl = Arc::new(Control::default());
    let mut small: Option<thread::JoinHandle<()>> = None;
    let mut small_info = Value::Null;
    let mut small_started_us = 0;
    if args.small_period {
        let (tx, rx) = mpsc::channel();
        let (ep, c) = (args.endpoint.clone(), ctl.clone());
        small = Some(thread::spawn(move || silent_stream(ep, c, tx)));
        match rx.recv() {
            Ok(Ok(info)) => {
                small_info = info;
                small_started_us = now_us();
            }
            Ok(Err(e)) => {
                if let Some(h) = small.take() {
                    let _ = h.join();
                }
                return Err(format!("--small-period: {e}"));
            }
            Err(_) => return Err("--small-period: stream thread ended unexpectedly".into()),
        }
    }

    let (endpoint, c1, c2) = (args.endpoint.clone(), ctl.clone(), ctl.clone());
    let src = thread::spawn(move || record(Source::Process(pid), secs, rate, &c1));
    let out = thread::spawn(move || record(Source::Endpoint(endpoint), secs, rate, &c2));
    let src = src.join();
    let out = out.join();
    // Whatever happened, the endpoint must not stay at the small period: stop the stream and
    // wait for it before reporting anything.
    ctl.stop.store(true, Ordering::SeqCst);
    let mut small_failure = None;
    if let Some(h) = small {
        if h.join().is_err() {
            small_failure = Some(("the stream thread panicked".to_string(), now_us()));
        }
        if let Ok(mut f) = ctl.silent_failure.lock() {
            if let Some(e) = f.take() {
                small_failure = Some(e);
            }
        }
    }
    let small_info = finish_small_period(
        small_info,
        small_failure.map(|(e, at)| (e, at.saturating_sub(small_started_us) as f64 / 1e6)),
    );
    let src = src.map_err(|_| "source capture panicked")??;
    let out = out.map_err(|_| "output capture panicked")??;

    if let Some(prefix) = &args.dump {
        dump(prefix, rate, &src, &out, &small_info)?;
    }
    if src.samples.len() < rate as usize * 2 || out.samples.len() < rate as usize * 2 {
        return Err("less than 1 s recorded; is the player playing?".into());
    }
    let mut r = report(
        rate,
        &src.samples,
        &out.samples,
        &src.packets,
        &out.packets,
        small_info,
        (src.discontinuities, out.discontinuities),
    );
    if r["runValid"] == json!(false) {
        eprintln!(
            "split_probe: RUN NOT VALID: {}",
            r["invalidReasons"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("; "))
                .unwrap_or_default()
        );
    }
    r["pid"] = json!(pid);
    r["seconds"] = json!(secs);
    r["endpoint"] = json!(args.endpoint);
    r["sourceTimestampErrors"] = json!(src.timestamp_errors);
    r["outputTimestampErrors"] = json!(out.timestamp_errors);
    r["sourceSilentPackets"] = json!(src.silent_packets);
    r["outputSilentPackets"] = json!(out.silent_packets);
    emit(r, &args.out)
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

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Contiguous packets of `size` frames covering `total` frames, packet k read at
    /// `arrival(k)`.
    fn packets(total: u64, size: u32, arrival: impl Fn(u64) -> u64) -> Vec<PacketTime> {
        (0..total / u64::from(size))
            .map(|k| PacketTime {
                arrival_us: arrival(k),
                device_qpc_100ns: arrival(k) * 10,
                first_frame: k * u64::from(size),
                frames: size,
            })
            .collect()
    }

    #[test]
    fn global_lag_finds_a_known_offset() {
        let rate = 48_000u32;
        let src = noise(6 * rate as usize, 9);
        let lag = 1234usize;
        let out: Vec<f32> = (0..src.len())
            .map(|i| if i < lag { 0.0 } else { src[i - lag] })
            .collect();
        let e = global_lag(&out, &src, rate, 500).expect("a lag");
        assert!((1233..=1235).contains(&e.frames), "{e:?}");
        assert!(e.corr > 0.99, "{e:?}");
        assert!(e.windows > 5, "{e:?}");
    }

    #[test]
    fn global_lag_of_unrelated_noise_is_none() {
        let rate = 48_000u32;
        let a = noise(6 * rate as usize, 1);
        let b = noise(6 * rate as usize, 2);
        assert_eq!(global_lag(&a, &b, rate, 500), None);
        assert_eq!(global_lag(&a, &vec![0.0; a.len()], rate, 500), None);
    }

    #[test]
    fn late_source_packets_give_a_positive_offset() {
        let rate = 48_000u64;
        let total = 5 * rate;
        // Output: 480-frame packets every 10 ms.
        let out = packets(total, 480, |k| k * 10_000);
        // Source: 441-frame packets with the same content (lag 0), each arriving 13 ms after
        // the output packet that holds its last frame.
        let src: Vec<PacketTime> = packets(total, 441, |_| 0)
            .into_iter()
            .map(|mut p| {
                let last = p.first_frame + u64::from(p.frames) - 1;
                p.arrival_us = last / 480 * 10_000 + 13_000;
                p
            })
            .collect();
        let offsets = delivery_offsets_us(&src, &out, 0);
        assert!(!offsets.is_empty());
        let s = spread(&offsets.iter().map(|&o| o as f64).collect::<Vec<_>>()).unwrap();
        assert!((12_999.0..=13_001.0).contains(&s.median), "{s:?}");
    }

    #[test]
    fn lag_shifts_the_matched_output_frame() {
        // The output is the source delayed by 480 frames, and was read at the same time as the
        // source packet carrying that content: offset 0 only when the lag is applied.
        let src = packets(48_000, 480, |k| k * 10_000);
        let out = packets(48_000, 480, |k| k * 10_000 + 10_000);
        let with = delivery_offsets_us(&src, &out, 480);
        // src packet k holds x = 480k+479; y = 480(k+1)+479 is in out packet k+1, read at
        // (k+1)*10_000 + 10_000.
        assert!(with.iter().all(|&o| o == -20_000), "{with:?}");
        let without = delivery_offsets_us(&src, &out, 0);
        assert!(without.iter().all(|&o| o == -10_000), "{without:?}");
    }

    #[test]
    fn offsets_skip_frames_outside_the_output() {
        let src = packets(48_000, 480, |k| k * 10_000);
        // Output covers only the first 100 packets' worth of frames.
        let out = packets(48_000 / 2, 480, |k| k * 10_000);
        let o = delivery_offsets_us(&src, &out, 0);
        assert_eq!(o.len(), 50);
        // A positive lag pushes the last source packets out of range too.
        let o = delivery_offsets_us(&src, &out, 480);
        assert_eq!(o.len(), 49);
        // A negative lag: the first packets point before the start of the output (skipped),
        // later ones now reach it (k = 2..=51).
        let o = delivery_offsets_us(&src, &out, -1000);
        assert_eq!(o.len(), 50);
        assert!(delivery_offsets_us(&src, &[], 0).is_empty());
    }

    #[test]
    fn spread_reports_percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = spread(&v).unwrap();
        assert_eq!(s.n, 100);
        assert!((s.median - 50.5).abs() < 1e-9, "{s:?}");
        assert!((s.p05 - 5.95).abs() < 1e-9, "{s:?}");
        assert!((s.p95 - 95.05).abs() < 1e-9, "{s:?}");
        assert_eq!((s.min, s.max), (1.0, 100.0));
        assert_eq!(spread(&[]), None);
        let one = spread(&[7.0]).unwrap();
        assert_eq!((one.median, one.p05, one.max), (7.0, 7.0, 7.0));
    }

    #[test]
    fn packet_stats_has_histogram_and_gaps() {
        let mut p = packets(4_410, 441, |k| k * 10_000);
        p.push(PacketTime {
            arrival_us: 100_000 + 500_000,
            device_qpc_100ns: 0,
            first_frame: 4_410,
            frames: 100,
        });
        let s = packet_stats(&p);
        assert_eq!(s["packets"], 11);
        assert_eq!(s["sizeHistogram"]["441"], 10);
        assert_eq!(s["sizeHistogram"]["100"], 1);
        assert_eq!(s["arrivalGapUsMedian"].as_f64(), Some(10_000.0));
        assert!(s["arrivalGapUsP95"].as_f64().unwrap() > 10_000.0);
        assert!(packet_stats(&[])["arrivalGapUsMedian"].is_null());
    }

    #[test]
    fn small_period_needs_explicit_confirmation() {
        let base = ["--pid", "42", "--seconds", "10"];
        let with = |extra: &[&str]| {
            let mut a: Vec<&str> = base.to_vec();
            a.extend_from_slice(extra);
            parse_args(&strs(&a))
        };
        assert!(with(&["--small-period"]).is_err());
        let ok = with(&["--small-period", "--confirm-period-change"]).unwrap();
        assert!(ok.small_period && ok.confirm_period_change);
        let plain = with(&[]).unwrap();
        assert!(!plain.small_period);
        assert_eq!(plain.pid, Some(42));
        assert!(parse_args(&strs(&["--analyze", "p", "--pid", "1"])).is_err());
        assert!(parse_args(&strs(&["--analyze", "p", "--small-period"])).is_err());
        let a = parse_args(&strs(&["--analyze", "p", "--out", "o.json"])).unwrap();
        assert_eq!(
            (a.analyze.as_deref(), a.out.as_deref()),
            (Some("p"), Some("o.json"))
        );
        assert!(parse_args(&strs(&["--seconds", "10"])).is_err());
        assert!(parse_args(&strs(&["--pid", "1", "--seconds", "0"])).is_err());
        assert!(parse_args(&strs(&["--pid", "1"])).is_err());
    }

    #[test]
    fn packets_csv_round_trips() {
        let s = packets(4_410, 441, |k| k * 10_000);
        let o = packets(4_800, 480, |k| k * 10_000 + 5);
        let (s2, o2) = parse_packets_csv(&packets_csv(&s, &o)).unwrap();
        assert_eq!((s, o), (s2, o2));
        assert!(parse_packets_csv("h\nsource,1,2,3\n").is_err());
        assert!(parse_packets_csv("h\nother,1,2,3,4\n").is_err());
    }

    #[test]
    fn report_has_the_documented_keys() {
        let rate = 48_000u32;
        let src_m = noise(6 * rate as usize, 4);
        let stereo = |m: &[f32]| m.iter().flat_map(|&s| [s, s]).collect::<Vec<f32>>();
        let lag = 960usize;
        let out_m: Vec<f32> = (0..src_m.len())
            .map(|i| if i < lag { 0.0 } else { src_m[i - lag] })
            .collect();
        let sp = packets(src_m.len() as u64, 441, |k| k * 9_187 + 3_000);
        let op = packets(src_m.len() as u64, 480, |k| k * 10_000);
        let r = report(
            rate,
            &stereo(&src_m),
            &stereo(&out_m),
            &sp,
            &op,
            Value::Null,
            (0, 0),
        );
        assert_eq!(r["rate"], 48_000);
        assert_eq!(r["lagFrames"], 960);
        assert!(r["lagCorr"].as_f64().unwrap() > 0.99);
        assert!(r["deliveryOffsetUs"]["n"].as_u64().unwrap() > 100);
        assert_eq!(r["sourcePackets"]["packets"], sp.len());
        assert_eq!(r["outputPackets"]["packets"], op.len());
        assert!(r["smallPeriod"].is_null());
        for key in [
            "lagSpreadFrames",
            "lagUnstable",
            "deliveryOffsetRawUs",
            "runValid",
            "invalidReasons",
        ] {
            assert!(!r[key].is_null(), "{key}");
        }
        assert_eq!(r["lagUnstable"], false);
        assert_eq!(r["runValid"], true, "{}", r["invalidReasons"]);
    }

    /// 10 s of noise; the output follows it at `lag1`, and from `switch` frames on at `lag2`.
    fn stepped(rate: u32, switch: usize, lag1: usize, lag2: usize) -> (Vec<f32>, Vec<f32>) {
        let src = noise(10 * rate as usize, 21);
        let out = (0..src.len())
            .map(|i| {
                let l = if i < switch { lag1 } else { lag2 };
                if i < l {
                    0.0
                } else {
                    src[i - l]
                }
            })
            .collect();
        (src, out)
    }

    #[test]
    fn a_constant_lag_is_stable() {
        let rate = 48_000;
        let (src, out) = stepped(rate, 0, 1234, 1234);
        let e = global_lag(&out, &src, rate, 500).unwrap();
        assert!(
            e.lag_spread.p95 - e.lag_spread.p05 <= LAG_UNSTABLE_FRAMES,
            "{e:?}"
        );
        assert!(invalid_reasons(Some(e), (0, 0), &Value::Null).is_empty());
    }

    #[test]
    fn a_step_in_the_lag_makes_the_run_invalid() {
        // A dropped packet / a pause: from 5 s on the output is 480 frames later.
        let rate = 48_000;
        let (src, out) = stepped(rate, 5 * rate as usize, 1234, 1234 + 480);
        let e = global_lag(&out, &src, rate, 500).unwrap();
        assert!(e.lag_spread.max - e.lag_spread.min >= 400.0, "{e:?}");
        let reasons = invalid_reasons(Some(e), (0, 0), &Value::Null);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("lag not constant"));
        let r = report(
            rate,
            &src.iter().flat_map(|&s| [s, s]).collect::<Vec<_>>(),
            &out.iter().flat_map(|&s| [s, s]).collect::<Vec<_>>(),
            &[],
            &[],
            Value::Null,
            (0, 0),
        );
        assert_eq!(r["lagUnstable"], true);
        assert_eq!(r["runValid"], false);
    }

    #[test]
    fn discontinuities_and_a_dead_small_period_stream_invalidate_the_run() {
        let rate = 48_000;
        let (src, out) = stepped(rate, 0, 100, 100);
        let e = global_lag(&out, &src, rate, 500);
        let r = invalid_reasons(e, (0, 2), &Value::Null);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].contains("discontinuities"));
        let info = finish_small_period(json!({ "minFrames": 128 }), None);
        assert_eq!(info["endedEarly"], false);
        assert!(invalid_reasons(e, (0, 0), &info).is_empty());
        let info = finish_small_period(info, Some(("render padding: gone".into(), 3.5)));
        assert_eq!(info["endedEarly"], true);
        assert_eq!(info["endedEarlyAfterS"], 3.5);
        let r = invalid_reasons(e, (0, 0), &info);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].contains("ended early") && r[0].contains("render padding: gone"));
        // No small-period stream: nothing to mark.
        assert!(finish_small_period(Value::Null, None).is_null());
        // No alignment at all is a reason too.
        assert_eq!(invalid_reasons(None, (0, 0), &Value::Null).len(), 1);
    }

    #[test]
    fn the_corrected_offset_removes_the_half_packet_bias() {
        // Frame f is captured at f / rate. Each packet is read 1 ms after its LAST frame; the
        // source (480-frame packets) is read a further `D` later than the output (441-frame
        // packets), lag 0. The true offset is D for every packet.
        let rate = 48_000u32;
        let t = |f: u64| f * 1_000_000 / u64::from(rate);
        let d = 7_000u64;
        let mk = |size: u32, extra: u64| -> Vec<PacketTime> {
            packets(5 * u64::from(rate), size, |_| 0)
                .into_iter()
                .map(|mut p| {
                    p.arrival_us = t(p.first_frame + u64::from(p.frames) - 1) + 1_000 + extra;
                    p
                })
                .collect()
        };
        let src = mk(480, d);
        let out = mk(441, 0);
        let corrected = delivery_offsets_corrected_us(&src, &out, 0, rate);
        assert!(corrected.len() > 100);
        assert!(
            corrected.iter().all(|&c| (c - d as f64).abs() <= 2.0),
            "{:?}",
            &corrected[..5]
        );
        // The raw offsets carry up to a packet (9.2 ms) of bias.
        let raw: Vec<f64> = delivery_offsets_us(&src, &out, 0)
            .into_iter()
            .map(|o| o as f64)
            .collect();
        let sp = spread(&raw).unwrap();
        assert!(sp.max - sp.min > 5_000.0, "{sp:?}");
        assert!(sp.max <= d as f64 + 2.0, "{sp:?}");
        // ... and the corrected spread is what the report quotes.
        let c = spread(&corrected).unwrap();
        assert!(c.max - c.min <= 4.0, "{c:?}");
    }
}
