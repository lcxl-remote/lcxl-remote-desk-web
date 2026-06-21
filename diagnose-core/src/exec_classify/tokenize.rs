//! Command tokenizer for whitelist matching.
//!
//! Whitelist matching happens at the **token** level, never on the raw string,
//! so shell metacharacters can never sneak through a "looks like a template"
//! command. The policy is deliberately strict: a tokenizable command may only
//! contain `[A-Za-z0-9]`, space, and `. _ -`. Anything else — quotes, slashes,
//! colons, `| & ; $ ( ) ` < > \n \t`, every other metacharacter — makes the
//! command untokenizable, so it can never match a template (it falls back to
//! suggest-only). The per-slot validators in `templates` tighten this further.
//!
//! This keeps the executable surface to plain `program arg arg` forms. Values
//! that genuinely need a space or path separator (e.g. a service *display*
//! name) do not tokenize and stay suggest-only in v1 — an acceptable trade for
//! closing the injection surface.

/// Why a command could not be tokenized into whitelist-matchable tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenizeError {
    /// Empty or whitespace-only command.
    Empty,
    /// A character outside the safe set (a shell metacharacter or control
    /// character) was present.
    DisallowedCharacter(char),
    /// Too many tokens / too long — bounded to keep matching cheap and to
    /// reject pathological input.
    TooLong,
}

/// Upper bound on raw command length considered for tokenization.
pub const MAX_COMMAND_LEN: usize = 512;
/// Upper bound on token count.
pub const MAX_TOKENS: usize = 16;
/// Upper bound on a single token's length.
pub const MAX_TOKEN_LEN: usize = 256;

/// Whether `c` is allowed inside a tokenizable command. Only ASCII
/// alphanumerics, space (the separator), and `. _ -`. Everything else —
/// including every shell metacharacter and any control or non-ASCII
/// character — is rejected.
fn is_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-')
}

/// Split a command into tokens, rejecting anything that is not a plain
/// `program arg arg` sequence of safe characters.
pub fn tokenize(command: &str) -> Result<Vec<String>, TokenizeError> {
    if command.len() > MAX_COMMAND_LEN {
        return Err(TokenizeError::TooLong);
    }
    if command.trim().is_empty() {
        return Err(TokenizeError::Empty);
    }
    for c in command.chars() {
        if !is_allowed(c) {
            return Err(TokenizeError::DisallowedCharacter(c));
        }
    }
    let tokens: Vec<String> = command
        .split(' ')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();
    if tokens.is_empty() {
        return Err(TokenizeError::Empty);
    }
    if tokens.len() > MAX_TOKENS || tokens.iter().any(|t| t.len() > MAX_TOKEN_LEN) {
        return Err(TokenizeError::TooLong);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_plain_command() {
        assert_eq!(
            tokenize("Get-Service -Name Spooler").unwrap(),
            vec!["Get-Service", "-Name", "Spooler"]
        );
    }

    #[test]
    fn collapses_repeated_spaces() {
        assert_eq!(
            tokenize("docker   logs   abc").unwrap(),
            vec!["docker", "logs", "abc"]
        );
    }

    #[test]
    fn empty_is_rejected() {
        assert_eq!(tokenize("   "), Err(TokenizeError::Empty));
        assert_eq!(tokenize(""), Err(TokenizeError::Empty));
    }

    #[test]
    fn shell_metacharacters_are_rejected() {
        // One representative per metacharacter class the policy must close.
        for bad in [
            "Get-Service | Out-String",
            "Get-Service; whoami",
            "Get-Service && whoami",
            "Get-Service $(whoami)",
            "Get-Service `whoami`",
            "Get-Service > out.txt",
            "Get-Service < in.txt",
            "Get-Service & echo",
            "cmd /c Get-Service", // slash
            "Get-Service \"quoted\"",
            "Get-Service 'quoted'",
            "Get-Service %PATH%",
            "Get-Service\nwhoami",
            "Get-Service\twhoami",
            "Restart-Service C:\\svc",
        ] {
            assert!(
                matches!(tokenize(bad), Err(TokenizeError::DisallowedCharacter(_))),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn over_long_inputs_are_rejected() {
        let long = "a".repeat(MAX_COMMAND_LEN + 1);
        assert_eq!(tokenize(&long), Err(TokenizeError::TooLong));
        let many = vec!["a"; MAX_TOKENS + 1].join(" ");
        assert_eq!(tokenize(&many), Err(TokenizeError::TooLong));
    }
}
