//! Session-fenced opaque catalog cursors; validation precedes native enumeration.
use super::catalog::CatalogCollection;
use desk_agent_protocol::application_launch::{ApplicationCatalogPage, ListApplicationsRequest};
use desk_diagnose_core::application_launch::ApplicationCatalogSnapshot;
use std::collections::HashMap;

const SNAPSHOT_TTL_MS: u64 = 5 * 60 * 1000;
const MAX_SNAPSHOTS: usize = 8;

struct RetainedCatalog {
    expires_at: u64,
    snapshot: ApplicationCatalogSnapshot,
}

#[derive(Default)]
pub(crate) struct ApplicationCatalogStore {
    snapshots: HashMap<String, RetainedCatalog>,
    cursors: HashMap<String, String>,
}

impl ApplicationCatalogStore {
    pub(crate) fn list(
        &mut self,
        device_id: &str,
        session_id: &str,
        now_unix_ms: u64,
        mut request: ListApplicationsRequest,
        enumerate: impl FnOnce() -> Result<CatalogCollection, &'static str>,
    ) -> Result<ApplicationCatalogPage, &'static str> {
        request.normalize()?;
        if device_id.is_empty() || session_id.is_empty() || now_unix_ms == 0 {
            return Err("application discovery has no authenticated session");
        }
        self.snapshots
            .retain(|_, value| value.expires_at > now_unix_ms);
        self.cursors
            .retain(|_, snapshot| self.snapshots.contains_key(snapshot));
        let snapshot_id = if let Some(cursor) = &request.cursor {
            self.cursors
                .get(cursor)
                .cloned()
                .ok_or("application cursor expired or unknown")?
        } else {
            if self.snapshots.len() >= MAX_SNAPSHOTS {
                // Do not invalidate an in-flight cursor silently to make room.
                return Err(
                    "application catalog snapshot limit reached; finish existing pages or wait for expiry",
                );
            }
            let collection = enumerate()?;
            let complete = collection.complete();
            let expires_at = now_unix_ms.saturating_add(SNAPSHOT_TTL_MS);
            let snapshot = ApplicationCatalogSnapshot::new(
                device_id.into(),
                session_id.into(),
                expires_at,
                request.clone(),
                collection.entries,
                complete,
                collection.warnings,
            )?;
            let id = uuid::Uuid::new_v4().to_string();
            self.snapshots.insert(
                id.clone(),
                RetainedCatalog {
                    expires_at,
                    snapshot,
                },
            );
            id
        };
        let continuation = request.cursor.is_some();
        let retained = self
            .snapshots
            .get_mut(&snapshot_id)
            .ok_or("application cursor expired")?;
        let page = retained.snapshot.page(
            device_id,
            session_id,
            now_unix_ms,
            request,
            uuid::Uuid::new_v4().to_string(),
        )?;
        if let Some(cursor) = &page.next_cursor {
            self.cursors.insert(cursor.clone(), snapshot_id);
        } else if !continuation {
            self.snapshots.remove(&snapshot_id);
        }
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn missing_search_and_unknown_cursor_never_invoke_native_enumeration() {
        let calls = Cell::new(0);
        let mut store = ApplicationCatalogStore::default();
        let mut request: ListApplicationsRequest = serde_json::from_str("{}").unwrap();
        assert!(
            store
                .list("device", "session", 1, request.clone(), || {
                    calls.set(calls.get() + 1);
                    Ok(CatalogCollection::default())
                })
                .is_err()
        );
        request.allow_unfiltered = true;
        request.cursor = Some("forged".into());
        assert!(
            store
                .list("device", "session", 1, request, || {
                    calls.set(calls.get() + 1);
                    Ok(CatalogCollection::default())
                })
                .is_err()
        );
        assert_eq!(calls.get(), 0);
    }
}
