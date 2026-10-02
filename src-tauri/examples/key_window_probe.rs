// Offline-only stdin probe. No file enumeration, capture, GUI or plugin APIs.
use serde_json::{json, Value};
use std::{io::Read, time::Instant};
use tune_love::{
    audio::PcmWindow,
    key_detection::{detect_with_diagnostics, RollingDetector},
};

const MIN_BYTES: usize = 6 * 48_000 * 2 * 4;
// Whole songs are allowed so the rolling detector's evidence stream can be
// replayed offline (scale-match validation); still bounded.
const MAX_SECONDS: usize = 900;
const MAX_BYTES: usize = MAX_SECONDS * 48_000 * 2 * 4;

#[cfg(test)]
fn parse_args(args: &[String]) -> Result<(bool, usize), String> {
    parse_all(args).map(|(batch, end, _)| (batch, end))
}

/// (batch, end, first): `--first N` (3..=6) opts the incremental detector into
/// windows from N seconds, for evaluating an earlier analysis start.
fn parse_all(args: &[String]) -> Result<(bool, usize, usize), String> {
    let (mut batch, mut end, mut first) = (false, 12, 6);
    let mut seen = [false; 3];
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("missing argument value")?;
        match flag.as_str() {
            "--mode" if !seen[0] => {
                batch = match value.as_str() { "batch" => true, "incremental" => false, _ => return Err("mode must be batch or incremental".into()) };
                seen[0] = true;
            }
            "--end" if !seen[1] => { end = value.parse().map_err(|_| "end must be an integer")?; seen[1] = true; }
            "--first" if !seen[2] => { first = value.parse().map_err(|_| "first must be an integer")?; seen[2] = true; }
            _ => return Err("usage: key_window_probe [--mode batch|incremental] [--end 6..900] < stereo48k.f32le".into()),
        }
    }
    if !(6..=MAX_SECONDS).contains(&end) {
        return Err(format!("end must be 6..{MAX_SECONDS}"));
    }
    if !(3..=6).contains(&first) || (batch && first != 6) {
        return Err("first must be 3..6 (incremental mode only)".into());
    }
    Ok((batch, end, first))
}

fn decode(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if bytes.len() < MIN_BYTES || bytes.len() > MAX_BYTES || bytes.len() % 8 != 0 {
        return Err(format!(
            "stdin requires 6..{MAX_SECONDS} seconds of frame-aligned stereo 48 kHz f32le"
        ));
    }
    let pcm: Vec<_> = bytes
        .chunks_exact(4)
        .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
        .collect();
    if pcm.iter().any(|v| !v.is_finite()) {
        return Err("non-finite PCM".into());
    }
    Ok(pcm)
}

fn run() -> Result<Value, String> {
    let (batch, end, first) = parse_all(&std::env::args().skip(1).collect::<Vec<_>>())?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let pcm = decode(&bytes)?;
    let mut detector = if first < 6 {
        RollingDetector::with_min_seconds(first)
    } else {
        RollingDetector::default()
    };
    let mut rows = Vec::new();
    for second in first..=end.min(pcm.len() / 96_000) {
        let end_frame = second * 48_000;
        let start_frame = end_frame.saturating_sub(8 * 48_000);
        let window = PcmWindow {
            source_id: "offline".into(),
            track_key: "probe".into(),
            target_generation: 1,
            capture_generation: 1,
            sample_end_sequence: end_frame as u64,
            captured_at_ms: (second * 1000) as u64,
            sample_rate: 48_000,
            channels: 2,
            samples: pcm[start_frame * 2..end_frame * 2].to_vec(),
        };
        let clock = Instant::now();
        let (candidate, diagnostics, stream) = if batch {
            let result = detect_with_diagnostics(&window.samples, 48_000, 2)?;
            (result.candidate, result.diagnostics, Value::Null)
        } else {
            let (result, diagnostics) = detector.analyze_with_diagnostics(&window)?;
            (
                result.candidate,
                diagnostics,
                serde_json::to_value(result).map_err(|e| e.to_string())?,
            )
        };
        let elapsed = clock.elapsed().as_secs_f64() * 1000.0;
        rows.push(
            json!({"second":second,"startFrame":start_frame,"endFrame":end_frame,
            "windowFrames":end_frame-start_frame,"candidate":candidate,"diagnostics":diagnostics,
            "elapsedMs":elapsed,"stream":stream}),
        );
    }
    Ok(
        json!({"schemaVersion":1,"engine":"libkeyfinder 2.2.6","mode":if batch {"batch"} else {"incremental"},
        "build":{"rustProfile":env!("KEYFINDER_RUST_PROFILE"),"nativeRequested":env!("KEYFINDER_NATIVE_OPT_REQUESTED"),
        "nativeEffective":env!("KEYFINDER_NATIVE_OPT_EFFECTIVE")},"inputFrames":pcm.len()/2,"rows":rows}),
    )
}

fn main() {
    match run() {
        Ok(value) => println!("{value}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[path = "../native/build_options.rs"]
mod build_options;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_are_bounded_and_fail_closed() {
        assert_eq!(parse_args(&[]).unwrap(), (false, 12));
        assert_eq!(
            parse_args(&["--mode".into(), "batch".into(), "--end".into(), "6".into()]).unwrap(),
            (true, 6)
        );
        assert_eq!(parse_args(&["--end".into(), "30".into()]), Ok((false, 30)));
        assert_eq!(
            parse_args(&["--end".into(), "900".into()]),
            Ok((false, 900))
        );
        assert_eq!(
            parse_all(&["--first".into(), "3".into()]),
            Ok((false, 12, 3))
        );
        assert!(parse_all(&["--first".into(), "2".into()]).is_err());
        assert!(parse_all(&[
            "--mode".into(),
            "batch".into(),
            "--first".into(),
            "4".into()
        ])
        .is_err());
        for a in [
            vec!["--mode", "bad"],
            vec!["--end", "5"],
            vec!["--end", "901"],
            vec!["file.wav"],
            vec!["--end"],
        ] {
            assert!(parse_args(&a.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
        }
    }
    #[test]
    fn pcm_bounds_and_invalid_values_are_rejected() {
        assert!(decode(&[]).is_err());
        assert!(decode(&vec![0; MIN_BYTES - 8]).is_err());
        assert!(decode(&vec![0; MAX_BYTES + 8]).is_err());
        assert!(decode(&vec![0; MIN_BYTES + 1]).is_err());
        let mut valid = vec![0; MIN_BYTES];
        assert_eq!(decode(&valid).unwrap().len(), MIN_BYTES / 4);
        valid[..4].copy_from_slice(&f32::INFINITY.to_le_bytes());
        assert!(decode(&valid).is_err());
    }
}
