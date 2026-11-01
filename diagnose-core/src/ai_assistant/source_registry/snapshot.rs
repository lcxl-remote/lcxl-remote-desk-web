//! Immutable leaf registrations from the exact audited native input snapshot.
//! Missing native parent/reference proofs stay unresolved, never permissive.

use super::*;
use crate::model_egress::{NativeModelInputSnapshot, sink_metadata_sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeInputLeafPolicy {
    pub contract: RegisteredSourceContract,
    pub policy_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeInputPolicyRegistration {
    pub source_contract_revision: u64,
    pub policy_revision: u64,
    pub input_snapshot_sha256: String,
    /// Ordered one-to-one with the complete original snapshot. Equivalent
    /// policies share a dictionary entry; actual inputs are never deduplicated.
    pub leaves: Vec<Option<NativeInputLeafPolicy>>,
    pub policies: BTreeMap<String, SourceUsePolicy>,
}

impl NativeInputPolicyRegistration {
    pub fn capture(
        snapshot: &NativeModelInputSnapshot,
        source_contract_revision: u64,
        policy_revision: u64,
    ) -> Result<Self, &'static str> {
        if source_contract_revision == 0 || policy_revision == 0 {
            return Err("missing original input policy revision");
        }
        let input_snapshot_sha256 = snapshot.fingerprint()?;
        let mut policies = BTreeMap::new();
        let mut leaves = Vec::with_capacity(snapshot.envelopes.len());
        for registration in registered_inputs(snapshot, source_contract_revision, policy_revision)?
        {
            leaves.push(match registration {
                Some(entry) => {
                    let reference = NativeInputLeafPolicy {
                        contract: entry.contract,
                        policy_sha256: entry.policy.policy_sha256.clone(),
                    };
                    if policies
                        .insert(reference.policy_sha256.clone(), entry.policy.clone())
                        .is_some_and(|old| old != entry.policy)
                    {
                        return Err("conflicting native policy dictionary entry");
                    }
                    Some(reference)
                }
                None => None,
            });
        }
        let value = Self {
            source_contract_revision,
            policy_revision,
            input_snapshot_sha256,
            leaves,
            policies,
        };
        value.validate_metadata(snapshot, source_contract_revision, policy_revision)?;
        Ok(value)
    }

    /// Structural historical evidence only. This intentionally does not apply
    /// current code to an unavailable original contract revision.
    pub fn validate_metadata(
        &self,
        snapshot: &NativeModelInputSnapshot,
        original_contract_revision: u64,
        original_policy_revision: u64,
    ) -> Result<(), &'static str> {
        if self.source_contract_revision == 0
            || self.policy_revision == 0
            || self.source_contract_revision != original_contract_revision
            || self.policy_revision != original_policy_revision
            || self.input_snapshot_sha256 != snapshot.fingerprint()?
            || self.leaves.len() != snapshot.envelopes.len()
        {
            return Err("native policy registration changed its original snapshot");
        }
        let mut referenced = BTreeSet::new();
        for entry in self.leaves.iter().flatten() {
            if !self.policies.contains_key(&entry.policy_sha256) {
                return Err("native leaf policy dictionary is incomplete");
            }
            referenced.insert(&entry.policy_sha256);
        }
        if referenced.len() != self.policies.len() {
            return Err("native policy dictionary contains unused policy claims");
        }
        for (hash, policy) in &self.policies {
            validate_policy(policy)?;
            if hash != &policy.policy_sha256 || policy.policy_revision != self.policy_revision {
                return Err("native leaf policy dictionary changed");
            }
        }
        sink_metadata_sha256(self)?;
        Ok(())
    }

    pub fn validate_registered_leaves(
        &self,
        snapshot: &NativeModelInputSnapshot,
    ) -> Result<(), &'static str> {
        self.validate_metadata(
            snapshot,
            self.source_contract_revision,
            self.policy_revision,
        )?;
        if self.source_contract_revision != SOURCE_CONTRACT_REVISION {
            return Err("original source contract revision is unavailable");
        }
        for (entry, expected) in self.leaves.iter().zip(registered_inputs(
            snapshot,
            self.source_contract_revision,
            self.policy_revision,
        )?) {
            match (entry, expected) {
                (None, None) => {}
                // A missing historical control registration grants nothing.
                // Its original label and actual parents remain mandatory.
                (None, Some(expected))
                    if expected.contract == RegisteredSourceContract::NativeControl => {}
                (Some(reference), Some(expected))
                    if reference.contract == expected.contract
                        && reference.policy_sha256 == expected.policy.policy_sha256
                        && self.policies.get(&reference.policy_sha256)
                            == Some(&expected.policy) => {}
                _ => return Err("native leaf registration changed its compiled source policy"),
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    /// This is a historical policy result, never a live read/send grant. A
    /// single unresolved actual input prevents claiming a complete derivation.
    pub fn complete_leaf_text_policy(
        &self,
        snapshot: &NativeModelInputSnapshot,
    ) -> Result<Option<SourceUsePolicy>, &'static str> {
        self.validate_registered_leaves(snapshot)?;
        if self.leaves.iter().any(Option::is_none) {
            return Ok(None);
        }
        super::super::source::derive_visible_text_policy(
            &self.policies.values().cloned().collect::<Vec<_>>(),
            self.policy_revision,
        )
        .map(Some)
    }
}

/// Controls and derived reads are not independent roots. Every parent is an actual
/// input in this immutable snapshot, where its own native/read policy remains
/// mandatory. No ancestor labels, bodies or permissions are invented here.
fn registered_inputs(
    snapshot: &NativeModelInputSnapshot,
    source_contract_revision: u64,
    policy_revision: u64,
) -> Result<Vec<Option<RegisteredSourcePolicy>>, &'static str> {
    let sources = snapshot.policy_sources();
    if source_contract_revision != SOURCE_CONTRACT_REVISION {
        return Ok(vec![None; sources.len()]);
    }
    let mut entries = sources
        .iter()
        .map(|source| registered_leaf(source, policy_revision, source_contract_revision))
        .collect::<Result<Vec<_>, _>>()?;
    let contracts = sources
        .iter()
        .map(|source| {
            if native_control(source) {
                Some(RegisteredSourceContract::NativeControl)
            } else {
                native_read(source)
            }
        })
        .collect::<Vec<_>>();
    let mut labels = BTreeMap::new();
    for (index, (original, projected)) in sources.iter().zip(&snapshot.envelopes).enumerate() {
        for label in [*original, projected] {
            if labels
                .insert(label.envelope_id.as_str(), (index, label))
                .is_some_and(|(_, old)| old != label)
            {
                return Err("native source parent has conflicting full labels");
            }
        }
    }
    // Iterative closure keeps cycles unresolved without recursive stack use.
    // A non-control parent still needs its registered leaf, native archive or
    // original read proof in the existing complete retention inspection.
    loop {
        let mut progress = false;
        for (index, source) in sources.iter().enumerate() {
            let Some(contract) = contracts[index].filter(|_| entries[index].is_none()) else {
                continue;
            };
            let parents = source
                .provenance
                .source_envelope_ids
                .iter()
                .map(|id| labels.get(id.as_str()).copied())
                .collect::<Option<Vec<_>>>();
            let Some(parents) = parents else {
                continue;
            };
            if parents.iter().any(|(parent_index, parent)| {
                if contracts[*parent_index].is_some() && entries[*parent_index].is_none() {
                    return true;
                }
                if contract != RegisteredSourceContract::NativeControl {
                    return false;
                }
                source.sensitivity < parent.sensitivity
                    || source.retention.most_restrictive(parent.retention) != source.retention
                    || source.allowed_destinations.iter().any(|destination| !parent.allowed_destinations.contains(destination))
                    || matches!(parent.content, ContentRef::EphemeralObservation { expires_at_unix_ms, .. }
                        if !source.retention.delete_with_run || source.retention.expires_at_unix_ms.is_none_or(|until| until > expires_at_unix_ms))
            }) { continue; }
            entries[index] = Some(registration(
                source,
                contract,
                policy_revision,
                source_contract_revision,
            )?);
            progress = true;
        }
        if !progress {
            return Ok(entries);
        }
    }
}

/// Native reads produce fresh content. Their actual parents remain separate
/// inputs whose copy restrictions and hard deadlines constrain the answer.
fn native_read(source: &DataEnvelope) -> Option<RegisteredSourceContract> {
    if source.provenance.source_envelope_ids.is_empty()
        || source
            .provenance
            .source_object_id
            .as_deref()
            .is_some_and(|id| id.starts_with("live-target:sha256:"))
        || !matches!(&source.content, ContentRef::ImmutableBlob { blob_id, sha256, size_bytes, media_type }
            if blob_id.strip_prefix("native-read-snapshot-").is_some_and(|id| !id.is_empty())
                && sha256 == &source.digest_sha256 && *size_bytes > 0 && media_type == "application/octet-stream")
    {
        return None;
    }
    let contract = classify(source);
    matches!(
        contract,
        RegisteredSourceContract::DeviceObservation
            | RegisteredSourceContract::UiObservation
            | RegisteredSourceContract::MediaObservation
            | RegisteredSourceContract::PublicResearchResult
            | RegisteredSourceContract::ProviderPrivate
            | RegisteredSourceContract::Secret
    )
    .then_some(contract)
}

/// Match the compiled producer's complete label shape, not a model-selected
/// source name. Parent policies above prevent control text laundering data.
#[inline(never)]
fn native_control(source: &DataEnvelope) -> bool {
    use sha2::{Digest, Sha256};
    if source.provenance.source_envelope_ids.is_empty() {
        return false;
    }
    let ContentRef::ImmutableBlob {
        blob_id,
        sha256,
        size_bytes,
        media_type,
    } = &source.content
    else {
        return false;
    };
    if sha256 != &source.digest_sha256 || *size_bytes == 0 {
        return false;
    }
    let provenance = &source.provenance;
    if provenance.source_provider_id == crate::dynamic_run::RUN_CONTROL_PROVIDER_ID {
        if provenance.source_object_id.is_some() {
            return false;
        }
        match provenance.source_tool_name.as_str() {
            "owner_context_time"
            | "context_review"
            | "runtime_context"
            | "input_watermark"
            | "latest_input_projection"
            | "schedule_activation"
            | "scheduled_task_input"
            | "invalid_tool_arguments"
            | "memory_batch_rejected"
            | "manage_memories"
            | "memory_control_rejected"
            | "schedule_proposal"
            | "schedule_query"
            | "schedule_cancellation"
            | "schedule_proposal_failed" => {
                let Some(id) = source.envelope_id.strip_prefix("status-result-") else {
                    return false;
                };
                id.len() == 64
                    && provenance.source_envelope_ids.len() == 1
                    && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && media_type == "text/plain"
                    && blob_id == &format!("status-content-{:x}", Sha256::digest(sha256.as_bytes()))
            }
            "delegation_state" => {
                source
                    .envelope_id
                    .strip_prefix("delegation-")
                    .is_some_and(|id| blob_id == &format!("delegation-message-{id}"))
                    && media_type == "application/json"
            }
            _ => false,
        }
    } else if provenance.source_provider_id == "assistant-runtime-control" {
        let Some(id) = provenance.source_object_id.as_deref() else {
            return false;
        };
        let prefix = match provenance.source_tool_name.as_str() {
            "scheduled-continuation" => "scheduled-resume",
            "permission-decision-resume" => "permission-resume",
            _ => return false,
        };
        source.envelope_id == format!("{prefix}-message-{id}")
            && provenance.source_envelope_ids.len() == 1
            && blob_id == &format!("{prefix}-content-{id}")
            && media_type == "text/plain;charset=utf-8"
    } else {
        false
    }
}
