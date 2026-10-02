//! Pipe protocol shared by the app and `devocal-engine.exe`.
//! One JSON object per line; every line carries `"protocol": 1`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

pub const PROTOCOL: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Command {
    Hello { version: u32 },
    Attach { pid: u32, created_at: u64 },
    SetMode { devocal: bool },
    SetModel { id: String, path: PathBuf, device: String, threads: u16 },
    Release,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Attaching,
    Active,
    Releasing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Passthrough,
    Devocal,
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    Overload,
    ModelError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Protocol,
    AttachFailed,
    CaptureFailed,
    RenderFailed,
    ModelLoadFailed,
    NoModel,
    BadDevice,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub mode: Option<Mode>,
    pub latency_ms: f32,
    pub load_ratio: f32,
    pub underruns: u64,
    pub fallback_reason: Option<FallbackReason>,
    pub attenuation: f32,
    pub attenuation_epoch: u64,
    pub session_overridden: u64,
    pub input_silent_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Event {
    State {
        phase: Phase,
        mode: Option<Mode>,
        fallback_reason: Option<FallbackReason>,
        attached_pid: Option<u32>,
    },
    Metrics(Metrics),
    Error { code: ErrorCode, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The line carried a `protocol` number other than [`PROTOCOL`].
    Version(u32),
    /// Not valid JSON, `protocol` missing/invalid, or not a known command/event.
    Malformed(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::Version(v) => {
                write!(f, "unsupported protocol version {v} (expected {PROTOCOL})")
            }
            ProtocolError::Malformed(m) => write!(f, "malformed message: {m}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Serialize)]
struct Envelope<'a, T: Serialize> {
    protocol: u32,
    #[serde(flatten)]
    body: &'a T,
}

/// Serialise `body` as one wire line: `{"protocol":1,...}\n`.
pub fn encode<T: Serialize>(body: &T) -> String {
    let mut line = serde_json::to_string(&Envelope { protocol: PROTOCOL, body })
        .expect("protocol types always serialise");
    line.push('\n');
    line
}

fn decode<T: DeserializeOwned>(line: &str) -> Result<T, ProtocolError> {
    let value: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    let version = value
        .get("protocol")
        .ok_or_else(|| ProtocolError::Malformed("missing `protocol` field".into()))?
        .as_u64()
        .ok_or_else(|| ProtocolError::Malformed("`protocol` is not an unsigned integer".into()))?;
    if version != u64::from(PROTOCOL) {
        // Versions that do not fit u32 saturate; they are still "not ours".
        return Err(ProtocolError::Version(u32::try_from(version).unwrap_or(u32::MAX)));
    }
    serde_json::from_value(value).map_err(|e| ProtocolError::Malformed(e.to_string()))
}

pub fn decode_command(line: &str) -> Result<Command, ProtocolError> {
    decode(line)
}

pub fn decode_event(line: &str) -> Result<Event, ProtocolError> {
    decode(line)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_round_trips_with_camel_case_fields() {
        let cmd = Command::Attach { pid: 1234, created_at: 5 };
        let line = encode(&cmd);
        assert_eq!(line, "{\"protocol\":1,\"cmd\":\"attach\",\"pid\":1234,\"createdAt\":5}\n");
        assert_eq!(decode_command(&line).unwrap(), cmd);
    }

    #[test]
    fn wrong_protocol_is_rejected() {
        assert_eq!(
            decode_command(r#"{"protocol":2,"cmd":"release"}"#),
            Err(ProtocolError::Version(2))
        );
    }

    #[test]
    fn missing_protocol_is_malformed() {
        assert!(matches!(
            decode_command(r#"{"cmd":"release"}"#),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_command_is_malformed() {
        assert!(matches!(
            decode_command(r#"{"protocol":1,"cmd":"explode"}"#),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn metrics_event_round_trips() {
        let ev = Event::Metrics(Metrics {
            mode: Some(Mode::Fallback),
            latency_ms: 12.5,
            load_ratio: 0.75,
            underruns: 3,
            fallback_reason: Some(FallbackReason::Overload),
            attenuation: 0.5,
            attenuation_epoch: 7,
            session_overridden: 2,
            input_silent_ms: 900,
        });
        let line = encode(&ev);
        assert!(line.contains("\"event\":\"metrics\""));
        assert!(line.contains("\"latencyMs\":12.5"));
        assert!(line.contains("\"inputSilentMs\":900"));
        assert_eq!(decode_event(&line).unwrap(), ev);
    }
}
