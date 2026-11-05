//! Compiled personal-assistant retention contracts. These registrations bind
//! native labels, not claims in model arguments, prose, or sensitivity alone.

use super::source::{policy_hash, validate_policy};
use desk_agent_protocol::{
    ai_assistant::source::{SourcePolicyClass, SourceUsePolicy},
    capability_provider::CapabilityEffect,
    data_lineage::{ContentRef, DataEnvelope, Sensitivity},
};
use serde::{Deserialize, Serialize};

pub const SOURCE_CONTRACT_REVISION: u64 = 1;

mod snapshot;
pub use snapshot::{NativeInputLeafPolicy, NativeInputPolicyRegistration};
mod derived;
pub use derived::{
    NativeDerivedInputPolicy, NativeInputPolicyGraph, NativeInputPolicyGraphBuilder,
    is_native_output_policy_source,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredSourceContract {
    OwnerText,
    ConversationSnapshot,
    /// Requires the original occurrence's native prompt retention proof.
    PublishedTaskInput,
    FrozenPersonalization,
    PublicSystem,
    PublicResearchResult,
    ExternalIssueResult,
    DeviceObservation,
    UiObservation,
    MediaObservation,
    ProviderPrivate,
    Secret,
    Unknown,
    /// A native control is registered only with all of its parents present in
    /// the same original snapshot. It never replaces their retention checks.
    NativeControl,
    /// Worker-authored path guidance has a live-use deadline, not a deadline
    /// for the visible answer derived from its immutable text snapshot.
    InteractiveUserHome,
}

/// A registered leaf policy grants no source lookup, copy, or send by itself.
/// The host must verify original native production, owner and all live controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredSourcePolicy {
    pub source_contract_revision: u64,
    pub contract: RegisteredSourceContract,
    pub envelope_id: String,
    pub content_sha256: String,
    pub label_sha256: String,
    pub policy: SourceUsePolicy,
}

impl RegisteredSourcePolicy {
    #[cfg(test)]
    pub fn validate_leaf(
        &self,
        envelope: &DataEnvelope,
        original_contract_revision: u64,
    ) -> Result<(), &'static str> {
        if original_contract_revision != self.source_contract_revision
            || registered_leaf(
                envelope,
                self.policy.policy_revision,
                original_contract_revision,
            )?
            .as_ref()
                != Some(self)
        {
            return Err("registered leaf policy changed its original contract or label");
        }
        Ok(())
    }
}

/// Resolve only compiled roots. Derived labels and live object observations
/// require their original native parent/reference proof, not a guessed policy.
pub fn registered_leaf(
    envelope: &DataEnvelope,
    policy_revision: u64,
    original_contract_revision: u64,
) -> Result<Option<RegisteredSourcePolicy>, &'static str> {
    envelope
        .validate()
        .map_err(|_| "invalid registered source label")?;
    if policy_revision == 0 || original_contract_revision != SOURCE_CONTRACT_REVISION {
        return Err("original source contract revision is unavailable");
    }
    if !envelope.provenance.source_envelope_ids.is_empty()
        || matches!(envelope.content, ContentRef::EphemeralObservation { .. })
        || envelope
            .provenance
            .source_object_id
            .as_deref()
            .is_some_and(|id| id.starts_with("live-target:sha256:"))
    {
        return Ok(None);
    }
    registration(
        envelope,
        classify(envelope),
        policy_revision,
        original_contract_revision,
    )
    .map(Some)
}

fn registration(
    envelope: &DataEnvelope,
    contract: RegisteredSourceContract,
    policy_revision: u64,
    original_contract_revision: u64,
) -> Result<RegisteredSourcePolicy, &'static str> {
    let (class, whole_history, independent_fact, visible_text) = match contract {
        RegisteredSourceContract::OwnerText => (SourcePolicyClass::UserText, true, true, true),
        RegisteredSourceContract::ConversationSnapshot => {
            (SourcePolicyClass::VisibleProviderText, true, false, true)
        }
        RegisteredSourceContract::PublishedTaskInput => {
            (SourcePolicyClass::UserText, false, false, true)
        }
        RegisteredSourceContract::FrozenPersonalization => {
            (SourcePolicyClass::UserText, false, false, true)
        }
        RegisteredSourceContract::PublicSystem => {
            (SourcePolicyClass::StructuredObservation, false, false, true)
        }
        RegisteredSourceContract::InteractiveUserHome => {
            (SourcePolicyClass::StructuredObservation, false, false, true)
        }
        RegisteredSourceContract::NativeControl => (
            SourcePolicyClass::StructuredObservation,
            false,
            false,
            envelope.sensitivity != Sensitivity::Secret,
        ),
        RegisteredSourceContract::PublicResearchResult
        | RegisteredSourceContract::ExternalIssueResult => {
            (SourcePolicyClass::VisibleProviderText, false, false, true)
        }
        RegisteredSourceContract::DeviceObservation | RegisteredSourceContract::UiObservation => {
            (SourcePolicyClass::StructuredObservation, false, false, true)
        }
        RegisteredSourceContract::MediaObservation => {
            (SourcePolicyClass::Media, false, false, true)
        }
        RegisteredSourceContract::ProviderPrivate | RegisteredSourceContract::Secret => {
            (SourcePolicyClass::ProviderPrivate, false, false, false)
        }
        RegisteredSourceContract::Unknown => (SourcePolicyClass::Unknown, false, false, false),
    };
    let (content_deadline, reference_freshness) = match contract {
        RegisteredSourceContract::InteractiveUserHome => {
            (None, envelope.retention.expires_at_unix_ms)
        }
        _ => (envelope.retention.expires_at_unix_ms, None),
    };
    let mut policy = SourceUsePolicy {
        policy_revision,
        policy_sha256: String::new(),
        class,
        history_retention_allowed: whole_history,
        independent_fact_allowed: independent_fact,
        derived_visible_text_allowed: visible_text,
        no_copy: !visible_text,
        no_export: !visible_text,
        // A finite native content deadline remains hard unless a separate
        // original reference-producer proof establishes freshness semantics.
        content_hard_expires_at_unix_ms: content_deadline,
        reference_fresh_until_unix_ms: reference_freshness,
        fixed_destinations: Vec::new(),
    };
    policy.policy_sha256 = policy_hash(&policy)?;
    validate_policy(&policy)?;
    Ok(RegisteredSourcePolicy {
        source_contract_revision: original_contract_revision,
        contract,
        envelope_id: envelope.envelope_id.clone(),
        content_sha256: envelope.digest_sha256.clone(),
        label_sha256: crate::model_egress::sink_metadata_sha256(envelope)?,
        policy,
    })
}

fn classify(envelope: &DataEnvelope) -> RegisteredSourceContract {
    if envelope.sensitivity == Sensitivity::Secret {
        return RegisteredSourceContract::Secret;
    }
    if crate::model_egress::is_public_system_prompt(envelope) {
        return RegisteredSourceContract::PublicSystem;
    }
    if canonical_interactive_home(envelope) {
        return RegisteredSourceContract::InteractiveUserHome;
    }
    if envelope.provenance.source_provider_id == "assistant-conversation-snapshot"
        && envelope
            .provenance
            .source_object_id
            .as_deref()
            .is_some_and(|id| {
                crate::native_ids::is(crate::native_ids::Kind::AssistantReadProjection, id)
            })
    {
        return RegisteredSourceContract::ConversationSnapshot;
    }
    if crate::schedule::published_input::is_published_input_label(envelope) {
        return RegisteredSourceContract::PublishedTaskInput;
    }
    if canonical_user_blob(envelope) {
        match (
            envelope.provenance.source_provider_id.as_str(),
            envelope.provenance.source_tool_name.as_str(),
        ) {
            ("ai-assistant-user", "send-message") => return RegisteredSourceContract::OwnerText,
            ("ai-assistant-personalization", "frozen-user-instructions") => {
                return RegisteredSourceContract::FrozenPersonalization;
            }
            _ => {}
        }
    }
    if crate::external_tools::is_issue_read_label(envelope)
        || crate::issue_authority::is_issue_workset_label(envelope)
    {
        return RegisteredSourceContract::ExternalIssueResult;
    }
    let registry = super::ai_assistant_provider_registry_ref();
    let Some(capability) = registry.capability_for_tool(&envelope.provenance.source_tool_name)
    else {
        return RegisteredSourceContract::Unknown;
    };
    let Some(provider) = registry.provider_for_capability(&capability.wire.capability_id) else {
        return RegisteredSourceContract::Unknown;
    };
    if provider.wire.provider_id != envelope.provenance.source_provider_id {
        return RegisteredSourceContract::Unknown;
    }
    // Only whole-result derived-text contracts are selected here. A field's
    // independent fact retention requires its own typed native projection.
    let (contract, expected_effect) = match capability.wire.capability_id.as_str() {
        super::WEB_RESEARCH_FETCH_CAPABILITY_ID => (
            RegisteredSourceContract::PublicResearchResult,
            CapabilityEffect::ReadExternal,
        ),
        super::WEB_RESEARCH_SEARCH_CAPABILITY_ID => (
            RegisteredSourceContract::PublicResearchResult,
            CapabilityEffect::ExportData,
        ),
        super::APPLICATION_LIST_CAPABILITY_ID
        | super::DESKTOP_SESSION_CAPABILITY_ID
        | super::SYSTEM_INFO_CAPABILITY_ID
        | super::SYSTEM_PROCESS_CAPABILITY_ID
        | super::SYSTEM_NETWORK_CAPABILITY_ID
        | super::SYSTEM_SERVICE_CAPABILITY_ID
        | super::SYSTEM_CONTAINER_CAPABILITY_ID => (
            RegisteredSourceContract::DeviceObservation,
            CapabilityEffect::ReadDevice,
        ),
        super::DESKTOP_UI_CAPABILITY_ID => (
            RegisteredSourceContract::UiObservation,
            CapabilityEffect::ReadDevice,
        ),
        super::BROWSER_SNAPSHOT_CAPABILITY_ID => (
            RegisteredSourceContract::UiObservation,
            CapabilityEffect::ReadExternal,
        ),
        super::CURRENT_SCREEN_CAPABILITY_ID => (
            RegisteredSourceContract::MediaObservation,
            CapabilityEffect::CaptureScreen,
        ),
        _ => return RegisteredSourceContract::ProviderPrivate,
    };
    if capability.wire.effect == expected_effect {
        contract
    } else {
        RegisteredSourceContract::ProviderPrivate
    }
}

fn canonical_user_blob(envelope: &DataEnvelope) -> bool {
    let Some(id) = envelope.provenance.source_object_id.as_deref() else {
        return false;
    };
    envelope.sensitivity == Sensitivity::UserContent
        && envelope.envelope_id == format!("user-message-{id}")
        && matches!(&envelope.content, ContentRef::ImmutableBlob { blob_id, sha256, size_bytes, media_type }
            if blob_id == &format!("session-message-{id}") && sha256 == &envelope.digest_sha256 && *size_bytes > 0
                && media_type == "text/plain;charset=utf-8")
}

fn canonical_interactive_home(envelope: &DataEnvelope) -> bool {
    envelope.sensitivity == Sensitivity::Sensitive
        && envelope.provenance.source_provider_id == "session-worker-runtime"
        && envelope.provenance.source_tool_name == "interactive-user-home"
        && envelope
            .provenance
            .source_object_id
            .as_ref()
            .is_some_and(|id| !id.is_empty())
        && envelope.retention.delete_with_run
        && envelope.retention.expires_at_unix_ms.is_some()
        && matches!(&envelope.content, ContentRef::ImmutableBlob { blob_id, sha256, size_bytes, media_type }
            if blob_id == &format!("interactive-home-snapshot-{}", envelope.envelope_id)
                && sha256 == &envelope.digest_sha256 && *size_bytes > 0
                && media_type == "text/plain;charset=utf-8")
}

#[cfg(test)]
mod tests;
