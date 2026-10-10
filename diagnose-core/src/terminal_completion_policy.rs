//! Revisioned, shared policy for terminal command completion output.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCompletionPolicy {
    pub revision: u64,
    pub max_output_tokens: u32,
}

impl Default for TerminalCompletionPolicy {
    fn default() -> Self {
        Self {
            revision: 0,
            max_output_tokens: 512,
        }
    }
}

impl TerminalCompletionPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_output_tokens == 0 {
            return Err("output tokens must be positive");
        }
        Ok(())
    }

    pub fn candidate(&self, max_output_tokens: u32) -> Result<Self, &'static str> {
        let next = Self {
            revision: self.revision.checked_add(1).ok_or("revision overflow")?,
            max_output_tokens,
        };
        next.validate()?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positive_values_are_configurable_without_a_hidden_ceiling() {
        let policy = TerminalCompletionPolicy::default();
        assert_eq!(policy.max_output_tokens, 512);
        assert!(policy.candidate(0).is_err());
        assert_eq!(
            policy.candidate(128_000).unwrap().max_output_tokens,
            128_000
        );
        assert_eq!(policy.candidate(u32::MAX).unwrap().revision, 1);
        for value in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(4294967296u64),
        ] {
            assert!(
                serde_json::from_value::<TerminalCompletionPolicy>(
                    serde_json::json!({"revision":0,"max_output_tokens":value})
                )
                .is_err()
            );
        }
    }
}
