//! Bounded Windows UI Automation projection and semantic actions.
//!
//! Every action rechecks the foreground process and relocates the target from
//! its process-incarnation-bound fingerprint. Password controls fail closed;
//! mutations are limited to typed UIA patterns and independently read back.

use std::time::{Duration, Instant};

use desk_agent_protocol::computer_use::{UiSemanticAction, UiSemanticActionKind};
use desk_agent_protocol::{AgentError, AgentErrorKind};
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
    IUIAutomationValuePattern, ToggleState, ToggleState_Off, ToggleState_On, UIA_InvokePatternId,
    UIA_SelectionItemPatternId, UIA_TogglePatternId, UIA_ValuePatternId, UIA_WindowControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
use windows::core::{BSTR, PWSTR};

const HARD_DEADLINE: Duration = Duration::from_secs(2);
const MAX_STRING_BYTES: usize = 16 * 1024;
const OBJECT_REF_BUDGET: usize = 320;
const ACTION_MAX_DEPTH: u16 = 16;
const ACTION_MAX_NODES: usize = 1_024;
const INVOKE_READBACK_MAX_BYTES: u32 = 1024 * 1024;
const INVOKE_READBACK_TIMEOUT: Duration = Duration::from_millis(750);
const INVOKE_READBACK_INTERVAL: Duration = Duration::from_millis(25);

use super::computer_use_broker::{CollectedUiNode, CollectedUiTree};

struct ComGuard;

impl ComGuard {
    fn initialize() -> Result<Self, AgentError> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(|_| failure("Windows UI Automation COM initialization failed", true))?;
        Ok(Self)
    }
}

struct LocatedElement {
    // COM interface fields must be dropped before the apartment guard.
    element: IUIAutomationElement,
    _com: ComGuard,
}

pub(super) struct AppliedUiAction {
    pub(super) changed: bool,
    pub(super) verified: bool,
    pub(super) summary: String,
}

#[derive(Clone, Debug)]
pub(super) struct WindowsForegroundApplication {
    pub(super) window_handle: isize,
    pub(super) process_id: u32,
    pub(super) image_path: String,
    pub(super) process_started_at: u64,
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

pub(super) fn resolve_foreground_application() -> Result<WindowsForegroundApplication, AgentError> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return Err(failure("the foreground window disappeared", true));
    }
    let mut host_process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut host_process_id)) };
    let host_image_path = process_image(host_process_id)
        .ok_or_else(|| failure("cannot resolve the foreground process image", true))?;
    let (process_id, image_path) = if executable_name(&host_image_path)
        .eq_ignore_ascii_case("ApplicationFrameHost.exe")
    {
        let _com = ComGuard::initialize()?;
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| failure("Windows UI Automation is unavailable", true))?;
        let root = unsafe { automation.ElementFromHandle(hwnd) }
            .map_err(|_| failure("the foreground window has no UI Automation root", true))?;
        let walker = unsafe { automation.ControlViewWalker() }
            .map_err(|_| failure("cannot create a UI Automation tree walker", true))?;
        let mut candidates = Vec::new();
        let mut visited = 0usize;
        collect_hosted_window_processes(
            &root,
            &walker,
            host_process_id,
            0,
            Instant::now() + HARD_DEADLINE,
            &mut visited,
            &mut candidates,
        );
        candidates.sort_unstable();
        candidates.dedup();
        if candidates.len() != 1 {
            return Err(failure(
                "the hosted foreground window does not resolve to exactly one application process",
                false,
            ));
        }
        let process_id = candidates[0];
        let image_path = process_image(process_id).ok_or_else(|| {
            failure(
                "cannot resolve the hosted foreground application image",
                false,
            )
        })?;
        (process_id, image_path)
    } else {
        (host_process_id, host_image_path)
    };
    let process_started_at = process_start(process_id).ok_or_else(|| {
        failure(
            "cannot bind the foreground application to its process incarnation",
            false,
        )
    })?;
    Ok(WindowsForegroundApplication {
        window_handle: hwnd.0 as isize,
        process_id,
        image_path,
        process_started_at,
    })
}

fn executable_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

#[allow(clippy::too_many_arguments)]
fn collect_hosted_window_processes(
    element: &IUIAutomationElement,
    walker: &IUIAutomationTreeWalker,
    host_process_id: u32,
    depth: u16,
    deadline: Instant,
    visited: &mut usize,
    candidates: &mut Vec<u32>,
) {
    if *visited >= ACTION_MAX_NODES || Instant::now() >= deadline {
        return;
    }
    *visited += 1;
    let process_id = unsafe { element.CurrentProcessId() }
        .unwrap_or_default()
        .max(0) as u32;
    let control_type = unsafe { element.CurrentControlType() }
        .map(|value| value.0)
        .unwrap_or_default();
    if process_id != 0 && process_id != host_process_id && control_type == UIA_WindowControlTypeId.0
    {
        candidates.push(process_id);
    }
    if depth >= ACTION_MAX_DEPTH {
        return;
    }
    let Ok(mut child) = (unsafe { walker.GetFirstChildElement(element) }) else {
        return;
    };
    loop {
        collect_hosted_window_processes(
            &child,
            walker,
            host_process_id,
            depth + 1,
            deadline,
            visited,
            candidates,
        );
        if *visited >= ACTION_MAX_NODES || Instant::now() >= deadline {
            return;
        }
        let Ok(next) = (unsafe { walker.GetNextSiblingElement(&child) }) else {
            return;
        };
        child = next;
    }
}

fn find_process_root(
    element: &IUIAutomationElement,
    walker: &IUIAutomationTreeWalker,
    expected_process_id: u32,
    depth: u16,
    deadline: Instant,
    visited: &mut usize,
) -> Option<IUIAutomationElement> {
    if *visited >= ACTION_MAX_NODES || Instant::now() >= deadline {
        return None;
    }
    *visited += 1;
    let process_id = unsafe { element.CurrentProcessId() }.ok()?.max(0) as u32;
    if process_id == expected_process_id {
        return Some(element.clone());
    }
    if depth >= ACTION_MAX_DEPTH {
        return None;
    }
    let mut child = unsafe { walker.GetFirstChildElement(element) }.ok()?;
    loop {
        if let Some(found) = find_process_root(
            &child,
            walker,
            expected_process_id,
            depth + 1,
            deadline,
            visited,
        ) {
            return Some(found);
        }
        if *visited >= ACTION_MAX_NODES || Instant::now() >= deadline {
            return None;
        }
        let Ok(next) = (unsafe { walker.GetNextSiblingElement(&child) }) else {
            return None;
        };
        child = next;
    }
}

pub(super) fn collect_foreground(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
) -> Result<CollectedUiTree, AgentError> {
    let _com = ComGuard::initialize()?;
    let foreground = resolve_foreground_application()?;
    if foreground.process_id != expected_process_id
        || !path_eq(&foreground.image_path, expected_image_path)
    {
        return Err(failure(
            "the foreground application changed during UI inspection",
            true,
        ));
    }

    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| failure("Windows UI Automation is unavailable", true))?;
    let root = unsafe {
        automation.ElementFromHandle(windows::Win32::Foundation::HWND(
            foreground.window_handle as *mut std::ffi::c_void,
        ))
    }
    .map_err(|_| failure("the foreground window has no UI Automation root", true))?;
    let walker = unsafe { automation.ControlViewWalker() }
        .map_err(|_| failure("cannot create a UI Automation tree walker", true))?;
    let mut root_search_visited = 0usize;
    let root = find_process_root(
        &root,
        &walker,
        expected_process_id,
        0,
        Instant::now() + HARD_DEADLINE,
        &mut root_search_visited,
    )
    .ok_or_else(|| {
        failure(
            "the foreground UI Automation tree has no root for the resolved application process",
            false,
        )
    })?;
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
    Ok(CollectedUiTree {
        nodes,
        truncated: state.truncated,
    })
}

pub(super) fn foreground_contains_protected_control(
    expected_process_id: u32,
    expected_image_path: &str,
) -> Result<bool, AgentError> {
    collect_foreground(
        expected_process_id,
        expected_image_path,
        ACTION_MAX_DEPTH,
        ACTION_MAX_NODES as u32,
        INVOKE_READBACK_MAX_BYTES,
    )
    .map(|tree| tree.truncated || tree.nodes.iter().any(|node| node.is_protected))
}

pub(super) fn preflight_action(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
    action: &UiSemanticAction,
) -> Result<(), AgentError> {
    let target =
        locate_action_target(expected_process_id, expected_image_path, target_fingerprint)?;
    validate_action_target(&target.element, action)
}

pub(super) fn apply_action(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
    action: &UiSemanticAction,
) -> Result<AppliedUiAction, AgentError> {
    let target =
        locate_action_target(expected_process_id, expected_image_path, target_fingerprint)?;
    validate_action_target(&target.element, action)?;
    match action {
        UiSemanticAction::Invoke => {
            let before = collect_foreground(
                expected_process_id,
                expected_image_path,
                ACTION_MAX_DEPTH,
                ACTION_MAX_NODES as u32,
                INVOKE_READBACK_MAX_BYTES,
            )?;
            let before_digest = semantic_tree_digest(&before);
            let pattern = invoke_pattern(&target.element)?;
            unsafe { pattern.Invoke() }.map_err(|_| {
                action_failure("the UI Automation invoke action was rejected by the target")
            })?;
            let deadline = Instant::now() + INVOKE_READBACK_TIMEOUT;
            let verified = loop {
                match collect_foreground(
                    expected_process_id,
                    expected_image_path,
                    ACTION_MAX_DEPTH,
                    ACTION_MAX_NODES as u32,
                    INVOKE_READBACK_MAX_BYTES,
                ) {
                    Ok(after) if semantic_tree_digest(&after) != before_digest => break true,
                    Ok(_) => {}
                    Err(_) => break false,
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(INVOKE_READBACK_INTERVAL);
            };
            Ok(AppliedUiAction {
                changed: true,
                verified,
                summary: if verified {
                    "UI Automation invoke was accepted and a semantic application-state change was read back"
                } else {
                    "UI Automation invoke was accepted, but no semantic application-state change was read back within the bounded verification window"
                }
                .into(),
            })
        }
        UiSemanticAction::Toggle { desired } => {
            let pattern = toggle_pattern(&target.element)?;
            let before = toggle_state(
                unsafe { pattern.CurrentToggleState() }
                    .map_err(|_| action_failure("cannot read the UI Automation toggle state"))?,
            )
            .ok_or_else(|| {
                unsupported("the UI Automation toggle has an indeterminate or unknown state")
            })?;
            if before != *desired {
                unsafe { pattern.Toggle() }.map_err(|_| {
                    action_failure("the UI Automation toggle action was rejected by the target")
                })?;
            }
            let after =
                toggle_state(unsafe { pattern.CurrentToggleState() }.map_err(|_| {
                    action_failure("cannot read back the UI Automation toggle state")
                })?);
            if after != Some(*desired) {
                return Err(action_failure(
                    "the UI Automation toggle read-back did not match the requested state",
                ));
            }
            Ok(AppliedUiAction {
                changed: before != *desired,
                verified: true,
                summary: "UI Automation toggle state was read back from the target element".into(),
            })
        }
        UiSemanticAction::Select => {
            let pattern = selection_pattern(&target.element)?;
            unsafe { pattern.Select() }.map_err(|_| {
                action_failure("the UI Automation selection action was rejected by the target")
            })?;
            let selected = unsafe { pattern.CurrentIsSelected() }
                .map(|value| value.as_bool())
                .unwrap_or(false);
            if !selected {
                return Err(action_failure(
                    "the UI Automation selection read-back did not match the requested state",
                ));
            }
            Ok(AppliedUiAction {
                changed: true,
                verified: true,
                summary: "UI Automation selection was read back from the target element".into(),
            })
        }
        UiSemanticAction::SetValue { value } => {
            if value.len() > MAX_STRING_BYTES {
                return Err(failure_with_kind(
                    AgentErrorKind::OutputLimitExceeded,
                    "the UI Automation value exceeds its bounded action ceiling",
                    false,
                ));
            }
            let pattern = value_pattern(&target.element)?;
            unsafe { pattern.SetValue(&BSTR::from(value)) }.map_err(|_| {
                action_failure("the UI Automation value action was rejected by the target")
            })?;
            let readback = unsafe { pattern.CurrentValue() }
                .map(|value| value.to_string())
                .map_err(|_| action_failure("cannot read back the UI Automation value"))?;
            if readback != *value {
                return Err(action_failure(
                    "the UI Automation value read-back did not match the requested value",
                ));
            }
            Ok(AppliedUiAction {
                changed: true,
                verified: true,
                summary: "UI Automation value was read back from the target element".into(),
            })
        }
        UiSemanticAction::Focus => {
            unsafe { target.element.SetFocus() }.map_err(|_| {
                action_failure("the UI Automation focus action was rejected by the target")
            })?;
            let focused = unsafe { target.element.CurrentHasKeyboardFocus() }
                .map(|value| value.as_bool())
                .unwrap_or(false);
            if !focused {
                return Err(action_failure(
                    "the UI Automation focus read-back did not match the requested state",
                ));
            }
            Ok(AppliedUiAction {
                changed: true,
                verified: true,
                summary: "UI Automation keyboard focus was read back from the target element"
                    .into(),
            })
        }
        UiSemanticAction::Scroll { .. } => Err(unsupported(
            "this UI Automation semantic action is not enabled by the Windows adapter",
        )),
    }
}

fn locate_action_target(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
) -> Result<LocatedElement, AgentError> {
    let com = ComGuard::initialize()?;
    let foreground = resolve_foreground_application()?;
    if foreground.process_id != expected_process_id
        || !path_eq(&foreground.image_path, expected_image_path)
    {
        return Err(failure(
            "the foreground application changed before the UI Automation action",
            false,
        ));
    }
    let process_id = foreground.process_id;
    let started = foreground.process_started_at;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| failure("Windows UI Automation is unavailable", false))?;
    let root = unsafe {
        automation.ElementFromHandle(windows::Win32::Foundation::HWND(
            foreground.window_handle as *mut std::ffi::c_void,
        ))
    }
    .map_err(|_| failure("the foreground window has no UI Automation root", false))?;
    let walker = unsafe { automation.ControlViewWalker() }
        .map_err(|_| failure("cannot create a UI Automation tree walker", false))?;
    let deadline = Instant::now() + HARD_DEADLINE;
    let mut root_search_visited = 0usize;
    let root = find_process_root(
        &root,
        &walker,
        process_id,
        0,
        deadline,
        &mut root_search_visited,
    )
    .ok_or_else(|| {
        failure(
            "the foreground UI Automation tree has no root for the resolved application process",
            false,
        )
    })?;
    let mut visited = 0usize;
    let element = find_element(
        &root,
        &walker,
        None,
        0,
        0,
        process_id,
        started,
        target_fingerprint,
        deadline,
        &mut visited,
    )
    .ok_or_else(|| {
        failure(
            "the UI Automation element reference is stale or no longer reachable",
            false,
        )
    })?;
    Ok(LocatedElement { element, _com: com })
}

#[allow(clippy::too_many_arguments)]
fn find_element(
    element: &IUIAutomationElement,
    walker: &IUIAutomationTreeWalker,
    parent_fingerprint: Option<&str>,
    depth: u16,
    sibling_ordinal: usize,
    process_id: u32,
    process_started_at: u64,
    target_fingerprint: &str,
    deadline: Instant,
    visited: &mut usize,
) -> Option<IUIAutomationElement> {
    if *visited >= ACTION_MAX_NODES || Instant::now() >= deadline {
        return None;
    }
    let element_process_id = unsafe { element.CurrentProcessId() }.ok()?.max(0) as u32;
    if element_process_id != process_id {
        return None;
    }
    *visited += 1;
    let hwnd = unsafe { element.CurrentNativeWindowHandle() }
        .map(|value| value.0 as isize)
        .unwrap_or_default();
    let control_type = unsafe { element.CurrentControlType() }
        .map(|value| value.0)
        .unwrap_or_default();
    let automation_id = unsafe { element.CurrentAutomationId() }
        .map(|value| value.to_string())
        .unwrap_or_default();
    let current_fingerprint = fingerprint(
        parent_fingerprint,
        sibling_ordinal,
        process_id as i32,
        Some(process_started_at),
        hwnd,
        control_type,
        &automation_id,
    );
    if current_fingerprint == target_fingerprint {
        return Some(element.clone());
    }
    if depth >= ACTION_MAX_DEPTH {
        return None;
    }
    let mut child = unsafe { walker.GetFirstChildElement(element) }.ok()?;
    let mut ordinal = 0usize;
    loop {
        if let Some(found) = find_element(
            &child,
            walker,
            Some(&current_fingerprint),
            depth + 1,
            ordinal,
            process_id,
            process_started_at,
            target_fingerprint,
            deadline,
            visited,
        ) {
            return Some(found);
        }
        if *visited >= ACTION_MAX_NODES || Instant::now() >= deadline {
            return None;
        }
        let Ok(next) = (unsafe { walker.GetNextSiblingElement(&child) }) else {
            return None;
        };
        child = next;
        ordinal += 1;
    }
}

fn validate_action_target(
    element: &IUIAutomationElement,
    action: &UiSemanticAction,
) -> Result<(), AgentError> {
    let is_password = unsafe { element.CurrentIsPassword() }
        .map(|value| value.as_bool())
        .unwrap_or(true);
    if is_password {
        return Err(failure_with_kind(
            AgentErrorKind::PermissionDenied,
            "password UI Automation controls cannot receive semantic actions",
            false,
        ));
    }
    if !unsafe { element.CurrentIsEnabled() }
        .map(|value| value.as_bool())
        .unwrap_or(false)
    {
        return Err(failure_with_kind(
            AgentErrorKind::InvalidInput,
            "the UI Automation target is disabled",
            false,
        ));
    }
    match action {
        UiSemanticAction::Invoke => invoke_pattern(element).map(|_| ()),
        UiSemanticAction::Toggle { .. } => {
            let pattern = toggle_pattern(element)?;
            let state = unsafe { pattern.CurrentToggleState() }
                .map_err(|_| action_failure("cannot read the UI Automation toggle state"))?;
            toggle_state(state).map(|_| ()).ok_or_else(|| {
                unsupported("the UI Automation toggle has an indeterminate or unknown state")
            })
        }
        UiSemanticAction::Select => selection_pattern(element).map(|_| ()),
        UiSemanticAction::SetValue { value } => {
            if value.len() > MAX_STRING_BYTES {
                return Err(failure_with_kind(
                    AgentErrorKind::OutputLimitExceeded,
                    "the UI Automation value exceeds its bounded action ceiling",
                    false,
                ));
            }
            let pattern = value_pattern(element)?;
            if unsafe { pattern.CurrentIsReadOnly() }
                .map(|value| value.as_bool())
                .unwrap_or(true)
            {
                Err(unsupported("the UI Automation value target is read-only"))
            } else {
                Ok(())
            }
        }
        UiSemanticAction::Focus => {
            if unsafe { element.CurrentIsKeyboardFocusable() }
                .map(|value| value.as_bool())
                .unwrap_or(false)
            {
                Ok(())
            } else {
                Err(unsupported(
                    "the UI Automation target cannot receive keyboard focus",
                ))
            }
        }
        UiSemanticAction::Scroll { .. } => Err(unsupported(
            "this UI Automation semantic action is not enabled by the Windows adapter",
        )),
    }
}

fn invoke_pattern(
    element: &IUIAutomationElement,
) -> Result<IUIAutomationInvokePattern, AgentError> {
    unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) }
        .map_err(|_| unsupported("the UI Automation target does not support invoke"))
}

fn toggle_pattern(
    element: &IUIAutomationElement,
) -> Result<IUIAutomationTogglePattern, AgentError> {
    unsafe { element.GetCurrentPatternAs(UIA_TogglePatternId) }
        .map_err(|_| unsupported("the UI Automation target does not support toggle"))
}

fn selection_pattern(
    element: &IUIAutomationElement,
) -> Result<IUIAutomationSelectionItemPattern, AgentError> {
    unsafe { element.GetCurrentPatternAs(UIA_SelectionItemPatternId) }
        .map_err(|_| unsupported("the UI Automation target does not support selection"))
}

fn value_pattern(element: &IUIAutomationElement) -> Result<IUIAutomationValuePattern, AgentError> {
    unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId) }
        .map_err(|_| unsupported("the UI Automation target does not support value updates"))
}

fn toggle_state(state: ToggleState) -> Option<bool> {
    if state == ToggleState_Off {
        Some(false)
    } else if state == ToggleState_On {
        Some(true)
    } else {
        None
    }
}

fn semantic_tree_digest(tree: &CollectedUiTree) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([u8::from(tree.truncated)]);
    hasher.update(
        serde_json::to_vec(&tree.nodes)
            .expect("UI Automation semantic nodes contain only serializable values"),
    );
    hasher.finalize().into()
}

fn walk(
    element: IUIAutomationElement,
    parent: Option<(u32, String)>,
    depth: u16,
    sibling_ordinal: usize,
    config: &WalkConfig<'_>,
    state: &mut WalkState,
    output: &mut Vec<CollectedUiNode>,
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
) -> (CollectedUiNode, bool) {
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
        if !is_protected
            && element
                .CurrentIsKeyboardFocusable()
                .map(|value| value.as_bool())
                .unwrap_or(false)
        {
            supported_actions.push(UiSemanticActionKind::Focus);
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
            CollectedUiNode {
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

pub(super) fn process_start(process_id: u32) -> Option<u64> {
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

fn unsupported(message: &str) -> AgentError {
    failure_with_kind(AgentErrorKind::UnsupportedCapability, message, false)
}

fn action_failure(message: &str) -> AgentError {
    failure_with_kind(AgentErrorKind::Internal, message, false)
}

fn failure_with_kind(kind: AgentErrorKind, message: &str, retryable: bool) -> AgentError {
    AgentError {
        kind,
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
        let replaced_window = fingerprint(Some("parent-a"), 1, 4, Some(8), 11, 10, "id");
        assert_ne!(first, second);
        assert_ne!(first, restarted);
        assert_ne!(first, replaced_window);
    }

    #[test]
    fn toggle_state_rejects_indeterminate_or_unknown_values() {
        assert_eq!(toggle_state(ToggleState_Off), Some(false));
        assert_eq!(toggle_state(ToggleState_On), Some(true));
        assert_eq!(toggle_state(ToggleState(2)), None);
        assert_eq!(toggle_state(ToggleState(99)), None);
    }

    #[test]
    fn semantic_tree_digest_changes_when_uia_value_changes() {
        let node = CollectedUiNode {
            parent_index: None,
            role: "text".into(),
            name: Some("Display".into()),
            value: Some("0".into()),
            is_protected: false,
            enabled: true,
            supported_actions: Vec::new(),
            fingerprint: "stable-object".into(),
        };
        let before = CollectedUiTree {
            nodes: vec![node.clone()],
            truncated: false,
        };
        let mut after_node = node;
        after_node.value = Some("1".into());
        let after = CollectedUiTree {
            nodes: vec![after_node],
            truncated: false,
        };
        assert_ne!(semantic_tree_digest(&before), semantic_tree_digest(&after));
    }

    #[test]
    #[ignore = "requires Calculator to be foreground on an interactive Windows desktop"]
    fn live_calculator_tree_and_invoke_use_the_production_uia_adapter() {
        let foreground = resolve_foreground_application().expect("foreground application identity");
        let process_id = foreground.process_id;
        let image_path = foreground.image_path;
        assert!(
            image_path.to_ascii_lowercase().contains("calculator"),
            "foreground app is not Calculator: {image_path}"
        );
        let tree = collect_foreground(
            process_id,
            &image_path,
            ACTION_MAX_DEPTH,
            ACTION_MAX_NODES as u32,
            INVOKE_READBACK_MAX_BYTES,
        )
        .expect("Calculator UIA tree");
        let button = tree
            .nodes
            .iter()
            .find(|node| {
                node.supported_actions
                    .contains(&UiSemanticActionKind::Invoke)
                    && node
                        .name
                        .as_deref()
                        .is_some_and(|name| matches!(name, "One" | "1" | "一" | "数字 1"))
            })
            .expect("Calculator digit-one invoke target");
        preflight_action(
            process_id,
            &image_path,
            &button.fingerprint,
            &UiSemanticAction::Invoke,
        )
        .expect("Calculator invoke preflight");
        let result = apply_action(
            process_id,
            &image_path,
            &button.fingerprint,
            &UiSemanticAction::Invoke,
        )
        .expect("Calculator invoke");
        assert!(result.changed);
        assert!(result.verified, "{}", result.summary);
    }

    #[test]
    #[ignore = "requires Settings to be foreground on an interactive Windows desktop"]
    fn live_settings_tree_resolves_the_hosted_application_process() {
        let foreground = resolve_foreground_application().expect("foreground application identity");
        assert!(
            foreground
                .image_path
                .to_ascii_lowercase()
                .contains("systemsettings"),
            "foreground app is not Settings: {}",
            foreground.image_path
        );
        let tree = collect_foreground(
            foreground.process_id,
            &foreground.image_path,
            ACTION_MAX_DEPTH,
            ACTION_MAX_NODES as u32,
            INVOKE_READBACK_MAX_BYTES,
        )
        .expect("Settings UIA tree");
        assert!(!tree.nodes.is_empty());
        assert!(tree.nodes.iter().any(|node| {
            node.supported_actions
                .contains(&UiSemanticActionKind::Focus)
                || node
                    .supported_actions
                    .contains(&UiSemanticActionKind::Invoke)
        }));
    }

    #[test]
    #[ignore = "requires Notepad to be foreground on an interactive Windows desktop"]
    fn live_notepad_focus_uses_the_production_uia_adapter() {
        let foreground = resolve_foreground_application().expect("foreground application identity");
        assert!(
            foreground
                .image_path
                .to_ascii_lowercase()
                .contains("notepad"),
            "foreground app is not Notepad: {}",
            foreground.image_path
        );
        let tree = collect_foreground(
            foreground.process_id,
            &foreground.image_path,
            ACTION_MAX_DEPTH,
            ACTION_MAX_NODES as u32,
            INVOKE_READBACK_MAX_BYTES,
        )
        .expect("Notepad UIA tree");
        let target = tree
            .nodes
            .iter()
            .find(|node| {
                !node.is_protected
                    && node.name.as_deref() == Some("File")
                    && node
                        .supported_actions
                        .contains(&UiSemanticActionKind::Focus)
            })
            .expect("focusable Notepad target");
        preflight_action(
            foreground.process_id,
            &foreground.image_path,
            &target.fingerprint,
            &UiSemanticAction::Focus,
        )
        .expect("Notepad focus preflight");
        let result = apply_action(
            foreground.process_id,
            &foreground.image_path,
            &target.fingerprint,
            &UiSemanticAction::Focus,
        )
        .expect("Notepad focus");
        assert!(result.changed);
        assert!(result.verified, "{}", result.summary);
    }
}
