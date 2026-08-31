//! One-shot interactive execution PTY contract.
//!
//! Input bytes are deliberately opaque. They are never interpreted as text or
//! credentials by this protocol, and their `Debug` implementation exposes only
//! a byte count so routine error logging cannot disclose their contents.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

/// Hard limit for one input or output data frame on every transport hop.
pub const MAX_PTY_DATA_FRAME_BYTES: usize = 64 * 1024;
/// Hard limit for one stream identifier. Identifiers are opaque and non-secret,
/// but bounding them prevents attacker-controlled allocation and log inflation.
pub const MAX_PTY_STREAM_ID_BYTES: usize = 128;

/// First text message on the dedicated browser carrier. It contains only safe
/// routing metadata; input bytes are valid exclusively in binary frames after
/// the server replies `Ready` and the host emits `PtyStreamOpened`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PtyCarrierPrepare {
    pub browser_connection_id: String,
    pub target_connection_id: String,
    pub exec_request_id: String,
}

impl PtyCarrierPrepare {
    pub fn validate(&self) -> Result<(), &'static str> {
        for value in [
            &self.browser_connection_id,
            &self.target_connection_id,
            &self.exec_request_id,
        ] {
            if value.is_empty() || value.len() > MAX_PTY_STREAM_ID_BYTES {
                return Err("PTY carrier binding id length is outside the allowed range");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PtyCarrierServerMessage {
    Ready {
        carrier_id: String,
        exec_request_id: String,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct PtyStreamOpened {
    pub task_id: String,
    pub execution_generation: String,
    pub stream_id: String,
    pub session_target_id: String,
    pub registration_generation: u64,
    pub worker_incarnation: u64,
}

impl PtyStreamOpened {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_common(&self.stream_id, &self.execution_generation)?;
        if self.task_id.is_empty() || self.task_id.len() > MAX_PTY_STREAM_ID_BYTES {
            return Err("PTY task id length is outside the allowed range");
        }
        if self.session_target_id.is_empty()
            || self.session_target_id.len() > MAX_PTY_STREAM_ID_BYTES
        {
            return Err("PTY session target id length is outside the allowed range");
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct PtyInputFrame {
    pub stream_id: String,
    pub execution_generation: String,
    pub session_target_id: String,
    pub registration_generation: u64,
    pub worker_incarnation: u64,
    pub sequence: u64,
    pub data: Vec<u8>,
}

impl fmt::Debug for PtyInputFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyInputFrame")
            .field("stream_id", &self.stream_id)
            .field("execution_generation", &self.execution_generation)
            .field("sequence", &self.sequence)
            .field(
                "data",
                &format_args!("<redacted:{} bytes>", self.data.len()),
            )
            .finish()
    }
}

impl PtyInputFrame {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_common(&self.stream_id, &self.execution_generation)?;
        validate_session_target(&self.session_target_id)?;
        if self.data.is_empty() || self.data.len() > MAX_PTY_DATA_FRAME_BYTES {
            return Err("PTY input frame length is outside the allowed range");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct PtyResizeFrame {
    pub stream_id: String,
    pub execution_generation: String,
    pub session_target_id: String,
    pub registration_generation: u64,
    pub worker_incarnation: u64,
    pub sequence: u64,
    pub rows: u16,
    pub cols: u16,
}

impl PtyResizeFrame {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_common(&self.stream_id, &self.execution_generation)?;
        validate_session_target(&self.session_target_id)?;
        if !(1..=500).contains(&self.rows) || !(1..=500).contains(&self.cols) {
            return Err("PTY rows and columns must be within 1..=500");
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct PtyOutputFrame {
    pub stream_id: String,
    pub execution_generation: String,
    pub session_target_id: String,
    pub registration_generation: u64,
    pub worker_incarnation: u64,
    pub sequence: u64,
    pub data: Vec<u8>,
}

impl fmt::Debug for PtyOutputFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyOutputFrame")
            .field("stream_id", &self.stream_id)
            .field("execution_generation", &self.execution_generation)
            .field("sequence", &self.sequence)
            .field(
                "data",
                &format_args!("<redacted:{} bytes>", self.data.len()),
            )
            .finish()
    }
}

impl PtyOutputFrame {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_common(&self.stream_id, &self.execution_generation)?;
        validate_session_target(&self.session_target_id)?;
        if self.data.is_empty() || self.data.len() > MAX_PTY_DATA_FRAME_BYTES {
            return Err("PTY output frame length is outside the allowed range");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PtyCloseReason {
    Exited,
    Cancelled,
    TimedOut,
    CarrierDisconnected,
    SequenceViolation,
    SlowConsumer,
    SessionStale,
    OutcomeUnknown,
    InternalError,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct PtyCancelFrame {
    pub stream_id: String,
    pub execution_generation: String,
    pub session_target_id: String,
    pub registration_generation: u64,
    pub worker_incarnation: u64,
    pub reason: PtyCloseReason,
}

impl PtyCancelFrame {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_common(&self.stream_id, &self.execution_generation)?;
        validate_session_target(&self.session_target_id)
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct PtyStreamClosed {
    pub stream_id: String,
    pub execution_generation: String,
    pub session_target_id: String,
    pub registration_generation: u64,
    pub worker_incarnation: u64,
    pub exit_status: Option<i32>,
    pub reason: PtyCloseReason,
    pub input_frames: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
}

impl PtyStreamClosed {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_common(&self.stream_id, &self.execution_generation)?;
        validate_session_target(&self.session_target_id)
    }
}

fn validate_common(stream_id: &str, generation: &str) -> Result<(), &'static str> {
    if stream_id.is_empty() || stream_id.len() > MAX_PTY_STREAM_ID_BYTES {
        return Err("PTY stream id length is outside the allowed range");
    }
    if generation.is_empty() || generation.len() > MAX_PTY_STREAM_ID_BYTES {
        return Err("PTY execution generation length is outside the allowed range");
    }
    Ok(())
}

fn validate_session_target(session_target_id: &str) -> Result<(), &'static str> {
    if session_target_id.is_empty() || session_target_id.len() > MAX_PTY_STREAM_ID_BYTES {
        return Err("PTY session target id length is outside the allowed range");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_debug_never_exposes_bytes() {
        let frame = PtyInputFrame {
            stream_id: "stream-1".into(),
            execution_generation: "generation-1".into(),
            session_target_id: "session-1".into(),
            registration_generation: 3,
            worker_incarnation: 4,
            sequence: 7,
            data: b"sensitive-canary".to_vec(),
        };
        let rendered = format!("{frame:?}");
        assert!(!rendered.contains("sensitive-canary"));
        assert!(rendered.contains("<redacted:16 bytes>"));
    }

    #[test]
    fn binary_input_accepts_nul_and_non_utf8() {
        let frame = PtyInputFrame {
            stream_id: "stream-1".into(),
            execution_generation: "generation-1".into(),
            session_target_id: "session-1".into(),
            registration_generation: 3,
            worker_incarnation: 4,
            sequence: 0,
            data: vec![0, 0xff, 0x80, b'\n'],
        };
        assert_eq!(frame.validate(), Ok(()));
        let encoded = wincode::serialize(&frame).unwrap();
        let decoded: PtyInputFrame = wincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn empty_and_oversized_data_are_rejected() {
        let mut frame = PtyInputFrame {
            stream_id: "stream-1".into(),
            execution_generation: "generation-1".into(),
            session_target_id: "session-1".into(),
            registration_generation: 3,
            worker_incarnation: 4,
            sequence: 0,
            data: Vec::new(),
        };
        assert!(frame.validate().is_err());
        frame.data = vec![0; MAX_PTY_DATA_FRAME_BYTES + 1];
        assert!(frame.validate().is_err());
    }
}
