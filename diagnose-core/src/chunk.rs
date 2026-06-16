//! Serialize an [`EvidenceSnapshot`] into the chunked remote-collect response
//! and reassemble it on the other side.
//!
//! The edge (B) serializes the whole snapshot **once** to JSON bytes, slices
//! those bytes (so the serialization is canonical — A reorders and concatenates
//! raw bytes without re-serializing), base64-encodes each slice, and ships one
//! [`CollectResponseChunk`] per slice. The final chunk carries the total byte
//! length and the hex SHA-256 of the whole stream. A
//! ([`SnapshotReassembler`]) verifies ordering, length, and hash before
//! deserializing.

use desk_agent_protocol::diagnose::CollectResponseChunk;
use desk_agent_protocol::evidence::EvidenceSnapshot;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

/// Why a chunked snapshot could not be encoded or reassembled.
#[derive(Debug)]
pub enum ChunkError {
    /// The snapshot did not serialize to JSON.
    Encode(serde_json::Error),
    /// A chunk's base64 payload did not decode.
    Base64(base64::DecodeError),
    /// The reassembled bytes did not deserialize to a snapshot.
    Decode(serde_json::Error),
    /// No chunks were received.
    Empty,
    /// A chunk arrived with an unexpected `seq` (not strictly increasing from 0).
    SeqGap { expected: u32, got: u32 },
    /// The final chunk was missing (no chunk had `last = true`).
    MissingFinal,
    /// The reassembled byte length did not match the declared `total_len`.
    TotalLenMismatch { declared: u64, actual: u64 },
    /// The final chunk carried no SHA-256.
    MissingHash,
    /// The reassembled bytes' SHA-256 did not match the declared hash.
    HashMismatch { declared: String, actual: String },
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Encode(e) => write!(f, "snapshot encode failed: {e}"),
            ChunkError::Base64(e) => write!(f, "chunk base64 decode failed: {e}"),
            ChunkError::Decode(e) => write!(f, "snapshot decode failed: {e}"),
            ChunkError::Empty => write!(f, "no chunks received"),
            ChunkError::SeqGap { expected, got } => {
                write!(f, "chunk sequence gap: expected {expected}, got {got}")
            }
            ChunkError::MissingFinal => write!(f, "no final chunk (last=true) received"),
            ChunkError::TotalLenMismatch { declared, actual } => {
                write!(f, "total_len mismatch: declared {declared}, got {actual}")
            }
            ChunkError::MissingHash => write!(f, "final chunk carried no sha256"),
            ChunkError::HashMismatch { declared, actual } => {
                write!(f, "sha256 mismatch: declared {declared}, got {actual}")
            }
        }
    }
}

impl std::error::Error for ChunkError {}

/// Hex SHA-256 of a byte slice (lowercase, no separators).
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Raw byte slice size per chunk derived from the base64 text budget: base64
/// inflates 3 bytes into 4 chars, so `limit` base64 chars hold `limit * 3 / 4`
/// raw bytes. Rounded down to a multiple of 3 so no chunk ends mid-group (clean
/// base64 per chunk; reassembly concatenates raw bytes regardless).
fn raw_chunk_size(base64_limit: usize) -> usize {
    let raw = base64_limit / 4 * 3;
    (raw / 3 * 3).max(3)
}

/// Serialize `snapshot` and split it into chunks whose base64 payload stays
/// within `base64_limit` characters. Always returns at least one chunk (an empty
/// snapshot still produces a single `last = true` chunk with an empty payload).
pub fn chunk_snapshot(
    request_id: &str,
    snapshot: &EvidenceSnapshot,
    base64_limit: usize,
) -> Result<Vec<CollectResponseChunk>, ChunkError> {
    let bytes = serde_json::to_vec(snapshot).map_err(ChunkError::Encode)?;
    let total_len = bytes.len() as u64;
    let hash = sha256_hex(&bytes);
    let step = raw_chunk_size(base64_limit);

    let mut chunks = Vec::new();
    let mut seq: u32 = 0;
    let mut offset = 0usize;
    loop {
        let end = (offset + step).min(bytes.len());
        let slice = &bytes[offset..end];
        let last = end >= bytes.len();
        chunks.push(CollectResponseChunk {
            request_id: request_id.to_string(),
            seq,
            last,
            total_len,
            payload_b64: BASE64.encode(slice),
            sha256: if last { Some(hash.clone()) } else { None },
        });
        if last {
            break;
        }
        offset = end;
        seq += 1;
    }
    Ok(chunks)
}

/// Stateful accumulator for B→A evidence chunks. The manager feeds chunks in as
/// they arrive (they ride the signaling socket in order) and finishes once the
/// `last` chunk has been seen.
#[derive(Default)]
pub struct SnapshotReassembler {
    buf: Vec<u8>,
    next_seq: u32,
    total_len: Option<u64>,
    hash: Option<String>,
    seen_final: bool,
}

impl SnapshotReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the final chunk has been accepted.
    pub fn is_complete(&self) -> bool {
        self.seen_final
    }

    /// Accept one chunk. Chunks must arrive in `seq` order starting at 0.
    pub fn push(&mut self, chunk: &CollectResponseChunk) -> Result<(), ChunkError> {
        if chunk.seq != self.next_seq {
            return Err(ChunkError::SeqGap {
                expected: self.next_seq,
                got: chunk.seq,
            });
        }
        let decoded = BASE64
            .decode(chunk.payload_b64.as_bytes())
            .map_err(ChunkError::Base64)?;
        self.buf.extend_from_slice(&decoded);
        self.next_seq += 1;
        self.total_len = Some(chunk.total_len);
        if chunk.last {
            self.seen_final = true;
            self.hash = chunk.sha256.clone();
        }
        Ok(())
    }

    /// Finalize: verify the final chunk was seen, the length and hash match, and
    /// deserialize the snapshot.
    pub fn finish(self) -> Result<EvidenceSnapshot, ChunkError> {
        if self.next_seq == 0 {
            return Err(ChunkError::Empty);
        }
        if !self.seen_final {
            return Err(ChunkError::MissingFinal);
        }
        if let Some(declared) = self.total_len
            && declared != self.buf.len() as u64
        {
            return Err(ChunkError::TotalLenMismatch {
                declared,
                actual: self.buf.len() as u64,
            });
        }
        let declared_hash = self.hash.ok_or(ChunkError::MissingHash)?;
        let actual_hash = sha256_hex(&self.buf);
        if declared_hash != actual_hash {
            return Err(ChunkError::HashMismatch {
                declared: declared_hash,
                actual: actual_hash,
            });
        }
        let json = std::str::from_utf8(&self.buf)
            .map_err(|e| ChunkError::Decode(serde_json::Error::custom_utf8(e)))?;
        EvidenceSnapshot::from_json(json).map_err(ChunkError::Decode)
    }
}

/// Internal helper so a UTF-8 error can be surfaced as a `serde_json::Error`
/// without an extra error variant.
trait CustomUtf8 {
    fn custom_utf8(e: std::str::Utf8Error) -> serde_json::Error;
}
impl CustomUtf8 for serde_json::Error {
    fn custom_utf8(e: std::str::Utf8Error) -> serde_json::Error {
        use serde::de::Error;
        serde_json::Error::custom(format!("reassembled bytes are not UTF-8: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::diagnose::CollectResponseChunk;
    use desk_agent_protocol::{
        AgentOutcome, Capability, OperationOutput, ProcessEntry, ProcessListOutput,
        ReadContextOutput,
    };

    fn snapshot(n: usize) -> EvidenceSnapshot {
        let processes = (0..n)
            .map(|i| ProcessEntry {
                pid: i as u32,
                name: format!("process-number-{i}-with-a-longish-name"),
                cpu_percent: 1.0,
                memory_bytes: 1000,
                user: Some("svc\\app".into()),
                command_line_redacted: false,
            })
            .collect();
        let outcome = AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ProcessList(ProcessListOutput {
                processes,
                truncated: false,
            }),
        ));
        EvidenceSnapshot::record(
            "live",
            "many processes",
            "2026-06-16T00:00:00Z",
            vec![(Capability::ProcessList, outcome)],
        )
    }

    fn reassemble(chunks: &[CollectResponseChunk]) -> Result<EvidenceSnapshot, ChunkError> {
        let mut r = SnapshotReassembler::new();
        for c in chunks {
            r.push(c)?;
        }
        r.finish()
    }

    /// A small snapshot fits in one chunk and round-trips exactly.
    #[test]
    fn single_chunk_round_trip() {
        let snap = snapshot(1);
        let chunks = chunk_snapshot("req", &snap, 1024 * 1024).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].last);
        assert!(chunks[0].sha256.is_some());
        assert_eq!(reassemble(&chunks).unwrap(), snap);
    }

    /// A large snapshot splits into multiple ordered chunks and round-trips.
    #[test]
    fn multi_chunk_round_trip() {
        let snap = snapshot(2000);
        // Tiny base64 budget forces many chunks.
        let chunks = chunk_snapshot("req", &snap, 256).unwrap();
        assert!(chunks.len() > 1, "expected multiple chunks");
        // Only the final chunk is `last` and carries the hash.
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.seq, i as u32);
            let is_last = i == chunks.len() - 1;
            assert_eq!(c.last, is_last);
            assert_eq!(c.sha256.is_some(), is_last);
            assert!(c.payload_b64.len() <= 256);
        }
        assert_eq!(reassemble(&chunks).unwrap(), snap);
    }

    /// An out-of-order chunk is rejected.
    #[test]
    fn out_of_order_chunk_rejected() {
        let snap = snapshot(2000);
        let chunks = chunk_snapshot("req", &snap, 256).unwrap();
        let mut r = SnapshotReassembler::new();
        r.push(&chunks[0]).unwrap();
        let err = r.push(&chunks[2]).unwrap_err();
        assert!(matches!(err, ChunkError::SeqGap { .. }));
    }

    /// A corrupted final hash is caught.
    #[test]
    fn tampered_hash_rejected() {
        let snap = snapshot(1);
        let mut chunks = chunk_snapshot("req", &snap, 1024 * 1024).unwrap();
        chunks[0].sha256 = Some("deadbeef".into());
        let err = reassemble(&chunks).unwrap_err();
        assert!(matches!(err, ChunkError::HashMismatch { .. }));
    }

    /// Tampered payload bytes are caught by the hash even if length still
    /// matches.
    #[test]
    fn tampered_payload_rejected() {
        let snap = snapshot(1);
        let mut chunks = chunk_snapshot("req", &snap, 1024 * 1024).unwrap();
        // Flip the payload to a same-length-ish different value; reassembly fails
        // on hash (or decode), never silently accepts.
        chunks[0].payload_b64 = BASE64.encode(b"{\"not\":\"a snapshot at all really\"}");
        let err = reassemble(&chunks).unwrap_err();
        assert!(matches!(
            err,
            ChunkError::HashMismatch { .. } | ChunkError::TotalLenMismatch { .. }
        ));
    }

    /// Missing final chunk is reported.
    #[test]
    fn missing_final_rejected() {
        let snap = snapshot(2000);
        let chunks = chunk_snapshot("req", &snap, 256).unwrap();
        let mut r = SnapshotReassembler::new();
        for c in &chunks[..chunks.len() - 1] {
            r.push(c).unwrap();
        }
        assert!(matches!(r.finish().unwrap_err(), ChunkError::MissingFinal));
    }

    /// No chunks at all is an error.
    #[test]
    fn empty_rejected() {
        let r = SnapshotReassembler::new();
        assert!(matches!(r.finish().unwrap_err(), ChunkError::Empty));
    }
}
