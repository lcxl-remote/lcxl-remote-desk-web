//! Bounded, read-only Windows UI Automation projection.
//!
//! This adapter never invokes a UIA action. It rechecks the foreground process
//! after COM initialization, redacts password controls before reading name or
//! value, and stops at the first caller or hard deadline/size bound.

use std::time::{Duration, Instant};

use desk_agent_protocol::computer_use::UiSemanticActionKind;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationTreeWalker,
    IUIAutomationValuePattern, UIA_InvokePatternId, UIA_SelectionItemPatternId,
    UIA_TogglePatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
use windows::core::PWSTR;

const HARD_DEADLINE: Duration = Duration::from_secs(2);
const MAX_STRING_BYTES: usize = 16 * 1024;
const OBJECT_REF_BUDGET: usize = 320;

#[derive(Clone, Debug, Serialize)]
pub struct CollectedNode {
    pub parent_index: Option<u32>,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub is_protected: bool,
    pub enabled: bool,
    pub supported_actions: Vec<UiSemanticActionKind>,
    pub fingerprint: String,
}

pub struct CollectedTree {
    pub nodes: Vec<CollectedNode>,
    pub truncated: bool,
}

struct ComGuard;

impl ComGuard {
    fn initialize() -> Result<Self, AgentError> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(|_| failure("Windows UI Automation COM initialization failed", true))?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct WalkConfig<'a> {
    walker: &'a IUIAutomationTreeWalker,
    process_id: u32,
    max_depth: u16,
    max_nodes: usize,
    max_bytes: usize,
    deadline: Instant,
}

#[derive(Default)]
struct WalkState {
    encoded_bytes: usize,
    truncated: bool,
}

pub fn collect_foreground(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
) -> Result<CollectedTree, AgentError> {
    let _com = ComGuard::initialize()?;
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return Err(failure("the foreground window disappeared", true));
    }
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    let image_path = process_image(process_id)
        .ok_or_else(|| failure("cannot resolve the foreground process image", true))?;
    if process_id != expected_process_id || !path_eq(&image_path, expected_image_path) {
        return Err(failure(
            "the foreground application changed during UI inspection",
            true,
        ));
    }

    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| failure("Windows UI Automation is unavailable", true))?;
    let root = unsafe { automation.ElementFromHandle(hwnd) }
        .map_err(|_| failure("the foreground window has no UI Automation root", true))?;
    let root_process_id = unsafe { root.CurrentProcessId() }
        .unwrap_or_default()
        .max(0) as u32;
    if root_process_id != expected_process_id {
        return Err(failure(
            "the foreground UI Automation root belongs to a different process",
            false,
        ));
    }
    let walker = unsafe { automation.ControlViewWalker() }
        .map_err(|_| failure("cannot create a UI Automation tree walker", true))?;
    let config = WalkConfig {
        walker: &walker,
        process_id: expected_process_id,
        max_depth,
        max_nodes: max_nodes as usize,
        max_bytes: max_bytes as usize,
        deadline: Instant::now() + HARD_DEADLINE,
    };
    let mut state = WalkState::default();
    let mut nodes = Vec::new();
    walk(root, None, 0, 0, &config, &mut state, &mut nodes);
    Ok(CollectedTree {
        nodes,
        truncated: state.truncated,
    })
}

fn walk(
    element: IUIAutomationElement,
    parent: Option<(u32, String)>,
    depth: u16,
    sibling_ordinal: usize,
    config: &WalkConfig<'_>,
    state: &mut WalkState,
    output: &mut Vec<CollectedNode>,
) {
    if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
        state.truncated = true;
        return;
    }
    let element_process_id = unsafe { element.CurrentProcessId() }
        .unwrap_or_default()
        .max(0) as u32;
    if element_process_id != config.process_id {
        state.truncated = true;
        return;
    }
    let (node, strings_truncated) = read_node(
        &element,
        parent.as_ref().map(|(_, fingerprint)| fingerprint.as_str()),
        parent.as_ref().map(|(index, _)| *index),
        sibling_ordinal,
    );
    let encoded_bytes = serde_json::to_vec(&node)
        .map_or(config.max_bytes.saturating_add(1), |encoded| encoded.len())
        .saturating_add(OBJECT_REF_BUDGET);
    if state.encoded_bytes.saturating_add(encoded_bytes) > config.max_bytes {
        state.truncated = true;
        return;
    }
    state.encoded_bytes += encoded_bytes;
    state.truncated |= strings_truncated;
    let index = output.len() as u32;
    let fingerprint = node.fingerprint.clone();
    output.push(node);

    if depth >= config.max_depth {
        if unsafe { config.walker.GetFirstChildElement(&element) }.is_ok() {
            state.truncated = true;
        }
        return;
    }
    let Ok(mut child) = (unsafe { config.walker.GetFirstChildElement(&element) }) else {
        return;
    };
    let mut ordinal = 0usize;
    loop {
        walk(
            child.clone(),
            Some((index, fingerprint.clone())),
            depth + 1,
            ordinal,
            config,
            state,
            output,
        );
        if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
            state.truncated = true;
            return;
        }
        let Ok(next) = (unsafe { config.walker.GetNextSiblingElement(&child) }) else {
            break;
        };
        ordinal += 1;
        child = next;
    }
}

fn read_node(
    element: &IUIAutomationElement,
    parent_fingerprint: Option<&str>,
    parent_index: Option<u32>,
    sibling_ordinal: usize,
) -> (CollectedNode, bool) {
    unsafe {
        let process_id = element.CurrentProcessId().unwrap_or_default();
        let started = process_start(process_id.max(0) as u32);
        let hwnd = element
            .CurrentNativeWindowHandle()
            .map(|value| value.0 as isize)
            .unwrap_or_default();
        let control_type = element
            .CurrentControlType()
            .map(|value| value.0)
            .unwrap_or_default();
        let automation_id = element
            .CurrentAutomationId()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let is_protected = element
            .CurrentIsPassword()
            .map(|value| value.as_bool())
            .unwrap_or(true);
        let enabled = element
            .CurrentIsEnabled()
            .map(|value| value.as_bool())
            .unwrap_or(false);
        let (role, role_truncated) = bounded_string(
            element
                .CurrentLocalizedControlType()
                .map(|value| value.to_string())
                .unwrap_or_else(|_| format!("control_type:{control_type}")),
        );
        let (name, name_truncated) = if is_protected {
            (None, false)
        } else {
            let raw = element
                .CurrentName()
                .map(|value| value.to_string())
                .unwrap_or_default();
            let (value, truncated) = bounded_string(raw);
            ((!value.is_empty()).then_some(value), truncated)
        };

        let invoke = element
            .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            .ok();
        let toggle = element
            .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
            .ok();
        let select = element
            .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            .ok();
        let value_pattern = element
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok();
        let (value, value_truncated) = if is_protected {
            (None, false)
        } else if let Some(pattern) = value_pattern.as_ref() {
            let raw = pattern
                .CurrentValue()
                .map(|value| value.to_string())
                .unwrap_or_default();
            let (value, truncated) = bounded_string(raw);
            ((!value.is_empty()).then_some(value), truncated)
        } else {
            (None, false)
        };
        let mut supported_actions = Vec::new();
        if invoke.is_some() {
            supported_actions.push(UiSemanticActionKind::Invoke);
        }
        if toggle.is_some() {
            supported_actions.push(UiSemanticActionKind::Toggle);
        }
        if select.is_some() {
            supported_actions.push(UiSemanticActionKind::Select);
        }
        if !is_protected
            && value_pattern.as_ref().is_some_and(|pattern| {
                !pattern
                    .CurrentIsReadOnly()
                    .map(|value| value.as_bool())
                    .unwrap_or(true)
            })
        {
            supported_actions.push(UiSemanticActionKind::SetValue);
        }
        let fingerprint = fingerprint(
            parent_fingerprint,
            sibling_ordinal,
            process_id,
            started,
            hwnd,
            control_type,
            &automation_id,
        );
        (
            CollectedNode {
                parent_index,
                role,
                name,
                value,
                is_protected,
                enabled,
                supported_actions,
                fingerprint,
            },
            role_truncated || name_truncated || value_truncated,
        )
    }
}

fn bounded_string(mut value: String) -> (String, bool) {
    if value.len() <= MAX_STRING_BYTES {
        return (value, false);
    }
    let mut end = MAX_STRING_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

fn fingerprint(
    parent: Option<&str>,
    sibling_ordinal: usize,
    process_id: i32,
    process_started_at: Option<u64>,
    hwnd: isize,
    control_type: i32,
    automation_id: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(parent.unwrap_or("root").as_bytes());
    hasher.update(sibling_ordinal.to_le_bytes());
    hasher.update(process_id.to_le_bytes());
    hasher.update(process_started_at.unwrap_or_default().to_le_bytes());
    hasher.update(hwnd.to_le_bytes());
    hasher.update(control_type.to_le_bytes());
    hasher.update(automation_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn process_start(process_id: u32) -> Option<u64> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let result =
            GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user).ok();
        let _ = CloseHandle(process);
        result.map(|_| ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
    }
}

fn process_image(process_id: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
        .ok();
        let _ = CloseHandle(process);
        result.map(|_| String::from_utf16_lossy(&buffer[..length as usize]))
    }
}

fn path_eq(left: &str, right: &str) -> bool {
    left.replace('/', "\\")
        .eq_ignore_ascii_case(&right.replace('/', "\\"))
}

fn failure(message: &str, retryable: bool) -> AgentError {
    AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: message.to_string(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_strings_stop_on_utf8_boundaries() {
        let source = "界".repeat(MAX_STRING_BYTES);
        let (value, truncated) = bounded_string(source);
        assert!(truncated);
        assert!(value.len() <= MAX_STRING_BYTES);
        assert!(value.is_char_boundary(value.len()));
    }

    #[test]
    fn fingerprints_bind_process_incarnation_and_parent() {
        let first = fingerprint(Some("parent-a"), 1, 4, Some(8), 9, 10, "id");
        let second = fingerprint(Some("parent-b"), 1, 4, Some(8), 9, 10, "id");
        let restarted = fingerprint(Some("parent-a"), 1, 4, Some(9), 9, 10, "id");
        assert_ne!(first, second);
        assert_ne!(first, restarted);
    }
}
