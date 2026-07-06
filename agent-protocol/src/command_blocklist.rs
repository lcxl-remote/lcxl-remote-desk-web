//! Manager → daemon command-blocklist sync carrier.
//!
//! Command classification hard-denies a small set of dangerous command shapes
//! **before** tokenization / template matching (credential access, disabling
//! security software, persistence, download-and-execute, audit/log tampering).
//! The built-in signatures live in [`crate::exec_policy`] as a compiled-in
//! [`builtin_blocklist`](crate::exec_policy::builtin_blocklist), which is the
//! single source of truth and the safe default whenever no manager sync has been
//! received (an open-source single-instance daemon, or the cold-start window
//! before the first push).
//!
//! On top of that floor, a manager platform admin may **disable** individual
//! built-in rules and **add** custom substring rules. The manager computes the
//! resulting *effective* set (built-in minus disabled, plus enabled custom) and
//! syncs it to the daemon over a `CommandBlocklistSync` frame; the daemon caches
//! it and matches against it at classify time. Unlike the whitelist (empty =
//! safe), the blocklist is fail-**open** when empty, so the cache falls back to
//! the full built-in set until a manager sync arrives — the blocklist floor is
//! never silently bypassed.
//!
//! This carrier is I/O-free and platform-agnostic so it compiles and runs
//! identically in the manager and in a standalone open-source build of the
//! daemon.

use serde::{Deserialize, Serialize};

/// Current `CommandBlocklistSync` payload wire version the manager emits. A
/// daemon accepts any version in `[MIN_COMMAND_BLOCKLIST_SYNC_VERSION,
/// COMMAND_BLOCKLIST_SYNC_VERSION]` and ignores anything outside it.
pub const COMMAND_BLOCKLIST_SYNC_VERSION: u16 = 1;

/// Oldest `CommandBlocklistSync` payload version a current daemon still accepts.
pub const MIN_COMMAND_BLOCKLIST_SYNC_VERSION: u16 = 1;

/// How a [`BlocklistRule`] decides whether a lowercased command is denied.
///
/// The two shapes mirror the built-in policy: a plain substring set (any hit
/// denies) and the "mutating verb aimed at a protected service" combination.
/// Custom operator rules are only ever [`BlocklistMatcher::Substring`]; the
/// [`BlocklistMatcher::ServiceVerb`] combination exists only as a built-in rule
/// (which an admin may disable but cannot author, avoiding an editable-JSON
/// misconfiguration surface).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlocklistMatcher {
    /// Denies when the lowercased command contains **any** of these substrings.
    Substring { patterns: Vec<String> },
    /// Denies when the command contains **any** protected service short-name
    /// **and** **any** mutating verb (both lowercased). Not adjacency-sensitive.
    ServiceVerb {
        services: Vec<String>,
        verbs: Vec<String>,
    },
}

/// One effective blocklist rule as synced from the manager to the daemon (or a
/// built-in rule constructed in code). A rule denies a command when its
/// [`matcher`](BlocklistRule::matcher) hits; the [`category`](BlocklistRule::category)
/// is the prohibited-category label surfaced in the `Blocked` reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlocklistRule {
    /// Stable identifier. Built-in rules use `builtin.<slug>` ids that are
    /// **never reused** (removing a built-in signature retires its id forever, so
    /// a stale disable-override can never silently disable a different rule);
    /// custom rules use `custom.<slug>`.
    pub rule_id: String,
    /// Prohibited-category label (display + audit + `Blocked` reason).
    pub category: String,
    /// How this rule matches a lowercased command.
    pub matcher: BlocklistMatcher,
}

impl BlocklistRule {
    /// Whether this rule denies `lowercased` (the command already lowercased with
    /// [`str::to_ascii_lowercase`]).
    pub fn matches(&self, lowercased: &str) -> bool {
        match &self.matcher {
            BlocklistMatcher::Substring { patterns } => {
                patterns.iter().any(|p| lowercased.contains(p.as_str()))
            }
            BlocklistMatcher::ServiceVerb { services, verbs } => {
                services.iter().any(|s| lowercased.contains(s.as_str()))
                    && verbs.iter().any(|v| lowercased.contains(v.as_str()))
            }
        }
    }
}

/// Apply a blocklist rule set to an already-lowercased command form, returning
/// the prohibited category of the first matching rule (or `None`). The lifetime
/// of the returned label is tied to the rules slice, so both the `'static`
/// built-in set and a borrowed effective set work.
pub fn blocklist_match<'a>(rules: &'a [BlocklistRule], lowercased: &str) -> Option<&'a str> {
    rules
        .iter()
        .find(|r| r.matches(lowercased))
        .map(|r| r.category.as_str())
}

/// The payload of a `CommandBlocklistSync` signaling frame: the full effective
/// blocklist set (built-in minus disabled, plus enabled custom). The daemon
/// replaces its cache wholesale, gated on the revision being monotonic — the
/// manager always sends the complete set, both on link establishment and on
/// change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandBlocklistSyncPayload {
    pub version: u16,
    pub rules: Vec<BlocklistRule>,
    /// The shared monotonic command-blocklist revision in force when the manager
    /// built this payload. The daemon uses it to reject a stale (older-revision)
    /// frame that arrives out of order — for the blocklist a stale rollback would
    /// re-open a denied command, so ordering is enforced (unlike the advisory
    /// command-template revision). Always `Some` on a manager push.
    #[serde(default)]
    pub command_blocklist_revision: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn substring_rule(rule_id: &str, category: &str, patterns: &[&str]) -> BlocklistRule {
        BlocklistRule {
            rule_id: rule_id.to_string(),
            category: category.to_string(),
            matcher: BlocklistMatcher::Substring {
                patterns: patterns.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn substring_rule_matches_case_lowered_input() {
        let rule = substring_rule("custom.mimikatz", "credential access", &["mimikatz"]);
        assert!(rule.matches("iex mimikatz"));
        assert!(!rule.matches("docker ps"));
    }

    #[test]
    fn service_verb_rule_requires_both_service_and_verb() {
        let rule = BlocklistRule {
            rule_id: "builtin.service_verb".to_string(),
            category: "disable security software".to_string(),
            matcher: BlocklistMatcher::ServiceVerb {
                services: vec!["windefend".to_string()],
                verbs: vec!["stop-service".to_string()],
            },
        };
        assert!(rule.matches("stop-service windefend"));
        // Service present but no mutating verb → not denied.
        assert!(!rule.matches("get-service windefend"));
        // Verb present but no protected service → not denied.
        assert!(!rule.matches("stop-service spooler"));
    }

    #[test]
    fn blocklist_match_returns_first_matching_category() {
        let rules = vec![
            substring_rule("custom.a", "cat-a", &["alpha"]),
            substring_rule("custom.b", "cat-b", &["beta"]),
        ];
        assert_eq!(blocklist_match(&rules, "run beta job"), Some("cat-b"));
        assert_eq!(blocklist_match(&rules, "run gamma job"), None);
    }

    #[test]
    fn payload_round_trips_with_revision() {
        let payload = CommandBlocklistSyncPayload {
            version: COMMAND_BLOCKLIST_SYNC_VERSION,
            rules: vec![
                substring_rule("custom.x", "credential access", &["mimikatz"]),
                BlocklistRule {
                    rule_id: "builtin.service_verb".to_string(),
                    category: "disable security software".to_string(),
                    matcher: BlocklistMatcher::ServiceVerb {
                        services: vec!["windefend".to_string()],
                        verbs: vec!["stop-service".to_string()],
                    },
                },
            ],
            command_blocklist_revision: Some(7),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: CommandBlocklistSyncPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);
        assert_eq!(back.command_blocklist_revision, Some(7));
    }

    #[test]
    fn payload_without_revision_deserializes_as_none() {
        let json = r#"{"version":1,"rules":[]}"#;
        let payload: CommandBlocklistSyncPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.version, 1);
        assert_eq!(payload.command_blocklist_revision, None);
        assert!(payload.rules.is_empty());
    }

    #[test]
    fn matcher_serializes_snake_case_tagged() {
        let json = serde_json::to_string(&BlocklistMatcher::Substring {
            patterns: vec!["mimikatz".to_string()],
        })
        .unwrap();
        assert_eq!(json, r#"{"substring":{"patterns":["mimikatz"]}}"#);
        let json = serde_json::to_string(&BlocklistMatcher::ServiceVerb {
            services: vec!["windefend".to_string()],
            verbs: vec!["stop".to_string()],
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"service_verb":{"services":["windefend"],"verbs":["stop"]}}"#
        );
    }
}
