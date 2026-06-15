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

/// Current `CommandTemplateSync` payload wire version. The daemon ignores a
/// payload whose version it does not understand.
pub const COMMAND_TEMPLATE_SYNC_VERSION: u16 = 1;

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
}
