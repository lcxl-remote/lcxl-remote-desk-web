use std::collections::BTreeSet;

use desk_agent_protocol::browser_control::{
    BROWSER_CONTROL_SCHEMA_VERSION, BrowserAdapterRef, BrowserElementRef, BrowserElementRole,
    BrowserFormField, BrowserFormFieldReadback, BrowserFormReadbackKind, BrowserNavigationTarget,
    BrowserPageRef, BrowserSemanticSnapshot, MAX_ACCESSIBLE_NAME_BYTES, MAX_BROWSER_ELEMENTS,
    MAX_BROWSER_FORM_TOTAL_BYTES, MAX_BROWSER_FORM_VALUE_BYTES, MAX_BROWSER_ID_BYTES,
};
use rmcp::model::CallToolResult;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use url::Url;

const MAX_UPSTREAM_SNAPSHOT_NODES: usize = 8_192;
const MAX_UPSTREAM_SNAPSHOT_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChromeMcpProjectionError {
    UpstreamToolError,
    MissingStructuredContent,
    InvalidPages,
    SelectedPageMismatch,
    CrossOriginRedirect,
    InvalidSnapshot,
    SnapshotTooDeep,
    DuplicateElement,
    InvalidProjectedContract,
}

pub(super) fn project_opened_page(
    result: &CallToolResult,
    adapter: &BrowserAdapterRef,
    target: &BrowserNavigationTarget,
    observed_at_unix_ms: u64,
) -> Result<BrowserPageRef, ChromeMcpProjectionError> {
    ensure_success(result)?;
    adapter
        .validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    target
        .validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    let structured = result
        .structured_content
        .as_ref()
        .and_then(Value::as_object)
        .ok_or(ChromeMcpProjectionError::MissingStructuredContent)?;
    let pages = structured
        .get("pages")
        .and_then(Value::as_array)
        .ok_or(ChromeMcpProjectionError::InvalidPages)?;
    let selected = pages
        .iter()
        .filter_map(Value::as_object)
        .filter(|page| page.get("selected").and_then(Value::as_bool) == Some(true))
        .collect::<Vec<_>>();
    if selected.len() != 1 {
        return Err(ChromeMcpProjectionError::SelectedPageMismatch);
    }
    let selected = selected[0];
    let page_id = selected
        .get("id")
        .and_then(Value::as_u64)
        .filter(|id| *id != 0)
        .ok_or(ChromeMcpProjectionError::InvalidPages)?;
    let final_url = selected
        .get("url")
        .and_then(Value::as_str)
        .ok_or(ChromeMcpProjectionError::InvalidPages)?;
    if final_url.len() > desk_agent_protocol::browser_control::MAX_BROWSER_URL_BYTES {
        return Err(ChromeMcpProjectionError::InvalidPages);
    }
    let parsed = Url::parse(final_url).map_err(|_| ChromeMcpProjectionError::InvalidPages)?;
    let final_host = parsed
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or(ChromeMcpProjectionError::InvalidPages)?;
    if parsed.scheme()
        != match target.origin.kind {
            desk_agent_protocol::browser_control::BrowserOriginKind::Https => "https",
            desk_agent_protocol::browser_control::BrowserOriginKind::HttpLoopback => "http",
        }
        || final_host != target.origin.host_ascii
        || parsed.port_or_known_default() != Some(target.origin.port)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(ChromeMcpProjectionError::CrossOriginRedirect);
    }

    let page_id = page_id.to_string();
    let page_incarnation = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}:{}:{}",
                adapter.profile_incarnation, adapter.connection_revision, page_id
            )
            .as_bytes()
        )
    );
    let page = BrowserPageRef {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        adapter: adapter.clone(),
        page_id,
        page_incarnation,
        origin: target.origin.clone(),
        document_revision: 1,
        url_sha256: format!("{:x}", Sha256::digest(final_url.as_bytes())),
        observed_at_unix_ms,
    };
    page.validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    Ok(page)
}

/// Reconcile an `OpenPage` whose upstream call timed out after Chrome may have
/// created the tab. This is verification, never a retry: it succeeds only when
/// the post-call inventory contains exactly one new page id, absent from the
/// pre-call inventory, whose canonical origin is the exact approved target.
pub(super) fn project_opened_page_from_inventory_delta(
    before: &CallToolResult,
    after: &CallToolResult,
    adapter: &BrowserAdapterRef,
    target: &BrowserNavigationTarget,
    observed_at_unix_ms: u64,
) -> Result<BrowserPageRef, ChromeMcpProjectionError> {
    ensure_success(before)?;
    ensure_success(after)?;
    adapter
        .validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    target
        .validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    let before_ids = projected_pages(before)?
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|page| page.get("id").and_then(Value::as_u64))
        .filter(|id| *id != 0)
        .collect::<BTreeSet<_>>();
    let expected_scheme = match target.origin.kind {
        desk_agent_protocol::browser_control::BrowserOriginKind::Https => "https",
        desk_agent_protocol::browser_control::BrowserOriginKind::HttpLoopback => "http",
    };
    let matching = projected_pages(after)?
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|page| {
            let id = page
                .get("id")
                .and_then(Value::as_u64)
                .filter(|id| *id != 0)?;
            if before_ids.contains(&id) {
                return None;
            }
            let final_url = page.get("url").and_then(Value::as_str)?;
            let parsed = Url::parse(final_url).ok()?;
            (parsed.scheme() == expected_scheme
                && parsed.host_str().map(str::to_ascii_lowercase).as_deref()
                    == Some(target.origin.host_ascii.as_str())
                && parsed.port_or_known_default() == Some(target.origin.port)
                && parsed.username().is_empty()
                && parsed.password().is_none())
            .then_some((id, final_url))
        })
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(ChromeMcpProjectionError::SelectedPageMismatch);
    }
    let (page_id, final_url) = matching[0];
    let page_id = page_id.to_string();
    let page_incarnation = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}:{}:{}",
                adapter.profile_incarnation, adapter.connection_revision, page_id
            )
            .as_bytes()
        )
    );
    let page = BrowserPageRef {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        adapter: adapter.clone(),
        page_id,
        page_incarnation,
        origin: target.origin.clone(),
        document_revision: 1,
        url_sha256: format!("{:x}", Sha256::digest(final_url.as_bytes())),
        observed_at_unix_ms,
    };
    page.validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    Ok(page)
}

fn projected_pages(result: &CallToolResult) -> Result<&Vec<Value>, ChromeMcpProjectionError> {
    result
        .structured_content
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|structured| structured.get("pages"))
        .and_then(Value::as_array)
        .ok_or(ChromeMcpProjectionError::InvalidPages)
}

pub(super) fn project_snapshot(
    result: &CallToolResult,
    page: BrowserPageRef,
    max_elements: usize,
    captured_at_unix_ms: u64,
) -> Result<BrowserSemanticSnapshot, ChromeMcpProjectionError> {
    ensure_success(result)?;
    if max_elements == 0 || max_elements > MAX_BROWSER_ELEMENTS {
        return Err(ChromeMcpProjectionError::InvalidSnapshot);
    }
    page.validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    let root = result
        .structured_content
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|structured| structured.get("snapshot"))
        .and_then(Value::as_object)
        .ok_or(ChromeMcpProjectionError::MissingStructuredContent)?;
    let mut state = ProjectionState {
        page: &page,
        max_elements,
        visited_nodes: 0,
        elements: Vec::new(),
        element_ids: BTreeSet::new(),
        projected_bytes: 0,
        truncated: false,
    };
    project_node(root, 0, &mut state)?;
    let elements = std::mem::take(&mut state.elements);
    let truncated = state.truncated;
    drop(state);
    let snapshot = BrowserSemanticSnapshot {
        schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
        page,
        elements,
        truncated,
        captured_at_unix_ms,
    };
    snapshot
        .validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    Ok(snapshot)
}

/// Prove only values already present in the authorized FillForm request. Raw
/// page text is inspected at the edge but never projected. A tokenizing
/// combobox may replace its control value with an exact committed StaticText
/// chip; that proof is bound to the nearest raw form ancestor so a reviewed
/// site adapter can correlate it with sibling controls.
pub(super) fn project_form_readback(
    result: &CallToolResult,
    fields: &[BrowserFormField],
) -> Result<Vec<BrowserFormFieldReadback>, ChromeMcpProjectionError> {
    ensure_success(result)?;
    let root = result
        .structured_content
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|structured| structured.get("snapshot"))
        .and_then(Value::as_object)
        .ok_or(ChromeMcpProjectionError::MissingStructuredContent)?;
    let mut nodes = Vec::new();
    let mut visited = 0usize;
    collect_readback_nodes(root, 0, None, &mut visited, &mut nodes)?;

    fields
        .iter()
        .map(|field| {
            let control_matches = nodes
                .iter()
                .filter(|node| {
                    node.id == field.element.element_id
                        && project_role(node.role) == Some(field.element.role)
                        && node.name == field.element.accessible_name
                        && node.value == Some(field.value.as_str())
                })
                .collect::<Vec<_>>();
            if control_matches.len() == 1 {
                let matched = control_matches[0];
                return Ok(BrowserFormFieldReadback {
                    request_element_id: field.element.element_id.clone(),
                    request_role: field.element.role,
                    request_accessible_name: field.element.accessible_name.clone(),
                    source_element_id: matched.id.to_string(),
                    container_element_id: matched.form_ancestor_id.map(str::to_string),
                    kind: BrowserFormReadbackKind::ControlValue,
                    value: field.value.clone(),
                });
            }
            if !control_matches.is_empty() || field.element.role != BrowserElementRole::Combobox {
                return Err(ChromeMcpProjectionError::InvalidSnapshot);
            }
            let committed_matches = nodes
                .iter()
                .filter(|node| {
                    node.role.eq_ignore_ascii_case("statictext")
                        && node.name == field.value
                        && node.form_ancestor_id.is_some()
                })
                .collect::<Vec<_>>();
            if committed_matches.len() != 1 {
                return Err(ChromeMcpProjectionError::InvalidSnapshot);
            }
            let matched = committed_matches[0];
            Ok(BrowserFormFieldReadback {
                request_element_id: field.element.element_id.clone(),
                request_role: field.element.role,
                request_accessible_name: field.element.accessible_name.clone(),
                source_element_id: matched.id.to_string(),
                container_element_id: matched.form_ancestor_id.map(str::to_string),
                kind: BrowserFormReadbackKind::CommittedText,
                value: field.value.clone(),
            })
        })
        .collect()
}

struct RawReadbackNode<'a> {
    id: &'a str,
    role: &'a str,
    name: &'a str,
    value: Option<&'a str>,
    form_ancestor_id: Option<&'a str>,
}

fn collect_readback_nodes<'a>(
    node: &'a Map<String, Value>,
    depth: usize,
    form_ancestor_id: Option<&'a str>,
    visited: &mut usize,
    nodes: &mut Vec<RawReadbackNode<'a>>,
) -> Result<(), ChromeMcpProjectionError> {
    if depth > MAX_UPSTREAM_SNAPSHOT_DEPTH {
        return Err(ChromeMcpProjectionError::SnapshotTooDeep);
    }
    *visited += 1;
    if *visited > MAX_UPSTREAM_SNAPSHOT_NODES {
        return Err(ChromeMcpProjectionError::InvalidSnapshot);
    }
    let id = node.get("id").and_then(Value::as_str).unwrap_or("");
    let role = node.get("role").and_then(Value::as_str).unwrap_or("");
    let name = node.get("name").and_then(Value::as_str).unwrap_or("");
    let next_form_ancestor = if role.eq_ignore_ascii_case("form") && !id.is_empty() {
        Some(id)
    } else {
        form_ancestor_id
    };
    if !id.is_empty() && !role.is_empty() {
        nodes.push(RawReadbackNode {
            id,
            role,
            name,
            value: node.get("value").and_then(Value::as_str),
            form_ancestor_id,
        });
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            collect_readback_nodes(
                child
                    .as_object()
                    .ok_or(ChromeMcpProjectionError::InvalidSnapshot)?,
                depth + 1,
                next_form_ancestor,
                visited,
                nodes,
            )?;
        }
    }
    Ok(())
}

pub(super) fn project_existing_page(
    result: &CallToolResult,
    previous: &BrowserPageRef,
    expected_origin: &desk_agent_protocol::browser_control::BrowserOrigin,
    document_revision: u64,
    observed_at_unix_ms: u64,
) -> Result<BrowserPageRef, ChromeMcpProjectionError> {
    ensure_success(result)?;
    previous
        .validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    expected_origin
        .validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    if document_revision <= previous.document_revision {
        return Err(ChromeMcpProjectionError::InvalidProjectedContract);
    }
    let pages = result
        .structured_content
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|structured| structured.get("pages"))
        .and_then(Value::as_array)
        .ok_or(ChromeMcpProjectionError::InvalidPages)?;
    let page_id = previous
        .page_id
        .parse::<u64>()
        .map_err(|_| ChromeMcpProjectionError::InvalidPages)?;
    let matching = pages
        .iter()
        .filter_map(Value::as_object)
        .filter(|page| page.get("id").and_then(Value::as_u64) == Some(page_id))
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(ChromeMcpProjectionError::InvalidPages);
    }
    let final_url = matching[0]
        .get("url")
        .and_then(Value::as_str)
        .ok_or(ChromeMcpProjectionError::InvalidPages)?;
    let parsed = Url::parse(final_url).map_err(|_| ChromeMcpProjectionError::InvalidPages)?;
    let expected_scheme = match expected_origin.kind {
        desk_agent_protocol::browser_control::BrowserOriginKind::Https => "https",
        desk_agent_protocol::browser_control::BrowserOriginKind::HttpLoopback => "http",
    };
    if parsed.scheme() != expected_scheme
        || parsed.host_str().map(str::to_ascii_lowercase).as_deref()
            != Some(expected_origin.host_ascii.as_str())
        || parsed.port_or_known_default() != Some(expected_origin.port)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(ChromeMcpProjectionError::CrossOriginRedirect);
    }
    let mut page = previous.clone();
    page.origin = expected_origin.clone();
    page.document_revision = document_revision;
    page.url_sha256 = format!("{:x}", Sha256::digest(final_url.as_bytes()));
    page.observed_at_unix_ms = observed_at_unix_ms;
    page.validate()
        .map_err(|_| ChromeMcpProjectionError::InvalidProjectedContract)?;
    Ok(page)
}

/// Project the page that owns the result of an element activation. Chrome may
/// report one selected page per browser window, so global `selected` flags are
/// not a stable ownership signal. Compare the exact pre/post inventory: keep
/// the original page when no page was created, or follow exactly one new
/// same-origin page. Concurrent, ambiguous, or cross-origin creation fails
/// closed. The caller takes a fresh snapshot when `true` is returned because
/// the click result's embedded snapshot still belongs to the source page.
pub(super) fn project_page_after_activation(
    before: &CallToolResult,
    after: &CallToolResult,
    adapter: &BrowserAdapterRef,
    previous: &BrowserPageRef,
    document_revision: u64,
    observed_at_unix_ms: u64,
) -> Result<(BrowserPageRef, bool), ChromeMcpProjectionError> {
    ensure_success(before)?;
    ensure_success(after)?;
    let before_ids = projected_pages(before)?
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|page| page.get("id").and_then(Value::as_u64))
        .filter(|id| *id != 0)
        .collect::<BTreeSet<_>>();
    let new_page_count = projected_pages(after)?
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|page| page.get("id").and_then(Value::as_u64))
        .filter(|id| *id != 0 && !before_ids.contains(id))
        .count();
    if new_page_count > 0 {
        if new_page_count != 1 {
            return Err(ChromeMcpProjectionError::SelectedPageMismatch);
        }
        let scheme = match previous.origin.kind {
            desk_agent_protocol::browser_control::BrowserOriginKind::Https => "https",
            desk_agent_protocol::browser_control::BrowserOriginKind::HttpLoopback => "http",
        };
        let new_page = projected_pages(after)?
            .iter()
            .filter_map(Value::as_object)
            .find(|page| {
                page.get("id")
                    .and_then(Value::as_u64)
                    .is_some_and(|id| id != 0 && !before_ids.contains(&id))
            })
            .ok_or(ChromeMcpProjectionError::InvalidPages)?;
        let new_url = new_page
            .get("url")
            .and_then(Value::as_str)
            .ok_or(ChromeMcpProjectionError::InvalidPages)?;
        let parsed = Url::parse(new_url).map_err(|_| ChromeMcpProjectionError::InvalidPages)?;
        if parsed.scheme() != scheme
            || parsed.host_str().map(str::to_ascii_lowercase).as_deref()
                != Some(previous.origin.host_ascii.as_str())
            || parsed.port_or_known_default() != Some(previous.origin.port)
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(ChromeMcpProjectionError::CrossOriginRedirect);
        }
        let target = BrowserNavigationTarget {
            url: format!(
                "{scheme}://{}:{}/",
                previous.origin.host_ascii, previous.origin.port
            ),
            origin: previous.origin.clone(),
        };
        return project_opened_page_from_inventory_delta(
            before,
            after,
            adapter,
            &target,
            observed_at_unix_ms,
        )
        .map(|page| (page, true));
    }

    project_existing_page(
        after,
        previous,
        &previous.origin,
        document_revision,
        observed_at_unix_ms,
    )
    .map(|page| (page, false))
}

fn ensure_success(result: &CallToolResult) -> Result<(), ChromeMcpProjectionError> {
    if result.is_error == Some(true) {
        return Err(ChromeMcpProjectionError::UpstreamToolError);
    }
    Ok(())
}

struct ProjectionState<'a> {
    page: &'a BrowserPageRef,
    max_elements: usize,
    visited_nodes: usize,
    elements: Vec<BrowserElementRef>,
    element_ids: BTreeSet<String>,
    projected_bytes: usize,
    truncated: bool,
}

fn project_node(
    node: &Map<String, Value>,
    depth: usize,
    state: &mut ProjectionState<'_>,
) -> Result<(), ChromeMcpProjectionError> {
    if depth > MAX_UPSTREAM_SNAPSHOT_DEPTH {
        return Err(ChromeMcpProjectionError::SnapshotTooDeep);
    }
    state.visited_nodes += 1;
    if state.visited_nodes > MAX_UPSTREAM_SNAPSHOT_NODES {
        state.truncated = true;
        return Ok(());
    }

    if let Some(role) = node
        .get("role")
        .and_then(Value::as_str)
        .and_then(project_role)
    {
        let id = node.get("id").and_then(Value::as_str);
        let name = node.get("name").and_then(Value::as_str);
        if let (Some(id), Some(name)) = (id, name) {
            if !id.is_empty()
                && id.len() <= MAX_BROWSER_ID_BYTES
                && !id.chars().any(char::is_control)
                && !name.is_empty()
                && name.len() <= MAX_ACCESSIBLE_NAME_BYTES
            {
                if !state.element_ids.insert(id.to_string()) {
                    return Err(ChromeMcpProjectionError::DuplicateElement);
                }
                if state.elements.len() < state.max_elements {
                    let mut value = project_value(node, role);
                    if value
                        .as_ref()
                        .is_some_and(|value| value.len() > MAX_BROWSER_FORM_VALUE_BYTES)
                    {
                        value = None;
                        state.truncated = true;
                    }
                    let element_bytes = name.len() + value.as_ref().map_or(0, String::len);
                    if state.projected_bytes.saturating_add(element_bytes)
                        > MAX_BROWSER_FORM_TOTAL_BYTES
                    {
                        state.truncated = true;
                        value = None;
                    }
                    let element_bytes = name.len() + value.as_ref().map_or(0, String::len);
                    if state.projected_bytes.saturating_add(element_bytes)
                        > MAX_BROWSER_FORM_TOTAL_BYTES
                    {
                        state.truncated = true;
                    } else {
                        state.projected_bytes += element_bytes;
                        state.elements.push(BrowserElementRef {
                            page_id: state.page.page_id.clone(),
                            page_incarnation: state.page.page_incarnation.clone(),
                            document_revision: state.page.document_revision,
                            element_id: id.to_string(),
                            role,
                            accessible_name: name.to_string(),
                            value,
                            element_revision: state.elements.len() as u64 + 1,
                        });
                    }
                } else {
                    state.truncated = true;
                }
            }
        }
    }

    if state.visited_nodes <= MAX_UPSTREAM_SNAPSHOT_NODES {
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            for child in children {
                let child = child
                    .as_object()
                    .ok_or(ChromeMcpProjectionError::InvalidSnapshot)?;
                project_node(child, depth + 1, state)?;
                if state.visited_nodes > MAX_UPSTREAM_SNAPSHOT_NODES {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn project_value(node: &Map<String, Value>, role: BrowserElementRole) -> Option<String> {
    if !matches!(
        role,
        BrowserElementRole::Textbox | BrowserElementRole::Combobox
    ) {
        return None;
    }
    node.get("value")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn project_role(role: &str) -> Option<BrowserElementRole> {
    match role.to_ascii_lowercase().as_str() {
        "button" => Some(BrowserElementRole::Button),
        "link" => Some(BrowserElementRole::Link),
        "textbox" | "searchbox" => Some(BrowserElementRole::Textbox),
        "checkbox" | "radio" | "switch" => Some(BrowserElementRole::Checkbox),
        "combobox" => Some(BrowserElementRole::Combobox),
        "option" | "menuitem" => Some(BrowserElementRole::Option),
        "tab" => Some(BrowserElementRole::Tab),
        "dialog" | "alertdialog" => Some(BrowserElementRole::Dialog),
        "generic" => Some(BrowserElementRole::Generic),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use desk_agent_protocol::browser_control::{
        BrowserEngineKind, BrowserOrigin, BrowserOriginKind,
    };
    use serde_json::json;

    use super::*;

    fn adapter() -> BrowserAdapterRef {
        BrowserAdapterRef {
            engine: BrowserEngineKind::ChromeDevtoolsMcp,
            device_id: "device-1".into(),
            os_session_id: "session-1".into(),
            browser_major_version: 151,
            browser_version: "151.0.7922.174".into(),
            adapter_id: "chrome-devtools-mcp".into(),
            adapter_version: "1.7.0".into(),
            profile_incarnation: "profile-1".into(),
            connection_revision: 2,
        }
    }

    fn target() -> BrowserNavigationTarget {
        BrowserNavigationTarget {
            url: "http://127.0.0.1:5174/compose".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::HttpLoopback,
                host_ascii: "127.0.0.1".into(),
                port: 5174,
            },
        }
    }

    #[test]
    fn page_projection_selects_only_the_new_same_origin_page() {
        let result = CallToolResult::structured(json!({
            "pages": [
                {"id": 6, "url": "https://example.com/", "title": "Existing", "selected": false},
                {"id": 7, "url": "http://127.0.0.1:5174/compose?draft=1", "title": "Compose", "selected": true}
            ]
        }));
        let page = project_opened_page(&result, &adapter(), &target(), 10).unwrap();
        assert_eq!(page.page_id, "7");
        assert_eq!(page.document_revision, 1);
        assert_eq!(page.url_sha256.len(), 64);
        assert!(!format!("{page:?}").contains("draft=1"));
    }

    #[test]
    fn cross_origin_redirect_and_ambiguous_selection_fail_closed() {
        let redirect = CallToolResult::structured(json!({
            "pages": [{"id": 7, "url": "https://accounts.example.com/login", "selected": true}]
        }));
        assert_eq!(
            project_opened_page(&redirect, &adapter(), &target(), 10),
            Err(ChromeMcpProjectionError::CrossOriginRedirect)
        );

        let ambiguous = CallToolResult::structured(json!({
            "pages": [
                {"id": 7, "url": "http://127.0.0.1:5174/compose", "selected": true},
                {"id": 8, "url": "http://127.0.0.1:5174/other", "selected": true}
            ]
        }));
        assert_eq!(
            project_opened_page(&ambiguous, &adapter(), &target(), 10),
            Err(ChromeMcpProjectionError::SelectedPageMismatch)
        );
    }

    #[test]
    fn timed_out_open_reconciles_only_one_new_exact_origin_page() {
        let before = CallToolResult::structured(json!({
            "pages": [{"id": 6, "url": "https://example.com/", "selected": true}]
        }));
        let after = CallToolResult::structured(json!({
            "pages": [
                {"id": 6, "url": "https://example.com/", "selected": false},
                {"id": 7, "url": "http://127.0.0.1:5174/compose?draft=1", "selected": true}
            ]
        }));
        let page =
            project_opened_page_from_inventory_delta(&before, &after, &adapter(), &target(), 10)
                .unwrap();
        assert_eq!(page.page_id, "7");

        let ambiguous = CallToolResult::structured(json!({
            "pages": [
                {"id": 6, "url": "https://example.com/", "selected": false},
                {"id": 7, "url": "http://127.0.0.1:5174/compose", "selected": false},
                {"id": 8, "url": "http://127.0.0.1:5174/other", "selected": true}
            ]
        }));
        assert_eq!(
            project_opened_page_from_inventory_delta(
                &before,
                &ambiguous,
                &adapter(),
                &target(),
                10,
            ),
            Err(ChromeMcpProjectionError::SelectedPageMismatch)
        );
    }

    #[test]
    fn snapshot_projection_keeps_only_bounded_interactive_semantics() {
        let opened = CallToolResult::structured(json!({
            "pages": [{"id": 7, "url": "http://127.0.0.1:5174/compose", "selected": true}]
        }));
        let mut page = project_opened_page(&opened, &adapter(), &target(), 10).unwrap();
        page.document_revision = 2;
        let result = CallToolResult::structured(json!({
            "snapshot": {
                "id": "root", "role": "RootWebArea", "name": "Compose",
                "children": [
                    {"id": "static", "role": "StaticText", "name": "private body"},
                    {"id": "to", "role": "textbox", "name": "To"},
                    {"id": "send", "role": "button", "name": "Send"}
                ]
            }
        }));
        let snapshot = project_snapshot(&result, page, 1, 11).unwrap();
        assert_eq!(snapshot.elements.len(), 1);
        assert_eq!(snapshot.elements[0].accessible_name, "To");
        assert!(snapshot.truncated);
        assert!(!format!("{snapshot:?}").contains("private body"));
    }

    #[test]
    fn form_readback_proves_tokenized_combobox_without_projecting_other_static_text() {
        let opened = CallToolResult::structured(json!({
            "pages": [{"id": 7, "url": "http://127.0.0.1:5174/compose", "selected": true}]
        }));
        let mut page = project_opened_page(&opened, &adapter(), &target(), 10).unwrap();
        page.document_revision = 2;
        let element = |id: &str, role: BrowserElementRole, name: &str| BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: id.into(),
            role,
            accessible_name: name.into(),
            value: None,
            element_revision: 1,
        };
        let fields = vec![
            BrowserFormField {
                element: element("to-input", BrowserElementRole::Combobox, "To recipients"),
                value: "review@example.invalid".into(),
            },
            BrowserFormField {
                element: element("subject", BrowserElementRole::Textbox, "Subject"),
                value: "Exact subject".into(),
            },
            BrowserFormField {
                element: element("body", BrowserElementRole::Textbox, "Message Body"),
                value: "Exact body".into(),
            },
        ];
        let result = CallToolResult::structured(json!({
            "snapshot": {
                "id": "root", "role": "RootWebArea", "name": "Compose",
                "children": [
                    {
                        "id": "compose-form", "role": "form", "name": "",
                        "children": [
                            {"id": "recipient-wrap", "role": "generic", "name": "", "children": [
                                {"id": "recipient-chip", "role": "StaticText", "name": "review@example.invalid"}
                            ]},
                            {"id": "subject", "role": "textbox", "name": "Subject", "value": "Exact subject"},
                            {"id": "private", "role": "StaticText", "name": "unrelated private text"}
                        ]
                    },
                    {"id": "body", "role": "textbox", "name": "Message Body", "value": "Exact body"}
                ]
            }
        }));

        let readback = project_form_readback(&result, &fields).unwrap();
        assert_eq!(readback.len(), 3);
        assert_eq!(readback[0].kind, BrowserFormReadbackKind::CommittedText);
        assert_eq!(
            readback[0].container_element_id.as_deref(),
            Some("compose-form")
        );
        assert_eq!(readback[1].kind, BrowserFormReadbackKind::ControlValue);
        assert_eq!(
            readback[1].container_element_id.as_deref(),
            Some("compose-form")
        );
        assert_eq!(readback[2].kind, BrowserFormReadbackKind::ControlValue);
        assert_eq!(readback[2].container_element_id, None);

        let snapshot = project_snapshot(&result, page, 32, 11).unwrap();
        assert!(
            snapshot
                .elements
                .iter()
                .all(|element| element.accessible_name != "review@example.invalid")
        );
        assert!(!format!("{snapshot:?}").contains("unrelated private text"));
    }

    #[test]
    fn form_readback_rejects_ambiguous_committed_text() {
        let opened = CallToolResult::structured(json!({
            "pages": [{"id": 7, "url": "http://127.0.0.1:5174/compose", "selected": true}]
        }));
        let mut page = project_opened_page(&opened, &adapter(), &target(), 10).unwrap();
        page.document_revision = 2;
        let fields = vec![BrowserFormField {
            element: BrowserElementRef {
                page_id: page.page_id.clone(),
                page_incarnation: page.page_incarnation.clone(),
                document_revision: page.document_revision,
                element_id: "to-input".into(),
                role: BrowserElementRole::Combobox,
                accessible_name: "To recipients".into(),
                value: None,
                element_revision: 1,
            },
            value: "review@example.invalid".into(),
        }];
        let result = CallToolResult::structured(json!({
            "snapshot": {
                "id": "root", "role": "RootWebArea", "name": "Compose",
                "children": [{
                    "id": "compose-form", "role": "form", "name": "", "children": [
                        {"id": "chip-1", "role": "StaticText", "name": "review@example.invalid"},
                        {"id": "chip-2", "role": "StaticText", "name": "review@example.invalid"}
                    ]
                }]
            }
        }));
        assert_eq!(
            project_form_readback(&result, &fields),
            Err(ChromeMcpProjectionError::InvalidSnapshot)
        );
    }

    #[test]
    fn existing_page_projection_bumps_revision_and_rechecks_origin() {
        let opened = CallToolResult::structured(json!({
            "pages": [{"id": 7, "url": "http://127.0.0.1:5174/compose", "selected": true}]
        }));
        let page = project_opened_page(&opened, &adapter(), &target(), 10).unwrap();
        let listed = CallToolResult::structured(json!({
            "pages": [{"id": 7, "url": "http://127.0.0.1:5174/compose?changed=1", "selected": false}]
        }));
        let updated = project_existing_page(&listed, &page, &page.origin, 2, 11).unwrap();
        assert_eq!(updated.document_revision, 2);
        assert_ne!(updated.url_sha256, page.url_sha256);
    }

    #[test]
    fn activation_follows_one_new_selected_same_origin_tab() {
        let opened = CallToolResult::structured(json!({
            "pages": [{"id": 7, "url": "http://127.0.0.1:5174/launcher", "selected": true}]
        }));
        let page = project_opened_page(&opened, &adapter(), &target(), 10).unwrap();
        let listed = CallToolResult::structured(json!({
            "pages": [
                {"id": 7, "url": "http://127.0.0.1:5174/launcher", "selected": false},
                {"id": 8, "url": "http://127.0.0.1:5174/compose", "selected": true}
            ]
        }));

        let (updated, followed_new_tab) =
            project_page_after_activation(&opened, &listed, &adapter(), &page, 2, 11).unwrap();
        assert!(followed_new_tab);
        assert_eq!(updated.page_id, "8");
        assert_eq!(updated.document_revision, 1);
        assert_eq!(updated.origin, page.origin);
    }

    #[test]
    fn activation_rejects_a_new_selected_cross_origin_tab() {
        let opened = CallToolResult::structured(json!({
            "pages": [{"id": 7, "url": "http://127.0.0.1:5174/launcher", "selected": true}]
        }));
        let page = project_opened_page(&opened, &adapter(), &target(), 10).unwrap();
        let listed = CallToolResult::structured(json!({
            "pages": [
                {"id": 7, "url": "http://127.0.0.1:5174/launcher", "selected": false},
                {"id": 8, "url": "https://example.com/compose", "selected": true}
            ]
        }));

        assert_eq!(
            project_page_after_activation(&opened, &listed, &adapter(), &page, 2, 11),
            Err(ChromeMcpProjectionError::CrossOriginRedirect)
        );
    }

    #[test]
    fn activation_ignores_preexisting_selected_pages_in_other_windows() {
        let before = CallToolResult::structured(json!({
            "pages": [
                {"id": 7, "url": "http://127.0.0.1:5174/compose", "selected": true},
                {"id": 9, "url": "https://example.com/other-window", "selected": true}
            ]
        }));
        let page = project_opened_page_from_inventory_delta(
            &CallToolResult::structured(json!({
                "pages": [{"id": 9, "url": "https://example.com/other-window", "selected": true}]
            })),
            &before,
            &adapter(),
            &target(),
            10,
        )
        .unwrap();
        let after = CallToolResult::structured(json!({
            "pages": [
                {"id": 7, "url": "http://127.0.0.1:5174/compose", "selected": true},
                {"id": 9, "url": "https://example.com/other-window", "selected": true}
            ]
        }));

        let (updated, followed_new_tab) =
            project_page_after_activation(&before, &after, &adapter(), &page, 2, 11).unwrap();
        assert!(!followed_new_tab);
        assert_eq!(updated.page_id, "7");
        assert_eq!(updated.document_revision, 2);
    }
}
