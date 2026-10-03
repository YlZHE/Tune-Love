//! Read-only listing of the shared-mode engine periods of every active render endpoint.
//!
//! For each endpoint it activates an `IAudioClient3` and calls only `GetMixFormat` and
//! `GetSharedModeEnginePeriod`. It NEVER initialises or starts a stream: opening one would
//! change the engine period of the whole endpoint (every program playing to it), and this
//! probe must leave the system exactly as it found it.
//!
//! An endpoint is `lowLatencyCapable` when its mix format is 32-bit float with at least two
//! channels and its minimum shared-mode period is at most 3 ms (the design's requirement for
//! the small-period path). A failing endpoint gets a row with an `error` field; the others are
//! still listed.
//!
//! Usage: `device_periods [--out <json>]`
//!
//! The decision logic and the row format are pure and unit-tested without devices
//! (`cargo test --workspace` runs them: the example is declared with `test = true`).

use serde_json::{json, Value};
use wasapi::DeviceEnumerator;
use windows::core::GUID;
use windows::Win32::Media::Audio::{
    eRender, IAudioClient3, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`.
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// Longest minimum shared-mode period (ms) that counts as low latency.
const MAX_LOW_LATENCY_MS: f64 = 3.0;

/// Summary of a shared-mode mix format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MixFormat {
    sample_rate: u32,
    channels: u16,
    float32: bool,
}

/// Reads a `WAVEFORMATEX` (or `WAVEFORMATEXTENSIBLE`) returned by `GetMixFormat`.
///
/// # Safety
/// `fmt` must point to a valid format block of at least `sizeof(WAVEFORMATEX) + cbSize` bytes.
unsafe fn read_mix_format(fmt: *const WAVEFORMATEX) -> MixFormat {
    let base = unsafe { std::ptr::read_unaligned(fmt) };
    let tag = base.wFormatTag;
    let bits = base.wBitsPerSample;
    let cb = base.cbSize;
    let float = match tag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE if cb >= 22 => {
            let ext = unsafe { std::ptr::read_unaligned(fmt.cast::<WAVEFORMATEXTENSIBLE>()) };
            let sub = ext.SubFormat;
            sub == SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    };
    MixFormat {
        sample_rate: base.nSamplesPerSec,
        channels: base.nChannels,
        float32: float && bits == 32,
    }
}

/// Period length in milliseconds at the mix rate (0 for an unusable rate).
fn period_ms(frames: u32, sample_rate: u32) -> f64 {
    if sample_rate == 0 {
        return 0.0;
    }
    f64::from(frames) * 1000.0 / f64::from(sample_rate)
}

/// Float stereo-or-more mix format and a minimum shared period of at most 3 ms. A minimum of 0
/// frames or a 0 Hz rate is no information, so it is not capable.
fn low_latency_capable(f: MixFormat, min_period_frames: u32) -> bool {
    f.float32
        && f.channels >= 2
        && f.sample_rate > 0
        && min_period_frames > 0
        && period_ms(min_period_frames, f.sample_rate) <= MAX_LOW_LATENCY_MS
}

fn row_json(
    id: &str,
    name: &str,
    f: MixFormat,
    default: u32,
    fundamental: u32,
    min: u32,
    max: u32,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "mixRate": f.sample_rate,
        "channels": f.channels,
        "float32": f.float32,
        "defaultFrames": default,
        "fundamentalFrames": fundamental,
        "minFrames": min,
        "maxFrames": max,
        "minMs": period_ms(min, f.sample_rate),
        "lowLatencyCapable": low_latency_capable(f, min),
    })
}

/// A row for an endpoint that could not be queried.
fn error_row(id: &str, name: Option<&str>, error: &str) -> Value {
    json!({ "id": id, "name": name, "error": error })
}

/// Mix format and the four engine periods (default, fundamental, minimum, maximum; frames) of
/// one endpoint. Read-only: no stream is initialised or started.
fn query_periods(dev: &IMMDevice) -> Result<(MixFormat, [u32; 4]), String> {
    unsafe {
        let client: IAudioClient3 = dev
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("IAudioClient3 unavailable on this endpoint: {e}"))?;
        let fmt = client
            .GetMixFormat()
            .map_err(|e| format!("mix format: {e}"))?;
        let mix = read_mix_format(fmt);
        let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
        let r = client.GetSharedModeEnginePeriod(
            fmt,
            &mut default,
            &mut fundamental,
            &mut min,
            &mut max,
        );
        // Freed on every path, success or not.
        CoTaskMemFree(Some(fmt.cast()));
        r.map_err(|e| format!("GetSharedModeEnginePeriod: {e}"))?;
        Ok((mix, [default, fundamental, min, max]))
    }
}

/// The friendly name of the endpoint, through wasapi.
fn friendly_name(id: &str) -> Result<String, String> {
    let en = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
    en.get_device(id)
        .map_err(|e| format!("get_device: {e}"))?
        .get_friendlyname()
        .map_err(|e| format!("friendly name: {e}"))
}

/// One row per active render endpoint. Every COM object is released when this returns.
fn list_rows() -> Result<Vec<Value>, String> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("device enumerator: {e}"))?;
        let devices = en
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;
        let count = devices.GetCount().map_err(|e| format!("GetCount: {e}"))?;
        let mut rows = Vec::with_capacity(count as usize);
        for i in 0..count {
            let dev = match devices.Item(i) {
                Ok(d) => d,
                Err(e) => {
                    rows.push(error_row(
                        &format!("#{i}"),
                        None,
                        &format!("Item({i}): {e}"),
                    ));
                    continue;
                }
            };
            let id = match dev.GetId() {
                Ok(p) => {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.as_ptr().cast()));
                    s
                }
                Err(e) => {
                    rows.push(error_row(&format!("#{i}"), None, &format!("GetId: {e}")));
                    continue;
                }
            };
            let name = friendly_name(&id);
            let row = match query_periods(&dev) {
                Ok((mix, [default, fundamental, min, max])) => match &name {
                    Ok(n) => row_json(&id, n, mix, default, fundamental, min, max),
                    Err(e) => {
                        let mut r = row_json(&id, "", mix, default, fundamental, min, max);
                        r["name"] = Value::Null;
                        r["nameError"] = json!(e);
                        r
                    }
                },
                Err(e) => error_row(&id, name.as_deref().ok(), &e),
            };
            rows.push(row);
        }
        Ok(rows)
    }
}

fn parse_args(args: &[String]) -> Result<Option<String>, String> {
    let mut out = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--out" => {
                out = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--out needs a value".to_string())?,
                )
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(out)
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let out = parse_args(&argv)?;
    wasapi::initialize_mta()
        .ok()
        .map_err(|e| format!("COM: {e}"))?;
    let rows = list_rows();
    wasapi::deinitialize();
    let report = json!({ "endpoints": rows? });
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    println!("{text}");
    if let Some(path) = out {
        std::fs::write(&path, &text).map_err(|e| format!("write {path}: {e}"))?;
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("device_periods: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(rate: u32) -> MixFormat {
        MixFormat {
            sample_rate: rate,
            channels: 2,
            float32: true,
        }
    }

    #[test]
    fn low_latency_needs_float_stereo_and_3ms() {
        assert!(low_latency_capable(f(48_000), 128));
        assert!(low_latency_capable(f(48_000), 144));
        assert!(!low_latency_capable(f(48_000), 480));
        assert!(!low_latency_capable(f(44_100), 441));
        assert!(!low_latency_capable(
            MixFormat {
                float32: false,
                ..f(48_000)
            },
            128
        ));
        assert!(!low_latency_capable(
            MixFormat {
                channels: 1,
                ..f(48_000)
            },
            128
        ));
        // No information is not capability.
        assert!(!low_latency_capable(f(48_000), 0));
        assert!(!low_latency_capable(f(0), 128));
    }

    #[test]
    fn mix_format_parses_plain_and_extensible_float() {
        let mut ext = WAVEFORMATEXTENSIBLE::default();
        ext.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE;
        ext.Format.nChannels = 2;
        ext.Format.nSamplesPerSec = 44_100;
        ext.Format.wBitsPerSample = 32;
        ext.Format.cbSize = 22;
        ext.SubFormat = SUBTYPE_IEEE_FLOAT;
        let m = unsafe { read_mix_format(std::ptr::addr_of!(ext).cast()) };
        assert_eq!(
            m,
            MixFormat {
                sample_rate: 44_100,
                channels: 2,
                float32: true
            }
        );
        // Extensible PCM is not float.
        ext.SubFormat = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
        let m = unsafe { read_mix_format(std::ptr::addr_of!(ext).cast()) };
        assert!(!m.float32);
        // Plain IEEE float, 48 kHz.
        let plain = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            wBitsPerSample: 32,
            ..Default::default()
        };
        let m = unsafe { read_mix_format(&plain) };
        assert!(m.float32);
        assert_eq!(m.sample_rate, 48_000);
    }

    #[test]
    fn row_json_reports_min_ms() {
        let r = row_json("id1", "Speakers", f(48_000), 480, 480, 128, 960);
        assert!((r["minMs"].as_f64().unwrap() - 2.667).abs() < 1e-3, "{r}");
        assert_eq!(r["lowLatencyCapable"], true);
        for (key, want) in [
            ("id", json!("id1")),
            ("name", json!("Speakers")),
            ("mixRate", json!(48_000)),
            ("channels", json!(2)),
            ("float32", json!(true)),
            ("defaultFrames", json!(480)),
            ("fundamentalFrames", json!(480)),
            ("minFrames", json!(128)),
            ("maxFrames", json!(960)),
        ] {
            assert_eq!(r[key], want, "{key}");
        }
        let slow = row_json("id2", "HDMI", f(48_000), 480, 480, 480, 480);
        assert_eq!(slow["lowLatencyCapable"], false);
        assert!((slow["minMs"].as_f64().unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn error_rows_carry_the_error() {
        let r = error_row("id", Some("Dev"), "boom");
        assert_eq!(r["error"].as_str(), Some("boom"));
        assert_eq!(r["name"].as_str(), Some("Dev"));
        assert!(error_row("id", None, "x")["name"].is_null());
    }

    #[test]
    fn args_take_only_out() {
        let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&[]), Ok(None));
        assert_eq!(
            parse_args(&s(&["--out", "o.json"])),
            Ok(Some("o.json".into()))
        );
        assert!(parse_args(&s(&["--out"])).is_err());
        assert!(parse_args(&s(&["--pid", "1"])).is_err());
    }
}
