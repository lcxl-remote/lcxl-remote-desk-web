//! Bounded keyword ranking over sources already validated by the host.
//! Candidate statistics are explicitly approximate, never tenant-wide corpus DF.

use super::retrieval::{bm25_term, diverse_candidates, fuse_rankings, query_groups};
use std::collections::{BTreeMap, BTreeSet};

pub const CANDIDATE_STATISTICS_REASON: &str = "candidate_statistics";

/// Thin identity and postings only. No source body or recursive proof is retained
/// for every scored document. Ranking cannot grant access or authorize a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadableSearchDocument {
    pub document_id: String,
    pub source_id: String,
    pub revision: u64,
    pub root_conversation_id: String,
    pub token_count: u32,
    pub frequencies: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedSearchDocument {
    pub document: ReadableSearchDocument,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedSearchCandidates {
    pub documents: Vec<RankedSearchDocument>,
    pub effective_queries: Vec<Vec<String>>,
    pub statistics_document_count: usize,
}

#[derive(Clone)]
struct RankingItem {
    document_id: String,
    root_conversation_id: String,
    score: f64,
}

pub fn rank_readable_documents(
    queries: &[String],
    documents: Vec<ReadableSearchDocument>,
    diversify_roots: bool,
    limits: &desk_agent_protocol::ai_assistant::limits::AssistantRetrievalLimits,
) -> Result<RankedSearchCandidates, &'static str> {
    if documents.len()
        > desk_agent_protocol::ai_assistant::user_instructions::MAX_RETRIEVAL_DOCUMENT_VISITS
            as usize
    {
        return Err("readable search corpus exceeds its work limit");
    }
    let mut groups = query_groups(queries)?;
    groups.sort();
    if groups.is_empty() {
        return Err("keyword query has no searchable terms");
    }
    let requested = groups.iter().flatten().cloned().collect::<BTreeSet<_>>();
    let mut by_id = BTreeMap::new();
    let mut versions = BTreeSet::new();
    let mut frequencies = BTreeMap::<String, u32>::new();
    let mut total_tokens = 0_u64;
    for doc in documents {
        for id in [&doc.document_id, &doc.source_id, &doc.root_conversation_id] {
            desk_agent_protocol::ai_assistant::source::validate_source_id(id)?;
        }
        if doc.revision == 0
            || doc.token_count == 0
            || doc.frequencies.is_empty()
            || !versions.insert((doc.source_id.clone(), doc.revision))
        {
            return Err("invalid or duplicate readable search version");
        }
        let mut selected_tokens = 0_u64;
        for (term, frequency) in &doc.frequencies {
            if !requested.contains(term) || *frequency == 0 {
                return Err("readable posting is outside the keyword query");
            }
            selected_tokens += u64::from(*frequency);
            *frequencies.entry(term.clone()).or_default() += 1;
        }
        if selected_tokens > u64::from(doc.token_count) {
            return Err("readable posting exceeds its source length");
        }
        total_tokens = total_tokens
            .checked_add(u64::from(doc.token_count))
            .ok_or("search length overflow")?;
        if by_id.insert(doc.document_id.clone(), doc).is_some() {
            return Err("duplicate search document identity");
        }
    }
    let statistics_document_count = by_id.len();
    let count = statistics_document_count as f64;
    let average = total_tokens as f64 / count.max(1.0);
    let mut rankings = Vec::with_capacity(groups.len());
    for terms in &groups {
        let mut scored = by_id
            .values()
            .filter_map(|doc| {
                let score = terms
                    .iter()
                    .filter_map(|term| {
                        doc.frequencies.get(term).map(|tf| {
                            bm25_term(
                                f64::from(*tf),
                                f64::from(doc.token_count),
                                average,
                                count,
                                f64::from(*frequencies.get(term).unwrap_or(&0)),
                            )
                        })
                    })
                    .sum::<f64>();
                (score > 0.0).then(|| RankingItem {
                    document_id: doc.document_id.clone(),
                    root_conversation_id: doc.root_conversation_id.clone(),
                    score,
                })
            })
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.document_id.cmp(&b.document_id))
        });
        let scored = if diversify_roots {
            diverse_candidates(
                &scored,
                |item| item.root_conversation_id.as_str(),
                limits.relevance_roots as usize,
                limits.fragments_per_root as usize,
                limits.query_candidates as usize,
            )
        } else {
            scored
                .into_iter()
                .take(limits.query_candidates as usize)
                .collect()
        };
        rankings.push(scored.into_iter().map(|item| item.document_id).collect());
    }
    let fused = fuse_rankings(&rankings)
        .into_iter()
        .map(|(id, score)| RankingItem {
            root_conversation_id: by_id[&id].root_conversation_id.clone(),
            document_id: id,
            score,
        })
        .collect::<Vec<_>>();
    let fused = if diversify_roots {
        diverse_candidates(
            &fused,
            |item| item.root_conversation_id.as_str(),
            limits.relevance_roots as usize,
            limits.fragments_per_root as usize,
            limits.query_candidates as usize,
        )
    } else {
        fused
            .into_iter()
            .take(limits.query_candidates as usize)
            .collect()
    };
    let documents = fused
        .into_iter()
        .map(|item| {
            Ok(RankedSearchDocument {
                document: by_id
                    .remove(&item.document_id)
                    .ok_or("ranked document is unavailable")?,
                score: item.score,
            })
        })
        .collect::<Result<Vec<_>, &'static str>>()?;
    Ok(RankedSearchCandidates {
        documents,
        effective_queries: groups,
        statistics_document_count,
    })
}

#[cfg(test)]
mod tests {
    fn rank_readable_documents(
        queries: &[String],
        documents: Vec<super::ReadableSearchDocument>,
        diversify: bool,
    ) -> Result<super::RankedSearchCandidates, &'static str> {
        super::rank_readable_documents(queries, documents, diversify, &Default::default())
    }
    use super::*;
    fn doc(id: usize, root: &str, tf: u32) -> ReadableSearchDocument {
        ReadableSearchDocument {
            document_id: format!("document-{id:04}"),
            source_id: format!("source-{id:04}"),
            revision: 1,
            root_conversation_id: root.into(),
            token_count: 100,
            frequencies: BTreeMap::from([("report".into(), tf)]),
        }
    }
    #[test]
    fn long_conversation_cannot_fill_global_candidates_before_root_representatives() {
        let mut docs = (0..100)
            .map(|id| doc(id, "long-root", 20))
            .collect::<Vec<_>>();
        docs.extend((100..125).map(|id| doc(id, &format!("root-{id}"), 1)));
        let result = rank_readable_documents(&["report".into()], docs, true).unwrap();
        let roots = result
            .documents
            .iter()
            .map(|doc| &doc.document.root_conversation_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            roots.len(),
            (desk_agent_protocol::ai_assistant::limits::AssistantRetrievalLimits::default()
                .relevance_roots as usize)
        );
        assert!(
            result.documents.len()
                <= (desk_agent_protocol::ai_assistant::limits::AssistantRetrievalLimits::default()
                    .query_candidates as usize)
        );
        assert!(
            result
                .documents
                .iter()
                .filter(|doc| doc.document.root_conversation_id == "long-root")
                .count()
                <= (desk_agent_protocol::ai_assistant::limits::AssistantRetrievalLimits::default()
                    .fragments_per_root as usize)
        );
        assert_eq!(
            result.documents
                [..(desk_agent_protocol::ai_assistant::limits::AssistantRetrievalLimits::default()
                    .relevance_roots as usize)]
                .iter()
                .map(|doc| &doc.document.root_conversation_id)
                .collect::<BTreeSet<_>>()
                .len(),
            (desk_agent_protocol::ai_assistant::limits::AssistantRetrievalLimits::default()
                .relevance_roots as usize)
        );
    }
    #[test]
    fn query_order_repetitions_and_input_order_do_not_change_votes_or_ties() {
        let docs = (0..12)
            .map(|id| {
                let mut value = doc(id, &format!("root-{id}"), 2);
                value.frequencies.insert("email".into(), 1);
                value
            })
            .collect::<Vec<_>>();
        let first = rank_readable_documents(&["report email".into()], docs.clone(), true).unwrap();
        let mut reversed = docs;
        reversed.reverse();
        let repeated = rank_readable_documents(
            &["email report".into(), "report email".into()],
            reversed,
            true,
        )
        .unwrap();
        assert_eq!(first, repeated);
    }
    #[test]
    fn explicit_conversation_queries_can_read_deeper_without_global_root_caps() {
        let docs = (0..80)
            .map(|id| doc(id, "selected-root", 2))
            .collect::<Vec<_>>();
        let result = rank_readable_documents(&["report".into()], docs, false).unwrap();
        assert_eq!(
            result.documents.len(),
            (desk_agent_protocol::ai_assistant::limits::AssistantRetrievalLimits::default()
                .query_candidates as usize)
        );
    }
    #[test]
    fn bad_versions_and_postings_cannot_add_votes() {
        let original = doc(1, "root", 1);
        let mut copy = original.clone();
        copy.document_id = "duplicate-version".into();
        assert!(
            rank_readable_documents(&["report".into()], vec![original.clone(), copy], true)
                .is_err()
        );
        let mut changed = original.clone();
        changed.frequencies.insert("unknown".into(), 1);
        assert!(rank_readable_documents(&["report".into()], vec![changed], true).is_err());
        let mut changed = original;
        changed.frequencies.insert("report".into(), 101);
        assert!(rank_readable_documents(&["report".into()], vec![changed], true).is_err());
        assert!(rank_readable_documents(&["!!!".into()], Vec::new(), true).is_err());
    }

    #[test]
    fn configured_candidate_count_root_coverage_and_per_root_fragments_are_used() {
        let limits = desk_agent_protocol::ai_assistant::limits::AssistantRetrievalLimits {
            query_candidates: 100,
            relevance_roots: 25,
            fragments_per_root: 4,
            ..Default::default()
        };
        let docs = (0..30)
            .flat_map(|root| {
                (0..5).map(move |fragment| doc(root * 5 + fragment, &format!("root-{root}"), 10))
            })
            .collect();
        let result =
            super::rank_readable_documents(&["report".into()], docs, true, &limits).unwrap();
        assert_eq!(result.documents.len(), 100);
        let mut roots = BTreeMap::<&str, usize>::new();
        for document in &result.documents {
            *roots
                .entry(&document.document.root_conversation_id)
                .or_default() += 1;
        }
        assert_eq!(roots.len(), 25);
        assert!(roots.values().all(|count| *count == 4));
    }
}
