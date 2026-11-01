//! Exact forgotten facts and their known fragments, distinct from producer lineage.

use crate::native_ids::{self, Kind};
use desk_agent_protocol::ai_assistant::source::HistoryDependency;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownMemorySource {
    pub source_id: String,
    pub revision: u64,
    pub origin_id: String,
    pub content_sha256: String,
}

impl KnownMemorySource {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !native_ids::is(Kind::AssistantSource, &self.source_id)
            || !native_ids::is(Kind::AssistantOrigin, &self.origin_id)
            || self.revision == 0
            || self.content_sha256.len() != 64
            || !self.content_sha256.bytes().all(|v| v.is_ascii_hexdigit())
        {
            return Err("invalid known memory source");
        }
        Ok(())
    }

    pub fn matches(&self, dependency: &HistoryDependency) -> bool {
        self.source_id == dependency.source_id
            && self.revision == dependency.source_revision
            && self.origin_id == dependency.origin_id
            && self.content_sha256 == dependency.content_sha256
    }
}

pub fn canonical_sources(
    mut sources: Vec<KnownMemorySource>,
) -> Result<Vec<KnownMemorySource>, &'static str> {
    for source in &sources {
        source.validate()?;
    }
    sources.sort_by(|a, b| {
        (&a.source_id, a.revision, &a.origin_id, &a.content_sha256).cmp(&(
            &b.source_id,
            b.revision,
            &b.origin_id,
            &b.content_sha256,
        ))
    });
    sources.dedup();
    Ok(sources)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryControlTarget {
    pub memory_id: String,
    pub revision: u64,
    pub scope_key: String,

    pub accepted_input_seq: u64,
    pub known_sources: Vec<KnownMemorySource>,
}

/// Created from the locked, owner-scoped rows. No model or REST body supplies it.
pub fn manifest_hash(
    actor: &str,
    cutoff_input_seq: u64,
    targets: &[MemoryControlTarget],
) -> Result<String, &'static str> {
    desk_agent_protocol::ai_assistant::source::validate_source_id(actor)?;
    if cutoff_input_seq == 0 {
        return Err("missing memory control cutoff");
    }
    let mut ordered = targets.to_vec();
    ordered.sort_by(|a, b| (&a.memory_id, a.revision).cmp(&(&b.memory_id, b.revision)));
    let mut identities = BTreeSet::new();
    for target in &mut ordered {
        if !native_ids::is(Kind::Memory, &target.memory_id)
            || target.revision == 0
            || !identities.insert(target.memory_id.clone())
            || target.accepted_input_seq > cutoff_input_seq
            || target.scope_key.is_empty()
        {
            return Err("invalid memory control target");
        }
        target.known_sources = canonical_sources(std::mem::take(&mut target.known_sources))?;
    }
    let bytes = serde_json::to_vec(&("memory-control-v1", actor, cutoff_input_seq, ordered))
        .map_err(|_| "invalid memory control manifest")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
/// A shared origin alone never makes an unrelated fragment a forgotten source.
pub fn known_source_is_suppressed(
    known: &[KnownMemorySource],
    dependency: &HistoryDependency,
) -> bool {
    known.iter().any(|source| source.matches(dependency))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(seed: &str) -> KnownMemorySource {
        KnownMemorySource {
            source_id: native_ids::stable(Kind::AssistantSource, &seed),
            revision: 1,
            origin_id: native_ids::stable(Kind::AssistantOrigin, &"same-message"),
            content_sha256: "a".repeat(64),
        }
    }

    fn target(seed: &str) -> MemoryControlTarget {
        MemoryControlTarget {
            memory_id: native_ids::stable(Kind::Memory, &seed),
            revision: 1,
            scope_key: "personal".into(),

            accepted_input_seq: 2,
            known_sources: vec![source(seed)],
        }
    }

    #[test]
    fn a_shared_origin_does_not_suppress_an_unrelated_fragment() {
        let known = source("preference");
        let unrelated = source("unrelated-background");
        let mut dependency = HistoryDependency {
            source_id: known.source_id.clone(),
            source_revision: 1,
            content_sha256: known.content_sha256.clone(),
            origin_id: known.origin_id.clone(),
            origin_deletion_generation: 1,
            policy_revision: 1,
            expires_at_unix_ms: None,
        };
        assert!(known_source_is_suppressed(
            std::slice::from_ref(&known),
            &dependency
        ));
        dependency.source_id = unrelated.source_id;
        assert!(!known_source_is_suppressed(&[known], &dependency));
    }

    #[test]
    fn a_control_manifest_is_order_independent_and_cannot_cover_a_later_assertion() {
        let a = target("A");
        let b = target("B");
        assert_eq!(
            manifest_hash("owner", 3, &[a.clone(), b.clone()]),
            manifest_hash("owner", 3, &[b, a.clone()])
        );
        assert!(manifest_hash("owner", 1, std::slice::from_ref(&a)).is_err());
        assert!(manifest_hash("owner", 3, &[a.clone(), a.clone()]).is_err());
        let original = manifest_hash("owner", 3, std::slice::from_ref(&a)).unwrap();
        let mut changed = a;
        changed.revision = 2;
        assert_ne!(original, manifest_hash("owner", 3, &[changed]).unwrap());
    }

    #[test]
    fn overlapping_evidence_deduplicates_without_an_independent_source_count_cap() {
        let known = (0..256)
            .map(|index| source(&format!("source-{index}")))
            .collect::<Vec<_>>();
        let repeated = (0..8).flat_map(|_| known.clone()).collect::<Vec<_>>();
        assert_eq!(canonical_sources(repeated).unwrap().len(), 256);
        let mut too_many = known;
        too_many.push(source("extra"));
        assert_eq!(canonical_sources(too_many).unwrap().len(), 257);
    }
}
