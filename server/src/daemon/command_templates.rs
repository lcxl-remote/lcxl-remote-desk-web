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
    /// The `(epoch, revision)` watermark of the last applied sync. `None` before
    /// the first sync. A sync is applied only when its `(epoch, revision)` is not
    /// strictly older (compared lexicographically, epoch first), so a stale or
    /// rolling-upgrade peer cannot downgrade a newer set — in particular an old
    /// all-org manager (epoch 0) can never re-widen a cache that already accepted a
    /// narrowed set (epoch 1). Reset to `None` on restart (this is in-memory only);
    /// the deployment barrier — upgrade daemons to accept the new epoch before
    /// switching managers to emit it — keeps a restart from reconnecting to an
    /// all-org manager and re-widening.
    watermark: RwLock<Option<(u16, i64)>>,
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

    /// The last applied command-template revision, if any (diagnostics).
    pub fn revision(&self) -> Option<i64> {
        self.watermark
            .read()
            .expect("command template watermark lock")
            .map(|(_, rev)| rev)
    }

    /// The last applied wire epoch, if any (diagnostics / tests).
    #[cfg(test)]
    pub fn epoch(&self) -> Option<u16> {
        self.watermark
            .read()
            .expect("command template watermark lock")
            .map(|(epoch, _)| epoch)
    }

    /// Replace the cache with a synced set, gated on a monotonic `(epoch, revision)`
    /// watermark. A v1 payload carries no revision, read as `0`; a v1 / v2 payload
    /// carries no epoch, read as `0`. Drops any entry whose argv fails the shape
    /// check (fail-closed). Returns `Some(count)` when applied, or `None` when the
    /// frame is rejected as strictly older than the last applied.
    pub fn replace(
        &self,
        templates: Vec<SyncedCommandTemplate>,
        epoch: u16,
        revision: Option<i64>,
    ) -> Option<usize> {
        let incoming = (epoch, revision.unwrap_or(0));
        // Hold the watermark write lock across the check + both updates so two
        // concurrent syncs cannot interleave into a non-monotonic result. Lock order
        // is always watermark-then-inner (readers take only one), so no deadlock.
        let mut mark = self
            .watermark
            .write()
            .expect("command template watermark lock");
        if let Some(current) = *mark
            && incoming < current
        {
            log::warn!(
                "[command-templates] dropping stale sync (epoch {}, revision {}) < current (epoch {}, revision {})",
                incoming.0,
                incoming.1,
                current.0,
                current.1
            );
            return None;
        }
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
        *mark = Some(incoming);
        Some(count)
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
            1,
            Some(7),
        );
        assert_eq!(n, Some(2));
        assert_eq!(cache.snapshot().len(), 2);
        assert_eq!(cache.revision(), Some(7));
        assert_eq!(cache.epoch(), Some(1));
    }

    #[test]
    fn replace_drops_invalid_argv_fail_closed() {
        let cache = CommandTemplateCache::new();
        let n = cache.replace(
            vec![tpl("ok", &["docker", "ps"]), tpl("bad", &["a;b"])],
            1,
            None,
        );
        assert_eq!(n, Some(1), "the metachar entry must be dropped");
        assert_eq!(cache.snapshot().len(), 1);
    }

    #[test]
    fn replace_is_wholesale_and_updates_revision() {
        let cache = CommandTemplateCache::new();
        cache.replace(vec![tpl("a", &["docker", "ps"])], 1, Some(1));
        cache.replace(vec![tpl("b", &["Get-Disk"])], 1, Some(2));
        let snap = cache.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].template_id, "b");
        assert_eq!(cache.revision(), Some(2));
    }

    #[test]
    fn v1_replace_reads_epoch_and_revision_as_zero() {
        // A v1 payload carries neither epoch nor revision; both default to 0 and
        // the watermark records (0, 0).
        let cache = CommandTemplateCache::new();
        cache.replace(vec![tpl("a", &["docker", "ps"])], 0, None);
        assert_eq!(cache.epoch(), Some(0));
        assert_eq!(cache.revision(), Some(0));
    }

    #[test]
    fn stale_revision_within_same_epoch_is_rejected() {
        let cache = CommandTemplateCache::new();
        assert_eq!(
            cache.replace(vec![tpl("a", &["docker", "ps"])], 1, Some(5)),
            Some(1)
        );
        // An out-of-order sync at a lower revision (same epoch) is dropped; the cache
        // keeps the newer set.
        assert_eq!(
            cache.replace(vec![tpl("b", &["Get-Disk"])], 1, Some(4)),
            None
        );
        assert_eq!(cache.snapshot()[0].template_id, "a");
        assert_eq!(cache.revision(), Some(5));
        // The same revision replays idempotently (not strictly older).
        assert_eq!(
            cache.replace(vec![tpl("c", &["uptime"])], 1, Some(5)),
            Some(1)
        );
        assert_eq!(cache.snapshot()[0].template_id, "c");
    }

    #[test]
    fn a_lower_epoch_cannot_downgrade_even_with_a_higher_revision() {
        // Once a narrowed (epoch 1) set is applied, an old all-org manager (epoch 0)
        // cannot re-widen the cache even though its revision counter is far higher —
        // the epoch is compared first (H2, the rolling-upgrade downgrade guard).
        let cache = CommandTemplateCache::new();
        assert_eq!(
            cache.replace(vec![tpl("narrowed", &["docker", "ps"])], 1, Some(2)),
            Some(1)
        );
        assert_eq!(
            cache.replace(vec![tpl("all_org", &["Get-Disk"])], 0, Some(999)),
            None,
            "an epoch-0 payload must never downgrade an epoch-1 cache"
        );
        assert_eq!(cache.snapshot()[0].template_id, "narrowed");
        assert_eq!(cache.epoch(), Some(1));
    }

    #[test]
    fn a_higher_epoch_wins_regardless_of_revision() {
        // A new epoch always outranks the old one, even at a lower revision number
        // (the revision counters are per-epoch semantics and not comparable across
        // epochs).
        let cache = CommandTemplateCache::new();
        assert_eq!(
            cache.replace(vec![tpl("old", &["docker", "ps"])], 0, Some(500)),
            Some(1)
        );
        assert_eq!(
            cache.replace(vec![tpl("new", &["Get-Disk"])], 1, Some(1)),
            Some(1)
        );
        assert_eq!(cache.snapshot()[0].template_id, "new");
        assert_eq!(cache.epoch(), Some(1));
    }
}
