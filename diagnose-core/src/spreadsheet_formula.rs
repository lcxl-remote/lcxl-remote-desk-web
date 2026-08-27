//! Frozen, model-independent safety contract for spreadsheet formulas.
//!
//! V1 deliberately accepts only A1 references, local arithmetic/comparison,
//! and a small closed function allowlist. It never evaluates a formula. The
//! caller must still bind the returned digest, locale, and target range into an
//! exact mutation grant and ask Excel (or an inert writer + Excel) to read back
//! the result.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FORMULA_POLICY_VERSION: &str = "spreadsheet-formula-v1";
pub const FORMULA_LOCALE_V1: &str = "en-US-a1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormulaExpr {
    Number {
        canonical: String,
    },
    Boolean {
        value: bool,
    },
    Cell {
        reference: CellRef,
    },
    Range {
        start: CellRef,
        end: CellRef,
    },
    Unary {
        op: UnaryOp,
        value: Box<FormulaExpr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<FormulaExpr>,
        right: Box<FormulaExpr>,
    },
    Function {
        name: FormulaFunction,
        args: Vec<FormulaExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellRef {
    pub sheet: Option<String>,
    pub column: u16,
    pub row: u32,
    pub absolute_column: bool,
    pub absolute_row: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnaryOp {
    Plus,
    Minus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Power,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum FormulaFunction {
    Sum,
    Average,
    Min,
    Max,
    Count,
    If,
    Abs,
    Round,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedFormulaPatch {
    pub policy_version: String,
    pub locale: String,
    pub target: FormulaExpr,
    pub formula: FormulaExpr,
    pub ast_digest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaValidationError(&'static str);

impl FormulaValidationError {
    pub const fn message(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for FormulaValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for FormulaValidationError {}

const INVALID: FormulaValidationError = FormulaValidationError(
    "formula is outside the frozen local A1 arithmetic, comparison, and aggregate allowlist",
);

/// Parse and validate a formula patch without evaluating it.
pub fn validate_formula_patch(
    formula: &str,
    target_range: &str,
    locale: &str,
    allowed_sheets: &[String],
) -> Result<ValidatedFormulaPatch, FormulaValidationError> {
    if locale != FORMULA_LOCALE_V1 || formula.len() > 4096 || target_range.len() > 256 {
        return Err(INVALID);
    }
    let formula_text = formula.strip_prefix('=').ok_or(INVALID)?;
    if formula_text.is_empty() || formula_text.starts_with('=') {
        return Err(INVALID);
    }
    let formula = Parser::new(formula_text)?.parse_expression_complete()?;
    let target = Parser::new(target_range)?.parse_reference_complete()?;
    if !matches!(target, FormulaExpr::Cell { .. } | FormulaExpr::Range { .. }) {
        return Err(INVALID);
    }
    let allowed = allowed_sheets
        .iter()
        .map(|sheet| sheet.to_lowercase())
        .collect::<BTreeSet<_>>();
    if allowed.is_empty()
        || !all_explicit_sheets_allowed(&formula, &allowed)
        || !all_explicit_sheets_allowed(&target, &allowed)
    {
        return Err(INVALID);
    }
    let digest_input =
        serde_json::to_vec(&(FORMULA_POLICY_VERSION, FORMULA_LOCALE_V1, &target, &formula))
            .map_err(|_| INVALID)?;
    Ok(ValidatedFormulaPatch {
        policy_version: FORMULA_POLICY_VERSION.into(),
        locale: FORMULA_LOCALE_V1.into(),
        target,
        formula,
        ast_digest_sha256: format!("{:x}", Sha256::digest(digest_input)),
    })
}

fn all_explicit_sheets_allowed(expr: &FormulaExpr, allowed: &BTreeSet<String>) -> bool {
    match expr {
        FormulaExpr::Cell { reference } => reference
            .sheet
            .as_ref()
            .is_none_or(|sheet| allowed.contains(&sheet.to_lowercase())),
        FormulaExpr::Range { start, end } => [start, end].iter().all(|reference| {
            reference
                .sheet
                .as_ref()
                .is_none_or(|sheet| allowed.contains(&sheet.to_lowercase()))
        }),
        FormulaExpr::Unary { value, .. } => all_explicit_sheets_allowed(value, allowed),
        FormulaExpr::Binary { left, right, .. } => {
            all_explicit_sheets_allowed(left, allowed)
                && all_explicit_sheets_allowed(right, allowed)
        }
        FormulaExpr::Function { args, .. } => args
            .iter()
            .all(|argument| all_explicit_sheets_allowed(argument, allowed)),
        FormulaExpr::Number { .. } | FormulaExpr::Boolean { .. } => true,
    }
}

/// Neutralize a CSV field that spreadsheet applications may interpret as a
/// formula. The returned boolean is suitable for transformation lineage.
pub fn escape_csv_formula_injection(value: &str) -> (String, bool) {
    let dangerous = value
        .chars()
        .next()
        .is_some_and(|first| matches!(first, '=' | '+' | '-' | '@' | '\t' | '\r' | '\n'));
    if dangerous {
        (format!("'{value}"), true)
    } else {
        (value.to_string(), false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    QuotedSheet(String),
    Number(String),
    LParen,
    RParen,
    Comma,
    Colon,
    Bang,
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

fn tokenize(input: &str) -> Result<Vec<Token>, FormulaValidationError> {
    if input.is_empty() || input.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(INVALID);
    }
    let chars = input.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        if current == ' ' {
            index += 1;
            continue;
        }
        let simple = match current {
            '(' => Some(Token::LParen),
            ')' => Some(Token::RParen),
            ',' => Some(Token::Comma),
            ':' => Some(Token::Colon),
            '!' => Some(Token::Bang),
            '+' => Some(Token::Plus),
            '-' => Some(Token::Minus),
            '*' => Some(Token::Star),
            '/' => Some(Token::Slash),
            '^' => Some(Token::Caret),
            '=' => Some(Token::Equal),
            _ => None,
        };
        if let Some(token) = simple {
            tokens.push(token);
            index += 1;
            continue;
        }
        if matches!(current, '<' | '>') {
            let next = chars.get(index + 1).copied();
            let token = match (current, next) {
                ('<', Some('=')) => {
                    index += 1;
                    Token::LessOrEqual
                }
                ('<', Some('>')) => {
                    index += 1;
                    Token::NotEqual
                }
                ('>', Some('=')) => {
                    index += 1;
                    Token::GreaterOrEqual
                }
                ('<', _) => Token::Less,
                ('>', _) => Token::Greater,
                _ => unreachable!(),
            };
            tokens.push(token);
            index += 1;
            continue;
        }
        if current == '\'' {
            index += 1;
            let mut sheet = String::new();
            let mut closed = false;
            while index < chars.len() {
                if chars[index] == '\'' {
                    if chars.get(index + 1) == Some(&'\'') {
                        sheet.push('\'');
                        index += 2;
                    } else {
                        index += 1;
                        closed = true;
                        break;
                    }
                } else {
                    sheet.push(chars[index]);
                    index += 1;
                }
            }
            if !closed || sheet.is_empty() || sheet.len() > 128 {
                return Err(INVALID);
            }
            tokens.push(Token::QuotedSheet(sheet));
            continue;
        }
        if current.is_ascii_digit() || current == '.' {
            let start = index;
            let mut dots = 0;
            while index < chars.len() && (chars[index].is_ascii_digit() || chars[index] == '.') {
                dots += usize::from(chars[index] == '.');
                index += 1;
            }
            let number = chars[start..index].iter().collect::<String>();
            if dots > 1 || number == "." || number.len() > 64 {
                return Err(INVALID);
            }
            tokens.push(Token::Number(canonical_number(&number)?));
            continue;
        }
        if current.is_ascii_alphabetic() || current == '_' || current == '$' {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || matches!(chars[index], '_' | '.' | '$'))
            {
                index += 1;
            }
            let identifier = chars[start..index].iter().collect::<String>();
            if identifier.len() > 128 {
                return Err(INVALID);
            }
            tokens.push(Token::Ident(identifier));
            continue;
        }
        // Strings, array constants, structured refs, external workbook refs,
        // percent, semicolon locale separators, and every other token fail shut.
        return Err(INVALID);
    }
    if tokens.is_empty() || tokens.len() > 1024 {
        return Err(INVALID);
    }
    Ok(tokens)
}

fn canonical_number(value: &str) -> Result<String, FormulaValidationError> {
    let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let fraction = fraction.trim_end_matches('0');
    let canonical = if fraction.is_empty() {
        integer.to_string()
    } else {
        format!("{integer}.{fraction}")
    };
    Ok(canonical)
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn new(input: &str) -> Result<Self, FormulaValidationError> {
        Ok(Self {
            tokens: tokenize(input)?,
            index: 0,
        })
    }

    fn parse_expression_complete(mut self) -> Result<FormulaExpr, FormulaValidationError> {
        let expression = self.parse_comparison()?;
        if self.peek().is_some() {
            return Err(INVALID);
        }
        Ok(expression)
    }

    fn parse_reference_complete(mut self) -> Result<FormulaExpr, FormulaValidationError> {
        let reference = self.parse_reference()?;
        if self.peek().is_some() {
            return Err(INVALID);
        }
        Ok(reference)
    }

    fn parse_comparison(&mut self) -> Result<FormulaExpr, FormulaValidationError> {
        let left = self.parse_additive()?;
        let op = match self.peek() {
            Some(Token::Equal) => Some(BinaryOp::Equal),
            Some(Token::NotEqual) => Some(BinaryOp::NotEqual),
            Some(Token::Less) => Some(BinaryOp::Less),
            Some(Token::LessOrEqual) => Some(BinaryOp::LessOrEqual),
            Some(Token::Greater) => Some(BinaryOp::Greater),
            Some(Token::GreaterOrEqual) => Some(BinaryOp::GreaterOrEqual),
            _ => None,
        };
        let Some(op) = op else {
            return Ok(left);
        };
        self.index += 1;
        let right = self.parse_additive()?;
        if matches!(
            self.peek(),
            Some(
                Token::Equal
                    | Token::NotEqual
                    | Token::Less
                    | Token::LessOrEqual
                    | Token::Greater
                    | Token::GreaterOrEqual
            )
        ) {
            return Err(INVALID);
        }
        Ok(FormulaExpr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    fn parse_additive(&mut self) -> Result<FormulaExpr, FormulaValidationError> {
        let mut value = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinaryOp::Add,
                Some(Token::Minus) => BinaryOp::Subtract,
                _ => return Ok(value),
            };
            self.index += 1;
            let right = self.parse_multiplicative()?;
            value = FormulaExpr::Binary {
                op,
                left: Box::new(value),
                right: Box::new(right),
            };
        }
    }

    fn parse_multiplicative(&mut self) -> Result<FormulaExpr, FormulaValidationError> {
        let mut value = self.parse_power()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinaryOp::Multiply,
                Some(Token::Slash) => BinaryOp::Divide,
                _ => return Ok(value),
            };
            self.index += 1;
            let right = self.parse_power()?;
            value = FormulaExpr::Binary {
                op,
                left: Box::new(value),
                right: Box::new(right),
            };
        }
    }

    fn parse_power(&mut self) -> Result<FormulaExpr, FormulaValidationError> {
        let left = self.parse_unary()?;
        if !matches!(self.peek(), Some(Token::Caret)) {
            return Ok(left);
        }
        self.index += 1;
        Ok(FormulaExpr::Binary {
            op: BinaryOp::Power,
            left: Box::new(left),
            right: Box::new(self.parse_power()?),
        })
    }

    fn parse_unary(&mut self) -> Result<FormulaExpr, FormulaValidationError> {
        let op = match self.peek() {
            Some(Token::Plus) => Some(UnaryOp::Plus),
            Some(Token::Minus) => Some(UnaryOp::Minus),
            _ => None,
        };
        let Some(op) = op else {
            return self.parse_primary();
        };
        self.index += 1;
        Ok(FormulaExpr::Unary {
            op,
            value: Box::new(self.parse_unary()?),
        })
    }

    fn parse_primary(&mut self) -> Result<FormulaExpr, FormulaValidationError> {
        match self.peek().cloned().ok_or(INVALID)? {
            Token::Number(canonical) => {
                self.index += 1;
                Ok(FormulaExpr::Number { canonical })
            }
            Token::LParen => {
                self.index += 1;
                let value = self.parse_comparison()?;
                self.expect(Token::RParen)?;
                Ok(value)
            }
            Token::Ident(identifier)
                if matches!(self.tokens.get(self.index + 1), Some(Token::LParen)) =>
            {
                self.parse_function(&identifier)
            }
            Token::Ident(identifier) if identifier.eq_ignore_ascii_case("TRUE") => {
                self.index += 1;
                Ok(FormulaExpr::Boolean { value: true })
            }
            Token::Ident(identifier) if identifier.eq_ignore_ascii_case("FALSE") => {
                self.index += 1;
                Ok(FormulaExpr::Boolean { value: false })
            }
            Token::Ident(_) | Token::QuotedSheet(_) => self.parse_reference(),
            _ => Err(INVALID),
        }
    }

    fn parse_function(&mut self, identifier: &str) -> Result<FormulaExpr, FormulaValidationError> {
        let name = match identifier.to_ascii_uppercase().as_str() {
            "SUM" => FormulaFunction::Sum,
            "AVERAGE" => FormulaFunction::Average,
            "MIN" => FormulaFunction::Min,
            "MAX" => FormulaFunction::Max,
            "COUNT" => FormulaFunction::Count,
            "IF" => FormulaFunction::If,
            "ABS" => FormulaFunction::Abs,
            "ROUND" => FormulaFunction::Round,
            _ => return Err(INVALID),
        };
        self.index += 1;
        self.expect(Token::LParen)?;
        let mut args = Vec::new();
        if !matches!(self.peek(), Some(Token::RParen)) {
            loop {
                if args.len() >= 32 {
                    return Err(INVALID);
                }
                args.push(self.parse_comparison()?);
                if !matches!(self.peek(), Some(Token::Comma)) {
                    break;
                }
                self.index += 1;
            }
        }
        self.expect(Token::RParen)?;
        let valid_arity = match name {
            FormulaFunction::Sum
            | FormulaFunction::Average
            | FormulaFunction::Min
            | FormulaFunction::Max
            | FormulaFunction::Count => (1..=32).contains(&args.len()),
            FormulaFunction::If => args.len() == 3,
            FormulaFunction::Abs => args.len() == 1,
            FormulaFunction::Round => args.len() == 2,
        };
        if !valid_arity {
            return Err(INVALID);
        }
        Ok(FormulaExpr::Function { name, args })
    }

    fn parse_reference(&mut self) -> Result<FormulaExpr, FormulaValidationError> {
        let mut sheet = None;
        if matches!(
            (self.peek(), self.tokens.get(self.index + 1)),
            (
                Some(Token::Ident(_) | Token::QuotedSheet(_)),
                Some(Token::Bang)
            )
        ) {
            sheet = Some(match self.next().ok_or(INVALID)? {
                Token::Ident(value) | Token::QuotedSheet(value) => value,
                _ => return Err(INVALID),
            });
            self.expect(Token::Bang)?;
        }
        let first = match self.next().ok_or(INVALID)? {
            Token::Ident(value) => parse_cell(&value, sheet.clone())?,
            _ => return Err(INVALID),
        };
        if !matches!(self.peek(), Some(Token::Colon)) {
            return Ok(FormulaExpr::Cell { reference: first });
        }
        self.index += 1;
        let mut second_sheet = None;
        if matches!(
            (self.peek(), self.tokens.get(self.index + 1)),
            (
                Some(Token::Ident(_) | Token::QuotedSheet(_)),
                Some(Token::Bang)
            )
        ) {
            second_sheet = Some(match self.next().ok_or(INVALID)? {
                Token::Ident(value) | Token::QuotedSheet(value) => value,
                _ => return Err(INVALID),
            });
            self.expect(Token::Bang)?;
        }
        if sheet.is_some() && second_sheet.is_some() && sheet != second_sheet {
            return Err(INVALID);
        }
        let effective_sheet = second_sheet.or(sheet);
        let second = match self.next().ok_or(INVALID)? {
            Token::Ident(value) => parse_cell(&value, effective_sheet.clone())?,
            _ => return Err(INVALID),
        };
        let mut first = first;
        if first.sheet.is_none() {
            first.sheet = effective_sheet;
        }
        Ok(FormulaExpr::Range {
            start: first,
            end: second,
        })
    }

    fn expect(&mut self, expected: Token) -> Result<(), FormulaValidationError> {
        if self.peek() != Some(&expected) {
            return Err(INVALID);
        }
        self.index += 1;
        Ok(())
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.index).cloned();
        self.index += usize::from(token.is_some());
        token
    }
}

fn parse_cell(value: &str, sheet: Option<String>) -> Result<CellRef, FormulaValidationError> {
    let bytes = value.as_bytes();
    let mut index = 0;
    let absolute_column = bytes.get(index) == Some(&b'$');
    index += usize::from(absolute_column);
    let column_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
        index += 1;
    }
    if index == column_start || index - column_start > 3 {
        return Err(INVALID);
    }
    let absolute_row = bytes.get(index) == Some(&b'$');
    index += usize::from(absolute_row);
    let row_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if index != bytes.len() || index == row_start {
        return Err(INVALID);
    }
    let mut column = 0u32;
    for byte in
        &bytes[column_start..column_start + (row_start - column_start - usize::from(absolute_row))]
    {
        column = column
            .checked_mul(26)
            .and_then(|value| value.checked_add((byte.to_ascii_uppercase() - b'A' + 1) as u32))
            .ok_or(INVALID)?;
    }
    let row = value[row_start..].parse::<u32>().map_err(|_| INVALID)?;
    if column == 0 || column > 16_384 || row == 0 || row > 1_048_576 {
        return Err(INVALID);
    }
    Ok(CellRef {
        sheet,
        column: column as u16,
        row,
        absolute_column,
        absolute_row,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheets() -> Vec<String> {
        vec!["Sheet1".into(), "Sales Q1".into()]
    }

    #[test]
    fn accepts_closed_arithmetic_aggregate_comparison_and_same_workbook_refs() {
        for formula in [
            "=A1+B2*2",
            "=SUM(A1:A10,ROUND(B1,2))",
            "=IF('Sales Q1'!$A$1>=10,ABS(B2),0)",
            "=AVERAGE(Sheet1!A1:B4)",
            "=TRUE",
        ] {
            let validated =
                validate_formula_patch(formula, "Sheet1!C2:C4", FORMULA_LOCALE_V1, &sheets())
                    .unwrap_or_else(|error| panic!("{formula}: {error}"));
            assert_eq!(validated.ast_digest_sha256.len(), 64);
        }
    }

    #[test]
    fn digest_is_ast_locale_and_target_bound_but_stable_for_numeric_spelling() {
        let first = validate_formula_patch("=01.500+A1", "Sheet1!B1", FORMULA_LOCALE_V1, &sheets())
            .unwrap();
        let same =
            validate_formula_patch("=1.5+A1", "Sheet1!B1", FORMULA_LOCALE_V1, &sheets()).unwrap();
        let other_target =
            validate_formula_patch("=1.5+A1", "Sheet1!B2", FORMULA_LOCALE_V1, &sheets()).unwrap();
        assert_eq!(first.ast_digest_sha256, same.ast_digest_sha256);
        assert_ne!(first.ast_digest_sha256, other_target.ast_digest_sha256);
    }

    #[test]
    fn rejects_external_dynamic_unknown_and_ambiguous_formula_surfaces() {
        for formula in [
            "=[Book.xlsx]Sheet1!A1",
            "=WEBSERVICE(\"https://example.com\")",
            "=FILTERXML(A1,B1)",
            "=RTD(A1,B1)",
            "=HYPERLINK(A1)",
            "=INDIRECT(A1)",
            "=DDE(A1)",
            "=A:A",
            "=Table1[Revenue]",
            "=SUM(A1;A2)",
            "=SUM()",
            "=IF(A1,1)",
            "=A1<B1<C1",
            "==A1",
            "=A0",
            "=XFE1",
            "=A1048577",
        ] {
            assert!(
                validate_formula_patch(formula, "Sheet1!B1", FORMULA_LOCALE_V1, &sheets()).is_err(),
                "unsafe formula accepted: {formula}"
            );
        }
        assert!(
            validate_formula_patch("=Other!A1", "Sheet1!B1", FORMULA_LOCALE_V1, &sheets()).is_err()
        );
        assert!(validate_formula_patch("=A1", "Sheet1!B1", "de-DE-a1", &sheets()).is_err());
    }

    #[test]
    fn csv_formula_injection_corpus_is_escaped_and_clean_values_are_unchanged() {
        for dangerous in [
            "=1+1",
            "+cmd",
            "-2+3",
            "@SUM(A1:A2)",
            "\t=1",
            "\r=1",
            "\n=1",
        ] {
            let (escaped, transformed) = escape_csv_formula_injection(dangerous);
            assert!(transformed);
            assert_eq!(escaped, format!("'{dangerous}"));
        }
        for clean in ["text", "  =kept-as-text", "42", "", "'already text"] {
            assert_eq!(
                escape_csv_formula_injection(clean),
                (clean.to_string(), false)
            );
        }
    }
}
