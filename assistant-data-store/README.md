# Personal assistant storage

This crate owns the common SeaORM entities and bounded storage operations used by
Manager and standalone Signal. It uses the caller's connection or transaction;
it does not open databases, select a SQL driver, create runtime tasks, or acquire
native account/device/session authority.

- `entity`: shared table shapes and explicit schema registration.
- `projection`, `retrieval_cache`, `citations`: immutable read snapshots, frozen
  query pages and display-only navigation references.
- `keyword_search`, `memory_keyword`, `history_query`: bounded SQL candidates and
  sparse ranking. Host source readers retain all original source authorization.
- `memory_control`, `suppression`, `evidence`: forgetting, source suppression and
  authenticated evidence-link decoding.
- `daily_budget`, `maintenance_request`: metadata accounting and immutable
  maintenance request bindings. Host adapters check current policy, funds and
  native call receipts.
- `registered_fact`: exact registered-family inspection and original SQL fences.
- `fact_mutation`: shared replay, row CAS, version retirement and runtime updates.
- `memory_publication`: immutable publication receipts, including a source-only
  registered insert with no relation provider call.
- `source_lifecycle`: generation revocation after the host authenticates deletion.
- `memory_quota`, `memory_index`, `memory_write_images`: exact resource deltas,
  index writes and original/post-write row validation.

A snapshot establishes immutable content identity. It does not authorize model
use, revive a deleted source, extend expiry, or replace the original host's
SELECT/version/lease/raw-JSON fences. Native writes finish before the original
transaction's final assistant quota mutation. Errors require rollback of the
whole caller transaction.

Maintenance workers belong to each host's application layer and construct its
model adapter there; storage modules do not construct provider adapters.

Manager keeps PostgreSQL actor locking, platform configuration, personal account
and device checks, and billing. Signal keeps SQLite writer admission, file-based
configuration and local instance binding. User-visible APIs are unchanged.

Manager's business schema registers these tables through `desk_database::schema`;
Signal uses its existing explicit SQLite schema initialization. The optional
`schema-sync` feature is enabled by Manager; Signal does not pull in schema-sync
as a production dependency. Re-exported
entities in both hosts refer to these same Rust types. Signal retains its quota
entity extension for native cleanup cursors; common accounting accesses only the
shared counter fields and preserves those host-only columns. No migration or backfill
is provided for this unreleased feature.

Run the crate's unit tests from the `web` workspace (`cargo test -p
desk-assistant-data-store --lib`). Manager is in the parent workspace and tests
the same operations through its own SQLite and PostgreSQL host integration tests.
Both build contexts collect independent frame reports for this crate.
