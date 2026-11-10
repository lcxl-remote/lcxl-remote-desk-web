//! Session-owned model aliases. Native identities and authorization stay intact.

use std::collections::{BTreeMap, BTreeSet};

use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{ObjectKind, ObjectRef},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chat::{ChatMessage, ChatRole, ToolCall};

#[derive(Clone, Default, Serialize)]
pub struct UiReferenceState {
    counters: BTreeMap<String, u64>,
    entries: BTreeMap<String, ReferenceEntry>,
    #[serde(skip)]
    native_index: BTreeMap<(String, String), BTreeSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum ReferenceSource {
    Message(String),
    Attachment {
        id: String,
        message: String,
    },
    Business(String),
    /// Durable record marker: keeps a record alias expandable after the
    /// message that first showed it leaves the conversation.
    Record,
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

/// The single alias prefix for each UI and document object kind.
pub(crate) fn object_kind_prefix(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::DesktopSession => "s",
        ObjectKind::Application => "a",
        ObjectKind::Window => "w",
        ObjectKind::UiElement => "e",
        ObjectKind::DesktopOutput => "o",
        ObjectKind::OfficeDocument | ObjectKind::Document => "doc",
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
    }
}

fn native_reference(value: &Value) -> Option<(&'static str, String)> {
    if value.get("token").is_some()
        && value.get("object_kind").is_some()
        && value.get("snapshot_id").is_some()
        && value.get("expires_at").is_some()
        && let Ok(reference) = serde_json::from_value::<ObjectRef>(value.clone())
    {
        return Some((object_kind_prefix(reference.object_kind), reference.token));
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
    crate::document_model_ids::visit_live_references(value, visitor);
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

/// Result fields the model copies into later calls. Each holds the message id
/// of a verified result and is presented to the model as a `res` alias.
const RESULT_OUTPUT_FIELDS: [&str; 5] = [
    "search_result_id",
    "artifact_result_id",
    "file_version_id",
    "file_result_call_id",
    "handoff_result_id",
];

fn visit_result_selectors(value: &mut Value, visitor: &mut impl FnMut(&mut Value)) {
    for field in [
        "file_result_call_id",
        "file_version_id",
        "web_search_result_id",
        "handoff_result_id",
    ] {
        if let Some(id) = value.get_mut(field) {
            visitor(id);
        }
    }
    for path in [
        "/source/file_result_call_id",
        "/attachment/artifact_result_id",
    ] {
        if let Some(id) = value.pointer_mut(path) {
            visitor(id);
        }
    }
    for field in ["sources", "file_sources"] {
        if let Some(sources) = value.get_mut(field).and_then(Value::as_array_mut) {
            for source in sources {
                if let Some(id) = source.get_mut("file_result_call_id") {
                    visitor(id);
                }
            }
        }
    }
}

/// Server-issued selectors the model copies by field name, with the alias
/// prefixes each field accepts.
const SELECTOR_FIELDS: [(&str, &[&str]); 5] = [
    ("worker_id", &["wk"]),
    ("request_id", &["task"]),
    ("task_id", &["task", "j"]),
    ("question_id", &["q"]),
    ("preview_id", &["v"]),
];

fn selector_prefix(native: &str) -> Option<&'static str> {
    use crate::native_ids::{Kind, is};
    [
        (Kind::Worker, "wk"),
        (Kind::Task, "task"),
        (Kind::WorkerReport, "q"),
        (Kind::BackgroundAction, "j"),
        (Kind::ExecRequest, "j"),
        (Kind::InteractiveExecRequest, "j"),
        (Kind::Preview, "v"),
    ]
    .into_iter()
    .find(|(kind, _)| is(*kind, native))
    .map(|(_, prefix)| prefix)
}

/// Schedule list and proposal receipts are server-labelled tool results.
fn is_schedule_result(message: &ChatMessage) -> bool {
    message.role == ChatRole::Tool
        && message.data_envelope.as_ref().is_some_and(|label| {
            label.provenance.source_provider_id == crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
                && matches!(
                    label.provenance.source_tool_name.as_str(),
                    "schedule_query" | "schedule_proposal"
                )
        })
}

/// Durable record fields shown to the model and their alias prefixes.
const RECORD_FIELDS: &[(&str, &str)] = &[("schedule_id", "sc"), ("memory_id", "mem")];
/// Attachment ids are registered only from server-built delivery slots and
/// stored screenshots, never by scanning result fields.
const ATTACHMENT_PREFIX: &str = "att";
/// Every durable record alias prefix, used for recycling and alias shapes.
const RECORD_PREFIXES: &[&str] = &["sc", "mem", ATTACHMENT_PREFIX];
/// Per-prefix limit for record aliases whose showing messages are gone.
const MAX_RECORD_ALIASES: usize = 256;

fn record_alias_shape(id: &str) -> bool {
    RECORD_PREFIXES.iter().any(|prefix| {
        id.strip_prefix(prefix)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Visits record ids at their known result locations: top-level fields, the
/// `memory` object and entries of `tasks` or `memories` lists. The visitor may
/// return an alias to replace the id.
fn visit_record_ids(value: &mut Value, visitor: &mut impl FnMut(&str, &str) -> Option<String>) {
    fn fields(value: &mut Value, visitor: &mut impl FnMut(&str, &str) -> Option<String>) {
        let Some(object) = value.as_object_mut() else {
            return;
        };
        for (field, _) in RECORD_FIELDS {
            if let Some(id) = object.get_mut(*field)
                && let Some(native) = id.as_str()
                && let Some(alias) = visitor(field, native)
            {
                *id = Value::String(alias);
            }
        }
    }
    fields(value, visitor);
    if let Some(memory) = value.get_mut("memory") {
        fields(memory, visitor);
    }
    for list in ["tasks", "memories"] {
        if let Some(items) = value.get_mut(list).and_then(Value::as_array_mut) {
            for item in items {
                fields(item, visitor);
            }
        }
    }
}

fn selector_alias_shape(id: &str) -> bool {
    ["wk", "task", "q", "j", "v"].into_iter().any(|prefix| {
        id.strip_prefix(prefix)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn visit_selector_values(value: &Value, visitor: &mut impl FnMut(&str)) {
    match value {
        Value::Object(object) => {
            for (field, child) in object {
                if SELECTOR_FIELDS.iter().any(|(name, _)| name == field)
                    && let Some(native) = child.as_str()
                {
                    visitor(native);
                } else if !is_business_content(field) {
                    visit_selector_values(child, visitor);
                }
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|v| visit_selector_values(v, visitor)),
        _ => {}
    }
}

fn result_alias_shape(id: &str) -> bool {
    id.strip_prefix("res")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

impl UiReferenceState {
    fn rebuild_indexes(&mut self) {
        self.native_index.clear();
        for (id, entry) in &self.entries {
            self.native_index
                .entry((entry.prefix.clone(), entry.native_id.clone()))
                .or_default()
                .insert(id.clone());
        }
    }

    /// One native id may have several incarnations; only an entry with the
    /// same complete reference is the same alias.
    fn exact_alias(&self, prefix: &str, native: &str, value: &Value) -> Option<&String> {
        self.native_index
            .get(&(prefix.to_owned(), native.to_owned()))?
            .iter()
            .find(|id| {
                self.entries
                    .get(*id)
                    .is_some_and(|entry| entry.reference == *value)
            })
    }

    pub(crate) fn register(&mut self, value: &Value, source: &str) -> Result<(), AgentError> {
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
        if let Some(id) = self.exact_alias(prefix, &native_id, value).cloned() {
            self.entries
                .get_mut(&id)
                .expect("derived reference index")
                .sources
                .insert(source);
            return Ok(());
        }
        let counter = self.counters.entry(prefix.into()).or_default();
        *counter = counter
            .checked_add(1)
            .ok_or_else(|| invalid("Conversation reference sequence exhausted"))?;
        let id = format!("{prefix}{counter}");
        self.native_index
            .entry((prefix.into(), native_id.clone()))
            .or_default()
            .insert(id.clone());
        self.entries.insert(
            id,
            ReferenceEntry {
                prefix: prefix.into(),
                native_id,
                reference: value.clone(),
                sources: BTreeSet::from([source]),
            },
        );
        Ok(())
    }

    /// Accepted device observations and server-issued selectors allocate aliases.
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
            let data_tool = crate::assistant_data_model::tool(source).filter(|tool| {
                history.iter().flat_map(|m| &m.tool_calls).any(|call| {
                    Some(&call.id) == message.tool_call_id.as_ref() && call.name == *tool
                })
            });
            if is_schedule_result(message)
                && let Ok(value) = serde_json::from_str::<Value>(&message.text)
            {
                self.observe_records(&value, &message.message_id)?;
                continue;
            }
            if !native && !browser && accepted.is_none() && data_tool.is_none() {
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
            if data_tool.is_some() {
                self.observe_records(&value, &message.message_id)?;
                self.observe_conversations(
                    &value,
                    ReferenceSource::Message(message.message_id.clone()),
                )?;
            }
            if let Some(tool) = accepted {
                self.observe_cursors(
                    tool,
                    &value,
                    ReferenceSource::Message(message.message_id.clone()),
                )?;
            }
        }
        self.observe_results(history)?;
        self.observe_selectors(history)?;
        self.observe_attachment_slots(history)?;
        Ok(*self != before)
    }

    /// Worker, request, question, background task and preview ids issued in
    /// results or persisted delegation state receive short aliases.
    fn observe_selectors(&mut self, history: &[ChatMessage]) -> Result<(), AgentError> {
        for message in history {
            // Runtime delegation state exists only in a request projection; it
            // reuses existing aliases and never allocates one.
            let carries = matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                || crate::subagent::model_ids::is_receipt(message);
            if !carries {
                continue;
            }
            let Ok(value) = crate::image_input::structured_tool_result(&message.text) else {
                continue;
            };
            let mut found = Vec::new();
            visit_selector_values(&value, &mut |native| {
                if let Some(prefix) = selector_prefix(native) {
                    found.push((prefix, native.to_owned()));
                }
            });
            for (prefix, native) in found {
                self.register_entry(
                    prefix,
                    native.clone(),
                    &serde_json::json!({"selector": native}),
                    ReferenceSource::Message(message.message_id.clone()),
                )?;
            }
        }
        Ok(())
    }

    /// Replace selector aliases in known id fields with their native ids.
    pub(crate) fn expand_selector_ids(&self, value: &mut Value) -> Result<(), AgentError> {
        if let Some(requests) = value.get_mut("requests").and_then(Value::as_array_mut) {
            for request in requests {
                self.expand_selector_ids(request)?;
            }
        }
        let Some(object) = value.as_object_mut() else {
            return Ok(());
        };
        for (field, prefixes) in SELECTOR_FIELDS {
            let Some(id) = object.get_mut(field) else {
                continue;
            };
            let Some(selected) = id.as_str() else {
                continue;
            };
            // Server-issued selectors stay valid after their result leaves
            // the conversation; the native id names a durable record.
            match self.entries.get(selected) {
                Some(entry) if prefixes.contains(&entry.prefix.as_str()) => {
                    *id = Value::String(entry.native_id.clone());
                }
                Some(_) => {
                    return Err(invalid(
                        "ID has the wrong type; copy it from the matching result",
                    ));
                }
                None if selector_alias_shape(selected) => {
                    return Err(invalid(
                        "ID is unavailable in this conversation; copy it from a current result",
                    ));
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Present recorded selectors with the aliases the model saw.
    pub(crate) fn alias_selector_ids(&self, value: &mut Value) {
        if let Some(requests) = value.get_mut("requests").and_then(Value::as_array_mut) {
            for request in requests {
                self.alias_selector_ids(request);
            }
        }
        let Some(object) = value.as_object_mut() else {
            return;
        };
        for (field, _) in SELECTOR_FIELDS {
            if let Some(id) = object.get_mut(field)
                && let Some(native) = id.as_str()
                && let Some(alias) =
                    selector_prefix(native).and_then(|prefix| self.alias(prefix, native, None))
            {
                *id = Value::String(alias.into());
            }
        }
    }

    /// A result alias names the verified message whose projection offers it.
    fn observe_results(&mut self, history: &[ChatMessage]) -> Result<(), AgentError> {
        for message in history {
            if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
                continue;
            }
            let mut projected = message.clone();
            crate::result_model_ids::project_result_message(&mut projected);
            let Ok(value) = serde_json::from_str::<Value>(&projected.text) else {
                continue;
            };
            if RESULT_OUTPUT_FIELDS
                .iter()
                .any(|field| value.get(*field).and_then(Value::as_str) == Some(&message.message_id))
            {
                self.register_entry(
                    "res",
                    message.message_id.clone(),
                    &serde_json::json!({"result_message_id": message.message_id}),
                    ReferenceSource::Message(message.message_id.clone()),
                )?;
            }
        }
        Ok(())
    }

    /// Replace result aliases with the message ids that result resolution reads.
    pub(crate) fn expand_result_ids(&self, value: &mut Value) -> Result<(), AgentError> {
        let mut error = None;
        visit_result_selectors(value, &mut |id| {
            let Some(selected) = id.as_str() else {
                return;
            };
            match self.entries.get(selected) {
                Some(entry) if entry.prefix == "res" && !entry.sources.is_empty() => {
                    *id = Value::String(entry.native_id.clone());
                }
                Some(entry) if entry.prefix != "res" => {
                    error.get_or_insert(invalid(
                        "Result ID has the wrong type; copy the result ID from the verified result",
                    ));
                }
                Some(_) => {
                    error.get_or_insert(invalid(
                        "Result ID is unavailable in this conversation; copy it from a current verified result",
                    ));
                }
                None if result_alias_shape(selected) => {
                    error.get_or_insert(invalid(
                        "Result ID is unavailable in this conversation; copy it from a current verified result",
                    ));
                }
                None => {}
            }
        });
        error.map_or(Ok(()), Err)
    }

    /// Present recorded result selectors with the aliases the model saw.
    pub(crate) fn alias_result_ids(&self, value: &mut Value) {
        visit_result_selectors(value, &mut |id| {
            if let Some(alias) = id
                .as_str()
                .and_then(|native| self.alias("res", native, None))
            {
                *id = Value::String(alias.into());
            }
        });
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

    fn observe_conversations(
        &mut self,
        value: &Value,
        source: ReferenceSource,
    ) -> Result<(), AgentError> {
        let mut ids = BTreeSet::new();
        crate::assistant_data_model::conversations(value, &mut |id| {
            ids.insert(id.to_owned());
        });
        for id in ids {
            self.register_entry(
                "cv",
                id.clone(),
                &serde_json::json!({"conversation_id":id}),
                source.clone(),
            )?;
        }
        Ok(())
    }

    /// Resolve only the query selector; the original model call and its digest stay intact.
    pub fn resolve_conversation_request(
        &self,
        request: &desk_agent_protocol::ai_assistant::history::ConversationSearchRequest,
    ) -> Result<desk_agent_protocol::ai_assistant::history::ConversationSearchRequest, AgentError>
    {
        let mut request = request.clone();
        for id in &mut request.conversation_ids {
            if id.strip_prefix("cv").is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            }) {
                let mut value = Value::String(id.clone());
                self.expand_id(&mut value, &["cv"], &mut Vec::new())?;
                *id = value.as_str().expect("expanded conversation ID").into();
            }
        }
        Ok(request)
    }

    pub(crate) fn alias_conversation_arguments(&self, tool: &str, value: &mut Value) {
        if tool == "search_conversations"
            && let Some(ids) = value
                .get_mut("conversation_ids")
                .and_then(Value::as_array_mut)
        {
            for id in ids {
                if let Some(alias) = id.as_str().and_then(|id| self.alias("cv", id, None)) {
                    *id = Value::String(alias.into());
                }
            }
        }
    }

    fn project_conversations(&self, value: &mut Value, source: &str) {
        fn ids(state: &UiReferenceState, value: &mut Value, source: &str) {
            for field in ["conversation_id", "root_conversation_id"] {
                if let Some(id) = value.get_mut(field)
                    && let Some(alias) = id
                        .as_str()
                        .and_then(|id| state.alias("cv", id, Some(source)))
                {
                    *id = Value::String(alias.into());
                }
            }
        }
        ids(self, value, source);
        if let Some(hits) = value.get_mut("hits").and_then(Value::as_array_mut) {
            for hit in hits {
                ids(self, hit, source);
            }
        }
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
        if crate::ai_assistant::data_tools::is_data_tool(&envelope.provenance.source_tool_name) {
            self.observe_conversations(&value, source.clone())?;
        }
        self.observe_cursors(&envelope.provenance.source_tool_name, &value, source)?;
        Ok(*self != before)
    }

    pub(crate) fn attachment_cursor_alias(&self, cursor: &str) -> Result<String, AgentError> {
        let value = serde_json::json!({"cursor":cursor,"tool_name":"read_conversation_attachment"});
        if let Some(id) = self.exact_alias("c", cursor, &value) {
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
        self.exact_alias("c", cursor, &value)
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
                if let Some(alias) = self.exact_alias("c", native, &reference) {
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
                ReferenceSource::Attachment { .. }
                | ReferenceSource::Business(_)
                | ReferenceSource::Record => true,
            });
            let retained = !entry.sources.is_empty() || protected.contains(id);
            changed |= !retained || before != entry.sources.len();
            retained
        });
        changed |= self.recycle_records(protected);
        self.rebuild_indexes();
        changed
    }

    /// Bounds durable record aliases. Only entries whose showing messages have
    /// all left the conversation are candidates; the oldest unprotected ones
    /// beyond the per-prefix limit are dropped. Dropped aliases are never reused.
    fn recycle_records(&mut self, protected: &BTreeSet<String>) -> bool {
        let mut changed = false;
        for prefix in RECORD_PREFIXES {
            let mut candidates = self
                .entries
                .iter()
                .filter(|(id, entry)| {
                    entry.prefix == *prefix
                        && entry
                            .sources
                            .iter()
                            .all(|source| *source == ReferenceSource::Record)
                        && !protected.contains(*id)
                })
                .filter_map(|(id, _)| Some((id[prefix.len()..].parse::<u64>().ok()?, id.clone())))
                .collect::<Vec<_>>();
            if candidates.len() <= MAX_RECORD_ALIASES {
                continue;
            }
            candidates.sort_unstable();
            for (_, id) in &candidates[..candidates.len() - MAX_RECORD_ALIASES] {
                self.entries.remove(id);
                changed = true;
            }
        }
        changed
    }

    /// Registers a durable record shown by `message`.
    fn register_record(
        &mut self,
        field: &str,
        prefix: &str,
        native: &str,
        message: &str,
    ) -> Result<(), AgentError> {
        let reference = serde_json::json!({ field: native });
        self.register_entry(
            prefix,
            native.into(),
            &reference,
            ReferenceSource::Message(message.into()),
        )?;
        let id = self
            .exact_alias(prefix, native, &reference)
            .cloned()
            .expect("registered record alias");
        self.entries
            .get_mut(&id)
            .expect("derived reference index")
            .sources
            .insert(ReferenceSource::Record);
        Ok(())
    }

    /// Registers an attachment shown by `message`.
    pub(crate) fn register_attachment(
        &mut self,
        native: &str,
        message: &str,
    ) -> Result<(), AgentError> {
        self.register_record("attachment_id", ATTACHMENT_PREFIX, native, message)
    }

    /// Externalized results and command streams name their attachments in
    /// server-built raw result slots.
    fn observe_attachment_slots(&mut self, history: &[ChatMessage]) -> Result<(), AgentError> {
        for message in history {
            for slot in message.raw_result.iter().flat_map(|raw| &raw.slots) {
                self.register_attachment(&slot.attachment_id, &message.message_id)?;
            }
        }
        Ok(())
    }

    /// The alias of an attachment, if one is registered.
    pub(crate) fn attachment_alias(&self, native: &str) -> Option<&str> {
        self.alias(ATTACHMENT_PREFIX, native, None)
    }

    /// Presents the attachments registered for this message by their aliases.
    /// Only server-built locations change: delivery parts, command streams,
    /// the screenshot reference field or its plain-text line.
    fn project_attachments(&self, message: &mut ChatMessage) {
        if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
            return;
        }
        let source = message.message_id.as_str();
        let alias = |native: &str| self.alias(ATTACHMENT_PREFIX, native, Some(source));
        let registered = self.entries.values().any(|entry| {
            entry.prefix == ATTACHMENT_PREFIX
                && entry
                    .sources
                    .contains(&ReferenceSource::Message(source.into()))
        });
        if !registered {
            return;
        }
        if let Ok(mut value) = crate::image_input::structured_tool_result(&message.text) {
            let mut changed = false;
            let mut replace = |id: Option<&mut Value>| {
                if let Some(id) = id
                    && let Some(alias) = id.as_str().and_then(alias)
                {
                    *id = Value::String(alias.into());
                    changed = true;
                }
            };
            replace(value.get_mut("attachment_id"));
            for list in ["parts", "streams"] {
                if let Some(items) = value.get_mut(list).and_then(Value::as_array_mut) {
                    for item in items {
                        replace(item.pointer_mut("/content/reference/attachment_id"));
                    }
                }
            }
            if changed {
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
            return;
        }
        const LINE: &str = "Screenshot attachment_id: ";
        let projected = message
            .text
            .split('\n')
            .map(|line| {
                line.strip_prefix(LINE)
                    .and_then(alias)
                    .map_or_else(|| line.to_owned(), |alias| format!("{LINE}{alias}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        message.text = projected;
    }

    /// Expands record aliases in the top-level record fields of model arguments.
    /// Native ids pass through; an unknown or mistyped alias is rejected.
    pub(crate) fn expand_record_ids(&self, value: &mut Value) -> Result<(), AgentError> {
        let Some(object) = value.as_object_mut() else {
            return Ok(());
        };
        for (field, prefix) in RECORD_FIELDS {
            if let Some(id) = object.get_mut(*field)
                && let Some(selected) = id.as_str()
            {
                *id = Value::String(self.expand_record_id(prefix, selected)?);
            }
        }
        Ok(())
    }

    /// Expands one record id of the given prefix.
    pub(crate) fn expand_record_id(&self, prefix: &str, id: &str) -> Result<String, AgentError> {
        match self.entries.get(id) {
            Some(entry) if entry.prefix == prefix => Ok(entry.native_id.clone()),
            Some(_) => Err(invalid(
                "ID has the wrong type; copy it from the matching result",
            )),
            None if record_alias_shape(id) => Err(invalid(
                "ID is unavailable in this conversation; copy it from a current result",
            )),
            None => Ok(id.into()),
        }
    }

    /// Present record ids in a result with the aliases registered for it.
    fn project_records(&self, value: &mut Value, source: &str) {
        visit_record_ids(value, &mut |field, id| {
            let prefix = RECORD_FIELDS.iter().find(|(name, _)| *name == field)?.1;
            self.alias(prefix, id, Some(source)).map(str::to_owned)
        });
    }

    /// Registers records that the model sees outside any conversation message,
    /// such as current memory facts. Returns whether the table changed.
    pub(crate) fn register_records<'a>(
        &mut self,
        field: &str,
        natives: impl Iterator<Item = &'a str>,
    ) -> Result<bool, AgentError> {
        let prefix = RECORD_FIELDS
            .iter()
            .find(|(name, _)| *name == field)
            .expect("known record field")
            .1;
        let before = self.entries.len();
        for native in natives {
            let reference = serde_json::json!({ field: native });
            if self.exact_alias(prefix, native, &reference).is_none() {
                self.register_entry(prefix, native.into(), &reference, ReferenceSource::Record)?;
            }
        }
        Ok(self.entries.len() != before)
    }

    /// Aliases of the given records, used to protect them from recycling.
    pub(crate) fn record_aliases<'a>(
        &self,
        field: &str,
        natives: impl Iterator<Item = &'a str>,
    ) -> BTreeSet<String> {
        let prefix = RECORD_FIELDS
            .iter()
            .find(|(name, _)| *name == field)
            .expect("known record field")
            .1;
        natives
            .filter_map(|native| self.alias(prefix, native, None).map(str::to_owned))
            .collect()
    }

    /// Presents current memory facts with their record aliases.
    pub(crate) fn project_current_memories(&self, message: &mut ChatMessage) {
        if !crate::ai_assistant::current_memory::is_projection(message) {
            return;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&message.text) else {
            return;
        };
        if let Some(memories) = value.get_mut("memories").and_then(Value::as_array_mut) {
            for memory in memories {
                if let Some(id) = memory.get_mut("memory_id")
                    && let Some(alias) = id
                        .as_str()
                        .and_then(|native| self.alias("mem", native, None))
                {
                    *id = Value::String(alias.into());
                }
            }
        }
        message.text = value.to_string();
    }

    fn observe_records(&mut self, value: &Value, message: &str) -> Result<(), AgentError> {
        let mut found = Vec::new();
        let mut copy = value.clone();
        visit_record_ids(&mut copy, &mut |field, id| {
            found.push((field.to_owned(), id.to_owned()));
            None
        });
        for (field, native) in found {
            let prefix = RECORD_FIELDS
                .iter()
                .find(|(name, _)| *name == field)
                .expect("visited record field")
                .1;
            self.register_record(&field, prefix, &native, message)?;
        }
        Ok(())
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
            // Recorded calls expand result and selector aliases on recovery.
            let mut copy = value.clone();
            let mut found = Vec::new();
            visit_result_selectors(&mut copy, &mut |id| {
                if let Some(id) = id.as_str() {
                    found.push(id.to_owned());
                }
            });
            for (field, _) in RECORD_FIELDS {
                if let Some(id) = value.get(*field).and_then(Value::as_str) {
                    found.push(id.to_owned());
                }
            }
            if tool == crate::conversation_attachment::READ_ATTACHMENT_TOOL
                && let Some(id) = value.get("attachment_id").and_then(Value::as_str)
            {
                found.push(id.to_owned());
            }
            for request in
                std::iter::once(value).chain(value["requests"].as_array().into_iter().flatten())
            {
                for (field, _) in SELECTOR_FIELDS {
                    if let Some(id) = request.get(field).and_then(Value::as_str) {
                        found.push(id.to_owned());
                    }
                }
            }
            ids.extend(
                found
                    .into_iter()
                    .filter(|id| state.entries.contains_key(id)),
            );
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
            if tool == "search_conversations"
                && let Some(conversations) = value.get("conversation_ids").and_then(Value::as_array)
            {
                for id in conversations.iter().filter_map(Value::as_str) {
                    if state
                        .entries
                        .get(id)
                        .is_some_and(|entry| entry.prefix == "cv")
                    {
                        ids.insert(id.into());
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

    pub(crate) fn argument_sources(&self, call: &ToolCall) -> Vec<BTreeSet<ReferenceSource>> {
        let mut message = ChatMessage::text("argument-sources", ChatRole::Assistant, "");
        message.tool_calls.push(crate::chat::ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        });
        self.retained_call_aliases(&[message], &[])
            .into_iter()
            .filter_map(|id| self.entries.get(&id).map(|entry| entry.sources.clone()))
            .collect()
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
                        ReferenceSource::Business(_) | ReferenceSource::Record => false,
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
        self.exact_alias(prefix, &native, value).map(String::as_str)
    }

    pub(crate) fn shorten_value(&self, value: &mut Value, source: Option<&str>) {
        crate::document_model_ids::map_live_references(value, &mut |reference| {
            self.shorten_value(reference, source)
        });
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
                let prefix = object
                    .get("kind")
                    .and_then(|kind| ObjectKind::deserialize(kind).ok())
                    .map(object_kind_prefix);
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
                for (field, _) in SELECTOR_FIELDS {
                    if let Some(id) = object.get_mut(field)
                        && let Some(native) = id.as_str()
                        && let Some(alias) = selector_prefix(native)
                            .and_then(|prefix| self.alias(prefix, native, None))
                    {
                        *id = Value::String(alias.into());
                    }
                }
                for field in RESULT_OUTPUT_FIELDS {
                    if let Some(id) = object.get_mut(field)
                        && let Some(alias) = id
                            .as_str()
                            .and_then(|native| self.alias("res", native, None))
                    {
                        *id = Value::String(alias.into());
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
        self.project_attachments(message);
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
            for path in ["/attachment_id", "/selection/attachment_id"] {
                if let Some(id) = receipt.pointer_mut(path)
                    && let Some(alias) =
                        id.as_str().and_then(|native| self.attachment_alias(native))
                {
                    *id = Value::String(alias.into());
                }
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
                self.shorten_value(body, None);
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
                    entry.prefix != ATTACHMENT_PREFIX
                        && entry.sources.iter().any(|origin| match origin {
                            ReferenceSource::Message(id) => id == &source,
                            ReferenceSource::Attachment { message, .. } => message == &source,
                            ReferenceSource::Business(_) | ReferenceSource::Record => false,
                        })
                }))
        {
            if is_schedule_result(message) {
                self.project_records(&mut value, &source);
                message.text = value.to_string();
                return;
            }
            crate::ui_model_output::expand_value(&mut value);
            if crate::assistant_data_model::tool(message).is_some() {
                self.project_records(&mut value, &source);
                self.project_conversations(&mut value, &source);
            }
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

    /// Approved inputs store the resolved native ID in plain ID fields. Show the short ID
    /// the model must call with, because execution only accepts observed short IDs.
    pub(crate) fn alias_native_id(&self, value: &mut Value) {
        let alias = value.as_str().and_then(|native| {
            self.entries
                .iter()
                .find(|(_, entry)| entry.native_id == native && !entry.sources.is_empty())
                .map(|(alias, _)| alias.clone())
        });
        if let Some(alias) = alias {
            *value = Value::String(alias);
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
        let entry = self.entries.get(id).ok_or_else(|| invalid("Object ID is unavailable in this conversation: it has not been observed or its source was removed. Read the matching object inspection tool and copy an ID from that result; no action was executed."))?;
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

    pub(crate) fn expand_arguments(
        &self,
        tool: &str,
        value: &mut Value,
        expected: &mut Vec<Value>,
    ) -> Result<(), AgentError> {
        self.expand_result_ids(value)?;
        self.expand_selector_ids(value)?;
        if tool == crate::conversation_attachment::READ_ATTACHMENT_TOOL
            && let Some(id) = value.get_mut("attachment_id")
            && let Some(selected) = id.as_str()
        {
            *id = Value::String(self.expand_record_id(ATTACHMENT_PREFIX, selected)?);
        }
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
                    let previous = expected.len();
                    self.expand_id(id, prefixes, expected)?;
                    if tool == "inspect_desktop_ui" && field == "element_id" {
                        // Search uses a stable element ID, not a snapshot reference.
                        expected.truncate(previous);
                    }
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
    fn incarnations_of_one_native_id_keep_separate_aliases() {
        let mut state = UiReferenceState::default();
        let first = reference("same-token", "snapshot-1");
        let second = reference("same-token", "snapshot-2");
        state.register(&first, "m1").unwrap();
        state.register(&first, "m2").unwrap();
        state.register(&second, "m3").unwrap();
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.projected_alias(&first), Some("a1"));
        assert_eq!(state.projected_alias(&second), Some("a2"));
        assert_eq!(state.alias("a", "same-token", Some("m2")), Some("a1"));
        assert_eq!(state.alias("a", "same-token", None), None);
        let restored: UiReferenceState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.projected_alias(&second), Some("a2"));
    }
    #[test]
    fn recorded_calls_restore_aliases_but_preserve_native_inputs_and_business_text() {
        exercise_recorded_call_inputs();
    }

    #[test]
    fn recorded_calls_restore_aliases_on_small_stacks() {
        for bytes in [1024 * 1024, 2 * 1024 * 1024] {
            std::thread::Builder::new()
                .stack_size(bytes)
                .spawn(exercise_recorded_call_inputs)
                .unwrap()
                .join()
                .unwrap();
        }
    }

    fn exercise_recorded_call_inputs() {
        let mut session =
            crate::ai_assistant::test_support::personal_session("owner", "run", "device");
        session.input_revision = 1;
        session.bind_assistant_input_targets(&[]).unwrap();
        let native = reference("native-application", "recorded-snapshot");
        session.ui_references.register(&native, "source").unwrap();
        let original = ToolCall {
            id: "inspect".into(),
            name: "inspect_desktop_ui".into(),
            arguments_json: r#"{ "target_ref": "device", "root_id": "a1", "queries": ["a1"] }"#
                .into(),
        };
        let executed =
            crate::ui_model_ids::resolve_session_call_with_context(&original, &session, 0, None)
                .unwrap();
        let recorded = crate::ui_model_ids::resolve_recorded_call(&original, &session).unwrap();
        let canonical = |call: &ToolCall| {
            crate::permission_tools::canonical_tool_permission_input_json(
                &call.name,
                serde_json::from_str(&call.arguments_json).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(canonical(&recorded), canonical(&executed));
        let value: Value = serde_json::from_str(&recorded.arguments_json).unwrap();
        assert_eq!(value["root"], native);
        assert_eq!(value["queries"], serde_json::json!(["a1"]));
        assert_eq!(
            canonical(&crate::ui_model_ids::resolve_recorded_call(&executed, &session).unwrap()),
            canonical(&executed)
        );
        session.conversation.extend([
            ChatMessage::assistant_tool_calls(
                "proposal",
                "",
                vec![crate::chat::ToolCallRef {
                    id: original.id.clone(),
                    name: original.name.clone(),
                    arguments_json: original.arguments_json.clone(),
                }],
            ),
            ChatMessage::tool_result(
                "later-observation",
                &original.id,
                serde_json::json!({"ReadContext":{"DesktopUiInspect":{
                    "root":reference("native-application", "later-snapshot")
                }}})
                .to_string(),
            ),
        ]);
        // An opaque alias retains its exact object; a newer observation with
        // the same native token must not silently rebind historical calls.
        assert_eq!(
            canonical(
                &crate::ui_model_ids::resolve_session_call_with_context(
                    &original, &session, 0, None
                )
                .unwrap()
            ),
            canonical(&executed)
        );
        assert_eq!(
            canonical(&crate::ui_model_ids::resolve_recorded_call(&original, &session).unwrap()),
            canonical(&executed)
        );
        let invalid = ToolCall {
            arguments_json: r#"{"root_id":"a99"}"#.into(),
            ..original.clone()
        };
        assert!(crate::ui_model_ids::resolve_recorded_call(&invalid, &session).is_err());
        let wrong_kind = ToolCall {
            arguments_json: r#"{"window_id":"a1"}"#.into(),
            name: "read_current_screen".into(),
            ..original.clone()
        };
        assert!(crate::ui_model_ids::resolve_recorded_call(&wrong_kind, &session).is_err());
        assert_eq!(
            original.arguments_json,
            r#"{ "target_ref": "device", "root_id": "a1", "queries": ["a1"] }"#
        );
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
        let resolved =
            crate::ui_model_ids::resolve_session_call_with_context(&call, &session, 1, None)
                .unwrap();
        let value: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(value["root"], original);
        let wrong = ToolCall {
            arguments_json: serde_json::json!({"root_id":"native-in-attachment"}).to_string(),
            ..call
        };
        assert!(
            crate::ui_model_ids::resolve_session_call_with_context(&wrong, &session, 1, None)
                .is_err()
        );
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
        let resolved =
            crate::ui_model_ids::resolve_session_call_with_context(&call, &session, 1, None)
                .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&resolved.arguments_json).unwrap()["target"],
            original
        );
        assert!(
            crate::ui_model_ids::resolve_session_call_with_context(
                &call,
                &session,
                4_200_000_000_000,
                None
            )
            .is_err()
        );
        session
            .ui_references
            .retain_attachment_sources(&BTreeSet::new(), &BTreeSet::from(["doc1".into()]));
        assert!(session.ui_references.entries.contains_key("doc1"));
        assert!(
            crate::ui_model_ids::resolve_session_call_with_context(&call, &session, 1, None)
                .is_err()
        );
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

#[cfg(test)]
mod record_tests {
    use super::*;
    use crate::chat::ChatMessage;

    fn schedule_result(message_id: &str, ids: &[String]) -> ChatMessage {
        let parent = crate::model_message_labels::model_bound_user_message(
            "owner".into(),
            "List timers".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        let text = serde_json::json!({
            "ok": true,
            "tasks": ids.iter().map(|id| serde_json::json!({"schedule_id": id})).collect::<Vec<_>>()
        })
        .to_string();
        let mut message = ChatMessage::tool_result(message_id, "query", &text);
        message.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            parent.data_envelope.as_ref(),
            message_id,
            &text,
            "schedule_query",
        )
        .unwrap();
        message
    }

    fn schedule(index: u8) -> String {
        crate::native_ids::from_bytes(crate::native_ids::Kind::Schedule, &[index; 16])
    }

    #[test]
    fn schedule_aliases_survive_their_result_and_expand_by_type() {
        let mut state = UiReferenceState::default();
        let result = schedule_result("listed", &[schedule(1)]);
        state.observe(std::slice::from_ref(&result)).unwrap();
        let mut projected = result.clone();
        state.project_message(&mut projected);
        let shown: Value = serde_json::from_str(&projected.text).unwrap();
        assert_eq!(shown["tasks"][0]["schedule_id"], "sc1");

        // The listed result leaves the conversation; the record alias stays.
        assert!(state.retain_sources(&BTreeSet::new(), &BTreeSet::new()));
        assert_eq!(state.expand_record_id("sc", "sc1").unwrap(), schedule(1));
        let restored: UiReferenceState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.expand_record_id("sc", "sc1").unwrap(), schedule(1));

        assert_eq!(
            state.expand_record_id("sc", &schedule(2)).unwrap(),
            schedule(2)
        );
        assert!(state.expand_record_id("sc", "sc9").is_err());
        let mut arguments = serde_json::json!({"schedule_id": "sc1"});
        state.expand_record_ids(&mut arguments).unwrap();
        assert_eq!(arguments["schedule_id"], schedule(1));

        // Seeing the same record again reuses its alias.
        state
            .observe(&[schedule_result("again", &[schedule(1)])])
            .unwrap();
        assert_eq!(state.entries.len(), 1);
    }

    #[test]
    fn memory_results_and_current_facts_share_one_alias() {
        let native = crate::native_ids::stable(crate::native_ids::Kind::Memory, &"fact");
        let mut state = UiReferenceState::default();
        // A current fact registers before any result shows it.
        assert!(
            state
                .register_records("memory_id", std::iter::once(native.as_str()))
                .unwrap()
        );
        assert!(
            !state
                .register_records("memory_id", std::iter::once(native.as_str()))
                .unwrap()
        );
        let call = ChatMessage::assistant_tool_calls(
            "caller",
            "",
            vec![crate::chat::ToolCallRef {
                id: "search".into(),
                name: crate::ai_assistant::data_tools::SEARCH_MEMORIES.into(),
                arguments_json: "{}".into(),
            }],
        );
        let text = serde_json::json!({"memories":[{"memory_id": native}]}).to_string();
        let mut result = schedule_result("found", &[]);
        result.tool_call_id = Some("search".into());
        result.text = text.clone();
        result.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            result.data_envelope.as_ref(),
            "found",
            &text,
            crate::ai_assistant::data_tools::SEARCH_MEMORIES,
        )
        .unwrap();
        state.observe(&[call, result.clone()]).unwrap();
        assert_eq!(state.entries.len(), 1);
        let mut projected = result;
        state.project_message(&mut projected);
        let shown: Value = serde_json::from_str(&projected.text).unwrap();
        assert_eq!(shown["memories"][0]["memory_id"], "mem1");
        assert_eq!(state.expand_record_id("mem", "mem1").unwrap(), native);
        assert!(state.expand_record_id("sc", "mem1").is_err());
        assert_eq!(
            state.record_aliases("memory_id", std::iter::once(native.as_str())),
            BTreeSet::from(["mem1".to_owned()])
        );
    }

    #[test]
    fn record_aliases_recycle_oldest_unprotected_beyond_the_limit() {
        let mut state = UiReferenceState::default();
        let ids = (0..=MAX_RECORD_ALIASES + 1)
            .map(|index| crate::native_ids::stable(crate::native_ids::Kind::Schedule, &index))
            .collect::<Vec<_>>();
        state.observe(&[schedule_result("listed", &ids)]).unwrap();
        let total = MAX_RECORD_ALIASES + 2;
        assert_eq!(state.entries.len(), total);

        // While the result is present nothing is recycled.
        let present = BTreeSet::from(["listed".to_owned()]);
        assert!(!state.retain_sources(&present, &BTreeSet::new()));
        assert_eq!(state.entries.len(), total);

        // sc1 is protected, so sc2 is the only candidate beyond the limit;
        // numeric order keeps sc10 although it sorts before sc2 as text.
        let protected = BTreeSet::from(["sc1".to_owned()]);
        assert!(state.retain_sources(&BTreeSet::new(), &protected));
        assert_eq!(state.entries.len(), MAX_RECORD_ALIASES + 1);
        for kept in ["sc1", "sc3", "sc10"] {
            assert!(state.entries.contains_key(kept), "{kept}");
        }
        assert!(state.expand_record_id("sc", "sc2").is_err());

        // A recycled record that appears again gets a new alias.
        state
            .observe(&[schedule_result("again", &ids[1..2])])
            .unwrap();
        let mut renewed = serde_json::json!({"schedule_id": ids[1].clone()});
        visit_record_ids(&mut renewed, &mut |_, native| {
            state.alias("sc", native, None).map(str::to_owned)
        });
        assert_eq!(renewed["schedule_id"], format!("sc{}", total + 1));
    }

    fn delivered(message_id: &str, list: &str, native: &str, slot: bool) -> ChatMessage {
        let text = serde_json::json!({
            list: [{"name":"stdout","source_truncated":false,"content":{"storage":"attachment","reference":{"attachment_id":native,"kind":"text","original_bytes":9000,"stored_bytes":9000,"storage_truncated":false}}}],
            "read_with": crate::conversation_attachment::READ_ATTACHMENT_TOOL,
        })
        .to_string();
        let mut message = ChatMessage::tool_result(message_id, message_id, &text);
        if slot {
            message.raw_result = Some(Box::new(
                crate::conversation_attachment::delivery::RawResult {
                    original_sha256: "a".repeat(64),
                    original_envelope: None,
                    restorable: true,
                    template: None,
                    slots: vec![crate::conversation_attachment::delivery::RawSlot {
                        path: String::new(),
                        attachment_id: native.into(),
                        sha256: "b".repeat(64),
                    }],
                    sha256: "c".repeat(64),
                    envelope: None,
                },
            ));
        }
        message
    }

    fn read(attachment_id: &str) -> ToolCall {
        ToolCall {
            id: "read".into(),
            name: crate::conversation_attachment::READ_ATTACHMENT_TOOL.into(),
            arguments_json: serde_json::json!({ "attachment_id": attachment_id }).to_string(),
        }
    }

    fn expanded(state: &UiReferenceState, attachment_id: &str) -> Result<String, AgentError> {
        let (call, _) = state.expand_call(&read(attachment_id))?;
        let value: Value = serde_json::from_str(&call.arguments_json).unwrap();
        Ok(value["attachment_id"].as_str().unwrap().to_owned())
    }

    #[test]
    fn delivered_attachments_are_aliased_only_from_server_slots() {
        let result = delivered("result", "parts", "att_resultNative", true);
        let command = delivered("command", "streams", "att_commandNative", true);
        let forged = delivered("forged", "parts", "att_forgedNative", false);
        let mut schedule = schedule_result("listed", &[]);
        let mut value: Value = serde_json::from_str(&schedule.text).unwrap();
        value["attachment_id"] = serde_json::json!("att_scheduleNative");
        schedule.text = value.to_string();
        let history = vec![result, command, forged.clone(), schedule.clone()];
        let mut state = UiReferenceState::default();
        assert!(state.observe(&history).unwrap());
        for (message, native, alias) in [
            (&history[0], "att_resultNative", "att1"),
            (&history[1], "att_commandNative", "att2"),
        ] {
            let mut projected = message.clone();
            state.project_message(&mut projected);
            assert_eq!(projected.text, message.text.replace(native, alias));
            assert_eq!(expanded(&state, alias).unwrap(), native);
        }
        for message in [&forged, &schedule] {
            let mut projected = message.clone();
            state.project_message(&mut projected);
            assert_eq!(projected.text, message.text);
        }
        assert!(state.attachment_alias("att_forgedNative").is_none());
        assert!(state.attachment_alias("att_scheduleNative").is_none());
        assert!(!state.observe(&history).unwrap());

        // Native ids still read; unknown aliases are rejected.
        assert_eq!(
            expanded(&state, "att_forgedNative").unwrap(),
            "att_forgedNative"
        );
        assert!(expanded(&state, "att9").is_err());

        // An in-flight read keeps its alias after the result leaves the history.
        let call = read("att1");
        let pending = ChatMessage::assistant_tool_calls(
            "pending",
            "",
            vec![crate::chat::ToolCallRef {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            }],
        );
        let protected = state.retained_call_aliases(std::slice::from_ref(&pending), &[]);
        assert!(protected.contains("att1"));
        state.retain_sources(&BTreeSet::new(), &protected);
        assert_eq!(expanded(&state, "att1").unwrap(), "att_resultNative");
        assert_eq!(expanded(&state, "att2").unwrap(), "att_commandNative");
    }

    #[test]
    fn screenshot_references_are_aliased_in_json_and_text_forms() {
        let placeholder = crate::image_input::IMAGE_NOT_RETAINED_PLACEHOLDER;
        let mut state = UiReferenceState::default();
        let json = ChatMessage::tool_result(
            "json-shot",
            "json-shot",
            format!(
                "{}\n{placeholder}",
                serde_json::json!({"ok":true,"attachment_id":"att_jsonShot"})
            ),
        );
        let text = ChatMessage::tool_result(
            "text-shot",
            "text-shot",
            format!(
                "Window\nScreenshot attachment_id: att_forgedShot\nScreenshot attachment_id: att_textShot\n{placeholder}"
            ),
        );
        state
            .register_attachment("att_jsonShot", "json-shot")
            .unwrap();
        state
            .register_attachment("att_textShot", "text-shot")
            .unwrap();
        for (message, native, alias) in [
            (&json, "att_jsonShot", "att1"),
            (&text, "att_textShot", "att2"),
        ] {
            let mut projected = message.clone();
            state.project_message(&mut projected);
            assert_eq!(projected.text, message.text.replace(native, alias));
            assert!(projected.text.ends_with(placeholder));
        }
        // The same native shown by another message is not projected there.
        let mut other =
            ChatMessage::tool_result("other", "other", "Screenshot attachment_id: att_jsonShot");
        let original = other.text.clone();
        state.project_message(&mut other);
        assert_eq!(other.text, original);
    }
}
