//! Assistant source ids are database primary keys with a table letter and a
//! check letter: `src_h42i` names `agent_assistant_history` row 42 and
//! `src_m7c` names `agent_memory_source` row 7.
//!
//! The check letter only detects copy errors (every single-character error and
//! adjacent transposition, including the table letter); it carries no identity
//! or trust. A mistyped id therefore fails to parse instead of naming another
//! nearby row.

use std::fmt;

const PREFIX: &str = "src_";

/// The table that owns a source row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceTable {
    /// `agent_assistant_history`: one conversation fragment.
    History,
    /// `agent_memory_source`: one memory evidence snapshot.
    Memory,
}

impl SourceTable {
    const fn letter(self) -> char {
        match self {
            Self::History => 'h',
            Self::Memory => 'm',
        }
    }

    const fn check_digit(self) -> u8 {
        match self {
            Self::History => 1,
            Self::Memory => 2,
        }
    }
}

/// A parsed source id: the owning table and its positive primary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceRef {
    pub table: SourceTable,
    pub id: i64,
}

impl SourceRef {
    /// Longest formatted id: prefix, table letter, 19 digits and check letter.
    pub const MAX_LEN: usize = PREFIX.len() + 1 + 19 + 1;

    pub const fn history(id: i64) -> Self {
        Self {
            table: SourceTable::History,
            id,
        }
    }

    pub const fn memory(id: i64) -> Self {
        Self {
            table: SourceTable::Memory,
            id,
        }
    }

    /// Parses `src_<table><decimal id><check>`; any other shape, a zero or
    /// leading-zero id, an id outside `i64` or a wrong check letter is `None`.
    pub fn parse(value: &str) -> Option<Self> {
        let body = value.strip_prefix(PREFIX).filter(|body| body.is_ascii())?;
        let table = match body.as_bytes().first()? {
            b'h' => SourceTable::History,
            b'm' => SourceTable::Memory,
            _ => return None,
        };
        let (digits, check) = body[1..].split_at(body.len().checked_sub(2)?);
        if digits.is_empty()
            || digits.starts_with('0')
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let id = digits.parse::<i64>().ok().filter(|id| *id > 0)?;
        let parsed = Self { table, id };
        (check.as_bytes() == [parsed.check_letter()]).then_some(parsed)
    }

    /// Whether `value` is a well-formed source id of `table`.
    pub fn is(table: SourceTable, value: &str) -> bool {
        Self::parse(value).is_some_and(|parsed| parsed.table == table)
    }

    fn check_letter(self) -> u8 {
        let id = self.id.to_string();
        let digits = std::iter::once(self.table.check_digit()).chain(id.bytes().map(|b| b - b'0'));
        b'a' + damm(digits)
    }
}

impl fmt::Display for SourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PREFIX}{}{}{}",
            self.table.letter(),
            self.id,
            char::from(self.check_letter())
        )
    }
}

/// Damm check digit over decimal digits.
fn damm(digits: impl Iterator<Item = u8>) -> u8 {
    const TABLE: [[u8; 10]; 10] = [
        [0, 3, 1, 7, 5, 9, 8, 6, 4, 2],
        [7, 0, 9, 2, 1, 5, 4, 8, 6, 3],
        [4, 2, 0, 6, 8, 7, 1, 3, 5, 9],
        [1, 7, 5, 0, 9, 8, 3, 4, 2, 6],
        [6, 1, 2, 3, 0, 4, 5, 9, 7, 8],
        [3, 6, 7, 4, 2, 0, 9, 5, 8, 1],
        [5, 8, 6, 9, 7, 2, 0, 1, 3, 4],
        [8, 9, 4, 5, 3, 6, 2, 0, 1, 7],
        [9, 4, 3, 8, 6, 1, 7, 2, 0, 5],
        [2, 5, 8, 1, 4, 3, 6, 7, 9, 0],
    ];
    digits.fold(0, |interim, digit| TABLE[interim as usize][digit as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_both_tables() {
        assert_eq!(SourceRef::history(42).to_string(), "src_h42i");
        for value in [
            SourceRef::history(1),
            SourceRef::history(42),
            SourceRef::memory(7),
            SourceRef::history(i64::MAX),
        ] {
            assert_eq!(SourceRef::parse(&value.to_string()), Some(value));
        }
        assert!(SourceRef::is(SourceTable::History, "src_h42i"));
        assert!(!SourceRef::is(SourceTable::Memory, "src_h42i"));
        assert_eq!(
            SourceRef::history(i64::MAX).to_string().len(),
            SourceRef::MAX_LEN
        );
    }

    #[test]
    fn rejects_malformed_and_legacy_shapes() {
        let valid = SourceRef::history(42).to_string();
        let without_check = &valid[..valid.len() - 1];
        for value in [
            "",
            "src_",
            "src_h",
            "src_hi",
            without_check,
            "src_x42i",
            "src_AAECAwQFBgcICQoLDA0ODw",
            "mem_h42i",
            "src_h042i",
            "src_h0a",
            "src_h99999999999999999999a",
            "src_h-1a",
            "src_h4 2i",
            "src_h4é",
        ] {
            assert_eq!(SourceRef::parse(value), None, "{value}");
        }
    }

    #[test]
    fn check_letter_catches_single_errors_transpositions_and_table_swaps() {
        for id in 1..=2_000_i64 {
            for table in [SourceTable::History, SourceTable::Memory] {
                let valid = SourceRef { table, id }.to_string();
                let bytes = valid.as_bytes();
                // Every single replacement of the table letter, a digit or the
                // check letter fails, except the valid value itself.
                for index in 4..bytes.len() {
                    let alphabet: &[u8] = if index == 4 {
                        b"hm"
                    } else if index == bytes.len() - 1 {
                        b"abcdefghij"
                    } else {
                        b"0123456789"
                    };
                    for &replacement in alphabet {
                        if replacement == bytes[index] {
                            continue;
                        }
                        let mut changed = bytes.to_vec();
                        changed[index] = replacement;
                        let changed = String::from_utf8(changed).unwrap();
                        assert_eq!(SourceRef::parse(&changed), None, "{valid} -> {changed}");
                    }
                }
                // Adjacent transpositions within the digits fail.
                for index in 5..bytes.len() - 2 {
                    if bytes[index] == bytes[index + 1] {
                        continue;
                    }
                    let mut changed = bytes.to_vec();
                    changed.swap(index, index + 1);
                    let changed = String::from_utf8(changed).unwrap();
                    assert_eq!(SourceRef::parse(&changed), None, "{valid} -> {changed}");
                }
            }
        }
    }
}
