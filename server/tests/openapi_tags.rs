//! Integration test: every `#[utoipa::path]` exposed by the desk-server
//! binary must carry a non-empty `tags` field whose entries are also non-empty
//! and not literally `"undefined"`. Without this guard the generated
//! `openapi.json` regresses to empty tags, which makes Kubb fall back to the
//! `undefinedController/` group and produces a broken frontend layout.
//!
//! Coverage source: [`lcxl_remote_desk_server::openapi::AllPathsDoc`] —
//! the static `OpenApi` derive listing every handler the binary can route
//! at runtime (including `StartupMode`-conditional ones like device-code
//! CRUD, signaling, and TURN). New handlers must be added to that list when
//! they are introduced; this test fails if any listed operation lacks a tag.

use lcxl_remote_desk_server::openapi::AllPathsDoc;
use utoipa::OpenApi;
use utoipa::openapi::path::{Operation, PathItem};

/// PathItem stores each HTTP method on its own field (`get`, `post`, `put`,
/// `delete`, `options`, `head`, `patch`, `trace`) rather than a generic map,
/// so iterate them explicitly.
fn operations_of(item: &PathItem) -> Vec<(&'static str, &Operation)> {
    [
        ("GET", item.get.as_ref()),
        ("PUT", item.put.as_ref()),
        ("POST", item.post.as_ref()),
        ("DELETE", item.delete.as_ref()),
        ("OPTIONS", item.options.as_ref()),
        ("HEAD", item.head.as_ref()),
        ("PATCH", item.patch.as_ref()),
        ("TRACE", item.trace.as_ref()),
    ]
    .into_iter()
    .filter_map(|(m, o)| o.map(|op| (m, op)))
    .collect()
}

#[test]
fn all_operations_have_non_empty_tags() {
    let api = AllPathsDoc::openapi();
    let mut violations: Vec<String> = Vec::new();
    let mut total_operations = 0_usize;
    for (path, item) in &api.paths.paths {
        for (method, op) in operations_of(item) {
            total_operations += 1;
            let tags = op.tags.as_deref().unwrap_or(&[]);
            if tags.is_empty() {
                violations.push(format!("{method} {path}: missing tags"));
                continue;
            }
            for t in tags {
                if t.is_empty() || t.eq_ignore_ascii_case("undefined") {
                    violations.push(format!("{method} {path}: invalid tag {t:?}"));
                }
            }
        }
    }
    assert!(
        total_operations > 0,
        "AllPathsDoc appears empty — registration regression"
    );
    assert!(
        violations.is_empty(),
        "OpenAPI tag policy violations across {total_operations} operations:\n  {}",
        violations.join("\n  ")
    );
}
