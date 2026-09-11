//! Bounded Windows UI Automation projection and semantic actions.
//!
//! Every action rechecks the foreground process and resolves a retained native
//! element bound to that process lifetime. Password controls fail closed;
//! mutations are limited to typed UIA patterns; callers inspect their effects separately.

use desk_agent_protocol::computer_use::UiInspectScope;
use std::time::{Duration, Instant};

use super::native_ui_identity::{IdentityStore, NativeElement};
use desk_agent_protocol::computer_use::{UiSemanticAction, UiSemanticActionKind};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use std::cell::RefCell;
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationTreeWalker,
    IUIAutomationValuePattern, ToggleState, ToggleState_Off, ToggleState_On,
    UIA_DataGridControlTypeId, UIA_InvokePatternId, UIA_ListControlTypeId,
    UIA_SelectionItemPatternId, UIA_TableControlTypeId, UIA_TogglePatternId, UIA_TreeControlTypeId,
    UIA_ValuePatternId, UIA_WindowControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
use windows::core::{BSTR, PWSTR};

const HARD_DEADLINE: Duration = Duration::from_secs(2);
const MAX_STRING_BYTES: usize = 16 * 1024;
const OBJECT_REF_BUDGET: usize = 320;
const ACTION_MAX_DEPTH: u16 = 16;
const ACTION_MAX_NODES: usize = 1_024;
const ACTION_OBSERVATION_MAX_BYTES: u32 = 1024 * 1024;

use super::computer_use_broker::{CollectedUiNode, CollectedUiTree};

struct ComGuard;

impl ComGuard {
    fn initialize() -> Result<Self, AgentError> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
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
    selection: Option<&'a str>,
    query: Option<&'a desk_agent_protocol::computer_use::UiInspectQuery>,
    element_only: bool,
    scope: UiInspectScope,
    walker: &'a IUIAutomationTreeWalker,
    process_id: u32,
    max_depth: u16,
    max_nodes: usize,
    max_bytes: usize,
    deadline: Instant,
}

#[derive(Default)]
struct WalkState {
    found_selection: bool,
    visited: usize,
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
    collect_foreground_with_scope(
        expected_process_id,
        expected_image_path,
        max_depth,
        max_nodes,
        max_bytes,
        UiInspectScope::Content,
    )
}

pub(super) fn collect_foreground_with_scope(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
    scope: UiInspectScope,
) -> Result<CollectedUiTree, AgentError> {
    collect_foreground_selection(
        expected_process_id,
        expected_image_path,
        max_depth,
        max_nodes,
        max_bytes,
        scope,
        None,
        None,
        false,
    )
}

pub(super) fn collect_foreground_selection(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
    scope: UiInspectScope,
    selection: Option<&str>,
    query: Option<&desk_agent_protocol::computer_use::UiInspectQuery>,
    element_only: bool,
) -> Result<CollectedUiTree, AgentError> {
    let expected_image_path = expected_image_path.to_owned();
    let query = query.cloned();
    let selection = selection.map(str::to_owned);
    super::native_ui_identity::run(move || {
        collect_foreground_selection_inner(
            expected_process_id,
            &expected_image_path,
            max_depth,
            max_nodes,
            max_bytes,
            scope,
            selection.as_deref(),
            query.as_ref(),
            element_only,
        )
    })
}

fn collect_foreground_selection_inner(
    expected_process_id: u32,
    expected_image_path: &str,
    max_depth: u16,
    max_nodes: u32,
    max_bytes: u32,
    scope: UiInspectScope,
    selection: Option<&str>,
    query: Option<&desk_agent_protocol::computer_use::UiInspectQuery>,
    element_only: bool,
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
    let walker = unsafe { automation.ControlViewWalker() }
        .map_err(|_| failure("cannot create a UI Automation tree walker", true))?;
    let root = if let Some(id) = selection {
        retained_element(id, expected_process_id, foreground.process_started_at)?
    } else {
        let root = unsafe {
            automation.ElementFromHandle(windows::Win32::Foundation::HWND(
                foreground.window_handle as *mut std::ffi::c_void,
            ))
        }
        .map_err(|_| failure("the foreground window has no UI Automation root", true))?;
        let mut root_search_visited = 0usize;
        find_process_root(
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
    })?
    };
    let config = WalkConfig {
        selection,
        query,
        element_only,
        scope,
        walker: &walker,
        process_id: expected_process_id,
        max_depth,
        max_nodes: max_nodes as usize,
        max_bytes: max_bytes as usize,
        deadline: Instant::now() + HARD_DEADLINE,
    };
    let mut state = WalkState::default();
    let mut nodes = Vec::new();
    walk(
        root, None, 0, 0, false, false, &config, &mut state, &mut nodes,
    )?;
    if selection.is_some() && !state.found_selection {
        return Err(failure(
            "selected UI root was not found within the bounded search",
            true,
        ));
    }
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
        ACTION_OBSERVATION_MAX_BYTES,
    )
    .map(|tree| tree.truncated || tree.nodes.iter().any(|node| node.is_protected))
}

pub(super) fn preflight_action(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
    action: &UiSemanticAction,
) -> Result<(), AgentError> {
    let expected_image_path = expected_image_path.to_owned();
    let target_fingerprint = target_fingerprint.to_owned();
    let action = action.clone();
    super::native_ui_identity::run(move || {
        preflight_action_inner(
            expected_process_id,
            &expected_image_path,
            &target_fingerprint,
            &action,
        )
    })
}

fn preflight_action_inner(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
    action: &UiSemanticAction,
) -> Result<(), AgentError> {
    let target =
        locate_action_target(expected_process_id, expected_image_path, target_fingerprint)?;
    validate_action_target(&target.element, action)
}

impl AppliedUiAction {
    // Verification covers native API completion, not the user's intended UI state.
    fn accepted(changed: bool) -> Self {
        Self {
            changed,
            verified: true,
            summary: "Native UI API completed successfully; application state is not verified. Use inspect_desktop_ui to check the expected result before deciding the next action.".into(),
        }
    }
}

pub(super) fn apply_action(
    expected_process_id: u32,
    expected_image_path: &str,
    target_fingerprint: &str,
    action: &UiSemanticAction,
) -> Result<AppliedUiAction, AgentError> {
    let expected_image_path = expected_image_path.to_owned();
    let target_fingerprint = target_fingerprint.to_owned();
    let action = action.clone();
    super::native_ui_identity::run(move || {
        apply_action_inner(
            expected_process_id,
            &expected_image_path,
            &target_fingerprint,
            &action,
        )
    })
}

fn apply_action_inner(
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
            let pattern = invoke_pattern(&target.element)?;
            unsafe { pattern.Invoke() }.map_err(|_| {
                action_failure("the UI Automation invoke action was rejected by the target")
            })?;
            Ok(AppliedUiAction::accepted(true))
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
            Ok(AppliedUiAction::accepted(before != *desired))
        }
        UiSemanticAction::Select => {
            let pattern = selection_pattern(&target.element)?;
            unsafe { pattern.Select() }.map_err(|_| {
                action_failure("the UI Automation selection action was rejected by the target")
            })?;
            Ok(AppliedUiAction::accepted(true))
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
            Ok(AppliedUiAction::accepted(true))
        }
        UiSemanticAction::Focus => {
            unsafe { target.element.SetFocus() }.map_err(|_| {
                action_failure("the UI Automation focus action was rejected by the target")
            })?;
            Ok(AppliedUiAction::accepted(true))
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
    let element = retained_element(
        target_fingerprint,
        foreground.process_id,
        foreground.process_started_at,
    )?;
    Ok(LocatedElement { element, _com: com })
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

fn is_menu_control(kind: windows::Win32::UI::Accessibility::UIA_CONTROLTYPE_ID) -> bool {
    use windows::Win32::UI::Accessibility::{
        UIA_MenuBarControlTypeId, UIA_MenuControlTypeId, UIA_MenuItemControlTypeId,
    };
    kind == UIA_MenuControlTypeId
        || kind == UIA_MenuBarControlTypeId
        || kind == UIA_MenuItemControlTypeId
}

fn element_identity(element: &IUIAutomationElement) -> Result<String, AgentError> {
    let process_id = unsafe { element.CurrentProcessId() }
        .map_err(|_| failure("cannot identify UI element process", false))?
        .max(0) as u32;
    let started = process_start(process_id)
        .ok_or_else(|| failure("cannot identify UI process lifetime", false))?;
    let key = runtime_key(element)?;
    IDENTITIES.with(|store| {
        store.borrow_mut().identify(
            process_id,
            started,
            key.clone(),
            RetainedUiaElement {
                element: element.clone(),
                runtime_id: key,
            },
        )
    })
}

fn walk(
    element: IUIAutomationElement,
    parent: Option<(Option<u32>, String)>,
    depth: u16,
    _sibling_ordinal: usize,
    inside_menu: bool,
    within_selection: bool,
    config: &WalkConfig<'_>,
    state: &mut WalkState,
    output: &mut Vec<CollectedUiNode>,
) -> Result<(), AgentError> {
    if config.element_only && state.found_selection {
        return Ok(());
    }
    if state.visited >= 4096 || Instant::now() >= config.deadline {
        state.truncated = true;
        return Ok(());
    }
    state.visited += 1;
    let inside_menu =
        inside_menu || unsafe { element.CurrentControlType() }.is_ok_and(is_menu_control);
    if config.scope == UiInspectScope::Content && inside_menu {
        return Ok(());
    }
    let identity = element_identity(&element)?;
    let within_selection =
        within_selection || config.selection.is_some_and(|target| target == identity);
    state.found_selection |= within_selection;
    let selected = config.selection.is_none() || within_selection;
    let emit = selected && (config.scope != UiInspectScope::Menus || inside_menu);
    if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
        state.truncated = true;
        return Ok(());
    }
    let element_process_id = unsafe { element.CurrentProcessId() }
        .unwrap_or_default()
        .max(0) as u32;
    if element_process_id != config.process_id {
        state.truncated = true;
        return Ok(());
    }
    let (index, fingerprint) = if emit {
        let (node, strings_truncated) = read_node(
            &element,
            identity.clone(),
            parent.as_ref().and_then(|(index, _)| *index),
        );
        if !super::computer_use_broker::ui_query_matches(config.query, &node) {
            (None, node.fingerprint)
        } else {
            let encoded_bytes = serde_json::to_vec(&node)
                .map_or(config.max_bytes.saturating_add(1), |encoded| encoded.len())
                .saturating_add(OBJECT_REF_BUDGET);
            if state.encoded_bytes.saturating_add(encoded_bytes) > config.max_bytes {
                state.truncated = true;
                return Ok(());
            }
            state.encoded_bytes += encoded_bytes;
            state.truncated |= strings_truncated;
            let index = output.len() as u32;
            let fingerprint = node.fingerprint.clone();
            output.push(node);
            (Some(index), fingerprint)
        }
    } else {
        (None, identity)
    };

    if selected && config.element_only {
        return Ok(());
    }
    if depth >= config.max_depth {
        if unsafe { config.walker.GetFirstChildElement(&element) }.is_ok() {
            state.truncated = true;
        }
        return Ok(());
    }
    let Ok(mut child) = (unsafe { config.walker.GetFirstChildElement(&element) }) else {
        return Ok(());
    };
    let mut ordinal = 0usize;
    loop {
        walk(
            child.clone(),
            Some((index, fingerprint.clone())),
            depth + 1,
            ordinal,
            inside_menu,
            within_selection,
            config,
            state,
            output,
        )?;
        if config.element_only && state.found_selection {
            return Ok(());
        }
        if output.len() >= config.max_nodes || Instant::now() >= config.deadline {
            state.truncated = true;
            return Ok(());
        }
        let Ok(next) = (unsafe { config.walker.GetNextSiblingElement(&child) }) else {
            break;
        };
        ordinal += 1;
        child = next;
    }
    Ok(())
}

fn read_node(
    element: &IUIAutomationElement,
    fingerprint: String,
    parent_index: Option<u32>,
) -> (CollectedUiNode, bool) {
    unsafe {
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
            let mut raw = element
                .CurrentName()
                .map(|value| value.to_string())
                .unwrap_or_default();
            if raw.trim().is_empty() {
                raw = element
                    .CurrentLabeledBy()
                    .ok()
                    .filter(|label| {
                        label
                            .CurrentIsPassword()
                            .is_ok_and(|value| !value.as_bool())
                    })
                    .and_then(|label| label.CurrentName().ok())
                    .map(|value| value.to_string())
                    .unwrap_or_default();
            }
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

        (
            CollectedUiNode {
                is_collection: [
                    UIA_DataGridControlTypeId.0,
                    UIA_TableControlTypeId.0,
                    UIA_TreeControlTypeId.0,
                    UIA_ListControlTypeId.0,
                ]
                .contains(&control_type),
                native_id: (!is_protected
                    && !automation_id.is_empty()
                    && automation_id.len() <= 512)
                    .then(|| automation_id.clone()),
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

thread_local! { static IDENTITIES: RefCell<IdentityStore<RetainedUiaElement>> = RefCell::new(IdentityStore::new(8192)); }

#[derive(Clone)]
struct RetainedUiaElement {
    element: IUIAutomationElement,
    runtime_id: Vec<u8>,
}

impl NativeElement for RetainedUiaElement {
    fn same_element(&self, other: &Self) -> bool {
        // Compare cached native identity; transient provider errors must not
        // allocate a different identity for the same still-live element.
        self.runtime_id == other.runtime_id
    }
    fn definitely_destroyed(&self) -> bool {
        unsafe { self.element.CurrentProcessId() }.is_err_and(|e| e.code().0 as u32 == 0x80040201)
    }
}

fn runtime_key(element: &IUIAutomationElement) -> Result<Vec<u8>, AgentError> {
    use windows::Win32::System::Ole::{
        SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetElemsize,
        SafeArrayGetLBound, SafeArrayGetUBound,
    };
    let array = unsafe { element.GetRuntimeId() }
        .map_err(|_| failure("UI element has no runtime identity", false))?;
    if array.is_null() {
        return Err(failure("UI element has no runtime identity", false));
    }
    let result = (|| unsafe {
        if SafeArrayGetDim(array) != 1 || SafeArrayGetElemsize(array) != 4 {
            return Err(failure("invalid UI runtime identity dimensions", false));
        }
        let lower = SafeArrayGetLBound(array, 1)
            .map_err(|_| failure("invalid UI runtime identity", false))?;
        let upper = SafeArrayGetUBound(array, 1)
            .map_err(|_| failure("invalid UI runtime identity", false))?;
        if upper < lower || i64::from(upper) - i64::from(lower) >= 128 {
            return Err(failure("UI runtime identity exceeds bounds", false));
        }
        let mut key = Vec::new();
        for index in lower..=upper {
            let mut value = 0i32;
            SafeArrayGetElement(array, &index, (&mut value as *mut i32).cast())
                .map_err(|_| failure("cannot read UI runtime identity", false))?;
            key.extend_from_slice(&value.to_le_bytes());
        }
        Ok(key)
    })();
    let _ = unsafe { SafeArrayDestroy(array) };
    result
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
    fn menu_classification_uses_native_types_instead_of_localized_names() {
        use windows::Win32::UI::Accessibility::*;
        for kind in [
            UIA_MenuControlTypeId,
            UIA_MenuBarControlTypeId,
            UIA_MenuItemControlTypeId,
        ] {
            assert!(is_menu_control(kind));
        }
        for kind in [
            UIA_ButtonControlTypeId,
            UIA_TextControlTypeId,
            UIA_WindowControlTypeId,
        ] {
            assert!(!is_menu_control(kind));
        }
    }

    #[test]
    fn bounded_strings_stop_on_utf8_boundaries() {
        let source = "界".repeat(MAX_STRING_BYTES);
        let (value, truncated) = bounded_string(source);
        assert!(truncated);
        assert!(value.len() <= MAX_STRING_BYTES);
        assert!(value.is_char_boundary(value.len()));
    }

    #[test]
    fn toggle_state_rejects_indeterminate_or_unknown_values() {
        assert_eq!(toggle_state(ToggleState_Off), Some(false));
        assert_eq!(toggle_state(ToggleState_On), Some(true));
        assert_eq!(toggle_state(ToggleState(2)), None);
        assert_eq!(toggle_state(ToggleState(99)), None);
    }

    #[test]
    fn native_acceptance_does_not_claim_application_state_verification() {
        for changed in [false, true] {
            let result = AppliedUiAction::accepted(changed);
            assert!(result.verified);
            assert_eq!(result.changed, changed);
            assert!(result.summary.contains("not verified"));
            assert!(result.summary.contains("inspect_desktop_ui"));
        }
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
            ACTION_OBSERVATION_MAX_BYTES,
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
            ACTION_OBSERVATION_MAX_BYTES,
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
            ACTION_OBSERVATION_MAX_BYTES,
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

pub(super) fn retained_element_ids() -> Result<std::collections::HashSet<String>, AgentError> {
    super::native_ui_identity::run(|| Ok(IDENTITIES.with(|store| store.borrow().retained_ids())))
}

fn retained_element(
    id: &str,
    process_id: u32,
    started: u64,
) -> Result<IUIAutomationElement, AgentError> {
    IDENTITIES.with(|store| store.borrow_mut().get(id, process_id, started))
        .map(|retained| retained.element)
        .ok_or_else(|| failure("the native UI element was destroyed or belongs to a different process lifetime; search for a new element", false))
}
