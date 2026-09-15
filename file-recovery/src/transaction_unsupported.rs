//! Platforms without a verified native cleanup implementation retain records.
use super::*;
pub(super) fn clean(_: &Transaction) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native transaction cleanup is unavailable",
    ))
}
