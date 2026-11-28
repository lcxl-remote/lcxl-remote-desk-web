//! What the host user can do with each entry of a file-manager listing.
//!
//! The flags only decide what a controller offers. Every action is still
//! authorized and checked when it runs, because the file system can change
//! between the listing and the click.
use std::path::{Path, PathBuf};

use desk_signal_facade::model::files::FileListResponse;

use super::platform::Probe;

/// Connection policy that bounds the listing flags. `true` means "not
/// explicitly denied": an action that will prompt is still offered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListPolicy {
    pub delete_allowed: bool,
    pub transfer_allowed: bool,
}

/// File-system answers for one entry, as the host user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct EntryAccess {
    pub readable: bool,
    pub deletable: bool,
}

/// Fill the capability flags of `response`. `entry_paths` holds the exact
/// path of each listed entry, in order; `dir` is the listed directory, or
/// `None` for the Windows drive list.
pub(super) fn fill(
    response: &mut FileListResponse,
    entry_paths: &[PathBuf],
    dir: Option<&Path>,
    policy: ListPolicy,
) {
    let probe = Probe::new();
    for (info, path) in response.file_info_list.iter_mut().zip(entry_paths) {
        // `..` is a navigation shortcut for the parent, never a target, and a
        // lossy path string could name a different entry than the one shown.
        if info.name == ".." || path.to_str().is_none() {
            continue;
        }
        let access = probe.entry(path);
        info.can_read = info.is_file && access.readable && policy.transfer_allowed;
        info.can_delete = access.deletable && policy.delete_allowed;
    }
    if let Some(dir) = dir {
        response.can_write = policy.transfer_allowed && probe.dir_writable(dir);
        response.trash_available = probe.trash_available(dir);
    }
}
