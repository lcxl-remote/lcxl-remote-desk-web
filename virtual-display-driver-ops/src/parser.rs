//! Pure parsing logic for `Get-WindowsDriver | ConvertTo-Json` (primary)
//! and `pnputil /enum-drivers` (fallback). Kept I/O-free so we can
//! exhaustively unit-test the zero/single/multi-result JSON shapes and
//! the locale-sensitive pnputil text layout without invoking any
//! external process.

use crate::InstallerError;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PsDriverEntry {
    #[serde(rename = "Driver")]
    driver: Option<String>,
    #[serde(rename = "OriginalFileName")]
    original_file_name: Option<String>,
}

/// Parses the (possibly empty / single-object / array) JSON output
/// of `Get-WindowsDriver -Online | ... | ConvertTo-Json -Compress`.
///
/// Returns the list of `Driver` (published `oem##.inf`) values for
/// entries whose `OriginalFileName` basename matches
/// `LcxlVirtualDisplay.inf` (case-insensitive).
pub(crate) fn parse_ps_get_windows_driver(stdout: &str) -> Result<Vec<String>, InstallerError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        return Ok(Vec::new());
    }
    let entries: Vec<PsDriverEntry> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed)
            .map_err(|e| InstallerError::Parse(format!("ps json array: {e}")))?
    } else {
        // Single object — Get-WindowsDriver | ConvertTo-Json emits a
        // bare object when exactly one item is selected.
        let single: PsDriverEntry = serde_json::from_str(trimmed)
            .map_err(|e| InstallerError::Parse(format!("ps json object: {e}")))?;
        vec![single]
    };
    Ok(filter_matching(entries))
}

fn filter_matching(entries: Vec<PsDriverEntry>) -> Vec<String> {
    let mut out = Vec::new();
    for entry in entries {
        let Some(orig) = entry.original_file_name else {
            continue;
        };
        if !matches_inf(&orig) {
            continue;
        }
        let Some(d) = entry.driver else { continue };
        out.push(d);
    }
    out
}

/// Best-effort English-locale parser for `pnputil /enum-drivers`.
/// Mirrors the install-or-update.ps1 fallback: walks pairs of
/// "Published Name" + "Original Name" and emits the published name
/// whenever the original matches `LcxlVirtualDisplay.inf`.
///
/// Locale-sensitive — non-English Windows builds will skip records.
/// The primary path (`Get-WindowsDriver`) returns structured DISM
/// data and is locale-stable, so this fallback only kicks in when
/// the host's PowerShell context cannot run Get-WindowsDriver.
pub(crate) fn parse_pnputil_enum(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current_published: Option<String> = None;
    let mut current_original: Option<String> = None;

    let commit = |pub_: &mut Option<String>, orig: &mut Option<String>, out: &mut Vec<String>| {
        if let (Some(p), Some(o)) = (pub_.take(), orig.take())
            && matches_inf(&o)
        {
            out.push(p);
        }
    };

    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("Published Name") {
            commit(&mut current_published, &mut current_original, &mut out);
            current_published = field_value(rest);
        } else if let Some(rest) = trimmed.strip_prefix("Original Name") {
            current_original = field_value(rest);
        }
    }
    commit(&mut current_published, &mut current_original, &mut out);
    out
}

fn field_value(after_key: &str) -> Option<String> {
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let v = after_colon.trim();
    if v.is_empty() { None } else { Some(v.to_owned()) }
}

fn matches_inf(orig: &str) -> bool {
    let basename = std::path::Path::new(orig)
        .file_name()
        .and_then(|os| os.to_str())
        .unwrap_or("");
    basename.eq_ignore_ascii_case(crate::DRIVER_INF_BASENAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_empty_or_null_returns_empty() {
        assert!(parse_ps_get_windows_driver("").unwrap().is_empty());
        assert!(parse_ps_get_windows_driver("   ").unwrap().is_empty());
        assert!(parse_ps_get_windows_driver("null").unwrap().is_empty());
        assert!(parse_ps_get_windows_driver("NULL").unwrap().is_empty());
    }

    #[test]
    fn ps_single_object_for_single_match() {
        let stdout = r#"{"Driver":"oem23.inf","OriginalFileName":"C:\\Windows\\System32\\DriverStore\\FileRepository\\lcxlvirtualdisplay.inf_amd64_xxxx\\LcxlVirtualDisplay.inf"}"#;
        let v = parse_ps_get_windows_driver(stdout).unwrap();
        assert_eq!(v, vec!["oem23.inf".to_owned()]);
    }

    #[test]
    fn ps_array_for_multiple_matches() {
        let stdout = r#"[{"Driver":"oem23.inf","OriginalFileName":"X\\LcxlVirtualDisplay.inf"},{"Driver":"oem55.inf","OriginalFileName":"Y\\lcxlvirtualdisplay.inf"}]"#;
        let v = parse_ps_get_windows_driver(stdout).unwrap();
        assert_eq!(v, vec!["oem23.inf".to_owned(), "oem55.inf".to_owned()]);
    }

    #[test]
    fn ps_skips_non_matching_inf_in_array() {
        let stdout = r#"[{"Driver":"oem23.inf","OriginalFileName":"X\\OtherDriver.inf"},{"Driver":"oem55.inf","OriginalFileName":"Y\\LcxlVirtualDisplay.inf"}]"#;
        let v = parse_ps_get_windows_driver(stdout).unwrap();
        assert_eq!(v, vec!["oem55.inf".to_owned()]);
    }

    #[test]
    fn ps_garbage_returns_parse_error() {
        let stdout = r#"{not json"#;
        assert!(matches!(
            parse_ps_get_windows_driver(stdout).unwrap_err(),
            InstallerError::Parse(_)
        ));
    }

    #[test]
    fn ps_entry_without_driver_field_skipped() {
        let stdout = r#"{"OriginalFileName":"X\\LcxlVirtualDisplay.inf"}"#;
        assert!(parse_ps_get_windows_driver(stdout).unwrap().is_empty());
    }

    #[test]
    fn pnputil_parses_multiple_records_in_order() {
        let stdout = "\n\
        Published Name :     oem23.inf\n\
        Original Name :      LcxlVirtualDisplay.inf\n\
        Provider Name :      Lcxl\n\
        \n\
        Published Name :     oem55.inf\n\
        Original Name :      C:\\path\\LcxlVirtualDisplay.inf\n\
        Provider Name :      Lcxl\n\
        \n\
        Published Name :     oem99.inf\n\
        Original Name :      OtherDriver.inf\n";
        let v = parse_pnputil_enum(stdout);
        assert_eq!(v, vec!["oem23.inf".to_owned(), "oem55.inf".to_owned()]);
    }

    #[test]
    fn pnputil_empty_input_returns_empty() {
        assert!(parse_pnputil_enum("").is_empty());
    }

    #[test]
    fn pnputil_record_missing_original_skipped() {
        let stdout = "\n\
        Published Name :     oem23.inf\n\
        Provider Name :      Lcxl\n";
        assert!(parse_pnputil_enum(stdout).is_empty());
    }
}
