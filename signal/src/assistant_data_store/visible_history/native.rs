//! Frozen retention constraints for accepted conversation content.
use super::*;
use desk_diagnose_core::ai_assistant::{
    source::derive_visible_text_policy,
    source_registry::{RegisteredSourceContract, is_native_output_policy_source},
};
mod inputs;

#[derive(Clone)]
pub(crate) struct RetainedSources {
    pub policy: SourceUsePolicy,
    pub read_roots: Vec<desk_diagnose_core::ai_assistant::seam::AssistantReadRoot>,
}

pub(crate) fn retained_sources(root: &ArchivedAssistantOutput) -> Option<RetainedSources> {
    root.retained_policy
        .clone()
        .filter(|policy| policy.history_retention_allowed && !policy.no_copy)
        .map(|policy| RetainedSources {
            policy,
            read_roots: root.retained_roots.clone(),
        })
}

/// Resolve copy policy from the authenticated original input snapshot.
pub(crate) fn resolve_policy_on<'a>(
    db: &'a DatabaseTransaction,
    root: &'a ArchivedAssistantOutput,
    now: u64,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<RetainedSources>, AssistantStoreError>>
            + Send
            + 'a,
    >,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let sources = root.sources.input_snapshot.policy_sources();
        let registration = &root.input_policies;
        let reads = super::super::native_export::load_owner_history_set_on(db, root).await?;
        let mut policies = BTreeMap::new();
        let mut roots = BTreeMap::new();
        let mut read_inputs = BTreeMap::new();
        for read in &reads {
            let Some(policy) = read.snapshot_policy() else {
                return Ok(None);
            };
            policies.insert(policy.policy_sha256.clone(), policy.clone());
            read_inputs.insert(
                read.input_envelope().envelope_id.clone(),
                read.input_envelope(),
            );
            for source in &read.usage().roots {
                roots.insert(
                    (source.source_id.clone(), source.source_revision),
                    source.clone(),
                );
            }
        }
        for (source, leaf) in sources.into_iter().zip(&registration.leaves) {
            if let Some(read) = read_inputs.get(&source.envelope_id) {
                if *read != source {
                    return Err(AssistantStoreError::Inaccessible);
                }
                continue;
            }
            if is_native_output_policy_source(source) {
                // A parent's retention was decided when it was accepted and stays final.
                let head = desk_assistant_data_store::output_store::find_head_on(
                    db,
                    &root.source_set.consumer_conversation_id,
                    source,
                )
                .await?
                .ok_or(AssistantStoreError::Inaccessible)?;
                if head.actor_id != root.source_set.actor_id {
                    return Err(AssistantStoreError::Inaccessible);
                }
                let summary = head.summary()?;
                let Some(policy) = summary.retained_policy else {
                    return Ok(None);
                };
                let retained = RetainedSources {
                    policy,
                    read_roots: summary.retained_roots,
                };
                policies.insert(retained.policy.policy_sha256.clone(), retained.policy);
                for source in &retained.read_roots {
                    roots.insert(
                        (source.source_id.clone(), source.source_revision),
                        source.clone(),
                    );
                }
                continue;
            }
            let Some(leaf) = leaf else {
                return Ok(None);
            };
            if leaf.contract == RegisteredSourceContract::ConversationSnapshot {
                // Locators support citations only. A missing preparation record
                // cannot withdraw content already saved in this conversation.
                match super::super::projection::load_conversation_snapshot_on(
                    db,
                    &root.source_set.actor_id,
                    &root.source_set.consumer_conversation_id,
                    &root.source_set.source_device_ref,
                    source,
                )
                .await
                {
                    Ok(read) => {
                        for source in &read.proof.usage.roots {
                            roots.insert(
                                (source.source_id.clone(), source.source_revision),
                                source.clone(),
                            );
                        }
                    }
                    Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
                    Err(_) => {}
                }
            }
            if leaf.contract == RegisteredSourceContract::OwnerText {
                let Some(policy) = inputs::owner_text_on(db, root, source).await? else {
                    return Ok(None);
                };
                policies.insert(policy.policy_sha256.clone(), policy);
            } else if leaf.contract == RegisteredSourceContract::PublishedTaskInput {
                let Some(policy) = inputs::published_on(db, root, source, now).await? else {
                    return Ok(None);
                };
                policies.insert(policy.policy_sha256.clone(), policy);
            } else {
                let policy = registration
                    .policies
                    .get(&leaf.policy_sha256)
                    .ok_or(AssistantStoreError::Inaccessible)?;
                policies.insert(policy.policy_sha256.clone(), policy.clone());
            }
        }
        if policies.is_empty() {
            return Ok(None);
        }
        let policy = derive_visible_text_policy(
            &policies.into_values().collect::<Vec<_>>(),
            root.source_set.context.policy_revision,
        )
        .map_err(AssistantStoreError::Invalid)?;
        Ok(Some(RetainedSources {
            policy,
            read_roots: roots.into_values().collect(),
        }))
    })
}

/// Citation identities come from actual consumed inputs. Whole-answer copy
/// permission is a separate decision and does not withdraw an observed locator.
pub(crate) fn citation_roots_on<'a, C: sea_orm::ConnectionTrait>(
    db: &'a C,
    output: &'a ArchivedAssistantOutput,
) -> impl std::future::Future<
    Output = Result<
        Vec<desk_diagnose_core::ai_assistant::seam::AssistantReadRoot>,
        AssistantStoreError,
    >,
> + Send
+ 'a {
    desk_diagnose_core::future::send_phase(|| async move {
        let mut roots = BTreeMap::new();
        for read in super::super::native_export::load_owner_history_set_on(db, output).await? {
            for source in read.usage().roots.iter() {
                roots.insert(
                    (source.source_id.clone(), source.source_revision),
                    source.clone(),
                );
            }
        }
        for source in output.sources.input_snapshot.policy_sources() {
            if is_native_output_policy_source(source) {
                if let Some(parent) = super::super::output_archive::find_on(
                    db,
                    &output.source_set.consumer_conversation_id,
                    source,
                )
                .await?
                {
                    if parent.proof.source_set.actor_id != output.source_set.actor_id {
                        return Err(AssistantStoreError::Inaccessible);
                    }
                    for source in &parent.proof.retained_roots {
                        roots.insert(
                            (source.source_id.clone(), source.source_revision),
                            source.clone(),
                        );
                    }
                }
            } else if source.provenance.source_provider_id == "assistant-conversation-snapshot" {
                match super::super::projection::load_conversation_snapshot_on(
                    db,
                    &output.source_set.actor_id,
                    &output.source_set.consumer_conversation_id,
                    &output.source_set.source_device_ref,
                    source,
                )
                .await
                {
                    Ok(read) => {
                        for source in &read.proof.usage.roots {
                            roots.insert(
                                (source.source_id.clone(), source.source_revision),
                                source.clone(),
                            );
                        }
                    }
                    Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
                    Err(_) => {}
                }
            }
        }
        Ok(roots.into_values().collect())
    })
}
