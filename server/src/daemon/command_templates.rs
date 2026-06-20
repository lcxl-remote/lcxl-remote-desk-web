//! Daemon-side cache of operator command templates synced from the manager.
//!
//! The manager (the trusted upstream) pushes the full enabled operator template
//! set over a `CommandTemplateSync` frame on link establishment and on every
//! change; the daemon replaces this cache wholesale and the exec classifier
//! unions it with the compiled-in built-in baseline. Single-machine and
//! remote-signaling links never populate this (no manager), so they classify
//! against the built-in baseline only.
//!
//! Ingestion is fail-closed: an entry whose argv fails the shape check is
//! dropped, so a buggy or compromised upstream cannot inject a metacharacter
//! argv that the worker would spawn.

use std::sync::{Arc, RwLock};

use desk_agent_protocol::command_template::{SyncedCommandTemplate, validate_template_argv};

/// Thread-safe cache of the operator templates. Reads (one per `ConfirmExec`)
/// take a cheap `Arc` snapshot; writes (one per sync) replace the whole set.
#[derive(Default)]
pub struct CommandTemplateCache {
    inner: RwLock<Arc<Vec<SyncedCommandTemplate>>>,
    /// The shared command-template revision last applied (from a v2 sync). `None`
    /// before the first v2 sync. Stored for diagnostics only — the daemon does not
    /// ACK it (no per-device applied-revision tracking in v1).
    revision: RwLock<Option<i64>>,
}

impl CommandTemplateCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// A cheap snapshot of the current operator templates for classification.
    pub fn snapshot(&self) -> Arc<Vec<SyncedCommandTemplate>> {
        self.inner
            .read()
            .expect("command template cache lock")
            .clone()
    }

    /// The last applied command-template revision, if any.
    pub fn revision(&self) -> Option<i64> {
        *self
            .revision
            .read()
            .expect("command template revision lock")
    }

    /// Replace the cache with a synced set, dropping any entry whose argv fails
    /// the shape check (fail-closed). `revision` is the sync payload's revision
    /// (`None` for a v1 payload). Returns the number of templates accepted.
    pub fn replace(&self, templates: Vec<SyncedCommandTemplate>, revision: Option<i64>) -> usize {
        let accepted: Vec<SyncedCommandTemplate> = templates
            .into_iter()
            .filter(|t| match validate_template_argv(&t.argv) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!(
                        "[command-templates] dropping template {}: {e}",
                        t.template_id
                    );
                    false
                }
            })
            .collect();
        let count = accepted.len();
        *self.inner.write().expect("command template cache lock") = Arc::new(accepted);
        *self
            .revision
            .write()
            .expect("command template revision lock") = revision;
        count
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("command template cache lock")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::exec::ExecEffect;

    fn tpl(id: &str, argv: &[&str]) -> SyncedCommandTemplate {
        SyncedCommandTemplate {
            template_id: id.to_string(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            effect: ExecEffect::ReadOnly,
        }
    }

    #[test]
    fn replace_accepts_valid_and_snapshots() {
        let cache = CommandTemplateCache::new();
        assert_eq!(cache.snapshot().len(), 0);
        let n = cache.replace(
            vec![tpl("a", &["docker", "ps"]), tpl("b", &["Get-Disk"])],
            Some(7),
        );
        assert_eq!(n, 2);
        assert_eq!(cache.snapshot().len(), 2);
        assert_eq!(cache.revision(), Some(7));
    }

    #[test]
    fn replace_drops_invalid_argv_fail_closed() {
        let cache = CommandTemplateCache::new();
        let n = cache.replace(
            vec![tpl("ok", &["docker", "ps"]), tpl("bad", &["a;b"])],
            None,
        );
        assert_eq!(n, 1, "the metachar entry must be dropped");
        assert_eq!(cache.snapshot().len(), 1);
    }

    #[test]
    fn replace_is_wholesale_and_updates_revision() {
        let cache = CommandTemplateCache::new();
        cache.replace(vec![tpl("a", &["docker", "ps"])], Some(1));
        cache.replace(vec![tpl("b", &["Get-Disk"])], Some(2));
        let snap = cache.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].template_id, "b");
        assert_eq!(cache.revision(), Some(2));
    }

    #[test]
    fn v1_replace_leaves_revision_none() {
        let cache = CommandTemplateCache::new();
        cache.replace(vec![tpl("a", &["docker", "ps"])], None);
        assert_eq!(cache.revision(), None);
    }
}
