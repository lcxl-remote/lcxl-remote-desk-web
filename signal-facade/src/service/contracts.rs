//! Extension contracts used by the signaling handler.

use super::*;
use async_trait::async_trait;

/// One wire role per signaling discriminant. Streaming responses remain
/// responses even when their frames intentionally omit `response_state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalingRole {
    Request,
    Response,
    Command,
    Notification,
    WebRtc,
    Meta,
}

/// Exhaustive role contract. Adding a variant without classifying it is a
/// compile error, while the accompanying iteration tests protect discriminants
/// and request/response separation.
pub fn signaling_role(t: SignalingType) -> SignalingRole {
    use SignalingRole::*;
    match t {
        SignalingType::SendHeartbeat
        | SignalingType::FetchConnections
        | SignalingType::RequestRemoteAccess
        | SignalingType::RequireControl
        | SignalingType::ReleaseControl
        | SignalingType::ChangeDisplaySettings
        | SignalingType::SetPrivateScreenVisibility
        | SignalingType::RequestSupportCode
        | SignalingType::UpdateRemoteAccessLock
        | SignalingType::TerminateRemotePeer
        | SignalingType::RetryMediaPipeline
        | SignalingType::ApplyRemoteSessionSettings
        | SignalingType::GetSystemInfo
        | SignalingType::ListFiles
        | SignalingType::DeleteFile
        | SignalingType::StartTerminal
        | SignalingType::ListTerminalCommands
        | SignalingType::InvokeAgentCapability
        | SignalingType::DiagnoseDevice
        | SignalingType::PreviewExecution
        | SignalingType::ResolveExecution
        | SignalingType::CollectEvidence
        | SignalingType::ExecuteEdgePlan
        | SignalingType::InvokeRemoteTool
        | SignalingType::AskTerminalCopilot
        | SignalingType::GenerateTerminalCompletions
        | SignalingType::ControlExecution
        | SignalingType::DispatchComputerAction
        | SignalingType::CancelComputerAction
        | SignalingType::QueryComputerActionState
        | SignalingType::AskDeviceAssistant
        | SignalingType::GetDeviceAssistantCapabilities
        | SignalingType::UpdateDeviceAssistantContext
        | SignalingType::UpdateDeviceAssistantObjectContext => Request,

        SignalingType::HeartbeatAcknowledged
        | SignalingType::ConnectionsFetched
        | SignalingType::RemoteAccessInitialized
        | SignalingType::ControlAccepted
        | SignalingType::ControlDenied
        | SignalingType::ControlReleased
        | SignalingType::DisplaySettingsChanged
        | SignalingType::PrivateScreenVisibilitySet
        | SignalingType::SupportCodeIssued
        | SignalingType::RemoteAccessLockUpdated
        | SignalingType::RemotePeerTerminationResolved
        | SignalingType::MediaPipelineRetryCompleted
        | SignalingType::RemoteSessionSettingsApplied
        | SignalingType::SystemInfoRetrieved
        | SignalingType::FilesListed
        | SignalingType::FileDeleted
        | SignalingType::TerminalStarted
        | SignalingType::TerminalCommandsListed
        | SignalingType::AgentCapabilityCompleted
        | SignalingType::DiagnosisUpdated
        | SignalingType::ExecutionPreviewGenerated
        | SignalingType::ExecutionCompleted
        | SignalingType::EvidenceCollectionUpdated
        | SignalingType::EdgeExecutionCompleted
        | SignalingType::RemoteToolOutputUpdated
        | SignalingType::TerminalCopilotUpdated
        | SignalingType::TerminalCompletionsGenerated
        | SignalingType::ExecutionStateReported
        | SignalingType::ComputerActionStarted
        | SignalingType::ComputerActionCompleted
        | SignalingType::ComputerActionStateReported
        | SignalingType::DeviceAssistantUpdated
        | SignalingType::DeviceAssistantCapabilitiesUpdated
        | SignalingType::DeviceAssistantContextUpdated
        | SignalingType::DeviceAssistantObjectContextUpdated => Response,

        SignalingType::RevokeSupportCode
        | SignalingType::RevokeAccessGrant
        | SignalingType::CloseRemoteSession
        | SignalingType::UpdateAdaptiveVideoQuality
        | SignalingType::SendTerminalInput
        | SignalingType::ResizeTerminal
        | SignalingType::CloseTerminal
        | SignalingType::CancelDiagnosis
        | SignalingType::ReportAiAuditEvent
        | SignalingType::SyncCommandTemplates
        | SignalingType::CancelTerminalCopilot
        | SignalingType::SyncCommandBlocklist
        | SignalingType::CancelDeviceAssistant => Command,

        SignalingType::ConnectionRemoved
        | SignalingType::PrivateScreenStateChanged
        | SignalingType::AudioPlaybackFailed
        | SignalingType::MediaPipelineStateChanged
        | SignalingType::SystemAudioCaptureStateChanged
        | SignalingType::TerminalOutputProduced
        | SignalingType::TerminalClosed
        | SignalingType::DesktopSwitching
        | SignalingType::DesktopReady
        | SignalingType::ExecutionProgressUpdated
        | SignalingType::ComputerUseReadinessUpdated => Notification,

        SignalingType::Offer | SignalingType::Answer | SignalingType::IceCandidate => WebRtc,
        SignalingType::Error | SignalingType::Unknown => Meta,
    }
}

/// Return the wire response type that completes a request. Requests with
/// multiple normal outcomes use the failure-shaped outcome for generic business
/// errors (`RequireControl` resolves to `ControlDenied`). One-way commands and
/// WebRTC standard frames return `None`.
pub fn response_type_for_request(t: SignalingType) -> Option<SignalingType> {
    Some(match t {
        SignalingType::SendHeartbeat => SignalingType::HeartbeatAcknowledged,
        SignalingType::FetchConnections => SignalingType::ConnectionsFetched,
        SignalingType::RequestRemoteAccess => SignalingType::RemoteAccessInitialized,
        SignalingType::RequireControl => SignalingType::ControlDenied,
        SignalingType::ReleaseControl => SignalingType::ControlReleased,
        SignalingType::ChangeDisplaySettings => SignalingType::DisplaySettingsChanged,
        SignalingType::SetPrivateScreenVisibility => SignalingType::PrivateScreenVisibilitySet,
        SignalingType::RequestSupportCode => SignalingType::SupportCodeIssued,
        SignalingType::UpdateRemoteAccessLock => SignalingType::RemoteAccessLockUpdated,
        SignalingType::TerminateRemotePeer => SignalingType::RemotePeerTerminationResolved,
        SignalingType::RetryMediaPipeline => SignalingType::MediaPipelineRetryCompleted,
        SignalingType::ApplyRemoteSessionSettings => SignalingType::RemoteSessionSettingsApplied,
        SignalingType::GetSystemInfo => SignalingType::SystemInfoRetrieved,
        SignalingType::ListFiles => SignalingType::FilesListed,
        SignalingType::DeleteFile => SignalingType::FileDeleted,
        SignalingType::StartTerminal => SignalingType::TerminalStarted,
        SignalingType::ListTerminalCommands => SignalingType::TerminalCommandsListed,
        SignalingType::InvokeAgentCapability => SignalingType::AgentCapabilityCompleted,
        SignalingType::DiagnoseDevice => SignalingType::DiagnosisUpdated,
        SignalingType::PreviewExecution => SignalingType::ExecutionPreviewGenerated,
        SignalingType::ResolveExecution => SignalingType::ExecutionCompleted,
        SignalingType::CollectEvidence => SignalingType::EvidenceCollectionUpdated,
        SignalingType::ExecuteEdgePlan => SignalingType::EdgeExecutionCompleted,
        SignalingType::InvokeRemoteTool => SignalingType::RemoteToolOutputUpdated,
        SignalingType::AskTerminalCopilot => SignalingType::TerminalCopilotUpdated,
        SignalingType::GenerateTerminalCompletions => SignalingType::TerminalCompletionsGenerated,
        SignalingType::ControlExecution => SignalingType::ExecutionStateReported,
        SignalingType::DispatchComputerAction => SignalingType::ComputerActionCompleted,
        SignalingType::CancelComputerAction | SignalingType::QueryComputerActionState => {
            SignalingType::ComputerActionStateReported
        }
        SignalingType::AskDeviceAssistant => SignalingType::DeviceAssistantUpdated,
        SignalingType::GetDeviceAssistantCapabilities => {
            SignalingType::DeviceAssistantCapabilitiesUpdated
        }
        SignalingType::UpdateDeviceAssistantContext => SignalingType::DeviceAssistantContextUpdated,
        SignalingType::UpdateDeviceAssistantObjectContext => {
            SignalingType::DeviceAssistantObjectContextUpdated
        }
        _ => return None,
    })
}

/// All response types a request may produce. The first item is not used as a
/// generic error choice; callers that synthesize business errors use
/// [`response_type_for_request`].
pub fn response_types_for_request(t: SignalingType) -> &'static [SignalingType] {
    use SignalingType::*;
    match t {
        SendHeartbeat => &[HeartbeatAcknowledged],
        FetchConnections => &[ConnectionsFetched],
        RequestRemoteAccess => &[RemoteAccessInitialized],
        RequireControl => &[ControlAccepted, ControlDenied],
        ReleaseControl => &[ControlReleased],
        ChangeDisplaySettings => &[DisplaySettingsChanged],
        SetPrivateScreenVisibility => &[PrivateScreenVisibilitySet],
        RequestSupportCode => &[SupportCodeIssued],
        UpdateRemoteAccessLock => &[RemoteAccessLockUpdated],
        TerminateRemotePeer => &[RemotePeerTerminationResolved],
        RetryMediaPipeline => &[MediaPipelineRetryCompleted],
        ApplyRemoteSessionSettings => &[RemoteSessionSettingsApplied],
        GetSystemInfo => &[SystemInfoRetrieved],
        ListFiles => &[FilesListed],
        DeleteFile => &[FileDeleted],
        StartTerminal => &[TerminalStarted],
        ListTerminalCommands => &[TerminalCommandsListed],
        InvokeAgentCapability => &[AgentCapabilityCompleted],
        DiagnoseDevice => &[DiagnosisUpdated],
        PreviewExecution => &[ExecutionPreviewGenerated],
        ResolveExecution => &[ExecutionCompleted],
        CollectEvidence => &[EvidenceCollectionUpdated],
        ExecuteEdgePlan => &[EdgeExecutionCompleted],
        InvokeRemoteTool => &[RemoteToolOutputUpdated],
        AskTerminalCopilot => &[TerminalCopilotUpdated],
        GenerateTerminalCompletions => &[TerminalCompletionsGenerated],
        ControlExecution => &[ExecutionStateReported],
        DispatchComputerAction => &[ComputerActionStarted, ComputerActionCompleted],
        CancelComputerAction | QueryComputerActionState => &[ComputerActionStateReported],
        AskDeviceAssistant => &[DeviceAssistantUpdated],
        GetDeviceAssistantCapabilities => &[DeviceAssistantCapabilitiesUpdated],
        UpdateDeviceAssistantContext => &[DeviceAssistantContextUpdated],
        UpdateDeviceAssistantObjectContext => &[DeviceAssistantObjectContextUpdated],
        _ => &[],
    }
}

#[cfg(test)]
mod signaling_contract_tests {
    use super::*;
    use std::collections::HashSet;
    use strum::IntoEnumIterator;

    #[test]
    fn every_discriminant_is_unique_and_has_one_role() {
        let mut values = HashSet::new();
        for signaling_type in SignalingType::iter() {
            assert!(values.insert(signaling_type as i32));
            let _ = signaling_role(signaling_type);
        }
    }

    #[test]
    fn requests_declare_distinct_responses_and_all_responses_are_reachable() {
        let mut declared = HashSet::new();
        for request in
            SignalingType::iter().filter(|t| signaling_role(*t) == SignalingRole::Request)
        {
            let responses = response_types_for_request(request);
            assert!(
                !responses.is_empty(),
                "{request:?} has no declared response"
            );
            for response in responses {
                assert_ne!(request as i32, *response as i32);
                assert_eq!(signaling_role(*response), SignalingRole::Response);
                declared.insert(*response as i32);
            }
        }
        for response in
            SignalingType::iter().filter(|t| signaling_role(*t) == SignalingRole::Response)
        {
            assert!(
                declared.contains(&(response as i32)),
                "{response:?} is not declared by any request"
            );
        }
    }
}

// ====== Credential heartbeat policy ======

/// Stable reason classification for a manager credential that is no longer
/// usable. The reason remains server-side; hosts receive only the terminal or
/// suspended disposition through the error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialInvalidityReason {
    MissingToken,
    TokenExpired,
    MissingOwner,
    OwnerDeletingOrDeleted,
    Disabled,
    InactiveOwner,
    OwnerDeletionPending,
}

/// Result of revalidating the credential bound to the current manager WebSocket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialHeartbeatOutcome {
    Proof(crate::model::credential_heartbeat::ManagerCredentialHeartbeatProof),
    TerminalRevoked(CredentialInvalidityReason),
    Suspended(CredentialInvalidityReason),
    SnapshotStale,
    BackendUnavailable,
}

#[async_trait]
pub trait CredentialHeartbeatAuthorizer: Send + Sync {
    async fn authorize_heartbeat(&self, state: &ConnectionState) -> CredentialHeartbeatOutcome;
}

/// Credential behavior is selected when a signaling handler is constructed.
/// Manager token connections cannot exist without their authorizer, while OSS
/// signal and cookie/control connections remain explicitly plain.
#[derive(Clone)]
pub enum CredentialPolicy {
    Plain,
    ManagerToken(Arc<dyn CredentialHeartbeatAuthorizer>),
}

// ====== DeviceCodeService trait ======

/// Trait for device code operations.
/// Signal implements this with SQLite DB.
/// Manager can return None (no device codes in manager).
pub trait DeviceCodeService: Send + Sync {
    fn get_or_create_device_code(
        &self,
        client_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, DeskSignalFacadeError>> + Send;
}

/// A no-op implementation that always returns None
pub struct NoOpDeviceCodeService;

impl DeviceCodeService for NoOpDeviceCodeService {
    async fn get_or_create_device_code(
        &self,
        _client_id: &str,
    ) -> Result<Option<String>, DeskSignalFacadeError> {
        Ok(None)
    }
}

// ====== NodeTokenValidator trait ======

/// Trait for validating node tokens (e.g. manager API tokens).
pub trait NodeTokenValidator: Send + Sync {
    fn validate_node_token<'a>(
        &'a self,
        token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
}

// ====== ControlFrameAuthorizer trait ======

/// Outcome of authorizing a control-end AI frame before it is relayed to the
/// host.
pub enum ControlFrameOutcome {
    /// Relay this (possibly wrapped) model to the peer.
    Forward(SignalingModel),
    /// Reject the frame; an error response is returned to the sender.
    Reject {
        code: DeskErrorCode,
        message: String,
    },
    /// The authorizer fully handled the frame; do **not** relay it to the peer.
    /// The manager uses this for `Diagnose` in the thin-edge model: instead of
    /// forwarding the question to the host, it runs the orchestration centrally
    /// (asking the host only for read-only evidence). The signal server never
    /// returns this (it has no authorizer), so its relay behaviour is unchanged.
    Handled,
}

/// Authorizes (and optionally wraps) the control-end AI frames
/// (`AgentRequest` / `Diagnose` / `ConfirmExec`) during relay. The manager
/// implements this as the fleet policy decision point: it resolves the actor
/// (the sending `actor` connection) and the target host (looked up in
/// `connection_map`), evaluates the policy matrix, and wraps the frame in an
/// `AuthorizedControlPayload`. The signal server leaves this unset (no fleet
/// PDP), so frames relay unwrapped.
pub trait ControlFrameAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ControlFrameOutcome> + Send + 'a>>;
}

// ====== AccessGrantAuthorizer (RequestRemoteAccess capability-ceiling stamp) ======

/// Outcome of authorizing a `RequestRemoteAccess` before it is relayed to the host.
pub enum RequestRemoteOutcome {
    /// Relay this model to the peer. The manager / signal server returns the frame
    /// wrapped in an `AuthorizedRequestRemote` (a trusted capability-ceiling
    /// stamp); with no authorizer the plain frame is relayed unchanged.
    Forward(SignalingModel),
    /// Reject the request (default-deny: neither an owner/org authorization nor a
    /// valid grant for the target). An error response is returned to the sender.
    Reject {
        code: DeskErrorCode,
        message: String,
    },
}

/// Stamps a trusted capability ceiling onto every `RequestRemoteAccess` before it is
/// relayed to the host — the enforcement seam that lets the host drop any bare
/// `RequestRemoteAccess` on its trusted-central upstream (defense against a grant
/// session stripping its stamp to masquerade as an owner). The manager and the
/// signal server each implement it (rule 22, same shape): an owner / org
/// authorization stamps `access_ceiling: None` (full), a valid redeemed grant
/// stamps `Some(ceiling)`, and neither is a default-deny reject. The signal
/// server injects its own single-account/grant implementation; a handler with no
/// authorizer relays plainly (used where the host applies no central trust).
pub trait RequestRemoteAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RequestRemoteOutcome> + Send + 'a>>;
}

// ====== TerminalStartAuthorizer (StartTerminal capability-ceiling stamp) ======

/// Stamps a trusted capability ceiling onto a `StartTerminal` frame before it is
/// relayed to the host — the terminal analogue of [`RequestRemoteAuthorizer`]. The
/// remote terminal opens on a distinct WS connection that never does a
/// `RequestRemoteAccess`, so without this stamp the host has no admission / ceiling for it
/// (and its first door would either reject it or fall back to the host global). The
/// manager and the signal server each implement it (rule 22, same shape): an owner
/// session stamps `access_ceiling: None` (full control), a valid redeemed grant
/// stamps `Some(ceiling)`, and anything else is a default-deny reject. Reuses
/// [`RequestRemoteOutcome`]: `Forward` carries the frame wrapped in an
/// `AuthorizedTerminalStart`, `Reject` fails the terminal open. Unlike
/// `RequestRemoteAuthorizer` this is invoked directly by the terminal WS controller
/// (which builds the `StartTerminal` frame itself) rather than through the signaling
/// handler's per-type dispatch.
pub trait TerminalStartAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RequestRemoteOutcome> + Send + 'a>>;
}

// ====== OwnerPlaneAuthorizer (owner-plane management-frame gate) ======

/// Outcome of authorizing an owner-plane management frame before it is relayed to
/// the host.
pub enum OwnerPlaneOutcome {
    /// The actor is the device owner (or org-authorized); relay the frame.
    Allow,
    /// The actor is not the owner (e.g. a capped grant-holder); default-deny. An
    /// error response is returned to the sender.
    Reject {
        code: DeskErrorCode,
        message: String,
    },
}

/// Default-denies owner-plane device-management frames
/// (`GetSystemInfo` / `SystemInfoRetrieved` / `ChangeDisplaySettings`) for
/// any sender that is not the
/// target device's owner (or an org-authorized operator). These frames carry no
/// capability ceiling and are meaningful only to the owner, so a capped
/// grant-holder session must never reach them.
///
/// `Some` only in the manager, where the owner/grant distinction is computed
/// per-target (a `CookieAuth` browser is not intrinsically owner or grant — it
/// depends on the target device). The signal server leaves it unset: there the
/// same frames are already denied for code sessions inside `forward_to_peer`
/// (`is_owner_plane_management_frame`), and an owner drives its own device by
/// cookie session, so no central owner-plane gate is needed (rule 22 keeps both
/// control-end flows identical).
pub trait OwnerPlaneAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = OwnerPlaneOutcome> + Send + 'a>>;
}

// ====== AuditObserver trait ======

/// Consumes inbound `AiAuditEvent` frames for persistence. The manager
/// implements this to write the audit row (after re-deriving the trusted
/// subject from the reporting connection's `AuthContext` and applying the
/// persist-level filter); the signal server leaves it unset, so audit frames
/// are simply ignored there. `source` is the reporting connection (a
/// token-authenticated desk server).
pub trait AuditObserver: Send + Sync {
    fn on_audit_event<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== CollectObserver trait ======

/// Consumes inbound `CollectResponse` frames (chunks of an evidence snapshot, or
/// a wholesale error) from a desk-server daemon. The manager implements this to
/// route the chunk into its orchestrator's pending store, keyed by `request_id`
/// and validated against the connection that the matching `CollectRequest` was
/// pushed to; the signal server leaves it unset, so the frames are ignored there.
/// `source` is the reporting connection (a token-authenticated desk server).
pub trait CollectObserver: Send + Sync {
    fn on_collect_response<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== EdgeExecObserver trait ======

/// Consumes inbound `EdgeExecResult` frames (a structured
/// `EdgeExecDisposition` for one central-agent or fleet execution attempt) from
/// a desk-server daemon. Manager and OSS Signal route the result into their
/// respective durable task stores, keyed by the per-attempt `request_id` and
/// validated against the connection that received the matching
/// `EdgeExecRequest`. `source` is the reporting connection (a
/// token-authenticated desk server).
pub trait EdgeExecObserver: Send + Sync {
    fn on_fleet_exec_result<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== ExecStateReplyObserver trait ======

/// Consumes inbound `ExecStateReply` frames that answer a query the central brain
/// itself issued (a reconcile of an execution whose live result it lost).
/// Manager and OSS Signal feed the reply into a state-query pending store keyed
/// by execution generation and validated against the reporting connection. A
/// reply carrying a `to_connection_id` is a browser-initiated query's answer and
/// is relayed to that peer instead.
pub trait ExecStateReplyObserver: Send + Sync {
    fn on_exec_state_reply<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== RemoteToolObserver trait ======

/// Consumes inbound `RemoteToolResponse` frames (chunks of an already-redacted
/// remote read result, or a wholesale error) from a desk-server daemon. The
/// manager implements this to feed the chunk into its remote-tool pending store,
/// keyed by `request_id` and validated against the connection the matching
/// `RemoteToolRequest` was written to; the signal server leaves it unset, so the
/// frames are ignored there. `source` is the reporting connection (a
/// token-authenticated desk server).
pub trait RemoteToolObserver: Send + Sync {
    fn on_remote_tool_response<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== ComputerActionObserver trait ======

/// Consumes centrally-owned Computer Use lifecycle frames. The observer binds
/// each frame to the authenticated reporting connection and the exact pending
/// execution generation before resolving a mutation waiter.
pub trait ComputerActionObserver: Send + Sync {
    fn on_computer_action_lifecycle<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== ComputerUseReadinessObserver trait ======

/// Consumes bounded dynamic Computer Use readiness from an authenticated host.
/// Implementations derive device/connection ownership from `source`; payload
/// fields never self-assert a manager node or presence owner.
pub trait ComputerUseReadinessObserver: Send + Sync {
    fn on_computer_use_readiness<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== ForwardLifecycleObserver trait ======

/// Observes a signaling frame only after it has been delivered to its target.
/// Manager uses this narrow, wire-neutral seam for its best-effort user activity
/// timeline; OSS Signal leaves it unset. Implementations must not fail or delay
/// the primary forwarding result.
pub trait ForwardLifecycleObserver: Send + Sync {
    fn on_delivered<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== SupportCodeMinter trait ======

/// Central-brain lifecycle for temporary support codes: mint on an inbound
/// `RequestSupportCode`, revoke on an inbound `RevokeSupportCode`.
///
/// Only a central brain (the manager) implements this. On mint it resolves
/// `source`'s owner-bound device, stores a short-lived code in shared state and
/// writes the issued code onto `source`'s own session as `SupportCodeIssued`. On
/// revoke it verifies `source` owns the code's device, then drops the code so it
/// can no longer be redeemed. A plain signal server leaves this unset, so both
/// frames are ignored there (support codes are a manager feature). `source` is the
/// requesting connection — a token-authenticated desk server whose regular
/// `Server` upstream carries the frame (there is no dedicated support upstream).
pub trait SupportCodeMinter: Send + Sync {
    fn on_request_support_code<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    fn on_revoke_support_code<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// Frames that may create, extend, or exercise remote access. The central lock
/// gate resolves the target host and rejects these while its durable mirror is
/// locked. Cleanup, heartbeat, and host→central lock maintenance remain allowed.
pub(crate) fn remote_access_frame_requires_unlocked(t: SignalingType) -> bool {
    !matches!(
        t,
        SignalingType::SendHeartbeat
            | SignalingType::HeartbeatAcknowledged
            | SignalingType::FetchConnections
            | SignalingType::ConnectionsFetched
            | SignalingType::ConnectionRemoved
            | SignalingType::ReleaseControl
            | SignalingType::ControlReleased
            | SignalingType::CloseRemoteSession
            | SignalingType::CloseTerminal
            | SignalingType::RequestSupportCode
            | SignalingType::SupportCodeIssued
            | SignalingType::RevokeSupportCode
            | SignalingType::RevokeAccessGrant
            | SignalingType::UpdateRemoteAccessLock
            | SignalingType::RemoteAccessLockUpdated
            | SignalingType::TerminateRemotePeer
            | SignalingType::RemotePeerTerminationResolved
            | SignalingType::SystemInfoRetrieved
            | SignalingType::DisplaySettingsChanged
            | SignalingType::PrivateScreenVisibilitySet
            | SignalingType::MediaPipelineRetryCompleted
            | SignalingType::FilesListed
            | SignalingType::FileDeleted
            | SignalingType::TerminalCommandsListed
            | SignalingType::ReportAiAuditEvent
            | SignalingType::EvidenceCollectionUpdated
            | SignalingType::EdgeExecutionCompleted
            | SignalingType::ExecutionStateReported
            | SignalingType::RemoteToolOutputUpdated
            | SignalingType::SyncCommandTemplates
            | SignalingType::SyncCommandBlocklist
            | SignalingType::Error
            | SignalingType::Unknown
    )
}

// ====== Host remote-access lock traits ======

pub enum RemoteAccessAdmissionOutcome {
    Allow,
    Reject {
        code: DeskErrorCode,
        message: String,
    },
}

/// Central durable-lock gate evaluated before any control-end request can reach
/// a host or central orchestration path. Both OSS signal and manager install it.
pub trait RemoteAccessAdmissionAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        source: &'a ConnectionState,
        connections: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = RemoteAccessAdmissionOutcome> + Send + 'a>,
    >;
}

/// Consumes authenticated host requests that update the durable central lock
/// mirror or terminate one remote peer, then writes the matching ack to `source`.
pub trait HostRemoteAccessController: Send + Sync {
    fn on_lock_request<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    fn on_terminate_peer_request<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== FetchConnectionsResolver trait ======

/// Resolves a `FetchConnections` request into the connection list to return.
///
/// The signal server leaves this unset and the handler falls back to the local
/// connection map (single instance, correct). The manager implements it to
/// return a cluster-wide, presence-backed and scope-authorized list whose items
/// carry `device_id` / `owner_node_id` — so a control end behind a load balancer
/// sees every device regardless of which instance holds the desk-server socket.
///
/// `requester` is the asking connection (carries the trusted `AuthContext` used
/// for scoped authorization); `scope` is the parsed request payload.
pub trait FetchConnectionsResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        requester: &'a ConnectionState,
        scope: FetchConnectionsScope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<ConnectionModel>, DeskSignalFacadeError>>
                + Send
                + 'a,
        >,
    >;
}

// ====== PeerFrameRelay trait ======

/// Result of a cross-instance peer-frame relay attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayOutcome {
    /// The frame was delivered to the peer connection on its owning instance.
    Delivered,
    /// No instance currently holds the target connection (genuinely offline).
    NotFound,
}

/// Relays a signaling frame whose `to_connection_id` this instance does not hold
/// locally, by forwarding it to the instance that does.
///
/// The signal server leaves this unset — it is single-instance, so a local miss is
/// a genuine "connection not found". The manager implements it: it resolves the
/// target connection to its owning instance via the connection-location registry
/// and forwards the frame over an authenticated internal hop, where the owning
/// instance delivers it to the local peer ([`deliver_to_local_peer`]). Both relay
/// directions flow through this — browser→host (OFFER / ICE) and host→browser
/// (ANSWER / ICE) — because each frame is routed independently by its
/// `to_connection_id`, and a browser connection (absent from device presence) is
/// locatable here just like a host.
///
/// The returned future is intentionally **not** `Send`: the whole signaling stack
/// runs on the single-threaded actix runtime (`rt::spawn`) on both the manager and
/// the OSS signal server, and the manager's implementation drives the internal hop
/// with `awc` (which is `!Send`). The trait object itself is `Send + Sync` so it can
/// live in the shared handler.
pub trait PeerFrameRelay: Send + Sync {
    fn relay<'a>(
        &'a self,
        to_connection_id: &'a str,
        from_connection_id: &'a str,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<RelayOutcome, DeskSignalFacadeError>> + 'a>,
    >;
}
