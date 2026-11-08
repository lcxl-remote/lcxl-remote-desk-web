//! Exact original input labels, without prompt or result bytes. These labels
//! constrain later derivations; their existence grants no new source access.

use super::{ModelInputLineage, project_model_input_lineage, sink_metadata_sha256};
use crate::sink_authorizer::{MAX_SINK_BYTES, SinkProjectionAudit};
use desk_agent_protocol::data_lineage::{DataEnvelope, DestinationIdentity};
use serde::{Deserialize, Serialize};

mod export;
pub use export::NativeModelInputExport;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeModelInputSnapshot {
    pub destination: DestinationIdentity,
    pub total_bytes: u64,
    pub envelopes: Vec<DataEnvelope>,
    pub exports: Vec<NativeModelInputExport>,
}

impl NativeModelInputSnapshot {
    pub fn capture(
        audit: &SinkProjectionAudit,
        envelopes: &[DataEnvelope],
    ) -> Result<Self, &'static str> {
        Self::capture_with_exports(audit, envelopes, &[])
    }

    pub fn capture_with_exports(
        audit: &SinkProjectionAudit,
        envelopes: &[DataEnvelope],
        exports: &[NativeModelInputExport],
    ) -> Result<Self, &'static str> {
        if envelopes.is_empty() || envelopes.len() > MAX_SINK_BYTES {
            return Err("invalid native input snapshot count");
        }
        // Check the borrowed labels before cloning them or allocating JSON.
        sink_metadata_sha256(&(&audit.destination, audit.total_bytes, envelopes, exports))?;
        project_model_input_lineage(audit, envelopes)
            .map_err(|_| "native input snapshot changed its exact sink")?;
        let mut unique = std::collections::BTreeMap::new();
        for edge in exports {
            if unique
                .insert(&edge.transformation.output_envelope_id, edge)
                .is_some_and(|old| old != edge)
            {
                return Err("conflicting native export metadata");
            }
        }
        let value = Self {
            destination: audit.destination.clone(),
            total_bytes: audit.total_bytes as u64,
            envelopes: envelopes.to_vec(),
            exports: unique.into_values().cloned().collect(),
        };
        value.validate_exports()?;
        Ok(value)
    }

    pub fn audit(&self) -> Result<SinkProjectionAudit, &'static str> {
        self.audited_metadata().map(|(audit, _)| audit)
    }

    fn audited_metadata(&self) -> Result<(SinkProjectionAudit, String), &'static str> {
        if self.envelopes.is_empty()
            || self.envelopes.len() > MAX_SINK_BYTES
            || self.total_bytes == 0
            || self.total_bytes > MAX_SINK_BYTES as u64
        {
            return Err("invalid native input snapshot bounds");
        }
        self.destination
            .validate()
            .map_err(|_| "invalid native input snapshot recipient")?;
        if !matches!(self.destination, DestinationIdentity::Model { .. }) {
            return Err("native input snapshot requires a model sink");
        }
        let fingerprint = sink_metadata_sha256(self)?;
        for envelope in &self.envelopes {
            envelope
                .validate()
                .map_err(|_| "invalid native input snapshot label")?;
        }
        self.validate_exports()?;
        Ok((
            SinkProjectionAudit {
                destination: self.destination.clone(),
                envelope_ids: self
                    .envelopes
                    .iter()
                    .map(|input| input.envelope_id.clone())
                    .collect(),
                total_bytes: self.total_bytes as usize,
            },
            fingerprint,
        ))
    }

    pub fn validate(
        &self,
        audit: &SinkProjectionAudit,
        lineage: &[ModelInputLineage],
    ) -> Result<(), &'static str> {
        let original = self.audit()?;
        if original.destination != audit.destination
            || original.total_bytes != audit.total_bytes
            || original.envelope_ids != audit.envelope_ids
        {
            return Err("native input snapshot replaced its original sink");
        }
        self.require_lineage(&original, lineage)
    }

    pub(crate) fn audit_with_lineage(
        &self,
        lineage: &[ModelInputLineage],
    ) -> Result<SinkProjectionAudit, &'static str> {
        let audit = self.audit()?;
        self.require_lineage(&audit, lineage)?;
        Ok(audit)
    }

    fn require_lineage(
        &self,
        audit: &SinkProjectionAudit,
        lineage: &[ModelInputLineage],
    ) -> Result<(), &'static str> {
        let actual = project_model_input_lineage(audit, &self.envelopes)
            .map_err(|_| "invalid original input labels")?;
        if actual != lineage {
            return Err("native input snapshot replaced its original lineage");
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<String, &'static str> {
        let (audit, fingerprint) = self.audited_metadata()?;
        project_model_input_lineage(&audit, &self.envelopes)
            .map_err(|_| "invalid native input snapshot labels")?;
        Ok(fingerprint)
    }

    pub fn validate_export_authorization(
        &self,
        authorization_id: &str,
    ) -> Result<(), &'static str> {
        self.validate_exports()?;
        for edge in &self.exports {
            edge.validate_authorization(authorization_id)?;
        }
        Ok(())
    }

    /// Ordered original labels before identity-preserving sink exports. These
    /// remain historical metadata and grant no fresh source or sink permission.
    /// This view uses the snapshot validated at its native/storage boundary.
    pub fn policy_sources(&self) -> Vec<&DataEnvelope> {
        let exports = self
            .exports
            .iter()
            .map(|edge| {
                (
                    edge.transformation.output_envelope_id.as_str(),
                    &edge.source,
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        self.envelopes
            .iter()
            .map(|input| {
                exports
                    .get(input.envelope_id.as_str())
                    .copied()
                    .unwrap_or(input)
            })
            .collect()
    }

    fn validate_exports(&self) -> Result<(), &'static str> {
        if self.exports.len() > self.envelopes.len() {
            return Err("native export metadata exceeds its input count");
        }
        let mut labels = std::collections::BTreeMap::new();
        for label in &self.envelopes {
            if labels
                .insert(label.envelope_id.as_str(), label)
                .is_some_and(|old| old != label)
            {
                return Err("native input identity has conflicting full labels");
            }
        }
        let mut originals = std::collections::BTreeMap::new();
        let mut used = std::collections::BTreeSet::new();
        for edge in &self.exports {
            let projected = labels
                .get(edge.transformation.output_envelope_id.as_str())
                .copied()
                .ok_or("native export is absent from its actual input set")?;
            if !used.insert(&edge.transformation.output_envelope_id) {
                return Err("duplicate native export metadata");
            }
            edge.validate(&self.destination, projected)?;
            if labels
                .get(edge.source.envelope_id.as_str())
                .is_some_and(|old| *old != &edge.source)
                || originals
                    .insert(edge.source.envelope_id.as_str(), &edge.source)
                    .is_some_and(|old| old != &edge.source)
            {
                return Err("native export source identity has conflicting full labels");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
