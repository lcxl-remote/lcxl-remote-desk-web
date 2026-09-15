//! Inspect ordinary rule/name/table expressions. This does not resolve their scopes
//! or authorize native execution; unsupported grammars must not silently pass.
use crate::{
    ooxml_package::PackageResult,
    ooxml_relationships::{Parts, attributes},
    xlsx_strings,
};
use desk_diagnose_core::spreadsheet_formula::{FORMULA_LOCALE_V1, validate_formula_patch};
use quick_xml::{NsReader, events::Event, name::ResolveResult};
const S: &[u8] = b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const MAX_RULES: usize = 512;
const MAX_BYTES: usize = 128 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct RuleFormula {
    pub part: String,
    pub element: String,
    pub expression: String,
}

fn expected_parent(name: &[u8]) -> Option<&'static [u8]> {
    match name {
        b"definedName" => Some(b"definedNames"),
        b"formula" => Some(b"cfRule"),
        b"formula1" | b"formula2" => Some(b"dataValidation"),
        b"calculatedColumnFormula" | b"totalsRowFormula" => Some(b"tableColumn"),
        _ => None,
    }
}

pub(crate) fn inspect(parts: &Parts, sheets: &[String]) -> PackageResult<Vec<RuleFormula>> {
    let mut result = Vec::new();
    let mut total = 0;
    // Scan every XML part, including nonstandard part paths and orphaned parts.
    // Relationship/type authorization is a separate prerequisite for native open.
    for (path, bytes) in parts
        .iter()
        .filter(|(path, _)| path.to_ascii_lowercase().ends_with(".xml"))
    {
        let mut reader = NsReader::from_reader(bytes.as_slice());
        reader.config_mut().expand_empty_elements = true;
        let mut stack: Vec<Vec<u8>> = Vec::new();
        let mut active: Option<(String, String, usize)> = None;
        loop {
            let (ns, event) = reader.read_resolved_event()?;
            let spreadsheet = matches!(ns, ResolveResult::Bound(ref n) if n.as_ref() == S);
            match event {
                Event::Start(element) => {
                    if active.is_some() {
                        return Err("nested rule formula markup".into());
                    }
                    let local = element.local_name();
                    if let Some(parent) = expected_parent(local.as_ref()) {
                        if !spreadsheet || stack.last().map(Vec::as_slice) != Some(parent) {
                            return Err("unsupported or misplaced rule formula".into());
                        }
                        let attrs = attributes(&reader, &element)?;
                        for flag in ["function", "vbProcedure", "xlm", "array"] {
                            if attrs
                                .get(flag)
                                .is_some_and(|value| !matches!(value.as_str(), "0" | "false"))
                            {
                                return Err("active or array rule formula is unsupported".into());
                            }
                        }
                        active = Some((
                            std::str::from_utf8(local.as_ref())?.to_owned(),
                            String::new(),
                            stack.len(),
                        ));
                    }
                    stack.push(if spreadsheet {
                        local.as_ref().to_vec()
                    } else {
                        Vec::new()
                    });
                }
                Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if active.is_some() => {
                    let text = xlsx_strings::event_text(event)?;
                    let (_, expression, _) = active.as_mut().ok_or("missing rule formula")?;
                    expression.push_str(&text);
                    if expression.len() >= 4096 {
                        return Err("rule formula exceeds frozen policy limit".into());
                    }
                }
                Event::End(_) => {
                    stack.pop().ok_or("invalid rule formula nesting")?;
                    if active
                        .as_ref()
                        .is_some_and(|(_, _, depth)| *depth == stack.len())
                    {
                        let (element, expression, _) =
                            active.take().ok_or("missing rule formula")?;
                        // A1 is only a syntax-policy probe. No target grant or digest
                        // is derived from this synthetic address or returned to callers.
                        validate_formula_patch(
                            &format!("={expression}"),
                            "A1",
                            FORMULA_LOCALE_V1,
                            sheets,
                        )?;
                        total += expression.len();
                        if result.len() >= MAX_RULES || total > MAX_BYTES {
                            return Err("rule formula inventory exceeds limit".into());
                        }
                        result.push(RuleFormula {
                            part: path.clone(),
                            element,
                            expression,
                        });
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if !stack.is_empty() || active.is_some() {
            return Err("incomplete rule formula XML".into());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parts(body: &str) -> Parts {
        [(
            "custom/rules.xml".into(),
            format!(
                "<worksheet xmlns=\"{}\">{body}</worksheet>",
                std::str::from_utf8(S).unwrap()
            )
            .into_bytes(),
        )]
        .into()
    }
    #[test]
    fn discovers_names_validation_conditional_and_table_formulas() {
        let parts = parts(
            "<definedNames><definedName name=\"amount\">Sheet1!A1</definedName></definedNames><conditionalFormatting><cfRule><formula>A1&lt;10</formula></cfRule></conditionalFormatting><dataValidations><dataValidation><formula1>1</formula1><formula2>10</formula2></dataValidation></dataValidations><tableColumns><tableColumn><calculatedColumnFormula>SUM(A1:A3)</calculatedColumnFormula><totalsRowFormula>SUM(A1:A3)</totalsRowFormula></tableColumn></tableColumns>",
        );
        let before = parts.clone();
        let result = inspect(&parts, &["Sheet1".into()]).unwrap();
        assert_eq!(result.len(), 6);
        assert_eq!(result[1].expression, "A1<10");
        assert_eq!(result[5].element, "totalsRowFormula");
        assert!(result.iter().all(|r| r.part == "custom/rules.xml"));
        assert_eq!(parts, before);
    }
    #[test]
    fn rejects_unsafe_misplaced_and_extension_rule_formulas() {
        for body in [
            "<definedNames><definedName>WEBSERVICE(1)</definedName></definedNames>",
            "<definedNames><definedName xlm=\"1\">1</definedName></definedNames>",
            "<cfRule><formula>Other!A1</formula></cfRule>",
            "<formula>1</formula>",
            "<dataValidation><formula1 xmlns=\"urn:extension\">1</formula1></dataValidation>",
            "<tableColumn><calculatedColumnFormula array=\"1\">1</calculatedColumnFormula></tableColumn>",
            "<dataValidation><formula1><x/></formula1></dataValidation>",
        ] {
            assert!(inspect(&parts(body), &["Sheet1".into()]).is_err(), "{body}");
        }
    }
}
