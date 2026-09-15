//! Accept device-resolved local drive paths on schedulers running on any OS.
pub(super) fn is_verbatim_local_directory(path: &str) -> bool {
    path.strip_prefix(r"\\?\").is_some_and(|plain| {
        crate::file_scope::windows_path::differs_only_by_verbatim_prefix(plain, path)
    })
}
