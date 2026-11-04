//! Registered retention policy and deterministic, redacted history projections.

use desk_agent_protocol::{
    ai_assistant::{
        source::{AssistantSourceKind, HistoryDependency, SourcePolicyClass, SourceUsePolicy},
        user_instructions::AssistantDataPolicy,
    },
    data_lineage::{DataEnvelope, DestinationIdentity, Sensitivity},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::LazyLock};

use super::retrieval::{IndexProjection, index_source};
use crate::{
    native_ids::{self, Kind},
    redaction::{Redactor, RegexRedactor},
};

static HISTORY_REDACTOR: LazyLock<RegexRedactor> = LazyLock::new(RegexRedactor::new);

/// Deterministic locators carry no authority; hosts still validate native rows.
pub fn history_origin_id(
    actor: &str,
    device: &str,
    conversation: &str,
    native: &str,
    kind: AssistantSourceKind,
) -> String {
    native_ids::stable(
        Kind::AssistantOrigin,
        &(actor, device, conversation, native, kind),
    )
}

pub fn history_source_id(origin: &str, fragment_index: u32) -> String {
    native_ids::stable(Kind::AssistantSource, &(origin, fragment_index))
}

pub fn valid_statement_timezone(value: &str) -> bool {
    value.len() <= 128 && value.parse::<chrono_tz::Tz>().is_ok()
}

pub fn policy_hash(policy: &SourceUsePolicy) -> Result<String, &'static str> {
    let mut unsigned = policy.clone();
    unsigned.policy_sha256.clear();
    let bytes = serde_json::to_vec(&unsigned).map_err(|_| "invalid source policy")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Called by the trusted input producer, never from model-supplied policy fields.
pub fn owner_text_policy(revision: u64, temporary: bool) -> SourceUsePolicy {
    let mut policy = SourceUsePolicy {
        policy_revision: revision,
        policy_sha256: String::new(),
        class: SourcePolicyClass::UserText,
        history_retention_allowed: !temporary,
        independent_fact_allowed: !temporary,
        derived_visible_text_allowed: !temporary,
        no_copy: temporary,
        no_export: false,
        content_hard_expires_at_unix_ms: None,
        reference_fresh_until_unix_ms: None,
        fixed_destinations: Vec::new(),
    };
    policy.policy_sha256 = policy_hash(&policy).expect("static source policy");
    policy
}

pub fn validate_policy(policy: &SourceUsePolicy) -> Result<(), &'static str> {
    if policy.policy_revision == 0
        || policy.policy_sha256 != policy_hash(policy)?
        || policy
            .fixed_destinations
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != policy.fixed_destinations.len()
        || [
            policy.content_hard_expires_at_unix_ms,
            policy.reference_fresh_until_unix_ms,
        ]
        .into_iter()
        .flatten()
        .any(|until| until == 0 || until > i64::MAX as u64)
        || policy
            .fixed_destinations
            .iter()
            .any(|d| d.validate().is_err())
    {
        return Err("invalid assistant source policy");
    }
    Ok(())
}

pub fn allows_model_use(
    policy: &SourceUsePolicy,
    destination: &DestinationIdentity,
    now: u64,
) -> bool {
    validate_policy(policy).is_ok()
        && policy.history_retention_allowed
        && !policy.no_export
        && !policy.no_copy
        && !matches!(
            policy.class,
            SourcePolicyClass::Media
                | SourcePolicyClass::ProviderPrivate
                | SourcePolicyClass::Unknown
        )
        && policy
            .content_hard_expires_at_unix_ms
            .is_none_or(|until| until > now)
        && (policy.fixed_destinations.is_empty() || policy.fixed_destinations.contains(destination))
}

/// Independent fact evidence has its own retention proof. Natural history
/// retention is not an export prerequisite for an already retained fact.
pub fn allows_independent_fact_use(
    policy: &SourceUsePolicy,
    destination: &DestinationIdentity,
    now: u64,
) -> bool {
    validate_policy(policy).is_ok()
        && policy.independent_fact_allowed
        && !policy.no_export
        && !policy.no_copy
        && !matches!(
            policy.class,
            SourcePolicyClass::Media
                | SourcePolicyClass::ProviderPrivate
                | SourcePolicyClass::Unknown
        )
        && policy
            .content_hard_expires_at_unix_ms
            .is_none_or(|until| until > now)
        && (policy.fixed_destinations.is_empty() || policy.fixed_destinations.contains(destination))
}

/// Derived text keeps every hard constraint from every actual producer input.
pub fn derive_visible_text_policy(
    parents: &[SourceUsePolicy],
    revision: u64,
) -> Result<SourceUsePolicy, &'static str> {
    if parents.is_empty() || revision == 0 {
        return Err("missing derived source policies");
    }
    crate::model_egress::sink_metadata_sha256(parents)?;
    for parent in parents {
        validate_policy(parent)?;
    }
    let mut fixed: Option<BTreeSet<DestinationIdentity>> = None;
    for parent in parents.iter().filter(|p| !p.fixed_destinations.is_empty()) {
        let candidates = parent
            .fixed_destinations
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        fixed = Some(match fixed {
            Some(current) => current.intersection(&candidates).cloned().collect(),
            None => candidates,
        });
    }
    let incompatible_destinations = fixed.as_ref().is_some_and(BTreeSet::is_empty);
    let unclassified_or_private = parents.iter().any(|parent| {
        matches!(
            parent.class,
            SourcePolicyClass::ProviderPrivate | SourcePolicyClass::Unknown
        )
    });
    let mut result = SourceUsePolicy {
        policy_revision: revision,
        policy_sha256: String::new(),
        class: SourcePolicyClass::AssistantText,
        history_retention_allowed: !unclassified_or_private
            && parents
                .iter()
                .all(|p| p.derived_visible_text_allowed && !p.no_copy),
        independent_fact_allowed: !unclassified_or_private
            && parents.iter().all(|p| {
                p.independent_fact_allowed
                    && p.derived_visible_text_allowed
                    && p.class != SourcePolicyClass::Media
                    && !p.no_copy
            }),
        derived_visible_text_allowed: !unclassified_or_private
            && parents.iter().all(|p| p.derived_visible_text_allowed),
        no_copy: unclassified_or_private || parents.iter().any(|p| p.no_copy),
        no_export: unclassified_or_private
            || incompatible_destinations
            || parents.iter().any(|p| p.no_export),
        content_hard_expires_at_unix_ms: parents
            .iter()
            .filter_map(|p| p.content_hard_expires_at_unix_ms)
            .min(),
        reference_fresh_until_unix_ms: parents
            .iter()
            .filter_map(|p| p.reference_fresh_until_unix_ms)
            .min(),
        fixed_destinations: fixed.unwrap_or_default().into_iter().collect(),
    };
    result.policy_sha256 = policy_hash(&result)?;
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRetentionProof {
    pub actor_id: String,
    pub device_ref: String,
    pub origin_id: String,
    pub original_envelope_id: String,
    pub accepted_input_seq: u64,
    pub original_content_sha256: String,
    pub retained_content_sha256: String,
    pub production_dependencies_sha256: String,
    pub native_metadata_sha256: String,
    pub source_policy_sha256: String,
    pub policy_revision: u64,
    pub recorded_at_unix_ms: u64,
    pub statement_timezone: String,
    pub expires_at_unix_ms: u64,
}

pub struct HistoryCapture<'a> {
    pub native: Option<&'a super::native_history::NativeHistoryMetadata>,
    pub actor_id: &'a str,
    pub device_ref: &'a str,
    pub directory: Option<HistorySourceDirectory<'a>>,
    pub conversation_id: &'a str,
    pub root_conversation_id: &'a str,
    pub native_id: &'a str,
    pub accepted_input_seq: u64,
    pub kind: AssistantSourceKind,
    pub role: &'a str,
    pub content: &'a str,
    pub envelope: &'a DataEnvelope,
    pub source_policy: &'a SourceUsePolicy,
    pub dependencies: &'a [HistoryDependency],
    pub recorded_at_unix_ms: u64,
    pub statement_timezone: &'a str,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct HistorySourceDirectory<'a> {
    pub instance_id: &'a str,
    pub audience: &'a str,
    pub device_name: Option<&'a str>,
}

pub struct PreparedHistoryCapture {
    pub origin_id: String,
    pub source_ids: Vec<String>,
    pub retained_content: String,
    pub content_sha256: String,
    pub index: IndexProjection,
    pub proof: HistoryRetentionProof,
    pub redacted: bool,
}

pub fn history_dependencies_sha256(
    dependencies: &[HistoryDependency],
) -> Result<String, &'static str> {
    if dependencies.len() > desk_agent_protocol::data_lineage::MAX_LINEAGE_ITEMS {
        return Err("too many history producer dependencies");
    }
    let mut ordered = dependencies.iter().collect::<Vec<_>>();
    ordered
        .sort_by(|a, b| (&a.source_id, a.source_revision).cmp(&(&b.source_id, b.source_revision)));
    let mut seen = BTreeSet::new();
    for dependency in &ordered {
        if !native_ids::is(Kind::AssistantSource, &dependency.source_id)
            || !native_ids::is(Kind::AssistantOrigin, &dependency.origin_id)
            || dependency.source_revision == 0
            || dependency.origin_deletion_generation == 0
            || dependency.policy_revision == 0
            || dependency.content_sha256.len() != 64
            || !dependency
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || dependency
                .expires_at_unix_ms
                .is_some_and(|until| until == 0 || until > i64::MAX as u64)
            || !seen.insert((&dependency.source_id, dependency.source_revision))
        {
            return Err("invalid history producer dependency");
        }
    }
    crate::model_egress::sink_metadata_sha256(&ordered)
}

/// Original scheduled output, owner supplements and native state observations
/// can precede ordinary owner chat. This checks format only; hosts still prove
/// the original native acceptance and current owner/source authorization.
pub fn zero_owner_input_allowed(
    kind: AssistantSourceKind,
    role: &str,
    conversation: &str,
    root: &str,
    native: Option<&super::native_history::NativeHistoryMetadata>,
) -> bool {
    match kind {
        AssistantSourceKind::Input => role == "user" && native.is_some(),
        AssistantSourceKind::AssistantMessage => {
            role == "assistant" && conversation == root && conversation.starts_with("schedule-run-")
        }
        AssistantSourceKind::NativeStatus => {
            role == "system"
                && conversation == root
                && native.is_some_and(|value| value.status_snapshot.is_some())
        }
        _ => false,
    }
}

pub fn prepare_capture(
    capture: &HistoryCapture<'_>,
    policy: &AssistantDataPolicy,
) -> Result<Option<PreparedHistoryCapture>, &'static str> {
    policy.validate()?;
    validate_policy(capture.source_policy)?;
    capture
        .envelope
        .validate()
        .map_err(|_| "invalid assistant source envelope")?;
    if capture.recorded_at_unix_ms == 0
        || capture.revision == 0
        || (capture.accepted_input_seq == 0
            && !zero_owner_input_allowed(
                capture.kind,
                capture.role,
                capture.conversation_id,
                capture.root_conversation_id,
                capture.native,
            ))
        || capture.accepted_input_seq > i64::MAX as u64
    {
        return Err("invalid assistant history origin");
    }
    if !valid_statement_timezone(capture.statement_timezone) {
        return Err("invalid assistant statement timezone");
    }
    let production_dependencies_sha256 = history_dependencies_sha256(capture.dependencies)?;
    let original_content_sha256 = format!("{:x}", Sha256::digest(capture.content.as_bytes()));
    if let Some(native) = capture.native {
        native.validate(
            capture.kind,
            &original_content_sha256,
            capture.recorded_at_unix_ms,
        )?;
    } else if matches!(
        capture.kind,
        AssistantSourceKind::Receipt | AssistantSourceKind::NativeStatus
    ) {
        return Err("native history source is missing its original metadata proof");
    }
    let native_metadata_sha256 = super::native_history::metadata_sha256(capture.native)?;
    for identity in [
        capture.actor_id,
        capture.conversation_id,
        capture.root_conversation_id,
        capture.native_id,
    ] {
        desk_agent_protocol::ai_assistant::source::validate_source_id(identity)?;
    }
    if capture.device_ref.is_empty()
        && (capture.directory.is_some()
            || capture.native.is_some()
            || capture.conversation_id != capture.root_conversation_id
            || !matches!(
                capture.kind,
                AssistantSourceKind::Input | AssistantSourceKind::AssistantMessage
            ))
    {
        return Err("device data requires its original device reference");
    }
    desk_agent_protocol::ai_assistant::source::validate_optional_device_ref(capture.device_ref)?;
    if let Some(directory) = capture.directory {
        for identity in [directory.instance_id, directory.audience] {
            desk_agent_protocol::ai_assistant::source::validate_source_id(identity)?;
        }
        if capture.device_ref
            != native_ids::stable(
                Kind::AssistantDeviceBinding,
                &(directory.instance_id, directory.audience),
            )
            || directory
                .device_name
                .is_some_and(|name| name.len() > 512 || name.chars().any(char::is_control))
        {
            return Err("invalid assistant source directory");
        }
    }
    if !capture.source_policy.history_retention_allowed
        || capture.source_policy.no_copy
        || capture.envelope.sensitivity == Sensitivity::Secret
        || matches!(
            capture.source_policy.class,
            SourcePolicyClass::Media
                | SourcePolicyClass::ProviderPrivate
                | SourcePolicyClass::Unknown
        )
    {
        return Ok(None);
    }
    let natural_expiry = capture
        .recorded_at_unix_ms
        .checked_add(u64::from(policy.history_retention_days) * 86_400_000)
        .ok_or("invalid assistant history expiry")?;
    let expires_at = capture
        .source_policy
        .content_hard_expires_at_unix_ms
        .into_iter()
        .chain(
            capture
                .dependencies
                .iter()
                .filter_map(|d| d.expires_at_unix_ms),
        )
        .fold(natural_expiry, u64::min);
    if expires_at <= capture.recorded_at_unix_ms {
        return Ok(None);
    }
    let redacted = HISTORY_REDACTOR
        .redact(capture.content)
        .map_err(|_| "history redaction unavailable")?;
    let retained_content = redacted.text;
    let mut index = index_source(&retained_content)?;
    index.partial |= capture
        .native
        .is_some_and(|native| native.projection_partial);
    let origin_id = history_origin_id(
        capture.actor_id,
        capture.device_ref,
        capture.conversation_id,
        capture.native_id,
        capture.kind,
    );
    let source_ids = index
        .fragments
        .iter()
        .map(|part| history_source_id(&origin_id, part.fragment_index))
        .collect();
    let content_sha256 = format!("{:x}", Sha256::digest(retained_content.as_bytes()));
    Ok(Some(PreparedHistoryCapture {
        proof: HistoryRetentionProof {
            actor_id: capture.actor_id.into(),
            device_ref: capture.device_ref.into(),
            origin_id: origin_id.clone(),
            original_envelope_id: capture.envelope.envelope_id.clone(),
            accepted_input_seq: capture.accepted_input_seq,
            original_content_sha256,
            retained_content_sha256: content_sha256.clone(),
            production_dependencies_sha256,
            native_metadata_sha256,
            source_policy_sha256: capture.source_policy.policy_sha256.clone(),
            policy_revision: capture.source_policy.policy_revision,
            recorded_at_unix_ms: capture.recorded_at_unix_ms,
            statement_timezone: capture.statement_timezone.into(),
            expires_at_unix_ms: expires_at,
        },
        origin_id,
        source_ids,
        retained_content,
        content_sha256,
        index,
        redacted: !redacted.kinds.is_empty(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> DestinationIdentity {
        DestinationIdentity::Model {
            connection_id: id.into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        }
    }

    #[test]
    fn deriving_text_does_not_remove_hard_constraints_or_turn_incompatible_destinations_into_public()
     {
        let mut a = owner_text_policy(1, false);
        a.fixed_destinations = vec![model("a")];
        a.content_hard_expires_at_unix_ms = Some(50);
        a.policy_sha256 = policy_hash(&a).unwrap();
        let mut b = owner_text_policy(1, false);
        b.fixed_destinations = vec![model("b")];
        b.policy_sha256 = policy_hash(&b).unwrap();
        let result = derive_visible_text_policy(&[a.clone(), b], 1).unwrap();
        assert!(result.no_export);
        assert!(!allows_model_use(&result, &model("a"), 10));
        assert_eq!(result.content_hard_expires_at_unix_ms, Some(50));
        assert!(!allows_model_use(&a, &model("a"), 50));
        assert!(allows_model_use(&a, &model("a"), 49));
    }

    #[test]
    fn unknown_or_modified_policies_cannot_grant_retention() {
        let mut policy = owner_text_policy(1, true);
        assert!(!policy.history_retention_allowed);
        policy.history_retention_allowed = true;
        assert!(validate_policy(&policy).is_err());
        assert!(derive_visible_text_policy(&[], 1).is_err());
    }

    #[test]
    fn private_or_unknown_classification_cannot_be_laundered_through_an_optimistic_text_policy() {
        for class in [
            SourcePolicyClass::Unknown,
            SourcePolicyClass::ProviderPrivate,
        ] {
            let mut parent = owner_text_policy(1, false);
            parent.class = class;
            parent.policy_sha256 = policy_hash(&parent).unwrap();
            validate_policy(&parent).unwrap();
            let derived =
                derive_visible_text_policy(&[parent, owner_text_policy(1, false)], 1).unwrap();
            assert!(
                !derived.history_retention_allowed
                    && !derived.independent_fact_allowed
                    && !derived.derived_visible_text_allowed
            );
            assert!(derived.no_copy && derived.no_export);
            assert!(!allows_model_use(&derived, &model("any"), 10));
            assert!(!allows_independent_fact_use(&derived, &model("any"), 10));
            let next = derive_visible_text_policy(&[derived], 1).unwrap();
            assert!(!next.history_retention_allowed && next.no_copy && next.no_export);
        }
    }

    #[test]
    fn media_can_allow_finite_visible_text_without_becoming_an_independent_stable_fact() {
        let mut parent = owner_text_policy(1, false);
        parent.class = SourcePolicyClass::Media;
        parent.content_hard_expires_at_unix_ms = Some(100);
        parent.policy_sha256 = policy_hash(&parent).unwrap();
        assert!(!allows_model_use(&parent, &model("any"), 10));
        let derived = derive_visible_text_policy(&[parent], 1).unwrap();
        assert!(derived.history_retention_allowed && derived.derived_visible_text_allowed);
        assert!(!derived.independent_fact_allowed);
        assert!(allows_model_use(&derived, &model("any"), 99));
        assert!(!allows_model_use(&derived, &model("any"), 100));
        assert!(!allows_independent_fact_use(&derived, &model("any"), 99));
    }

    #[test]
    fn source_policy_rejects_invalid_deadlines_and_duplicate_fixed_recipients_even_with_valid_hash()
    {
        for case in 0..4 {
            let mut policy = owner_text_policy(1, false);
            match case {
                0 => policy.content_hard_expires_at_unix_ms = Some(0),
                1 => policy.reference_fresh_until_unix_ms = Some(0),
                2 => policy.content_hard_expires_at_unix_ms = Some(i64::MAX as u64 + 1),
                _ => policy.fixed_destinations = vec![model("one"), model("one")],
            }
            policy.policy_sha256 = policy_hash(&policy).unwrap();
            assert!(validate_policy(&policy).is_err());
            assert!(derive_visible_text_policy(&[policy], 1).is_err());
        }
    }

    #[test]
    fn independent_evidence_export_preserves_hard_policy_without_requiring_original_history_retention()
     {
        let mut policy = owner_text_policy(1, false);
        policy.history_retention_allowed = false;
        policy.fixed_destinations = vec![model("original-account")];
        policy.content_hard_expires_at_unix_ms = Some(100);
        policy.policy_sha256 = policy_hash(&policy).unwrap();
        assert!(!allows_model_use(&policy, &model("original-account"), 10));
        assert!(allows_independent_fact_use(
            &policy,
            &model("original-account"),
            10
        ));
        assert!(!allows_independent_fact_use(
            &policy,
            &model("another-account"),
            10
        ));
        assert!(!allows_independent_fact_use(
            &policy,
            &model("original-account"),
            100
        ));
        policy.no_export = true;
        policy.policy_sha256 = policy_hash(&policy).unwrap();
        assert!(!allows_independent_fact_use(
            &policy,
            &model("original-account"),
            10
        ));
        policy.no_export = false;
        policy.no_copy = true;
        policy.policy_sha256 = policy_hash(&policy).unwrap();
        assert!(!allows_independent_fact_use(
            &policy,
            &model("original-account"),
            10
        ));
    }

    #[test]
    fn history_producer_fingerprint_is_order_independent_but_rejects_trimming_or_conflicting_versions()
     {
        let source = |name: &str| HistoryDependency {
            source_id: native_ids::stable(Kind::AssistantSource, &name),
            source_revision: 1,
            content_sha256: "a".repeat(64),
            origin_id: native_ids::stable(Kind::AssistantOrigin, &name),
            origin_deletion_generation: 1,
            policy_revision: 1,
            expires_at_unix_ms: None,
        };
        let a = source("A");
        let b = source("B");
        let original = history_dependencies_sha256(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(
            original,
            history_dependencies_sha256(&[b, a.clone()]).unwrap()
        );
        assert_ne!(
            original,
            history_dependencies_sha256(std::slice::from_ref(&a)).unwrap()
        );
        let mut changed = a.clone();
        changed.origin_deletion_generation += 1;
        assert_ne!(
            history_dependencies_sha256(std::slice::from_ref(&a)).unwrap(),
            history_dependencies_sha256(&[changed]).unwrap()
        );
        let mut conflicting = a.clone();
        conflicting.content_sha256 = "b".repeat(64);
        assert!(history_dependencies_sha256(&[a, conflicting]).is_err());
    }
}
