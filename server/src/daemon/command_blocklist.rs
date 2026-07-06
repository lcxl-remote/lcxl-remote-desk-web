//! Daemon-side cache of the effective command blocklist synced from the manager.
//!
//! Command classification hard-denies dangerous command shapes before
//! tokenization (Step 0). The compiled-in built-in floor
//! ([`builtin_blocklist`]) is the safe default: this cache is **seeded** with it
//! at construction, so an unsynced daemon (single-machine, remote-signaling, or
//! the cold-start window before the first manager push) still enforces the full
//! floor — the blocklist is fail-**open** when empty, so it is never left empty.
//!
//! The manager (the trusted upstream) pushes the full *effective* set (built-in
//! minus admin-disabled rules, plus enabled custom rules) over a
//! `CommandBlocklistSync` frame on link establishment and on every change; the
//! daemon replaces this cache wholesale, but only when the frame's revision is
//! **monotonic** — a stale (older-revision) frame arriving out of order is
//! dropped, because for the blocklist a rollback would re-open a denied command.
//!
//! Ingestion is fail-closed in the over-blocking-is-safe direction: a rule with
//! no usable pattern is dropped rather than kept as a match-everything or
//! match-nothing rule.

use std::sync::{Arc, RwLock};

use desk_agent_protocol::command_blocklist::{BlocklistMatcher, BlocklistRule};
use desk_agent_protocol::exec_policy::builtin_blocklist;

/// Thread-safe cache of the effective blocklist. Reads (one per classification)
/// take a cheap `Arc` snapshot; writes (one per sync) replace the whole set.
pub struct CommandBlocklistCache {
    inner: RwLock<Arc<Vec<BlocklistRule>>>,
    /// The shared command-blocklist revision last applied. `None` until the first
    /// manager sync; while `None` the cache holds the built-in floor (seeded at
    /// construction). Used to reject a stale, out-of-order sync frame.
    revision: RwLock<Option<i64>>,
}

impl Default for CommandBlocklistCache {
    fn default() -> Self {
        Self {
            // Seed with the built-in floor so an unsynced daemon never fail-opens.
            inner: RwLock::new(Arc::new(builtin_blocklist().to_vec())),
            revision: RwLock::new(None),
        }
    }
}

impl CommandBlocklistCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// A cheap snapshot of the current effective blocklist for classification.
    /// Before the first manager sync this is the built-in floor.
    pub fn snapshot(&self) -> Arc<Vec<BlocklistRule>> {
        self.inner
            .read()
            .expect("command blocklist cache lock")
            .clone()
    }

    /// The last applied command-blocklist revision, if any.
    pub fn revision(&self) -> Option<i64> {
        *self
            .revision
            .read()
            .expect("command blocklist revision lock")
    }

    /// Replace the cache with a manager-synced effective set, gated on a monotonic
    /// `revision`. Rules with no usable pattern are dropped (fail-closed). Returns
    /// `Some(count)` when applied, or `None` when the frame is rejected as stale
    /// (its revision is older than the one already applied).
    pub fn replace(&self, rules: Vec<BlocklistRule>, revision: i64) -> Option<usize> {
        // Hold the revision write lock across the check + both updates so two
        // concurrent syncs cannot interleave into a non-monotonic result. Lock
        // order is always revision-then-inner (readers take only one), so no
        // deadlock.
        let mut rev = self
            .revision
            .write()
            .expect("command blocklist revision lock");
        if let Some(current) = *rev
            && revision < current
        {
            log::warn!(
                "[command-blocklist] dropping stale sync revision {revision} < current {current}"
            );
            return None;
        }
        let accepted = sanitize(rules);
        let count = accepted.len();
        *self.inner.write().expect("command blocklist cache lock") = Arc::new(accepted);
        *rev = Some(revision);
        log::info!(
            "[command-blocklist] applied effective sync: {count} rule(s) (revision {revision})"
        );
        Some(count)
    }
}

/// Drop rules that would be degenerate: a `Substring` rule with no non-empty
/// pattern (an empty substring matches every command — a match-everything
/// footgun), or a `ServiceVerb` rule missing either side (matches nothing).
/// Empty individual patterns are stripped first.
fn sanitize(rules: Vec<BlocklistRule>) -> Vec<BlocklistRule> {
    rules
        .into_iter()
        .filter_map(|mut rule| {
            match &mut rule.matcher {
                BlocklistMatcher::Substring { patterns } => {
                    patterns.retain(|p| !p.is_empty());
                    if patterns.is_empty() {
                        log::warn!(
                            "[command-blocklist] dropping rule {} with no usable substring",
                            rule.rule_id
                        );
                        return None;
                    }
                }
                BlocklistMatcher::ServiceVerb { services, verbs } => {
                    services.retain(|s| !s.is_empty());
                    verbs.retain(|v| !v.is_empty());
                    if services.is_empty() || verbs.is_empty() {
                        log::warn!(
                            "[command-blocklist] dropping service-verb rule {} missing a side",
                            rule.rule_id
                        );
                        return None;
                    }
                }
            }
            Some(rule)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn substring_rule(rule_id: &str, patterns: &[&str]) -> BlocklistRule {
        BlocklistRule {
            rule_id: rule_id.to_string(),
            category: "test".to_string(),
            matcher: BlocklistMatcher::Substring {
                patterns: patterns.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn unsynced_cache_holds_the_builtin_floor() {
        let cache = CommandBlocklistCache::new();
        assert_eq!(cache.revision(), None);
        // Seeded with the built-in floor (non-empty), so never fail-open.
        assert_eq!(cache.snapshot().len(), builtin_blocklist().len());
    }

    #[test]
    fn replace_applies_and_records_revision() {
        let cache = CommandBlocklistCache::new();
        let applied = cache.replace(vec![substring_rule("custom.a", &["mimikatz"])], 5);
        assert_eq!(applied, Some(1));
        assert_eq!(cache.revision(), Some(5));
        assert_eq!(cache.snapshot().len(), 1);
    }

    #[test]
    fn stale_frame_is_dropped_and_does_not_roll_back() {
        let cache = CommandBlocklistCache::new();
        cache.replace(
            vec![
                substring_rule("custom.a", &["alpha"]),
                substring_rule("custom.b", &["beta"]),
            ],
            10,
        );
        assert_eq!(cache.snapshot().len(), 2);
        // A frame with an older revision must be rejected — no rollback.
        let rejected = cache.replace(vec![substring_rule("custom.c", &["gamma"])], 9);
        assert_eq!(rejected, None);
        assert_eq!(
            cache.snapshot().len(),
            2,
            "stale frame must not replace the newer set"
        );
        assert_eq!(cache.revision(), Some(10));
    }

    #[test]
    fn equal_revision_is_accepted() {
        // Re-push at the same revision (a resync on reconnect) is allowed.
        let cache = CommandBlocklistCache::new();
        cache.replace(vec![substring_rule("custom.a", &["alpha"])], 7);
        let applied = cache.replace(vec![substring_rule("custom.b", &["beta"])], 7);
        assert_eq!(applied, Some(1));
        assert_eq!(cache.snapshot()[0].rule_id, "custom.b");
    }

    #[test]
    fn empty_substring_rule_is_dropped_fail_closed() {
        let cache = CommandBlocklistCache::new();
        // A rule whose only pattern is empty would match every command; drop it.
        let applied = cache.replace(
            vec![
                substring_rule("custom.ok", &["mimikatz"]),
                substring_rule("custom.bad", &[""]),
            ],
            1,
        );
        assert_eq!(applied, Some(1), "the empty-substring rule must be dropped");
        assert_eq!(cache.snapshot()[0].rule_id, "custom.ok");
    }

    #[test]
    fn service_verb_missing_a_side_is_dropped() {
        let cache = CommandBlocklistCache::new();
        let rule = BlocklistRule {
            rule_id: "builtin.service_verb".to_string(),
            category: "disable security software".to_string(),
            matcher: BlocklistMatcher::ServiceVerb {
                services: vec!["windefend".to_string()],
                verbs: vec![],
            },
        };
        let applied = cache.replace(vec![rule], 1);
        assert_eq!(applied, Some(0));
    }
}
