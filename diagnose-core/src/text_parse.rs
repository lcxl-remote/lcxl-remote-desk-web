//! Bounded helpers for parsing model-generated JSON text.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    Structured,
    Degraded,
}

impl ParseOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structured => "structured",
            Self::Degraded => "degraded",
        }
    }
}

pub(crate) fn extract_json_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let bytes = content.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&content[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn truncate_on_char_boundary(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}
