//! Controlled low-volume playback in two owned sibling processes; never controls a user player.
#[cfg(windows)]
mod native {
    use std::{
        io::{BufRead, BufReader, Write},
        os::windows::process::CommandExt,
        process::{Child, ChildStdin, Command, Stdio},
        sync::mpsc::{self, Receiver},
        time::{Duration, Instant},
    };
    use tune_love::audio::capture::capture_process;
    use wasapi::*;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const AMPLITUDE: f32 = 0.01;

    fn reply(value: serde_json::Value) -> Result<()> {
        println!("{value}");
        std::io::stdout().flush()?;
        Ok(())
    }

    fn renderer(frequency: f32) -> Result<()> {
        initialize_mta().ok()?;
        let (sender, commands) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let enumerator = DeviceEnumerator::new()?;
        let device = enumerator.get_default_device(&Direction::Render)?;
        let mut client = device.get_iaudioclient()?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
        let (period, _) = client.get_device_period()?;
        client.initialize_client(
            &format,
            &Direction::Render,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: period,
            },
        )?;
        let event = client.set_get_eventhandle()?;
        let render = client.get_audiorenderclient()?;
        let frames = client.get_available_space_in_frames()? as usize;
        render.write_to_device(frames, &vec![0; frames * 8], None)?;
        client.start_stream()?;
        let started = Instant::now();
        let mut active = false;
        let mut position = 0_u64;
        let mut rendered_tone_frames = 0_u64;
        let mut exit = false;
        reply(
            serde_json::json!({"ready":true,"pid":std::process::id(),"frequency":frequency,"amplitude":AMPLITUDE}),
        )?;
        let result = (|| -> Result<()> {
            while !exit && started.elapsed() < Duration::from_secs(90) {
                while let Ok(command) = commands.try_recv() {
                    match command.trim() {
                        "on" => active = true,
                        "off" => active = false,
                        "exit" => exit = true,
                        "stats" => {}
                        _ => return Err("unexpected fixture command".into()),
                    }
                    reply(
                        serde_json::json!({"command":command.trim(),"active":active,"toneFrames":rendered_tone_frames}),
                    )?;
                }
                if exit {
                    break;
                }
                let _ = event.wait_for_event(50);
                let frames = client.get_available_space_in_frames()? as usize;
                if frames == 0 {
                    continue;
                }
                let mut bytes = Vec::with_capacity(frames * 8);
                for _ in 0..frames {
                    let value = if active {
                        (std::f32::consts::TAU * frequency * (position % 48_000) as f32 / 48_000.0)
                            .sin()
                            * AMPLITUDE
                    } else {
                        0.0
                    };
                    position += 1;
                    bytes.extend_from_slice(&value.to_le_bytes());
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
                render.write_to_device(frames, &bytes, None)?;
                if active {
                    rendered_tone_frames += frames as u64;
                }
            }
            Ok(())
        })();
        let _ = client.stop_stream();
        result
    }

    struct Player {
        child: Child,
        input: ChildStdin,
        replies: Receiver<String>,
    }
    impl Player {
        fn start(frequency: &str) -> Result<Self> {
            let mut child = Command::new(std::env::current_exe()?)
                .args(["--renderer", frequency])
                .creation_flags(0x08000000)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let input = child.stdin.take().ok_or("missing child stdin")?;
            let output = child.stdout.take().ok_or("missing child stdout")?;
            let (sender, replies) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let Ok(line) = line else { break };
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            });
            let player = Self {
                child,
                input,
                replies,
            };
            let ready: serde_json::Value =
                serde_json::from_str(&player.replies.recv_timeout(Duration::from_secs(8))?)?;
            if ready["ready"] != true || ready["pid"].as_u64() != Some(player.child.id() as u64) {
                return Err("fixture readiness identity mismatch".into());
            }
            Ok(player)
        }
        fn command(&mut self, command: &str) -> Result<serde_json::Value> {
            writeln!(self.input, "{command}")?;
            self.input.flush()?;
            let value: serde_json::Value =
                serde_json::from_str(&self.replies.recv_timeout(Duration::from_secs(3))?)?;
            if value["command"] != command {
                return Err("fixture acknowledgement mismatch".into());
            }
            Ok(value)
        }
        fn close(&mut self) -> Result<()> {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            self.command("exit")?;
            let started = Instant::now();
            loop {
                if let Some(status) = self.child.try_wait()? {
                    if !status.success() {
                        return Err(format!("fixture exited {status}").into());
                    }
                    return Ok(());
                }
                if started.elapsed() > Duration::from_secs(3) {
                    return Err("owned fixture failed to exit normally".into());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    impl Drop for Player {
        fn drop(&mut self) {
            if self.close().is_err() {
                // Child retains the OS handle for this exact process created by this fixture.
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    pub fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().collect();
        if args.get(1).is_some_and(|arg| arg == "--renderer") {
            return renderer(args.get(2).ok_or("missing frequency")?.parse()?);
        }
        let mut target = Player::start("440")?;
        let mut other = Player::start("880")?;
        let target_pid = target.child.id();
        let other_pid = other.child.id();
        let mut phases = Vec::new();
        for (name, target_on, other_on) in [
            ("both_silent", false, false),
            ("target_only", true, false),
            ("other_only", false, true),
            ("both_active", true, true),
            ("silent_again", false, false),
        ] {
            target.command(if target_on { "on" } else { "off" })?;
            other.command(if other_on { "on" } else { "off" })?;
            std::thread::sleep(Duration::from_millis(350));
            let summary = capture_process(target_pid, Duration::from_millis(1100))
                .map_err(std::io::Error::other)?;
            if target_on && (summary.frames < 1000 || summary.rms <= 0.0001) {
                return Err(format!(
                    "{name}: target did not produce captured audio: {}",
                    summary.rms
                )
                .into());
            }
            if !target_on && summary.rms > 0.000001 {
                return Err(format!("{name}: non-target audio leaked: {}", summary.rms).into());
            }
            let other_evidence = if name == "other_only" {
                let value = capture_process(other_pid, Duration::from_millis(650))
                    .map_err(std::io::Error::other)?;
                if value.frames < 1000 || value.rms <= 0.0001 {
                    return Err("non-target render was not independently observed, so isolation is unproven".into());
                }
                Some(value)
            } else {
                None
            };
            let target_render = target.command("stats")?;
            let other_render = other.command("stats")?;
            phases.push(serde_json::json!({"phase":name,"capture":summary,"targetRender":target_render,"otherRender":other_render,"otherCapture":other_evidence}));
        }
        if capture_process(0, Duration::from_millis(50)).is_ok() {
            return Err("invalid PID was accepted".into());
        }
        target.close()?;
        other.close()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "passed":true,"targetPid":target_pid,"otherPid":other_pid,"amplitude":AMPLITUDE,
                "sampleRate":48_000,"channels":2,"invalidPidRejected":true,"childrenExitedNormally":true,"phases":phases,
            }))?
        );
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    native::run()
}
#[cfg(not(windows))]
fn main() {
    eprintln!("This isolation fixture requires Windows process loopback capture.");
    std::process::exit(1);
}
