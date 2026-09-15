//! Bind a stored transaction location to the caller's held object identities.
use super::*;
use std::path::Path;

/// Returned handles pin the resolved directories through the ledger write.
/// The caller must retain the production replacement guard (which denies delete
/// sharing): the identity read handle must share DELETE with that guard so its
/// already-owned rename right can be used at commit. Source version protection
/// and authorization remain separate caller requirements.
pub(crate) fn transaction_location(
    parent_path: &Path,
    parent: &File,
    directory_name: &str,
    directory: Option<&File>,
    staged: Option<&File>,
) -> io::Result<Vec<File>> {
    let mut held = root::resolve_directory(parent_path)?;
    if file_identity(held.last().unwrap(), FileKind::Directory)?
        != file_identity(parent, FileKind::Directory)?
    {
        return Err(crate::invalid(
            "transaction parent path does not match its handle",
        ));
    }
    let Some(directory) = directory else {
        if staged.is_some() {
            return Err(crate::invalid("transaction staging has no directory"));
        }
        return Ok(held);
    };
    let user = security::current_user()?;
    let resolved = open_relative(
        held.last().unwrap(),
        directory_name,
        &user,
        OpenKind::InspectDirectory,
    )?;
    if file_identity(&resolved, FileKind::Directory)?
        != file_identity(directory, FileKind::Directory)?
    {
        return Err(crate::invalid(
            "transaction directory path does not match its handle",
        ));
    }
    security::validate_private(&resolved, &user)?;
    held.push(resolved);
    if let Some(staged) = staged {
        let resolved = open_relative(
            held.last().unwrap(),
            "replacement",
            &user,
            OpenKind::InspectTransactionFile,
        )?;
        if file_identity(&resolved, FileKind::File)? != file_identity(staged, FileKind::File)? {
            return Err(crate::invalid(
                "transaction staging path does not match its handle",
            ));
        }
        held.push(resolved);
    }
    Ok(held)
}
