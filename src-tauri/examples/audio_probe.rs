//! Bounded read-only diagnostic; prints statistics, never PCM or audio files.
use std::time::Duration;
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [] => helper_now_playing::audio::capture::probe_current(Duration::from_secs(5)),
        [option, pid] if option == "--pid" => pid
            .parse::<u32>()
            .map_err(|_| "invalid PID".to_owned())
            .and_then(|pid| {
                helper_now_playing::audio::capture::capture_process(pid, Duration::from_secs(5))
            }),
        _ => Err("usage: audio_probe [--pid PID]".to_owned()),
    };
    match result {
        Ok(summary) => println!("{}", serde_json::to_string_pretty(&summary).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
