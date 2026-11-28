//! Linux: mount table from `/proc/self/mountinfo` and the freedesktop trash
//! as the `trash` crate implements it.
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use desk_utils::error::DeskErrorCode;

use super::{MountTable, access};
use crate::error::DeskError;

pub(super) fn mount_points() -> io::Result<Vec<PathBuf>> {
    Ok(parse_mountinfo(&fs::read("/proc/self/mountinfo")?))
}

/// Mount points from mountinfo text: the fifth field of each line, with the
/// kernel's octal escapes (`\040` for a space and so on) decoded.
fn parse_mountinfo(text: &[u8]) -> Vec<PathBuf> {
    text.split(|byte| *byte == b'\n')
        .filter_map(|line| line.split(|byte| *byte == b' ').nth(4))
        .filter(|field| !field.is_empty())
        .map(|field| PathBuf::from(OsString::from_vec(unescape(field))))
        .collect()
}

fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        let octal = field.get(index + 1..index + 4).filter(|digits| {
            field[index] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });
        match octal {
            Some(digits) => {
                let value = digits
                    .iter()
                    .fold(0u32, |acc, digit| acc * 8 + u32::from(digit - b'0'));
                out.push(value as u8);
                index += 4;
            }
            None => {
                out.push(field[index]);
                index += 1;
            }
        }
    }
    out
}

/// `path` itself when it exists, otherwise its closest existing ancestor.
fn nearest_existing(path: &Path) -> Option<&Path> {
    path.ancestors()
        .find(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// Whether a directory accepts new entries, or could be created by the host
/// user because its closest existing ancestor does.
fn writable_or_creatable(path: &Path) -> bool {
    match nearest_existing(path) {
        Some(existing) if existing == path => {
            fs::metadata(path).is_ok_and(|m| m.is_dir()) && access(path, libc::W_OK | libc::X_OK)
        }
        Some(existing) => access(existing, libc::W_OK | libc::X_OK),
        None => false,
    }
}

/// A trash directory can take an entry when `files/` and `info/` can be
/// written; the crate creates both as needed.
fn trash_dir_usable(trash: &Path) -> bool {
    writable_or_creatable(&trash.join("files")) && writable_or_creatable(&trash.join("info"))
}

/// The home trash, as the freedesktop spec and the crate resolve it.
fn home_trash() -> Option<PathBuf> {
    let non_empty = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(data_home) = non_empty("XDG_DATA_HOME") {
        return Some(PathBuf::from(data_home).join("Trash"));
    }
    non_empty("HOME").map(|home| PathBuf::from(home).join(".local/share/Trash"))
}

/// Canonical form of a path that may not exist yet: canonicalize the closest
/// existing ancestor and append the rest.
fn canonical_or_parents(path: &Path) -> PathBuf {
    let Some(existing) = nearest_existing(path) else {
        return path.to_path_buf();
    };
    let Ok(canonical) = fs::canonicalize(existing) else {
        return path.to_path_buf();
    };
    match path.strip_prefix(existing) {
        Ok(rest) if !rest.as_os_str().is_empty() => canonical.join(rest),
        _ => canonical,
    }
}

/// The per-volume trash the crate would pick on `top`, if it is usable.
fn volume_trash(top: &Path) -> Option<PathBuf> {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let shared = top.join(".Trash");
    let shared_valid = fs::symlink_metadata(&shared)
        .is_ok_and(|m| m.is_dir() && m.mode() & u32::from(libc::S_ISVTX) != 0);
    if shared_valid {
        let user = shared.join(uid.to_string());
        if user.is_dir() {
            return trash_dir_usable(&user).then_some(user);
        }
    }
    let own = top.join(format!(".Trash-{uid}"));
    if own.is_dir() {
        return trash_dir_usable(&own).then_some(own);
    }
    (!own.exists() && access(top, libc::W_OK | libc::X_OK)).then_some(own)
}

/// Whether deleting from `dir` can probably use a trash: `false` only when
/// neither the home trash nor the volume trash can take an entry.
pub(super) fn trash_available(dir: &Path, mounts: &MountTable) -> bool {
    let Some(home) = home_trash() else {
        return false;
    };
    let home = canonical_or_parents(&home);
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let home_usable = trash_dir_usable(&home);
    let top = mounts.topdir(&dir);
    if mounts.topdir(&home) == top {
        return home_usable;
    }
    home_usable || volume_trash(top).is_some()
}

/// The trash the crate moves an entry of `parent` into.
fn chosen_trash(parent: &Path, mounts: &MountTable) -> Option<PathBuf> {
    let home = canonical_or_parents(&home_trash()?);
    let top = mounts.topdir(parent);
    if mounts.topdir(&home) == top {
        return Some(home);
    }
    volume_trash(top).or(Some(home))
}

/// A symlink may only reach the trash by `rename`. Across devices the crate
/// copies the link's target instead, losing the link, so refuse first.
pub(super) fn guard_link_trash(target: &Path, mounts: &MountTable) -> Result<(), DeskError> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    let Some(trash) = chosen_trash(parent, mounts) else {
        // No trash at all: the crate fails without touching the link.
        return Ok(());
    };
    let files = trash.join("files");
    let Some(existing) = nearest_existing(&files) else {
        return Ok(());
    };
    if fs::metadata(existing)?.dev() != fs::metadata(parent)?.dev() {
        return Err(DeskError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "A symbolic link cannot be moved to the trash of this location; delete it permanently",
        ));
    }
    Ok(())
}

pub(super) fn move_to_trash(path: &Path) -> Result<(), trash::Error> {
    trash::delete(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_fields_are_unescaped() {
        let text = b"22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
            40 22 0:5 / /mnt/my\\040disk rw - tmpfs tmpfs rw\n\
            41 22 0:6 / /odd\\134name rw - tmpfs tmpfs rw\n";
        assert_eq!(
            parse_mountinfo(text),
            vec![
                PathBuf::from("/"),
                PathBuf::from("/mnt/my disk"),
                PathBuf::from("/odd\\name"),
            ]
        );
    }

    #[test]
    fn creatable_directories_count_as_writable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(writable_or_creatable(&dir.path().join("a/b")));
        assert!(writable_or_creatable(dir.path()));
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(!writable_or_creatable(&file));
    }

    #[test]
    fn trash_with_writable_subdirectories_is_usable_even_if_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let trash = dir.path().join("Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o555)).unwrap();
        let usable = trash_dir_usable(&trash);
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(usable);
    }

    #[test]
    fn read_only_trash_ancestor_is_unusable() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root bypasses permission bits
        }
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let usable = trash_dir_usable(&locked.join("Trash"));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!usable);
    }

    #[test]
    fn canonical_or_parents_keeps_the_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing/Trash");
        assert_eq!(
            canonical_or_parents(&path),
            fs::canonicalize(dir.path()).unwrap().join("missing/Trash")
        );
    }
}
