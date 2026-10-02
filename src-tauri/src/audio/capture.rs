//! Process-only WASAPI capture. There is deliberately no endpoint or microphone fallback.
use super::signal::{self, Packet, PacketFlags};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureSummary {
    pub source_id: Option<String>,
    pub track_key: Option<String>,
    pub pid: u32,
    pub process_created_at: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub packets: u64,
    pub rms: f64,
    pub peak: f64,
    pub silent_packets: u64,
    pub discontinuities: u64,
    pub timestamp_errors: u64,
}
impl CaptureSummary {
    fn empty(pid: u32, created: u64) -> Self {
        Self {
            source_id: None,
            track_key: None,
            pid,
            process_created_at: created,
            sample_rate: signal::SAMPLE_RATE,
            channels: signal::CHANNELS,
            frames: 0,
            packets: 0,
            rms: 0.0,
            peak: 0.0,
            silent_packets: 0,
            discontinuities: 0,
            timestamp_errors: 0,
        }
    }
}
#[cfg(windows)]
pub(crate) struct Apartment;
#[cfg(windows)]
impl Apartment {
    pub(crate) fn enter() -> Result<Self, String> {
        wasapi::initialize_mta().ok().map_err(|e| e.to_string())?;
        Ok(Self)
    }
}
#[cfg(windows)]
impl Drop for Apartment {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}
// Virtual process endpoints must NOT use GetMixFormat/GetDevicePeriod/GetBufferSize.
fn packet_bytes(frames: u32) -> Result<usize, String> {
    if frames > signal::SAMPLE_RATE {
        return Err("capture packet exceeds one-second safety bound".into());
    }
    Ok(frames as usize * signal::CHANNELS as usize * 4)
}
#[cfg(windows)]
pub(crate) fn run(
    identity: super::process::native::ProcessIdentity,
    duration: Option<Duration>,
    mut current: impl FnMut() -> bool,
    mut consume: impl FnMut(Packet),
) -> Result<CaptureSummary, String> {
    use wasapi::*;
    if !identity.is_alive() || !current() {
        return Err("capture target is no longer current".into());
    }
    // Upstream waits synchronously for activation. Only this dedicated worker may call it.
    let mut client = AudioClient::new_application_loopback_client(identity.record.pid, true)
        .map_err(|e| e.to_string())?;
    if !identity.is_alive() || !current() {
        return Err("capture target changed during activation".into());
    }
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48000, 2, None);
    client
        .initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            },
        )
        .map_err(|e| e.to_string())?;
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
    if !identity.is_alive() || !current() {
        return Err("capture target changed during initialization".into());
    }
    client.start_stream().map_err(|e| e.to_string())?;
    struct Stream<'a>(&'a AudioClient);
    impl Drop for Stream<'_> {
        fn drop(&mut self) {
            let _ = self.0.stop_stream();
        }
    }
    let _stream = Stream(&client);
    let started = Instant::now();
    let mut result = CaptureSummary::empty(identity.record.pid, identity.record.created);
    let mut squares = 0.0;
    while duration.is_none_or(|d| started.elapsed() < d) && current() {
        if !identity.is_alive() {
            return Err("capture process exited".into());
        }
        // Check cancellation on every packet, including a continuously signaled stream.
        for _ in 0..32 {
            if !current() || duration.is_some_and(|d| started.elapsed() >= d) {
                break;
            }
            let frames = capture
                .get_next_packet_size()
                .map_err(|e| e.to_string())?
                .ok_or("process capture must be shared mode")?;
            if frames == 0 {
                break;
            }
            let mut bytes = vec![0u8; packet_bytes(frames)?];
            let (read, info) = capture
                .read_from_device(&mut bytes)
                .map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            let flags = PacketFlags {
                silent: info.flags.silent,
                discontinuity: info.flags.data_discontinuity,
                timestamp_error: info.flags.timestamp_error,
            };
            let packet = signal::decode_packet(&bytes[..packet_bytes(read)?], flags)?;
            if !identity.is_alive() || !current() {
                break;
            }
            let (rms, peak) = signal::measure(&packet.samples);
            squares += rms * rms * packet.samples.len() as f64;
            result.frames += read as u64;
            result.packets += 1;
            result.peak = result.peak.max(peak);
            result.silent_packets += u64::from(flags.silent);
            result.discontinuities += u64::from(flags.discontinuity);
            result.timestamp_errors += u64::from(flags.timestamp_error);
            consume(packet);
        }
        // An ordinary timeout means no data, not a failed device.
        match event.wait_for_event(50) {
            Ok(()) | Err(WasapiError::EventTimeout) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    if result.frames > 0 {
        result.rms = (squares / (result.frames * signal::CHANNELS as u64) as f64).sqrt();
    }
    Ok(result)
}
fn bounded(
    duration: Duration,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<CaptureSummary, String> + Send + 'static,
) -> Result<CaptureSummary, String> {
    if duration.is_zero() || duration > Duration::from_secs(30) {
        return Err("probe duration must be between zero and thirty seconds".into());
    }
    bounded_with_timeout(duration + Duration::from_secs(5), work)
}
fn bounded_with_timeout(
    timeout: Duration,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<CaptureSummary, String> + Send + 'static,
) -> Result<CaptureSummary, String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    std::thread::Builder::new()
        .name("audio-probe-mta".into())
        .spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(worker_cancelled)))
                    .unwrap_or_else(|_| Err("audio probe worker panicked".into()));
            let _ = tx.send(result);
        })
        .map_err(|e| e.to_string())?;
    // A wedged external activation is not joined. One call creates one worker; no retries.
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            cancelled.store(true, Ordering::Release);
            Err("audio probe timed out".to_string())
        }
    }
}
pub fn capture_process(pid: u32, duration: Duration) -> Result<CaptureSummary, String> {
    #[cfg(windows)]
    return bounded(duration, move |cancelled| {
        let _apartment = Apartment::enter()?;
        let identity = super::process::native::ProcessIdentity::open(pid)?;
        let result = run(
            identity,
            Some(duration),
            || !cancelled.load(Ordering::Acquire),
            |_| {},
        )?;
        if result.frames == 0 {
            return Err("capture timed out without PCM frames".into());
        }
        Ok(result)
    });
    #[cfg(not(windows))]
    {
        let _ = (pid, duration);
        Err("Windows process loopback is unavailable".into())
    }
}
pub fn probe_current(duration: Duration) -> Result<CaptureSummary, String> {
    #[cfg(windows)]
    return bounded(duration, move |cancelled| {
        let _apartment = Apartment::enter()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let controller = runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(2), nowplaying::MediaController::new())
                    .await
            })
            .map_err(|_| "GSMTC initialization timed out")?
            .map_err(|e| e.to_string())?;
        let session = runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(2), controller.current()).await
            })
            .map_err(|_| "GSMTC read timed out")?
            .map_err(|e| e.to_string())?
            .ok_or("no current GSMTC source")?;
        let target = crate::media::audio_target_from_session(&session)
            .ok_or("current source has no track")?;
        if !target.playing {
            return Err(format!(
                "current source {} is paused or inactive",
                target.source_id
            ));
        }
        let identity = super::process::native::resolve(&target.source_id)?;
        let mut next_check = Instant::now();
        let mut result = run(
            identity,
            Some(duration),
            || {
                if cancelled.load(Ordering::Acquire) {
                    return false;
                }
                if Instant::now() < next_check {
                    return true;
                }
                next_check = Instant::now() + Duration::from_millis(700);
                runtime
                    .block_on(async {
                        tokio::time::timeout(Duration::from_millis(250), controller.current()).await
                    })
                    .ok()
                    .and_then(Result::ok)
                    .flatten()
                    .and_then(|s| crate::media::audio_target_from_session(&s))
                    .as_ref()
                    == Some(&target)
            },
            |_| {},
        )?;
        result.source_id = Some(target.source_id);
        result.track_key = Some(target.track_key);
        if result.frames == 0 {
            return Err("capture timed out without PCM frames".into());
        }
        Ok(result)
    });
    #[cfg(not(windows))]
    {
        let _ = duration;
        Err("Windows process loopback is unavailable".into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packet_allocation_rejects_untrusted_huge_counts() {
        assert_eq!(packet_bytes(480), Ok(3840));
        assert!(packet_bytes(u32::MAX).is_err());
    }
    #[test]
    fn probe_rejects_invalid_duration_without_spawning_capture() {
        assert!(bounded(Duration::ZERO, |_| panic!("must not run")).is_err());
    }
    #[test]
    fn outer_timeout_cancels_late_activation_worker_before_it_can_capture() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let result = bounded_with_timeout(Duration::from_millis(50), move |cancelled| {
            ready_tx.send(()).unwrap();
            loop {
                if cancelled.load(Ordering::Acquire) {
                    tx.send(true).unwrap();
                    return Err("cancelled".into());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        assert!(result.unwrap_err().contains("timed out"));
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)), Ok(true));
    }
}
