//! Process-start bootstrap token configuration.

use std::fmt;

use super::{
    client_ip::NetworkKey,
    rate_limit::{AuthRateLimiter, BootstrapAttempt},
};

pub const BOOTSTRAP_TOKEN_ENV: &str = "LRD_BOOTSTRAP_TOKEN";

#[derive(Clone)]
pub struct BootstrapToken(Option<Vec<u8>>);

impl fmt::Debug for BootstrapToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootstrapToken")
            .field("required", &self.is_required())
            .finish()
    }
}

impl BootstrapToken {
    pub fn from_env() -> Result<Self, String> {
        let parsed = match std::env::var(BOOTSTRAP_TOKEN_ENV) {
            Ok(value) => Self::from_value(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::from_value(None),
            Err(error) => return Err(format!("failed to read {BOOTSTRAP_TOKEN_ENV}: {error}")),
        };
        let (token, warn_short) = parsed?;
        if warn_short {
            log::warn!(
                "{BOOTSTRAP_TOKEN_ENV} is shorter than 16 characters; use at least 32 random bytes"
            );
        }
        Ok(token)
    }

    fn from_value(value: Option<&str>) -> Result<(Self, bool), String> {
        let Some(value) = value else {
            return Ok((Self(None), false));
        };
        let value = value.trim();
        if value.is_empty() {
            return Err(format!(
                "{BOOTSTRAP_TOKEN_ENV} is empty; remove the variable to disable the bootstrap gate or set a non-empty high-entropy value"
            ));
        }
        Ok((Self(Some(value.as_bytes().to_vec())), value.len() < 16))
    }

    pub fn disabled() -> Self {
        Self(None)
    }

    pub fn required(value: impl AsRef<str>) -> Self {
        Self(Some(value.as_ref().trim().as_bytes().to_vec()))
    }

    pub fn is_required(&self) -> bool {
        self.0.is_some()
    }

    pub fn evaluate(
        &self,
        limiter: &AuthRateLimiter,
        key: NetworkKey,
        provided: Option<&str>,
    ) -> BootstrapAttempt {
        let Some(expected) = self.0.as_deref() else {
            return BootstrapAttempt::Allowed;
        };
        limiter.evaluate_bootstrap_attempt(
            key,
            expected,
            provided.unwrap_or_default().trim().as_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_contains_the_secret() {
        let token = BootstrapToken::required("secret-value-that-must-not-leak");
        let debug = format!("{token:?}");
        assert!(debug.contains("required: true"));
        assert!(!debug.contains("secret-value"));
    }

    #[test]
    fn value_contract_disables_missing_rejects_blank_and_trims_present_tokens() {
        let (missing, warn_short) = BootstrapToken::from_value(None).unwrap();
        assert!(!missing.is_required());
        assert!(!warn_short);

        let error = BootstrapToken::from_value(Some(" \t ")).unwrap_err();
        assert!(error.contains(BOOTSTRAP_TOKEN_ENV));
        assert!(error.contains("remove the variable"));
        assert!(error.contains("non-empty high-entropy value"));

        let (short, warn_short) = BootstrapToken::from_value(Some(" short-token ")).unwrap();
        assert!(short.is_required());
        assert!(warn_short);
        assert_eq!(short.0.as_deref(), Some(b"short-token".as_slice()));

        let (strong, warn_short) =
            BootstrapToken::from_value(Some("0123456789abcdef0123456789abcdef")).unwrap();
        assert!(strong.is_required());
        assert!(!warn_short);
    }
}
