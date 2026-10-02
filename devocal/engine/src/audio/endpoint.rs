//! Output endpoint selection: which endpoint the player plays on, opening it, and whether its
//! shared-mode mix format allows the low-latency `IAudioClient3` path.

use devocal_core::sessions::SessionInfo;
use windows::core::{GUID, HSTRING};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

use crate::dsp::SAMPLE_RATE;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`.
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// The endpoint the player is playing on: the first active session's endpoint, else the first
/// session's, else `None` (render to the default endpoint).
pub fn session_endpoint(sessions: &[SessionInfo]) -> Option<String> {
    sessions
        .iter()
        .find(|s| s.active)
        .or_else(|| sessions.first())
        .map(|s| s.endpoint_id.clone())
}

/// Summary of a shared-mode mix format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub float32: bool,
}

/// The low-latency path renders in the mix format without conversion, so it needs exactly
/// the engine rate, 32-bit float and at least two channels (stereo goes to the first two).
pub fn low_latency_eligible(f: MixFormat) -> bool {
    f.sample_rate == SAMPLE_RATE && f.float32 && f.channels >= 2
}

/// Reads a `WAVEFORMATEX` (or `WAVEFORMATEXTENSIBLE`) returned by `GetMixFormat`.
///
/// # Safety
/// `fmt` must point to a valid format block of at least `sizeof(WAVEFORMATEX) + cbSize` bytes.
pub(crate) unsafe fn read_mix_format(fmt: *const WAVEFORMATEX) -> MixFormat {
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

/// Frees a `CoTaskMemAlloc`ed format block on drop.
pub(crate) struct CoTaskFormat(pub *mut WAVEFORMATEX);

impl Drop for CoTaskFormat {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0.cast())) };
        }
    }
}

/// The render endpoint with this id, or the default console render endpoint. COM (MTA) must
/// be initialised on the calling thread.
pub(crate) fn find_render_device(endpoint_id: Option<&str>) -> Result<IMMDevice, String> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("device enumerator: {e}"))?;
        match endpoint_id {
            Some(id) => en
                .GetDevice(&HSTRING::from(id))
                .map_err(|e| format!("endpoint {id}: {e}")),
            None => en
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("default render endpoint: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pid: u32, endpoint: &str, active: bool) -> SessionInfo {
        SessionInfo {
            instance_id: format!("{endpoint}|session-{pid}"),
            session_identifier: format!("{endpoint}|app"),
            pid,
            endpoint_id: endpoint.to_string(),
            active,
        }
    }

    #[test]
    fn session_endpoint_prefers_active() {
        let sessions = [session(1, "ep-a", false), session(2, "ep-b", true)];
        assert_eq!(session_endpoint(&sessions).as_deref(), Some("ep-b"));
        let idle = [session(1, "ep-a", false), session(2, "ep-b", false)];
        assert_eq!(session_endpoint(&idle).as_deref(), Some("ep-a"));
        assert_eq!(session_endpoint(&[]), None);
    }

    #[test]
    fn low_latency_needs_44100_float_stereo_or_more() {
        let ok = MixFormat {
            sample_rate: 44_100,
            channels: 2,
            float32: true,
        };
        assert!(low_latency_eligible(ok));
        assert!(low_latency_eligible(MixFormat { channels: 6, ..ok }));
        assert!(!low_latency_eligible(MixFormat {
            sample_rate: 48_000,
            ..ok
        }));
        assert!(!low_latency_eligible(MixFormat {
            float32: false,
            ..ok
        }));
        assert!(!low_latency_eligible(MixFormat { channels: 1, ..ok }));
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
        let f = unsafe { read_mix_format(std::ptr::addr_of!(ext).cast()) };
        assert_eq!(
            f,
            MixFormat {
                sample_rate: 44_100,
                channels: 2,
                float32: true
            }
        );
        // Extensible PCM is not float.
        ext.SubFormat = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
        let f = unsafe { read_mix_format(std::ptr::addr_of!(ext).cast()) };
        assert!(!f.float32);
        // Plain IEEE float, 48 kHz.
        let plain = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            wBitsPerSample: 32,
            ..Default::default()
        };
        let f = unsafe { read_mix_format(&plain) };
        assert!(f.float32);
        assert_eq!(f.sample_rate, 48_000);
    }
}
