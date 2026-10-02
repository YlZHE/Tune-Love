use std::collections::VecDeque;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const MAX_SAMPLES: usize = 48_000 * 2 * 8;
#[derive(Default, Clone, Copy)]
pub struct PacketFlags {
    pub silent: bool,
    pub discontinuity: bool,
    pub timestamp_error: bool,
}
pub struct Packet {
    pub samples: Vec<f32>,
    pub reset: bool,
}
pub fn decode_packet(bytes: &[u8], flags: PacketFlags) -> Result<Packet, String> {
    if bytes.len() % 4 != 0 || bytes.len() / 4 % CHANNELS as usize != 0 {
        return Err("packet is not complete stereo f32 frames".into());
    }
    let mut samples = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let value = f32::from_le_bytes(chunk.try_into().unwrap());
        samples.push(if flags.silent || !value.is_finite() {
            0.0
        } else {
            value
        });
    }
    Ok(Packet {
        samples,
        reset: flags.discontinuity || flags.timestamp_error,
    })
}
pub fn measure(samples: &[f32]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut sum = 0.0;
    let mut peak: f64 = 0.0;
    for &value in samples {
        let x = f64::from(value);
        if x.is_finite() {
            sum += x * x;
            peak = peak.max(x.abs());
        }
    }
    ((sum / samples.len() as f64).sqrt(), peak)
}
pub fn retain_samples(ring: &mut VecDeque<f32>, samples: &[f32]) {
    let keep = &samples[samples.len().saturating_sub(MAX_SAMPLES)..];
    let remove = ring
        .len()
        .saturating_add(keep.len())
        .saturating_sub(MAX_SAMPLES);
    ring.drain(..remove);
    ring.extend(keep.iter().copied());
}
#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    #[test]
    fn hand_derived_quarter_amplitude_has_quarter_rms_and_peak() {
        assert_eq!(measure(&[0.25, -0.25, 0.25, -0.25]), (0.25, 0.25));
        assert_eq!(measure(&[0.0; 4]), (0.0, 0.0));
    }
    #[test]
    fn nonfinite_samples_are_zero_and_stereo_frames_stay_aligned() {
        let p = decode_packet(
            &bytes(&[f32::NAN, 0.5, f32::INFINITY, -0.5]),
            PacketFlags::default(),
        )
        .unwrap();
        assert_eq!(p.samples, vec![0.0, 0.5, 0.0, -0.5]);
        assert!(decode_packet(&[0; 7], PacketFlags::default()).is_err());
        assert!(decode_packet(&[0; 4], PacketFlags::default()).is_err());
    }
    #[test]
    fn silent_packet_conversion_ignores_payload_and_discontinuity_resets_history() {
        let p = decode_packet(
            &bytes(&[0.8, -0.8]),
            PacketFlags {
                silent: true,
                discontinuity: true,
                timestamp_error: false,
            },
        )
        .unwrap();
        assert_eq!(p.samples, vec![0.0, 0.0]);
        assert!(p.reset);
        let p = decode_packet(
            &bytes(&[0.2, 0.2]),
            PacketFlags {
                timestamp_error: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(p.reset);
    }
    #[test]
    fn rolling_history_keeps_only_last_eight_seconds_even_for_oversized_input() {
        let mut ring = VecDeque::from([0.9, 0.9]);
        let mut samples = vec![0.1; MAX_SAMPLES + 20];
        samples[MAX_SAMPLES + 18] = 0.2;
        samples[MAX_SAMPLES + 19] = 0.3;
        retain_samples(&mut ring, &samples);
        assert_eq!(ring.len(), 768000);
        assert_eq!(ring.back(), Some(&0.3));
        assert_eq!(ring.front(), Some(&0.1));
        retain_samples(&mut ring, &[0.4, 0.5]);
        assert_eq!(ring.len(), 768000);
        assert_eq!(ring.back(), Some(&0.5));
    }
}
