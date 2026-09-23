use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::{ConversionError, ConversionWarning, ConversionWarningCode};

const MAX_EVENTS: usize = 100_000;
const MAX_DEPTH: usize = 32;
const MAX_TABLE_ROWS: usize = 2_000;
const MAX_TABLE_COLUMNS: usize = 64;
const MAX_CODE_BLOCK_BYTES: usize = 256 * 1024;
const MAX_URL_BYTES: usize = 2_048;

pub(crate) struct RenderedMarkdown {
    pub typst: String,
    pub warnings: Vec<ConversionWarning>,
}

pub(crate) fn to_typst(source: &str) -> Result<RenderedMarkdown, ConversionError> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let parser = Parser::new_ext(source, options);
    let mut output = String::with_capacity(source.len().saturating_mul(2));
    let mut warnings = Vec::new();
    let mut depth = 0usize;
    let mut events = 0usize;
    let mut code_block = None::<String>;
    let mut in_html_block = false;
    let mut code_bytes = 0usize;
    let mut table_rows = 0usize;
    let mut table_columns = None;

    for event in parser {
        events += 1;
        if events > MAX_EVENTS {
            return Err(limit("Markdown event count exceeds the supported limit"));
        }
        match event {
            Event::Start(tag) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(limit("Markdown nesting exceeds the supported limit"));
                }
                match tag {
                    Tag::Paragraph => output.push_str("#block["),
                    Tag::Heading { level, .. } => {
                        output.push_str("#heading(level: ");
                        output.push_str(heading_number(level));
                        output.push_str(")[");
                    }
                    Tag::BlockQuote(_) => output.push_str("#quote(block: true)["),
                    Tag::CodeBlock(_) => {
                        code_block = Some(String::new());
                        code_bytes = 0;
                    }
                    Tag::List(start) => {
                        if start.is_some() {
                            output.push_str("#enum(");
                        } else {
                            output.push_str("#list(");
                        }
                    }
                    Tag::Item => output.push('['),
                    Tag::Table(alignments) => {
                        if alignments.is_empty() || alignments.len() > MAX_TABLE_COLUMNS {
                            return Err(limit(
                                "Markdown table column count exceeds the supported limit",
                            ));
                        }
                        table_rows = 0;
                        table_columns = Some(alignments.len());
                        output.push_str("#table(columns: ");
                        output.push_str(&alignments.len().to_string());
                        output.push_str(", inset: 4pt,");
                    }
                    Tag::TableHead => {}
                    Tag::TableRow => {
                        table_rows += 1;
                        if table_rows > MAX_TABLE_ROWS {
                            return Err(limit(
                                "Markdown table row count exceeds the supported limit",
                            ));
                        }
                    }
                    Tag::TableCell => output.push('['),
                    Tag::Emphasis => output.push_str("#emph["),
                    Tag::Strong => output.push_str("#strong["),
                    Tag::Strikethrough => output.push_str("#strike["),
                    Tag::Link { dest_url, .. } => {
                        let url = checked_url(&dest_url, &mut warnings);
                        if let Some(url) = url {
                            output.push_str("#link(");
                            push_string(&mut output, url);
                            output.push_str(")[");
                        } else {
                            output.push('[');
                        }
                    }
                    Tag::HtmlBlock => in_html_block = true,
                    Tag::FootnoteDefinition(_)
                    | Tag::DefinitionList
                    | Tag::DefinitionListTitle
                    | Tag::DefinitionListDefinition
                    | Tag::Superscript
                    | Tag::Subscript
                    | Tag::Image { .. }
                    | Tag::MetadataBlock(_) => return Err(unsupported_tag()),
                }
            }
            Event::End(tag) => {
                depth = depth.saturating_sub(1);
                match tag {
                    TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote(_) => {
                        output.push_str("]\n")
                    }
                    TagEnd::CodeBlock => {
                        let code = code_block.take().ok_or_else(|| {
                            ConversionError::new(
                                "invalid_markdown",
                                "Markdown parser returned an unmatched code block",
                            )
                        })?;
                        output.push_str("#raw(");
                        push_string(&mut output, &code);
                        output.push_str(", block: true)\n");
                    }
                    TagEnd::List(_) => output.push_str(")\n"),
                    TagEnd::Item | TagEnd::TableCell => output.push_str("],"),
                    TagEnd::Table => {
                        table_columns = None;
                        output.push_str(")\n");
                    }
                    TagEnd::TableHead | TagEnd::TableRow => {}
                    TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                        output.push(']')
                    }
                    TagEnd::HtmlBlock => in_html_block = false,
                    TagEnd::FootnoteDefinition
                    | TagEnd::DefinitionList
                    | TagEnd::DefinitionListTitle
                    | TagEnd::DefinitionListDefinition
                    | TagEnd::Superscript
                    | TagEnd::Subscript
                    | TagEnd::Image
                    | TagEnd::MetadataBlock(_) => return Err(unsupported_tag()),
                }
            }
            Event::Text(text) => {
                if let Some(code_block) = code_block.as_mut() {
                    code_bytes = code_bytes.saturating_add(text.len());
                    if code_bytes > MAX_CODE_BLOCK_BYTES {
                        return Err(limit("Markdown code block exceeds the supported limit"));
                    }
                    code_block.push_str(&text);
                } else {
                    output.push_str("#text(");
                    push_string(&mut output, &text);
                    output.push(')');
                }
            }
            Event::Code(code) => {
                if code.len() > MAX_CODE_BLOCK_BYTES {
                    return Err(limit("Markdown inline code exceeds the supported limit"));
                }
                output.push_str("#raw(");
                push_string(&mut output, &code);
                output.push(')');
            }
            Event::SoftBreak => output.push_str("#text(\" \")"),
            Event::HardBreak => output.push_str("#linebreak()"),
            Event::Rule => output.push_str("#line(length: 100%)\n"),
            Event::Html(html) | Event::InlineHtml(html) => {
                if in_html_block && exact_page_marker(&html).is_some() {
                    output.push_str("#pagebreak()\n");
                } else {
                    return Err(unsupported_tag());
                }
            }
            Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => return Err(unsupported_tag()),
        }
    }

    if depth != 0 || code_block.is_some() || in_html_block || table_columns.is_some() {
        return Err(ConversionError::new(
            "invalid_markdown",
            "Markdown parser returned an incomplete document",
        ));
    }
    Ok(RenderedMarkdown {
        typst: output,
        warnings,
    })
}

pub(crate) fn text_to_typst(source: &str) -> Result<String, ConversionError> {
    if source
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ConversionError::new(
            "invalid_source_encoding",
            "TXT contains unsupported control characters",
        ));
    }
    let mut output = String::with_capacity(source.len().saturating_add(32));
    output.push_str("#text(");
    push_string(&mut output, source);
    output.push(')');
    Ok(output)
}

fn checked_url<'a>(value: &'a str, warnings: &mut Vec<ConversionWarning>) -> Option<&'a str> {
    let parsed = (value.len() <= MAX_URL_BYTES)
        .then(|| url::Url::parse(value).ok())
        .flatten();
    if parsed
        .as_ref()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https" | "mailto"))
    {
        Some(value)
    } else {
        warnings.push(ConversionWarning {
            code: ConversionWarningCode::LinkDegraded,
            page: None,
            detail: "unsupported Markdown link was rendered as plain text".into(),
        });
        None
    }
}

fn exact_page_marker(value: &str) -> Option<u32> {
    let value = value.trim();
    let number = value
        .strip_prefix("<!-- lcxl-page: ")?
        .strip_suffix(" -->")?;
    if number.is_empty() || number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

fn push_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{{{:x}}}", character as u32);
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

fn heading_number(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "1",
        HeadingLevel::H2 => "2",
        HeadingLevel::H3 => "3",
        HeadingLevel::H4 => "4",
        HeadingLevel::H5 => "5",
        HeadingLevel::H6 => "6",
    }
}

fn unsupported_tag() -> ConversionError {
    ConversionError::new(
        "unsupported_markdown_feature",
        "raw HTML, images, footnotes, math, task markers, metadata, definition lists, superscript, and subscript are not supported",
    )
}

fn limit(message: &'static str) -> ConversionError {
    ConversionError::new("markdown_limit_exceeded", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_is_always_a_typst_string() {
        let rendered = to_typst("# title\n\n`#import \\\"evil\\\"` and #include x").unwrap();
        assert!(rendered.typst.contains("#raw(\"#import"));
        assert!(rendered.typst.contains("#text(\" and #include x\")"));
    }

    #[test]
    fn rejects_html_but_accepts_exact_internal_page_marker() {
        assert!(to_typst("<script>alert(1)</script>").is_err());
        let rendered = to_typst("before\n\n<!-- lcxl-page: 2 -->\n\nafter").unwrap();
        assert!(rendered.typst.contains("#pagebreak()"));
    }

    #[test]
    fn degrades_non_http_links() {
        let rendered = to_typst("[bad](file:///etc/passwd)").unwrap();
        assert_eq!(rendered.warnings.len(), 1);
        assert!(!rendered.typst.contains("file:///etc/passwd"));
    }
}
