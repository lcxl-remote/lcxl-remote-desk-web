//! Closed runtime observation definitions and numerical samples.

use super::{Origin, RuntimeCategory};
use desk_agent_protocol::content_safety::{
    ContentSafetyDecision, ContentSafetyStage, ContentSafetySurface, StreamRetractionReason,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAssistantTurnOrigin {
    User,
    PermissionResume,
    WorkCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAssistantTurnOutcome {
    Answered,
    Waiting,
    PermissionRequired,
    ContentRejected,
    SafetyUnavailable,
    Truncated,
    ContextWindowExceeded,
    CircuitBreak,
    ProtocolError,
    Busy,
    SubjectRejected,
    Superseded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAssistantProviderFamily {
    Browser,
    DesktopUi,
    RawInput,
    Iwork,
    Artifact,
    Communication,
    LegacyOffice,
    ApplicationLaunch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAssistantProviderCompletion {
    Verified,
    Accepted,
    Failed,
    Unknown,
    DuplicateReceipt,
    StaleReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAssistantProviderCancel {
    Requested,
    NotSent,
    AlreadyTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAssistantPermissionDecision {
    Approved,
    Narrowed,
    Denied,
    PolicyRejected,
    Unavailable,
    Replay,
    Revoked,
    RevokeReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSafetyProviderFamily {
    OpenAiCompatible,
    Anthropic,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSafetyFailureKind {
    Config,
    ModelResolution,
    ModelRevalidation,
    Capability,
    Admission,
    Heartbeat,
    Provider,
    Timeout,
    OutputTruncated,
    InvalidVerdict,
    Usage,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeDefinition {
    #[serde(rename = "action_log.persist_failure")]
    ActionLogPersistFailure,
    #[serde(rename = "admission_backend_error")]
    AdmissionBackendError,
    #[serde(rename = "admission_cancel")]
    AdmissionCancel,
    #[serde(rename = "admission_lease_loss")]
    AdmissionLeaseLoss,
    #[serde(rename = "ai_operation_generation_mismatch")]
    AiOperationGenerationMismatch,
    #[serde(rename = "ai_operation_generation_rotated")]
    AiOperationGenerationRotated,
    #[serde(rename = "ai_operation_parent_missing")]
    AiOperationParentMissing,
    #[serde(rename = "ai_operation_renewal_exhausted")]
    AiOperationRenewalExhausted,
    #[serde(rename = "ai_provider_fence_completion_failure")]
    AiProviderFenceCompletionFailure,
    #[serde(rename = "ai_provider_fence_conflict")]
    AiProviderFenceConflict,
    #[serde(rename = "ai_provider_fence_conflict_disappeared")]
    AiProviderFenceConflictDisappeared,
    #[serde(rename = "ai_provider_fence_fingerprint_mismatch")]
    AiProviderFenceFingerprintMismatch,
    #[serde(rename = "ai_provider_fence_provider_model_mismatch")]
    AiProviderFenceProviderModelMismatch,
    #[serde(rename = "background_promoted")]
    BackgroundPromoted,
    #[serde(rename = "buffer_high_water_chunks")]
    BufferHighWaterChunks,
    #[serde(rename = "compression_failed")]
    CompressionFailed,
    #[serde(rename = "compression_started")]
    CompressionStarted,
    #[serde(rename = "compression_succeeded")]
    CompressionSucceeded,
    #[serde(rename = "content_safety_fleet_global_queue_full")]
    ContentSafetyFleetGlobalQueueFull,
    #[serde(rename = "content_safety_fleet_org_rate_limited")]
    ContentSafetyFleetOrgRateLimited,
    #[serde(rename = "content_safety_global_queue_full")]
    ContentSafetyGlobalQueueFull,
    #[serde(rename = "content_safety_interactive_admission_timeout")]
    ContentSafetyInteractiveAdmissionTimeout,
    #[serde(rename = "content_safety_partial_release_timeout")]
    ContentSafetyPartialReleaseTimeout,
    #[serde(rename = "content_safety_unexpected_queue_full")]
    ContentSafetyUnexpectedQueueFull,
    #[serde(rename = "content_safety_user_queue_full")]
    ContentSafetyUserQueueFull,
    #[serde(rename = "content_safety_user_rate_limited")]
    ContentSafetyUserRateLimited,
    #[serde(rename = "content_safety_waiter_cancel_timeout")]
    ContentSafetyWaiterCancelTimeout,
    #[serde(rename = "context_reject")]
    ContextReject,
    #[serde(rename = "estimator_overflow")]
    EstimatorOverflow,
    #[serde(rename = "estimator_sample")]
    EstimatorSample,
    #[serde(rename = "free_global_admission_wait_ms")]
    FreeGlobalAdmissionWaitMs,
    #[serde(rename = "free_global_queue_depth")]
    FreeGlobalQueueDepth,
    #[serde(rename = "free_global_queue_full")]
    FreeGlobalQueueFull,
    #[serde(rename = "free_output_cap_applied")]
    FreeOutputCapApplied,
    #[serde(rename = "free_output_cap_disabled")]
    FreeOutputCapDisabled,
    #[serde(rename = "free_output_cap_enabled")]
    FreeOutputCapEnabled,
    #[serde(rename = "free_output_cap_hit")]
    FreeOutputCapHit,
    #[serde(rename = "inventory")]
    Inventory,
    #[serde(rename = "output_credit_backend_error")]
    OutputCreditBackendError,
    #[serde(rename = "output_wait_cancel")]
    OutputWaitCancel,
    #[serde(rename = "output_wait_ms")]
    OutputWaitMs,
    #[serde(rename = "permission_decided")]
    PermissionDecided,
    #[serde(rename = "permission_requested")]
    PermissionRequested,
    #[serde(rename = "posture_lock_error")]
    PostureLockError,
    #[serde(rename = "posture_lock_wait_ms")]
    PostureLockWaitMs,
    #[serde(rename = "projection")]
    Projection,
    #[serde(rename = "provider_cancelled")]
    ProviderCancelled,
    #[serde(rename = "provider_completed")]
    ProviderCompleted,
    #[serde(rename = "provider_dispatch_suppressed")]
    ProviderDispatchSuppressed,
    #[serde(rename = "provider_dispatched")]
    ProviderDispatched,
    #[serde(rename = "provider_read_cancel")]
    ProviderReadCancel,
    #[serde(rename = "registration_rate_limit_backend_error")]
    RegistrationRateLimitBackendError,
    #[serde(rename = "remote_tool_chunk_rejected")]
    RemoteToolChunkRejected,
    #[serde(rename = "remote_tool_error_rejected")]
    RemoteToolErrorRejected,
    #[serde(rename = "remote_tool_inflight")]
    RemoteToolInflight,
    #[serde(rename = "remote_tool_pending_duplicate")]
    RemoteToolPendingDuplicate,
    #[serde(rename = "remote_tool_result_consumed_bytes")]
    RemoteToolResultConsumedBytes,
    #[serde(rename = "remote_tool_result_stored_bytes")]
    RemoteToolResultStoredBytes,
    #[serde(rename = "safety_decision")]
    SafetyDecision,
    #[serde(rename = "safety_disabled")]
    SafetyDisabled,
    #[serde(rename = "safety_failure")]
    SafetyFailure,
    #[serde(rename = "safety_fleet_global_admission_wait_ms")]
    SafetyFleetGlobalAdmissionWaitMs,
    #[serde(rename = "safety_fleet_global_queue_depth")]
    SafetyFleetGlobalQueueDepth,
    #[serde(rename = "safety_fleet_global_queue_full")]
    SafetyFleetGlobalQueueFull,
    #[serde(rename = "safety_global_admission_wait_ms")]
    SafetyGlobalAdmissionWaitMs,
    #[serde(rename = "safety_global_queue_depth")]
    SafetyGlobalQueueDepth,
    #[serde(rename = "safety_global_queue_full")]
    SafetyGlobalQueueFull,
    #[serde(rename = "safety_latency")]
    SafetyLatency,
    #[serde(rename = "safety_main_call_avoided")]
    SafetyMainCallAvoided,
    #[serde(rename = "safety_retracted")]
    SafetyRetracted,
    #[serde(rename = "safety_user_admission_wait_ms")]
    SafetyUserAdmissionWaitMs,
    #[serde(rename = "safety_user_queue_depth")]
    SafetyUserQueueDepth,
    #[serde(rename = "safety_user_queue_full")]
    SafetyUserQueueFull,
    #[serde(rename = "support.admitted")]
    SupportAdmitted,
    #[serde(rename = "support.answer.not_covered")]
    SupportAnswerNotCovered,
    #[serde(rename = "support.outcome.cancelled")]
    SupportOutcomeCancelled,
    #[serde(rename = "support.outcome.completed")]
    SupportOutcomeCompleted,
    #[serde(rename = "support.outcome.failed")]
    SupportOutcomeFailed,
    #[serde(rename = "support.outcome.no_match")]
    SupportOutcomeNoMatch,
    #[serde(rename = "support.outcome.outcome_unknown")]
    SupportOutcomeOutcomeUnknown,
    #[serde(rename = "support.outcome.rejected")]
    SupportOutcomeRejected,
    #[serde(rename = "support.public_index.admitted")]
    SupportPublicIndexAdmitted,
    #[serde(rename = "support.public_index.completed")]
    SupportPublicIndexCompleted,
    #[serde(rename = "support.public_index.gate_failed")]
    SupportPublicIndexGateFailed,
    #[serde(rename = "support.public_index.no_match")]
    SupportPublicIndexNoMatch,
    #[serde(rename = "support.public_index.throttled")]
    SupportPublicIndexThrottled,
    #[serde(rename = "support.retrieval.bm25_fallback")]
    SupportRetrievalBm25Fallback,
    #[serde(rename = "support.retrieval.bm25_fallback_no_match")]
    SupportRetrievalBm25FallbackNoMatch,
    #[serde(rename = "support.retrieval.bm25_fallback_outcome_unknown")]
    SupportRetrievalBm25FallbackOutcomeUnknown,
    #[serde(rename = "support.retrieval.direct")]
    SupportRetrievalDirect,
    #[serde(rename = "support.rewrite.hit")]
    SupportRewriteHit,
    #[serde(rename = "support.rewrite.no_match")]
    SupportRewriteNoMatch,
    #[serde(rename = "support.rewrite.succeeded")]
    SupportRewriteSucceeded,
    #[serde(rename = "support.rewrite.triggered")]
    SupportRewriteTriggered,
    #[serde(rename = "support.turn_latency_ms")]
    SupportTurnLatencyMs,
    #[serde(rename = "turn_completed")]
    TurnCompleted,
    #[serde(rename = "turn_started")]
    TurnStarted,
    #[serde(rename = "upstream_idle_timeout")]
    UpstreamIdleTimeout,
    #[serde(rename = "user_admission_wait_ms")]
    UserAdmissionWaitMs,
    #[serde(rename = "user_queue_depth")]
    UserQueueDepth,
    #[serde(rename = "user_queue_full")]
    UserQueueFull,
}

impl RuntimeDefinition {
    pub fn category(self) -> RuntimeCategory {
        match self {
            Self::ActionLogPersistFailure => RuntimeCategory::Audit,
            Self::AdmissionBackendError => RuntimeCategory::Admission,
            Self::AdmissionCancel => RuntimeCategory::Admission,
            Self::AdmissionLeaseLoss => RuntimeCategory::Admission,
            Self::AiOperationGenerationMismatch => RuntimeCategory::Fence,
            Self::AiOperationGenerationRotated => RuntimeCategory::Fence,
            Self::AiOperationParentMissing => RuntimeCategory::Fence,
            Self::AiOperationRenewalExhausted => RuntimeCategory::Fence,
            Self::AiProviderFenceCompletionFailure => RuntimeCategory::Fence,
            Self::AiProviderFenceConflict => RuntimeCategory::Fence,
            Self::AiProviderFenceConflictDisappeared => RuntimeCategory::Fence,
            Self::AiProviderFenceFingerprintMismatch => RuntimeCategory::Fence,
            Self::AiProviderFenceProviderModelMismatch => RuntimeCategory::Fence,
            Self::BackgroundPromoted => RuntimeCategory::Turn,
            Self::BufferHighWaterChunks => RuntimeCategory::Budget,
            Self::CompressionFailed => RuntimeCategory::Compression,
            Self::CompressionStarted => RuntimeCategory::Compression,
            Self::CompressionSucceeded => RuntimeCategory::Compression,
            Self::ContentSafetyFleetGlobalQueueFull => RuntimeCategory::Safety,
            Self::ContentSafetyFleetOrgRateLimited => RuntimeCategory::Safety,
            Self::ContentSafetyGlobalQueueFull => RuntimeCategory::Safety,
            Self::ContentSafetyInteractiveAdmissionTimeout => RuntimeCategory::Safety,
            Self::ContentSafetyPartialReleaseTimeout => RuntimeCategory::Safety,
            Self::ContentSafetyUnexpectedQueueFull => RuntimeCategory::Safety,
            Self::ContentSafetyUserQueueFull => RuntimeCategory::Safety,
            Self::ContentSafetyUserRateLimited => RuntimeCategory::Safety,
            Self::ContentSafetyWaiterCancelTimeout => RuntimeCategory::Safety,
            Self::ContextReject => RuntimeCategory::Budget,
            Self::EstimatorOverflow => RuntimeCategory::Estimator,
            Self::EstimatorSample => RuntimeCategory::Estimator,
            Self::FreeGlobalAdmissionWaitMs => RuntimeCategory::Admission,
            Self::FreeGlobalQueueDepth => RuntimeCategory::Admission,
            Self::FreeGlobalQueueFull => RuntimeCategory::Admission,
            Self::FreeOutputCapApplied => RuntimeCategory::Budget,
            Self::FreeOutputCapDisabled => RuntimeCategory::Budget,
            Self::FreeOutputCapEnabled => RuntimeCategory::Budget,
            Self::FreeOutputCapHit => RuntimeCategory::Budget,
            Self::Inventory => RuntimeCategory::Inventory,
            Self::OutputCreditBackendError => RuntimeCategory::Budget,
            Self::OutputWaitCancel => RuntimeCategory::Budget,
            Self::OutputWaitMs => RuntimeCategory::Budget,
            Self::PermissionDecided => RuntimeCategory::Turn,
            Self::PermissionRequested => RuntimeCategory::Turn,
            Self::PostureLockError => RuntimeCategory::Admission,
            Self::PostureLockWaitMs => RuntimeCategory::Admission,
            Self::Projection => RuntimeCategory::Projection,
            Self::ProviderCancelled => RuntimeCategory::RemoteTool,
            Self::ProviderCompleted => RuntimeCategory::RemoteTool,
            Self::ProviderDispatchSuppressed => RuntimeCategory::RemoteTool,
            Self::ProviderDispatched => RuntimeCategory::RemoteTool,
            Self::ProviderReadCancel => RuntimeCategory::Budget,
            Self::RegistrationRateLimitBackendError => RuntimeCategory::Registration,
            Self::RemoteToolChunkRejected => RuntimeCategory::RemoteTool,
            Self::RemoteToolErrorRejected => RuntimeCategory::RemoteTool,
            Self::RemoteToolInflight => RuntimeCategory::RemoteTool,
            Self::RemoteToolPendingDuplicate => RuntimeCategory::RemoteTool,
            Self::RemoteToolResultConsumedBytes => RuntimeCategory::RemoteTool,
            Self::RemoteToolResultStoredBytes => RuntimeCategory::RemoteTool,
            Self::SafetyDecision => RuntimeCategory::Safety,
            Self::SafetyDisabled => RuntimeCategory::Safety,
            Self::SafetyFailure => RuntimeCategory::Safety,
            Self::SafetyFleetGlobalAdmissionWaitMs => RuntimeCategory::Admission,
            Self::SafetyFleetGlobalQueueDepth => RuntimeCategory::Admission,
            Self::SafetyFleetGlobalQueueFull => RuntimeCategory::Admission,
            Self::SafetyGlobalAdmissionWaitMs => RuntimeCategory::Admission,
            Self::SafetyGlobalQueueDepth => RuntimeCategory::Admission,
            Self::SafetyGlobalQueueFull => RuntimeCategory::Admission,
            Self::SafetyLatency => RuntimeCategory::Safety,
            Self::SafetyMainCallAvoided => RuntimeCategory::Safety,
            Self::SafetyRetracted => RuntimeCategory::Safety,
            Self::SafetyUserAdmissionWaitMs => RuntimeCategory::Admission,
            Self::SafetyUserQueueDepth => RuntimeCategory::Admission,
            Self::SafetyUserQueueFull => RuntimeCategory::Admission,
            Self::SupportAdmitted => RuntimeCategory::Support,
            Self::SupportAnswerNotCovered => RuntimeCategory::Support,
            Self::SupportOutcomeCancelled => RuntimeCategory::Support,
            Self::SupportOutcomeCompleted => RuntimeCategory::Support,
            Self::SupportOutcomeFailed => RuntimeCategory::Support,
            Self::SupportOutcomeNoMatch => RuntimeCategory::Support,
            Self::SupportOutcomeOutcomeUnknown => RuntimeCategory::Support,
            Self::SupportOutcomeRejected => RuntimeCategory::Support,
            Self::SupportPublicIndexAdmitted => RuntimeCategory::Support,
            Self::SupportPublicIndexCompleted => RuntimeCategory::Support,
            Self::SupportPublicIndexGateFailed => RuntimeCategory::Support,
            Self::SupportPublicIndexNoMatch => RuntimeCategory::Support,
            Self::SupportPublicIndexThrottled => RuntimeCategory::Support,
            Self::SupportRetrievalBm25Fallback => RuntimeCategory::Support,
            Self::SupportRetrievalBm25FallbackNoMatch => RuntimeCategory::Support,
            Self::SupportRetrievalBm25FallbackOutcomeUnknown => RuntimeCategory::Support,
            Self::SupportRetrievalDirect => RuntimeCategory::Support,
            Self::SupportRewriteHit => RuntimeCategory::Support,
            Self::SupportRewriteNoMatch => RuntimeCategory::Support,
            Self::SupportRewriteSucceeded => RuntimeCategory::Support,
            Self::SupportRewriteTriggered => RuntimeCategory::Support,
            Self::SupportTurnLatencyMs => RuntimeCategory::Support,
            Self::TurnCompleted => RuntimeCategory::Turn,
            Self::TurnStarted => RuntimeCategory::Turn,
            Self::UpstreamIdleTimeout => RuntimeCategory::Budget,
            Self::UserAdmissionWaitMs => RuntimeCategory::Admission,
            Self::UserQueueDepth => RuntimeCategory::Admission,
            Self::UserQueueFull => RuntimeCategory::Admission,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeQuantity {
    InventoryTotal,
    InventoryCompiled,
    InventoryEnabled,
    InventoryConnected,
    InventoryReady,
    Generation,
    CoveredMessageCount,
    InputContextCost,
    SummaryContextCost,
    FinalContextCost,
    EstimatedTokens,
    ProviderTokens,
    PairedEstimatedTokens,
    MissingProviderUsage,
    OutputCapHit,
    StaticInstructionBytes,
    RuntimeContextBytes,
    MessageCount,
    MessageJsonBytes,
    AdvertisedToolCount,
    AdvertisedToolJsonBytes,
    CapabilityRegistryCount,
    RuntimeReadyCount,
    PermissionCandidateCount,
    CapabilityCatalogUtf8Bytes,
    CapabilityIndexUtf8Bytes,
    LoadedCapabilityDetailUtf8Bytes,
    ConversationMessageCount,
    SessionSnapshotJsonBytes,
    ContextAttachmentCount,
    PermissionRequestCount,
    PendingWorkTriggerCount,
    PermissionItemCount,
    SafetySexual,
    SafetySexualMinors,
    SafetyViolence,
    SafetyGraphicViolence,
    SafetyViolentWrongdoing,
    SafetyHate,
    SafetyThreateningHarassment,
    SafetySelfHarm,
    SafetySelfHarmInstructions,
    SafetyIllicit,
    SafetyPolitics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimatorClass {
    AsciiDominant,
    CjkDominant,
    Mixed,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimatorVersion {
    EstimatedTokensV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionFailure {
    InputTooLarge,
    ProviderRejected,
    ProviderTimeout,
    Truncated,
    InvalidSchema,
    UnsafeOutput,
    SummaryTooLarge,
    ProtectedStateTooLarge,
    ProtectedReplayUnsafe,
    StaleContext,
    UnsupportedEndpoint,
    InvalidEffectiveBudget,
    AttemptExhausted,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLabels {
    pub origin: Option<Origin>,
    pub turn_outcome: Option<AiAssistantTurnOutcome>,
    pub provider_family: Option<AiAssistantProviderFamily>,
    pub provider_completion: Option<AiAssistantProviderCompletion>,
    pub provider_cancel: Option<AiAssistantProviderCancel>,
    pub permission_decision: Option<AiAssistantPermissionDecision>,
    pub safety_surface: Option<ContentSafetySurface>,
    pub safety_stage: Option<ContentSafetyStage>,
    pub safety_decision: Option<ContentSafetyDecision>,
    pub safety_provider: Option<ContentSafetyProviderFamily>,
    pub safety_failure: Option<ContentSafetyFailureKind>,
    pub retraction: Option<StreamRetractionReason>,
    pub compression_failure: Option<CompressionFailure>,
    pub replay: Option<bool>,
    pub estimator_class: Option<EstimatorClass>,
    pub estimator_version: Option<EstimatorVersion>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_definitions_and_identity_labels_are_rejected() {
        assert!(serde_json::from_str::<RuntimeDefinition>("\"request.user.123\"").is_err());
        assert!(serde_json::from_str::<RuntimeLabels>("{\"user_id\":123}").is_err());
        assert_eq!(
            serde_json::from_str::<RuntimeDefinition>("\"safety_decision\"")
                .unwrap()
                .category(),
            RuntimeCategory::Safety
        );
        assert!(serde_json::from_str::<RuntimeLabels>("{\"origin\":\"free-text\"}").is_err());
    }
}
