// Offline-only evaluator. No capture APIs, no audio persistence, no plugin writes.
use tune_love::audio::PcmWindow;
#[cfg(test)]
use tune_love::key_detection;
use tune_love::key_detection::{detect, Mode, MusicalKey, RollingDetector};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::Path, process::Command, time::Instant};
#[path = "../src/key_detection/stability.rs"]
mod stability;

const SECOND: usize = 48_000 * 2;
const LIMIT_SECONDS: usize = 32;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Item {
    id: String,
    path: String,
    expected: Option<Label>,
    label_source: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Label {
    pitch_class: u8,
    mode: String,
}

fn validated_label(item: &Item) -> Result<Option<MusicalKey>, String> {
    let Some(label) = &item.expected else {
        return Ok(None);
    };
    if item
        .label_source
        .as_ref()
        .is_none_or(|source| source.trim().is_empty())
    {
        return Err("expected key requires a non-empty independent labelSource".into());
    }
    if label.pitch_class > 11 {
        return Err("pitchClass must be 0..11 (C..B)".into());
    }
    let mode = match label.mode.as_str() {
        "major" => Mode::Major,
        "minor" => Mode::Minor,
        _ => return Err("mode must be major or minor".into()),
    };
    Ok(Some(MusicalKey {
        pitch_class: label.pitch_class,
        mode,
    }))
}

fn accuracy(expected: Option<MusicalKey>, actual: Option<MusicalKey>) -> Option<bool> {
    expected.map(|key| Some(key) == actual)
}

fn cadence(root: u8, minor: bool, amplitude: f32, antiphase: bool) -> Vec<f32> {
    let chords = if minor {
        [[0, 3, 7], [5, 8, 0], [7, 11, 2], [0, 3, 7]]
    } else {
        [[0, 4, 7], [5, 9, 0], [7, 11, 2], [0, 4, 7]]
    };
    let mut pcm = Vec::with_capacity(LIMIT_SECONDS * SECOND);
    for frame in 0..LIMIT_SECONDS * 48_000 {
        let local = frame % 96_000;
        let envelope = (local.min(96_000 - local - 1) as f64 / 480.0).min(1.0);
        let t = frame as f64 / 48_000.0;
        let mut sample = 0.0;
        for interval in chords[(frame / 96_000) % 4] {
            let midi = 48 + ((root + interval) % 12);
            let f = 440.0 * 2.0_f64.powf((midi as f64 - 69.0) / 12.0);
            sample += (std::f64::consts::TAU * f * t).sin()
                + 0.2 * (std::f64::consts::TAU * 2.0 * f * t).sin();
        }
        let sample = (sample * envelope / 3.6) as f32 * amplitude;
        pcm.extend_from_slice(&[sample, if antiphase { -sample } else { sample }]);
    }
    pcm
}

fn evaluate(pcm: &[f32], expected: Option<MusicalKey>, streaming: bool) -> Result<Value, String> {
    let mut detector = RollingDetector::default();
    let mut stable = stability::Stabilizer::new();
    let mut rows = vec![];
    let mut first_confirmed = None;
    let mut first_correct = None;
    let mut previous = None;
    let mut flips = 0;
    let mut times = vec![];
    for second in 6..=(pcm.len() / SECOND).min(LIMIT_SECONDS) {
        let end = second * SECOND;
        let window = PcmWindow {
            source_id: "offline".into(),
            track_key: "case".into(),
            target_generation: 1,
            capture_generation: 1,
            sample_end_sequence: (end / 2) as u64,
            captured_at_ms: (second * 1_000) as u64,
            sample_rate: 48_000,
            channels: 2,
            samples: pcm[end.saturating_sub(8 * SECOND)..end].to_vec(),
        };
        let begin = Instant::now();
        let (candidate, metrics) = if streaming {
            let result = detector.analyze(&window)?;
            (
                result.candidate,
                serde_json::to_value(result).map_err(|e| e.to_string())?,
            )
        } else {
            (
                detect(&window.samples, 48_000, 2)?,
                json!({"fed_frames": window.samples.len() / 2}),
            )
        };
        let elapsed = begin.elapsed().as_secs_f64() * 1_000.0;
        times.push(elapsed);
        let observed = stability::Observation {
            identity: Some(stability::Identity {
                source_id: "offline".into(),
                track_key: "case".into(),
                target_generation: 1,
            }),
            playing: true,
            capture_generation: 1,
            sample_end_sequence: window.sample_end_sequence,
        };
        stable.observe(&observed, second as u64 * 1_000);
        if !stable.can_begin(&observed) {
            return Err("offline window not ready".into());
        }
        let token = stable
            .begin(&observed, second as u64 * 1_000)
            .ok_or("offline window rejected")?;
        stable.complete(&token, &observed, candidate, false, second as u64 * 1_000);
        let snapshot = stable.snapshot();
        if snapshot.key.is_some() {
            first_confirmed.get_or_insert(second);
            if accuracy(expected, snapshot.key) == Some(true) {
                first_correct.get_or_insert(second);
            }
            if previous.is_some() && previous != snapshot.key {
                flips += 1;
            }
            previous = snapshot.key;
        }
        rows.push(json!({"audioEndSeconds":second, "computeMs": elapsed,
            "candidate":candidate, "confirmed":snapshot.key,
            "correct":accuracy(expected, snapshot.key), "engine":metrics}));
    }
    let cold_ms = times.first().copied();
    let mut warm = times.iter().skip(1).copied().collect::<Vec<_>>();
    warm.sort_by(f64::total_cmp);
    let mean = (!warm.is_empty()).then(|| warm.iter().sum::<f64>() / warm.len() as f64);
    let p95 = (!warm.is_empty())
        .then(|| warm[((warm.len() as f64 * 0.95).ceil() as usize - 1).min(warm.len() - 1)]);
    let result = json!({"coldComputeMs":cold_ms, "warmMeanMs":mean, "warmP95Ms":p95,
        "firstConfirmedAudioSeconds":first_confirmed, "firstCorrectAudioSeconds":first_correct,
        "confirmedFlips":flips, "finalKey":stable.snapshot().key,
        "finalCorrect":accuracy(expected, stable.snapshot().key), "windows":rows});
    stable.stop(LIMIT_SECONDS as u64 * 1_000);
    Ok(result)
}

fn case(
    id: &str,
    kind: &str,
    source: Option<&str>,
    expected: Option<MusicalKey>,
    pcm: &[f32],
) -> Result<Value, String> {
    if pcm.len() < 6 * SECOND {
        return Err(format!("{id}: need at least six seconds of decoded audio"));
    }
    // Use the same sample endpoints for both paths. These are signal-time
    // confirmation measurements, not claims of observed GUI wall-clock latency.
    let batch = evaluate(pcm, expected, false)?;
    let incremental = evaluate(pcm, expected, true)?;
    Ok(
        json!({"id": id, "kind":kind, "labelSource":source, "expected":expected,
        "accuracyEligible":expected.is_some(), "batch":batch, "incremental":incremental}),
    )
}

fn decode_file(path: &Path) -> Result<Vec<f32>, String> {
    if !path.is_file() {
        return Err("manifest audio must be an existing local file".into());
    }
    let path = path.canonicalize().map_err(|e| e.to_string())?;
    // A standard decoder, bounded to 32 seconds. PCM only travels through a pipe
    // and memory. Never opens a recording device or writes decoded audio to disk.
    let output = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ])
        .arg(path)
        .args([
            "-t",
            "32",
            "-vn",
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "pipe:1",
        ])
        .output()
        .map_err(|e| format!("FFmpeg decoder unavailable: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    if output.stdout.len() % 8 != 0 || output.stdout.len() > LIMIT_SECONDS * SECOND * 4 {
        return Err("decoder returned invalid or oversized PCM".into());
    }
    Ok(output
        .stdout
        .chunks_exact(4)
        .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
        .collect())
}

fn run() -> Result<Value, String> {
    let argument = std::env::args()
        .nth(1)
        .ok_or("usage: key_benchmark --synthetic | <manifest.json>")?;
    let mut cases = vec![];
    if argument == "--synthetic" {
        for (id, root, minor, level, anti) in [
            ("c-major", 0, false, 0.65, false),
            ("a-minor", 9, true, 0.65, false),
            ("quiet-c-major", 0, false, 0.00065, false),
            ("antiphase-c-major", 0, false, 0.65, true),
        ] {
            let expected = Some(MusicalKey {
                pitch_class: root,
                mode: if minor { Mode::Minor } else { Mode::Major },
            });
            cases.push(case(
                id,
                "synthetic",
                Some("constructed tonic/subdominant/dominant cadence; not a real-song label"),
                expected,
                &cadence(root, minor, level, anti),
            )?);
        }
    } else {
        let manifest_path = Path::new(&argument);
        let metadata = std::fs::metadata(manifest_path).map_err(|e| e.to_string())?;
        if metadata.len() > 1_048_576 {
            return Err("manifest exceeds 1 MiB".into());
        }
        let items: Vec<Item> =
            serde_json::from_slice(&std::fs::read(manifest_path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if items.is_empty() || items.len() > 100 {
            return Err("manifest needs 1..100 items".into());
        }
        for item in items {
            let expected = validated_label(&item)?;
            let path = manifest_path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&item.path);
            cases.push(case(
                &item.id,
                "local-file",
                item.label_source.as_deref(),
                expected,
                &decode_file(&path)?,
            )?);
        }
    }
    Ok(
        json!({"schemaVersion":1, "engine":"libkeyfinder 2.2.6", "profile":"debug unless explicitly built otherwise",
        "clock":"audio endpoints every second; not live GUI latency", "audioStored":false, "cases":cases}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accuracy_requires_an_independent_label_source() {
        let raw = r#"{"id":"song","path":"song.wav","expected":{"pitchClass":0,"mode":"major"}}"#;
        let item: Item = serde_json::from_str(raw).unwrap();
        assert!(validated_label(&item).is_err());
        let mut item = item;
        item.label_source = Some("Publisher score, C major".into());
        assert_eq!(
            validated_label(&item).unwrap(),
            Some(MusicalKey {
                pitch_class: 0,
                mode: Mode::Major
            })
        );
    }

    #[test]
    fn unknown_songs_have_no_accuracy_score() {
        let item: Item = serde_json::from_str(r#"{"id":"unknown","path":"song.wav"}"#).unwrap();
        assert_eq!(validated_label(&item).unwrap(), None);
        assert_eq!(
            accuracy(
                None,
                Some(MusicalKey {
                    pitch_class: 0,
                    mode: Mode::Major
                })
            ),
            None
        );
    }

    #[test]
    fn malformed_labels_fail_closed() {
        for label in [
            r#"{"pitchClass":12,"mode":"major"}"#,
            r#"{"pitchClass":0,"mode":"dorian"}"#,
        ] {
            let item: Item = serde_json::from_value(serde_json::json!({"id":"bad","path":"song.wav",
                "expected": serde_json::from_str::<serde_json::Value>(label).unwrap(), "labelSource":"score"})).unwrap();
            assert!(validated_label(&item).is_err());
        }
    }
}

fn main() {
    match run() {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
