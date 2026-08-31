//! Binary carrier codec for one-shot exec PTY traffic.
//!
//! The fixed header keeps frame admission independent of JSON parsing and the
//! data section remains opaque bytes. JSON is used only for bounded metadata;
//! input/output bytes are never converted to text or embedded in a loggable
//! control object.

use serde::{Deserialize, Serialize};

use crate::exec_pty::{
    MAX_PTY_DATA_FRAME_BYTES, PtyCancelFrame, PtyInputFrame, PtyOutputFrame, PtyResizeFrame,
    PtyStreamClosed, PtyStreamOpened,
};

const MAGIC: [u8; 4] = *b"LPTY";
pub const PTY_WIRE_VERSION: u8 = 1;
const HEADER_LEN: usize = 16;
const MAX_METADATA_BYTES: usize = 16 * 1024;
pub const MAX_PTY_WIRE_FRAME_BYTES: usize =
    HEADER_LEN + MAX_METADATA_BYTES + MAX_PTY_DATA_FRAME_BYTES;

const KIND_INPUT: u8 = 1;
const KIND_RESIZE: u8 = 2;
const KIND_CANCEL: u8 = 3;
const KIND_OPENED: u8 = 4;
const KIND_OUTPUT: u8 = 5;
const KIND_CLOSED: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyWireFrame {
    Input(PtyInputFrame),
    Resize(PtyResizeFrame),
    Cancel(PtyCancelFrame),
    Opened(PtyStreamOpened),
    Output(PtyOutputFrame),
    Closed(PtyStreamClosed),
}

impl PtyWireFrame {
    pub fn stream_id(&self) -> &str {
        match self {
            Self::Input(frame) => &frame.stream_id,
            Self::Resize(frame) => &frame.stream_id,
            Self::Cancel(frame) => &frame.stream_id,
            Self::Opened(frame) => &frame.stream_id,
            Self::Output(frame) => &frame.stream_id,
            Self::Closed(frame) => &frame.stream_id,
        }
    }

    pub fn execution_generation(&self) -> &str {
        match self {
            Self::Input(frame) => &frame.execution_generation,
            Self::Resize(frame) => &frame.execution_generation,
            Self::Cancel(frame) => &frame.execution_generation,
            Self::Opened(frame) => &frame.execution_generation,
            Self::Output(frame) => &frame.execution_generation,
            Self::Closed(frame) => &frame.execution_generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyWireError {
    TooShort,
    BadMagic,
    UnsupportedVersion(u8),
    UnknownKind(u8),
    ReservedBits,
    MetadataTooLarge,
    DataTooLarge,
    LengthMismatch,
    UnexpectedData,
    MissingData,
    InvalidMetadata,
    InvalidFrame(&'static str),
}

impl std::fmt::Display for PtyWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => f.write_str("PTY wire frame is shorter than its fixed header"),
            Self::BadMagic => f.write_str("PTY wire frame magic is invalid"),
            Self::UnsupportedVersion(version) => {
                write!(f, "PTY wire version {version} is unsupported")
            }
            Self::UnknownKind(kind) => write!(f, "PTY wire frame kind {kind} is unknown"),
            Self::ReservedBits => f.write_str("PTY wire reserved bits must be zero"),
            Self::MetadataTooLarge => f.write_str("PTY wire metadata exceeds its hard limit"),
            Self::DataTooLarge => f.write_str("PTY wire data exceeds its hard limit"),
            Self::LengthMismatch => f.write_str("PTY wire frame length does not match its header"),
            Self::UnexpectedData => f.write_str("PTY control frame unexpectedly contains data"),
            Self::MissingData => f.write_str("PTY data frame contains no data"),
            Self::InvalidMetadata => f.write_str("PTY wire metadata is invalid"),
            Self::InvalidFrame(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PtyWireError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DataMetadata {
    stream_id: String,
    execution_generation: String,
    session_target_id: String,
    registration_generation: u64,
    worker_incarnation: u64,
    sequence: u64,
}

impl From<&PtyInputFrame> for DataMetadata {
    fn from(frame: &PtyInputFrame) -> Self {
        Self {
            stream_id: frame.stream_id.clone(),
            execution_generation: frame.execution_generation.clone(),
            session_target_id: frame.session_target_id.clone(),
            registration_generation: frame.registration_generation,
            worker_incarnation: frame.worker_incarnation,
            sequence: frame.sequence,
        }
    }
}

impl From<&PtyOutputFrame> for DataMetadata {
    fn from(frame: &PtyOutputFrame) -> Self {
        Self {
            stream_id: frame.stream_id.clone(),
            execution_generation: frame.execution_generation.clone(),
            session_target_id: frame.session_target_id.clone(),
            registration_generation: frame.registration_generation,
            worker_incarnation: frame.worker_incarnation,
            sequence: frame.sequence,
        }
    }
}

pub fn encode(frame: &PtyWireFrame) -> Result<Vec<u8>, PtyWireError> {
    let (kind, metadata, data) = match frame {
        PtyWireFrame::Input(frame) => {
            frame.validate().map_err(PtyWireError::InvalidFrame)?;
            (
                KIND_INPUT,
                json(&DataMetadata::from(frame))?,
                frame.data.as_slice(),
            )
        }
        PtyWireFrame::Resize(frame) => {
            frame.validate().map_err(PtyWireError::InvalidFrame)?;
            (KIND_RESIZE, json(frame)?, &[][..])
        }
        PtyWireFrame::Cancel(frame) => {
            frame.validate().map_err(PtyWireError::InvalidFrame)?;
            (KIND_CANCEL, json(frame)?, &[][..])
        }
        PtyWireFrame::Opened(frame) => {
            frame.validate().map_err(PtyWireError::InvalidFrame)?;
            (KIND_OPENED, json(frame)?, &[][..])
        }
        PtyWireFrame::Output(frame) => {
            frame.validate().map_err(PtyWireError::InvalidFrame)?;
            (
                KIND_OUTPUT,
                json(&DataMetadata::from(frame))?,
                frame.data.as_slice(),
            )
        }
        PtyWireFrame::Closed(frame) => {
            frame.validate().map_err(PtyWireError::InvalidFrame)?;
            (KIND_CLOSED, json(frame)?, &[][..])
        }
    };
    if metadata.len() > MAX_METADATA_BYTES {
        return Err(PtyWireError::MetadataTooLarge);
    }
    if data.len() > MAX_PTY_DATA_FRAME_BYTES {
        return Err(PtyWireError::DataTooLarge);
    }

    let mut encoded = Vec::with_capacity(HEADER_LEN + metadata.len() + data.len());
    encoded.extend_from_slice(&MAGIC);
    encoded.push(PTY_WIRE_VERSION);
    encoded.push(kind);
    encoded.extend_from_slice(&0u16.to_le_bytes());
    encoded.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&(data.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&metadata);
    encoded.extend_from_slice(data);
    Ok(encoded)
}

pub fn decode(encoded: &[u8]) -> Result<PtyWireFrame, PtyWireError> {
    if encoded.len() < HEADER_LEN {
        return Err(PtyWireError::TooShort);
    }
    if encoded[..4] != MAGIC {
        return Err(PtyWireError::BadMagic);
    }
    if encoded[4] != PTY_WIRE_VERSION {
        return Err(PtyWireError::UnsupportedVersion(encoded[4]));
    }
    let kind = encoded[5];
    if u16::from_le_bytes([encoded[6], encoded[7]]) != 0 {
        return Err(PtyWireError::ReservedBits);
    }
    let metadata_len = u32::from_le_bytes(encoded[8..12].try_into().expect("fixed slice")) as usize;
    let data_len = u32::from_le_bytes(encoded[12..16].try_into().expect("fixed slice")) as usize;
    if metadata_len > MAX_METADATA_BYTES {
        return Err(PtyWireError::MetadataTooLarge);
    }
    if data_len > MAX_PTY_DATA_FRAME_BYTES {
        return Err(PtyWireError::DataTooLarge);
    }
    if HEADER_LEN
        .checked_add(metadata_len)
        .and_then(|length| length.checked_add(data_len))
        != Some(encoded.len())
    {
        return Err(PtyWireError::LengthMismatch);
    }
    let metadata = &encoded[HEADER_LEN..HEADER_LEN + metadata_len];
    let data = &encoded[HEADER_LEN + metadata_len..];

    let frame = match kind {
        KIND_INPUT => {
            require_data(data)?;
            let metadata: DataMetadata = parse(metadata)?;
            PtyWireFrame::Input(PtyInputFrame {
                stream_id: metadata.stream_id,
                execution_generation: metadata.execution_generation,
                session_target_id: metadata.session_target_id,
                registration_generation: metadata.registration_generation,
                worker_incarnation: metadata.worker_incarnation,
                sequence: metadata.sequence,
                data: data.to_vec(),
            })
        }
        KIND_OUTPUT => {
            require_data(data)?;
            let metadata: DataMetadata = parse(metadata)?;
            PtyWireFrame::Output(PtyOutputFrame {
                stream_id: metadata.stream_id,
                execution_generation: metadata.execution_generation,
                session_target_id: metadata.session_target_id,
                registration_generation: metadata.registration_generation,
                worker_incarnation: metadata.worker_incarnation,
                sequence: metadata.sequence,
                data: data.to_vec(),
            })
        }
        KIND_RESIZE => {
            reject_data(data)?;
            PtyWireFrame::Resize(parse(metadata)?)
        }
        KIND_CANCEL => {
            reject_data(data)?;
            PtyWireFrame::Cancel(parse(metadata)?)
        }
        KIND_OPENED => {
            reject_data(data)?;
            PtyWireFrame::Opened(parse(metadata)?)
        }
        KIND_CLOSED => {
            reject_data(data)?;
            PtyWireFrame::Closed(parse(metadata)?)
        }
        other => return Err(PtyWireError::UnknownKind(other)),
    };
    validate(&frame)?;
    Ok(frame)
}

fn validate(frame: &PtyWireFrame) -> Result<(), PtyWireError> {
    let result = match frame {
        PtyWireFrame::Input(frame) => frame.validate(),
        PtyWireFrame::Resize(frame) => frame.validate(),
        PtyWireFrame::Cancel(frame) => frame.validate(),
        PtyWireFrame::Opened(frame) => frame.validate(),
        PtyWireFrame::Output(frame) => frame.validate(),
        PtyWireFrame::Closed(frame) => frame.validate(),
    };
    result.map_err(PtyWireError::InvalidFrame)
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>, PtyWireError> {
    serde_json::to_vec(value).map_err(|_| PtyWireError::InvalidMetadata)
}

fn parse<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, PtyWireError> {
    serde_json::from_slice(bytes).map_err(|_| PtyWireError::InvalidMetadata)
}

fn require_data(data: &[u8]) -> Result<(), PtyWireError> {
    if data.is_empty() {
        Err(PtyWireError::MissingData)
    } else {
        Ok(())
    }
}

fn reject_data(data: &[u8]) -> Result<(), PtyWireError> {
    if data.is_empty() {
        Ok(())
    } else {
        Err(PtyWireError::UnexpectedData)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> PtyInputFrame {
        PtyInputFrame {
            stream_id: "stream-1".into(),
            execution_generation: "generation-1".into(),
            session_target_id: "session-1".into(),
            registration_generation: 7,
            worker_incarnation: 9,
            sequence: 11,
            data: vec![0, 0xff, b'\n'],
        }
    }

    #[test]
    fn opaque_input_round_trips_outside_metadata() {
        let encoded = encode(&PtyWireFrame::Input(input())).unwrap();
        assert_eq!(&encoded[..4], b"LPTY");
        assert_eq!(encoded[4], PTY_WIRE_VERSION);
        assert_eq!(encoded[5], KIND_INPUT);
        let metadata_len = u32::from_le_bytes(encoded[8..12].try_into().unwrap()) as usize;
        let data_len = u32::from_le_bytes(encoded[12..16].try_into().unwrap()) as usize;
        assert_eq!(data_len, 3);
        assert_eq!(&encoded[HEADER_LEN + metadata_len..], &[0, 0xff, b'\n']);
        assert_eq!(decode(&encoded).unwrap(), PtyWireFrame::Input(input()));
    }

    #[test]
    fn trailing_or_oversized_data_is_rejected() {
        let mut encoded = encode(&PtyWireFrame::Input(input())).unwrap();
        encoded.push(1);
        assert_eq!(decode(&encoded), Err(PtyWireError::LengthMismatch));

        let mut header = vec![0; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4] = PTY_WIRE_VERSION;
        header[5] = KIND_INPUT;
        header[12..16].copy_from_slice(&((MAX_PTY_DATA_FRAME_BYTES + 1) as u32).to_le_bytes());
        assert_eq!(decode(&header), Err(PtyWireError::DataTooLarge));
    }
}
