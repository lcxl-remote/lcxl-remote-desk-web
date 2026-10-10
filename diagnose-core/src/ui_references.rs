//! Session-owned model aliases. Native references and authorization stay intact.

use std::collections::{BTreeMap, BTreeSet};

use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{ObjectKind, ObjectRef},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::chat::{ChatMessage, ChatRole, ToolCall};

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiReferenceState {
    counters: BTreeMap<String, u64>,
    entries: BTreeMap<String, ReferenceEntry>,
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
    sources: BTreeSet<String>,
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
    if let Ok(reference) = serde_json::from_value::<ObjectRef>(value.clone()) {
        let prefix = match reference.object_kind {
            ObjectKind::DesktopSession => "s",
            ObjectKind::Application => "a",
            ObjectKind::Window => "w",
            ObjectKind::UiElement => "e",
            ObjectKind::DesktopOutput => "o",
            _ => return None,
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

fn visit_references(value: &Value, visitor: &mut impl FnMut(&Value)) {
    if native_reference(value).is_some() {
        visitor(value);
        return;
    }
    match value {
        Value::Object(object) => object.values().for_each(|v| visit_references(v, visitor)),
        Value::Array(array) => array.iter().for_each(|v| visit_references(v, visitor)),
        _ => {}
    }
}

impl UiReferenceState {
    fn register(&mut self, value: &Value, source: &str) -> Result<(), AgentError> {
        let Some((prefix, native_id)) = native_reference(value) else {
            return Ok(());
        };
        let identity = format!("{:x}", Sha256::digest(value.to_string().as_bytes()));
        if let Some(entry) = self
            .entries
            .values_mut()
            .find(|entry| entry.identity == identity)
        {
            entry.sources.insert(source.into());
            return Ok(());
        }
        let counter = self.counters.entry(prefix.into()).or_default();
        *counter = counter
            .checked_add(1)
            .ok_or_else(|| invalid("UI reference sequence exhausted"))?;
        self.entries.insert(
            format!("{prefix}{counter}"),
            ReferenceEntry {
                identity,
                prefix: prefix.into(),
                native_id,
                reference: value.clone(),
                sources: BTreeSet::from([source.into()]),
            },
        );
        Ok(())
    }

    /// Only accepted device observations allocate aliases. Ordering follows the
    /// persisted result structure, so rebuilding a request never allocates IDs.
    pub fn observe(&mut self, history: &[ChatMessage]) -> Result<bool, AgentError> {
        let before = self.clone();
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
            if !native && !browser {
                continue;
            }
            let Ok(mut value) = crate::image_input::structured_tool_result(&source.text) else {
                continue;
            };
            if native {
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
        }
        Ok(*self != before)
    }

    /// Retire unreachable mappings without reusing their sequence numbers.
    pub fn retain_sources(&mut self, sources: &BTreeSet<String>, protected: &BTreeSet<String>) {
        self.entries.retain(|id, entry| {
            entry.sources.retain(|source| sources.contains(source));
            !entry.sources.is_empty() || protected.contains(id)
        });
    }

    pub(crate) fn retained_call_aliases(&self, history: &[ChatMessage]) -> BTreeSet<String> {
        fn collect(
            value: &Value,
            entries: &BTreeMap<String, ReferenceEntry>,
            ids: &mut BTreeSet<String>,
        ) {
            match value {
                Value::String(id) if entries.contains_key(id) => {
                    ids.insert(id.clone());
                }
                Value::Object(object) => object
                    .values()
                    .for_each(|value| collect(value, entries, ids)),
                Value::Array(array) => array.iter().for_each(|value| collect(value, entries, ids)),
                _ => {}
            }
        }
        let mut ids = BTreeSet::new();
        for call in history.iter().flat_map(|message| &message.tool_calls) {
            if let Ok(value) = serde_json::from_str::<Value>(&call.arguments_json) {
                collect(&value, &self.entries, &mut ids);
            }
        }
        ids
    }

    fn alias(&self, prefix: &str, native_id: &str, source: Option<&str>) -> Option<&str> {
        let mut matches = self.entries.iter().filter(|(_, entry)| {
            entry.prefix == prefix
                && entry.native_id == native_id
                && source.is_none_or(|source| entry.sources.contains(source))
        });
        let first = matches.next()?;
        // Multiple incarnations must never silently bind to the latest one.
        matches.next().is_none().then_some(first.0.as_str())
    }

    fn projected_alias(&self, value: &Value) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, entry)| &entry.reference == value)
            .map(|(id, _)| id.as_str())
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
                for child in object.values_mut() {
                    self.shorten_value(child, source);
                }
            }
            Value::Array(values) => values
                .iter_mut()
                .for_each(|v| self.shorten_value(v, source)),
            _ => {}
        }
    }

    pub(crate) fn project_message(&self, message: &mut ChatMessage) {
        let source = message.message_id.clone();
        if matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
            && let Ok(mut value) = crate::image_input::structured_tool_result(&message.text)
            && (value.pointer("/ReadContext").is_some()
                || self
                    .entries
                    .values()
                    .any(|entry| entry.sources.contains(&source)))
        {
            crate::ui_model_output::expand_value(&mut value);
            self.shorten_value(&mut value, Some(&source));
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
        ) || crate::browser_model_ids::supports(&call.name);
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
                .values_mut()
                .for_each(|v| self.alias_native_arguments(v)),
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
        let entry = self.entries.get(id).ok_or_else(|| invalid("Unknown or retired UI ID. Read the desktop/UI or browser again; no action was executed."))?;
        if !prefixes.contains(&entry.prefix.as_str()) {
            return Err(invalid(
                "UI ID has the wrong object type; read the required object again",
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
        let native = matches!(
            tool,
            "inspect_desktop_ui"
                | "execute_ui_actions"
                | "send_background_input"
                | "send_raw_input"
                | "execute_wayland_output_input"
                | "read_current_screen"
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
