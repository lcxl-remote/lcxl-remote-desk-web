//! Deterministic retrieval primitives. Hosts supply only currently authorized rows.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

pub const BM25_K1: f64 = 1.2;
pub const BM25_B: f64 = 0.75;
pub const RRF_K: f64 = 60.0;
pub const MAX_INDEX_FRAGMENT_BYTES: usize = 4 * 1024;

fn is_cjk(character: char) -> bool {
    matches!(character as u32, 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff)
}

pub fn normalize_query(value: &str) -> String {
    value
        .nfkc()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() || is_cjk(character) || "_.:-".contains(character) {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn tokenize(value: &str) -> Vec<String> {
    let normalized = normalize_query(value);
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut cjk = Vec::new();
    let flush_word = |word: &mut String, tokens: &mut Vec<String>| {
        if !word.is_empty() {
            tokens.push(std::mem::take(word));
        }
    };
    let flush_cjk = |cjk: &mut Vec<char>, tokens: &mut Vec<String>| {
        for width in 1..=3 {
            if cjk.len() >= width {
                for window in cjk.windows(width) {
                    tokens.push(window.iter().collect());
                }
            }
        }
        cjk.clear();
    };
    for character in normalized.chars() {
        if is_cjk(character) {
            flush_word(&mut word, &mut tokens);
            cjk.push(character);
        } else if character.is_whitespace() {
            flush_word(&mut word, &mut tokens);
            flush_cjk(&mut cjk, &mut tokens);
        } else {
            flush_cjk(&mut cjk, &mut tokens);
            word.push(character);
        }
    }
    flush_word(&mut word, &mut tokens);
    flush_cjk(&mut cjk, &mut tokens);
    tokens
}

pub fn query_groups(queries: &[String]) -> Result<Vec<Vec<String>>, &'static str> {
    desk_agent_protocol::ai_assistant::history::validate_queries(queries, true)?;
    let mut seen = BTreeSet::new();
    let mut groups = Vec::new();
    for query in queries {
        let mut terms = tokenize(query).into_iter().collect::<BTreeSet<_>>();
        // A multi-character CJK query already has adjacent n-grams. Its
        // component single characters add broad unrelated postings and can
        // exhaust a bounded scan before the meaningful terms are visited.
        let covered = terms
            .iter()
            .filter(|term| term.chars().count() > 1 && term.chars().all(is_cjk))
            .flat_map(|term| term.chars())
            .collect::<BTreeSet<_>>();
        terms.retain(|term| {
            !(term.chars().count() == 1
                && term
                    .chars()
                    .next()
                    .is_some_and(|character| covered.contains(&character)))
        });
        if terms.is_empty() {
            continue;
        }
        let terms = terms.into_iter().collect::<Vec<_>>();
        if seen.insert(terms.clone()) {
            groups.push(terms);
        }
    }
    Ok(groups)
}

/// Score one term using statistics from the exact readable corpus partition.
pub fn bm25_term(tf: f64, length: f64, average_length: f64, document_count: f64, df: f64) -> f64 {
    if !tf.is_finite()
        || !length.is_finite()
        || !average_length.is_finite()
        || !document_count.is_finite()
        || !df.is_finite()
        || tf <= 0.0
        || document_count <= 0.0
        || df <= 0.0
        || df > document_count
    {
        return 0.0;
    }
    let idf = ((document_count - df + 0.5) / (df + 0.5) + 1.0).ln();
    let denominator = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * length / average_length.max(1.0));
    idf * (tf * (BM25_K1 + 1.0) / denominator)
}

pub fn reciprocal_rank(rank: usize) -> f64 {
    1.0 / (RRF_K + rank as f64 + 1.0)
}

pub fn fuse_rankings(rankings: &[Vec<String>]) -> Vec<(String, f64)> {
    let mut scores = BTreeMap::<String, f64>::new();
    for ranking in rankings {
        let mut seen = BTreeSet::new();
        for (rank, id) in ranking.iter().enumerate() {
            if seen.insert(id) {
                *scores.entry(id.clone()).or_default() += reciprocal_rank(rank);
            }
        }
    }
    let mut result = scores.into_iter().collect::<Vec<_>>();
    result.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    result
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexedFragment {
    pub fragment_index: u32,
    pub byte_start: u32,
    pub byte_end: u32,
    pub token_count: u32,
    pub terms: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexProjection {
    pub fragments: Vec<IndexedFragment>,
    pub indexed_bytes: u32,
    pub partial: bool,
}

pub fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub fn index_source(value: &str) -> Result<IndexProjection, &'static str> {
    let indexed_bytes = u32::try_from(value.len()).map_err(|_| "index byte position overflow")?;
    let mut projection = IndexProjection {
        fragments: Vec::new(),
        indexed_bytes,
        partial: false,
    };
    let mut start = 0;
    let word_character = |c: char| !is_cjk(c) && (c.is_alphanumeric() || "_.:-".contains(c));
    while start < value.len() {
        let hard_end = start + utf8_prefix(&value[start..], MAX_INDEX_FRAGMENT_BYTES).len();
        let mut end = hard_end;
        if end < value.len()
            && value[..end].chars().next_back().is_some_and(word_character)
            && value[end..].chars().next().is_some_and(word_character)
        {
            if let Some((offset, character)) = value[start..end]
                .char_indices()
                .rev()
                .find(|(_, c)| !word_character(*c))
            {
                end = start + offset + character.len_utf8();
            } else {
                // A single oversized token cannot fit one fixed-size fragment.
                // Preserve the complete text and report the lexical gap.
                projection.partial = true;
            }
        }
        let fragment = &value[start..end];
        let mut terms = BTreeMap::<String, u32>::new();
        let mut token_count = 0_u32;
        for term in tokenize(fragment) {
            token_count = token_count.saturating_add(1);
            let frequency = terms.entry(term).or_default();
            *frequency = frequency.saturating_add(1);
        }
        projection.fragments.push(IndexedFragment {
            fragment_index: projection.fragments.len() as u32,
            byte_start: start as u32,
            byte_end: end as u32,
            token_count,
            terms,
        });
        let overlap = if end < value.len() && value[end..].chars().next().is_some_and(is_cjk) {
            value[start..end]
                .chars()
                .rev()
                .take(2)
                .take_while(|c| is_cjk(*c))
                .map(char::len_utf8)
                .sum::<usize>()
        } else {
            0
        };
        start = if end - overlap > start {
            end - overlap
        } else {
            end
        };
    }
    Ok(projection)
}

/// Allocate representatives before any global candidate truncation.
pub fn diverse_candidates<T: Clone>(
    ranked: &[T],
    root: impl Fn(&T) -> &str,
    max_roots: usize,
    per_root: usize,
    max_items: usize,
) -> Vec<T> {
    let mut roots = BTreeSet::new();
    let mut selected = Vec::new();
    let mut extras = Vec::new();
    let mut counts = BTreeMap::<String, usize>::new();
    for item in ranked {
        let key = root(item);
        if !roots.contains(key) {
            if roots.len() >= max_roots || selected.len() >= max_items {
                continue;
            }
            roots.insert(key.to_string());
            counts.insert(key.to_string(), 1);
            selected.push(item.clone());
        } else if counts.get(key).copied().unwrap_or_default() < per_root {
            *counts.entry(key.to_string()).or_default() += 1;
            extras.push(item.clone());
        }
    }
    selected.extend(
        extras
            .into_iter()
            .take(max_items.saturating_sub(selected.len())),
    );
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_keywords_preserve_cjk_singletons_and_normalize_width() {
        let terms = tokenize("发邮件 ＡＴＬＡＳ /Workspace/Atlas");
        for term in ["发", "邮件", "发邮件", "atlas", "workspace"] {
            assert!(terms.contains(&term.to_string()), "{term}");
        }
        assert_eq!(
            query_groups(&["发送 报告".into(), "报告 发送".into()])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn indexes_preserve_full_unicode_sources_beyond_old_capacity() {
        let text = "邮件内容🙂".repeat(4000);
        let projection = index_source(&text).unwrap();
        assert!(!projection.partial);
        assert_eq!(projection.indexed_bytes as usize, text.len());
        assert!(projection.fragments.len() > 8);
        for part in projection.fragments {
            assert!(text.is_char_boundary(part.byte_start as usize));
            assert!(text.is_char_boundary(part.byte_end as usize));
            assert!(part.byte_end - part.byte_start <= MAX_INDEX_FRAGMENT_BYTES as u32);
        }
    }

    #[test]
    fn long_sources_keep_tail_terms_and_all_normalized_vocabulary() {
        let text = format!("{} tail_delivery_confirmed", "background ".repeat(30000));
        let index = index_source(&text).unwrap();
        assert!(text.len() > 256 * 1024);
        assert!(!index.partial);
        assert_eq!(index.indexed_bytes as usize, text.len());
        assert!(index.fragments.len() > 8);
        assert!(
            index
                .fragments
                .last()
                .unwrap()
                .terms
                .contains_key("tail_delivery_confirmed")
        );

        let diverse = (0x4e00..0x4e00 + 9000)
            .map(|value| char::from_u32(value).unwrap())
            .collect::<String>();
        let index = index_source(&diverse).unwrap();
        assert!(!index.partial);
        assert!(
            index
                .fragments
                .iter()
                .flat_map(|part| part.terms.keys())
                .collect::<BTreeSet<_>>()
                .len()
                > 16384
        );
        let last = diverse
            .chars()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        assert!(index.fragments.last().unwrap().terms.contains_key(&last));

        let expanded = (0x4e00..0x4e00 + 682)
            .map(|value| format!("㍿{}", char::from_u32(value).unwrap()))
            .collect::<String>();
        let index = index_source(&expanded).unwrap();
        assert!(!index.partial);
        assert_eq!(index.fragments.len(), 1);
        assert!(index.fragments[0].terms.len() > 4096);
    }

    #[test]
    fn fragment_boundaries_preserve_english_words_and_cjk_ngrams() {
        let english = format!("{}quarterlyreport remainder", "a ".repeat(2045));
        let english_index = index_source(&english).unwrap();
        assert!(
            english_index
                .fragments
                .iter()
                .any(|fragment| fragment.terms.contains_key("quarterlyreport"))
        );
        assert!(!english_index.partial);
        let chinese = format!("{}甲乙丙后续", "a ".repeat(2046));
        let chinese_index = index_source(&chinese).unwrap();
        assert!(
            chinese_index
                .fragments
                .iter()
                .any(|fragment| fragment.terms.contains_key("甲乙丙"))
        );
        assert!(!chinese_index.partial);
        assert!(
            chinese_index
                .fragments
                .windows(2)
                .any(|parts| parts[1].byte_start < parts[0].byte_end)
        );
    }

    #[test]
    fn long_conversation_does_not_displace_other_roots() {
        let mut candidates = (0..40).map(|n| ("long".to_string(), n)).collect::<Vec<_>>();
        candidates.extend((0..19).map(|n| (format!("other-{n}"), 0)));
        let result = diverse_candidates(&candidates, |v| &v.0, 20, 3, 40);
        assert_eq!(
            result[..20]
                .iter()
                .map(|v| &v.0)
                .collect::<BTreeSet<_>>()
                .len(),
            20
        );
        assert_eq!(result.iter().filter(|v| v.0 == "long").count(), 3);
    }

    #[test]
    fn fusion_has_stable_ties_and_no_duplicate_vote_within_query() {
        let rankings = vec![vec!["b".into(), "b".into(), "a".into()], vec!["a".into()]];
        let result = fuse_rankings(&rankings);
        assert_eq!(result[0].0, "a");
        assert_eq!(
            result.iter().find(|v| v.0 == "b").unwrap().1,
            reciprocal_rank(0)
        );
        assert_eq!(bm25_term(1.0, 1.0, 1.0, 1.0, 2.0), 0.0);
    }
}

#[cfg(test)]
mod bounded_cjk_queries {
    use super::*;
    #[test]
    fn adjacent_terms_keep_names_and_single_character_queries_without_common_character_flooding() {
        let groups = query_groups(&["季度会议 陈晨".into(), "王 Atlas".into()]).unwrap();
        assert!(groups[0].contains(&"会议".into()) && groups[0].contains(&"陈晨".into()));
        assert!(!groups[0].contains(&"会".into()));
        assert!(groups[1].contains(&"王".into()));
    }
}
