//! Session-owned model aliases. Native identities and authorization stay intact.

use std::collections::{BTreeMap, BTreeSet};

use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{ObjectKind, ObjectRef},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::chat::{ChatMessage, ChatRole, ToolCall};

#[derive(Clone, Default, Serialize)]
pub struct UiReferenceState {
    counters: BTreeMap<String, u64>,
    entries: BTreeMap<String, ReferenceEntry>,
    #[serde(skip)]
    identity_index: BTreeMap<String, String>,
    #[serde(skip)]
    native_index: BTreeMap<(String, String), BTreeSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum ReferenceSource {
    Message(String),
    Attachment { id: String, message: String },
    Business(String),
}

impl PartialEq for UiReferenceState {
    fn eq(&self, other: &Self) -> bool {
        self.counters == other.counters && self.entries == other.entries
    }
}

impl Eq for UiReferenceState {}

impl<'de> Deserialize<'de> for UiReferenceState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            counters: BTreeMap<String, u64>,
            entries: BTreeMap<String, ReferenceEntry>,
        }
        let stored = Stored::deserialize(deserializer)?;
        let mut state = Self {
            counters: stored.counters,
            entries: stored.entries,
            identity_index: BTreeMap::new(),
            native_index: BTreeMap::new(),
        };
        state.rebuild_indexes();
        Ok(state)
    }
}

impl std::fmt::Debug for UiReferenceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiReferenceState")
            .field("entries", &self.entries.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReferenceEntry {
    identity: String,
    prefix: String,
    native_id: String,
    reference: Value,
    sources: BTreeSet<ReferenceSource>,
}

fn invalid(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn native_reference(value: &Value) -> Option<(&'static str, String)> {
    if value.get("token").is_some()
        && value.get("object_kind").is_some()
        && value.get("snapshot_id").is_some()
        && value.get("expires_at").is_some()
        && let Ok(reference) = serde_json::from_value::<ObjectRef>(value.clone())
    {
        let prefix = match reference.object_kind {
            ObjectKind::DesktopSession => "s",
            ObjectKind::Application => "a",
            ObjectKind::Window => "w",
            ObjectKind::UiElement => "e",
            ObjectKind::DesktopOutput => "o",
            ObjectKind::OfficeDocument => "doc",
            ObjectKind::Document => "doc",
            ObjectKind::Worksheet => "ws",
            ObjectKind::Range => "r",
            ObjectKind::Presentation => "pr",
            ObjectKind::Slide => "sl",
            ObjectKind::Shape => "sh",
            ObjectKind::File => "f",
            ObjectKind::Directory => "d",
            ObjectKind::TerminalOutput => "term",
            ObjectKind::BrowserSurface => "bs",
            ObjectKind::ApplicationLaunchTarget => "al",
        };
        return Some((prefix, reference.token));
    }
    if value.get("adapter").is_some()
        && value.get("page_incarnation").is_some()
        && let Ok(page) = serde_json::from_value::<
            desk_agent_protocol::browser_control::BrowserPageRef,
        >(value.clone())
    {
        return Some(("p", page.page_id));
    }
    if value.get("element_revision").is_some()
        && let Ok(element) = serde_json::from_value::<
            desk_agent_protocol::browser_control::BrowserElementRef,
        >(value.clone())
    {
        return Some(("b", element.element_id));
    }
    None
}

pub(crate) fn is_business_content(field: &str) -> bool {
    matches!(
        field,
        "rows"
            | "cells"
            | "values"
            | "data"
            | "body"
            | "body_plain_text"
            | "content"
            | "content_utf8"
            | "arguments"
            | "note"
            | "answer"
            | "report"
    )
}

fn visit_references(value: &Value, visitor: &mut impl FnMut(&Value)) {
    if native_reference(value).is_some() {
        visitor(value);
        return;
    }
    match value {
        Value::Object(object) => object
            .iter()
            .filter(|(field, _)| !is_business_content(field))
            .for_each(|(_, v)| visit_references(v, visitor)),
        Value::Array(array) => array.iter().for_each(|v| visit_references(v, visitor)),
        _ => {}
    }
}

impl UiReferenceState {
    fn rebuild_indexes(&mut self) {
        self.identity_index.clear();
        self.native_index.clear();
        for (id, entry) in &self.entries {
            self.identity_index
                .insert(entry.identity.clone(), id.clone());
            self.native_index
                .entry((entry.prefix.clone(), entry.native_id.clone()))
                .or_default()
                .insert(id.clone());
        }
    }

    fn identity(prefix: &str, native: &str, value: &Value) -> String {
        format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(prefix, native, value))
                    .expect("reference identity serialization"),
            )
        )
    }

    fn register(&mut self, value: &Value, source: &str) -> Result<(), AgentError> {
        let Some((prefix, native_id)) = native_reference(value) else {
            return Ok(());
        };
        self.register_reference(prefix, native_id, value, source)
    }

    fn register_reference(
        &mut self,
        prefix: &str,
        native_id: String,
        value: &Value,
        source: &str,
    ) -> Result<(), AgentError> {
        self.register_entry(
            prefix,
            native_id,
            value,
            ReferenceSource::Message(source.into()),
        )
    }

    fn register_entry(
        &mut self,
        prefix: &str,
        native_id: String,
        value: &Value,
        source: ReferenceSource,
    ) -> Result<(), AgentError> {
        let identity = Self::identity(prefix, &native_id, value);
        if let Some(id) = self.identity_index.get(&identity) {
            let entry = self.entries.get_mut(id).expect("derived reference index");
            if entry.prefix != prefix || entry.native_id != native_id || entry.reference != *value {
                return Err(invalid(
                    "Reference identity collision; no action was executed",
                ));
            }
            entry.sources.insert(source);
            return Ok(());
        }
        let counter = self.counters.entry(prefix.into()).or_default();
        *counter = counter
            .checked_add(1)
            .ok_or_else(|| invalid("Conversation reference sequence exhausted"))?;
        let id = format!("{prefix}{counter}");
        self.identity_index.insert(identity.clone(), id.clone());
        self.native_index
            .entry((prefix.into(), native_id.clone()))
            .or_default()
            .insert(id.clone());
        self.entries.insert(
            id,
            ReferenceEntry {
                identity,
                prefix: prefix.into(),
                native_id,
                reference: value.clone(),
                sources: BTreeSet::from([source]),
            },
        );
        Ok(())
    }

    /// Accepted device observations allocate aliases; native business IDs do not.
    /// Ordering follows the result structure; request projection allocates no IDs.
    pub fn observe(&mut self, history: &[ChatMessage]) -> Result<bool, AgentError> {
        let before = self.clone();
        let verified = crate::result_model_ids::verified_results(history, 0);
        for message in history {
            let source = message.trusted_tool_result();
            let browser = crate::agent_loop::verified_browser_result(source).is_some();
            let native = message.role == ChatRole::Tool
                && message.tool_call_id.as_ref().is_some_and(|id| {
                    history.iter().flat_map(|m| &m.tool_calls).any(|call| {
                        call.id == *id
                            && matches!(
                                call.name.as_str(),
                                "inspect_desktop_ui"
                                    | "inspect_desktop_session"
                                    | "read_current_screen"
                            )
                    })
                });
            let accepted = message.tool_call_id.as_ref().and_then(|id| {
                verified
                    .iter()
                    .find(|(call, _)| call.id == *id)
                    .map(|(call, _)| call.name.as_str())
            });
            if !native && !browser && accepted.is_none() {
                continue;
            }
            let Ok(mut value) = crate::image_input::structured_tool_result(&source.text) else {
                continue;
            };
            if native || accepted.is_some() {
                crate::ui_model_output::expand_value(&mut value);
            }
            let mut error = None;
            visit_references(&value, &mut |reference| {
                if error.is_none() {
                    error = self.register(reference, &message.message_id).err();
                }
            });
            if let Some(error) = error {
                return Err(error);
            }
            if let Some(tool) = accepted {
                self.observe_cursors(
                    tool,
                    &value,
                    ReferenceSource::Message(message.message_id.clone()),
                )?;
            }
        }
        Ok(*self != before)
    }

    fn cursor_paths(tool: &str) -> &'static [&'static str] {
        match tool {
            "inspect_files" => &["/ReadContext/FileMetadataInspect/next_cursor"],
            "read_service_status" => &["/ReadContext/ServiceStatus/next_cursor", "/next_cursor"],
            "list_applications" => &["/next_cursor", "/ReadContext/ApplicationList/next_cursor"],
            "list_subagents" => &["/next_cursor"],
            "read_conversation_attachment" => &["/cursor"],
            _ => &[],
        }
    }

    fn observe_cursors(
        &mut self,
        tool: &str,
        value: &Value,
        source: ReferenceSource,
    ) -> Result<(), AgentError> {
        for path in Self::cursor_paths(tool) {
            if let Some(cursor) = value.pointer(path).and_then(Value::as_str)
                && !cursor.is_empty()
                && cursor.parse::<i64>().is_err()
            {
                let reference = serde_json::json!({"cursor":cursor,"tool_name":tool});
                self.register_entry("c", cursor.into(), &reference, source.clone())?;
            }
        }
        Ok(())
    }

    pub(crate) fn observe_attachment(
        &mut self,
        part: &crate::conversation_attachment::batch::PreparedAttachment,
    ) -> Result<bool, AgentError> {
        part.metadata.verify(&part.content)?;
        let Some(envelope) = &part.metadata.source_envelope else {
            return Ok(false);
        };
        if part.metadata.kind != crate::conversation_attachment::ContentKind::Json {
            return Ok(false);
        }
        if envelope.validate().is_err() || part.metadata.storage_truncated {
            return Ok(false);
        }
        let Ok(mut value) = crate::image_input::structured_tool_result(
            std::str::from_utf8(&part.content)
                .map_err(|_| invalid("Invalid attachment encoding"))?,
        ) else {
            return Ok(false);
        };
        crate::ui_model_output::expand_value(&mut value);
        let before = self.clone();
        let source = ReferenceSource::Attachment {
            id: part.metadata.attachment_id.clone(),
            message: part.metadata.message_id.clone(),
        };
        let mut error = None;
        visit_references(&value, &mut |reference| {
            if error.is_none()
                && let Some((prefix, native)) = native_reference(reference)
            {
                error = self
                    .register_entry(prefix, native, reference, source.clone())
                    .err();
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        self.observe_cursors(&envelope.provenance.source_tool_name, &value, source)?;
        Ok(*self != before)
    }

    pub(crate) fn attachment_cursor_alias(&self, cursor: &str) -> Result<String, AgentError> {
        let value = serde_json::json!({"cursor":cursor,"tool_name":"read_conversation_attachment"});
        if let Some(id) = self
            .identity_index
            .get(&Self::identity("c", cursor, &value))
        {
            return Ok(id.clone());
        }
        let next = self
            .counters
            .get("c")
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("Conversation reference sequence exhausted"))?;
        Ok(format!("c{next}"))
    }

    pub(crate) fn register_attachment_cursor(
        &mut self,
        attachment: &str,
        message: &str,
        cursor: &str,
    ) -> Result<String, AgentError> {
        let value = serde_json::json!({"cursor":cursor,"tool_name":"read_conversation_attachment"});
        self.register_entry(
            "c",
            cursor.into(),
            &value,
            ReferenceSource::Attachment {
                id: attachment.into(),
                message: message.into(),
            },
        )?;
        self.identity_index
            .get(&Self::identity("c", cursor, &value))
            .cloned()
            .ok_or_else(|| invalid("Attachment cursor was not registered"))
    }

    pub(crate) fn retain_attachment_sources(
        &mut self,
        available: &BTreeSet<String>,
        protected: &BTreeSet<String>,
    ) {
        for entry in self.entries.values_mut() {
            entry.sources.retain(|source| match source {
                ReferenceSource::Attachment { id, .. } => available.contains(id),
                _ => true,
            });
        }
        self.entries
            .retain(|id, entry| !entry.sources.is_empty() || protected.contains(id));
        self.rebuild_indexes();
    }

    pub(crate) fn has_attachment_sources(&self) -> bool {
        self.entries.values().any(|entry| {
            entry
                .sources
                .iter()
                .any(|source| matches!(source, ReferenceSource::Attachment { .. }))
        })
    }

    pub(crate) fn observe_directories(
        &mut self,
        scope: &crate::file_scope::SessionFileScope,
        protected: &BTreeSet<String>,
    ) -> Result<bool, AgentError> {
        let before = self.clone();
        let mut live = BTreeSet::new();
        for record in scope.records() {
            if record.state != crate::file_scope::DirectoryConsentState::Approved {
                continue;
            }
            let id = record.proposal.request_id.clone();
            live.insert(id.clone());
            let reference = serde_json::to_value(&record.proposal.directory)
                .expect("directory reference serialization");
            if let Some((prefix, native)) = native_reference(&reference) {
                self.register_entry(prefix, native, &reference, ReferenceSource::Business(id))?;
            }
        }
        for entry in self.entries.values_mut() {
            entry.sources.retain(|source| match source {
                ReferenceSource::Business(id) => live.contains(id),
                _ => true,
            });
        }
        self.entries
            .retain(|id, entry| !entry.sources.is_empty() || protected.contains(id));
        self.rebuild_indexes();
        Ok(*self != before)
    }

    fn project_cursors(&self, tool: &str, value: &mut Value) {
        for path in Self::cursor_paths(tool) {
            if let Some(id) = value.pointer_mut(path)
                && let Some(native) = id.as_str()
            {
                let reference = serde_json::json!({"cursor":native,"tool_name":tool});
                if let Some(alias) = self
                    .identity_index
                    .get(&Self::identity("c", native, &reference))
                {
                    *id = Value::String(alias.clone());
                }
            }
        }
    }

    /// Retire unreachable mappings without reusing their sequence numbers.
    pub fn retain_sources(
        &mut self,
        sources: &BTreeSet<String>,
        protected: &BTreeSet<String>,
    ) -> bool {
        let mut changed = false;
        self.entries.retain(|id, entry| {
            let before = entry.sources.len();
            entry.sources.retain(|source| match source {
                ReferenceSource::Message(id) => sources.contains(id),
                ReferenceSource::Attachment { .. } | ReferenceSource::Business(_) => true,
            });
            let retained = !entry.sources.is_empty() || protected.contains(id);
            changed |= !retained || before != entry.sources.len();
            retained
        });
        self.rebuild_indexes();
        changed
    }

    pub(crate) fn retained_call_aliases(
        &self,
        history: &[ChatMessage],
        requests: &[crate::dynamic_run::PermissionRequest],
    ) -> BTreeSet<String> {
        fn collect(
            state: &UiReferenceState,
            tool: &str,
            value: &Value,
            ids: &mut BTreeSet<String>,
        ) {
            let ui = matches!(
                tool,
                "inspect_desktop_ui"
                    | "read_current_screen"
                    | "execute_ui_actions"
                    | "send_background_input"
                    | "send_raw_input"
                    | "execute_wayland_output_input"
                    | "preview_computer_action"
            ) || crate::browser_model_ids::supports(tool);
            if ui {
                if let Some(alias) = value
                    .pointer("/application_scope/application")
                    .and_then(|reference| state.projected_alias(reference))
                {
                    ids.insert(alias.into());
                }
                if let Some(id) = value
                    .pointer("/application_scope/application_id")
                    .and_then(Value::as_str)
                    && state.entries.contains_key(id)
                {
                    ids.insert(id.into());
                }
                for field in [
                    "application",
                    "target",
                    "root",
                    "window",
                    "page",
                    "element",
                    "composer",
                    "send_control",
                    "to_field",
                    "subject_field",
                    "body_field",
                ] {
                    if let Some(alias) = value
                        .get(field)
                        .and_then(|reference| state.projected_alias(reference))
                    {
                        ids.insert(alias.into());
                    }
                }
                for field in [
                    "application_id",
                    "window_id",
                    "root_id",
                    "output_id",
                    "page_id",
                    "element_id",
                    "composer_id",
                    "send_control_id",
                    "to_field_id",
                    "subject_field_id",
                    "body_field_id",
                ] {
                    if let Some(id) = value.get(field).and_then(Value::as_str)
                        && state.entries.contains_key(id)
                    {
                        ids.insert(id.into());
                    }
                }
                for field in ["steps", "fields"] {
                    if let Some(items) = value.get(field).and_then(Value::as_array) {
                        for item in items {
                            collect(state, tool, item, ids);
                        }
                    }
                }
                if let Some(action) = value.get("action") {
                    collect(state, tool, action, ids);
                }
                if let Some(params) = value.get("params") {
                    collect(state, tool, params, ids);
                }
            }
            if crate::document_model_ids::supports(tool) {
                for field in ["target", "document"] {
                    if let Some(alias) = value
                        .get(field)
                        .and_then(|reference| state.projected_alias(reference))
                    {
                        ids.insert(alias.into());
                    }
                }
                for field in ["target_id", "document_id"] {
                    if let Some(id) = value.get(field).and_then(Value::as_str)
                        && state.entries.contains_key(id)
                    {
                        ids.insert(id.into());
                    }
                }
                if tool == "preview_computer_action"
                    && let Some(actions) = value.get("actions").and_then(Value::as_array)
                {
                    for action in actions {
                        collect(state, tool, action, ids);
                    }
                }
            }
            if !UiReferenceState::cursor_paths(tool).is_empty()
                && let Some(id) = value.get("cursor").and_then(Value::as_str)
                && state
                    .entries
                    .get(id)
                    .is_some_and(|entry| entry.prefix == "c")
            {
                ids.insert(id.into());
            }
            if tool == "request_permissions"
                && let Some(items) = value.get("items").and_then(Value::as_array)
            {
                for item in items {
                    if let Some(alias) = item
                        .pointer("/application_scope/application")
                        .and_then(|reference| state.projected_alias(reference))
                    {
                        ids.insert(alias.into());
                    }
                    if let Some(id) = item
                        .pointer("/application_scope/application_id")
                        .and_then(Value::as_str)
                        && state.entries.contains_key(id)
                    {
                        ids.insert(id.into());
                    }
                    if let Some(name) = item["tool_name"].as_str() {
                        collect(state, name, &item["exact_input"], ids);
                    }
                }
            }
        }
        let mut ids = BTreeSet::new();
        for call in history.iter().flat_map(|message| &message.tool_calls) {
            if let Ok(value) = serde_json::from_str::<Value>(&call.arguments_json) {
                collect(self, &call.name, &value, &mut ids);
            }
        }
        for request in requests {
            if matches!(
                request.state,
                crate::dynamic_run::PermissionRequestState::Denied
                    | crate::dynamic_run::PermissionRequestState::Replaced
                    | crate::dynamic_run::PermissionRequestState::Withdrawn
            ) {
                continue;
            }
            for item in &request.items {
                if let Some(input) = item.canonical_input_json.as_deref()
                    && let Ok(value) = serde_json::from_str::<Value>(input)
                {
                    collect(self, &item.tool_name, &value, &mut ids);
                }
            }
        }
        ids
    }

    fn alias(&self, prefix: &str, native_id: &str, source: Option<&str>) -> Option<&str> {
        let aliases = self.native_index.get(&(prefix.into(), native_id.into()))?;
        let mut matches = aliases.iter().filter_map(|id| {
            let entry = self.entries.get(id)?;
            source
                .is_none_or(|source| {
                    entry.sources.iter().any(|locator| match locator {
                        ReferenceSource::Message(id) => id == source,
                        ReferenceSource::Attachment { message, .. } => message == source,
                        ReferenceSource::Business(_) => false,
                    })
                })
                .then_some((id, entry))
        });
        let first = matches.next()?;
        // Multiple incarnations must never silently bind to the latest one.
        matches.next().is_none().then_some(first.0.as_str())
    }

    fn projected_alias(&self, value: &Value) -> Option<&str> {
        let (prefix, native) = native_reference(value)?;
        self.identity_index
            .get(&Self::identity(prefix, &native, value))
            .map(String::as_str)
    }

    pub(crate) fn shorten_value(&self, value: &mut Value, source: Option<&str>) {
        if let Some(alias) = self.projected_alias(value).map(str::to_owned)
            && value.get("token").is_some()
        {
            let kind = value["object_kind"].clone();
            *value = serde_json::json!({"id":alias,"kind":kind});
            return;
        }
        match value {
            Value::Object(object) => {
                // Reference-only result fields, never arbitrary strings or IDs.
                let prefix = match object.get("kind").and_then(Value::as_str) {
                    Some("desktop_session") => Some("s"),
                    Some("application") => Some("a"),
                    Some("window") => Some("w"),
                    Some("ui_element") => Some("e"),
                    Some("desktop_output") => Some("o"),
                    Some("office_document" | "document") => Some("doc"),
                    Some("worksheet") => Some("ws"),
                    Some("range") => Some("r"),
                    Some("presentation") => Some("pr"),
                    Some("slide") => Some("sl"),
                    Some("shape") => Some("sh"),
                    Some("file") => Some("f"),
                    Some("directory") => Some("d"),
                    Some("terminal_output") => Some("term"),
                    Some("browser_surface") => Some("bs"),
                    Some("application_launch_target") => Some("al"),
                    _ => None,
                };
                if let Some(prefix) = prefix
                    && let Some(native) = object.get("id").and_then(Value::as_str)
                    && let Some(alias) = self.alias(prefix, native, source)
                {
                    object.insert("id".into(), Value::String(alias.into()));
                }
                if let Some(reference) = object.get("object_ref") {
                    let alias = self
                        .projected_alias(reference)
                        .or_else(|| {
                            let prefix = match reference["kind"].as_str()? {
                                "application" => "a",
                                "window" => "w",
                                "ui_element" => "e",
                                "desktop_session" => "s",
                                "desktop_output" => "o",
                                _ => return None,
                            };
                            self.alias(prefix, reference["id"].as_str()?, source)
                        })
                        .map(str::to_owned);
                    if let Some(alias) = alias
                        && object.contains_key("element_id")
                    {
                        object.insert("element_id".into(), Value::String(alias));
                    }
                }
                for (key, prefix) in [
                    ("application_id", "a"),
                    ("window_id", "w"),
                    ("root_id", ""),
                    ("output_id", "o"),
                    ("page_id", "p"),
                    ("element_id", "e"),
                    ("element_id", "b"),
                    ("composer_id", "b"),
                    ("send_control_id", "b"),
                    ("to_field_id", "b"),
                    ("subject_field_id", "b"),
                    ("body_field_id", "b"),
                ] {
                    if let Some(native) = object.get(key).and_then(Value::as_str) {
                        let alias = if prefix.is_empty() {
                            ["e", "w", "a", "s"]
                                .into_iter()
                                .find_map(|p| self.alias(p, native, source))
                        } else {
                            self.alias(prefix, native, source)
                        };
                        if let Some(alias) = alias {
                            object.insert(key.into(), Value::String(alias.into()));
                        }
                    }
                }
                for (field, child) in object.iter_mut() {
                    if !is_business_content(field) {
                        self.shorten_value(child, source);
                    }
                }
            }
            Value::Array(values) => values
                .iter_mut()
                .for_each(|v| self.shorten_value(v, source)),
            _ => {}
        }
    }

    pub(crate) fn project_message(&self, message: &mut ChatMessage) {
        if message
            .attachment_read
            .as_ref()
            .is_some_and(|receipt| receipt.released)
            && let Ok(mut value) = serde_json::from_str::<Value>(&message.text)
            && let Some(receipt) = value.get_mut("attachment_read")
        {
            if let Some(object) = receipt.as_object_mut() {
                object.remove("page_sha256");
            }
            if let Some(selection) = receipt.get_mut("selection") {
                self.project_cursors(
                    crate::conversation_attachment::READ_ATTACHMENT_TOOL,
                    selection,
                );
            }
            message.text = value.to_string();
        }
        let source = message.message_id.clone();
        let delegation = crate::subagent::model_ids::is_runtime(message);
        if (delegation || crate::subagent::model_ids::is_receipt(message))
            && let Ok(mut value) = crate::image_input::structured_tool_result(&message.text)
        {
            let body = if delegation && value.get("runtime_delegation_state").is_some() {
                value.get_mut("runtime_delegation_state")
            } else {
                Some(&mut value)
            };
            if let Some(body) = body {
                crate::subagent::model_ids::omit_internal_selectors(body);
            }
            message.text = value.to_string();
        }
        if matches!(message.role, ChatRole::SystemEvent)
            && crate::runtime_context::is_runtime(message)
            && let Ok(mut value) = serde_json::from_str::<Value>(&message.text)
        {
            self.shorten_value(&mut value, None);
            message.text = value.to_string();
        }
        if matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
            && let Ok(mut value) = crate::image_input::structured_tool_result(&message.text)
            && (value.pointer("/ReadContext").is_some()
                || self.entries.values().any(|entry| {
                    entry.sources.iter().any(|origin| match origin {
                        ReferenceSource::Message(id) => id == &source,
                        ReferenceSource::Attachment { message, .. } => message == &source,
                        ReferenceSource::Business(_) => false,
                    })
                }))
        {
            crate::ui_model_output::expand_value(&mut value);
            self.shorten_value(&mut value, Some(&source));
            if let Some(envelope) = &message.data_envelope {
                self.project_cursors(&envelope.provenance.source_tool_name, &mut value);
            }
            let omitted = message
                .text
                .trim_end()
                .ends_with(crate::image_input::IMAGE_NOT_RETAINED_PLACEHOLDER);
            message.text = value.to_string();
            if omitted {
                message.text.push('\n');
                message
                    .text
                    .push_str(crate::image_input::IMAGE_NOT_RETAINED_PLACEHOLDER);
            }
        }
    }

    /// Expand only known tool reference fields, then validate the exact native
    /// identity after the existing source/geometry/browser resolution.
    pub(crate) fn expand_call(
        &self,
        call: &ToolCall,
    ) -> Result<(ToolCall, Vec<Value>), AgentError> {
        let mut value: Value = serde_json::from_str(&call.arguments_json)
            .map_err(|_| invalid("Tool arguments must be JSON"))?;
        let ui_tool = matches!(
            call.name.as_str(),
            "inspect_desktop_ui"
                | "read_current_screen"
                | "execute_ui_actions"
                | "send_background_input"
                | "send_raw_input"
                | "execute_wayland_output_input"
                | "request_permissions"
        ) || crate::browser_model_ids::supports(&call.name)
            || crate::document_model_ids::supports(&call.name);
        if ui_tool {
            let mut native = false;
            visit_references(&value, &mut |_| native = true);
            if native {
                return Err(invalid(
                    "Use observed short UI IDs in tool arguments; native reference objects are supplied by the server.",
                ));
            }
        }
        let mut expected = Vec::new();
        self.expand_arguments(&call.name, &mut value, &mut expected)?;
        Ok((
            ToolCall {
                arguments_json: value.to_string(),
                ..call.clone()
            },
            expected,
        ))
    }

    pub(crate) fn alias_native_arguments(&self, value: &mut Value) {
        let alias = self.projected_alias(value).map(str::to_owned);
        if let Some(alias) = alias {
            if value.get("task_id").is_some() {
                return;
            }
            let key = if value.get("token").is_some() {
                "token"
            } else if value.get("element_id").is_some() {
                "element_id"
            } else {
                "page_id"
            };
            value[key] = Value::String(alias);
            return;
        }
        match value {
            Value::Object(object) => object
                .iter_mut()
                .filter(|(field, _)| !is_business_content(field))
                .for_each(|(_, v)| self.alias_native_arguments(v)),
            Value::Array(array) => array
                .iter_mut()
                .for_each(|v| self.alias_native_arguments(v)),
            _ => {}
        }
    }

    fn expand_id(
        &self,
        value: &mut Value,
        prefixes: &[&str],
        expected: &mut Vec<Value>,
    ) -> Result<(), AgentError> {
        if value.is_null() {
            return Ok(());
        }
        let id = value
            .as_str()
            .ok_or_else(|| invalid("Use an observed short UI ID"))?;
        let entry = self.entries.get(id).ok_or_else(|| invalid("Unknown or retired object ID. Obtain a fresh ID from the matching object inspection tool; no action was executed."))?;
        if entry.sources.is_empty() {
            return Err(invalid(
                "Object source is unavailable. Obtain a fresh ID from the matching object inspection tool; no action was executed.",
            ));
        }
        if !prefixes.contains(&entry.prefix.as_str()) {
            return Err(invalid(
                "Object ID has the wrong type; read the required object again",
            ));
        }
        expected.push(entry.reference.clone());
        *value = Value::String(entry.native_id.clone());
        Ok(())
    }

    fn expand_arguments(
        &self,
        tool: &str,
        value: &mut Value,
        expected: &mut Vec<Value>,
    ) -> Result<(), AgentError> {
        if !Self::cursor_paths(tool).is_empty()
            && let Some(id) = value.get_mut("cursor")
            && let Some(selector) = id.as_str().filter(|id| id.parse::<i64>().is_err())
        {
            let entry = self.entries.get(selector)
                .filter(|entry| !entry.sources.is_empty() && entry.prefix == "c" && entry.reference["tool_name"] == tool)
                .ok_or_else(|| invalid("Unknown, expired or mismatched cursor. Read the first page of the same resource again; no action was executed."))?;
            *id = Value::String(entry.native_id.clone());
        }
        if crate::document_model_ids::supports(tool) {
            for field in ["target_id", "document_id"] {
                if let Some(id) = value.get_mut(field) {
                    self.expand_id(
                        id,
                        &["doc", "ws", "r", "pr", "sl", "sh", "f", "d", "w", "a", "e"],
                        expected,
                    )?;
                }
            }
            if tool == "preview_computer_action"
                && let Some(actions) = value.get_mut("actions").and_then(Value::as_array_mut)
            {
                for action in actions {
                    self.expand_arguments(tool, action, expected)?;
                    if action.pointer("/action/adapter").and_then(Value::as_str)
                        == Some("ui_in_application")
                        && let Some(id) = action.pointer_mut("/action/action/application_id")
                    {
                        self.expand_id(id, &["a"], expected)?;
                    }
                }
            }
        }
        let native = matches!(
            tool,
            "inspect_desktop_ui"
                | "execute_ui_actions"
                | "send_background_input"
                | "send_raw_input"
                | "execute_wayland_output_input"
                | "read_current_screen"
                | "preview_computer_action"
        );
        let browser = crate::browser_model_ids::supports(tool) && tool != "create_formula_workbook";
        if native || browser {
            for (field, prefixes) in [
                ("application_id", &["a"][..]),
                ("window_id", &["w"][..]),
                ("root_id", &["s", "a", "w", "e"][..]),
                ("output_id", &["o"][..]),
                ("page_id", &["p"][..]),
                ("element_id", if browser { &["b"][..] } else { &["e"][..] }),
                ("composer_id", &["b"][..]),
                ("send_control_id", &["b"][..]),
                ("to_field_id", &["b"][..]),
                ("subject_field_id", &["b"][..]),
                ("body_field_id", &["b"][..]),
            ] {
                if let Some(id) = value.get_mut(field) {
                    self.expand_id(id, prefixes, expected)?;
                }
            }
            if let Some(steps) = value.get_mut("steps").and_then(Value::as_array_mut) {
                for step in steps {
                    self.expand_arguments(tool, step, expected)?;
                }
            }
            if tool == "send_background_input"
                && let Some(action) = value.get_mut("action")
                && let Some(id) = action.get_mut("element_id")
            {
                self.expand_id(id, &["e"], expected)?;
            }
            if browser {
                if let Some(fields) = value.get_mut("fields").and_then(Value::as_array_mut) {
                    for field in fields {
                        if let Some(id) = field.get_mut("element_id") {
                            self.expand_id(id, &["b"], expected)?;
                        }
                    }
                }
                if let Some(id) = value.pointer_mut("/attachment/element_id") {
                    self.expand_id(id, &["b"], expected)?;
                }
            }
        }
        if tool == "request_permissions"
            && let Some(items) = value.get_mut("items").and_then(Value::as_array_mut)
        {
            for item in items {
                if let Some(id) = item.pointer_mut("/application_scope/application_id") {
                    self.expand_id(id, &["a"], expected)?;
                }
                if let Some(tool) = item
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    && let Some(exact) = item.get_mut("exact_input")
                {
                    self.expand_arguments(&tool, exact, expected)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn validate_resolved(expected: &[Value], call: &ToolCall) -> Result<(), AgentError> {
        let value: Value = serde_json::from_str(&call.arguments_json)
            .map_err(|_| invalid("Invalid resolved UI call"))?;
        let mut found = Vec::new();
        visit_references(&value, &mut |value| found.push(value.clone()));
        if expected.iter().any(|reference| !found.contains(reference)) {
            return Err(invalid(
                "UI reference changed since observation. Read the desktop/UI or browser again; no action was executed.",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reference(token: &str, snapshot: &str) -> Value {
        serde_json::json!({"token":token,"snapshot_id":snapshot,"object_kind":"application","expires_at":"2030-01-01T00:00:00Z"})
    }
    #[test]
    fn aliases_are_stable_persisted_independent_and_never_reused() {
        let mut state = UiReferenceState::default();
        state
            .register(&reference("long-native-id", "first"), "obs1")
            .unwrap();
        state
            .register(&reference("long-native-id", "first"), "obs2")
            .unwrap();
        assert_eq!(state.entries.len(), 1);
        let mut restored: UiReferenceState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        restored
            .register(&reference("long-native-id", "second"), "obs3")
            .unwrap();
        assert_eq!(
            restored.entries["a1"].reference,
            reference("long-native-id", "first")
        );
        restored.retain_sources(&BTreeSet::new(), &BTreeSet::new());
        restored
            .register(&reference("another-id", "third"), "obs4")
            .unwrap();
        assert!(restored.entries.contains_key("a3"));
        let mut other = UiReferenceState::default();
        other
            .register(&reference("different-object", "first"), "obs1")
            .unwrap();
        assert_ne!(state.entries["a1"].reference, other.entries["a1"].reference);
    }
    #[test]
    fn expansion_preserves_text_and_rejects_incarnation_changes() {
        let mut state = UiReferenceState::default();
        state.register(&reference("native", "old"), "obs").unwrap();
        let call = ToolCall {
            id: "call".into(),
            name: "send_raw_input".into(),
            arguments_json: serde_json::json!({"application_id":"a1","action":{"text":"a1"}})
                .to_string(),
        };
        let (expanded, expected) = state.expand_call(&call).unwrap();
        let value: Value = serde_json::from_str(&expanded.arguments_json).unwrap();
        assert_eq!(value["action"]["text"], "a1");
        let resolved = ToolCall {
            arguments_json: serde_json::json!({"target":reference("native","new")}).to_string(),
            ..call
        };
        assert!(UiReferenceState::validate_resolved(&expected, &resolved).is_err());
    }
    #[test]
    fn authenticated_attachment_reference_resolves_without_inline_body() {
        let mut session = crate::session::PersistedAgentSession::new(
            "session",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-10-10T00:00:00Z",
        );
        let original = reference("native-in-attachment", "original-snapshot");
        session
            .ui_references
            .register(&original, "attachment-source")
            .unwrap();
        let call = ToolCall {
            id: "read".into(),
            name: "inspect_desktop_ui".into(),
            arguments_json: serde_json::json!({"root_id":"a1","queries":["Calendar"]}).to_string(),
        };
        let resolved = crate::ui_model_ids::resolve_session_call(&call, &session, 1).unwrap();
        let value: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(value["root"], original);
        let wrong = ToolCall {
            arguments_json: serde_json::json!({"root_id":"native-in-attachment"}).to_string(),
            ..call
        };
        assert!(crate::ui_model_ids::resolve_session_call(&wrong, &session, 1).is_err());
    }
}

#[cfg(test)]
mod reference_lifecycle_tests {
    use super::*;
    use serde_json::json;

    fn document(token: &str) -> Value {
        json!({"token":token,"snapshot_id":"document-version","object_kind":"document","expires_at":"2099-01-01T00:00:00Z"})
    }

    #[test]
    fn attachment_sources_survive_history_retirement_and_restore_indexes() {
        let mut state = UiReferenceState::default();
        let value = document("complete-device-token");
        state
            .register_entry(
                "doc",
                "complete-device-token".into(),
                &value,
                ReferenceSource::Attachment {
                    id: "attachment".into(),
                    message: "observation".into(),
                },
            )
            .unwrap();
        state.retain_sources(&BTreeSet::new(), &BTreeSet::new());
        let restored: UiReferenceState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.projected_alias(&value), Some("doc1"));
        let mut retired = restored;
        retired.retain_attachment_sources(&BTreeSet::new(), &BTreeSet::new());
        assert!(retired.projected_alias(&value).is_none());
        retired
            .register(&document("different-device-token"), "new")
            .unwrap();
        assert!(retired.entries.contains_key("doc2"));
    }

    #[test]
    fn service_cursor_uses_the_registered_tool_name_and_restores_original_binding() {
        let history = crate::result_model_ids::tests::evidence(
            "read_service_status",
            "services",
            json!({"queries":["ssh"]}),
            json!({"ReadContext":{"ServiceStatus":{"next_cursor":"complete-query-bound-service-cursor","complete":false}}}),
        );
        let mut references = UiReferenceState::default();
        references.observe(&history).unwrap();
        let mut projected = history.last().unwrap().clone();
        references.project_message(&mut projected);
        assert_eq!(
            serde_json::from_str::<Value>(&projected.text).unwrap()["ReadContext"]["ServiceStatus"]
                ["next_cursor"],
            "c1"
        );
        let call = ToolCall {
            id: "next".into(),
            name: "read_service_status".into(),
            arguments_json: json!({"queries":["ssh"],"cursor":"c1"}).to_string(),
        };
        let (expanded, _) = references.expand_call(&call).unwrap();
        let value: Value = serde_json::from_str(&expanded.arguments_json).unwrap();
        assert_eq!(value["cursor"], "complete-query-bound-service-cursor");
        assert_eq!(value["queries"], json!(["ssh"]));
    }

    #[test]
    fn cursors_are_typed_and_unrelated_references_do_not_change_them() {
        let mut state = UiReferenceState::default();
        let cursor = state
            .register_attachment_cursor("attachment", "source", "complete-query-bound-cursor")
            .unwrap();
        assert_eq!(cursor, "c1");
        state.register(&document("unrelated"), "another").unwrap();
        assert_eq!(
            state
                .attachment_cursor_alias("complete-query-bound-cursor")
                .unwrap(),
            cursor
        );
        let call = ToolCall {
            id: "read".into(),
            name: "read_conversation_attachment".into(),
            arguments_json: json!({"attachment_id":"attachment","cursor":cursor}).to_string(),
        };
        let (expanded, _) = state.expand_call(&call).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&expanded.arguments_json).unwrap()["cursor"],
            "complete-query-bound-cursor"
        );
        let wrong = ToolCall {
            name: "inspect_files".into(),
            ..call
        };
        assert!(state.expand_call(&wrong).is_err());
    }

    #[test]
    fn only_declared_selector_fields_protect_references() {
        let mut state = UiReferenceState::default();
        state
            .register(&document("device-token"), "observation")
            .unwrap();
        let history = vec![ChatMessage::assistant_tool_calls(
            "call",
            "",
            vec![crate::chat::ToolCallRef {
                id: "model-call".into(),
                name: "replace_live_document_body".into(),
                arguments_json:
                    json!({"target_id":"doc1","text":"doc2","note":{"target_id":"doc3"}})
                        .to_string(),
            }],
        )];
        assert_eq!(
            state.retained_call_aliases(&history, &[]),
            BTreeSet::from(["doc1".into()])
        );
    }

    #[test]
    fn retained_document_reference_restores_exact_snapshot_and_rejects_evicted_source() {
        let mut session = crate::session::PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                expires_at: None,
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                policy_name: None,
            },
            "2026-01-01T00:00:00Z",
        );
        let original = document("opaque-document");
        session
            .ui_references
            .register_entry(
                "doc",
                "opaque-document".into(),
                &original,
                ReferenceSource::Attachment {
                    id: "retained".into(),
                    message: "source".into(),
                },
            )
            .unwrap();
        let call = ToolCall {
            id: "inspect".into(),
            name: "inspect_live_document".into(),
            arguments_json: json!({"target_id":"doc1"}).to_string(),
        };
        let resolved = crate::ui_model_ids::resolve_session_call(&call, &session, 1).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&resolved.arguments_json).unwrap()["target"],
            original
        );
        assert!(
            crate::ui_model_ids::resolve_session_call(&call, &session, 4_200_000_000_000).is_err()
        );
        session
            .ui_references
            .retain_attachment_sources(&BTreeSet::new(), &BTreeSet::from(["doc1".into()]));
        assert!(session.ui_references.entries.contains_key("doc1"));
        assert!(crate::ui_model_ids::resolve_session_call(&call, &session, 1).is_err());
    }

    #[test]
    fn business_json_is_not_registered_or_rewritten_as_device_metadata() {
        let mut state = UiReferenceState::default();
        let original = document("device-token");
        state.register(&original, "observation").unwrap();
        let mut result = json!({"target":original,"note":{"target":original},"rows":[original]});
        state.shorten_value(&mut result, None);
        assert_eq!(result["target"]["id"], "doc1");
        assert_eq!(result["note"]["target"], original);
        assert_eq!(result["rows"][0], original);
    }

    #[test]
    fn equal_native_strings_in_different_types_are_not_merged() {
        let mut state = UiReferenceState::default();
        let native = "shared-string";
        let doc = document(native);
        state.register(&doc, "source").unwrap();
        let mut file = doc.clone();
        file["object_kind"] = json!("file");
        state.register(&file, "source").unwrap();
        assert_eq!(state.projected_alias(&doc), Some("doc1"));
        assert_eq!(state.projected_alias(&file), Some("f1"));
    }
}
