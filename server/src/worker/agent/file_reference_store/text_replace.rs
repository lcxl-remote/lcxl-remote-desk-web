use desk_agent_protocol::{AgentError, AgentErrorKind};

use super::error;

pub(super) fn replace_once(text: &str, before: &str, after: &str) -> Result<Vec<u8>, AgentError> {
    if before.is_empty() {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "replace_once.before must not be empty",
            false,
        ));
    }

    let matches = text
        .char_indices()
        .filter(|(offset, _)| text[*offset..].starts_with(before))
        .count();
    match matches {
        0 => Err(error(
            AgentErrorKind::InvalidInput,
            "replace_once.before matched 0 locations in the current file; read the file again, provide an exact UTF-8 fragment, and request approval for the revised replacement",
            false,
        )),
        1 => Ok(text.replacen(before, after, 1).into_bytes()),
        count => Err(error(
            AgentErrorKind::InvalidInput,
            format!(
                "replace_once.before matched {count} locations in the current file; include more surrounding text so it matches exactly once, then request approval for the revised replacement"
            ),
            false,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_zero_and_multiple_matches_without_modifying_content() {
        let missing = replace_once("alpha beta", "gamma", "replacement").unwrap_err();
        assert_eq!(missing.kind, AgentErrorKind::InvalidInput);
        assert!(missing.message.contains("matched 0 locations"));

        let repeated = replace_once("alpha alpha alpha", "alpha", "replacement").unwrap_err();
        assert_eq!(repeated.kind, AgentErrorKind::InvalidInput);
        assert!(repeated.message.contains("matched 3 locations"));
        assert!(repeated.message.contains("more surrounding text"));
    }

    #[test]
    fn counts_overlapping_matches_and_replaces_one_unique_match() {
        let overlapping = replace_once("aaa", "aa", "b").unwrap_err();
        assert!(overlapping.message.contains("matched 2 locations"));

        assert_eq!(
            replace_once("alpha beta", "beta", "gamma").unwrap(),
            b"alpha gamma"
        );
    }
}
