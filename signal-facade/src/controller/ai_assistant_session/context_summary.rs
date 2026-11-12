//! Owner view of the summary one context compaction produced.
use desk_diagnose_core::model_context::{
    ContextSummaryLookup, SummaryFactV1, context_summary_lookup,
};
use desk_diagnose_core::session::PersistedAgentSession;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use utoipa::ToSchema;

/// Selects one compaction notice of an owner session. The session id is a
/// selector, never access authority.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAssistantContextSummaryQuery {
    pub session: String,
    pub turn: String,
    pub generation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextSummaryStateDto {
    Available,
    /// A later compaction folded this summary into its own.
    Superseded,
    /// The summary is no longer retained.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextSummaryKindDto {
    Goals,
    HistoricalConstraints,
    ReportedObservations,
    CompletedActions,
    UnresolvedQuestions,
    NextSteps,
    ImportantIdentifiers,
    OmittedEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextSummaryFactDto {
    pub text: String,
    /// Original messages shown in the conversation snapshot.
    pub source_message_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextSummarySectionDto {
    pub kind: ContextSummaryKindDto,
    pub facts: Vec<ContextSummaryFactDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextSummaryContentDto {
    pub created_at: String,
    pub sections: Vec<ContextSummarySectionDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantContextSummaryDto {
    pub state: ContextSummaryStateDto,
    pub summary: Option<ContextSummaryContentDto>,
}

impl AiAssistantContextSummaryDto {
    pub fn from_session(session: &PersistedAgentSession, turn_id: &str, generation: u32) -> Self {
        let checkpoint = match context_summary_lookup(
            &session.model_context_state,
            &session.context_notices,
            turn_id,
            generation,
        ) {
            ContextSummaryLookup::Available(checkpoint) => checkpoint,
            ContextSummaryLookup::Superseded => {
                return Self::without(ContextSummaryStateDto::Superseded);
            }
            ContextSummaryLookup::Unavailable => {
                return Self::without(ContextSummaryStateDto::Unavailable);
            }
        };
        let visible: BTreeSet<&str> = session
            .conversation
            .iter()
            .filter(|message| super::visible_snapshot_message(message))
            .map(|message| message.message_id.as_str())
            .collect();
        let summary = &checkpoint.summary;
        use ContextSummaryKindDto as Kind;
        let sections = [
            (Kind::Goals, &summary.goals),
            (Kind::HistoricalConstraints, &summary.historical_constraints),
            (Kind::ReportedObservations, &summary.reported_observations),
            (Kind::CompletedActions, &summary.completed_actions),
            (Kind::UnresolvedQuestions, &summary.unresolved_questions),
            (Kind::NextSteps, &summary.next_steps),
            (Kind::ImportantIdentifiers, &summary.important_identifiers),
            (Kind::OmittedEvidence, &summary.omitted_evidence),
        ]
        .into_iter()
        .filter(|(_, facts)| !facts.is_empty())
        .map(
            |(kind, facts): (_, &Vec<SummaryFactV1>)| ContextSummarySectionDto {
                kind,
                facts: facts
                    .iter()
                    .map(|fact| ContextSummaryFactDto {
                        text: fact.text.clone(),
                        source_message_ids: fact
                            .source_message_ids
                            .iter()
                            .filter(|id| visible.contains(id.as_str()))
                            .cloned()
                            .collect(),
                    })
                    .collect(),
            },
        )
        .collect();
        Self {
            state: ContextSummaryStateDto::Available,
            summary: Some(ContextSummaryContentDto {
                created_at: checkpoint.compressor.created_at.clone(),
                sections,
            }),
        }
    }

    fn without(state: ContextSummaryStateDto) -> Self {
        Self {
            state,
            summary: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::model_context::{ContextNotice, compacted_history_for_test};

    #[test]
    fn a_compaction_notice_reads_its_summary_in_field_order_with_visible_sources() {
        let (conversation, state) = compacted_history_for_test(
            "turn-7",
            r#"{"next_steps":[{"text":"Run the report","source_message_ids":["old-b"]}],
                "goals":[{"text":"Keep the goal","source_message_ids":["old-a","old-b"]}]}"#,
        );
        let mut session = PersistedAgentSession::new(
            "conv",
            "actor",
            "",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-08-23T00:00:00Z",
        );
        session.conversation = conversation;
        session.model_context_state = state;
        session
            .context_notices
            .push(ContextNotice::compacted("turn-7", 1, 2));
        // A source the snapshot cannot show is not offered as a jump target.
        session.conversation[1].message_id = "hidden".into();
        let view = AiAssistantContextSummaryDto::from_session(&session, "turn-7", 1);
        assert_eq!(view.state, ContextSummaryStateDto::Available);
        let summary = view.summary.unwrap();
        assert_eq!(summary.created_at, "2026-08-23T00:00:00Z");
        let kinds: Vec<_> = summary
            .sections
            .iter()
            .map(|section| section.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                ContextSummaryKindDto::Goals,
                ContextSummaryKindDto::NextSteps
            ]
        );
        assert_eq!(summary.sections[0].facts[0].text, "Keep the goal");
        assert_eq!(summary.sections[0].facts[0].source_message_ids, ["old-a"]);
        assert!(summary.sections[1].facts[0].source_message_ids.is_empty());
        let wire = serde_json::to_value(AiAssistantContextSummaryDto::from_session(
            &session, "turn-7", 1,
        ))
        .unwrap();
        assert_eq!(wire["state"], "available");
        assert_eq!(wire["summary"]["sections"][0]["kind"], "goals");
        assert_eq!(
            wire["summary"]["sections"][0]["facts"][0]["sourceMessageIds"][0],
            "old-a"
        );

        session.model_context_state = Default::default();
        let gone = AiAssistantContextSummaryDto::from_session(&session, "turn-7", 1);
        assert_eq!(gone.state, ContextSummaryStateDto::Unavailable);
        assert!(gone.summary.is_none());
        session
            .context_notices
            .push(ContextNotice::compacted("turn-8", 2, 3));
        assert_eq!(
            AiAssistantContextSummaryDto::from_session(&session, "turn-7", 1).state,
            ContextSummaryStateDto::Superseded
        );
    }
}
