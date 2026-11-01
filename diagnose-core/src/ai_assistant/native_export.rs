//! Physical-call bindings. Read preparation and owner-view responses cannot grant a send.

use super::{read_projection::ReadProjectionProof, seam::AssistantModelDataManifest};
use crate::{
    native_ids::{self, Kind},
    session::PersistedAgentSession,
};
use desk_agent_protocol::{
    ai_assistant::source::{SourceReadPurpose, validate_source_id},
    data_lineage::DestinationIdentity,
};
use serde::{Deserialize, Serialize};

pub const MAX_NATIVE_EXPORT_TTL_MS: u64 = 5 * 60 * 1000;

mod context;
pub use context::AssistantNativeContextFrame;

/// The native receipt seals its original context and complete actual-use set.
/// These references seal direct reads. Empty direct references do not prove
/// that an old answer input has no dependencies: hosts also revalidate its
/// accepted native ancestry from the exact frozen input lineage at both gates.
/// Deleting a direct ledger row cannot remove it from this sealed identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantNativeSourceSet {
    pub actor_id: String,
    pub consumer_conversation_id: String,
    pub source_device_ref: String,
    pub physical_call_id: String,
    pub model_export_authorization_id: String,
    pub destination: DestinationIdentity,
    pub input_lineage_sha256: String,
    pub frame_sha256: String,
    pub context: AssistantNativeContextFrame,
    pub authorizations: Vec<AssistantNativeAuthorizationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantNativeAuthorizationRef {
    pub authorization_id: String,
    pub proof_sha256: String,
}

impl AssistantNativeSourceSet {
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_grants(grants: &[AssistantDataExportAuthorization]) -> Result<Self, &'static str> {
        let first = grants
            .first()
            .ok_or("native assistant source set is empty")?;
        let mut value = Self {
            actor_id: first.actor_id.clone(),
            consumer_conversation_id: first.consumer_conversation_id.clone(),
            source_device_ref: first.source_device_ref.clone(),
            physical_call_id: first.native_call.physical_call_id.clone(),
            model_export_authorization_id: first.native_call.model_export_authorization_id.clone(),
            destination: first.native_call.destination.clone(),
            input_lineage_sha256: first.native_call.input_lineage_sha256.clone(),
            frame_sha256: frame_sha256(first)?,
            context: AssistantNativeContextFrame::from_grant(first),
            authorizations: grants
                .iter()
                .map(|grant| {
                    Ok(AssistantNativeAuthorizationRef {
                        authorization_id: grant.authorization_id.clone(),
                        proof_sha256: grant.fingerprint()?,
                    })
                })
                .collect::<Result<Vec<_>, &'static str>>()?,
        };
        value
            .authorizations
            .sort_by(|a, b| a.authorization_id.cmp(&b.authorization_id));
        value.validate_grants(grants)?;
        Ok(value)
    }

    /// A zero-read call still freezes its original native input identity. The
    /// caller has already performed all actual native/session/source admission.
    pub fn for_request(
        session: &PersistedAgentSession,
        manifest: &AssistantModelDataManifest,
        call: &NativeAssistantModelCall,
        grants: &[AssistantDataExportAuthorization],
        started: u64,
    ) -> Result<Self, &'static str> {
        call.validate()?;
        if !super::data_access::retained_context_eligible(session)
            || (!super::data_access::eligible_origin(session)
                && (!grants.is_empty() || !manifest.read_projection_ids.is_empty()))
            || call
                .authority_expires_at_unix_ms
                .is_none_or(|until| until <= started)
        {
            return Err("native context requires original main-assistant authority");
        }
        let context = AssistantNativeContextFrame::from_request(session, manifest, started)?;
        let mut value = Self {
            actor_id: manifest.actor_id.clone(),
            consumer_conversation_id: manifest.conversation_id.clone(),
            source_device_ref: manifest.source_device_ref.clone(),
            physical_call_id: call.physical_call_id.clone(),
            model_export_authorization_id: call.model_export_authorization_id.clone(),
            destination: call.destination.clone(),
            input_lineage_sha256: call.input_lineage_sha256.clone(),
            frame_sha256: String::new(),
            context,
            authorizations: grants
                .iter()
                .map(|grant| {
                    Ok(AssistantNativeAuthorizationRef {
                        authorization_id: grant.authorization_id.clone(),
                        proof_sha256: grant.fingerprint()?,
                    })
                })
                .collect::<Result<Vec<_>, &'static str>>()?,
        };
        value.frame_sha256 = context_sha256(
            &value.actor_id,
            &value.consumer_conversation_id,
            &value.source_device_ref,
            &value.context,
        )?;
        value
            .authorizations
            .sort_by(|a, b| a.authorization_id.cmp(&b.authorization_id));
        let actual = grants
            .iter()
            .map(|grant| grant.projection_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let selected = manifest
            .read_projection_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        if actual != selected || selected.len() != manifest.read_projection_ids.len() {
            return Err("native context omitted selected reads");
        }
        value.validate_grants(grants)?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        for id in [
            &self.actor_id,
            &self.consumer_conversation_id,
            &self.source_device_ref,
            &self.physical_call_id,
            &self.model_export_authorization_id,
        ] {
            validate_source_id(id)?;
        }
        self.destination
            .validate()
            .map_err(|_| "invalid native source-set destination")?;
        if !matches!(self.destination, DestinationIdentity::Model { .. })
            || !valid_digest(&self.input_lineage_sha256)
            || !valid_digest(&self.frame_sha256)
        {
            return Err("invalid native assistant source set");
        }
        self.context.validate()?;
        if self.context.actor_input_seq == 0
            && (self.context.task_id.as_deref() != Some(self.consumer_conversation_id.as_str())
                || !self.authorizations.is_empty())
        {
            return Err("a task definition has no accepted chat input or retrieved-source grants");
        }
        if self.frame_sha256
            != context_sha256(
                &self.actor_id,
                &self.consumer_conversation_id,
                &self.source_device_ref,
                &self.context,
            )?
        {
            return Err("native context frame changed");
        }
        let mut previous: Option<&str> = None;
        for reference in &self.authorizations {
            if !native_ids::is(
                Kind::AssistantReadAuthorization,
                &reference.authorization_id,
            ) || !valid_digest(&reference.proof_sha256)
                || previous.is_some_and(|id| id >= reference.authorization_id.as_str())
            {
                return Err("invalid native assistant authorization reference");
            }
            previous = Some(&reference.authorization_id);
        }
        Ok(())
    }

    pub fn decode(raw: &str) -> Result<Self, &'static str> {
        let value: Self =
            serde_json::from_str(raw).map_err(|_| "invalid stored native assistant source set")?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate_grants(
        &self,
        grants: &[AssistantDataExportAuthorization],
    ) -> Result<(), &'static str> {
        self.validate()?;
        if grants.len() != self.authorizations.len() {
            return Err("native assistant source set is incomplete");
        }
        let mut seen = std::collections::BTreeSet::new();
        for grant in grants {
            if !seen.insert(&grant.authorization_id) {
                return Err("duplicate native assistant grant");
            }
            self.validate_member(grant)?;
        }
        Ok(())
    }

    /// Historical membership only; callers still read the actual accepted
    /// native receipt, consumed projection and current source controls.
    pub fn validate_member(
        &self,
        grant: &AssistantDataExportAuthorization,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let reference = self
            .authorizations
            .iter()
            .find(|reference| reference.authorization_id == grant.authorization_id)
            .ok_or("native assistant use is not sealed by its receipt")?;
        if reference.proof_sha256 != grant.fingerprint()?
            || grant.actor_id != self.actor_id
            || grant.consumer_conversation_id != self.consumer_conversation_id
            || grant.source_device_ref != self.source_device_ref
            || grant.native_call.physical_call_id != self.physical_call_id
            || grant.native_call.model_export_authorization_id != self.model_export_authorization_id
            || grant.native_call.destination != self.destination
            || grant.native_call.input_lineage_sha256 != self.input_lineage_sha256
            || frame_sha256(grant)? != self.frame_sha256
        {
            return Err("native assistant source-set binding changed");
        }
        Ok(())
    }

    pub fn validate_input_lineage(
        &self,
        inputs: &[crate::model_egress::ModelInputLineage],
    ) -> Result<(), &'static str> {
        self.validate()?;
        if self.input_lineage_sha256 != input_lineage_sha256(inputs)? {
            return Err("native source set changed its audited input set");
        }
        if self.authorizations.is_empty()
            && inputs.iter().any(|input| {
                input.source_provider_id == "assistant-data-source"
                    || (input.source_provider_id == "assistant-data-result"
                        && input.source_envelope_ids.len() > 1)
            })
        {
            return Err("native source content cannot have an empty authorization set");
        }
        Ok(())
    }
}

fn frame_sha256(grant: &AssistantDataExportAuthorization) -> Result<String, &'static str> {
    context_sha256(
        &grant.actor_id,
        &grant.consumer_conversation_id,
        &grant.source_device_ref,
        &AssistantNativeContextFrame::from_grant(grant),
    )
}

fn context_sha256(
    actor: &str,
    conversation: &str,
    device: &str,
    frame: &AssistantNativeContextFrame,
) -> Result<String, &'static str> {
    frame.validate()?;
    crate::model_egress::sink_metadata_sha256(&(
        "assistant-native-frame-v2",
        actor,
        conversation,
        device,
        frame,
    ))
}

/// Supplied by the native model-receipt transaction after its original dispatch
/// admission. A call's exact sink audit and receipt identity stay immutable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAssistantModelCall {
    pub physical_call_id: String,
    pub model_export_authorization_id: String,
    pub destination: DestinationIdentity,
    pub input_lineage_sha256: String,
    pub authority_expires_at_unix_ms: Option<u64>,
}

impl NativeAssistantModelCall {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_source_id(&self.physical_call_id)?;
        validate_source_id(&self.model_export_authorization_id)?;
        self.destination
            .validate()
            .map_err(|_| "invalid native assistant recipient")?;
        if !matches!(self.destination, DestinationIdentity::Model { .. })
            || !valid_digest(&self.input_lineage_sha256)
            || self
                .authority_expires_at_unix_ms
                .is_some_and(|until| until == 0 || until > i64::MAX as u64)
        {
            return Err("invalid native assistant model call");
        }
        Ok(())
    }
}

/// One purpose-bound grant references one immutable read proof. Source and
/// memory use rows index that proof instead of copying its complete dependency
/// arrays for every physical use. This is host-only and never a model argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantDataExportAuthorization {
    pub authorization_id: String,
    pub actor_id: String,
    pub consumer_conversation_id: String,
    pub source_device_ref: String,
    pub session_version: i64,
    pub lease_token: u64,
    pub input_revision: u64,
    pub actor_input_seq: u64,
    pub input_recorded_at_unix_ms: u64,
    pub statement_timezone: String,
    pub retrieval_episode_id: String,
    pub task_id: Option<String>,
    pub scope_sha256: String,

    pub policy_revision: u64,
    pub source_contract_revision: u64,
    pub memory_state_generation: u64,
    pub control_generation: u64,
    pub projection_id: String,
    pub projection_sha256: String,
    pub source_envelope_id: String,
    pub purpose: SourceReadPurpose,
    pub native_call: NativeAssistantModelCall,
    pub authorized_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

impl AssistantDataExportAuthorization {
    pub fn fingerprint(&self) -> Result<String, &'static str> {
        self.validate()?;
        crate::model_egress::sink_metadata_sha256(self)
    }

    /// All live native/session/task/source checks must already have succeeded
    /// in the caller's original receipt transaction. This validates their exact
    /// common bindings; it cannot perform or replace those database checks.
    pub fn for_projection(
        session: &PersistedAgentSession,
        manifest: &AssistantModelDataManifest,
        proof: &ReadProjectionProof,
        native_call: NativeAssistantModelCall,
        first_consumed_at: Option<u64>,
        now: u64,
    ) -> Result<Self, &'static str> {
        if !super::data_access::eligible_origin(session) {
            return Err("assistant export requires a personal main origin");
        }
        let data = session
            .assistant_data
            .as_ref()
            .ok_or("missing assistant export state")?;
        if manifest.actor_id != session.actor_id
            || manifest.conversation_id != session.conversation_id
            || manifest.source_device_ref != data.source_device_ref
            || manifest.session_version != session.version
            || manifest.lease_token != session.lease_token
            || manifest.input_revision != session.input_revision
            || manifest.actor_input_seq != data.current_actor_input_seq
            || manifest.scope_sha256 != super::data_access::scope_sha256(session)?
            || manifest.retrieval_episode_id != data.retrieval_budget.episode_id
            || manifest.task_id != data.retrieval_task_id
            || manifest.memory_state_generation != data.memory_state_generation
            || manifest.control_generation != data.control_generation
            || !manifest
                .read_projection_ids
                .contains(&proof.usage.projection_id)
            || (manifest.task_id.is_some() && native_call.authority_expires_at_unix_ms.is_none())
            || !proof.preparation_available(now, first_consumed_at)
        {
            return Err("assistant export source session changed");
        }
        let mut expiry = now
            .checked_add(MAX_NATIVE_EXPORT_TTL_MS)
            .ok_or("assistant export deadline overflow")?;
        if let Some(until) = proof.usage.expires_at_unix_ms {
            expiry = expiry.min(until);
        }
        if let Some(until) = native_call.authority_expires_at_unix_ms {
            expiry = expiry.min(until);
        }
        let scope_expiry = match proof.usage.purpose {
            SourceReadPurpose::ModelMemory | SourceReadPurpose::MemoryEvidence => {
                super::memory_access::usable_memory_scope(session, now)
                    .ok_or("assistant export has no original memory scope")?
                    .expires_at_unix_ms
            }
            SourceReadPurpose::ModelHistory => {
                super::data_access::usable_history_scope(session, now)
                    .ok_or("assistant export has no original history scope")?
                    .expires_at_unix_ms
            }
            SourceReadPurpose::OwnerHistory => {
                return Err("owner history viewing cannot grant a model export");
            }
        };
        if let Some(until) = scope_expiry {
            expiry = expiry.min(until);
        }
        let mut grant = Self {
            authorization_id: String::new(),
            actor_id: manifest.actor_id.clone(),
            consumer_conversation_id: manifest.conversation_id.clone(),
            source_device_ref: manifest.source_device_ref.clone(),
            session_version: manifest.session_version,
            lease_token: manifest.lease_token,
            input_revision: manifest.input_revision,
            actor_input_seq: manifest.actor_input_seq,
            input_recorded_at_unix_ms: data.current_input_recorded_at_unix_ms,
            statement_timezone: data.current_input_timezone.clone(),
            retrieval_episode_id: manifest.retrieval_episode_id.clone(),
            task_id: manifest.task_id.clone(),
            scope_sha256: manifest.scope_sha256.clone(),

            policy_revision: manifest.policy_revision,
            memory_state_generation: manifest.memory_state_generation,
            source_contract_revision: super::source_registry::SOURCE_CONTRACT_REVISION,
            control_generation: manifest.control_generation,
            projection_id: proof.usage.projection_id.clone(),
            projection_sha256: proof.fingerprint()?,
            source_envelope_id: proof.source_envelope.envelope_id.clone(),
            purpose: proof.usage.purpose,
            native_call,
            authorized_at_unix_ms: now,
            expires_at_unix_ms: expiry,
        };
        grant.authorization_id = grant.expected_id();
        grant.validate_projection(proof)?;
        Ok(grant)
    }

    fn expected_id(&self) -> String {
        native_ids::stable(
            Kind::AssistantReadAuthorization,
            &(
                "assistant-native-export-v1",
                &self.actor_id,
                &self.consumer_conversation_id,
                &self.native_call.physical_call_id,
                &self.projection_id,
                self.purpose,
            ),
        )
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.native_call.validate()?;
        AssistantNativeContextFrame::from_grant(self).validate()?;
        for id in [
            &self.actor_id,
            &self.consumer_conversation_id,
            &self.source_device_ref,
            &self.retrieval_episode_id,
            &self.source_envelope_id,
        ] {
            validate_source_id(id)?;
        }
        if let Some(id) = &self.task_id {
            validate_source_id(id)?;
        }
        if self.authorization_id != self.expected_id()
            || !native_ids::is(Kind::AssistantReadProjection, &self.projection_id)
            || !valid_digest(&self.projection_sha256)
            || !valid_digest(&self.scope_sha256)
            || self.session_version < 0
            || self.lease_token == 0
            || self.lease_token > i64::MAX as u64
            || self.input_revision == 0
            || self.input_revision > i64::MAX as u64
            || self.actor_input_seq == 0
            || self.actor_input_seq > i64::MAX as u64
            || self.policy_revision == 0
            || self.memory_state_generation == 0
            || self.control_generation == 0
            || self.purpose == SourceReadPurpose::OwnerHistory
            || self.authorized_at_unix_ms == 0
            || self.expires_at_unix_ms <= self.authorized_at_unix_ms
            || self.expires_at_unix_ms > i64::MAX as u64
            || self.expires_at_unix_ms - self.authorized_at_unix_ms > MAX_NATIVE_EXPORT_TTL_MS
        {
            return Err("invalid assistant physical-call grant");
        }
        if (self.task_id.is_some() && self.native_call.authority_expires_at_unix_ms.is_none())
            || self
                .native_call
                .authority_expires_at_unix_ms
                .is_some_and(|until| self.expires_at_unix_ms > until)
        {
            return Err("assistant physical-call grant outlives its original authority");
        }
        Ok(())
    }

    pub fn validate_projection(&self, proof: &ReadProjectionProof) -> Result<(), &'static str> {
        self.validate()?;
        if self.projection_sha256 != proof.fingerprint()?
            || self.projection_id != proof.usage.projection_id
            || self.actor_id != proof.actor_id
            || self.consumer_conversation_id != proof.root_conversation_id
            || self.source_device_ref != proof.source_device_ref
            || self.input_revision != proof.input_revision
            || self.actor_input_seq != proof.actor_input_seq
            || self.task_id != proof.task_id
            || self.retrieval_episode_id != proof.usage.retrieval_episode_id
            || self.scope_sha256 != proof.usage.scope_sha256
            || self.policy_revision != proof.usage.policy_revision
            || self.control_generation != proof.usage.control_generation
            || self.memory_state_generation != proof.usage.memory_state_generation
            || self.purpose != proof.usage.purpose
            || self.source_envelope_id != proof.source_envelope.envelope_id
            || proof
                .usage
                .expires_at_unix_ms
                .is_some_and(|until| self.expires_at_unix_ms > until)
            || self.authorized_at_unix_ms < proof.created_at_unix_ms
        {
            return Err("assistant export does not match its immutable read proof");
        }
        Ok(())
    }
}

pub fn input_lineage_sha256(
    lineage: &[crate::model_egress::ModelInputLineage],
) -> Result<String, &'static str> {
    crate::model_egress::model_input_lineage_sha256(lineage)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    mod owner_history;
    use crate::{
        ai_assistant::{
            data_access::scope_sha256,
            read_projection::ReadProjectionKind,
            seam::{AssistantDataReadUse, SelectedAssistantEnvelope},
            test_support::personal_session,
        },
        model_message_labels::model_bound_user_message,
    };

    fn fixture() -> (
        PersistedAgentSession,
        AssistantModelDataManifest,
        ReadProjectionProof,
        NativeAssistantModelCall,
    ) {
        let mut session = personal_session("owner", "root", "device");
        session.lease_token = 1;
        session.input_revision = 1;
        let data = session.assistant_data.as_ref().unwrap();
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model-a".into(),
            profile_revision: 1,
        };
        let envelope =
            model_bound_user_message("read-body".into(), "{}".into(), destination.clone())
                .unwrap()
                .data_envelope
                .unwrap();
        let proof = ReadProjectionProof {
            kind: ReadProjectionKind::CurrentMemories,
            actor_id: session.actor_id.clone(),
            root_conversation_id: session.conversation_id.clone(),
            source_device_ref: data.source_device_ref.clone(),
            input_revision: session.input_revision,
            actor_input_seq: data.current_actor_input_seq,
            task_id: None,
            producer_message_id: None,
            producer_output_envelope_id: None,
            tool_call_id: None,
            tool_name: "current_memories".into(),
            logical_call_id: "preparation".into(),
            request_sha256: "a".repeat(64),
            usage: AssistantDataReadUse {
                decision_roots: Vec::new(),
                projection_id: native_ids::stable(Kind::AssistantReadProjection, &"test"),
                source_envelope_id: envelope.envelope_id.clone(),
                purpose: SourceReadPurpose::ModelMemory,
                retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
                scope_sha256: scope_sha256(&session).unwrap(),
                policy_revision: 1,
                control_generation: 1,
                memory_state_generation: 1,
                roots: Vec::new(),
                history_dependencies: Vec::new(),
                memories: Vec::new(),
                expires_at_unix_ms: None,
            },
            source_envelope: envelope.clone(),
            snapshot_policy: None,
            created_at_unix_ms: 1000,
            unused_until_unix_ms: 1000
                + desk_agent_protocol::ai_assistant::limits::DEFAULT_UNUSED_PROJECTION_TTL_MS,
        };
        let manifest = AssistantModelDataManifest {
            actor_id: session.actor_id.clone(),
            conversation_id: session.conversation_id.clone(),
            source_device_ref: data.source_device_ref.clone(),
            session_version: session.version,
            lease_token: session.lease_token,
            input_revision: session.input_revision,
            actor_input_seq: data.current_actor_input_seq,
            retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
            task_id: None,
            scope_sha256: scope_sha256(&session).unwrap(),
            policy_revision: 1,

            memory_state_generation: 1,
            control_generation: 1,
            selected_envelopes: vec![SelectedAssistantEnvelope {
                envelope_id: envelope.envelope_id.clone(),
                digest_sha256: envelope.digest_sha256.clone(),
            }],
            read_projection_ids: vec![proof.usage.projection_id.clone()],
            current_memories: Vec::new(),
        };
        let call = NativeAssistantModelCall {
            physical_call_id: "native-physical-call-1".into(),
            model_export_authorization_id: "native-export-1".into(),
            destination,
            input_lineage_sha256: "b".repeat(64),
            authority_expires_at_unix_ms: None,
        };
        (session, manifest, proof, call)
    }

    #[test]
    fn source_free_dispatch_still_seals_its_original_context_and_audited_inputs() {
        let (session, mut manifest, _, mut call) = fixture();
        manifest.read_projection_ids.clear();
        manifest.current_memories.clear();
        call.authority_expires_at_unix_ms = Some(2000);
        let envelope = model_bound_user_message(
            "plain-input".into(),
            "new independent question".into(),
            call.destination.clone(),
        )
        .unwrap()
        .data_envelope
        .unwrap();
        let audit = crate::sink_authorizer::SinkProjectionAudit {
            destination: call.destination.clone(),
            envelope_ids: vec![envelope.envelope_id.clone()],
            digests_sha256: vec![envelope.digest_sha256.clone()],
            total_bytes: 24,
        };
        let inputs = crate::model_egress::project_model_input_lineage(
            &audit,
            std::slice::from_ref(&envelope),
        )
        .unwrap();
        call.input_lineage_sha256 = input_lineage_sha256(&inputs).unwrap();
        let set =
            AssistantNativeSourceSet::for_request(&session, &manifest, &call, &[], 1001).unwrap();
        assert!(set.authorizations.is_empty());
        set.validate_grants(&[]).unwrap();
        set.validate_input_lineage(&inputs).unwrap();
        set.context.validate_session(&session).unwrap();
        assert_eq!(
            set.context.input_recorded_at_unix_ms,
            session
                .assistant_data
                .as_ref()
                .unwrap()
                .current_input_recorded_at_unix_ms
        );
        assert_eq!(
            AssistantNativeSourceSet::decode(&serde_json::to_string(&set).unwrap()).unwrap(),
            set
        );
        let mut another_input = session.clone();
        another_input.input_revision += 1;
        assert!(set.context.validate_session(&another_input).is_err());
        let mut another_time = set;
        another_time.context.input_recorded_at_unix_ms += 1;
        assert!(another_time.validate().is_err());
    }

    #[test]
    fn empty_read_set_cannot_hide_a_direct_assistant_source_or_omit_selected_grants() {
        let (session, mut manifest, proof, mut call) = fixture();
        call.authority_expires_at_unix_ms = Some(2000);
        assert!(
            AssistantNativeSourceSet::for_request(&session, &manifest, &call, &[], 1001).is_err()
        );
        let grant = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &proof,
            call.clone(),
            None,
            1001,
        )
        .unwrap();
        let set = AssistantNativeSourceSet::for_request(
            &session,
            &manifest,
            &call,
            std::slice::from_ref(&grant),
            1001,
        )
        .unwrap();
        assert_eq!(
            set,
            AssistantNativeSourceSet::from_grants(&[grant]).unwrap()
        );
        manifest.read_projection_ids.clear();
        let mut input = crate::model_egress::ModelInputLineage {
            envelope_id: "direct-memory".into(),
            digest_sha256: "a".repeat(64),
            source_provider_id: "assistant-data-source".into(),
            source_tool_name: "current_memories".into(),
            source_envelope_ids: Vec::new(),
            public_system_prompt: false,
        };
        for case in 0..2 {
            if case == 1 {
                input.source_provider_id = "assistant-data-result".into();
                input.source_envelope_ids = vec!["producer".into(), "read".into()];
            }
            call.input_lineage_sha256 = input_lineage_sha256(std::slice::from_ref(&input)).unwrap();
            let empty =
                AssistantNativeSourceSet::for_request(&session, &manifest, &call, &[], 1001)
                    .unwrap();
            assert!(
                empty
                    .validate_input_lineage(std::slice::from_ref(&input))
                    .is_err()
            );
        }
    }

    #[test]
    fn callbacks_seal_retained_context_but_cannot_add_direct_read_grants() {
        use crate::session::{TriggerOrigin, WorkKind};
        let (mut session, mut manifest, proof, mut call) = fixture();
        manifest.read_projection_ids.clear();
        call.authority_expires_at_unix_ms = Some(2000);
        session.trigger_origin = TriggerOrigin::WorkCompletion {
            kind: WorkKind::ComputerAction,
        };
        manifest.scope_sha256 = super::super::data_access::scope_sha256(&session).unwrap();
        let set =
            AssistantNativeSourceSet::for_request(&session, &manifest, &call, &[], 1001).unwrap();
        set.context.validate_session(&session).unwrap();
        assert!(set.authorizations.is_empty());
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session,
                &manifest,
                &proof,
                call.clone(),
                None,
                1001,
            )
            .is_err()
        );
        let mut direct = manifest.clone();
        direct
            .read_projection_ids
            .push(proof.usage.projection_id.clone());
        assert!(
            AssistantNativeSourceSet::for_request(&session, &direct, &call, &[], 1001).is_err()
        );
    }

    #[test]
    fn explicit_subagent_wait_preserves_original_retrieval_grants() {
        let (mut session, mut manifest, proof, call) = fixture();
        session.trigger_origin = crate::session::TriggerOrigin::SubAgentCompletion;
        manifest.scope_sha256 = super::super::data_access::scope_sha256(&session).unwrap();
        let before = session
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .clone();
        let grant = AssistantDataExportAuthorization::for_projection(
            &session, &manifest, &proof, call, None, 1001,
        )
        .unwrap();
        assert_eq!(grant.input_revision, session.input_revision);
        assert_eq!(
            session.assistant_data.as_ref().unwrap().retrieval_budget,
            before
        );
    }

    #[test]
    fn native_context_rejects_invalid_time_subject_scope_and_temporary_origins() {
        let (session, mut manifest, _, mut call) = fixture();
        manifest.read_projection_ids.clear();
        call.authority_expires_at_unix_ms = Some(2000);
        for case in 0..7 {
            let mut changed = session.clone();
            let mut selected = manifest.clone();
            match case {
                0 => selected.actor_id = "other".into(),
                1 => selected.input_revision += 1,
                2 => selected.lease_token += 1,
                3 => selected.scope_sha256 = "c".repeat(64),
                4 => {
                    changed
                        .assistant_data
                        .as_mut()
                        .unwrap()
                        .current_input_recorded_at_unix_ms = 1002
                }
                5 => {
                    changed
                        .assistant_data
                        .as_mut()
                        .unwrap()
                        .current_input_timezone = "invalid-timezone".into()
                }
                _ => changed.assistant_data.as_mut().unwrap().temporary = true,
            }
            assert!(
                AssistantNativeSourceSet::for_request(&changed, &selected, &call, &[], 1001)
                    .is_err(),
                "case {case}"
            );
        }
    }

    #[test]
    fn native_source_sets_seal_every_actual_use_and_reject_missing_changed_or_mixed_calls() {
        let (session, mut manifest, proof, call) = fixture();
        let first = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &proof,
            call.clone(),
            None,
            1001,
        )
        .unwrap();
        let mut second_proof = proof.clone();
        second_proof.usage.projection_id =
            native_ids::stable(Kind::AssistantReadProjection, &"second-read");
        manifest
            .read_projection_ids
            .push(second_proof.usage.projection_id.clone());
        let second = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &second_proof,
            call,
            None,
            1001,
        )
        .unwrap();
        let set = AssistantNativeSourceSet::from_grants(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(
            set,
            AssistantNativeSourceSet::from_grants(&[second.clone(), first.clone()]).unwrap()
        );
        assert!(set.validate_grants(std::slice::from_ref(&first)).is_err());
        assert!(
            set.validate_grants(&[first.clone(), first.clone()])
                .is_err()
        );
        for field in 0..5 {
            let mut changed = second.clone();
            match field {
                0 => changed.input_revision += 1,
                1 => changed.control_generation += 1,
                2 => changed.authorized_at_unix_ms += 1,
                3 => changed.source_contract_revision += 1,
                _ => changed.native_call.input_lineage_sha256 = "f".repeat(64),
            }
            assert!(
                set.validate_grants(&[first.clone(), changed.clone()])
                    .is_err()
            );
            assert!(AssistantNativeSourceSet::from_grants(&[first.clone(), changed]).is_err());
        }
        let encoded = serde_json::to_string(&set).unwrap();
        assert_eq!(AssistantNativeSourceSet::decode(&encoded).unwrap(), set);
        assert!(AssistantNativeSourceSet::decode("invalid-json").is_err());
        assert!(AssistantNativeSourceSet::from_grants(&[]).is_err());
        assert!(AssistantNativeSourceSet::from_grants(&[first.clone(), first]).is_err());
    }

    #[test]
    fn physical_use_is_bound_to_exact_receipt_recipient_and_complete_read_proof() {
        let (session, manifest, proof, call) = fixture();
        let grant = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &proof,
            call.clone(),
            None,
            1001,
        )
        .unwrap();
        grant.validate_projection(&proof).unwrap();
        let mut second_call = call;
        second_call.physical_call_id = "native-physical-call-2".into();
        let second = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &proof,
            second_call,
            None,
            1002,
        )
        .unwrap();
        assert_ne!(grant.authorization_id, second.authorization_id);
        let mut changed_proof = proof.clone();
        changed_proof.request_sha256 = "c".repeat(64);
        assert!(grant.validate_projection(&changed_proof).is_err());
        let mut changed_grant = grant.clone();
        changed_grant.native_call.physical_call_id = "another-call".into();
        assert!(changed_grant.validate().is_err());
        let mut changed_grant = grant;
        changed_grant.actor_id = "another-owner".into();
        assert!(changed_grant.validate().is_err());
    }

    #[test]
    fn changed_scope_control_memory_policy_or_input_cannot_reuse_a_prepared_read() {
        let (session, manifest, proof, call) = fixture();
        for field in 0..6 {
            let mut changed = manifest.clone();
            match field {
                0 => changed.control_generation += 1,
                1 => changed.memory_state_generation += 1,
                2 => changed.policy_revision += 1,
                3 => changed.actor_input_seq += 1,
                4 => changed.scope_sha256 = "d".repeat(64),
                _ => changed.lease_token += 1,
            }
            assert!(
                AssistantDataExportAuthorization::for_projection(
                    &session,
                    &changed,
                    &proof,
                    call.clone(),
                    None,
                    1001
                )
                .is_err()
            );
        }
        let mut missing = manifest;
        missing.read_projection_ids.clear();
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session, &missing, &proof, call, None, 1001
            )
            .is_err()
        );
    }

    #[test]
    fn consumed_proof_survives_cache_ttl_but_each_send_is_short_and_bounded_by_hard_expiry() {
        let (session, manifest, mut proof, call) = fixture();
        let now = proof.unused_until_unix_ms + 1;
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session,
                &manifest,
                &proof,
                call.clone(),
                None,
                now
            )
            .is_err()
        );
        let grant = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &proof,
            call.clone(),
            Some(1001),
            now,
        )
        .unwrap();
        assert_eq!(grant.expires_at_unix_ms, now + MAX_NATIVE_EXPORT_TTL_MS);
        proof.usage.expires_at_unix_ms = Some(now + 10);
        proof.source_envelope.retention.expires_at_unix_ms = Some(now + 10);
        let grant = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &proof,
            call.clone(),
            Some(1001),
            now,
        )
        .unwrap();
        assert_eq!(grant.expires_at_unix_ms, now + 10);
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session,
                &manifest,
                &proof,
                call,
                Some(1001),
                now + 10
            )
            .is_err()
        );
    }

    #[test]
    fn task_call_requires_its_original_native_deadline_and_never_extends_it() {
        let (mut session, mut manifest, mut proof, mut call) = fixture();
        super::super::data_access::bind_opened_task(&mut session, "goal", 9000).unwrap();
        manifest.task_id = Some("goal".into());
        manifest.scope_sha256 = scope_sha256(&session).unwrap();
        proof.task_id = Some("goal".into());
        proof.usage.scope_sha256 = manifest.scope_sha256.clone();
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session,
                &manifest,
                &proof,
                call.clone(),
                None,
                1001
            )
            .is_err()
        );
        call.authority_expires_at_unix_ms = Some(1200);
        let grant = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &proof,
            call.clone(),
            None,
            1001,
        )
        .unwrap();
        assert_eq!(grant.expires_at_unix_ms, 1200);
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session, &manifest, &proof, call, None, 1200
            )
            .is_err()
        );
    }

    #[test]
    fn memory_sends_require_the_original_memory_consent_and_its_own_deadline() {
        let (mut session, mut manifest, mut proof, mut call) = fixture();
        let data = session.assistant_data.as_mut().unwrap();
        data.history_access_scope
            .as_mut()
            .unwrap()
            .expires_at_unix_ms = Some(1100);
        data.memory_access_scope
            .as_mut()
            .unwrap()
            .expires_at_unix_ms = Some(2000);
        manifest.scope_sha256 = scope_sha256(&session).unwrap();
        proof.usage.scope_sha256 = manifest.scope_sha256.clone();
        call.authority_expires_at_unix_ms = Some(9000);
        let grant = AssistantDataExportAuthorization::for_projection(
            &session,
            &manifest,
            &proof,
            call.clone(),
            None,
            1001,
        )
        .unwrap();
        assert_eq!(grant.expires_at_unix_ms, 2000);
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session,
                &manifest,
                &proof,
                call.clone(),
                None,
                2000
            )
            .is_err()
        );
        session.assistant_data.as_mut().unwrap().memory_access_scope = None;
        manifest.scope_sha256 = scope_sha256(&session).unwrap();
        proof.usage.scope_sha256 = manifest.scope_sha256.clone();
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session, &manifest, &proof, call, None, 1001
            )
            .is_err()
        );
    }

    #[test]
    fn owner_view_purpose_temporary_conversations_and_non_model_recipients_never_grant_export() {
        let (mut session, manifest, mut proof, mut call) = fixture();
        proof.usage.purpose = SourceReadPurpose::OwnerHistory;
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session,
                &manifest,
                &proof,
                call.clone(),
                None,
                1001
            )
            .is_err()
        );
        proof.usage.purpose = SourceReadPurpose::ModelMemory;
        call.destination = DestinationIdentity::EmailAccount {
            account_id: "mail".into(),
        };
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session,
                &manifest,
                &proof,
                call.clone(),
                None,
                1001
            )
            .is_err()
        );
        session.assistant_data.as_mut().unwrap().temporary = true;
        assert!(
            AssistantDataExportAuthorization::for_projection(
                &session, &manifest, &proof, call, None, 1001
            )
            .is_err()
        );
    }
}
