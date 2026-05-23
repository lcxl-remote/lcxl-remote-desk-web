//! OEM INF filename allow-list.
//!
//! `pnputil` re-publishes third-party INFs as `oem##.inf`. Before passing
//! a name back to `pnputil /delete-driver` we explicitly anchor a regex
//! match so that a hypothetical compromise of the Get-WindowsDriver
//! output (e.g. someone seeding a forged `OriginalFileName`) cannot
//! turn into "remove an arbitrary path on disk".

use crate::InstallerError;
use regex::Regex;
use std::sync::OnceLock;

static OEM_RE: OnceLock<Regex> = OnceLock::new();

pub(crate) fn validate(name: &str) -> Result<(), InstallerError> {
    let re = OEM_RE.get_or_init(|| Regex::new(r"^oem\d+\.inf$").unwrap());
    if re.is_match(name) {
        Ok(())
    } else {
        Err(InstallerError::InvalidOemName(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_oem_inf() {
        validate("oem1.inf").unwrap();
        validate("oem23.inf").unwrap();
        validate("oem999.inf").unwrap();
    }

    #[test]
    fn rejects_suffix_path_or_injection() {
        assert!(validate("oem1.inf.bak").is_err());
        assert!(validate("oem1.inf;cmd").is_err());
        assert!(validate("../../oem1.inf").is_err());
        assert!(validate("oem.inf").is_err());
        // pnputil's PnP store always writes lowercase oem##.inf; we
        // reject capitalised variants to avoid case-folding ambiguity.
        assert!(validate("OEM1.INF").is_err());
        assert!(validate("oem1.INF").is_err());
        assert!(validate("").is_err());
        assert!(validate("oem 1.inf").is_err());
    }
}
