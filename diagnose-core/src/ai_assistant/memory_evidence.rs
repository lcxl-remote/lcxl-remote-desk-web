//! Independent stable-fact evidence retains registered hard source constraints.

use super::{
    memory_control::{KnownMemorySource, canonical_sources},
    source::{valid_statement_timezone, validate_policy},
};
use crate::native_ids::{self, Kind};
use desk_agent_protocol::ai_assistant::source::{
    AssistantSourceKind, HistoryDependency, SourcePolicyClass, SourceRefRole, SourceUsePolicy,
    validate_source_id,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MEMORY_EVIDENCE_PROOF_VERSION: u16 = 3;

/// An authenticated independent copy keeps the origin's hard constraints,
/// rather than the original chat fragment's temporary read availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedMemoryOrigin {
    pub origin_id: String,
    pub deletion_generation: u64,
    pub accepted_input_seq: u64,
    /// The origin row's policy, including its hard content deadline.
    pub policy: SourceUsePolicy,
}

/// Built from authenticated source and memory-revision rows. No transport or
/// model argument may populate this snapshot directly.
pub struct MemoryEvidenceSnapshot<'a> {
    pub actor_id: &'a str,
    pub memory_id: &'a str,
    pub memory_revision: u64,
    pub memory_accepted_input_seq: u64,
    pub evidence_source_id: &'a str,
    pub role: SourceRefRole,
    pub body: &'a str,
    pub origin_id: &'a str,
    pub origin_kind: AssistantSourceKind,
    pub origin_deletion_generation: u64,
    pub origin_accepted_input_seq: u64,
    pub recorded_at_unix_ms: u64,
    pub statement_timezone: &'a str,
    pub origin_policy: &'a SourceUsePolicy,
    pub evidence_policy: &'a SourceUsePolicy,
    pub known_sources: &'a [KnownMemorySource],
    pub forget_sources: &'a [KnownMemorySource],
    pub production_dependencies: &'a [HistoryDependency],
    pub retained_origins: &'a [RetainedMemoryOrigin],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEvidenceRetentionProof {
    pub schema_version: u16,
    pub actor_id: String,
    pub memory_id: String,
    pub memory_revision: u64,
    pub memory_accepted_input_seq: u64,
    pub evidence_source_id: String,
    pub role: SourceRefRole,
    pub origin_id: String,
    pub origin_kind: AssistantSourceKind,
    pub origin_deletion_generation: u64,
    pub origin_accepted_input_seq: u64,
    /// The origin row's policy. The body, source lists and evidence policy
    /// live in this proof's own row and are read from there.
    pub origin_policy: SourceUsePolicy,
    pub retained_origins: Vec<RetainedMemoryOrigin>,
    pub recorded_at_unix_ms: u64,
    pub statement_timezone: String,
    pub hard_expires_at_unix_ms: Option<u64>,
}

impl MemoryEvidenceRetentionProof {
    pub fn from_snapshot(source: &MemoryEvidenceSnapshot<'_>) -> Result<Self, &'static str> {
        if source.body.trim().is_empty() {
            return Err("invalid independent memory evidence body");
        }
        Self::from_identity(source)
    }

    fn from_identity(source: &MemoryEvidenceSnapshot<'_>) -> Result<Self, &'static str> {
        validate_source_id(source.actor_id)?;
        validate_policy(source.origin_policy)?;
        validate_policy(source.evidence_policy)?;
        let (known, _) = source_links(
            source.known_sources,
            source.forget_sources,
            source.origin_id,
        )?;
        if !native_ids::is(Kind::Memory, source.memory_id)
            || !native_ids::is(Kind::AssistantSource, source.evidence_source_id)
            || !native_ids::is(Kind::AssistantOrigin, source.origin_id)
            || source.memory_revision == 0
            || source.memory_accepted_input_seq == 0
            || source.memory_accepted_input_seq > i64::MAX as u64
            || source.origin_accepted_input_seq == 0
            || source.origin_accepted_input_seq > source.memory_accepted_input_seq
            || source.origin_deletion_generation == 0
            || source.recorded_at_unix_ms == 0
            || source.recorded_at_unix_ms > i64::MAX as u64
            || !valid_statement_timezone(source.statement_timezone)
            || known.is_empty()
            || !known
                .iter()
                .any(|entry| entry.origin_id == source.origin_id)
            || !retained_snapshot_policy(source.origin_policy)
            || !retained_snapshot_policy(source.evidence_policy)
        {
            return Err("invalid independent memory evidence source");
        }
        if source.role == SourceRefRole::Evidence
            && (!source.origin_policy.independent_fact_allowed
                || source.origin_accepted_input_seq != source.memory_accepted_input_seq
                || !matches!(
                    (source.origin_kind, source.origin_policy.class),
                    (AssistantSourceKind::Input, SourcePolicyClass::UserText)
                        | (
                            AssistantSourceKind::Receipt,
                            SourcePolicyClass::StructuredObservation
                        )
                ))
        {
            return Err("memory evidence requires an eligible original assertion or observation");
        }
        let dependencies = canonical_dependencies(source.production_dependencies)?;
        let retained_origins = canonical_retained_origins(source.retained_origins)?;
        if retained_origins.iter().any(|origin| {
            origin.origin_id == source.origin_id
                || origin.accepted_input_seq > source.memory_accepted_input_seq
                || !known
                    .iter()
                    .any(|entry| entry.origin_id == origin.origin_id)
        }) {
            return Err("independent production origin is outside the authenticated source set");
        }
        let mut known_identities = BTreeSet::new();
        if known.iter().any(|entry| {
            !known_identities.insert((&entry.source_id, entry.revision))
                || (entry.origin_id != source.origin_id
                    && !dependencies
                        .iter()
                        .any(|dependency| entry.matches(dependency))
                    && !retained_origins
                        .iter()
                        .any(|origin| origin.origin_id == entry.origin_id))
        }) {
            return Err("known memory evidence is not covered by its origin or production proof");
        }
        let hard_expires = [
            source.origin_policy.content_hard_expires_at_unix_ms,
            source.evidence_policy.content_hard_expires_at_unix_ms,
        ]
        .into_iter()
        .flatten()
        .chain(
            dependencies
                .iter()
                .filter_map(|entry| entry.expires_at_unix_ms),
        )
        .chain(
            retained_origins
                .iter()
                .filter_map(|origin| origin.policy.content_hard_expires_at_unix_ms),
        )
        .min();
        if hard_expires
            .is_some_and(|until| until <= source.recorded_at_unix_ms || until > i64::MAX as u64)
        {
            return Err("invalid independent memory evidence hard deadline");
        }
        Ok(Self {
            schema_version: MEMORY_EVIDENCE_PROOF_VERSION,
            actor_id: source.actor_id.into(),
            memory_id: source.memory_id.into(),
            memory_revision: source.memory_revision,
            memory_accepted_input_seq: source.memory_accepted_input_seq,
            evidence_source_id: source.evidence_source_id.into(),
            role: source.role,
            origin_id: source.origin_id.into(),
            origin_kind: source.origin_kind,
            origin_deletion_generation: source.origin_deletion_generation,
            origin_accepted_input_seq: source.origin_accepted_input_seq,
            origin_policy: source.origin_policy.clone(),
            retained_origins,
            recorded_at_unix_ms: source.recorded_at_unix_ms,
            statement_timezone: source.statement_timezone.into(),
            hard_expires_at_unix_ms: hard_expires,
        })
    }

    pub fn validate_snapshot(
        &self,
        source: &MemoryEvidenceSnapshot<'_>,
        row_expiry: Option<u64>,
        now: u64,
    ) -> Result<(), &'static str> {
        if self != &Self::from_snapshot(source)? {
            return Err("independent memory evidence body or identity changed");
        }
        self.validate_lifetime(row_expiry, now)
    }

    /// A damaged evidence body must not disable owner deletion. Source-link
    /// suppression still requires the original two independently bound sets;
    /// this check grants neither body access nor a new write authorization.
    pub fn forgetting_sources(
        &self,
        known: &[KnownMemorySource],
        forget: &[KnownMemorySource],
    ) -> Result<Vec<KnownMemorySource>, &'static str> {
        let (_, forget) = source_links(known, forget, &self.origin_id)?;
        if self.schema_version != MEMORY_EVIDENCE_PROOF_VERSION {
            return Err("memory forgetting source proof changed");
        }
        Ok(forget)
    }

    /// Verify retained identity without reading the snapshot's body. Model
    /// callers must prove natural cleanup and an independently retained copy;
    /// original owner-history viewing has its own memory-deletion semantics.
    /// Neither use authorizes fetching or recreating the removed memory body.
    pub fn validate_identity(
        &self,
        source: &MemoryEvidenceSnapshot<'_>,
        row_expiry: Option<u64>,
        now: u64,
    ) -> Result<(), &'static str> {
        if self != &Self::from_identity(source)? {
            return Err("independent memory evidence identity changed");
        }
        self.validate_lifetime(row_expiry, now)
    }

    fn validate_lifetime(&self, row_expiry: Option<u64>, now: u64) -> Result<(), &'static str> {
        if self.schema_version != MEMORY_EVIDENCE_PROOF_VERSION
            || self
                .hard_expires_at_unix_ms
                .is_some_and(|until| until <= now || row_expiry.is_none_or(|expiry| expiry > until))
            || row_expiry.is_some_and(|until| until <= now || until > i64::MAX as u64)
        {
            return Err("independent memory evidence proof changed or expired");
        }
        Ok(())
    }
}

fn source_links(
    known: &[KnownMemorySource],
    forget: &[KnownMemorySource],
    origin: &str,
) -> Result<(Vec<KnownMemorySource>, Vec<KnownMemorySource>), &'static str> {
    let known = canonical_sources(known.to_vec())?;
    let forget = canonical_sources(forget.to_vec())?;
    if forget.is_empty()
        || forget
            .iter()
            .any(|entry| entry.origin_id != origin || !known.contains(entry))
    {
        return Err("memory forgetting sources require exact retained evidence inputs");
    }
    Ok((known, forget))
}

/// Whether generated visible text may be retained in unrestricted personal memory.
/// New model-authored writes require a policy valid for independent personal storage.
pub fn retained_text_policy(policy: &SourceUsePolicy) -> bool {
    retained_snapshot_policy(policy) && !policy.no_export
}

// Existing retained evidence may be owner-visible while remaining forbidden to export.
fn retained_snapshot_policy(policy: &SourceUsePolicy) -> bool {
    policy.history_retention_allowed
        && policy.derived_visible_text_allowed
        && !policy.no_copy
        && policy.fixed_destinations.is_empty()
        && !matches!(
            policy.class,
            SourcePolicyClass::Media
                | SourcePolicyClass::ProviderPrivate
                | SourcePolicyClass::Unknown
        )
}

pub fn canonical_retained_origins(
    sources: &[RetainedMemoryOrigin],
) -> Result<Vec<RetainedMemoryOrigin>, &'static str> {
    let mut ordered = sources.to_vec();
    ordered.sort_by(|a, b| a.origin_id.cmp(&b.origin_id));
    ordered.dedup();
    let mut seen = BTreeSet::new();
    for origin in &ordered {
        if !native_ids::is(Kind::AssistantOrigin, &origin.origin_id)
            || !seen.insert(&origin.origin_id)
            || origin.deletion_generation == 0
            || origin.accepted_input_seq == 0
            || origin.accepted_input_seq > i64::MAX as u64
            || validate_policy(&origin.policy).is_err()
            || origin
                .policy
                .content_hard_expires_at_unix_ms
                .is_some_and(|until| until == 0 || until > i64::MAX as u64)
        {
            return Err("invalid independent production origin");
        }
    }
    Ok(ordered)
}

fn canonical_dependencies(
    sources: &[HistoryDependency],
) -> Result<Vec<HistoryDependency>, &'static str> {
    let mut values = sources.to_vec();
    values
        .sort_by(|a, b| (&a.source_id, a.source_revision).cmp(&(&b.source_id, b.source_revision)));
    let mut seen = BTreeSet::new();
    for value in &values {
        if !native_ids::is(Kind::AssistantSource, &value.source_id)
            || !native_ids::is(Kind::AssistantOrigin, &value.origin_id)
            || value.source_revision == 0
            || value.origin_deletion_generation == 0
            || value.policy_revision == 0
            || !seen.insert((&value.source_id, value.source_revision))
            || value
                .expires_at_unix_ms
                .is_some_and(|until| until == 0 || until > i64::MAX as u64)
        {
            return Err("invalid memory production dependency");
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::super::source::owner_text_policy;
    use super::*;

    struct Fixture {
        memory: String,
        evidence: String,
        origin: String,
        role: SourceRefRole,
        kind: AssistantSourceKind,
        origin_seq: u64,
        generation: u64,
        body: String,
        policy: SourceUsePolicy,
        known: Vec<KnownMemorySource>,
        dependencies: Vec<HistoryDependency>,
    }
    impl Fixture {
        fn new() -> Self {
            let origin = native_ids::stable(Kind::AssistantOrigin, &"assertion");
            Self {
                memory: native_ids::stable(Kind::Memory, &"language"),
                evidence: native_ids::stable(Kind::AssistantSource, &"evidence"),
                known: vec![KnownMemorySource {
                    source_id: native_ids::stable(Kind::AssistantSource, &"original-fragment"),
                    revision: 1,
                    origin_id: origin.clone(),
                }],
                origin,
                role: SourceRefRole::Evidence,
                kind: AssistantSourceKind::Input,
                origin_seq: 4,
                generation: 1,
                body: "I prefer Chinese".into(),
                policy: owner_text_policy(1, false),
                dependencies: Vec::new(),
            }
        }
        fn snapshot(&self) -> MemoryEvidenceSnapshot<'_> {
            MemoryEvidenceSnapshot {
                actor_id: "owner",
                memory_id: &self.memory,
                memory_revision: 1,
                memory_accepted_input_seq: 4,
                evidence_source_id: &self.evidence,
                role: self.role,
                body: &self.body,
                origin_id: &self.origin,
                origin_kind: self.kind,
                origin_deletion_generation: self.generation,
                origin_accepted_input_seq: self.origin_seq,
                recorded_at_unix_ms: 100,
                statement_timezone: "UTC",
                origin_policy: &self.policy,
                evidence_policy: &self.policy,
                known_sources: &self.known,
                forget_sources: &self.known[..1],
                production_dependencies: &self.dependencies,
                retained_origins: &[],
            }
        }
    }

    #[test]
    fn full_production_sources_and_selected_forgetting_sources_have_independent_proofs() {
        let mut data = Fixture::new();
        let first = data.known[0].clone();
        let second = KnownMemorySource {
            source_id: native_ids::stable(Kind::AssistantSource, &"another-original-fragment"),
            revision: 1,
            origin_id: data.origin.clone(),
        };
        data.known.push(second.clone());
        let mut snapshot = data.snapshot();
        snapshot.forget_sources = std::slice::from_ref(&first);
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&snapshot).unwrap();
        proof.validate_snapshot(&snapshot, None, 1000).unwrap();
        assert_eq!(
            proof
                .forgetting_sources(&data.known, std::slice::from_ref(&first))
                .unwrap(),
            vec![first.clone()]
        );
        // Forgetting sources must stay within the origin and the known set.
        let mut foreign = second.clone();
        foreign.origin_id = native_ids::stable(Kind::AssistantOrigin, &"foreign-origin");
        assert!(
            proof
                .forgetting_sources(&data.known, std::slice::from_ref(&foreign))
                .is_err()
        );
        snapshot.forget_sources = std::slice::from_ref(&second);
        // The source lists live in the proof's own row and are read from there.
        let other = MemoryEvidenceRetentionProof::from_snapshot(&snapshot).unwrap();
        assert_eq!(proof, other);
    }

    #[test]
    fn forgetting_sources_require_nonempty_exact_same_origin_membership() {
        let mut data = Fixture::new();
        let mut snapshot = data.snapshot();
        snapshot.forget_sources = &[];
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&snapshot).is_err());
        let mut changed = data.known[0].clone();
        changed.revision += 1;
        snapshot.forget_sources = std::slice::from_ref(&changed);
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&snapshot).is_err());
        let foreign = KnownMemorySource {
            source_id: native_ids::stable(Kind::AssistantSource, &"foreign-input"),
            revision: 1,
            origin_id: native_ids::stable(Kind::AssistantOrigin, &"foreign-origin"),
        };
        data.known.push(foreign.clone());
        data.dependencies.push(HistoryDependency {
            source_id: foreign.source_id.clone(),
            source_revision: foreign.revision,
            origin_id: foreign.origin_id.clone(),
            origin_deletion_generation: 1,
            policy_revision: 1,
            expires_at_unix_ms: Some(5000),
        });
        let mut snapshot = data.snapshot();
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&snapshot).is_ok());
        snapshot.forget_sources = std::slice::from_ref(&foreign);
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&snapshot).is_err());
    }

    #[test]
    fn independent_evidence_has_no_natural_history_deadline_and_binds_original_sequence() {
        let mut data = Fixture::new();
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).unwrap();
        assert_eq!(proof.hard_expires_at_unix_ms, None);
        proof
            .validate_snapshot(&data.snapshot(), None, 365 * 86_400_000)
            .unwrap();
        data.origin_seq = 5;
        assert!(
            proof
                .validate_snapshot(&data.snapshot(), None, 1000)
                .is_err()
        );
        data.origin_seq = 4;
        data.generation = 2;
        assert!(
            proof
                .validate_snapshot(&data.snapshot(), None, 1000)
                .is_err()
        );
    }

    #[test]
    fn hard_source_deadlines_survive_independent_retention_and_additional_expiry_only_narrows() {
        let mut data = Fixture::new();
        data.policy.content_hard_expires_at_unix_ms = Some(1000);
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).unwrap();
        assert!(
            proof
                .validate_snapshot(&data.snapshot(), None, 999)
                .is_err()
        );
        assert!(
            proof
                .validate_snapshot(&data.snapshot(), Some(1001), 999)
                .is_err()
        );
        proof
            .validate_snapshot(&data.snapshot(), Some(1000), 999)
            .unwrap();
        assert!(
            proof
                .validate_snapshot(&data.snapshot(), Some(1000), 1000)
                .is_err()
        );
        assert!(
            proof
                .validate_snapshot(&data.snapshot(), Some(900), 901)
                .is_err()
        );
    }

    #[test]
    fn old_sources_and_assistant_text_cannot_become_new_owner_assertions() {
        let mut data = Fixture::new();
        data.origin_seq = 3;
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).is_err());
        data.role = SourceRefRole::Background;
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).is_ok());
        data.origin_seq = 4;
        data.role = SourceRefRole::Evidence;
        data.kind = AssistantSourceKind::AssistantMessage;
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).is_err());
        data.kind = AssistantSourceKind::Receipt;
        data.policy.class = SourcePolicyClass::CommunicationReceipt;
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).is_err());
        data.policy.class = SourcePolicyClass::StructuredObservation;
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).is_ok());
    }

    #[test]
    fn known_background_fragments_require_a_complete_origin_proof_and_no_copy_is_preserved() {
        let mut data = Fixture::new();
        let known = KnownMemorySource {
            source_id: native_ids::stable(Kind::AssistantSource, &"background"),
            revision: 1,
            origin_id: native_ids::stable(Kind::AssistantOrigin, &"background"),
        };
        data.known.push(known.clone());
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).is_err());
        data.dependencies.push(HistoryDependency {
            source_id: known.source_id,
            source_revision: 1,
            origin_id: known.origin_id,
            origin_deletion_generation: 1,
            policy_revision: 1,
            expires_at_unix_ms: None,
        });
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).is_ok());
        data.policy.no_copy = true;
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).is_err());
    }

    #[test]
    fn retained_identity_survives_body_cleanup_without_becoming_a_body_read_proof() {
        let mut data = Fixture::new();
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).unwrap();
        data.body.clear();
        assert!(
            proof
                .validate_snapshot(&data.snapshot(), None, 1000)
                .is_err()
        );
        proof
            .validate_identity(&data.snapshot(), None, 1000)
            .unwrap();
        data.generation += 1;
        assert!(
            proof
                .validate_identity(&data.snapshot(), None, 1000)
                .is_err()
        );
    }

    #[test]
    fn identity_proof_keeps_hard_deadlines_and_no_copy_after_natural_body_cleanup() {
        let mut data = Fixture::new();
        data.policy.content_hard_expires_at_unix_ms = Some(1000);
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&data.snapshot()).unwrap();
        data.body.clear();
        proof
            .validate_identity(&data.snapshot(), Some(1000), 999)
            .unwrap();
        assert!(
            proof
                .validate_identity(&data.snapshot(), None, 999)
                .is_err()
        );
        assert!(
            proof
                .validate_identity(&data.snapshot(), Some(1001), 999)
                .is_err()
        );
        assert!(
            proof
                .validate_identity(&data.snapshot(), Some(1000), 1000)
                .is_err()
        );
        data.policy.no_copy = true;
        assert!(
            proof
                .validate_identity(&data.snapshot(), Some(1000), 999)
                .is_err()
        );
    }
    fn background_origin(data: &mut Fixture) -> RetainedMemoryOrigin {
        let source = KnownMemorySource {
            source_id: native_ids::stable(Kind::AssistantSource, &"independent-background"),
            revision: 1,
            origin_id: native_ids::stable(Kind::AssistantOrigin, &"independent-background"),
        };
        let origin = RetainedMemoryOrigin {
            origin_id: source.origin_id.clone(),
            deletion_generation: 2,
            accepted_input_seq: 3,
            policy: data.policy.clone(),
        };
        data.known.push(source);
        origin
    }

    #[test]
    fn independent_background_keeps_identity_without_the_original_chat_read_deadline() {
        let mut data = Fixture::new();
        let origin = background_origin(&mut data);
        let mut snapshot = data.snapshot();
        snapshot.retained_origins = std::slice::from_ref(&origin);
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&snapshot).unwrap();
        assert_eq!(proof.hard_expires_at_unix_ms, None);
        proof
            .validate_snapshot(&snapshot, None, 365 * 86_400_000)
            .unwrap();
        snapshot.retained_origins = &[];
        assert!(proof.validate_snapshot(&snapshot, None, 1000).is_err());
        let mut changed = origin.clone();
        changed.deletion_generation += 1;
        snapshot.retained_origins = std::slice::from_ref(&changed);
        assert!(proof.validate_snapshot(&snapshot, None, 1000).is_err());
    }

    #[test]
    fn independently_retained_background_still_binds_hard_expiry_and_exact_origin_membership() {
        let mut data = Fixture::new();
        let mut origin = background_origin(&mut data);
        origin.policy.content_hard_expires_at_unix_ms = Some(5000);
        let mut snapshot = data.snapshot();
        snapshot.retained_origins = std::slice::from_ref(&origin);
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&snapshot).unwrap();
        assert_eq!(proof.hard_expires_at_unix_ms, Some(5000));
        assert!(proof.validate_snapshot(&snapshot, None, 4999).is_err());
        proof
            .validate_snapshot(&snapshot, Some(5000), 4999)
            .unwrap();
        assert!(
            proof
                .validate_snapshot(&snapshot, Some(5000), 5000)
                .is_err()
        );
        let mut changed = origin.clone();
        changed.policy.no_export = !changed.policy.no_export;
        assert!(canonical_retained_origins(&[origin.clone(), changed]).is_err());
        let mut future = origin.clone();
        future.accepted_input_seq = 5;
        snapshot.retained_origins = std::slice::from_ref(&future);
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&snapshot).is_err());
        let mut changed = origin.clone();
        changed.origin_id = native_ids::stable(Kind::AssistantOrigin, &"unobserved-origin");
        snapshot.retained_origins = std::slice::from_ref(&changed);
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&snapshot).is_err());
    }
    #[test]
    fn full_bounded_origin_set_preserves_the_original_evidence_identity() {
        let mut data = Fixture::new();
        let mut retained = Vec::new();
        for index in 1..256 {
            let origin_id = native_ids::stable(Kind::AssistantOrigin, &index);
            data.known.push(KnownMemorySource {
                source_id: native_ids::stable(Kind::AssistantSource, &index),
                revision: 1,
                origin_id: origin_id.clone(),
            });
            retained.push(RetainedMemoryOrigin {
                origin_id,
                deletion_generation: 1,
                accepted_input_seq: 1,
                policy: data.policy.clone(),
            });
        }
        let mut snapshot = data.snapshot();
        snapshot.retained_origins = &retained;
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&snapshot).unwrap();
        proof.validate_snapshot(&snapshot, None, 1000).unwrap();
    }
    #[test]
    fn generated_text_needs_retention_permission_while_original_evidence_stays_independent() {
        let data = Fixture::new();
        let mut derived = data.policy.clone();
        derived.independent_fact_allowed = false;
        let mut snapshot = data.snapshot();
        snapshot.evidence_policy = &derived;
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&snapshot).is_ok());
        for case in 0..3 {
            let mut restricted = derived.clone();
            match case {
                0 => restricted.history_retention_allowed = false,
                1 => restricted.derived_visible_text_allowed = false,
                _ => restricted.no_copy = true,
            }
            let mut restricted_snapshot = data.snapshot();
            restricted_snapshot.evidence_policy = &restricted;
            assert!(MemoryEvidenceRetentionProof::from_snapshot(&restricted_snapshot).is_err());
        }
        let mut original = data.policy.clone();
        original.independent_fact_allowed = false;
        snapshot.evidence_policy = &derived;
        snapshot.origin_policy = &original;
        assert!(MemoryEvidenceRetentionProof::from_snapshot(&snapshot).is_err());
    }
}
