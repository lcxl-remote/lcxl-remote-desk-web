//! Remote tool RPC wire types (central orchestrator A ↔ edge B).
//!
//! When the agentic loop runs on the manager (the central-brain runtime), a tool
//! call cannot run in-process: the device is the edge. The manager ships one
//! [`RemoteToolRequest`] to the edge over the already-established B→A signaling
//! socket and the edge replies with a chunked [`RemoteToolResponse`] carrying the
//! serialized, **already-redacted** [`crate::AgentOutcome`]. The edge is the sole
//! party permitted to answer a request and re-runs its own capability gate before
//! running anything, so it keeps final say over what leaves the machine.
//!
//! This mirrors the diagnose remote-collect RPC ([`crate::diagnose`]) but carries
//! a single capability call (a full server-stamped [`crate::AgentEnvelope`]),
//! not a diagnosis intent. The chunk framing is identical (byte-sliced, base64,
//! length + final SHA-256) so the manager reassembler stays the same shape; the
//! reassembler additionally enforces a hard upper bound on the declared length
//! *before* allocating (§8.2), since a remote tool result is attacker-influenced
//! in a way the trusted diagnose snapshot is not.
//!
//! These ride the signaling socket as JSON, like [`crate::diagnose::CollectResponse`],
//! so they derive only `serde` (no `wincode` / `utoipa`).

use serde::{Deserialize, Serialize};

use crate::{AgentEnvelope, AgentError};

/// Hard upper bound on the total (pre-base64) byte length of a reassembled remote
/// tool result. The reassembler rejects a first chunk whose declared `total_len`
/// exceeds this **before** allocating, so a forged/oversized declaration cannot
/// drive an unbounded allocation. Edges already cap their own output (per-read
/// truncation), so this is a defensive ceiling, not the expected size. The
/// concrete value is tuned against measurement; the semantics (checked before
/// allocation) are the contract.
pub const MAX_REMOTE_TOOL_RESULT_BYTES: u64 = 8 * 1024 * 1024;

/// Upper bound on the base64 `payload_b64` slice in a single
/// [`RemoteToolResponseChunk`]. Held below the signaling WebSocket's per-frame
/// continuation cap ([`crate::diagnose::SIGNALING_FRAME_LIMIT`]) with headroom for
/// the surrounding JSON + `SignalingModel` envelope, exactly as the collect chunk
/// budget is.
pub const REMOTE_TOOL_CHUNK_PAYLOAD_LIMIT: usize = 768 * 1024;

/// How long the manager waits for an edge to answer a [`RemoteToolRequest`]
/// (all chunks) before it abandons the call and fails (or retries with a fresh
/// attempt id, §8.3) the tool step.
pub const REMOTE_TOOL_TIMEOUT_SECS: u64 = 60;

/// A→B: run one capability call on the edge for the agentic loop.
///
/// `tool_call_id` correlates the result back to the model's tool call inside the
/// turn; `request_id` correlates the chunked response frames (and, on the manager,
/// the cross-instance pending/result keys). The `envelope` is the full
/// server-stamped call — every trusted field (`actor` / `scope` / `target` /
/// `caller`) is set by the manager, never the edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteToolRequest {
    /// Correlates the response chunks (and cross-instance routing keys) back to
    /// this request.
    pub request_id: String,
    /// The model tool-call id this result answers (loop-level correlation).
    pub tool_call_id: String,
    /// The server-stamped capability call the edge should invoke.
    pub envelope: AgentEnvelope,
}

/// One chunk of a B→A remote tool result.
///
/// The edge serializes the whole [`crate::AgentOutcome`] **once** to JSON bytes,
/// slices those bytes on byte boundaries, and base64-encodes each slice. The
/// manager reassembles by ordering on `seq`, concatenating the decoded bytes,
/// verifying `total_len` and the final `sha256`, then deserializing. Field shape
/// matches [`crate::diagnose::CollectResponseChunk`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteToolResponseChunk {
    /// Correlates back to the originating [`RemoteToolRequest`].
    pub request_id: String,
    /// Zero-based, monotonic chunk index.
    pub seq: u32,
    /// Whether this is the final chunk.
    pub last: bool,
    /// Total length, in bytes, of the full (pre-base64) JSON byte stream.
    pub total_len: u64,
    /// Base64 of this chunk's byte slice.
    pub payload_b64: String,
    /// Hex SHA-256 of the full byte stream. Set only on the final chunk
    /// (`last = true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// B→A: the tool call failed wholesale before producing a result (gate denied,
/// redaction failed, a fatal invocation error). Distinct from an
/// [`crate::AgentOutcome::Err`] that rides inside a completed (chunked) result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteToolResponseError {
    /// Correlates back to the originating [`RemoteToolRequest`].
    pub request_id: String,
    /// The model-safe failure. The edge sets `safe_for_model` appropriately so the
    /// manager never leaks policy detail into the prompt.
    pub error: AgentError,
}

/// B→A response frame for a [`RemoteToolRequest`]: either a chunk of the result or
/// a wholesale failure. Carried by one `SignalingType` variant so the manager has
/// a single consume path, like [`crate::diagnose::CollectResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteToolResponse {
    Chunk(RemoteToolResponseChunk),
    Error(RemoteToolResponseError),
}

impl RemoteToolResponse {
    /// The `request_id` this response correlates to, regardless of variant.
    pub fn request_id(&self) -> &str {
        match self {
            RemoteToolResponse::Chunk(c) => &c.request_id,
            RemoteToolResponse::Error(e) => &e.request_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActorRef, ActorType, AgentErrorKind, AgentOperation, AgentScope, CallerRef, CallerType,
        Capability, ContextKind, ExecutionMode, OperationInput, ProtocolVersion, ReadContextInput,
        RequestId, RiskLevel, SystemInfoParams, TargetRef,
    };

    fn sample_envelope() -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId("req-1".into()),
            parent_task_id: None,
            target: TargetRef {
                device_id: "dev-1".into(),
                session_id: None,
                worker_id: None,
            },
            actor: ActorRef {
                actor_type: ActorType::User,
                actor_id: "actor-1".into(),
                tenant_id: None,
            },
            caller: CallerRef {
                caller_type: CallerType::AiModel,
                model_provider: Some("example".into()),
                model_name: Some("m".into()),
                adapter: Some("lcxl-openai-tools".into()),
            },
            scope: AgentScope {
                granted: vec![Capability::SystemInfo],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            operation: AgentOperation {
                risk_hint: Some(RiskLevel::Low),
                input: OperationInput::ReadContext(ReadContextInput {
                    kind: ContextKind::SystemInfo(SystemInfoParams::default()),
                }),
            },
            audit: crate::AuditMeta {
                approval_id: None,
                reason: None,
            },
        }
    }

    #[test]
    fn remote_tool_request_round_trips() {
        let req = RemoteToolRequest {
            request_id: "rt-1".into(),
            tool_call_id: "call-1".into(),
            envelope: sample_envelope(),
        };
        let json = serde_json::to_string(&req).expect("encode");
        let back: RemoteToolRequest = serde_json::from_str(&json).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn remote_tool_response_chunk_omits_sha_when_absent_and_correlates() {
        let chunk = RemoteToolResponseChunk {
            request_id: "rt-1".into(),
            seq: 0,
            last: false,
            total_len: 10,
            payload_b64: "AAAA".into(),
            sha256: None,
        };
        let json = serde_json::to_string(&chunk).expect("encode");
        assert!(!json.contains("sha256"));
        let resp = RemoteToolResponse::Chunk(chunk);
        let back: RemoteToolResponse =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(resp, back);
        assert_eq!(resp.request_id(), "rt-1");
    }

    #[test]
    fn remote_tool_response_error_round_trips_and_correlates() {
        let resp = RemoteToolResponse::Error(RemoteToolResponseError {
            request_id: "rt-2".into(),
            error: AgentError {
                kind: AgentErrorKind::PermissionDenied,
                message: "denied".into(),
                retryable: false,
                safe_for_model: true,
            },
        });
        let back: RemoteToolResponse =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(resp, back);
        assert_eq!(resp.request_id(), "rt-2");
    }

    /// A maximally-sized chunk, serialized as a `RemoteToolResponse`, stays under
    /// the signaling WebSocket's continuation cap with room for the
    /// `SignalingModel` envelope — so a full chunk frame cannot overflow the WS
    /// codec, exactly as the collect chunk budget guarantees.
    #[test]
    fn max_chunk_frame_stays_under_signaling_cap() {
        let chunk = RemoteToolResponseChunk {
            request_id: "00000000-0000-0000-0000-000000000000".into(),
            seq: u32::MAX,
            last: true,
            total_len: u64::MAX,
            payload_b64: "A".repeat(REMOTE_TOOL_CHUNK_PAYLOAD_LIMIT),
            sha256: Some("f".repeat(64)),
        };
        let json = serde_json::to_string(&RemoteToolResponse::Chunk(chunk)).expect("encode");
        assert!(
            json.len() + 4096 < crate::diagnose::SIGNALING_FRAME_LIMIT,
            "serialized chunk frame ({} bytes) too close to the {}-byte cap",
            json.len(),
            crate::diagnose::SIGNALING_FRAME_LIMIT
        );
    }
}
