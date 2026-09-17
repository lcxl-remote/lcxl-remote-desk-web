//! Bounded discovery from platform application directories, never a disk scan.
use desk_agent_protocol::application_launch::ApplicationCatalogEntry;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

pub(crate) const MAX_CATALOG_FILES: usize = 16_384;
#[cfg(target_os = "linux")]
pub(crate) const MAX_ENTRY_BYTES: u64 = 1024 * 1024;

#[derive(Default)]
pub(crate) struct CatalogCollection {
    pub(crate) entries: Vec<ApplicationCatalogEntry>,
    pub(crate) warnings: Vec<String>,
    retained_bytes: usize,
}
impl CatalogCollection {
    pub(crate) fn complete(&self) -> bool {
        self.warnings.is_empty()
    }
    pub(crate) fn warn(&mut self, warning: &str) {
        if self.warnings.len() < 32 && !self.warnings.iter().any(|value| value == warning) {
            self.warnings.push(warning.into());
        }
    }
    /// Preserve distinct entry configurations and merge only equivalent sources.
    pub(crate) fn insert(&mut self, entry: ApplicationCatalogEntry) {
        let bytes = match serde_json::to_vec(&entry) {
            Ok(bytes) => bytes.len(),
            Err(_) => {
                self.warn("Application entry metadata could not be encoded");
                return;
            }
        };
        if bytes > 128 * 1024 || self.retained_bytes.saturating_add(bytes) > 8 * 1024 * 1024 {
            self.warn("Application catalog metadata limit reached");
            return;
        }
        self.retained_bytes += bytes;
        if let Some(existing) = self.entries.iter_mut().find(|existing| {
            existing.target.is_some()
                && existing.target == entry.target
                && existing.suggested_args == entry.suggested_args
                && existing.argument_template == entry.argument_template
                && existing.suggested_cwd == entry.suggested_cwd
                && existing.unsupported_reason == entry.unsupported_reason
        }) {
            for alias in std::iter::once(entry.display_name).chain(entry.aliases) {
                if alias != existing.display_name && !existing.aliases.contains(&alias) {
                    existing.aliases.push(alias);
                }
            }
            for source in entry.sources {
                if !existing.sources.contains(&source) {
                    existing.sources.push(source);
                }
            }
        } else if self.entries.len() < MAX_CATALOG_FILES {
            self.entries.push(entry);
        } else {
            self.warn("Application catalog entry limit reached");
        }
    }
}

/// Missing standard directories are normal; unreadable/truncated sources are not.
pub(crate) fn collect_files(
    root: &Path,
    extension: &str,
    collection: &mut CatalogCollection,
) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                collection.warn("An application directory could not be read");
                continue;
            }
        };
        for entry in entries {
            visited += 1;
            if visited > MAX_CATALOG_FILES {
                collection.warn("Application directory traversal limit reached");
                return result;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    collection.warn("An application entry could not be read");
                    continue;
                }
            };
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => {
                    collection.warn("Application entry metadata is unavailable");
                    continue;
                }
            };
            // Do not escape known roots through filesystem links or junctions.
            if kind.is_symlink() {
                collection.warn("Linked application entries were not traversed");
                continue;
            }
            #[cfg(windows)]
            if windows::is_reparse_entry(&entry) {
                collection.warn("Redirected application entries were not traversed");
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                if depth < 8 {
                    pending.push((path, depth + 1));
                } else {
                    collection.warn("Application directory depth limit reached");
                }
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|value| value.eq_ignore_ascii_case(extension))
            {
                result.push(path);
            }
        }
    }
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::application_launch::{ApplicationTarget, ApplicationTargetKind};
    #[test]
    fn equivalent_sources_merge_but_distinct_arguments_remain_separate() {
        let entry = ApplicationCatalogEntry {
            display_name: "Editor".into(),
            aliases: vec![],
            target: Some(ApplicationTarget {
                kind: ApplicationTargetKind::Executable,
                value: "/editor".into(),
            }),
            suggested_args: Some(vec![]),
            argument_template: None,
            suggested_cwd: None,
            sources: vec!["user".into()],
            unsupported_reason: None,
        };
        let mut collection = CatalogCollection::default();
        collection.insert(entry.clone());
        let mut alias = entry.clone();
        alias.display_name = "编辑器".into();
        alias.sources = vec!["system".into()];
        collection.insert(alias);
        assert_eq!(collection.entries.len(), 1);
        assert_eq!(collection.entries[0].aliases, ["编辑器"]);
        let mut different = entry;
        different.suggested_args = Some(vec!["--private".into()]);
        collection.insert(different);
        assert_eq!(collection.entries.len(), 2);
    }
}
