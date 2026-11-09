//! Display references select exact consumed read roots, never new read authority.
use super::seam::AssistantReadRoot;
use desk_agent_protocol::ai_assistant::source::AssistantCitation;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub struct AnswerCitations {
    pub citations: Vec<AssistantCitation>,
    pub incomplete: bool,
}

/// The host supplies only authenticated, actually consumed returned-content
/// roots. Decision locators and transitive policy dependencies are excluded.
pub fn for_answer(
    answer_id: &str,
    text: &str,
    roots: &[AssistantReadRoot],
    limits: &desk_agent_protocol::ai_assistant::limits::AssistantConversationLimits,
) -> AnswerCitations {
    let mut versions = BTreeMap::new();
    for root in roots {
        if desk_agent_protocol::ai_assistant::source_ref::SourceRef::parse(&root.source_id)
            .is_none()
            || root.source_revision == 0
        {
            continue;
        }
        versions
            .entry(root.source_id.as_str())
            .and_modify(|previous: &mut Option<u64>| {
                if *previous != Some(root.source_revision) {
                    *previous = None;
                }
            })
            .or_insert(Some(root.source_revision));
    }
    let (markers, incomplete) = source_markers(text, limits.citations as usize);
    let mut result = AnswerCitations {
        incomplete,
        ..Default::default()
    };
    for source in markers {
        let Some(Some(revision)) = versions.get(source.as_str()) else {
            result.incomplete = true;
            continue;
        };
        let citation = AssistantCitation {
            answer_message_id: answer_id.into(),
            answer_revision: 1,
            source_id: source,
            source_revision: *revision,
            source_device: None,
        };
        result.citations.push(citation);
        if serde_json::to_vec(&result.citations)
            .map_or(true, |value| value.len() > limits.citation_bytes as usize)
        {
            result.citations.pop();
            result.incomplete = true;
            break;
        }
    }
    result
}

/// Conservatively skip fenced/indented code, inline code and Markdown link
/// labels. The original answer text is never rewritten by this selector.
fn source_markers(text: &str, maximum: usize) -> (Vec<String>, bool) {
    let mut fence: Option<(u8, usize)> = None;
    let mut inline = 0;
    let mut seen = BTreeSet::new();
    let mut sources = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let bytes = trimmed.as_bytes();
        let marker = bytes
            .first()
            .copied()
            .filter(|byte| matches!(byte, b'`' | b'~'));
        let count = marker.map_or(0, |byte| {
            bytes.iter().take_while(|value| **value == byte).count()
        });
        if let Some((byte, minimum)) = fence {
            if marker == Some(byte) && count >= minimum && trimmed[count..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if inline == 0 && count >= 3 {
            fence = Some((marker.unwrap(), count));
            continue;
        }
        if line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }
        let mut rest = line;
        while !rest.is_empty() {
            if rest.starts_with('`') {
                let count = rest.bytes().take_while(|byte| *byte == b'`').count();
                if inline == 0 {
                    inline = count;
                } else if inline == count {
                    inline = 0;
                }
                rest = &rest[count..];
                continue;
            }
            if inline != 0 {
                rest = &rest[rest.chars().next().unwrap().len_utf8()..];
                continue;
            }
            if rest.starts_with('[') {
                let Some(end) = rest.find(']') else {
                    break;
                };
                // Includes reference labels and unknown link schemes. UI link
                // policies decide their presentation, never this proof selector.
                rest = &rest[end + 1..];
                continue;
            }
            if let Some(value) = rest.strip_prefix("〔来源:") {
                let Some(end) = value.find('〕') else {
                    break;
                };
                let id = &value[..end];
                if desk_agent_protocol::ai_assistant::source_ref::SourceRef::parse(id).is_some()
                    && seen.insert(id.to_string())
                {
                    if sources.len() == maximum {
                        return (sources, true);
                    }
                    sources.push(id.to_string());
                }
                rest = &value[end + '〕'.len_utf8()..];
                continue;
            }
            rest = &rest[rest.chars().next().unwrap().len_utf8()..];
        }
    }
    (sources, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root(seed: usize, revision: u64) -> AssistantReadRoot {
        AssistantReadRoot {
            source_id: crate::native_ids::fixture_source_id(&("citation-test", seed)),
            source_revision: revision,
        }
    }
    #[test]
    fn a_mistyped_neighbouring_id_is_dropped_instead_of_citing_another_root() {
        use desk_agent_protocol::ai_assistant::source_ref::SourceRef;
        let roots = [42, 43].map(|id| AssistantReadRoot {
            source_id: SourceRef::history(id).to_string(),
            source_revision: 1,
        });
        // 42 formats as src_h42i; copying the digit 3 for 2 keeps the check
        // letter of 42, which does not match 43.
        let result = for_answer("answer", "〔来源:src_h43i〕", &roots, &Default::default());
        assert!(result.citations.is_empty());
        let result = for_answer("answer", "〔来源:src_h42i〕", &roots, &Default::default());
        assert_eq!(result.citations[0].source_id, "src_h42i");
    }

    #[test]
    fn ordinary_markers_require_exact_consumed_roots_and_unambiguous_versions() {
        let a = root(1, 2);
        let b = root(2, 1);
        let text = format!(
            "中文🙂〔来源:{}〕 **〔来源:{}〕** 〔来源:{}〕",
            a.source_id, b.source_id, a.source_id
        );
        let result = for_answer(
            "answer",
            &text,
            std::slice::from_ref(&a),
            &Default::default(),
        );
        assert_eq!(result.citations.len(), 1);
        assert_eq!(result.citations[0].source_revision, 2);
        assert_eq!(result.citations[0].answer_message_id, "answer");
        assert!(result.incomplete);
        assert!(
            for_answer("answer", &text, &[a, root(1, 3)], &Default::default())
                .citations
                .is_empty()
        );
        assert!(
            for_answer("answer", &text, &[], &Default::default())
                .citations
                .is_empty()
        );
    }
    #[test]
    fn code_incomplete_ids_and_existing_markdown_links_are_never_citations() {
        let a = root(1, 1);
        let marker = format!("〔来源:{}〕", a.source_id);
        for text in [
            format!("`{marker}` ``a `{marker}```"),
            format!("```rust\n{marker}\n```"),
            format!("~~~\n{marker}\n~~~"),
            format!("    {marker}"),
            format!("[{marker}](https://example.com)"),
            format!("[{marker}][ref]"),
            "〔来源:src_fake〕 〔来源:src_incomplete".into(),
        ] {
            assert!(
                for_answer(
                    "answer",
                    &text,
                    std::slice::from_ref(&a),
                    &Default::default()
                )
                .citations
                .is_empty(),
                "{text}"
            );
        }
        assert_eq!(
            for_answer(
                "answer",
                &format!("```\n{marker}\n```\n{marker}"),
                &[a],
                &Default::default()
            )
            .citations
            .len(),
            1
        );
    }
    #[test]
    fn count_and_serialized_metadata_are_bounded_without_changing_answer_text() {
        let roots = (0..100).map(|seed| root(seed, 1)).collect::<Vec<_>>();
        let text = roots
            .iter()
            .map(|root| format!("〔来源:{}〕", root.source_id))
            .collect::<String>();
        let result = for_answer("answer", &text, &roots, &Default::default());
        assert!(result.incomplete);
        assert!(
            result.citations.len()
                <= desk_agent_protocol::ai_assistant::limits::AssistantConversationLimits::default()
                    .citations as usize
        );
        assert!(
            serde_json::to_vec(&result.citations).unwrap().len()
                <= desk_agent_protocol::ai_assistant::limits::AssistantConversationLimits::default()
                    .citation_bytes as usize
        );
        assert_eq!(text.matches("〔来源:").count(), 100);
    }
}

#[cfg(test)]
#[test]
fn configured_citation_capacity_accepts_large_answer_projections() {
    let roots = (0..80)
        .map(|index| AssistantReadRoot {
            source_id: crate::native_ids::fixture_source_id(&index),
            source_revision: 1,
        })
        .collect::<Vec<_>>();
    let text = roots
        .iter()
        .map(|root| format!("〔来源:{}〕", root.source_id))
        .collect::<String>();
    let mut limits = desk_agent_protocol::ai_assistant::limits::AssistantConversationLimits {
        citations: 80,
        citation_bytes: 131072,
        ..Default::default()
    };
    let projection = for_answer("answer", &text, &roots, &limits);
    assert_eq!(projection.citations.len(), 80);
    assert!(!projection.incomplete);
    limits.citations = 3;
    let smaller = for_answer("answer", &text, &roots, &limits);
    assert_eq!(smaller.citations.len(), 3);
    assert!(smaller.incomplete);
}
