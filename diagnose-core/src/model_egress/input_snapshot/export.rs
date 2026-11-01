//! Historical evidence of the exact identity-preserving sink export. This is
//! not a reusable declassification, copy, retention, or execution grant.

use super::*;
use desk_agent_protocol::data_lineage::{
    Sensitivity, TransformationAuthority, TransformationRecord,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeModelInputExport {
    pub source_message_id: String,
    pub source: DataEnvelope,
    pub transformation: TransformationRecord,
}

impl NativeModelInputExport {
    pub(super) fn validate(
        &self,
        destination: &DestinationIdentity,
        projected: &DataEnvelope,
    ) -> Result<(), &'static str> {
        self.source
            .validate()
            .map_err(|_| "invalid original export label")?;
        self.transformation
            .validate()
            .map_err(|_| "invalid native export transformation")?;
        let TransformationAuthority::ExplicitDeclassificationGrant { grant_id } =
            &self.transformation.authority
        else {
            return Err("native sink export requires its original explicit authority identity");
        };
        if self.source_message_id.trim().is_empty()
            || self.source_message_id.len() > 512
            || self.source_message_id.chars().any(char::is_control)
            || self.source.sensitivity == Sensitivity::Secret
            || self.source.allowed_destinations.contains(destination)
            || self.transformation.input_envelope_ids != [self.source.envelope_id.clone()]
            || self.transformation.output_envelope_id == self.source.envelope_id
            || self.transformation.output_envelope_id
                != crate::model_egress::model_export_envelope_id(
                    grant_id,
                    &self.source.envelope_id,
                    &self.source_message_id,
                )
        {
            return Err("native export replaced its original source or sink identity");
        }
        let mut expected = self.source.clone();
        expected.envelope_id = self.transformation.output_envelope_id.clone();
        expected.allowed_destinations.push(destination.clone());
        expected.allowed_destinations.sort();
        expected.provenance.source_envelope_ids = vec![self.source.envelope_id.clone()];
        if &expected != projected {
            return Err("native export changed source content or constraints");
        }
        Ok(())
    }

    pub(super) fn validate_authorization(&self, actual: &str) -> Result<(), &'static str> {
        if !matches!(&self.transformation.authority, TransformationAuthority::ExplicitDeclassificationGrant { grant_id } if grant_id == actual)
        {
            return Err("native export used a different dispatch authorization");
        }
        Ok(())
    }
}
