//! Manager → daemon command-template sync carrier.
//!
//! Operator-configurable command templates live in the manager database (admin
//! CRUD); command classification happens in the desk-server daemon. The manager
//! therefore syncs the enabled set to the daemon over a `CommandTemplateSync`
//! signaling frame, and the daemon unions them with its compiled-in built-in
//! templates at classify time.
//!
//! Unlike the built-in slot-pattern templates, an operator template is an
//! **exact argv allowlist entry**: an inbound command matches only when its
//! tokens equal the template's `argv`, and the executed argv *is* that `argv`
//! (the worker runs it verbatim, never re-parsing a free-form string). This is
//! the safest possible shape — there is no parameter substitution to escape.
//!
//! Operator templates are purely **additive** over the built-in baseline and
//! every executed command still passes the policy `max_risk` ceiling, so an
//! operator template can never escalate a command past the policy matrix.

use serde::{Deserialize, Serialize};

use crate::RiskLevel;
use crate::exec::ExecEffect;

/// Current `CommandTemplateSync` payload wire version the manager emits. A daemon
/// accepts any version in `[MIN_COMMAND_TEMPLATE_SYNC_VERSION,
/// COMMAND_TEMPLATE_SYNC_VERSION]` and ignores anything outside it.
///
/// - v1: `{ version, templates }`.
/// - v2: adds `command_template_revision` (the shared monotonic revision the
///   manager stamped the set with).
/// - v3: adds `epoch`. The set is now **narrowed** to the device's authorized
///   organizations, so a `revision=N` no longer means the same set it did under
///   the old all-org semantics. The epoch names the wire-semantics generation:
///   the daemon accepts a payload only when `(epoch, revision)` is not strictly
///   older than the last it applied, so once it accepts a narrowed v3 set an old
///   manager's all-org set (epoch 0) can no longer downgrade it.
///
/// During a rolling upgrade an old daemon (which only knows v1) receives a newer
/// payload and safely ignores it — it keeps its prior template set rather than
/// misapplying an unknown shape; a fleet exec request it cannot template-match is
/// then refused by its PEP, never mis-executed.
pub const COMMAND_TEMPLATE_SYNC_VERSION: u16 = 3;

/// Oldest `CommandTemplateSync` payload version a current daemon still accepts.
/// A v1 payload (from an old manager during a rolling upgrade) carries no
/// revision and still replaces the cache.
pub const MIN_COMMAND_TEMPLATE_SYNC_VERSION: u16 = 1;

/// The wire-semantics generation the current manager stamps on a v3 payload. A
/// payload with no `epoch` (v1 / v2 from an older manager) is read as epoch 0, so
/// this scoped generation (epoch 1) always outranks the old all-org semantics and
/// a rolling-upgrade downgrade cannot re-widen an already-narrowed cache. Bump this
/// only when a change again alters what a given `revision` means (e.g. a different
/// narrowing dimension); a plain content change just advances the revision.
pub const COMMAND_TEMPLATE_SYNC_EPOCH: u16 = 1;

/// One operator-configured exact-argv command template, as synced from the
/// manager to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedCommandTemplate {
    /// Stable identifier (the manager's `template_id`), surfaced as the
    /// classification's `matched_template`.
    pub template_id: String,
    /// The exact argv this template allowlists. `argv[0]` is the program; the
    /// rest are its arguments. An inbound command matches only when its tokens
    /// equal this argv, and the worker executes exactly this argv.
    pub argv: Vec<String>,
    /// Whether running it changes state. Drives risk grading and the read-only
    /// vs. mutating execution-mode gate.
    pub effect: ExecEffect,
}

impl SyncedCommandTemplate {
    /// Risk grade derived from the template's effect.
    pub fn risk(&self) -> RiskLevel {
        risk_for_effect(self.effect)
    }
}

/// Risk grade derived from a template's effect. Operator templates carry only an
/// effect (the DB schema has no risk column), so read-only maps to `Low` and
/// mutating to `High`, mirroring the built-in templates' convention. The
/// execution path still enforces the policy `max_risk` ceiling on top of this.
pub fn risk_for_effect(effect: ExecEffect) -> RiskLevel {
    match effect {
        ExecEffect::ReadOnly => RiskLevel::Low,
        ExecEffect::Mutating => RiskLevel::High,
    }
}

/// The payload of a `CommandTemplateSync` signaling frame: the full enabled
/// operator template set. The daemon replaces its cache wholesale (the manager
/// always sends the complete set, both on link establishment and on change).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandTemplateSyncPayload {
    pub version: u16,
    pub templates: Vec<SyncedCommandTemplate>,
    /// The shared monotonic command-template revision in force when the manager
    /// built this payload (v2+). `None` on a v1 payload from an old manager. The
    /// daemon uses `(epoch, revision)` to reject a strictly-older sync so a
    /// rolling-upgrade peer cannot overwrite a newer set with a stale one;
    /// execution-time safety still comes from the daemon PEP re-validate plus the
    /// manager intent-transaction recheck.
    #[serde(default)]
    pub command_template_revision: Option<i64>,
    /// The wire-semantics generation (v3+). `0` (the default, and what a v1 / v2
    /// payload decodes to) is the legacy all-org semantics; the current manager
    /// stamps [`COMMAND_TEMPLATE_SYNC_EPOCH`] for the narrowed set. Compared before
    /// `command_template_revision` so a higher epoch always wins regardless of the
    /// revision number, which is what stops an old all-org manager (epoch 0) from
    /// downgrading a daemon that already accepted a narrowed set.
    #[serde(default)]
    pub epoch: u16,
}

/// Why an operator template's argv was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTemplateError {
    /// The argv array was empty (no program).
    Empty,
    /// One of the argv tokens was the empty string.
    EmptyToken,
    /// A token contained a character outside the safe set (the same set the
    /// command tokenizer permits): a metacharacter, whitespace, or control char.
    UnsafeChar(String),
}

impl std::fmt::Display for CommandTemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandTemplateError::Empty => write!(f, "argv is empty"),
            CommandTemplateError::EmptyToken => write!(f, "argv contains an empty token"),
            CommandTemplateError::UnsafeChar(t) => {
                write!(f, "argv token {t:?} contains an unsafe character")
            }
        }
    }
}

impl std::error::Error for CommandTemplateError {}

/// Validate an operator template's argv shape (fail-closed): non-empty, with
/// every token non-empty and built only from the tokenizer's safe character set
/// (ASCII alphanumeric plus `.`, `_`, `-`). This is enforced both at manager
/// save time and at daemon ingestion, so a buggy or compromised upstream cannot
/// inject a metacharacter-bearing argv. Tokens outside the safe set could never
/// match a tokenized input anyway, so rejecting them only removes dead/unsafe
/// entries.
pub fn validate_template_argv(argv: &[String]) -> Result<(), CommandTemplateError> {
    if argv.is_empty() {
        return Err(CommandTemplateError::Empty);
    }
    for token in argv {
        if token.is_empty() {
            return Err(CommandTemplateError::EmptyToken);
        }
        if !token.chars().all(is_safe_token_char) {
            return Err(CommandTemplateError::UnsafeChar(token.clone()));
        }
    }
    Ok(())
}

/// The tokenizer's safe character set: ASCII alphanumeric plus `.`, `_`, `-`.
fn is_safe_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_tracks_effect() {
        assert_eq!(risk_for_effect(ExecEffect::ReadOnly), RiskLevel::Low);
        assert_eq!(risk_for_effect(ExecEffect::Mutating), RiskLevel::High);
    }

    #[test]
    fn valid_argv_passes() {
        assert!(validate_template_argv(&["docker".into(), "ps".into(), "-a".into()]).is_ok());
        assert!(validate_template_argv(&["Get-Disk".into()]).is_ok());
    }

    #[test]
    fn empty_argv_is_rejected() {
        assert_eq!(
            validate_template_argv(&[]),
            Err(CommandTemplateError::Empty)
        );
    }

    #[test]
    fn empty_token_is_rejected() {
        assert_eq!(
            validate_template_argv(&["docker".into(), "".into()]),
            Err(CommandTemplateError::EmptyToken)
        );
    }

    #[test]
    fn metachar_tokens_are_rejected() {
        for bad in ["docker;rm", "a b", "$(x)", "a|b", "a>b", "a/b", "a=b"] {
            assert!(
                matches!(
                    validate_template_argv(&[bad.to_string()]),
                    Err(CommandTemplateError::UnsafeChar(_))
                ),
                "expected {bad:?} to be rejected"
            );
        }
    }

    fn sample_template() -> SyncedCommandTemplate {
        SyncedCommandTemplate {
            template_id: "docker_ps".into(),
            argv: vec!["docker".into(), "ps".into()],
            effect: ExecEffect::ReadOnly,
        }
    }

    #[test]
    fn v3_payload_round_trips_with_revision_and_epoch() {
        let payload = CommandTemplateSyncPayload {
            version: COMMAND_TEMPLATE_SYNC_VERSION,
            templates: vec![sample_template()],
            command_template_revision: Some(42),
            epoch: COMMAND_TEMPLATE_SYNC_EPOCH,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: CommandTemplateSyncPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);
        assert_eq!(back.command_template_revision, Some(42));
        assert_eq!(back.epoch, 1);
    }

    #[test]
    fn v1_payload_without_revision_or_epoch_deserializes_as_defaults() {
        // A v1 / v2 frame from an old manager has no `epoch` (and v1 no revision);
        // a current daemon must still decode it (serde default → None / 0), reading
        // it as the legacy all-org epoch 0.
        let v1_json = r#"{"version":1,"templates":[{"template_id":"docker_ps","argv":["docker","ps"],"effect":"read_only"}]}"#;
        let payload: CommandTemplateSyncPayload = serde_json::from_str(v1_json).unwrap();
        assert_eq!(payload.version, 1);
        assert_eq!(payload.command_template_revision, None);
        assert_eq!(payload.epoch, 0);
        assert_eq!(payload.templates, vec![sample_template()]);
    }

    #[test]
    fn current_version_is_in_supported_range() {
        // The emitted version must not be below the minimum supported one.
        assert_eq!(COMMAND_TEMPLATE_SYNC_VERSION, 3);
        assert_eq!(MIN_COMMAND_TEMPLATE_SYNC_VERSION, 1);
        assert_eq!(COMMAND_TEMPLATE_SYNC_EPOCH, 1);
    }
}
