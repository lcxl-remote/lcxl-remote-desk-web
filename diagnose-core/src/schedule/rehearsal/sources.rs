//! Assemble source paths only from independently verified rehearsal evidence.
use super::{fixed_input::fixed_task_input_source, read_sources::RehearsalToolSource};
use crate::{
    chat::ChatMessage,
    model_egress::{
        ModelInputLineage, is_audited_public_system_prompt, model_export_envelope_id,
        model_output_message_envelope,
    },
    schedule::{
        contract::ValidatedTaskContract,
        source_graph::{
            ResolvedTaskSources, TaskSourceAuthority, TaskSourceBinding, TaskSourceError,
            resolve_task_sources,
        },
    },
    session::PersistedAgentSession,
};
use std::collections::BTreeSet;

/// Supplied by the store after checking the original successful model receipt.
/// This type is deliberately not a deserializable client permission claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RehearsalModelCall {
    pub output_message_id: String,
    pub export_authorization_id: String,
    pub inputs: Vec<ModelInputLineage>,
}

/// Each trace must have a verified original successful model receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RehearsalCompressionCall {
    pub trace: crate::model_context::ContextSummaryDerivationV1,
    pub inputs: Vec<ModelInputLineage>,
}

/// Historical shape validation only; the server still verifies every model receipt.
pub fn retained_rehearsal_compressions(
    session: &PersistedAgentSession,
) -> Result<Vec<crate::model_context::ContextSummaryDerivationV1>, TaskSourceError> {
    use sha2::{Digest, Sha256};
    let mut traces: Vec<crate::model_context::ContextSummaryDerivationV1> = Vec::new();
    for entry in &session.model_context_state.entries {
        let Some(checkpoint) = &entry.checkpoint else {
            continue;
        };
        let checkpoint = checkpoint.v1();
        let lineage = checkpoint
            .lineage
            .as_ref()
            .ok_or(TaskSourceError::MissingSource)?;
        crate::model_context::validate_summary_derivations(lineage, &session.conversation)
            .map_err(|_| TaskSourceError::InvalidNode)?;
        let encoded =
            serde_json::to_vec(&checkpoint.summary).map_err(|_| TaskSourceError::InvalidNode)?;
        if lineage.envelope.digest_sha256 != format!("{:x}", Sha256::digest(&encoded))
            || lineage.derivations.last().is_none_or(|trace| {
                trace.generation != checkpoint.generation
                    || trace.compressor != checkpoint.compressor
            })
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        for trace in &lineage.derivations {
            if let Some(previous) = traces.iter().find(|previous| {
                previous.model_output.envelope_id == trace.model_output.envelope_id
            }) {
                if previous != trace {
                    return Err(TaskSourceError::ConflictingNode);
                }
            } else {
                traces.push(trace.clone());
            }
        }
    }
    Ok(traces)
}

pub(super) fn source_node(
    label: &desk_agent_protocol::data_lineage::DataEnvelope,
) -> ModelInputLineage {
    ModelInputLineage {
        envelope_id: label.envelope_id.clone(),
        digest_sha256: label.digest_sha256.clone(),
        source_provider_id: label.provenance.source_provider_id.clone(),
        source_tool_name: label.provenance.source_tool_name.clone(),
        source_envelope_ids: label.provenance.source_envelope_ids.clone(),
        public_system_prompt: false,
    }
}

fn unique_message<'a>(
    session: &'a PersistedAgentSession,
    id: &str,
) -> Result<&'a ChatMessage, TaskSourceError> {
    let mut matches = session
        .conversation
        .iter()
        .filter(|message| message.message_id == id);
    let message = matches.next().ok_or(TaskSourceError::MissingSource)?;
    if matches.next().is_some() {
        return Err(TaskSourceError::ConflictingNode);
    }
    Ok(message)
}

/// The store must authenticate/freeze the same session and verify each supplied
/// read/model receipt. Unknown transforms are rejected rather than treating
/// their declared parents as a substitute for the original operation's scope.
/// An assembled graph of store-authenticated evidence; resolving a target still
/// checks graph completeness and rejects conflicting nodes or missing parents.
pub struct RehearsalSourceGraph {
    pub nodes: Vec<ModelInputLineage>,
    pub bindings: Vec<TaskSourceBinding>,
    verified_model_messages: BTreeSet<String>,
}

impl RehearsalSourceGraph {
    pub fn resolve_model_output(
        &self,
        session: &PersistedAgentSession,
        message_id: &str,
    ) -> Result<ResolvedTaskSources, TaskSourceError> {
        if !self.verified_model_messages.contains(message_id) {
            return Err(TaskSourceError::MissingSource);
        }
        let output = model_output_message_envelope(unique_message(session, message_id)?)
            .map_err(|_| TaskSourceError::InvalidNode)?;
        resolve_task_sources(
            &output.envelope_id,
            &output.digest_sha256,
            &self.nodes,
            &self.bindings,
        )
    }
}

pub fn resolve_rehearsal_model_sources(
    session: &PersistedAgentSession,
    original_input_id: &str,
    contract: &ValidatedTaskContract,
    reads: &[RehearsalToolSource],
    calls: &[RehearsalModelCall],
    compressions: &[RehearsalCompressionCall],
    output_message_id: &str,
) -> Result<ResolvedTaskSources, TaskSourceError> {
    build_rehearsal_source_graph(
        session,
        original_input_id,
        contract,
        reads,
        calls,
        compressions,
    )?
    .resolve_model_output(session, output_message_id)
}

pub fn build_rehearsal_source_graph(
    session: &PersistedAgentSession,
    original_input_id: &str,
    contract: &ValidatedTaskContract,
    reads: &[RehearsalToolSource],
    calls: &[RehearsalModelCall],
    compressions: &[RehearsalCompressionCall],
) -> Result<RehearsalSourceGraph, TaskSourceError> {
    let (fixed, fixed_binding) = fixed_task_input_source(session, original_input_id, contract)
        .map_err(|_| TaskSourceError::InvalidRoot)?;
    let mut known = vec![fixed];
    let mut bindings = vec![fixed_binding];
    for read in reads {
        known.push(read.lineage.clone());
        bindings.push(read.authority.clone());
    }
    let mut seen = BTreeSet::new();
    for call in calls {
        if !seen.insert(call.output_message_id.as_str())
            || call.export_authorization_id.trim().is_empty()
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        let message = unique_message(session, &call.output_message_id)?;
        let output =
            model_output_message_envelope(message).map_err(|_| TaskSourceError::InvalidNode)?;
        if output.provenance.source_provider_id != "external-model"
            || output.provenance.source_tool_name != "model-response"
            || output
                .provenance
                .source_envelope_ids
                .iter()
                .collect::<BTreeSet<_>>()
                != call
                    .inputs
                    .iter()
                    .map(|input| &input.envelope_id)
                    .collect::<BTreeSet<_>>()
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        known.push(ModelInputLineage {
            envelope_id: output.envelope_id.clone(),
            digest_sha256: output.digest_sha256.clone(),
            source_provider_id: output.provenance.source_provider_id.clone(),
            source_tool_name: output.provenance.source_tool_name.clone(),
            source_envelope_ids: output.provenance.source_envelope_ids.clone(),
            public_system_prompt: false,
        });
    }
    known.extend(directory_control_nodes(session)?.0);
    known.extend(super::permission_controls::collect(session)?.nodes);
    let original = unique_message(session, original_input_id)?;
    for bridge in session
        .conversation
        .iter()
        .filter(|message| crate::permission_resume::is_permission_resume_message(message))
    {
        let label = crate::permission_resume::verified_permission_resume_envelope(original, bridge)
            .ok_or(TaskSourceError::ConflictingNode)?;
        known.push(source_node(label));
    }
    let retained = retained_rehearsal_compressions(session)?;
    if retained.len() != compressions.len() {
        return Err(TaskSourceError::MissingSource);
    }
    let mut compression_seen = BTreeSet::new();
    for call in compressions {
        if !compression_seen.insert(&call.trace.model_output.envelope_id)
            || !retained.contains(&call.trace)
            || call
                .trace
                .model_output
                .provenance
                .source_envelope_ids
                .iter()
                .collect::<BTreeSet<_>>()
                != call
                    .inputs
                    .iter()
                    .map(|input| &input.envelope_id)
                    .collect::<BTreeSet<_>>()
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        known.extend([
            source_node(&call.trace.model_output),
            source_node(&call.trace.compression_input),
            source_node(&call.trace.summary),
        ]);
    }
    // Packed compression parents are inputs to a verified transform, not roots.
    // They still need original read/model evidence or a verified export edge.
    let parent_batches: Vec<Vec<ModelInputLineage>> = compressions
        .iter()
        .map(|call| {
            call.trace
                .compression_sources
                .iter()
                .map(source_node)
                .collect()
        })
        .collect();
    let batches = calls
        .iter()
        .map(|call| {
            (
                call.export_authorization_id.as_str(),
                call.inputs.as_slice(),
            )
        })
        .chain(compressions.iter().map(|call| {
            (
                call.trace.export_authorization_id.as_str(),
                call.inputs.as_slice(),
            )
        }))
        .chain(
            compressions
                .iter()
                .zip(&parent_batches)
                .map(|(call, parents)| {
                    (
                        call.trace.export_authorization_id.as_str(),
                        parents.as_slice(),
                    )
                }),
        );
    let mut nodes = known.clone();
    for (export_id, inputs) in batches {
        for input in inputs {
            if known.contains(input) {
                continue;
            }
            if is_audited_public_system_prompt(input) {
                nodes.push(input.clone());
                bindings.push(TaskSourceBinding {
                    envelope_id: input.envelope_id.clone(),
                    digest_sha256: input.digest_sha256.clone(),
                    authority: TaskSourceAuthority::SystemPrompt,
                });
                continue;
            }
            let mut matched = false;
            for source in &known {
                if input.public_system_prompt
                    || input.digest_sha256 != source.digest_sha256
                    || input.source_provider_id != source.source_provider_id
                    || input.source_tool_name != source.source_tool_name
                    || input.source_envelope_ids != [source.envelope_id.clone()]
                {
                    continue;
                }
                for message in &session.conversation {
                    if message
                        .data_envelope
                        .as_ref()
                        .is_some_and(|label| label.envelope_id == source.envelope_id)
                        && input.envelope_id
                            == model_export_envelope_id(
                                export_id,
                                &source.envelope_id,
                                &message.message_id,
                            )
                    {
                        if matched {
                            return Err(TaskSourceError::ConflictingNode);
                        }
                        matched = true;
                    }
                }
            }
            if !matched {
                return Err(TaskSourceError::MissingSource);
            }
            nodes.push(input.clone());
        }
    }
    Ok(RehearsalSourceGraph {
        nodes,
        bindings,
        verified_model_messages: calls
            .iter()
            .map(|call| call.output_message_id.clone())
            .collect(),
    })
}

impl RehearsalSourceGraph {
    /// Resolve a specific authenticated artifact receipt using the same verified
    /// model, read and compression evidence as the generated message proposal.
    pub fn resolve_attachment(
        &self,
        run_id: &str,
        attachment: &desk_agent_protocol::communication::ImmutableAttachmentSnapshot,
        receipt: &crate::schedule::source_graph::attachment::TaskAttachmentReceipt<'_>,
    ) -> Result<ResolvedTaskSources, TaskSourceError> {
        crate::schedule::source_graph::attachment::resolve_task_attachment_sources(
            run_id,
            attachment,
            receipt,
            &self.nodes,
            &self.bindings,
        )
    }
}

#[cfg(test)]
mod attachment_graph_tests {
    use super::*;
    use crate::schedule::source_graph::attachment::TaskAttachmentReceipt;
    use desk_agent_protocol::{
        communication::ImmutableAttachmentSnapshot, data_lineage::ContentRef,
    };

    #[test]
    fn artifact_graph_uses_receipt_digest_and_keeps_upstream_scope() {
        let artifact = ImmutableAttachmentSnapshot {
            content: ContentRef::Artifact {
                artifact_id: "file".into(),
                sha256: "a".repeat(64),
                size_bytes: 1,
                media_type: "text/plain".into(),
            },
            file_name: "report.txt".into(),
            media_type: "text/plain".into(),
            size_bytes: 1,
            digest_sha256: "a".repeat(64),
        };
        let receipt_digest = "b".repeat(64);
        let receipt = TaskAttachmentReceipt {
            run_id: "run",
            envelope_id: "receipt",
            receipt_digest_sha256: &receipt_digest,
            attachment: &artifact,
        };
        let node = |id: &str, parents: Vec<String>| ModelInputLineage {
            public_system_prompt: false,
            envelope_id: id.into(),
            digest_sha256: receipt_digest.clone(),
            source_provider_id: "device".into(),
            source_tool_name: "read".into(),
            source_envelope_ids: parents,
        };
        let graph = RehearsalSourceGraph {
            nodes: vec![
                node("receipt", vec!["original".into()]),
                node("original", vec![]),
            ],
            bindings: vec![TaskSourceBinding {
                envelope_id: "original".into(),
                digest_sha256: receipt_digest.clone(),
                authority: TaskSourceAuthority::Scopes(vec!["file:original".into()]),
            }],
            verified_model_messages: BTreeSet::new(),
        };
        assert_eq!(
            graph
                .resolve_attachment("run", &artifact, &receipt)
                .unwrap()
                .scopes,
            vec!["file:original"]
        );
        assert_eq!(
            graph.resolve_attachment("other", &artifact, &receipt),
            Err(TaskSourceError::InvalidRoot)
        );
        let wrong_digest = "c".repeat(64);
        let wrong_receipt = TaskAttachmentReceipt {
            run_id: "run",
            envelope_id: "receipt",
            receipt_digest_sha256: &wrong_digest,
            attachment: &artifact,
        };
        assert_eq!(
            graph.resolve_attachment("run", &artifact, &wrong_receipt),
            Err(TaskSourceError::ConflictingNode)
        );
        let incomplete = RehearsalSourceGraph {
            nodes: graph.nodes[..1].to_vec(),
            bindings: vec![],
            verified_model_messages: BTreeSet::new(),
        };
        assert_eq!(
            incomplete.resolve_attachment("run", &artifact, &receipt),
            Err(TaskSourceError::MissingSource)
        );
    }
}

/// Directory results are deterministic control messages. They inherit the model
/// proposal's parents and introduce no permission or independent data root.
type ControlLineage = (Vec<ModelInputLineage>, Vec<(String, String)>);

fn directory_control_nodes(
    session: &PersistedAgentSession,
) -> Result<ControlLineage, TaskSourceError> {
    use crate::{
        chat::ChatRole,
        file_scope::{DirectoryConsentSource, DirectoryConsentState},
    };
    let mut nodes = Vec::new();
    let mut requests = Vec::new();
    let mut pauses = Vec::new();
    for message in &session.conversation {
        if message.role != ChatRole::Tool {
            continue;
        }
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let proposals: Vec<_> = session
            .conversation
            .iter()
            .filter(|parent| parent.role == ChatRole::Assistant)
            .flat_map(|parent| {
                parent
                    .tool_calls
                    .iter()
                    .filter(move |call| {
                        call.id == call_id && call.name == crate::directory_tools::REQUEST_DIRECTORY
                    })
                    .map(move |call| (parent, call))
            })
            .collect();
        if proposals.is_empty() {
            continue;
        }
        if proposals.len() != 1 {
            return Err(TaskSourceError::ConflictingNode);
        }
        let (parent, call) = proposals[0];
        let input = crate::directory_tools::parse(&crate::chat::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        })
        .map_err(|_| TaskSourceError::InvalidNode)?;
        let mut matched = false;
        for record in session.file_scope.records() {
            if record.proposal.requested_path != input.path
                || record.proposal.purpose != input.purpose
            {
                continue;
            }
            let pending =
                crate::directory_tools::pending_result(&record.proposal.request_id).to_string();
            let approved =
                crate::directory_tools::task_approved_result(&record.proposal.request_id)
                    .to_string();
            let kind = if message.text == pending {
                "directory_proposal_pending"
            } else if message.text == approved
                && record.proposal.source == DirectoryConsentSource::TaskContract
                && record.state == DirectoryConsentState::Approved
            {
                "task_directory_resolved"
            } else {
                continue;
            };
            let expected = crate::model_message_labels::internal_tool_result_envelope(
                parent.data_envelope.as_ref(),
                call_id,
                &message.text,
                kind,
            )
            .map_err(|_| TaskSourceError::InvalidNode)?
            .ok_or(TaskSourceError::MissingSource)?;
            if message.data_envelope.as_ref() != Some(&expected)
                || message.image_data_url.is_some()
                || !message.tool_calls.is_empty()
                || message.background_task_id.is_some()
                || matched
            {
                return Err(TaskSourceError::ConflictingNode);
            }
            matched = true;
            nodes.push(source_node(&expected));
            requests.push((call_id.to_owned(), record.proposal.request_id.clone()));
            if kind == "directory_proposal_pending" {
                pauses.push((call_id.to_owned(), record.proposal.request_id.clone()));
            }
        }
        // Failed resolutions are not silently promoted to verified directory data.
        if !matched {
            return Err(TaskSourceError::MissingSource);
        }
    }
    let (paused_nodes, paused_calls) = super::paused_controls::collect(
        session,
        &pauses,
        "directory_pause_tool_call",
        &["not executed: waiting for directory confirmation"],
    )?;
    nodes.extend(paused_nodes);
    requests.extend(paused_calls);
    Ok((nodes, requests))
}

/// Classify deterministic directory control calls. Each returned request still
/// requires its immutable store receipt before a host may exclude it from actions.
pub fn directory_control_requests(
    session: &PersistedAgentSession,
) -> Result<Vec<(String, String)>, TaskSourceError> {
    directory_control_nodes(session).map(|(_, requests)| requests)
}
