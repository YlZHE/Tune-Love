//! Records the default render endpoint's mix (WASAPI shared-mode loopback) for live diagnosis.
//!
//! Usage: `endpoint_record <seconds> <out_prefix>`
//!
//! Opens the endpoint's own mix format (sample rate and channel count) as float32, so the
//! loopback does no rate or channel conversion: this is the mix the audio engine hands to the
//! endpoint's driver. Writes
//! - `<out_prefix>.f32`          interleaved f32le, the mix format's channels and rate;
//! - `<out_prefix>.packets.csv`  per packet: first_frame, frames, arrival_us (QPC, right after
//!   the read), device_qpc_100ns, silent, discontinuity, timestamp_error;
//! - `<out_prefix>.json`         endpoint name/id, rate, channels, start time, totals.
//!
//! Read-only: it only captures, never renders or changes a volume.

use std::io::{BufWriter, Write};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

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

fn main() -> Result<(), String> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 3 {
        return Err("usage: endpoint_record <seconds> <out_prefix>".into());
    }
    let seconds: f64 = a[1].parse().map_err(|e| format!("seconds: {e}"))?;
    let prefix = &a[2];
    wasapi::initialize_mta()
        .ok()
        .map_err(|e| format!("COM: {e}"))?;
    let en = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
    let dev = en
        .get_default_device(&Direction::Render)
        .map_err(|e| format!("default render endpoint: {e}"))?;
    let name = dev.get_friendlyname().unwrap_or_default();
    let desc = dev.get_description().unwrap_or_default();
    let iface = dev.get_interface_friendlyname().unwrap_or_default();
    let id = dev.get_id().unwrap_or_default();
    let mut client = dev
        .get_iaudioclient()
        .map_err(|e| format!("endpoint client: {e}"))?;
    let mix = client
        .get_mixformat()
        .map_err(|e| format!("mix format: {e}"))?;
    let (rate, ch) = (mix.get_samplespersec(), mix.get_nchannels());
    println!("endpoint: {name} (device \"{desc}\", adapter \"{iface}\")");
    println!(
        "mix format: {rate} Hz, {ch} ch, {} bit {:?}",
        mix.get_bitspersample(),
        mix.get_subformat()
    );
    // Same rate and channels as the mix: autoconvert only changes the sample type if the mix
    // is not float32 (it practically always is).
    let fmt = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, ch as usize, None);
    client
        .initialize_client(
            &fmt,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            },
        )
        .map_err(|e| format!("initialise loopback: {e}"))?;
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let cap = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
    let io = |e: std::io::Error| e.to_string();
    let mut out = BufWriter::new(std::fs::File::create(format!("{prefix}.f32")).map_err(io)?);
    let mut csv =
        BufWriter::new(std::fs::File::create(format!("{prefix}.packets.csv")).map_err(io)?);
    writeln!(
        csv,
        "first_frame,frames,arrival_us,device_qpc_100ns,silent,discontinuity,timestamp_error"
    )
    .map_err(io)?;
    let frame_bytes = 4 * ch as usize;
    let mut bytes = vec![0u8; rate as usize * frame_bytes];
    let start_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let start_us = now_us();
    client.start_stream().map_err(|e| e.to_string())?;
    let (mut frames, mut packets, mut disc, mut silent) = (0u64, 0u64, 0u64, 0u64);
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < seconds {
        let _ = event.wait_for_event(50);
        while cap
            .get_next_packet_size()
            .map_err(|e| e.to_string())?
            .unwrap_or(0)
            > 0
        {
            let (n, info) = cap
                .read_from_device(&mut bytes)
                .map_err(|e| e.to_string())?;
            let arrival = now_us();
            if n == 0 {
                continue;
            }
            let f = &info.flags;
            writeln!(
                csv,
                "{frames},{n},{arrival},{},{},{},{}",
                info.timestamp,
                u8::from(f.silent),
                u8::from(f.data_discontinuity),
                u8::from(f.timestamp_error)
            )
            .map_err(io)?;
            // A silent packet's buffer content is undefined: write zeros.
            if f.silent {
                bytes[..n as usize * frame_bytes].fill(0);
                silent += 1;
            }
            if f.data_discontinuity && packets > 0 {
                disc += 1;
            }
            out.write_all(&bytes[..n as usize * frame_bytes])
                .map_err(io)?;
            frames += u64::from(n);
            packets += 1;
        }
    }
    let _ = client.stop_stream();
    out.flush().map_err(io)?;
    csv.flush().map_err(io)?;
    let meta = serde_json::json!({
        "endpoint": name, "deviceDescription": desc, "adapter": iface, "endpointId": id,
        "format": "f32le interleaved", "sampleRate": rate, "channels": ch,
        "startUnixMs": start_unix_ms, "startQpcUs": start_us,
        "frames": frames, "packets": packets, "discontinuities": disc, "silentPackets": silent,
    });
    std::fs::write(format!("{prefix}.json"), meta.to_string()).map_err(io)?;
    println!(
        "{frames} frames ({:.2} s) in {packets} packets; discontinuities {disc}, silent packets {silent}",
        frames as f64 / f64::from(rate)
    );
    Ok(())
}
