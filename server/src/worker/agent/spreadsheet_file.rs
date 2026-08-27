//! Bounded, inert spreadsheet projections from exact owner-selected files.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::computer_use::{
    SpreadsheetCellKind, SpreadsheetCellProjection, SpreadsheetFileInspectOutput,
    SpreadsheetFileInspectParams, SpreadsheetMergePreviewOutput, SpreadsheetMergePreviewParams,
    SpreadsheetNamedValue, SpreadsheetRowSource, SpreadsheetSheetProjection,
    SpreadsheetStatisticOperation, SpreadsheetStatisticResult, SpreadsheetWorkbookProjection,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use zip::ZipArchive;

use super::file_reference_store::read_verified_bytes;

const MAX_WORKBOOK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 512;
const MAX_XML_BYTES: u64 = 4 * 1024 * 1024;
const PREVIEW_TTL_SECS: i64 = 5 * 60;
const MAX_RETAINED_PREVIEWS: usize = 32;

#[derive(Clone)]
struct StoredMergePreview {
    expires_at: DateTime<Utc>,
    output: SpreadsheetMergePreviewOutput,
}

fn preview_store() -> &'static Mutex<HashMap<String, StoredMergePreview>> {
    static STORE: OnceLock<Mutex<HashMap<String, StoredMergePreview>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn reset_preview_store() {
    if let Ok(mut previews) = preview_store().lock() {
        previews.clear();
    }
}

fn retain_preview(output: &SpreadsheetMergePreviewOutput) -> Result<(), AgentError> {
    let now = Utc::now();
    let mut previews = preview_store()
        .lock()
        .map_err(|_| internal("lock spreadsheet preview store"))?;
    previews.retain(|_, preview| preview.expires_at > now);
    if previews.len() >= MAX_RETAINED_PREVIEWS {
        let oldest = previews
            .iter()
            .min_by_key(|(_, preview)| preview.expires_at)
            .map(|(preview_id, _)| preview_id.clone());
        if let Some(oldest) = oldest {
            previews.remove(&oldest);
        }
    }
    previews.insert(
        output.preview_id.clone(),
        StoredMergePreview {
            expires_at: now + Duration::seconds(PREVIEW_TTL_SECS),
            output: output.clone(),
        },
    );
    Ok(())
}

fn load_preview(preview_id: &str) -> Result<SpreadsheetMergePreviewOutput, AgentError> {
    if preview_id.is_empty() || preview_id.len() > 128 {
        return Err(invalid("spreadsheet preview id is invalid"));
    }
    let now = Utc::now();
    let mut previews = preview_store()
        .lock()
        .map_err(|_| internal("lock spreadsheet preview store"))?;
    previews.retain(|_, preview| preview.expires_at > now);
    previews
        .get(preview_id)
        .map(|preview| preview.output.clone())
        .ok_or_else(|| invalid("spreadsheet merge preview is stale or unknown"))
}

pub fn inspect(
    params: &SpreadsheetFileInspectParams,
) -> Result<SpreadsheetFileInspectOutput, AgentError> {
    if params.files.is_empty()
        || params.files.len() > 8
        || params.max_workbooks == 0
        || params.max_workbooks > 8
        || params.max_sheets == 0
        || params.max_sheets > 32
        || params.max_rows == 0
        || params.max_rows > 512
        || params.max_columns == 0
        || params.max_columns > 128
        || params.max_bytes < 1024
        || params.max_bytes > 1024 * 1024
    {
        return Err(invalid(
            "spreadsheet projection bounds exceed the frozen ceiling",
        ));
    }

    let mut workbooks = Vec::new();
    let mut truncated = false;
    for file in params.files.iter().take(params.max_workbooks as usize) {
        let selected = read_verified_bytes(file, MAX_WORKBOOK_BYTES)?;
        let lower = selected.display_name.to_ascii_lowercase();
        let workbook = if lower.ends_with(".xlsx") {
            inspect_xlsx(
                selected.display_name,
                selected.bytes,
                selected.sha256,
                params,
            )?
        } else if lower.ends_with(".csv") || lower.ends_with(".tsv") {
            inspect_delimited(
                selected.display_name,
                selected.bytes,
                selected.sha256,
                params,
            )?
        } else {
            return Err(invalid(
                "selected spreadsheet must be an inert .xlsx, .csv, or .tsv file",
            ));
        };
        workbooks.push(workbook);
        if serde_json::to_vec(&workbooks)
            .map_err(|_| internal("encode spreadsheet projection"))?
            .len()
            > params.max_bytes as usize
        {
            workbooks.pop();
            truncated = true;
            break;
        }
    }
    truncated |= params.files.len() > workbooks.len();
    Ok(SpreadsheetFileInspectOutput {
        snapshot_id: format!("spreadsheet-selection-{}", uuid::Uuid::new_v4()),
        workbooks,
        truncated,
    })
}

pub fn preview_merge(
    params: &SpreadsheetMergePreviewParams,
) -> Result<SpreadsheetMergePreviewOutput, AgentError> {
    validate_merge_params(params)?;
    let projection = inspect(&SpreadsheetFileInspectParams {
        files: params.files.clone(),
        max_workbooks: 8,
        max_sheets: 32,
        max_rows: 512,
        max_columns: 128,
        max_bytes: 1024 * 1024,
    })?;
    let columns = params
        .columns
        .iter()
        .map(|column| column.output_header.clone())
        .collect::<Vec<_>>();
    let column_indexes = columns
        .iter()
        .enumerate()
        .map(|(index, name)| (name.to_ascii_lowercase(), index))
        .collect::<HashMap<_, _>>();
    let mut warnings = BTreeSet::new();
    let mut merged = Vec::<(Vec<String>, SpreadsheetRowSource)>::new();
    let mut truncated = projection.truncated;
    for workbook in &projection.workbooks {
        let sheet = match &params.source_sheet {
            Some(name) => workbook
                .sheets
                .iter()
                .find(|sheet| sheet.name.eq_ignore_ascii_case(name)),
            None => workbook.sheets.first(),
        };
        let Some(sheet) = sheet else {
            warnings.insert(format!(
                "{}: requested source sheet is absent",
                workbook.display_name
            ));
            continue;
        };
        truncated |= sheet.truncated;
        let cells = sheet
            .cells
            .iter()
            .map(|cell| ((cell.row, cell.column), cell))
            .collect::<HashMap<_, _>>();
        let headers = (1..=sheet.observed_columns.min(128))
            .filter_map(|column| {
                cells
                    .get(&(params.header_row, column))
                    .map(|cell| (cell.value.trim().to_ascii_lowercase(), column))
            })
            .filter(|(header, _)| !header.is_empty())
            .collect::<HashMap<_, _>>();
        let resolved = params
            .columns
            .iter()
            .map(|rule| {
                let resolved = rule
                    .source_headers
                    .iter()
                    .find_map(|alias| headers.get(&alias.trim().to_ascii_lowercase()).copied());
                if resolved.is_none() {
                    warnings.insert(format!(
                        "{} / {}: no source header matched output column {}",
                        workbook.display_name, sheet.name, rule.output_header
                    ));
                }
                resolved
            })
            .collect::<Vec<_>>();
        for row in (params.header_row + 1)..=sheet.observed_rows.min(512) {
            let values = resolved
                .iter()
                .map(|column| {
                    column
                        .and_then(|column| cells.get(&(row, column)))
                        .map(|cell| cell.value.clone())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>();
            if values.iter().all(String::is_empty) {
                continue;
            }
            merged.push((
                values,
                SpreadsheetRowSource {
                    workbook_sha256: workbook.sha256.clone(),
                    sheet_name: sheet.name.clone(),
                    source_row: row,
                },
            ));
        }
    }

    let dedupe_indexes = params
        .dedupe_keys
        .iter()
        .map(|key| column_indexes[&key.to_ascii_lowercase()])
        .collect::<Vec<_>>();
    let mut duplicate_rows_removed = 0u32;
    if !dedupe_indexes.is_empty() {
        let mut seen = HashSet::new();
        merged.retain(|(values, _)| {
            let key = dedupe_indexes
                .iter()
                .map(|index| {
                    let value = values[*index].trim().to_ascii_lowercase();
                    format!("{}:{value}", value.len())
                })
                .collect::<Vec<_>>()
                .join("|");
            if seen.insert(key) {
                true
            } else {
                duplicate_rows_removed = duplicate_rows_removed.saturating_add(1);
                false
            }
        });
    }
    let statistics = compute_statistics(params, &column_indexes, &merged)?;
    warnings.insert(
        "statistics use bounded decimal text; averages round half away from zero to at least 6 fractional places; formulas are never recalculated"
            .into(),
    );

    let max_rows = params.max_rows as usize;
    truncated |= merged.len() > max_rows;
    let mut rows = merged
        .iter()
        .take(max_rows)
        .map(|(values, _)| values.clone())
        .collect::<Vec<_>>();
    let mut lineage = merged
        .iter()
        .take(max_rows)
        .map(|(_, source)| source.clone())
        .collect::<Vec<_>>();
    while serde_json::to_vec(&(&columns, &rows, &lineage, &statistics))
        .map_err(|_| internal("encode spreadsheet merge preview"))?
        .len()
        > params.max_bytes as usize
    {
        if rows.pop().is_none() {
            return Err(invalid(
                "spreadsheet merge preview metadata exceeds the output ceiling",
            ));
        }
        lineage.pop();
        truncated = true;
    }
    let output = SpreadsheetMergePreviewOutput {
        preview_id: format!("spreadsheet-merge-preview-{}", uuid::Uuid::new_v4()),
        input_digests_sha256: projection
            .workbooks
            .iter()
            .map(|workbook| workbook.sha256.clone())
            .collect(),
        columns,
        rows,
        lineage,
        statistics,
        duplicate_rows_removed,
        warnings: warnings.into_iter().collect(),
        truncated,
    };
    retain_preview(&output)?;
    Ok(output)
}

pub fn materialize_preview_xlsx(preview_id: &str) -> Result<Vec<u8>, AgentError> {
    let preview = load_preview(preview_id)?;
    if preview.truncated {
        return Err(invalid(
            "a truncated spreadsheet preview cannot be materialized",
        ));
    }
    if preview.columns.is_empty()
        || preview.columns.len() > 64
        || preview.rows.len() > 1000
        || preview
            .rows
            .iter()
            .any(|row| row.len() != preview.columns.len())
        || preview.lineage.len() != preview.rows.len()
    {
        return Err(invalid(
            "spreadsheet preview shape is not safe to materialize",
        ));
    }

    let mut merged_rows = Vec::with_capacity(preview.rows.len() + 1);
    merged_rows.push(preview.columns.clone());
    merged_rows.extend(preview.rows.clone());
    let mut statistic_rows = vec![vec![
        "Operation".into(),
        "Column".into(),
        "Group".into(),
        "Value".into(),
        "Row Count".into(),
        "Skipped Non-Numeric".into(),
    ]];
    statistic_rows.extend(preview.statistics.iter().map(|statistic| {
        vec![
            format!("{:?}", statistic.operation).to_ascii_lowercase(),
            statistic.column.clone().unwrap_or_default(),
            statistic
                .group
                .iter()
                .map(|item| format!("{}={}", item.name, item.value))
                .collect::<Vec<_>>()
                .join("; "),
            statistic.value.clone(),
            statistic.row_count.to_string(),
            statistic.skipped_non_numeric.to_string(),
        ]
    }));

    let entries = [
        ("[Content_Types].xml", content_types_xml()),
        ("_rels/.rels", package_relationships_xml()),
        ("xl/workbook.xml", workbook_xml()),
        ("xl/_rels/workbook.xml.rels", workbook_relationships_xml()),
        ("xl/styles.xml", styles_xml()),
        ("xl/worksheets/sheet1.xml", worksheet_xml(&merged_rows)?),
        ("xl/worksheets/sheet2.xml", worksheet_xml(&statistic_rows)?),
    ];
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, contents) in entries {
        writer
            .start_file(name, options)
            .map_err(|_| internal("start XLSX package entry"))?;
        writer
            .write_all(contents.as_bytes())
            .map_err(|_| internal("write XLSX package entry"))?;
    }
    let bytes = writer
        .finish()
        .map_err(|_| internal("finish XLSX package"))?
        .into_inner();
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(invalid("materialized XLSX exceeds the artifact ceiling"));
    }
    Ok(bytes)
}

/// Materialize the same retained preview as a new XLSX copy with exactly one
/// AST-approved formula cell. This is an inert package writer: it does not
/// start Excel, evaluate the formula, or accept caller-provided OOXML.
pub fn materialize_preview_formula_xlsx(
    preview_id: &str,
    target_cell: &str,
    formula: &str,
    locale: &str,
    expected_policy_digest_sha256: &str,
) -> Result<Vec<u8>, AgentError> {
    let preview = load_preview(preview_id)?;
    if preview.truncated
        || preview.columns.is_empty()
        || preview.columns.len() >= 64
        || preview.rows.is_empty()
        || preview.rows.len() > 1000
        || preview
            .rows
            .iter()
            .any(|row| row.len() != preview.columns.len())
        || preview.lineage.len() != preview.rows.len()
    {
        return Err(invalid(
            "spreadsheet preview shape is not safe for a formula workbook copy",
        ));
    }
    let validated = desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
        formula,
        target_cell,
        locale,
        &["Merged".into(), "Statistics".into()],
    )
    .map_err(|error| invalid(error.message()))?;
    if validated.ast_digest_sha256 != expected_policy_digest_sha256 {
        return Err(invalid(
            "spreadsheet formula policy digest does not match the sealed action",
        ));
    }
    let desk_diagnose_core::spreadsheet_formula::FormulaExpr::Cell { reference } =
        &validated.target
    else {
        return Err(invalid("batch formula target must be one cell"));
    };
    let expected_column = u16::try_from(preview.columns.len() + 1)
        .map_err(|_| invalid("formula target column exceeds the workbook limit"))?;
    let max_row = u32::try_from(preview.rows.len() + 1)
        .map_err(|_| invalid("formula target row exceeds the workbook limit"))?;
    if reference.sheet.as_deref() != Some("Merged")
        || reference.column != expected_column
        || !(2..=max_row).contains(&reference.row)
    {
        return Err(invalid(
            "formula target must be the first empty Merged column on one retained data row",
        ));
    }

    let mut merged_rows = Vec::with_capacity(preview.rows.len() + 1);
    let mut header = preview.columns.clone();
    header.push("AI Formula".into());
    merged_rows.push(header);
    merged_rows.extend(preview.rows.iter().cloned().map(|mut row| {
        row.push(String::new());
        row
    }));
    let mut statistic_rows = vec![vec![
        "Operation".into(),
        "Column".into(),
        "Group".into(),
        "Value".into(),
        "Row Count".into(),
        "Skipped Non-Numeric".into(),
    ]];
    statistic_rows.extend(preview.statistics.iter().map(|statistic| {
        vec![
            format!("{:?}", statistic.operation).to_ascii_lowercase(),
            statistic.column.clone().unwrap_or_default(),
            statistic
                .group
                .iter()
                .map(|item| format!("{}={}", item.name, item.value))
                .collect::<Vec<_>>()
                .join("; "),
            statistic.value.clone(),
            statistic.row_count.to_string(),
            statistic.skipped_non_numeric.to_string(),
        ]
    }));
    let formula_text = formula
        .strip_prefix('=')
        .ok_or_else(|| invalid("formula must start with equals"))?;
    let entries = [
        ("[Content_Types].xml", content_types_xml()),
        ("_rels/.rels", package_relationships_xml()),
        ("xl/workbook.xml", formula_workbook_xml()),
        ("xl/_rels/workbook.xml.rels", workbook_relationships_xml()),
        ("xl/styles.xml", styles_xml()),
        (
            "xl/worksheets/sheet1.xml",
            worksheet_xml_with_formula(
                &merged_rows,
                usize::try_from(reference.row).map_err(|_| invalid("formula row overflow"))?,
                usize::from(reference.column),
                formula_text,
            )?,
        ),
        ("xl/worksheets/sheet2.xml", worksheet_xml(&statistic_rows)?),
    ];
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, contents) in entries {
        writer
            .start_file(name, options)
            .map_err(|_| internal("start formula XLSX package entry"))?;
        writer
            .write_all(contents.as_bytes())
            .map_err(|_| internal("write formula XLSX package entry"))?;
    }
    let bytes = writer
        .finish()
        .map_err(|_| internal("finish formula XLSX package"))?
        .into_inner();
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(invalid(
            "materialized formula XLSX exceeds the artifact ceiling",
        ));
    }
    let mut package = ZipArchive::new(Cursor::new(bytes.as_slice()))
        .map_err(|_| internal("reopen formula XLSX package"))?;
    let mut sheet_xml = String::new();
    package
        .by_name("xl/worksheets/sheet1.xml")
        .map_err(|_| internal("reopen generated formula worksheet"))?
        .read_to_string(&mut sheet_xml)
        .map_err(|_| internal("read generated formula worksheet"))?;
    let address = format!(
        "{}{}",
        column_name(usize::from(reference.column))?,
        reference.row
    );
    let expected = format!(
        r#"<c r="{address}"><f>{}</f></c>"#,
        escape_xml(formula_text)
    );
    if !sheet_xml.contains(&expected) || sheet_xml.matches("<f>").count() != 1 {
        return Err(internal(
            "generated formula worksheet did not read back as exactly one sealed formula cell",
        ));
    }
    drop(package);
    Ok(bytes)
}

/// Build a deterministic Word business report from the same immutable preview
/// that backs XLSX creation. No caller-controlled OOXML, relationship, field,
/// macro, image, or embedded object is accepted by this boundary.
pub fn materialize_preview_docx(preview_id: &str, title: &str) -> Result<Vec<u8>, AgentError> {
    let preview = load_preview(preview_id)?;
    let title = title.trim();
    if title.is_empty()
        || title.chars().count() > 160
        || !is_safe_word_text(title)
        || preview.truncated
        || preview.columns.is_empty()
        || preview.columns.len() > 16
        || preview.rows.len() > 250
        || preview
            .rows
            .iter()
            .any(|row| row.len() != preview.columns.len())
        || preview.lineage.len() != preview.rows.len()
        || preview.statistics.len() > 64
    {
        return Err(invalid(
            "spreadsheet preview or report title is not safe to materialize as DOCX",
        ));
    }
    if preview
        .columns
        .iter()
        .chain(preview.rows.iter().flatten())
        .chain(preview.warnings.iter())
        .any(|value| value.chars().count() > 8_192 || !is_safe_word_text(value))
    {
        return Err(invalid("report text exceeds the safe DOCX boundary"));
    }

    let document = word_document_xml(&preview, title)?;
    let entries = [
        ("[Content_Types].xml", word_content_types_xml()),
        ("_rels/.rels", word_package_relationships_xml()),
        ("docProps/core.xml", word_core_properties_xml(title)),
        ("docProps/app.xml", word_app_properties_xml()),
        ("word/document.xml", document),
        ("word/styles.xml", word_styles_xml()),
        ("word/settings.xml", word_settings_xml()),
        (
            "word/_rels/document.xml.rels",
            word_document_relationships_xml(),
        ),
    ];
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, contents) in entries {
        writer
            .start_file(name, options)
            .map_err(|_| internal("start DOCX package entry"))?;
        writer
            .write_all(contents.as_bytes())
            .map_err(|_| internal("write DOCX package entry"))?;
    }
    let bytes = writer
        .finish()
        .map_err(|_| internal("finish DOCX package"))?
        .into_inner();
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(invalid("materialized DOCX exceeds the artifact ceiling"));
    }
    Ok(bytes)
}

fn is_safe_word_text(value: &str) -> bool {
    value.chars().all(|character| {
        matches!(character, '\t' | '\n' | '\r') || (character >= '\u{20}' && character != '\u{7f}')
    })
}

fn word_document_xml(
    preview: &SpreadsheetMergePreviewOutput,
    title: &str,
) -> Result<String, AgentError> {
    let mut body = String::new();
    word_paragraph(&mut body, title, Some("Title"), false);
    word_paragraph(
        &mut body,
        "AI Assistant / Structured Data Brief",
        Some("Subtitle"),
        false,
    );
    word_paragraph(&mut body, "Executive Summary", Some("Heading1"), false);
    word_paragraph(
        &mut body,
        &format!(
            "This report consolidates {} data rows from {} immutable source workbook(s). {} duplicate row(s) were removed before reporting.",
            preview.rows.len(),
            preview.input_digests_sha256.len(),
            preview.duplicate_rows_removed
        ),
        None,
        false,
    );
    if !preview.statistics.is_empty() {
        word_paragraph(&mut body, "Key Statistics", Some("Heading2"), false);
        for statistic in &preview.statistics {
            let operation = format!("{:?}", statistic.operation).to_ascii_lowercase();
            let column = statistic.column.as_deref().unwrap_or("all rows");
            let group = statistic
                .group
                .iter()
                .map(|item| format!("{}={}", item.name, item.value))
                .collect::<Vec<_>>()
                .join(", ");
            let text = if group.is_empty() {
                format!("{operation} of {column}: {}", statistic.value)
            } else {
                format!("{group} — {operation} of {column}: {}", statistic.value)
            };
            word_paragraph(&mut body, &text, None, true);
        }
    }

    word_paragraph(&mut body, "Merged Data", Some("Heading1"), false);
    let mut merged_rows = Vec::with_capacity(preview.rows.len() + 1);
    merged_rows.push(preview.columns.clone());
    merged_rows.extend(preview.rows.clone());
    word_table(&mut body, &merged_rows)?;

    word_paragraph(&mut body, "Statistics", Some("Heading1"), false);
    let mut statistic_rows = vec![vec![
        "Operation".into(),
        "Column".into(),
        "Group".into(),
        "Value".into(),
        "Rows".into(),
    ]];
    statistic_rows.extend(preview.statistics.iter().map(|statistic| {
        vec![
            format!("{:?}", statistic.operation).to_ascii_lowercase(),
            statistic.column.clone().unwrap_or_default(),
            statistic
                .group
                .iter()
                .map(|item| format!("{}={}", item.name, item.value))
                .collect::<Vec<_>>()
                .join("; "),
            statistic.value.clone(),
            statistic.row_count.to_string(),
        ]
    }));
    word_table(&mut body, &statistic_rows)?;

    word_paragraph(&mut body, "Data Lineage", Some("Heading1"), false);
    for (index, source) in preview.lineage.iter().enumerate() {
        let digest = source
            .workbook_sha256
            .get(..12)
            .unwrap_or(&source.workbook_sha256);
        word_paragraph(
            &mut body,
            &format!(
                "Report row {} — source {} / sheet {} / row {}",
                index + 1,
                digest,
                source.sheet_name,
                source.source_row
            ),
            None,
            true,
        );
    }
    word_paragraph(&mut body, "Processing Notes", Some("Heading1"), false);
    for warning in &preview.warnings {
        word_paragraph(&mut body, warning, None, true);
    }
    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1080" w:right="1080" w:bottom="1080" w:left="1080" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#,
    );
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    ))
}

fn word_paragraph(xml: &mut String, text: &str, style: Option<&str>, bullet: bool) {
    xml.push_str("<w:p><w:pPr>");
    if let Some(style) = style {
        xml.push_str(&format!(r#"<w:pStyle w:val="{}"/>"#, escape_xml(style)));
    }
    if bullet {
        xml.push_str(r#"<w:ind w:left="360" w:hanging="180"/>"#);
    }
    xml.push_str("</w:pPr><w:r><w:t xml:space=\"preserve\">");
    if bullet {
        xml.push_str("• ");
    }
    xml.push_str(&escape_xml(text));
    xml.push_str("</w:t></w:r></w:p>");
}

fn word_table(xml: &mut String, rows: &[Vec<String>]) -> Result<(), AgentError> {
    let columns = rows.first().map_or(0, Vec::len);
    if columns == 0 || rows.iter().any(|row| row.len() != columns) {
        return Err(invalid("DOCX report table has an invalid shape"));
    }
    let width = 9_360u32 / columns as u32;
    xml.push_str(r#"<w:tbl><w:tblPr><w:tblW w:w="9360" w:type="dxa"/><w:tblLayout w:type="fixed"/><w:tblBorders><w:top w:val="single" w:sz="4" w:color="B8C2CC"/><w:left w:val="single" w:sz="4" w:color="B8C2CC"/><w:bottom w:val="single" w:sz="4" w:color="B8C2CC"/><w:right w:val="single" w:sz="4" w:color="B8C2CC"/><w:insideH w:val="single" w:sz="4" w:color="D6DCE2"/><w:insideV w:val="single" w:sz="4" w:color="D6DCE2"/></w:tblBorders><w:tblCellMar><w:top w:w="80" w:type="dxa"/><w:left w:w="100" w:type="dxa"/><w:bottom w:w="80" w:type="dxa"/><w:right w:w="100" w:type="dxa"/></w:tblCellMar></w:tblPr><w:tblGrid>"#);
    for _ in 0..columns {
        xml.push_str(&format!(r#"<w:gridCol w:w="{width}"/>"#));
    }
    xml.push_str("</w:tblGrid>");
    for (row_index, row) in rows.iter().enumerate() {
        xml.push_str("<w:tr>");
        for value in row {
            xml.push_str(&format!(
                r#"<w:tc><w:tcPr><w:tcW w:w="{width}" w:type="dxa"/>"#
            ));
            if row_index == 0 {
                xml.push_str(r#"<w:shd w:val="clear" w:color="auto" w:fill="E9EEF3"/>"#);
            }
            xml.push_str("</w:tcPr><w:p><w:r>");
            if row_index == 0 {
                xml.push_str("<w:rPr><w:b/></w:rPr>");
            }
            xml.push_str("<w:t xml:space=\"preserve\">");
            xml.push_str(&escape_xml(value));
            xml.push_str("</w:t></w:r></w:p></w:tc>");
        }
        xml.push_str("</w:tr>");
    }
    xml.push_str("</w:tbl>");
    Ok(())
}

fn word_content_types_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/><Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/></Types>"#.into()
}

fn word_package_relationships_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/></Relationships>"#.into()
}

fn word_document_relationships_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/></Relationships>"#.into()
}

fn word_core_properties_xml(title: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{}</dc:title><dc:creator>lcxl AI Assistant</dc:creator></cp:coreProperties>"#,
        escape_xml(title)
    )
}

fn word_app_properties_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>lcxl AI Assistant</Application><AppVersion>1.0</AppVersion></Properties>"#.into()
}

fn word_settings_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:defaultTabStop w:val="720"/><w:doNotTrackMoves/><w:doNotTrackFormatting/></w:settings>"#.into()
}

fn word_styles_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri"/><w:sz w:val="22"/><w:szCs w:val="22"/><w:color w:val="243447"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120" w:line="276" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:next w:val="Subtitle"/><w:pPr><w:spacing w:before="0" w:after="120"/></w:pPr><w:rPr><w:rFonts w:ascii="Calibri Light" w:hAnsi="Calibri Light"/><w:color w:val="17365D"/><w:sz w:val="42"/><w:szCs w:val="42"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Subtitle"><w:name w:val="Subtitle"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="360"/></w:pPr><w:rPr><w:color w:val="667788"/><w:sz w:val="20"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:keepNext/><w:spacing w:before="320" w:after="120"/><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:b/><w:color w:val="17365D"/><w:sz w:val="30"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:keepNext/><w:spacing w:before="240" w:after="80"/><w:outlineLvl w:val="1"/></w:pPr><w:rPr><w:b/><w:color w:val="2F5597"/><w:sz w:val="24"/></w:rPr></w:style></w:styles>"#.into()
}

fn worksheet_xml(rows: &[Vec<String>]) -> Result<String, AgentError> {
    worksheet_xml_with_optional_formula(rows, None)
}

fn worksheet_xml_with_formula(
    rows: &[Vec<String>],
    target_row: usize,
    target_column: usize,
    formula: &str,
) -> Result<String, AgentError> {
    worksheet_xml_with_optional_formula(rows, Some((target_row, target_column, formula)))
}

fn worksheet_xml_with_optional_formula(
    rows: &[Vec<String>],
    formula_cell: Option<(usize, usize, &str)>,
) -> Result<String, AgentError> {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
    );
    for (row_index, row) in rows.iter().enumerate() {
        let row_number = row_index + 1;
        xml.push_str(&format!(r#"<row r="{row_number}">"#));
        for (column_index, value) in row.iter().enumerate() {
            if value.len() > 32_767 {
                return Err(invalid("spreadsheet cell text exceeds the XLSX limit"));
            }
            let address = format!("{}{}", column_name(column_index + 1)?, row_number);
            if formula_cell.is_some_and(|(target_row, target_column, _)| {
                target_row == row_number && target_column == column_index + 1
            }) {
                let formula = formula_cell.expect("formula cell was just matched").2;
                xml.push_str(&format!(
                    r#"<c r="{address}"><f>{}</f></c>"#,
                    escape_xml(formula)
                ));
            } else if parse_decimal(value.trim()).is_some() && value.trim() == value {
                xml.push_str(&format!(
                    r#"<c r="{address}"><v>{}</v></c>"#,
                    escape_xml(value)
                ));
            } else {
                xml.push_str(&format!(
                    r#"<c r="{address}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
                    escape_xml(value)
                ));
            }
        }
        xml.push_str("</row>");
    }
    xml.push_str("</sheetData></worksheet>");
    Ok(xml)
}

fn column_name(mut column: usize) -> Result<String, AgentError> {
    if column == 0 || column > 16_384 {
        return Err(invalid("spreadsheet column exceeds the XLSX limit"));
    }
    let mut name = String::new();
    while column > 0 {
        column -= 1;
        name.insert(0, char::from(b'A' + (column % 26) as u8));
        column /= 26;
    }
    Ok(name)
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn content_types_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#.into()
}

fn package_relationships_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.into()
}

fn workbook_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Merged" sheetId="1" r:id="rId1"/><sheet name="Statistics" sheetId="2" r:id="rId2"/></sheets></workbook>"#.into()
}

fn formula_workbook_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Merged" sheetId="1" r:id="rId1"/><sheet name="Statistics" sheetId="2" r:id="rId2"/></sheets><calcPr calcMode="auto" fullCalcOnLoad="1" forceFullCalc="1"/></workbook>"#.into()
}

fn workbook_relationships_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#.into()
}

fn styles_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs></styleSheet>"#.into()
}

fn validate_merge_params(params: &SpreadsheetMergePreviewParams) -> Result<(), AgentError> {
    if params.files.is_empty()
        || params.files.len() > 8
        || params.header_row == 0
        || params.header_row > 32
        || params.columns.is_empty()
        || params.columns.len() > 64
        || params.dedupe_keys.len() > 8
        || params.statistics.len() > 16
        || params.max_rows == 0
        || params.max_rows > 1000
        || params.max_bytes < 1024
        || params.max_bytes > 1024 * 1024
        || params
            .source_sheet
            .as_ref()
            .is_some_and(|name| name.trim().is_empty() || name.len() > 128)
    {
        return Err(invalid("spreadsheet merge rules exceed the frozen ceiling"));
    }
    let mut output_headers = HashSet::new();
    for column in &params.columns {
        let output = column.output_header.trim().to_ascii_lowercase();
        if output.is_empty()
            || output.len() > 128
            || !output_headers.insert(output)
            || column.source_headers.is_empty()
            || column.source_headers.len() > 8
            || column
                .source_headers
                .iter()
                .any(|header| header.trim().is_empty() || header.len() > 128)
        {
            return Err(invalid("spreadsheet merge column rules are invalid"));
        }
    }
    for key in &params.dedupe_keys {
        if !output_headers.contains(&key.trim().to_ascii_lowercase()) {
            return Err(invalid("spreadsheet dedupe key is not an output column"));
        }
    }
    for statistic in &params.statistics {
        if statistic.group_by.len() > 4
            || statistic
                .group_by
                .iter()
                .any(|name| !output_headers.contains(&name.trim().to_ascii_lowercase()))
            || statistic
                .column
                .as_ref()
                .is_some_and(|name| !output_headers.contains(&name.trim().to_ascii_lowercase()))
            || (!matches!(statistic.operation, SpreadsheetStatisticOperation::Count)
                && statistic.column.is_none())
        {
            return Err(invalid("spreadsheet statistic rule is invalid"));
        }
    }
    Ok(())
}

fn compute_statistics(
    params: &SpreadsheetMergePreviewParams,
    column_indexes: &HashMap<String, usize>,
    rows: &[(Vec<String>, SpreadsheetRowSource)],
) -> Result<Vec<SpreadsheetStatisticResult>, AgentError> {
    let mut results = Vec::new();
    for request in &params.statistics {
        let group_indexes = request
            .group_by
            .iter()
            .map(|name| column_indexes[&name.to_ascii_lowercase()])
            .collect::<Vec<_>>();
        let mut groups = BTreeMap::<Vec<String>, Vec<&Vec<String>>>::new();
        for (values, _) in rows {
            groups
                .entry(
                    group_indexes
                        .iter()
                        .map(|index| values[*index].clone())
                        .collect(),
                )
                .or_default()
                .push(values);
        }
        for (group_values, grouped_rows) in groups {
            let column_index = request
                .column
                .as_ref()
                .map(|name| column_indexes[&name.to_ascii_lowercase()]);
            let mut numbers = Vec::new();
            let mut skipped_non_numeric = 0u32;
            if !matches!(request.operation, SpreadsheetStatisticOperation::Count)
                && let Some(column_index) = column_index
            {
                for row in &grouped_rows {
                    match parse_decimal(row[column_index].trim()) {
                        Some(value) => numbers.push(value),
                        _ => skipped_non_numeric = skipped_non_numeric.saturating_add(1),
                    }
                }
            }
            let value = match request.operation {
                SpreadsheetStatisticOperation::Count => column_index.map_or_else(
                    || grouped_rows.len().to_string(),
                    |index| {
                        grouped_rows
                            .iter()
                            .filter(|row| !row[index].trim().is_empty())
                            .count()
                            .to_string()
                    },
                ),
                SpreadsheetStatisticOperation::Sum => decimal_sum(&numbers)?,
                SpreadsheetStatisticOperation::Average => decimal_average(&numbers)?,
                SpreadsheetStatisticOperation::Min => decimal_extreme(&numbers, false)?,
                SpreadsheetStatisticOperation::Max => decimal_extreme(&numbers, true)?,
            };
            results.push(SpreadsheetStatisticResult {
                operation: request.operation,
                column: request.column.clone(),
                group: request
                    .group_by
                    .iter()
                    .cloned()
                    .zip(group_values)
                    .map(|(name, value)| SpreadsheetNamedValue { name, value })
                    .collect(),
                value,
                row_count: grouped_rows.len().min(u32::MAX as usize) as u32,
                skipped_non_numeric,
            });
        }
    }
    Ok(results)
}

#[derive(Debug, Clone, Copy)]
struct DecimalValue {
    coefficient: i128,
    scale: u32,
}

fn parse_decimal(input: &str) -> Option<DecimalValue> {
    if input.is_empty() || input.len() > 48 {
        return None;
    }
    let (negative, unsigned) = if let Some(value) = input.strip_prefix('-') {
        (true, value)
    } else if let Some(value) = input.strip_prefix('+') {
        (false, value)
    } else {
        (false, input)
    };
    let mut parts = unsigned.split('.');
    let integer = parts.next()?;
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 12
        || integer.len() + fraction.len() > 38
    {
        return None;
    }
    let digits = format!("{integer}{fraction}");
    let mut coefficient = digits.parse::<i128>().ok()?;
    if negative {
        coefficient = coefficient.checked_neg()?;
    }
    Some(DecimalValue {
        coefficient,
        scale: fraction.len() as u32,
    })
}

fn pow10(exponent: u32) -> Result<i128, AgentError> {
    (0..exponent).try_fold(1i128, |value, _| {
        value
            .checked_mul(10)
            .ok_or_else(|| invalid("spreadsheet decimal statistic overflows"))
    })
}

fn scaled(value: DecimalValue, target_scale: u32) -> Result<i128, AgentError> {
    value
        .coefficient
        .checked_mul(pow10(target_scale.saturating_sub(value.scale))?)
        .ok_or_else(|| invalid("spreadsheet decimal statistic overflows"))
}

fn decimal_sum_value(values: &[DecimalValue]) -> Result<Option<DecimalValue>, AgentError> {
    let Some(scale) = values.iter().map(|value| value.scale).max() else {
        return Ok(None);
    };
    let coefficient = values.iter().try_fold(0i128, |sum, value| {
        sum.checked_add(scaled(*value, scale)?)
            .ok_or_else(|| invalid("spreadsheet decimal statistic overflows"))
    })?;
    Ok(Some(DecimalValue { coefficient, scale }))
}

fn decimal_sum(values: &[DecimalValue]) -> Result<String, AgentError> {
    decimal_sum_value(values).map(|value| value.map(format_decimal).unwrap_or_default())
}

fn decimal_average(values: &[DecimalValue]) -> Result<String, AgentError> {
    let Some(sum) = decimal_sum_value(values)? else {
        return Ok(String::new());
    };
    let output_scale = sum.scale.max(6);
    let numerator = scaled(sum, output_scale)?;
    let divisor = values.len() as i128;
    let mut quotient = numerator / divisor;
    let remainder = numerator % divisor;
    if remainder.abs().saturating_mul(2) >= divisor {
        quotient = quotient
            .checked_add(if numerator.is_negative() { -1 } else { 1 })
            .ok_or_else(|| invalid("spreadsheet decimal statistic overflows"))?;
    }
    Ok(format_decimal(DecimalValue {
        coefficient: quotient,
        scale: output_scale,
    }))
}

fn decimal_extreme(values: &[DecimalValue], maximum: bool) -> Result<String, AgentError> {
    let Some(scale) = values.iter().map(|value| value.scale).max() else {
        return Ok(String::new());
    };
    let mut selected: Option<i128> = None;
    for value in values {
        let value = scaled(*value, scale)?;
        selected = Some(selected.map_or(value, |current| {
            if (maximum && value > current) || (!maximum && value < current) {
                value
            } else {
                current
            }
        }));
    }
    Ok(format_decimal(DecimalValue {
        coefficient: selected.unwrap_or_default(),
        scale,
    }))
}

fn format_decimal(value: DecimalValue) -> String {
    let negative = value.coefficient.is_negative();
    let digits = value.coefficient.unsigned_abs().to_string();
    let formatted = if value.scale == 0 {
        digits
    } else {
        let scale = value.scale as usize;
        let padded = if digits.len() <= scale {
            format!("{}{}", "0".repeat(scale + 1 - digits.len()), digits)
        } else {
            digits
        };
        let split = padded.len() - scale;
        format!("{}.{}", &padded[..split], &padded[split..])
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    };
    if negative && formatted != "0" {
        format!("-{formatted}")
    } else {
        formatted
    }
}

fn inspect_delimited(
    display_name: String,
    bytes: Vec<u8>,
    sha256: String,
    params: &SpreadsheetFileInspectParams,
) -> Result<SpreadsheetWorkbookProjection, AgentError> {
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("selected delimited spreadsheet is not valid UTF-8"))?;
    let delimiter = if display_name.to_ascii_lowercase().ends_with(".tsv") {
        '\t'
    } else {
        ','
    };
    let rows = parse_delimited(text, delimiter)?;
    let mut cells = Vec::new();
    let mut observed_columns = 0u32;
    let mut sheet_truncated = false;
    for (row_index, row) in rows.iter().enumerate() {
        if row_index >= params.max_rows as usize {
            sheet_truncated = true;
            break;
        }
        observed_columns = observed_columns.max(row.len().min(u32::MAX as usize) as u32);
        for (column_index, value) in row.iter().enumerate() {
            if column_index >= params.max_columns as usize {
                sheet_truncated = true;
                break;
            }
            cells.push(SpreadsheetCellProjection {
                row: row_index as u32 + 1,
                column: column_index as u32 + 1,
                address: cell_address(row_index as u32 + 1, column_index as u32 + 1),
                kind: SpreadsheetCellKind::Text,
                value: value.chars().take(4096).collect(),
                formula: None,
                formula_injection_candidate: is_formula_candidate(value),
            });
        }
    }
    let format = if delimiter == '\t' { "tsv" } else { "csv" };
    Ok(SpreadsheetWorkbookProjection {
        display_name,
        format: format.into(),
        byte_len: bytes.len() as u64,
        sha256,
        sheets: vec![SpreadsheetSheetProjection {
            name: "Sheet1".into(),
            observed_rows: rows.len().min(u32::MAX as usize) as u32,
            observed_columns,
            cells,
            truncated: sheet_truncated,
        }],
        unsupported_features: Vec::new(),
    })
}

fn parse_delimited(text: &str, delimiter: char) -> Result<Vec<Vec<String>>, AgentError> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if field.is_empty() => quoted = true,
            value if value == delimiter => {
                row.push(std::mem::take(&mut field));
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(ch),
        }
    }
    if quoted {
        return Err(invalid(
            "delimited spreadsheet has an unterminated quoted field",
        ));
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

fn inspect_xlsx(
    display_name: String,
    bytes: Vec<u8>,
    sha256: String,
    params: &SpreadsheetFileInspectParams,
) -> Result<SpreadsheetWorkbookProjection, AgentError> {
    let byte_len = bytes.len() as u64;
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| invalid("selected .xlsx is not a valid ZIP package"))?;
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(invalid("selected .xlsx contains too many package entries"));
    }
    let mut unsupported_features = Vec::new();
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .map_err(|_| invalid("cannot inspect .xlsx package entry"))?;
        let name = file.name().to_ascii_lowercase();
        if file.size() > MAX_XML_BYTES && name.ends_with(".xml") {
            return Err(invalid("selected .xlsx contains an oversized XML part"));
        }
        if name == "xl/vbaproject.bin" || name.starts_with("xl/externallinks/") {
            unsupported_features.push(name);
        } else if name == "xl/connections.xml" || name.starts_with("xl/querytables/") {
            unsupported_features.push(name);
        }
    }
    if !unsupported_features.is_empty() {
        return Err(invalid(format!(
            "selected .xlsx contains unsupported active or external features: {}",
            unsupported_features.join(", ")
        )));
    }

    let has_shared_strings = archive
        .file_names()
        .any(|name| name == "xl/sharedStrings.xml");
    let shared_strings = if has_shared_strings {
        parse_shared_strings(&read_zip_part(
            &mut archive,
            "xl/sharedStrings.xml",
            MAX_XML_BYTES,
        )?)?
    } else {
        Vec::new()
    };
    let workbook_xml = read_zip_part(&mut archive, "xl/workbook.xml", MAX_XML_BYTES)?;
    let rels_xml = read_zip_part(&mut archive, "xl/_rels/workbook.xml.rels", MAX_XML_BYTES)?;
    let relationships = parse_relationships(&rels_xml)?;
    let sheet_refs = parse_workbook_sheets(&workbook_xml)?;
    let mut sheets = Vec::new();
    for (name, relationship_id) in sheet_refs.into_iter().take(params.max_sheets as usize) {
        let target = relationships
            .get(&relationship_id)
            .ok_or_else(|| invalid("workbook sheet relationship is missing"))?;
        let part = normalize_sheet_target(target)?;
        let xml = read_zip_part(&mut archive, &part, MAX_XML_BYTES)?;
        sheets.push(parse_worksheet(&name, &xml, &shared_strings, params)?);
    }
    Ok(SpreadsheetWorkbookProjection {
        display_name,
        format: "xlsx".into(),
        byte_len,
        sha256,
        sheets,
        unsupported_features,
    })
}

fn read_zip_part<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, AgentError> {
    let mut file = archive
        .by_name(name)
        .map_err(|_| invalid(format!("selected .xlsx is missing {name}")))?;
    if file.size() > max_bytes {
        return Err(invalid(format!("selected .xlsx part {name} is oversized")));
    }
    let mut bytes = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|_| invalid(format!("cannot read .xlsx part {name}")))?;
    Ok(bytes)
}

fn parse_shared_strings(xml: &[u8]) -> Result<Vec<String>, AgentError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_item = false;
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) if xml_name_is(event.name().as_ref(), b"si") => {
                current.clear();
                in_item = true;
            }
            Ok(Event::End(event)) if xml_name_is(event.name().as_ref(), b"si") => {
                strings.push(std::mem::take(&mut current));
                in_item = false;
            }
            Ok(Event::Start(event)) if xml_name_is(event.name().as_ref(), b"t") => in_text = true,
            Ok(Event::End(event)) if xml_name_is(event.name().as_ref(), b"t") => in_text = false,
            Ok(Event::Text(text)) if in_item && in_text => current.push_str(
                &text
                    .decode()
                    .map_err(|_| invalid("shared string is not valid XML text"))?,
            ),
            Ok(Event::Eof) => break,
            Err(_) => return Err(invalid("shared strings XML is malformed")),
            _ => {}
        }
    }
    Ok(strings)
}

fn parse_relationships(xml: &[u8]) -> Result<HashMap<String, String>, AgentError> {
    let mut reader = Reader::from_reader(xml);
    let mut relationships = HashMap::new();
    loop {
        match reader.read_event() {
            Ok(Event::Empty(event)) | Ok(Event::Start(event))
                if xml_name_is(event.name().as_ref(), b"Relationship") =>
            {
                if let (Some(id), Some(target)) = (
                    xml_attribute(&reader, &event, b"Id")?,
                    xml_attribute(&reader, &event, b"Target")?,
                ) {
                    relationships.insert(id, target);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(invalid("workbook relationships XML is malformed")),
            _ => {}
        }
    }
    Ok(relationships)
}

fn parse_workbook_sheets(xml: &[u8]) -> Result<Vec<(String, String)>, AgentError> {
    let mut reader = Reader::from_reader(xml);
    let mut sheets = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Empty(event)) | Ok(Event::Start(event))
                if xml_name_is(event.name().as_ref(), b"sheet") =>
            {
                if let (Some(name), Some(id)) = (
                    xml_attribute(&reader, &event, b"name")?,
                    xml_attribute(&reader, &event, b"r:id")?,
                ) {
                    sheets.push((name.chars().take(128).collect(), id));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(invalid("workbook XML is malformed")),
            _ => {}
        }
    }
    Ok(sheets)
}

fn parse_worksheet(
    name: &str,
    xml: &[u8],
    shared_strings: &[String],
    params: &SpreadsheetFileInspectParams,
) -> Result<SpreadsheetSheetProjection, AgentError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut cells = Vec::new();
    let mut current: Option<(String, Option<String>, String, Option<String>)> = None;
    let mut active_value = false;
    let mut active_formula = false;
    let mut active_inline_text = false;
    let mut observed_rows = 0u32;
    let mut observed_columns = 0u32;
    let mut truncated = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) if xml_name_is(event.name().as_ref(), b"c") => {
                let address = xml_attribute(&reader, &event, b"r")?
                    .ok_or_else(|| invalid("worksheet cell has no address"))?;
                let cell_type = xml_attribute(&reader, &event, b"t")?;
                current = Some((address, cell_type, String::new(), None));
            }
            Ok(Event::Start(event)) if xml_name_is(event.name().as_ref(), b"v") => {
                active_value = true;
            }
            Ok(Event::End(event)) if xml_name_is(event.name().as_ref(), b"v") => {
                active_value = false;
            }
            Ok(Event::Start(event)) if xml_name_is(event.name().as_ref(), b"f") => {
                active_formula = true;
            }
            Ok(Event::End(event)) if xml_name_is(event.name().as_ref(), b"f") => {
                active_formula = false;
            }
            Ok(Event::Start(event))
                if xml_name_is(event.name().as_ref(), b"t")
                    && current
                        .as_ref()
                        .and_then(|(_, cell_type, _, _)| cell_type.as_deref())
                        == Some("inlineStr") =>
            {
                active_inline_text = true;
            }
            Ok(Event::End(event)) if xml_name_is(event.name().as_ref(), b"t") => {
                active_inline_text = false;
            }
            Ok(Event::Text(text))
                if current.is_some() && (active_value || active_formula || active_inline_text) =>
            {
                let decoded = text
                    .decode()
                    .map_err(|_| invalid("worksheet cell text is malformed"))?
                    .into_owned();
                if let Some((_, _, value, formula)) = current.as_mut() {
                    if active_formula {
                        *formula = Some(decoded);
                    } else {
                        value.push_str(&decoded);
                    }
                }
            }
            Ok(Event::End(event)) if xml_name_is(event.name().as_ref(), b"c") => {
                let Some((address, cell_type, raw_value, formula)) = current.take() else {
                    continue;
                };
                let (row, column) = parse_cell_address(&address)?;
                observed_rows = observed_rows.max(row);
                observed_columns = observed_columns.max(column);
                if row > params.max_rows || column > params.max_columns {
                    truncated = true;
                    continue;
                }
                let (kind, value) = match cell_type.as_deref() {
                    Some("s") => {
                        let index = raw_value
                            .parse::<usize>()
                            .map_err(|_| invalid("shared string index is invalid"))?;
                        (
                            SpreadsheetCellKind::Text,
                            shared_strings.get(index).cloned().ok_or_else(|| {
                                invalid("shared string index is outside the table")
                            })?,
                        )
                    }
                    Some("b") => (SpreadsheetCellKind::Boolean, raw_value),
                    Some("e") => (SpreadsheetCellKind::Error, raw_value),
                    Some("str") | Some("inlineStr") => (SpreadsheetCellKind::Text, raw_value),
                    _ if raw_value.is_empty() && formula.is_none() => {
                        (SpreadsheetCellKind::Blank, String::new())
                    }
                    _ => (SpreadsheetCellKind::Number, raw_value),
                };
                cells.push(SpreadsheetCellProjection {
                    row,
                    column,
                    address,
                    kind,
                    value: value.chars().take(4096).collect(),
                    formula,
                    formula_injection_candidate: false,
                });
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(invalid("worksheet XML is malformed")),
            _ => {}
        }
    }
    Ok(SpreadsheetSheetProjection {
        name: name.to_string(),
        observed_rows,
        observed_columns,
        cells,
        truncated,
    })
}

fn xml_attribute(
    reader: &Reader<&[u8]>,
    event: &BytesStart<'_>,
    key: &[u8],
) -> Result<Option<String>, AgentError> {
    for attribute in event.attributes().with_checks(false) {
        let attribute = attribute.map_err(|_| invalid("XML attribute is malformed"))?;
        if attribute.key.as_ref() == key {
            return attribute
                .decode_and_unescape_value(reader.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|_| invalid("XML attribute value is malformed"));
        }
    }
    Ok(None)
}

fn xml_name_is(name: &[u8], expected_local_name: &[u8]) -> bool {
    name.rsplit(|byte| *byte == b':').next() == Some(expected_local_name)
}

fn normalize_sheet_target(target: &str) -> Result<String, AgentError> {
    let normalized = target.replace('\\', "/");
    if normalized.starts_with("//") || normalized.contains([':', '?', '#', '%', '\0']) {
        return Err(invalid(
            "worksheet relationship escapes the workbook package",
        ));
    }
    let normalized = normalized.strip_prefix('/').unwrap_or(&normalized);
    if normalized
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid(
            "worksheet relationship escapes the workbook package",
        ));
    }
    let part = if normalized.starts_with("xl/") {
        normalized.to_string()
    } else {
        format!("xl/{normalized}")
    };
    let lower = part.to_ascii_lowercase();
    if !lower.starts_with("xl/worksheets/") || !lower.ends_with(".xml") {
        return Err(invalid(
            "worksheet relationship does not target a worksheet XML part",
        ));
    }
    Ok(part)
}

fn parse_cell_address(address: &str) -> Result<(u32, u32), AgentError> {
    let mut column = 0u32;
    let mut split = 0usize;
    for (index, byte) in address.bytes().enumerate() {
        if byte.is_ascii_alphabetic() {
            column = column
                .checked_mul(26)
                .and_then(|value| value.checked_add((byte.to_ascii_uppercase() - b'A' + 1) as u32))
                .ok_or_else(|| invalid("worksheet cell column overflows"))?;
            split = index + 1;
        } else {
            break;
        }
    }
    let row = address[split..]
        .parse::<u32>()
        .map_err(|_| invalid("worksheet cell row is invalid"))?;
    if row == 0 || column == 0 {
        return Err(invalid("worksheet cell address is invalid"));
    }
    Ok((row, column))
}

fn cell_address(row: u32, mut column: u32) -> String {
    let mut letters = Vec::new();
    while column > 0 {
        column -= 1;
        letters.push((b'A' + (column % 26) as u8) as char);
        column /= 26;
    }
    letters.reverse();
    format!("{}{row}", letters.into_iter().collect::<String>())
}

fn is_formula_candidate(value: &str) -> bool {
    matches!(
        value.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r' | '\n')
    )
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn internal(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::io::Write;

    #[test]
    fn delimited_parser_handles_quotes_newlines_and_formula_candidates() {
        let rows =
            parse_delimited("name,value\r\n\"a,b\",\"line1\nline2\"\r\nunsafe,=1+1", ',').unwrap();
        assert_eq!(rows[1], ["a,b", "line1\nline2"]);
        assert!(is_formula_candidate(&rows[2][1]));
    }

    #[test]
    fn cell_addresses_round_trip_common_columns() {
        for (address, row, column) in [("A1", 1, 1), ("Z9", 9, 26), ("AA10", 10, 27)] {
            assert_eq!(parse_cell_address(address).unwrap(), (row, column));
            assert_eq!(cell_address(row, column), address);
        }
    }

    #[test]
    fn sheet_target_cannot_escape_package() {
        assert_eq!(
            normalize_sheet_target("worksheets/sheet1.xml").unwrap(),
            "xl/worksheets/sheet1.xml"
        );
        assert_eq!(
            normalize_sheet_target("/xl/worksheets/sheet1.xml").unwrap(),
            "xl/worksheets/sheet1.xml"
        );
        assert!(normalize_sheet_target("../external.xml").is_err());
        assert!(normalize_sheet_target("worksheets/%2e%2e/external.xml").is_err());
        assert!(normalize_sheet_target("externalLinks/externalLink1.xml").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn selected_csv_projects_text_and_never_executes_formula_candidates() {
        let _guard = super::super::file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("input.csv");
        std::fs::write(&path, "name,value\r\nalpha,10\r\nunsafe,=1+1").unwrap();
        super::super::file_reference_store::reset_worker_incarnation();
        let file = super::super::file_reference_store::issue(&path).unwrap();
        let output = inspect(&SpreadsheetFileInspectParams {
            files: vec![file],
            max_workbooks: 1,
            max_sheets: 1,
            max_rows: 20,
            max_columns: 10,
            max_bytes: 64 * 1024,
        })
        .unwrap();
        let sheet = &output.workbooks[0].sheets[0];
        let candidate = sheet
            .cells
            .iter()
            .find(|cell| cell.address == "B3")
            .unwrap();
        assert_eq!(candidate.value, "=1+1");
        assert!(candidate.formula.is_none());
        assert!(candidate.formula_injection_candidate);
    }

    #[cfg(windows)]
    #[test]
    fn selected_xlsx_projects_shared_values_and_formula_without_calculation() {
        let _guard = super::super::file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("input.xlsx");
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, xml) in [
            (
                "xl/workbook.xml",
                r#"<x:workbook xmlns:x="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><x:sheets><x:sheet name="Data" sheetId="1" r:id="rId1"/></x:sheets></x:workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<x:sst xmlns:x="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><x:si><x:t>name</x:t></x:si><x:si><x:t>alpha</x:t></x:si></x:sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<x:worksheet xmlns:x="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><x:sheetData><x:row r="1"><x:c r="A1" t="s"><x:v>0</x:v></x:c><x:c r="B1"><x:v>2</x:v></x:c></x:row><x:row r="2"><x:c r="A2" t="s"><x:v>1</x:v></x:c><x:c r="B2"><x:f>1+2</x:f><x:v>3</x:v></x:c><x:c r="C2" t="inlineStr"><x:is><x:t>inline</x:t></x:is></x:c></x:row></x:sheetData></x:worksheet>"#,
            ),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(xml.as_bytes()).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        std::fs::write(&path, bytes).unwrap();
        super::super::file_reference_store::reset_worker_incarnation();
        let file = super::super::file_reference_store::issue(&path).unwrap();
        let output = inspect(&SpreadsheetFileInspectParams {
            files: vec![file],
            max_workbooks: 1,
            max_sheets: 4,
            max_rows: 20,
            max_columns: 10,
            max_bytes: 64 * 1024,
        })
        .unwrap();
        let sheet = &output.workbooks[0].sheets[0];
        assert_eq!(sheet.name, "Data");
        assert_eq!(
            sheet
                .cells
                .iter()
                .find(|cell| cell.address == "A2")
                .unwrap()
                .value,
            "alpha"
        );
        let formula = sheet
            .cells
            .iter()
            .find(|cell| cell.address == "B2")
            .unwrap();
        assert_eq!(formula.formula.as_deref(), Some("1+2"));
        assert_eq!(formula.value, "3");
        assert_eq!(
            sheet
                .cells
                .iter()
                .find(|cell| cell.address == "C2")
                .unwrap()
                .value,
            "inline"
        );
    }

    #[cfg(windows)]
    #[test]
    fn merge_preview_uses_typed_columns_dedupe_statistics_and_lineage() {
        let _guard = super::super::file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.csv");
        let second = temp.path().join("second.csv");
        std::fs::write(&first, "Region,Revenue\r\nNorth,10\r\nSouth,20").unwrap();
        std::fs::write(&second, "region,revenue\r\nNorth,10\r\nWest,30").unwrap();
        super::super::file_reference_store::reset_worker_incarnation();
        let output = preview_merge(&SpreadsheetMergePreviewParams {
            files: vec![
                super::super::file_reference_store::issue(&first).unwrap(),
                super::super::file_reference_store::issue(&second).unwrap(),
            ],
            source_sheet: None,
            header_row: 1,
            columns: vec![
                desk_agent_protocol::computer_use::SpreadsheetMergeColumnRule {
                    output_header: "Region".into(),
                    source_headers: vec!["Region".into()],
                },
                desk_agent_protocol::computer_use::SpreadsheetMergeColumnRule {
                    output_header: "Revenue".into(),
                    source_headers: vec!["Revenue".into()],
                },
            ],
            dedupe_keys: vec!["Region".into()],
            statistics: vec![
                desk_agent_protocol::computer_use::SpreadsheetStatisticRequest {
                    operation: SpreadsheetStatisticOperation::Count,
                    column: None,
                    group_by: Vec::new(),
                },
                desk_agent_protocol::computer_use::SpreadsheetStatisticRequest {
                    operation: SpreadsheetStatisticOperation::Sum,
                    column: Some("Revenue".into()),
                    group_by: Vec::new(),
                },
            ],
            max_rows: 100,
            max_bytes: 64 * 1024,
        })
        .unwrap();
        assert_eq!(output.rows.len(), 3);
        assert_eq!(output.lineage.len(), 3);
        assert_eq!(output.duplicate_rows_removed, 1);
        assert_eq!(output.statistics[0].value, "3");
        assert_eq!(output.statistics[1].value, "60");

        let bytes = materialize_preview_xlsx(&output.preview_id).unwrap();
        let mut package = ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
        assert!(package.by_name("xl/worksheets/sheet1.xml").is_ok());
        assert!(package.by_name("xl/worksheets/sheet2.xml").is_ok());
        for index in 0..package.len() {
            let mut entry = package.by_index(index).unwrap();
            if entry.name().ends_with(".xml") {
                let mut xml = String::new();
                entry.read_to_string(&mut xml).unwrap();
                assert!(
                    !xml.contains("<f>"),
                    "generated workbook must be formula-free"
                );
                assert!(!xml.contains("TargetMode=\"External\""));
            }
        }
        drop(package);
        let formula_validation = desk_diagnose_core::spreadsheet_formula::validate_formula_patch(
            "=B2*1.1",
            "Merged!C2",
            desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
            &["Merged".into(), "Statistics".into()],
        )
        .unwrap();
        let formula_bytes = materialize_preview_formula_xlsx(
            &output.preview_id,
            "Merged!C2",
            "=B2*1.1",
            desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
            &formula_validation.ast_digest_sha256,
        )
        .unwrap();
        let mut formula_package = ZipArchive::new(Cursor::new(formula_bytes)).unwrap();
        let mut formula_sheet = String::new();
        formula_package
            .by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut formula_sheet)
            .unwrap();
        assert!(formula_sheet.contains(r#"<c r="C2"><f>B2*1.1</f></c>"#));
        assert_eq!(formula_sheet.matches("<f>").count(), 1);
        assert!(
            materialize_preview_formula_xlsx(
                &output.preview_id,
                "Merged!B2",
                "=B2*1.1",
                desk_diagnose_core::spreadsheet_formula::FORMULA_LOCALE_V1,
                &formula_validation.ast_digest_sha256,
            )
            .is_err()
        );
        let docx = materialize_preview_docx(&output.preview_id, "Regional Revenue Brief")
            .expect("retained preview materializes as a Word report");
        let mut word_package = ZipArchive::new(Cursor::new(docx)).unwrap();
        let mut document_xml = String::new();
        word_package
            .by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut document_xml)
            .unwrap();
        assert!(document_xml.contains("Regional Revenue Brief"));
        assert!(document_xml.contains("Executive Summary"));
        assert!(document_xml.contains("North"));
        assert!(document_xml.contains("South"));
        assert!(document_xml.contains("West"));
        assert!(document_xml.contains("Data Lineage"));
        assert!(!document_xml.contains("altChunk"));
        assert!(!document_xml.contains("fldSimple"));
        drop(document_xml);
        drop(word_package);
        let docx = materialize_preview_docx(&output.preview_id, "Unsafe\u{1}Title");
        assert!(docx.is_err());
        let generated = temp.path().join("merged.xlsx");
        std::fs::write(&generated, bytes).unwrap();
        super::super::file_reference_store::reset_worker_incarnation();
        let projection = inspect(&SpreadsheetFileInspectParams {
            files: vec![super::super::file_reference_store::issue(&generated).unwrap()],
            max_workbooks: 1,
            max_sheets: 4,
            max_rows: 100,
            max_columns: 20,
            max_bytes: 128 * 1024,
        })
        .unwrap();
        assert_eq!(
            projection.workbooks[0]
                .sheets
                .iter()
                .map(|sheet| sheet.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Merged", "Statistics"]
        );
        assert_eq!(projection.workbooks[0].sheets[0].observed_rows, 4);
        assert!(
            projection.workbooks[0].sheets[0]
                .cells
                .iter()
                .all(|cell| cell.formula.is_none())
        );
    }

    #[test]
    fn decimal_statistics_are_deterministic_and_do_not_leak_binary_float_tails() {
        let values = ["72000", "59000", "86000"]
            .into_iter()
            .map(|value| parse_decimal(value).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(decimal_sum(&values).unwrap(), "217000");
        assert_eq!(decimal_average(&values).unwrap(), "72333.333333");
        assert_eq!(decimal_extreme(&values, false).unwrap(), "59000");
        assert_eq!(decimal_extreme(&values, true).unwrap(), "86000");
        assert!(parse_decimal("1e3").is_none());
        assert!(parse_decimal("-+1").is_none());
    }
}
